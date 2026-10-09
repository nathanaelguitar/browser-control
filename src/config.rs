//! Persistent user configuration (TOML at `<config_dir>/config.toml`).
//!
//! Edited via the `browser-control set|get|unset` subcommands.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

use crate::paths;

const HEADER: &str =
    "# Managed by browser-control. Edit with `browser-control set <key> <value>`.\n";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Browser the MCP server prefers when no explicit `-b`/`BROWSER_CONTROL`
    /// is given. Overrides `BROWSER_CONTROL_MCP_DEFAULT` (which the Canopy
    /// extension manifest sets to `obscura`) and is consulted before
    /// `default`. The value `inherit` disables the preference entirely.
    #[serde(
        default,
        rename = "mcp-default",
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_default: Option<String>,
    /// MCP tab policy: `reuse` (default) or `free`. See [`TabPolicy`].
    /// Overridden by `BROWSER_CONTROL_TAB_POLICY`.
    #[serde(
        default,
        rename = "tab-policy",
        skip_serializing_if = "Option::is_none"
    )]
    pub tab_policy: Option<String>,
    /// Minutes of inactivity after which the MCP server closes tabs it
    /// opened (`off` disables). Default 10. See [`IdleMinutes`].
    /// Overridden by `BROWSER_CONTROL_TAB_IDLE_CLOSE`.
    #[serde(
        default,
        rename = "tab-idle-close",
        skip_serializing_if = "Option::is_none"
    )]
    pub tab_idle_close: Option<String>,
    /// Minutes of inactivity after which a browser that browser-control
    /// launched is quit (`off` disables). Default 15. Overridden by
    /// `BROWSER_CONTROL_BROWSER_IDLE_QUIT`.
    #[serde(
        default,
        rename = "browser-idle-quit",
        skip_serializing_if = "Option::is_none"
    )]
    pub browser_idle_quit: Option<String>,
    /// `on` keeps named (durable) tabs open when an MCP session ends or they
    /// idle; default `off` (they are closed like any other tab the MCP
    /// server opened). Overridden by `BROWSER_CONTROL_KEEP_NAMED_TABS`.
    #[serde(
        default,
        rename = "keep-named-tabs",
        skip_serializing_if = "Option::is_none"
    )]
    pub keep_named_tabs: Option<String>,
}

/// Env var that overrides the persisted `tab-idle-close` setting.
pub const TAB_IDLE_CLOSE_ENV: &str = "BROWSER_CONTROL_TAB_IDLE_CLOSE";
/// Env var that overrides the persisted `browser-idle-quit` setting.
pub const BROWSER_IDLE_QUIT_ENV: &str = "BROWSER_CONTROL_BROWSER_IDLE_QUIT";
/// Env var that overrides the persisted `keep-named-tabs` setting.
pub const KEEP_NAMED_TABS_ENV: &str = "BROWSER_CONTROL_KEEP_NAMED_TABS";

/// Default minutes before an unused MCP-opened tab is closed.
pub const DEFAULT_TAB_IDLE_CLOSE_MIN: u64 = 10;
/// Default minutes before an unused browser-control-launched browser quits.
pub const DEFAULT_BROWSER_IDLE_QUIT_MIN: u64 = 15;

/// An idle timeout setting: a number of minutes, or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleMinutes {
    Off,
    Minutes(u64),
}

impl IdleMinutes {
    /// Parse `off` (also `never`, `0`, `none`, `false`) or a positive integer
    /// number of minutes.
    pub fn parse(value: &str) -> Result<Self> {
        let v = value.trim().to_ascii_lowercase();
        match v.as_str() {
            "off" | "never" | "none" | "false" | "0" => Ok(IdleMinutes::Off),
            other => match other.parse::<u64>() {
                Ok(n) if n > 0 => Ok(IdleMinutes::Minutes(n)),
                _ => Err(anyhow::anyhow!(
                    "invalid idle timeout `{value}`; expected a number of minutes or `off`"
                )),
            },
        }
    }

    /// Canonical stored form: `off` or the integer.
    pub fn canonical(self) -> String {
        match self {
            IdleMinutes::Off => "off".into(),
            IdleMinutes::Minutes(n) => n.to_string(),
        }
    }

    /// The timeout as a duration; `None` when off.
    pub fn duration(self) -> Option<std::time::Duration> {
        match self {
            IdleMinutes::Off => None,
            IdleMinutes::Minutes(n) => Some(std::time::Duration::from_secs(n * 60)),
        }
    }

    /// Valid env value wins over a valid config value, which wins over
    /// `default_min`. Blank or invalid values are ignored.
    pub fn resolve(config_value: Option<&str>, env_value: Option<&str>, default_min: u64) -> Self {
        [env_value, config_value]
            .into_iter()
            .flatten()
            .find_map(|v| Self::parse(v).ok())
            .unwrap_or(IdleMinutes::Minutes(default_min))
    }
}

/// Parse an `on`/`off` flag value.
pub fn parse_on_off(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        other => Err(anyhow::anyhow!(
            "invalid value `{other}`; expected `on` or `off`"
        )),
    }
}

/// Effective lifecycle settings for the MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleSettings {
    pub tab_idle_close: IdleMinutes,
    pub browser_idle_quit: IdleMinutes,
    pub keep_named_tabs: bool,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            tab_idle_close: IdleMinutes::Minutes(DEFAULT_TAB_IDLE_CLOSE_MIN),
            browser_idle_quit: IdleMinutes::Minutes(DEFAULT_BROWSER_IDLE_QUIT_MIN),
            keep_named_tabs: false,
        }
    }
}

impl LifecycleSettings {
    pub fn resolve(cfg: &Config, env: impl Fn(&str) -> Option<String>) -> Self {
        let keep_env = env(KEEP_NAMED_TABS_ENV);
        let keep = [keep_env.as_deref(), cfg.keep_named_tabs.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|v| parse_on_off(v).ok())
            .unwrap_or(false);
        Self {
            tab_idle_close: IdleMinutes::resolve(
                cfg.tab_idle_close.as_deref(),
                env(TAB_IDLE_CLOSE_ENV).as_deref(),
                DEFAULT_TAB_IDLE_CLOSE_MIN,
            ),
            browser_idle_quit: IdleMinutes::resolve(
                cfg.browser_idle_quit.as_deref(),
                env(BROWSER_IDLE_QUIT_ENV).as_deref(),
                DEFAULT_BROWSER_IDLE_QUIT_MIN,
            ),
            keep_named_tabs: keep,
        }
    }

    /// Read from the environment and the config file.
    pub fn current() -> Self {
        let cfg = load().unwrap_or_default();
        Self::resolve(&cfg, |k| std::env::var(k).ok())
    }
}

/// Env var that overrides the persisted `tab-policy` setting.
pub const TAB_POLICY_ENV: &str = "BROWSER_CONTROL_TAB_POLICY";

/// How the MCP server treats unnamed `browser_tab_new` calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TabPolicy {
    /// Navigate the live active tab instead of opening another one.
    #[default]
    Reuse,
    /// Always open a new tab.
    Free,
}

impl TabPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            TabPolicy::Reuse => "reuse",
            TabPolicy::Free => "free",
        }
    }

    /// Parse `reuse` / `free` (case-insensitive, trimmed).
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "reuse" => Ok(TabPolicy::Reuse),
            "free" => Ok(TabPolicy::Free),
            other => Err(anyhow::anyhow!(
                "invalid tab-policy `{other}`; expected `reuse` or `free`"
            )),
        }
    }

    /// Effective policy: a valid env value wins over the config value, which
    /// wins over the default (`reuse`). Blank or invalid values are ignored.
    pub fn resolve(config_value: Option<&str>, env_value: Option<&str>) -> Self {
        [env_value, config_value]
            .into_iter()
            .flatten()
            .find_map(|v| Self::parse(v).ok())
            .unwrap_or_default()
    }

    /// Read the effective policy from the environment and config file.
    pub fn current() -> Self {
        let cfg = load().ok();
        let env = std::env::var(TAB_POLICY_ENV).ok();
        Self::resolve(
            cfg.as_ref().and_then(|c| c.tab_policy.as_deref()),
            env.as_deref(),
        )
    }
}

impl Config {
    pub fn is_empty(&self) -> bool {
        self.default.is_none()
            && self.mcp_default.is_none()
            && self.tab_policy.is_none()
            && self.tab_idle_close.is_none()
            && self.browser_idle_quit.is_none()
            && self.keep_named_tabs.is_none()
    }
}

/// Load the config from disk. A missing file yields `Config::default()`.
pub fn load() -> Result<Config> {
    load_from(&paths::config_file_path()?)
}

/// Save `cfg` to disk atomically.
pub fn save(cfg: &Config) -> Result<()> {
    save_to(&paths::config_file_path()?, cfg)
}

pub(crate) fn load_from(path: &PathBuf) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str::<Config>(&s)
            .with_context(|| format!("parsing config file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("reading config file {}", path.display())),
    }
}

pub(crate) fn save_to(path: &PathBuf, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(cfg).context("serializing config to TOML")?;
    let mut contents = String::with_capacity(HEADER.len() + body.len());
    contents.push_str(HEADER);
    contents.push_str(&body);

    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating tmp config file {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("writing tmp config file {}", tmp.display()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cfg() -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        (td, p)
    }

    #[test]
    fn missing_file_yields_default() {
        let (_td, p) = tmp_cfg();
        let cfg = load_from(&p).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(cfg.is_empty());
    }

    #[test]
    fn round_trip_default() {
        let (_td, p) = tmp_cfg();
        let cfg = Config {
            default: Some("firefox".into()),
            mcp_default: Some("obscura".into()),
            tab_policy: Some("free".into()),
            tab_idle_close: Some("5".into()),
            browser_idle_quit: Some("off".into()),
            keep_named_tabs: Some("on".into()),
        };
        save_to(&p, &cfg).unwrap();
        let read = load_from(&p).unwrap();
        assert_eq!(read, cfg);

        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# Managed by browser-control"));
        assert!(text.contains("default = \"firefox\""));
        assert!(text.contains("mcp-default = \"obscura\""));
        assert!(text.contains("tab-policy = \"free\""));
        assert!(text.contains("tab-idle-close = \"5\""));
        assert!(text.contains("browser-idle-quit = \"off\""));
        assert!(text.contains("keep-named-tabs = \"on\""));
    }

    #[test]
    fn idle_minutes_parse() {
        assert_eq!(IdleMinutes::parse("off").unwrap(), IdleMinutes::Off);
        assert_eq!(IdleMinutes::parse(" NEVER ").unwrap(), IdleMinutes::Off);
        assert_eq!(IdleMinutes::parse("0").unwrap(), IdleMinutes::Off);
        assert_eq!(IdleMinutes::parse("15").unwrap(), IdleMinutes::Minutes(15));
        assert!(IdleMinutes::parse("-3").is_err());
        assert!(IdleMinutes::parse("soon").is_err());
        assert!(IdleMinutes::parse("").is_err());
        assert_eq!(IdleMinutes::Minutes(2).canonical(), "2");
        assert_eq!(
            IdleMinutes::Minutes(2).duration(),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(IdleMinutes::Off.duration(), None);
    }

    #[test]
    fn idle_minutes_resolve_precedence() {
        assert_eq!(
            IdleMinutes::resolve(None, None, 10),
            IdleMinutes::Minutes(10)
        );
        assert_eq!(
            IdleMinutes::resolve(Some("3"), None, 10),
            IdleMinutes::Minutes(3)
        );
        assert_eq!(
            IdleMinutes::resolve(Some("3"), Some("off"), 10),
            IdleMinutes::Off
        );
        // Invalid / blank env falls through to config.
        assert_eq!(
            IdleMinutes::resolve(Some("3"), Some("bogus"), 10),
            IdleMinutes::Minutes(3)
        );
        assert_eq!(
            IdleMinutes::resolve(Some("3"), Some(""), 10),
            IdleMinutes::Minutes(3)
        );
    }

    #[test]
    fn lifecycle_settings_defaults_and_overrides() {
        let none = |_: &str| None;
        let d = LifecycleSettings::resolve(&Config::default(), none);
        assert_eq!(d, LifecycleSettings::default());
        assert_eq!(d.tab_idle_close, IdleMinutes::Minutes(10));
        assert_eq!(d.browser_idle_quit, IdleMinutes::Minutes(15));
        assert!(!d.keep_named_tabs);

        let cfg = Config {
            tab_idle_close: Some("4".into()),
            keep_named_tabs: Some("on".into()),
            ..Config::default()
        };
        let env = |k: &str| match k {
            BROWSER_IDLE_QUIT_ENV => Some("1".to_string()),
            TAB_IDLE_CLOSE_ENV => Some("off".to_string()),
            _ => None,
        };
        let s = LifecycleSettings::resolve(&cfg, env);
        assert_eq!(s.tab_idle_close, IdleMinutes::Off);
        assert_eq!(s.browser_idle_quit, IdleMinutes::Minutes(1));
        assert!(s.keep_named_tabs);
    }

    #[test]
    fn save_clears_when_default_is_none() {
        let (_td, p) = tmp_cfg();
        save_to(
            &p,
            &Config {
                default: Some("chrome".into()),
                ..Config::default()
            },
        )
        .unwrap();
        save_to(&p, &Config::default()).unwrap();
        let read = load_from(&p).unwrap();
        assert!(read.is_empty());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            !text.contains("default ="),
            "expected key to be absent, got: {text}"
        );
    }

    #[test]
    fn malformed_file_is_an_error() {
        let (_td, p) = tmp_cfg();
        std::fs::write(&p, "this is = not [valid toml").unwrap();
        let err = load_from(&p).unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(msg.contains("parsing config file"), "got: {msg}");
    }

    #[test]
    fn tab_policy_parse_accepts_known_values_only() {
        assert_eq!(TabPolicy::parse("reuse").unwrap(), TabPolicy::Reuse);
        assert_eq!(TabPolicy::parse(" FREE ").unwrap(), TabPolicy::Free);
        assert!(TabPolicy::parse("sometimes").is_err());
        assert!(TabPolicy::parse("").is_err());
    }

    #[test]
    fn tab_policy_resolve_precedence() {
        assert_eq!(TabPolicy::resolve(None, None), TabPolicy::Reuse);
        assert_eq!(TabPolicy::resolve(Some("free"), None), TabPolicy::Free);
        // env overrides config in both directions
        assert_eq!(
            TabPolicy::resolve(Some("free"), Some("reuse")),
            TabPolicy::Reuse
        );
        assert_eq!(
            TabPolicy::resolve(Some("reuse"), Some("free")),
            TabPolicy::Free
        );
        // blank / invalid env falls back to config, then default
        assert_eq!(TabPolicy::resolve(Some("free"), Some("")), TabPolicy::Free);
        assert_eq!(
            TabPolicy::resolve(Some("free"), Some("bogus")),
            TabPolicy::Free
        );
        assert_eq!(TabPolicy::resolve(Some("bogus"), None), TabPolicy::Reuse);
    }
}
