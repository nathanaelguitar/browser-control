//! `browser-control mcp` subcommand entry point.

use anyhow::{Context, Result};

use crate::cli::env_resolver::{self, BrowserSelector, ResolvedBrowser, Source};
use crate::detect::Kind;
use crate::mcp::server::{run, ServerState, ToolRegistry};
use crate::registry::{BrowserRow, Registry};

/// Entry point for `browser-control mcp`.
pub async fn run_cli(
    browser_arg: Option<String>,
    playwright_version: Option<String>,
) -> Result<()> {
    let resolved = resolve_mcp_browser(browser_arg).await?;
    let sidecar_config = crate::sidecar::SidecarConfig {
        version: playwright_version,
    };
    let state = ServerState::with_sidecar_config(resolved, sidecar_config);
    let tools = ToolRegistry::new();
    crate::mcp::tools::register_all(&tools);
    run(state, tools).await
}

/// Env var naming the browser the MCP server prefers when nothing explicit
/// is selected. The Canopy extension manifest sets it to `obscura`.
pub const MCP_DEFAULT_ENV: &str = "BROWSER_CONTROL_MCP_DEFAULT";

/// `mcp-default` / `BROWSER_CONTROL_MCP_DEFAULT` value that disables the
/// MCP preference, falling through to `default`.
pub const MCP_DEFAULT_INHERIT: &str = "inherit";

/// Where the effective MCP preference came from (for log messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPreference {
    /// `browser-control set mcp-default <value>`.
    Config(String),
    /// `BROWSER_CONTROL_MCP_DEFAULT` (set by the Canopy extension manifest).
    Env(String),
}

impl McpPreference {
    pub fn value(&self) -> &str {
        match self {
            McpPreference::Config(v) | McpPreference::Env(v) => v,
        }
    }

    fn origin(&self) -> &'static str {
        match self {
            McpPreference::Config(_) => "`mcp-default` setting",
            McpPreference::Env(_) => MCP_DEFAULT_ENV,
        }
    }
}

/// The effective MCP browser preference: the persisted `mcp-default` wins
/// over the env var; `inherit` (from either) disables the preference.
pub fn mcp_preference(
    config_value: Option<&str>,
    env_value: Option<&str>,
) -> Option<McpPreference> {
    let pick = config_value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| McpPreference::Config(v.to_string()))
        .or_else(|| {
            env_value
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| McpPreference::Env(v.to_string()))
        })?;
    if pick.value().eq_ignore_ascii_case(MCP_DEFAULT_INHERIT) {
        return None;
    }
    Some(pick)
}

/// Read the effective MCP preference from config and environment.
pub fn current_mcp_preference() -> Result<Option<McpPreference>> {
    let cfg = crate::config::load()?;
    let env = std::env::var(MCP_DEFAULT_ENV).ok();
    Ok(mcp_preference(cfg.mcp_default.as_deref(), env.as_deref()))
}

/// Kind that `browser_start` should launch when the caller names none: the
/// MCP preference if it is a kind (or generated name) that is installed,
/// otherwise `None` (= first installed Chromium-family browser).
pub fn preferred_start_kind() -> Option<Kind> {
    let pref = current_mcp_preference().ok().flatten()?;
    let sel = env_resolver::parse(pref.value()).ok()?;
    let kind = startable_kind_from_selector(&sel)?;
    crate::detect::list_installed()
        .iter()
        .any(|i| i.kind == kind)
        .then_some(kind)
}

/// Browser resolution for the MCP server (`browser-control mcp`): explicit
/// selection first, then the MCP preference, then [`resolve_browser`]'s
/// chain. Resolution order:
///
/// 1. positional arg / `BROWSER_CONTROL` env (arg wins, env is the fallback —
///    both are merged by clap into `browser_arg`);
/// 2. the MCP preference: persisted `mcp-default`, else
///    `BROWSER_CONTROL_MCP_DEFAULT` (the Canopy extension sets `obscura`).
///    A live browser of that kind is reused, otherwise one is started. If
///    that fails (for example Obscura is not installed) the reason is logged
///    to stderr and resolution falls through to the steps below, which is
///    the automatic fallback to Chrome/Chromium;
/// 3. [`resolve_browser`]: persisted default (`browser-control set default
///    ...`), most recent live browser, then start the default installed
///    browser.
///
/// The preference only applies to the MCP server; CLI commands keep using
/// [`resolve_browser`] directly.
pub async fn resolve_mcp_browser(browser_arg: Option<String>) -> Result<ResolvedBrowser> {
    if browser_arg.as_deref().is_some_and(|s| !s.is_empty()) {
        return resolve_browser(browser_arg).await;
    }
    if let Some(pref) = current_mcp_preference()? {
        let registry = Registry::open()?;
        match env_resolver::parse(pref.value()) {
            Ok(sel) => match resolve_selector_or_start(sel, &registry).await {
                Ok(resolved) => return Ok(resolved),
                Err(e) => eprintln!(
                    "browser-control: preferred MCP browser `{}` (from {}) is unavailable, \
                     falling back to the regular default: {e:#}",
                    pref.value(),
                    pref.origin()
                ),
            },
            Err(e) => eprintln!(
                "browser-control: ignoring invalid MCP browser preference `{}` (from {}): {e:#}",
                pref.value(),
                pref.origin()
            ),
        }
    }
    resolve_browser(None).await
}

/// Resolution order: positional arg / `BROWSER_CONTROL` env (arg wins, env is
/// the fallback — both are merged by clap into `browser_arg`) > persisted
/// default (`browser-control set default ...`) > most recent live browser >
/// start the default installed browser.
///
/// MCP must be recoverable by the agent using it. Unlike short-lived CLI
/// commands, the server cannot exit before exposing tools such as
/// `browser_start` / `browser_select`, so the no-explicit-selection path picks
/// or starts a usable browser.
pub async fn resolve_browser(browser_arg: Option<String>) -> Result<ResolvedBrowser> {
    let registry = Registry::open()?;
    if let Some(arg) = browser_arg.as_deref().filter(|s| !s.is_empty()) {
        let sel = env_resolver::parse(arg)?;
        return resolve_selector_or_start(sel, &registry).await;
    }
    if let Some(value) = crate::config::load()?.default {
        match resolve_default_browser(&value, &registry).await {
            Ok(resolved) => return Ok(resolved),
            Err(default_err) => {
                if let Some(row) = registry.most_recent_alive()? {
                    return Ok(resolved_from_row(row));
                }
                if let Ok(sel) = env_resolver::parse(&value) {
                    if let Some(kind) = startable_kind_from_selector(&sel) {
                        return start_and_resolve(Some(kind.as_str().to_string()), false, 30)
                            .await
                            .with_context(|| {
                                format!(
                                    "default browser `{value}` failed to resolve ({default_err:#}); also failed to start {}",
                                    kind.as_str()
                                )
                            });
                    }
                }
                return start_and_resolve(None, false, 30).await.with_context(|| {
                    format!(
                        "default browser `{value}` failed to resolve ({default_err:#}); also failed to start a browser"
                    )
                });
            }
        }
    }
    if let Some(row) = registry.most_recent_alive()? {
        return Ok(resolved_from_row(row));
    }
    start_and_resolve(None, false, 30).await
}

async fn resolve_default_browser(value: &str, registry: &Registry) -> Result<ResolvedBrowser> {
    let sel = env_resolver::parse(value)?;
    if let BrowserSelector::Name(name) = &sel {
        if let Some(kind) = stale_default_kind(name, registry)? {
            rewrite_default_if_unchanged(value, kind)?;
            return env_resolver::resolve(BrowserSelector::Kind(kind), registry)
                .await
                .with_context(|| {
                    format!(
                        "default browser `{name}` is stale; rewrote default to `{}` but failed to resolve a live {} browser",
                        kind.as_str(),
                        kind.as_str()
                    )
                });
        }
    }
    env_resolver::resolve(sel, registry).await
}

async fn resolve_selector_or_start(
    selector: BrowserSelector,
    registry: &Registry,
) -> Result<ResolvedBrowser> {
    match env_resolver::resolve(selector.clone(), registry).await {
        Ok(resolved) => Ok(resolved),
        Err(resolve_err) => {
            if let Some(kind) = startable_kind_from_selector(&selector) {
                return start_and_resolve(Some(kind.as_str().to_string()), false, 30)
                    .await
                    .with_context(|| {
                        format!(
                            "browser selector failed to resolve ({resolve_err:#}); also failed to start {}",
                            kind.as_str()
                        )
                    });
            }
            Err(resolve_err)
        }
    }
}

pub(crate) fn startable_kind_from_selector(selector: &BrowserSelector) -> Option<Kind> {
    match selector {
        BrowserSelector::Kind(kind) => Some(*kind),
        BrowserSelector::Name(name) => kind_from_generated_name(name),
        BrowserSelector::Url(_) | BrowserSelector::ExecutablePath(_) => None,
    }
}

pub async fn start_and_resolve(
    browser: Option<String>,
    headless: bool,
    wait_timeout: u64,
) -> Result<ResolvedBrowser> {
    let started = crate::cli::start::ensure_started(browser, headless, false, wait_timeout).await?;
    Ok(ResolvedBrowser {
        endpoint: started.endpoint,
        engine: started.engine,
        source: Source::Registered { name: started.name },
    })
}

fn resolved_from_row(row: BrowserRow) -> ResolvedBrowser {
    ResolvedBrowser {
        endpoint: row.endpoint,
        engine: row.engine,
        source: Source::Registered { name: row.name },
    }
}

fn stale_default_kind(name: &str, registry: &Registry) -> Result<Option<Kind>> {
    if let Some(row) = registry
        .get_by_name(name)
        .with_context(|| format!("looking up default browser {name}"))?
    {
        return match crate::registry::liveness(&row) {
            crate::registry::BrowserLiveness::DeadPid => {
                registry
                    .delete(&row.name)
                    .with_context(|| format!("pruning stale default browser {}", row.name))?;
                Ok(Some(row.kind))
            }
            crate::registry::BrowserLiveness::Alive
            | crate::registry::BrowserLiveness::EndpointUnreachable => Ok(None),
        };
    }

    Ok(kind_from_generated_name(name))
}

fn kind_from_generated_name(name: &str) -> Option<Kind> {
    let (prefix, _) = name.split_once('-')?;
    Kind::parse(prefix)
}

fn rewrite_default_if_unchanged(previous: &str, kind: Kind) -> Result<()> {
    let mut cfg = crate::config::load()?;
    if cfg.default.as_deref() == Some(previous) {
        cfg.default = Some(kind.as_str().to_string());
        crate::config::save(&cfg)
            .with_context(|| format!("rewriting stale default browser `{previous}`"))?;
    }
    Ok(())
}

/// Acquire the Firefox BiDi single-session lock if `resolved` is a
/// registered browser on the BiDi engine. Returns `None` for external
/// URL endpoints (the lock is per-registered-name) and for CDP engines.
/// Wait bound: 30 s; on timeout, returns the typed `BidiLockBusy` error.
pub fn acquire_bidi_lock_if_needed(
    registry: &crate::registry::Registry,
    resolved: &ResolvedBrowser,
) -> Result<Option<crate::registry::BidiLockGuard>> {
    use crate::detect::Engine;
    if resolved.engine != Engine::Bidi {
        return Ok(None);
    }
    let name = match &resolved.source {
        crate::cli::env_resolver::Source::Registered { name } => name.clone(),
        crate::cli::env_resolver::Source::External => return Ok(None),
    };
    let guard = registry.bidi_lock_acquire(&name, std::time::Duration::from_secs(30))?;
    Ok(Some(guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Config};
    use crate::detect::{Engine, Kind};
    use crate::registry::{BrowserRow, Registry};
    use std::path::PathBuf;

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
            std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
            std::env::remove_var(MCP_DEFAULT_ENV);
        }
    }

    struct AliveListener {
        _listener: std::net::TcpListener,
        port: u16,
    }

    fn alive_listener() -> AliveListener {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        AliveListener {
            _listener: listener,
            port,
        }
    }

    fn row(name: &str, kind: Kind, port: u16, pid: u32, started_at: &str) -> BrowserRow {
        BrowserRow {
            name: name.to_string(),
            kind,
            engine: kind.engine(),
            pid,
            endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/{name}"),
            port,
            profile_dir: PathBuf::from(format!("/tmp/profiles/{name}")),
            executable: PathBuf::from("/usr/bin/example"),
            headless: false,
            started_at: started_at.to_string(),
        }
    }

    fn with_tmp_env<R>(f: impl FnOnce() -> R) -> R {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data = tempfile::TempDir::new().unwrap();
        let cfg = tempfile::TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_DATA_DIR", data.path());
        std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", cfg.path());
        let _guard = EnvGuard;
        f()
    }

    #[test]
    fn mcp_preference_precedence() {
        assert_eq!(mcp_preference(None, None), None);
        assert_eq!(
            mcp_preference(None, Some("obscura")),
            Some(McpPreference::Env("obscura".into()))
        );
        assert_eq!(
            mcp_preference(Some("chrome"), Some("obscura")),
            Some(McpPreference::Config("chrome".into()))
        );
        // `inherit` in config disables the env preference (user opt-out).
        assert_eq!(mcp_preference(Some("inherit"), Some("obscura")), None);
        assert_eq!(mcp_preference(None, Some("INHERIT")), None);
        // Blank values are ignored.
        assert_eq!(
            mcp_preference(Some("  "), Some("obscura")),
            Some(McpPreference::Env("obscura".into()))
        );
    }

    #[test]
    fn mcp_preference_wins_over_default_and_reuses_live_browser() {
        with_tmp_env(|| {
            let live_chrome = alive_listener();
            let live_obscura = alive_listener();
            let reg = Registry::open().unwrap();
            reg.insert(&row(
                "chrome-oak",
                Kind::Chrome,
                live_chrome.port,
                std::process::id(),
                "2024-01-03T00:00:00Z",
            ))
            .unwrap();
            reg.insert(&row(
                "obscura-fern",
                Kind::Obscura,
                live_obscura.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            ))
            .unwrap();
            config::save(&Config {
                default: Some("chrome".into()),
                ..Config::default()
            })
            .unwrap();
            drop(reg);
            std::env::set_var(MCP_DEFAULT_ENV, "obscura");
            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_mcp_browser(None)).unwrap();
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "obscura-fern".into()
                }
            );
        });
    }

    #[test]
    fn missing_obscura_falls_back_to_regular_default() {
        with_tmp_env(|| {
            let live_chrome = alive_listener();
            let reg = Registry::open().unwrap();
            reg.insert(&row(
                "chrome-oak",
                Kind::Chrome,
                live_chrome.port,
                std::process::id(),
                "2024-01-01T00:00:00Z",
            ))
            .unwrap();
            config::save(&Config {
                default: Some("chrome".into()),
                ..Config::default()
            })
            .unwrap();
            drop(reg);
            std::env::set_var(MCP_DEFAULT_ENV, "obscura");
            std::env::set_var(
                crate::detect::obscura::ENV_OVERRIDE,
                "/nonexistent/browser-control-test/obscura",
            );
            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_mcp_browser(None));
            std::env::remove_var(crate::detect::obscura::ENV_OVERRIDE);
            assert_eq!(
                got.unwrap().source,
                env_resolver::Source::Registered {
                    name: "chrome-oak".into()
                }
            );
        });
    }

    #[test]
    fn cli_resolution_ignores_mcp_preference() {
        with_tmp_env(|| {
            let live_chrome = alive_listener();
            let live_obscura = alive_listener();
            let reg = Registry::open().unwrap();
            reg.insert(&row(
                "chrome-oak",
                Kind::Chrome,
                live_chrome.port,
                std::process::id(),
                "2024-01-01T00:00:00Z",
            ))
            .unwrap();
            reg.insert(&row(
                "obscura-fern",
                Kind::Obscura,
                live_obscura.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            ))
            .unwrap();
            config::save(&Config {
                default: Some("chrome".into()),
                mcp_default: Some("obscura".into()),
                ..Config::default()
            })
            .unwrap();
            drop(reg);
            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_browser(None)).unwrap();
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "chrome-oak".into()
                }
            );
        });
    }

    #[test]
    fn mcp_default_inherit_opts_out_of_env_preference() {
        with_tmp_env(|| {
            let live_chrome = alive_listener();
            let live_obscura = alive_listener();
            let reg = Registry::open().unwrap();
            reg.insert(&row(
                "chrome-oak",
                Kind::Chrome,
                live_chrome.port,
                std::process::id(),
                "2024-01-01T00:00:00Z",
            ))
            .unwrap();
            reg.insert(&row(
                "obscura-fern",
                Kind::Obscura,
                live_obscura.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            ))
            .unwrap();
            config::save(&Config {
                default: Some("chrome".into()),
                mcp_default: Some("inherit".into()),
                ..Config::default()
            })
            .unwrap();
            drop(reg);
            std::env::set_var(MCP_DEFAULT_ENV, "obscura");
            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_mcp_browser(None)).unwrap();
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "chrome-oak".into()
                }
            );
        });
    }

    #[test]
    fn stale_named_default_rewrites_to_kind_and_resolves_live_same_kind() {
        with_tmp_env(|| {
            let live = alive_listener();
            let reg = Registry::open().unwrap();
            reg.insert(&row(
                "brave-cosmos",
                Kind::Brave,
                9,
                99_999_999,
                "2024-01-01T00:00:00Z",
            ))
            .unwrap();
            let live_row = row(
                "brave-spruce",
                Kind::Brave,
                live.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            );
            reg.insert(&live_row).unwrap();
            config::save(&Config {
                default: Some("brave-cosmos".into()),
                ..Config::default()
            })
            .unwrap();
            drop(reg);

            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_browser(None)).unwrap();
            assert_eq!(got.endpoint, live_row.endpoint);
            assert_eq!(got.engine, Engine::Cdp);
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "brave-spruce".into()
                }
            );
            assert_eq!(config::load().unwrap().default.as_deref(), Some("brave"));
            let reg = Registry::open().unwrap();
            assert!(reg.get_by_name("brave-cosmos").unwrap().is_none());
        });
    }

    #[test]
    fn missing_generated_default_rewrites_to_kind_and_resolves_live_same_kind() {
        with_tmp_env(|| {
            let live = alive_listener();
            let reg = Registry::open().unwrap();
            let live_row = row(
                "brave-spruce",
                Kind::Brave,
                live.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            );
            reg.insert(&live_row).unwrap();
            config::save(&Config {
                default: Some("brave-cosmos".into()),
                ..Config::default()
            })
            .unwrap();
            drop(reg);

            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_browser(None)).unwrap();
            assert_eq!(got.endpoint, live_row.endpoint);
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "brave-spruce".into()
                }
            );
            assert_eq!(config::load().unwrap().default.as_deref(), Some("brave"));
        });
    }
}
