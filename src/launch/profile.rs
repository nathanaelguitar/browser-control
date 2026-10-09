//! Launch hygiene for browser-control-managed profiles.
//!
//! browser-control owns the automation profiles it creates under
//! `<config_dir>/profiles/<kind>/default`. Nobody reads their old tabs, so a
//! cold launch must never bring a previous session back (a profile that
//! "continues where it left off" slowly accumulated dozens of stale
//! job-application tabs). Before spawning a browser we therefore:
//!
//! * Chromium family: rewrite `Default/Preferences` so startup opens the new
//!   tab page (`session.restore_on_startup = 5`), drop `session.startup_urls`,
//!   mark the last exit clean (no crash-restore bubble), and delete the saved
//!   session files (`Sessions/Session_*`, `Sessions/Tabs_*` and the legacy
//!   `Current/Last Session|Tabs`).
//! * Firefox: write `browser.startup.page=0` and
//!   `browser.sessionstore.resume_from_crash=false` into `user.js` and delete
//!   the session store.
//!
//! Everything here is a no-op for profiles that are not browser-control
//! managed, so a user's own profile (or an attached external browser) is never
//! touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Marker file written into profile dirs browser-control created that live
/// outside the standard `profiles/` tree (for example `--profile` style
/// callers and tests). Its presence declares the dir managed.
pub const MANAGED_MARKER: &str = ".browser-control-managed";

/// Chromium `session.restore_on_startup` value for "open the New Tab page".
pub const RESTORE_ON_STARTUP_NEW_TAB: i64 = 5;

/// Chromium command-line flags that keep a managed profile from restoring
/// sessions or showing first-run / crash-restore UI. `--hide-crash-restore-bubble`
/// is the current name; `--disable-session-crashed-bubble` was removed from
/// Chromium but is harmless on builds that ignore it.
pub const CHROMIUM_NO_RESTORE_FLAGS: &[&str] = &[
    "--no-first-run",
    "--no-default-browser-check",
    "--hide-crash-restore-bubble",
    "--disable-session-crashed-bubble",
    "--noerrdialogs",
];

/// True when `dir` is a profile directory browser-control owns: it sits under
/// `<config_dir>/profiles/`, or carries [`MANAGED_MARKER`].
pub fn is_managed_profile(dir: &Path) -> bool {
    if dir.join(MANAGED_MARKER).exists() {
        return true;
    }
    match crate::paths::config_dir() {
        Ok(cfg) => {
            let root = cfg.join("profiles");
            let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
            let root = root.canonicalize().unwrap_or(root);
            dir.starts_with(root)
        }
        Err(_) => false,
    }
}

/// Declare `dir` browser-control managed. Only call for directories
/// browser-control itself created.
pub fn mark_managed(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(MANAGED_MARKER), b"")?;
    Ok(())
}

/// True when a live browser process still holds this profile
/// (Chromium `SingletonLock` -> `host-pid`). Rewriting prefs or deleting
/// sessions under a running browser would be wrong.
fn profile_in_use(dir: &Path) -> bool {
    let lock = dir.join("SingletonLock");
    let Ok(target) = std::fs::read_link(&lock) else {
        return false;
    };
    target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .is_some_and(crate::registry::pid_alive)
}

/// Pure transform: apply the no-restore settings to a parsed Preferences
/// document, preserving every other key. Returns true if anything changed.
pub fn rewrite_chromium_prefs(prefs: &mut Value) -> bool {
    if !prefs.is_object() {
        *prefs = json!({});
    }
    let before = prefs.clone();
    let root = prefs.as_object_mut().expect("object");

    let session = root
        .entry("session")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    match session {
        Some(session) => {
            session.insert(
                "restore_on_startup".into(),
                json!(RESTORE_ON_STARTUP_NEW_TAB),
            );
            session.remove("startup_urls");
        }
        None => {
            root.insert(
                "session".into(),
                json!({ "restore_on_startup": RESTORE_ON_STARTUP_NEW_TAB }),
            );
        }
    }

    let profile = root
        .entry("profile")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    match profile {
        Some(profile) => {
            profile.insert("exit_type".into(), json!("Normal"));
            profile.insert("exited_cleanly".into(), json!(true));
        }
        None => {
            root.insert(
                "profile".into(),
                json!({ "exit_type": "Normal", "exited_cleanly": true }),
            );
        }
    }
    *prefs != before
}

/// Profile subdirectories that hold a Preferences file: `Default` and
/// `Profile N`.
fn chromium_profile_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.join("Default")];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("Profile ") && e.path().is_dir() {
                out.push(e.path());
            }
        }
    }
    out
}

/// Delete saved-session files under one Chromium profile subdir. Returns the
/// number of files removed.
fn delete_chromium_sessions(sub: &Path) -> usize {
    let mut removed = 0;
    let sessions = sub.join("Sessions");
    if let Ok(rd) = std::fs::read_dir(&sessions) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if (name.starts_with("Session_") || name.starts_with("Tabs_"))
                && e.path().is_file()
                && std::fs::remove_file(e.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    for legacy in [
        "Current Session",
        "Last Session",
        "Current Tabs",
        "Last Tabs",
    ] {
        if std::fs::remove_file(sub.join(legacy)).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Prepare a managed Chromium profile for launch. No-op (returns `Ok(false)`)
/// when `dir` is not managed or a live browser still uses it.
pub fn prepare_chromium_profile(dir: &Path) -> Result<bool> {
    if !is_managed_profile(dir) {
        tracing::debug!(target = "launch", dir = %dir.display(), "not a managed profile; leaving sessions alone");
        return Ok(false);
    }
    if profile_in_use(dir) {
        tracing::debug!(target = "launch", dir = %dir.display(), "profile in use; skipping hygiene");
        return Ok(false);
    }
    for sub in chromium_profile_subdirs(dir) {
        let prefs_path = sub.join("Preferences");
        // A brand-new profile has no Preferences yet; seed one only for
        // `Default`. If an existing file cannot be parsed leave it alone and
        // rely on the session-file deletion.
        if prefs_path.exists() || sub.file_name().is_some_and(|n| n == "Default") {
            let mut prefs = match std::fs::read_to_string(&prefs_path) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(target = "launch", path = %prefs_path.display(), error = %e, "unparseable Preferences; not rewriting");
                        None
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(json!({})),
                Err(e) => {
                    tracing::warn!(target = "launch", path = %prefs_path.display(), error = %e, "unreadable Preferences; not rewriting");
                    None
                }
            };
            if let Some(prefs) = prefs.as_mut() {
                if rewrite_chromium_prefs(prefs) || !prefs_path.exists() {
                    std::fs::create_dir_all(&sub)?;
                    let tmp = prefs_path.with_extension("bc-tmp");
                    std::fs::write(&tmp, serde_json::to_vec(prefs)?)
                        .with_context(|| format!("writing {}", tmp.display()))?;
                    std::fs::rename(&tmp, &prefs_path)
                        .with_context(|| format!("replacing {}", prefs_path.display()))?;
                }
            }
        }
        let n = delete_chromium_sessions(&sub);
        if n > 0 {
            tracing::debug!(target = "launch", removed = n, dir = %sub.display(), "deleted saved session files");
        }
    }
    Ok(true)
}

/// `user.js` lines Firefox needs to start blank and not offer crash resume.
const FIREFOX_USER_PREFS: &[(&str, &str)] = &[
    ("browser.startup.page", "0"),
    ("browser.sessionstore.resume_from_crash", "false"),
    ("browser.sessionstore.max_resumed_crashes", "0"),
];

/// Prepare a managed Firefox profile for launch. No-op when not managed or in
/// use.
pub fn prepare_firefox_profile(dir: &Path) -> Result<bool> {
    if !is_managed_profile(dir) {
        return Ok(false);
    }
    // Firefox's lock is `lock` (symlink on unix) / `parent.lock`; a live
    // Firefox would also hold `.parentlock`. Skip if the unix symlink names a
    // live pid.
    if let Ok(target) = std::fs::read_link(dir.join("lock")) {
        if target
            .to_string_lossy()
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_start_matches('+').parse::<u32>().ok())
            .is_some_and(crate::registry::pid_alive)
        {
            return Ok(false);
        }
    }
    std::fs::create_dir_all(dir)?;
    let user_js = dir.join("user.js");
    let existing = std::fs::read_to_string(&user_js).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            !FIREFOX_USER_PREFS
                .iter()
                .any(|(k, _)| l.contains(&format!("\"{k}\"")))
        })
        .map(String::from)
        .collect();
    for (k, v) in FIREFOX_USER_PREFS {
        lines.push(format!("user_pref(\"{k}\", {v});"));
    }
    let mut body = lines.join("\n");
    body.push('\n');
    if body != existing {
        std::fs::write(&user_js, body)?;
    }
    let _ = std::fs::remove_file(dir.join("sessionstore.jsonlz4"));
    let _ = std::fs::remove_file(dir.join("sessionstore.js"));
    let _ = std::fs::remove_dir_all(dir.join("sessionstore-backups"));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> Value {
        json!({
            "browser": { "window_placement": { "top": 10 } },
            "session": {
                "restore_on_startup": 1,
                "startup_urls": ["https://jobs.example/1", "https://jobs.example/2"],
                "other": "keep"
            },
            "profile": { "exit_type": "Crashed", "exited_cleanly": false, "name": "Person 1" },
            "protection": { "macs": {} }
        })
    }

    #[test]
    fn rewrite_prefs_sets_restore_and_clean_exit_and_keeps_rest() {
        let mut p = fixture();
        assert!(rewrite_chromium_prefs(&mut p));
        assert_eq!(p["session"]["restore_on_startup"], 5);
        assert!(p["session"].get("startup_urls").is_none());
        assert_eq!(p["session"]["other"], "keep");
        assert_eq!(p["profile"]["exit_type"], "Normal");
        assert_eq!(p["profile"]["exited_cleanly"], true);
        assert_eq!(p["profile"]["name"], "Person 1");
        assert_eq!(p["browser"]["window_placement"]["top"], 10);
        assert!(p["protection"]["macs"].is_object());
        // Idempotent.
        assert!(!rewrite_chromium_prefs(&mut p));
    }

    #[test]
    fn rewrite_prefs_handles_missing_and_wrong_shaped_sections() {
        let mut p = json!({});
        assert!(rewrite_chromium_prefs(&mut p));
        assert_eq!(p["session"]["restore_on_startup"], 5);
        assert_eq!(p["profile"]["exit_type"], "Normal");

        let mut p = json!({"session": "weird", "profile": 3});
        assert!(rewrite_chromium_prefs(&mut p));
        assert_eq!(p["session"]["restore_on_startup"], 5);
        assert_eq!(p["profile"]["exited_cleanly"], true);

        let mut p = json!([1, 2]);
        assert!(rewrite_chromium_prefs(&mut p));
        assert!(p.is_object());
    }

    fn seed_profile(dir: &Path) {
        let default = dir.join("Default");
        std::fs::create_dir_all(default.join("Sessions")).unwrap();
        std::fs::write(
            default.join("Preferences"),
            serde_json::to_vec(&fixture()).unwrap(),
        )
        .unwrap();
        for f in ["Session_13300000000", "Tabs_13300000001", "keepme.txt"] {
            std::fs::write(default.join("Sessions").join(f), b"x").unwrap();
        }
        std::fs::write(default.join("Current Session"), b"x").unwrap();
        std::fs::write(default.join("Last Tabs"), b"x").unwrap();
        std::fs::write(default.join("Cookies"), b"x").unwrap();
        let p1 = dir.join("Profile 1");
        std::fs::create_dir_all(p1.join("Sessions")).unwrap();
        std::fs::write(p1.join("Sessions").join("Session_1"), b"x").unwrap();
    }

    #[test]
    fn prepare_managed_profile_rewrites_prefs_and_deletes_sessions() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("p");
        seed_profile(&dir);
        mark_managed(&dir).unwrap();
        assert!(prepare_chromium_profile(&dir).unwrap());

        let prefs: Value =
            serde_json::from_slice(&std::fs::read(dir.join("Default/Preferences")).unwrap())
                .unwrap();
        assert_eq!(prefs["session"]["restore_on_startup"], 5);
        assert_eq!(prefs["profile"]["exit_type"], "Normal");
        assert!(!dir.join("Default/Sessions/Session_13300000000").exists());
        assert!(!dir.join("Default/Sessions/Tabs_13300000001").exists());
        assert!(!dir.join("Default/Current Session").exists());
        assert!(!dir.join("Default/Last Tabs").exists());
        assert!(!dir.join("Profile 1/Sessions/Session_1").exists());
        // Unrelated files survive.
        assert!(dir.join("Default/Sessions/keepme.txt").exists());
        assert!(dir.join("Default/Cookies").exists());
    }

    #[test]
    fn prepare_unmanaged_profile_is_left_untouched() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cfg = TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", cfg.path());
        let td = TempDir::new().unwrap();
        let dir = td.path().join("users-own-brave");
        seed_profile(&dir);
        let before = std::fs::read(dir.join("Default/Preferences")).unwrap();
        assert!(!prepare_chromium_profile(&dir).unwrap());
        assert!(!prepare_firefox_profile(&dir).unwrap());
        std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
        assert_eq!(
            std::fs::read(dir.join("Default/Preferences")).unwrap(),
            before
        );
        assert!(dir.join("Default/Sessions/Session_13300000000").exists());
        assert!(dir.join("Default/Sessions/Tabs_13300000001").exists());
        assert!(!dir.join("user.js").exists());
    }

    #[test]
    fn profiles_tree_under_config_dir_is_managed() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cfg = TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", cfg.path());
        let managed = cfg.path().join("profiles/brave/default");
        std::fs::create_dir_all(&managed).unwrap();
        let managed_ok = is_managed_profile(&managed);
        let other = TempDir::new().unwrap();
        let other_ok = is_managed_profile(other.path());
        std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
        assert!(managed_ok);
        assert!(!other_ok);
    }

    #[test]
    fn prepare_creates_preferences_for_fresh_managed_profile() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("fresh");
        mark_managed(&dir).unwrap();
        assert!(prepare_chromium_profile(&dir).unwrap());
        let prefs: Value =
            serde_json::from_slice(&std::fs::read(dir.join("Default/Preferences")).unwrap())
                .unwrap();
        assert_eq!(prefs["session"]["restore_on_startup"], 5);
    }

    #[test]
    fn unparseable_preferences_are_kept_but_sessions_still_deleted() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("p");
        seed_profile(&dir);
        std::fs::write(dir.join("Default/Preferences"), b"{not json").unwrap();
        mark_managed(&dir).unwrap();
        assert!(prepare_chromium_profile(&dir).unwrap());
        assert_eq!(
            std::fs::read(dir.join("Default/Preferences")).unwrap(),
            b"{not json"
        );
        assert!(!dir.join("Default/Sessions/Session_13300000000").exists());
    }

    #[test]
    fn firefox_prefs_written_idempotently_and_sessionstore_removed() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("ff");
        std::fs::create_dir_all(dir.join("sessionstore-backups")).unwrap();
        std::fs::write(dir.join("sessionstore-backups/recovery.jsonlz4"), b"x").unwrap();
        std::fs::write(dir.join("sessionstore.jsonlz4"), b"x").unwrap();
        std::fs::write(
            dir.join("user.js"),
            "user_pref(\"my.pref\", 1);\nuser_pref(\"browser.startup.page\", 3);\n",
        )
        .unwrap();
        mark_managed(&dir).unwrap();
        assert!(prepare_firefox_profile(&dir).unwrap());
        assert!(prepare_firefox_profile(&dir).unwrap());
        let js = std::fs::read_to_string(dir.join("user.js")).unwrap();
        assert!(js.contains("user_pref(\"my.pref\", 1);"));
        assert_eq!(js.matches("browser.startup.page").count(), 1);
        assert!(js.contains("user_pref(\"browser.startup.page\", 0);"));
        assert!(js.contains("user_pref(\"browser.sessionstore.resume_from_crash\", false);"));
        assert!(!dir.join("sessionstore.jsonlz4").exists());
        assert!(!dir.join("sessionstore-backups").exists());
    }

    #[test]
    fn flags_include_restore_suppression() {
        for f in [
            "--no-first-run",
            "--no-default-browser-check",
            "--hide-crash-restore-bubble",
        ] {
            assert!(CHROMIUM_NO_RESTORE_FLAGS.contains(&f));
        }
    }
}
