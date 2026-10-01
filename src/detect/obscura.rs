//! Obscura discovery (all platforms).
//!
//! Obscura is not installed through an OS package or app bundle, so it is
//! looked up in three places, first match wins:
//!
//! 1. `BROWSER_CONTROL_OBSCURA`: absolute path to the `obscura` binary.
//! 2. `obscura` on `PATH`.
//! 3. The managed location `<data_dir>/bin/obscura` (`obscura.exe` on Windows),
//!    for installs that should not touch `PATH`.
//!
//! browser-control never downloads Obscura itself; [`missing_message`] tells
//! the user where to get the release build.

use std::path::{Path, PathBuf};

use super::{Installed, Kind, Probe};

/// Env var naming an explicit Obscura binary.
pub const ENV_OVERRIDE: &str = "BROWSER_CONTROL_OBSCURA";

/// Release page linked from the "not installed" message.
pub const RELEASES_URL: &str = "https://github.com/h4ckf0r0day/obscura/releases";

#[cfg(windows)]
const BINARY_NAME: &str = "obscura.exe";
#[cfg(not(windows))]
const BINARY_NAME: &str = "obscura";

/// `BROWSER_CONTROL_OBSCURA`, when set to a non-empty value.
pub fn env_override() -> Option<PathBuf> {
    std::env::var_os(ENV_OVERRIDE)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `<data_dir>/bin/obscura`, when the data dir can be determined.
pub fn managed_path() -> Option<PathBuf> {
    crate::paths::data_dir()
        .ok()
        .map(|d| d.join("bin").join(BINARY_NAME))
}

/// Locate Obscura. `env_path` and `managed` are injected so tests do not
/// depend on the process environment.
pub fn detect<P: Probe>(
    probe: &P,
    env_path: Option<&Path>,
    managed: Option<&Path>,
) -> Option<Installed> {
    let exe = find(probe, env_path, managed)?;
    let version = probe
        .run_version(&exe)
        .unwrap_or_else(|| "unknown".to_string());
    Some(Installed {
        kind: Kind::Obscura,
        executable: exe,
        version,
        engine: Kind::Obscura.engine(),
    })
}

fn find<P: Probe>(probe: &P, env_path: Option<&Path>, managed: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = env_path {
        // An explicit override that does not exist is not silently replaced
        // by a different binary: the user asked for this one.
        return probe.exists(p).then(|| p.to_path_buf());
    }
    if let Some(p) = probe.which("obscura") {
        return Some(p);
    }
    managed.filter(|p| probe.exists(p)).map(Path::to_path_buf)
}

/// Actionable error text for "obscura requested but not found".
pub fn missing_message() -> String {
    let managed = managed_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<data dir>/bin/obscura".to_string());
    let override_note = match env_override() {
        Some(p) => format!(
            " ({ENV_OVERRIDE} is set to {}, which does not exist)",
            p.display()
        ),
        None => String::new(),
    };
    format!(
        "obscura is not installed{override_note}. Download the non-stealth build for your \
         platform (obscura-<arch>-<os>.tar.gz, not a -stealth archive) from {RELEASES_URL}, \
         then either put `obscura` on PATH, extract it to {managed}, or set {ENV_OVERRIDE}=/path/to/obscura. \
         Other browsers stay available: pass `chrome`, `chromium`, `edge`, `brave` or `firefox` instead."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Default)]
    struct FakeProbe {
        existing: HashSet<PathBuf>,
        path_map: HashMap<String, PathBuf>,
    }

    impl Probe for FakeProbe {
        fn exists(&self, p: &Path) -> bool {
            self.existing.contains(p)
        }
        fn run_version(&self, _exe: &Path) -> Option<String> {
            Some("0.2.3".into())
        }
        fn which(&self, name: &str) -> Option<PathBuf> {
            self.path_map.get(name).cloned()
        }
    }

    #[test]
    fn env_override_wins_over_path_and_managed() {
        let env = PathBuf::from("/opt/obscura/obscura");
        let on_path = PathBuf::from("/usr/local/bin/obscura");
        let managed = PathBuf::from("/data/bin/obscura");
        let mut probe = FakeProbe::default();
        probe.existing.insert(env.clone());
        probe.existing.insert(managed.clone());
        probe.path_map.insert("obscura".into(), on_path);
        let found = detect(&probe, Some(&env), Some(&managed)).unwrap();
        assert_eq!(found.executable, env);
        assert_eq!(found.kind, Kind::Obscura);
        assert_eq!(found.version, "0.2.3");
    }

    #[test]
    fn missing_env_override_is_not_replaced_by_path() {
        let on_path = PathBuf::from("/usr/local/bin/obscura");
        let mut probe = FakeProbe::default();
        probe.path_map.insert("obscura".into(), on_path);
        assert!(detect(&probe, Some(Path::new("/nope/obscura")), None).is_none());
    }

    #[test]
    fn path_then_managed() {
        let on_path = PathBuf::from("/usr/local/bin/obscura");
        let managed = PathBuf::from("/data/bin/obscura");
        let mut probe = FakeProbe::default();
        probe.existing.insert(managed.clone());
        probe.path_map.insert("obscura".into(), on_path.clone());
        assert_eq!(
            detect(&probe, None, Some(&managed)).unwrap().executable,
            on_path
        );
        probe.path_map.clear();
        assert_eq!(
            detect(&probe, None, Some(&managed)).unwrap().executable,
            managed
        );
        probe.existing.clear();
        assert!(detect(&probe, None, Some(&managed)).is_none());
    }

    #[test]
    fn missing_message_points_at_release_and_alternatives() {
        let msg = missing_message();
        assert!(msg.contains(RELEASES_URL));
        assert!(msg.contains(ENV_OVERRIDE));
        assert!(msg.contains("chrome"));
        assert!(msg.contains("non-stealth"));
    }
}
