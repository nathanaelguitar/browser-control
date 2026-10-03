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
        self.default.is_none() && self.mcp_default.is_none() && self.tab_policy.is_none()
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
        };
        save_to(&p, &cfg).unwrap();
        let read = load_from(&p).unwrap();
        assert_eq!(read, cfg);

        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# Managed by browser-control"));
        assert!(text.contains("default = \"firefox\""));
        assert!(text.contains("mcp-default = \"obscura\""));
        assert!(text.contains("tab-policy = \"free\""));
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
