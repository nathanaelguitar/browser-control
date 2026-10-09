//! Resource lifecycle for the MCP server: which tabs this server opened, when
//! they were last useful, and when the browser itself should be closed.
//!
//! Rules:
//!
//! * Only tabs this server created are ever closed (`Lifecycle::track`). The
//!   browser's initial tab, tabs a human opened, and tabs created by other
//!   processes are never touched.
//! * Owned tabs are closed when the MCP session ends and when they have had no
//!   tool activity for `tab-idle-close` minutes.
//! * A browser browser-control launched is quit (CDP `Browser.close`) after
//!   `browser-idle-quit` minutes without MCP activity, and at session end when
//!   this server launched it and no other MCP session is using it. The next
//!   tool call relaunches a fresh browser on demand.
//!
//! Timers read a clock with an adjustable skew (`Lifecycle::advance`, test
//! only) so idle behaviour is testable without sleeping and without pausing
//! tokio time (which would also fast-forward real socket timeouts).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use std::time::Instant;

use crate::cli::env_resolver::{ResolvedBrowser, Source};
use crate::config::LifecycleSettings;
use crate::detect::{Engine, Kind};
use crate::mcp::server::ServerState;
use crate::session::backend::TabBackend;

/// How often the reaper checks for idle tabs and an idle browser.
pub const REAP_TICK: Duration = Duration::from_secs(30);
/// Upper bound for session-end cleanup so a wedged browser cannot hang exit.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(8);
/// How long to wait for a browser to exit after `Browser.close`.
/// How long session-end cleanup waits for running tool calls to finish.
const IN_FLIGHT_GRACE: Duration = Duration::from_secs(3);
const QUIT_WAIT: Duration = Duration::from_secs(10);
/// Endpoint of the placeholder `ResolvedBrowser` meaning "not started yet".
pub const PENDING_ENDPOINT: &str = "pending://browser-control/start-on-demand";
/// Minimum spacing between writes of the cross-process activity file.
const ACTIVITY_FILE_THROTTLE: Duration = Duration::from_secs(10);
const ACTIVITY_FILE: &str = ".browser-control-activity";
const SESSIONS_DIR: &str = ".browser-control-mcp-sessions";

/// A tab this server created.
#[derive(Debug, Clone)]
pub struct OwnedTab {
    pub last_used: Instant,
    /// Registry name for durable tabs opened with `browser_tab_new name=...`.
    pub named: Option<String>,
}

#[derive(Debug)]
struct Inner {
    tabs: HashMap<String, OwnedTab>,
    last_activity: Instant,
    in_flight: usize,
    profile_dir: Option<PathBuf>,
    last_file_touch: Option<Instant>,
    /// Added to the real clock; only tests move it.
    skew: Duration,
}

impl Inner {
    fn now(&self) -> Instant {
        Instant::now() + self.skew
    }
}

/// Shared tracker. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    inner: Arc<Mutex<Inner>>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// Held for the duration of a tool call. Counts the call as in flight and
/// stamps activity when it starts and when it ends.
#[must_use]
pub struct CallGuard {
    lc: Lifecycle,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut g = self.lc.lock();
        g.in_flight = g.in_flight.saturating_sub(1);
        g.last_activity = g.now();
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                tabs: HashMap::new(),
                last_activity: Instant::now(),
                in_flight: 0,
                profile_dir: None,
                last_file_touch: None,
                skew: Duration::ZERO,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark a tool call as started.
    pub fn begin(&self) -> CallGuard {
        {
            let mut g = self.lock();
            g.in_flight += 1;
            g.last_activity = g.now();
        }
        self.touch_activity_file();
        CallGuard { lc: self.clone() }
    }

    pub fn in_flight(&self) -> usize {
        self.lock().in_flight
    }

    /// Record activity without a tool call (for example browser resolution).
    pub fn touch(&self) {
        let mut g = self.lock();
        g.last_activity = g.now();
    }

    /// Move this tracker's clock forward (tests).
    #[cfg(test)]
    pub fn advance(&self, d: Duration) {
        self.lock().skew += d;
    }

    /// Time since the last tool activity seen by this server.
    pub fn idle_for(&self) -> Duration {
        let g = self.lock();
        g.now().saturating_duration_since(g.last_activity)
    }

    /// Record a tab this server created.
    pub fn track(&self, target_id: &str, named: Option<String>) {
        let mut g = self.lock();
        let last_used = g.now();
        g.tabs
            .insert(target_id.to_string(), OwnedTab { last_used, named });
    }

    pub fn untrack(&self, target_id: &str) {
        self.lock().tabs.remove(target_id);
    }

    pub fn is_owned(&self, target_id: &str) -> bool {
        self.lock().tabs.contains_key(target_id)
    }

    pub fn owned_count(&self) -> usize {
        self.lock().tabs.len()
    }

    /// Refresh a tab's idle timer (no-op for tabs this server does not own).
    pub fn touch_tab(&self, target_id: &str) {
        let mut g = self.lock();
        let now = g.now();
        if let Some(t) = g.tabs.get_mut(target_id) {
            t.last_used = now;
        }
    }

    pub fn clear_tabs(&self) {
        self.lock().tabs.clear();
    }

    /// Owned tabs idle for at least `idle`, optionally sparing named tabs.
    pub fn tabs_to_reap(&self, idle: Duration, keep_named: bool) -> Vec<(String, Option<String>)> {
        let g = self.lock();
        let now = g.now();
        let mut out: Vec<_> = g
            .tabs
            .iter()
            .filter(|(_, t)| !(keep_named && t.named.is_some()))
            .filter(|(_, t)| now.saturating_duration_since(t.last_used) >= idle)
            .map(|(id, t)| (id.clone(), t.named.clone()))
            .collect();
        out.sort();
        out
    }

    /// Every owned tab (session end), optionally sparing named tabs.
    pub fn all_owned(&self, keep_named: bool) -> Vec<(String, Option<String>)> {
        self.tabs_to_reap(Duration::ZERO, keep_named)
    }

    /// Remember the managed profile of the active browser, register this
    /// process as a session of it and refresh the shared activity stamp.
    pub fn set_profile_dir(&self, dir: &Path) {
        let changed = {
            let mut g = self.lock();
            let changed = g.profile_dir.as_deref() != Some(dir);
            if changed {
                g.profile_dir = Some(dir.to_path_buf());
                g.last_file_touch = None;
            }
            changed
        };
        if changed {
            register_session(dir);
        }
        self.touch_activity_file();
    }

    pub fn profile_dir(&self) -> Option<PathBuf> {
        self.lock().profile_dir.clone()
    }

    /// Stamp the cross-process activity file (throttled) so other MCP servers
    /// sharing this browser do not quit it underneath an active session.
    fn touch_activity_file(&self) {
        let dir = {
            let mut g = self.lock();
            let Some(dir) = g.profile_dir.clone() else {
                return;
            };
            let now = g.now();
            if g.last_file_touch
                .is_some_and(|t| now.saturating_duration_since(t) < ACTIVITY_FILE_THROTTLE)
            {
                return;
            }
            g.last_file_touch = Some(now);
            dir
        };
        write_activity(&dir, crate::registry::now_epoch_s());
    }
}

/// Write the activity stamp (epoch seconds) into a profile dir.
pub fn write_activity(profile_dir: &Path, epoch_s: i64) {
    let _ = std::fs::write(profile_dir.join(ACTIVITY_FILE), epoch_s.to_string());
}

/// Seconds since another (or this) MCP server last stamped activity in this
/// profile; `None` when no stamp exists.
pub fn activity_age(profile_dir: &Path, now_epoch_s: i64) -> Option<Duration> {
    let text = std::fs::read_to_string(profile_dir.join(ACTIVITY_FILE)).ok()?;
    let ts: i64 = text.trim().parse().ok()?;
    Some(Duration::from_secs(
        now_epoch_s.saturating_sub(ts).max(0) as u64
    ))
}

fn register_session(profile_dir: &Path) {
    let dir = profile_dir.join(SESSIONS_DIR);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(std::process::id().to_string()), b"");
    }
}

fn unregister_session(profile_dir: &Path) {
    let _ = std::fs::remove_file(
        profile_dir
            .join(SESSIONS_DIR)
            .join(std::process::id().to_string()),
    );
}

/// True when another live MCP server is registered against this profile.
/// Stale files of dead processes are removed.
pub fn other_sessions_alive(profile_dir: &Path) -> bool {
    let me = std::process::id();
    let Ok(rd) = std::fs::read_dir(profile_dir.join(SESSIONS_DIR)) else {
        return false;
    };
    let mut any = false;
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        if pid == me {
            continue;
        }
        if crate::registry::pid_alive(pid) {
            any = true;
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    any
}

// ---------------------------------------------------------------------------
// Idle-quit marker: tells other MCP servers that a browser vanished because of
// an idle quit (so they relaunch it instead of reporting a crash).
// ---------------------------------------------------------------------------

fn idle_quit_marker(name: &str) -> Result<PathBuf> {
    let dir = crate::paths::data_dir()?.join("idle-quit");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(name.replace(['/', '\\'], "_")))
}

pub fn mark_idle_quit(name: &str) {
    if let Ok(p) = idle_quit_marker(name) {
        let _ = std::fs::write(p, b"");
    }
}

pub fn take_idle_quit_marker(name: &str) -> bool {
    idle_quit_marker(name)
        .map(|p| std::fs::remove_file(p).is_ok())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Placeholder ("start on demand") browser.
// ---------------------------------------------------------------------------

/// A `ResolvedBrowser` meaning "launch `kind` when the first tool needs it".
pub fn pending_browser(kind: Kind) -> ResolvedBrowser {
    ResolvedBrowser {
        endpoint: PENDING_ENDPOINT.to_string(),
        engine: if kind == Kind::Firefox {
            Engine::Bidi
        } else {
            Engine::Cdp
        },
        source: Source::Registered {
            name: format!("{}-pending", kind.as_str()),
        },
    }
}

pub fn is_pending(b: &ResolvedBrowser) -> bool {
    b.endpoint == PENDING_ENDPOINT
}

fn pending_kind(b: &ResolvedBrowser) -> Option<Kind> {
    match &b.source {
        Source::Registered { name } => name.split_once('-').and_then(|(p, _)| Kind::parse(p)),
        Source::External => None,
    }
}

fn process_gone(pid: u32) -> bool {
    let p = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    match sys.process(p) {
        None => true,
        Some(proc_) => proc_.status() == sysinfo::ProcessStatus::Zombie,
    }
}

// ---------------------------------------------------------------------------
// ServerState operations
// ---------------------------------------------------------------------------

impl ServerState {
    /// Create a tab and record this server as its owner.
    pub async fn create_owned_tab(
        &self,
        backend: &TabBackend,
        url: &str,
        named: Option<&str>,
    ) -> Result<String> {
        let tid = backend.create_tab(url).await?;
        self.lifecycle.track(&tid, named.map(String::from));
        Ok(tid)
    }

    /// Settings in force: the test override, else config + env.
    pub fn lifecycle_settings(&self) -> LifecycleSettings {
        self.settings_override
            .unwrap_or_else(LifecycleSettings::current)
    }

    /// Replace the placeholder browser with a freshly started one.
    pub async fn start_pending_browser(&self) -> Result<ResolvedBrowser> {
        let _g = self.start_lock.lock().await;
        let cur = self.browser_snapshot().await;
        if !is_pending(&cur) {
            return Ok(cur);
        }
        let kind = pending_kind(&cur)
            .ok_or_else(|| anyhow::anyhow!("pending browser has no recognizable kind"))?;
        let resolved = match &self.launcher {
            Some(launch) => launch(kind).await?,
            None => {
                let started = crate::cli::start::ensure_started(
                    Some(kind.as_str().to_string()),
                    false,
                    false,
                    30,
                )
                .await?;
                ResolvedBrowser {
                    endpoint: started.endpoint.clone(),
                    engine: started.engine,
                    source: Source::Registered {
                        name: started.name.clone(),
                    },
                }
            }
        };
        *self.browser.write().await = resolved.clone();
        self.reset_connection_state().await;
        Ok(resolved)
    }

    /// Drop everything bound to the previous browser process.
    pub async fn reset_connection_state(&self) {
        if let Some(client) = self.bidi.lock().await.take() {
            let _ = client.session_end().await;
        }
        *self.backend.lock().await = None;
        *self.bidi_lock.lock().await = crate::mcp::server::BidiLockState::Pending;
        *self.active_target_id.lock().await = None;
        self.origin_target_ids.lock().await.clear();
        self.lifecycle.clear_tabs();
        if let Some(sc) = self.sidecar.lock().await.take() {
            let _ = sc.call("dispose", serde_json::json!({})).await;
        }
    }

    /// Close the given owned tabs. Never closes the browser's last page: it
    /// is navigated to `about:blank` instead so the window survives.
    /// Returns the number of tabs closed or blanked.
    pub async fn close_owned_tabs(
        &self,
        backend: &TabBackend,
        ids: Vec<(String, Option<String>)>,
    ) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let live = match backend.live_targets().await {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(target = "lifecycle", error = %e, "cannot list targets; skipping tab close");
                return 0;
            }
        };
        let live_ids: std::collections::HashSet<&str> =
            live.iter().map(|t| t.id.as_str()).collect();
        let mut to_close: Vec<(String, Option<String>)> = Vec::new();
        for (id, named) in ids {
            if live_ids.contains(id.as_str()) {
                to_close.push((id, named));
            } else {
                // Already gone: just forget it.
                self.forget_tab(&id, named.as_deref()).await;
            }
        }
        let spare = if !to_close.is_empty() && to_close.len() >= live.len() {
            to_close.pop()
        } else {
            None
        };
        let mut n = 0;
        for (id, named) in to_close {
            if backend.close_tab(&id).await.is_ok() {
                n += 1;
            }
            self.forget_tab(&id, named.as_deref()).await;
        }
        if let Some((id, named)) = spare {
            let blank = live
                .iter()
                .find(|t| t.id == id)
                .is_some_and(|t| t.url == "about:blank");
            if blank || backend.navigate(&id, "about:blank").await.is_ok() {
                n += 1;
            }
            self.forget_tab(&id, named.as_deref()).await;
        }
        n
    }

    async fn forget_tab(&self, id: &str, named: Option<&str>) {
        self.lifecycle.untrack(id);
        {
            let mut ptr = self.active_target_id.lock().await;
            if ptr.as_deref() == Some(id) {
                *ptr = None;
            }
        }
        self.origin_target_ids.lock().await.retain(|_, v| v != id);
        if let Some(name) = named {
            if let Ok(bn) = self.registered_browser_name().await {
                let name = name.to_string();
                let _ = crate::mcp::server::sync_registry_op(move |reg| reg.tab_delete(&bn, &name))
                    .await;
            }
        }
    }

    /// Close owned tabs idle for at least `idle`. Never opens a connection:
    /// with no cached backend there is nothing of ours to close. Skips while
    /// any tool call is in flight (a `wait_for_cookie` or other user-wait).
    pub async fn reap_idle_tabs(&self, idle: Duration, keep_named: bool) -> usize {
        if self.lifecycle.in_flight() > 0 {
            return 0;
        }
        let due = self.lifecycle.tabs_to_reap(idle, keep_named);
        if due.is_empty() {
            return 0;
        }
        let Some(backend) = self.backend.lock().await.clone() else {
            return 0;
        };
        self.close_owned_tabs(&backend, due).await
    }

    /// Quit the active browser if it is browser-control-launched and has been
    /// idle for `idle` across every MCP server sharing it. Returns true if it
    /// was quit. The next tool call relaunches it.
    pub async fn quit_browser_if_idle(&self, idle: Duration) -> Result<bool> {
        if self.lifecycle.in_flight() > 0 || self.lifecycle.idle_for() < idle {
            return Ok(false);
        }
        let Some(row) = self.active_managed_row().await else {
            return Ok(false);
        };
        if let Some(age) = activity_age(&row.profile_dir, crate::registry::now_epoch_s()) {
            if age < idle {
                return Ok(false);
            }
        }
        // Drain in-flight work and block new calls while we tear down.
        let _barrier = self.op_barrier.write().await;
        if self.lifecycle.in_flight() > 0 || self.lifecycle.idle_for() < idle {
            return Ok(false);
        }
        self.quit_browser_row(&row, true).await?;
        Ok(true)
    }

    /// The registry row of the active browser when it is a live
    /// browser-control-launched, managed, quit-capable browser.
    async fn active_managed_row(&self) -> Option<crate::registry::BrowserRow> {
        let resolved = self.browser_snapshot().await;
        if is_pending(&resolved) {
            return None;
        }
        let Source::Registered { name } = resolved.source else {
            return None;
        };
        let row = crate::mcp::server::sync_registry_op(move |reg| reg.get_by_name(&name))
            .await
            .ok()
            .flatten()?;
        if row.kind == Kind::Obscura
            || crate::registry::liveness(&row) == crate::registry::BrowserLiveness::DeadPid
            || !crate::launch::profile::is_managed_profile(&row.profile_dir)
        {
            return None;
        }
        Some(row)
    }

    /// Ask the browser to exit, wait for the process, drop its registry row,
    /// and (when `relaunch_on_demand`) arm lazy relaunch.
    async fn quit_browser_row(
        &self,
        row: &crate::registry::BrowserRow,
        relaunch_on_demand: bool,
    ) -> Result<()> {
        let backend = match self.backend.lock().await.clone() {
            Some(b) => b,
            None => crate::session::backend::open_backend(&row.endpoint, row.engine).await?,
        };
        mark_idle_quit(&row.name);
        // The connection usually drops before a reply arrives; that is fine.
        let _ = tokio::time::timeout(Duration::from_secs(5), backend.close_browser()).await;
        let deadline = std::time::Instant::now() + QUIT_WAIT;
        while !process_gone(row.pid) {
            if std::time::Instant::now() >= deadline {
                take_idle_quit_marker(&row.name);
                anyhow::bail!(
                    "browser `{}` (pid {}) did not exit after Browser.close",
                    row.name,
                    row.pid
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let name = row.name.clone();
        let _ = crate::mcp::server::sync_registry_op(move |reg| reg.delete(&name)).await;
        unregister_session(&row.profile_dir);
        self.reset_connection_state().await;
        if relaunch_on_demand {
            *self.browser.write().await = pending_browser(row.kind);
            // Registration against the new browser happens when it starts.
            self.lifecycle.clear_profile();
        }
        Ok(())
    }

    /// Session end: close what this server opened, and quit the browser if
    /// this process launched it and no other MCP server is using it.
    /// Bounded by [`SHUTDOWN_BUDGET`]; never opens a new connection just to
    /// clean up.
    pub async fn shutdown_cleanup(&self) {
        let _ = tokio::time::timeout(SHUTDOWN_BUDGET, self.shutdown_cleanup_inner()).await;
    }

    async fn shutdown_cleanup_inner(&self) {
        let settings = self.lifecycle_settings();
        // Let calls that are still running finish so none can open a tab after
        // we have cleaned up. Bounded: a long user-wait must not stall exit.
        let wait_until = std::time::Instant::now() + IN_FLIGHT_GRACE;
        while self.lifecycle.in_flight() > 0 && std::time::Instant::now() < wait_until {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let backend = self.backend.lock().await.clone();
        if let Some(backend) = backend {
            let ids = self.lifecycle.all_owned(settings.keep_named_tabs);
            let n = self.close_owned_tabs(&backend, ids).await;
            if n > 0 {
                tracing::debug!(
                    target = "lifecycle",
                    closed = n,
                    "closed MCP-owned tabs at session end"
                );
            }
        }
        let row = self.active_managed_row().await;
        if let Some(row) = row {
            let launched_here = crate::launch::launched_by_this_process(&row.name);
            unregister_session(&row.profile_dir);
            if launched_here
                && settings.browser_idle_quit != crate::config::IdleMinutes::Off
                && !other_sessions_alive(&row.profile_dir)
            {
                if let Err(e) = self.quit_browser_row(&row, false).await {
                    tracing::debug!(
                        target = "lifecycle",
                        error = %e,
                        "browser quit at session end failed"
                    );
                }
            }
        }
    }

    /// One reaper pass: close idle tabs, then quit an idle browser.
    pub async fn reap_tick(&self) {
        let s = self.lifecycle_settings();
        if let Some(d) = s.tab_idle_close.duration() {
            let n = self.reap_idle_tabs(d, s.keep_named_tabs).await;
            if n > 0 {
                tracing::debug!(
                    target = "lifecycle",
                    closed = n,
                    "closed idle MCP-owned tabs"
                );
            }
        }
        if let Some(d) = s.browser_idle_quit.duration() {
            match self.quit_browser_if_idle(d).await {
                Ok(true) => tracing::debug!(target = "lifecycle", "quit idle browser"),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(target = "lifecycle", error = %e, "idle browser quit failed")
                }
            }
        }
    }

    /// Spawn the periodic reaper. Abort the handle to stop it.
    pub fn spawn_reaper(&self, tick: Duration) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            iv.tick().await; // first tick fires immediately; skip it
            loop {
                iv.tick().await;
                state.reap_tick().await;
            }
        })
    }
}

impl Lifecycle {
    /// Forget the registered profile (the browser it belonged to is gone).
    pub fn clear_profile(&self) {
        let mut g = self.lock();
        g.profile_dir = None;
        g.last_file_touch = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IdleMinutes, LifecycleSettings};
    use crate::detect::Engine;
    use crate::registry::{BrowserRow, Registry};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_tungstenite::tungstenite::Message;

    const MIN: Duration = Duration::from_secs(60);

    // -- pure tracker ------------------------------------------------------

    #[test]
    fn tracker_reaps_only_idle_tabs_and_touch_resets_the_timer() {
        let lc = Lifecycle::new();
        lc.track("A", None);
        lc.track("B", None);
        assert!(lc.tabs_to_reap(10 * MIN, false).is_empty());
        lc.advance(6 * MIN);
        lc.touch_tab("A");
        lc.advance(5 * MIN);
        let due: Vec<_> = lc
            .tabs_to_reap(10 * MIN, false)
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(due, vec!["B"]);
        lc.advance(6 * MIN);
        assert_eq!(lc.tabs_to_reap(10 * MIN, false).len(), 2);
        lc.untrack("B");
        assert_eq!(lc.tabs_to_reap(10 * MIN, false).len(), 1);
        // Touching a tab we do not own does not start tracking it.
        lc.touch_tab("USER");
        assert!(!lc.is_owned("USER"));
    }

    #[test]
    fn tracker_keep_named_spares_named_tabs() {
        let lc = Lifecycle::new();
        lc.track("A", None);
        lc.track("N", Some("work".into()));
        lc.advance(30 * MIN);
        assert_eq!(lc.tabs_to_reap(10 * MIN, true).len(), 1);
        assert_eq!(lc.tabs_to_reap(10 * MIN, false).len(), 2);
        assert_eq!(lc.all_owned(true).len(), 1);
        assert_eq!(lc.all_owned(false).len(), 2);
    }

    #[test]
    fn call_guard_counts_in_flight_and_stamps_activity() {
        let lc = Lifecycle::new();
        lc.advance(20 * MIN);
        assert!(lc.idle_for() >= 20 * MIN);
        let g = lc.begin();
        assert_eq!(lc.in_flight(), 1);
        assert!(lc.idle_for() < MIN);
        lc.advance(5 * MIN);
        drop(g);
        assert_eq!(lc.in_flight(), 0);
        assert!(lc.idle_for() < MIN, "finishing a call counts as activity");
    }

    #[test]
    fn activity_file_round_trip_and_session_files() {
        let td = TempDir::new().unwrap();
        assert!(activity_age(td.path(), 1000).is_none());
        write_activity(td.path(), 940);
        assert_eq!(activity_age(td.path(), 1000), Some(Duration::from_secs(60)));
        // Future stamps clamp to zero.
        assert_eq!(activity_age(td.path(), 900), Some(Duration::ZERO));

        assert!(!other_sessions_alive(td.path()));
        let dir = td.path().join(SESSIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(std::process::id().to_string()), b"").unwrap();
        assert!(!other_sessions_alive(td.path()), "own pid does not count");
        // A pid that cannot exist is pruned.
        std::fs::write(dir.join("4294967"), b"").unwrap();
        assert!(!other_sessions_alive(td.path()));
        assert!(!dir.join("4294967").exists());
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        std::fs::write(dir.join(child.id().to_string()), b"").unwrap();
        assert!(other_sessions_alive(td.path()));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn pending_browser_round_trips_kind() {
        let b = pending_browser(Kind::Brave);
        assert!(is_pending(&b));
        assert_eq!(pending_kind(&b), Some(Kind::Brave));
        assert_eq!(pending_browser(Kind::Firefox).engine, Engine::Bidi);
    }

    // -- CDP mock ----------------------------------------------------------

    struct Mock {
        endpoint: String,
        port: u16,
        /// (target id, url) of every live page.
        targets: Arc<AsyncMutex<Vec<(String, String)>>>,
        closed: Arc<AsyncMutex<Vec<String>>>,
        browser_closed: Arc<AtomicBool>,
    }

    impl Mock {
        async fn ids(&self) -> Vec<String> {
            self.targets
                .lock()
                .await
                .iter()
                .map(|t| t.0.clone())
                .collect()
        }
    }

    async fn spawn_mock(preseed: &[&str]) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let targets = Arc::new(AsyncMutex::new(
            preseed
                .iter()
                .map(|t| (t.to_string(), "https://user.example/".to_string()))
                .collect::<Vec<_>>(),
        ));
        let closed = Arc::new(AsyncMutex::new(Vec::new()));
        let browser_closed = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(AsyncMutex::new(0u32));
        tokio::spawn({
            let targets = targets.clone();
            let closed = closed.clone();
            let browser_closed = browser_closed.clone();
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let targets = targets.clone();
                    let closed = closed.clone();
                    let browser_closed = browser_closed.clone();
                    let counter = counter.clone();
                    tokio::spawn(async move {
                        // Liveness probes connect without speaking WebSocket.
                        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let mut sessions: std::collections::HashMap<String, String> =
                            Default::default();
                        let mut next_session = 0u32;
                        while let Some(Ok(Message::Text(t))) = ws.next().await {
                            let req: Value = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let p = &req["params"];
                            let result = match req["method"].as_str().unwrap_or("") {
                                "Target.createTarget" => {
                                    let mut n = counter.lock().await;
                                    *n += 1;
                                    let tid = format!("NEW{n}");
                                    let url = p["url"].as_str().unwrap_or("about:blank");
                                    targets.lock().await.push((tid.clone(), url.to_string()));
                                    json!({"targetId": tid})
                                }
                                "Target.getTargets" => json!({
                                    "targetInfos": targets.lock().await.iter().map(|(t, u)| json!({
                                        "targetId": t, "type": "page", "url": u, "title": "",
                                    })).collect::<Vec<_>>()
                                }),
                                "Target.closeTarget" => {
                                    let tid = p["targetId"].as_str().unwrap_or("").to_string();
                                    targets.lock().await.retain(|t| t.0 != tid);
                                    closed.lock().await.push(tid);
                                    json!({"success": true})
                                }
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    let sid = format!("S{next_session}");
                                    sessions.insert(
                                        sid.clone(),
                                        p["targetId"].as_str().unwrap_or("").to_string(),
                                    );
                                    json!({"sessionId": sid})
                                }
                                "Page.navigate" => {
                                    let sid = req["sessionId"].as_str().unwrap_or("");
                                    if let Some(tid) = sessions.get(sid) {
                                        let url = p["url"].as_str().unwrap_or("").to_string();
                                        for t in targets.lock().await.iter_mut() {
                                            if &t.0 == tid {
                                                t.1 = url.clone();
                                            }
                                        }
                                    }
                                    json!({"frameId": "F"})
                                }
                                "Browser.close" => {
                                    browser_closed.store(true, Ordering::SeqCst);
                                    json!({})
                                }
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            if ws.send(Message::Text(resp.to_string())).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            }
        });
        Mock {
            endpoint: format!("ws://127.0.0.1:{port}"),
            port,
            targets,
            closed,
            browser_closed,
        }
    }

    fn settings(tab_min: u64, quit: IdleMinutes, keep_named: bool) -> LifecycleSettings {
        LifecycleSettings {
            tab_idle_close: IdleMinutes::Minutes(tab_min),
            browser_idle_quit: quit,
            keep_named_tabs: keep_named,
        }
    }

    fn external_state(endpoint: &str) -> ServerState {
        ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: endpoint.to_string(),
            source: Source::External,
        })
        .with_lifecycle_settings(settings(10, IdleMinutes::Minutes(15), false))
    }

    // -- tab ownership and reaping ------------------------------------------

    #[tokio::test]
    async fn reaper_closes_only_owned_idle_tabs() {
        let mock = spawn_mock(&["USER1"]).await;
        let state = external_state(&mock.endpoint);
        let backend = state.ensure_backend().await.unwrap();
        let a = state
            .create_owned_tab(&backend, "https://a.example/", None)
            .await
            .unwrap();
        let b = state
            .create_owned_tab(&backend, "https://b.example/", None)
            .await
            .unwrap();

        state.lifecycle.advance(6 * MIN);
        state.lifecycle.touch_tab(&a);
        state.lifecycle.advance(5 * MIN);
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 1);
        assert_eq!(*mock.closed.lock().await, vec![b.clone()]);
        assert_eq!(mock.ids().await, vec!["USER1".to_string(), a.clone()]);

        // A running tool call (e.g. a user-wait) suspends reaping.
        state.lifecycle.advance(60 * MIN);
        let guard = state.lifecycle.begin();
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 0);
        state.lifecycle.advance(60 * MIN);
        drop(guard);
        state.lifecycle.advance(60 * MIN);
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 1);
        // The tab browser-control did not create is still there.
        assert_eq!(mock.ids().await, vec!["USER1".to_string()]);
        assert_eq!(state.lifecycle.owned_count(), 0);
    }

    #[tokio::test]
    async fn closed_active_tab_clears_the_active_pointer() {
        let mock = spawn_mock(&["USER1"]).await;
        let state = external_state(&mock.endpoint);
        let (_, tid) = state.current_tab().await.unwrap();
        assert!(state.lifecycle.is_owned(&tid));
        state.lifecycle.advance(11 * MIN);
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 1);
        assert!(state.active_target_id.lock().await.is_none());
        // Next use recreates a blank tab transparently.
        let (_, again) = state.current_tab().await.unwrap();
        assert_ne!(again, tid);
    }

    #[tokio::test]
    async fn named_tabs_are_kept_only_when_asked() {
        let mock = spawn_mock(&["USER1"]).await;
        let state = external_state(&mock.endpoint);
        let backend = state.ensure_backend().await.unwrap();
        let n = state
            .create_owned_tab(&backend, "https://n.example/", Some("work"))
            .await
            .unwrap();
        state.lifecycle.advance(30 * MIN);
        assert_eq!(state.reap_idle_tabs(10 * MIN, true).await, 0);
        assert!(mock.ids().await.contains(&n));
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 1);
        assert!(!mock.ids().await.contains(&n));
    }

    #[tokio::test]
    async fn the_last_page_is_blanked_not_closed() {
        let mock = spawn_mock(&[]).await;
        let state = external_state(&mock.endpoint);
        let backend = state.ensure_backend().await.unwrap();
        state
            .create_owned_tab(&backend, "https://a.example/", None)
            .await
            .unwrap();
        state
            .create_owned_tab(&backend, "https://b.example/", None)
            .await
            .unwrap();
        state.lifecycle.advance(30 * MIN);
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 2);
        assert_eq!(mock.closed.lock().await.len(), 1);
        let left = mock.targets.lock().await.clone();
        assert_eq!(left.len(), 1, "one window must survive");
        assert_eq!(left[0].1, "about:blank");
        assert_eq!(state.lifecycle.owned_count(), 0);
    }

    #[tokio::test]
    async fn reaper_never_opens_a_connection() {
        // Nothing is listening here; with no cached backend the reaper must
        // return immediately instead of dialing.
        let state = external_state("ws://127.0.0.1:1");
        state.lifecycle.track("A", None);
        state.lifecycle.advance(30 * MIN);
        assert_eq!(state.reap_idle_tabs(10 * MIN, false).await, 0);
        state.shutdown_cleanup().await;
    }

    #[tokio::test]
    async fn shutdown_cleanup_closes_owned_tabs_but_not_the_users() {
        let mock = spawn_mock(&["USER1", "USER2"]).await;
        let state = external_state(&mock.endpoint);
        let backend = state.ensure_backend().await.unwrap();
        let a = state
            .create_owned_tab(&backend, "https://a.example/", None)
            .await
            .unwrap();
        let n = state
            .create_owned_tab(&backend, "https://n.example/", Some("work"))
            .await
            .unwrap();
        state.shutdown_cleanup().await;
        let mut closed = mock.closed.lock().await.clone();
        closed.sort();
        let mut want = vec![a, n];
        want.sort();
        assert_eq!(closed, want, "named tabs close by default");
        assert_eq!(
            mock.ids().await,
            vec!["USER1".to_string(), "USER2".to_string()]
        );
        // External endpoints are never quit.
        assert!(!mock.browser_closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_cleanup_keeps_named_tabs_when_configured() {
        let mock = spawn_mock(&["USER1"]).await;
        let state = external_state(&mock.endpoint).with_lifecycle_settings(settings(
            10,
            IdleMinutes::Off,
            true,
        ));
        let backend = state.ensure_backend().await.unwrap();
        let n = state
            .create_owned_tab(&backend, "https://n.example/", Some("work"))
            .await
            .unwrap();
        let a = state
            .create_owned_tab(&backend, "https://a.example/", None)
            .await
            .unwrap();
        state.shutdown_cleanup().await;
        assert_eq!(*mock.closed.lock().await, vec![a]);
        assert!(mock.ids().await.contains(&n));
    }

    #[test]
    fn session_end_over_stdio_closes_tabs_the_session_opened() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cfg = TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", cfg.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use tokio::io::AsyncWriteExt;
            let mock = spawn_mock(&["USER1"]).await;
            let state = external_state(&mock.endpoint);
            let tools = crate::mcp::server::ToolRegistry::new();
            crate::mcp::tools::register_all(&tools);
            let (mut client_w, server_r) = tokio::io::duplex(8192);
            let (server_w, _client_r) = tokio::io::duplex(8192);
            let join = tokio::spawn(crate::mcp::server::run_with_streams(
                state, tools, server_r, server_w,
            ));
            for req in [
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_tab_new","arguments":{"url":"https://jobs.example/1"}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_tab_new","arguments":{"url":"https://jobs.example/2"}}}),
            ] {
                let mut b = serde_json::to_vec(&req).unwrap();
                b.push(b'\n');
                client_w.write_all(&b).await.unwrap();
            }
            // Session ends: the client goes away.
            drop(client_w);
            join.await.unwrap().unwrap();
            assert_eq!(mock.ids().await, vec!["USER1".to_string()]);
            assert_eq!(mock.closed.lock().await.len(), 2);
        });
        std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
        std::env::remove_var(crate::config::TAB_POLICY_ENV);
    }

    // -- idle browser quit and relaunch --------------------------------------

    struct QuitEnv {
        _cfg: TempDir,
        _data: TempDir,
        profile: PathBuf,
        mock: Mock,
        child: Option<std::process::Child>,
        name: String,
    }

    impl QuitEnv {
        async fn new(name: &str, managed: bool) -> Self {
            let cfg = TempDir::new().unwrap();
            let data = TempDir::new().unwrap();
            std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", cfg.path());
            std::env::set_var("BROWSER_CONTROL_DATA_DIR", data.path());
            let profile = if managed {
                cfg.path().join("profiles/brave/default")
            } else {
                data.path().join("users-own-brave")
            };
            std::fs::create_dir_all(&profile).unwrap();
            let mock = spawn_mock(&["USER1"]).await;
            let child = std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .unwrap();
            let row = BrowserRow {
                name: name.to_string(),
                kind: Kind::Brave,
                engine: Engine::Cdp,
                pid: child.id(),
                endpoint: mock.endpoint.clone(),
                port: mock.port,
                profile_dir: profile.clone(),
                executable: PathBuf::from("/bin/true"),
                headless: false,
                started_at: "2026-01-01T00:00:00Z".into(),
            };
            Registry::open().unwrap().insert(&row).unwrap();
            Self {
                _cfg: cfg,
                _data: data,
                profile,
                mock,
                child: Some(child),
                name: name.to_string(),
            }
        }

        /// Make the fake browser exit when it is asked to close.
        fn die_on_close(&mut self) {
            let mut child = self.child.take().unwrap();
            let flag = self.mock.browser_closed.clone();
            tokio::spawn(async move {
                while !flag.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }

        fn state(&self) -> ServerState {
            ServerState::new(ResolvedBrowser {
                engine: Engine::Cdp,
                endpoint: self.mock.endpoint.clone(),
                source: Source::Registered {
                    name: self.name.clone(),
                },
            })
            .with_lifecycle_settings(settings(10, IdleMinutes::Minutes(15), false))
        }
    }

    impl Drop for QuitEnv {
        fn drop(&mut self) {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
            std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn idle_browser_is_quit_then_relaunched_on_the_next_call() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            let mut env = QuitEnv::new("brave-old", true).await;
            env.die_on_close();
            let mut state = env.state();
            // Fake launcher: registers a second "browser" and hands it back.
            let second = spawn_mock(&["FRESH_BLANK"]).await;
            let second_child = std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .unwrap();
            let second_pid = second_child.id();
            let second_endpoint = second.endpoint.clone();
            let second_port = second.port;
            let profile = env.profile.clone();
            state.launcher = Some(Arc::new(move |kind| {
                let endpoint = second_endpoint.clone();
                let profile = profile.clone();
                Box::pin(async move {
                    assert_eq!(kind, Kind::Brave);
                    Registry::open()?.insert(&BrowserRow {
                        name: "brave-new".into(),
                        kind,
                        engine: Engine::Cdp,
                        pid: second_pid,
                        endpoint: endpoint.clone(),
                        port: second_port,
                        profile_dir: profile,
                        executable: PathBuf::from("/bin/true"),
                        headless: false,
                        started_at: "2026-01-01T00:00:01Z".into(),
                    })?;
                    Ok(ResolvedBrowser {
                        engine: Engine::Cdp,
                        endpoint,
                        source: Source::Registered {
                            name: "brave-new".into(),
                        },
                    })
                })
            }));

            // Use the browser, then go quiet.
            let backend = state.ensure_backend().await.unwrap();
            state
                .create_owned_tab(&backend, "https://a.example/", None)
                .await
                .unwrap();
            assert!(
                !state.quit_browser_if_idle(15 * MIN).await.unwrap(),
                "not idle yet"
            );
            state.lifecycle.advance(14 * MIN);
            assert!(!state.quit_browser_if_idle(15 * MIN).await.unwrap());
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
            state.lifecycle.advance(2 * MIN);
            // Another MCP server on the same browser was active a moment ago.
            write_activity(&env.profile, crate::registry::now_epoch_s());
            assert!(!state.quit_browser_if_idle(15 * MIN).await.unwrap());
            write_activity(&env.profile, crate::registry::now_epoch_s() - 20 * 60);
            assert!(state.quit_browser_if_idle(15 * MIN).await.unwrap());

            assert!(
                env.mock.browser_closed.load(Ordering::SeqCst),
                "Browser.close sent"
            );
            assert!(Registry::open()
                .unwrap()
                .get_by_name("brave-old")
                .unwrap()
                .is_none());
            assert!(is_pending(&state.browser_snapshot().await));
            assert!(state.backend.lock().await.is_none());
            assert_eq!(state.lifecycle.owned_count(), 0);

            // Next tool call: a fresh browser starts and the call proceeds.
            let backend = state.ensure_backend().await.unwrap();
            let live = backend.live_target_ids().await.unwrap();
            assert!(live.contains("FRESH_BLANK"));
            assert_eq!(state.registered_browser_name().await.unwrap(), "brave-new");
            let mut c = second_child;
            let _ = c.kill();
            let _ = c.wait();
        });
    }

    #[test]
    fn other_server_relaunches_after_the_idle_quit_marker() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            let env = QuitEnv::new("brave-shared", true).await;
            let mut state_b = env.state();
            let fresh = spawn_mock(&["B_BLANK"]).await;
            let fresh_endpoint = fresh.endpoint.clone();
            state_b.launcher = Some(Arc::new(move |_kind| {
                let endpoint = fresh_endpoint.clone();
                Box::pin(async move {
                    Ok(ResolvedBrowser {
                        engine: Engine::Cdp,
                        endpoint,
                        source: Source::External,
                    })
                })
            }));
            // Server A quit the browser: row gone, marker left behind.
            Registry::open().unwrap().delete("brave-shared").unwrap();
            mark_idle_quit("brave-shared");
            let backend = state_b.ensure_backend().await.unwrap();
            assert!(backend.live_target_ids().await.unwrap().contains("B_BLANK"));
        });
    }

    #[test]
    fn an_unexpected_exit_is_still_reported_not_silently_relaunched() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            let mut env = QuitEnv::new("brave-crashed", true).await;
            let state = env.state();
            let mut c = env.child.take().unwrap();
            c.kill().unwrap();
            c.wait().unwrap();
            let err = state.ensure_backend().await.err().unwrap();
            assert!(format!("{err:#}").contains("has exited"));
        });
    }

    #[test]
    fn external_and_unmanaged_browsers_are_never_quit() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            // Unmanaged profile: a registered browser outside browser-control's tree.
            let env = QuitEnv::new("brave-users", false).await;
            let state = env.state();
            state.ensure_backend().await.unwrap();
            state.lifecycle.advance(60 * MIN);
            assert!(!state.quit_browser_if_idle(15 * MIN).await.unwrap());
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
            assert!(Registry::open()
                .unwrap()
                .get_by_name("brave-users")
                .unwrap()
                .is_some());

            // External endpoint (attached browser).
            let ext = external_state(&env.mock.endpoint);
            ext.ensure_backend().await.unwrap();
            ext.lifecycle.advance(60 * MIN);
            assert!(!ext.quit_browser_if_idle(15 * MIN).await.unwrap());
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn a_pending_tool_call_blocks_the_idle_quit() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            let env = QuitEnv::new("brave-wait", true).await;
            let state = env.state();
            state.ensure_backend().await.unwrap();
            state.lifecycle.advance(60 * MIN);
            write_activity(&env.profile, 0);
            // e.g. browser_wait_for_cookie while the user logs in.
            let guard = state.lifecycle.begin();
            state.lifecycle.advance(60 * MIN);
            assert!(!state.quit_browser_if_idle(15 * MIN).await.unwrap());
            drop(guard);
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn session_end_quits_only_a_browser_this_process_launched() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            // Launched by someone else: tabs close, browser stays.
            let env = QuitEnv::new("brave-theirs", true).await;
            let state = env.state();
            let backend = state.ensure_backend().await.unwrap();
            let t = state
                .create_owned_tab(&backend, "https://a.example/", None)
                .await
                .unwrap();
            state.shutdown_cleanup().await;
            assert!(!env.mock.ids().await.contains(&t));
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
            drop(env);

            // Launched here, but another MCP server still uses it.
            let mut env = QuitEnv::new("brave-mine", true).await;
            crate::launch::note_launched("brave-mine");
            let state = env.state();
            state.ensure_backend().await.unwrap();
            let other = std::process::Command::new("sleep").arg("60").spawn();
            let mut other = other.unwrap();
            std::fs::write(
                env.profile.join(SESSIONS_DIR).join(other.id().to_string()),
                b"",
            )
            .unwrap();
            state.shutdown_cleanup().await;
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
            other.kill().unwrap();
            other.wait().unwrap();

            // Launched here and alone: it quits with the session.
            env.die_on_close();
            let state = env.state();
            state.ensure_backend().await.unwrap();
            state.shutdown_cleanup().await;
            assert!(env.mock.browser_closed.load(Ordering::SeqCst));
            assert!(Registry::open()
                .unwrap()
                .get_by_name("brave-mine")
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn browser_idle_quit_off_keeps_the_browser_at_session_end() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            let env = QuitEnv::new("brave-keep", true).await;
            crate::launch::note_launched("brave-keep");
            let state = env
                .state()
                .with_lifecycle_settings(settings(10, IdleMinutes::Off, false));
            state.ensure_backend().await.unwrap();
            state.shutdown_cleanup().await;
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn reap_tick_honours_off_settings() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        runtime().block_on(async {
            let env = QuitEnv::new("brave-off", true).await;
            let state = env
                .state()
                .with_lifecycle_settings(settings(10, IdleMinutes::Off, false));
            let backend = state.ensure_backend().await.unwrap();
            state
                .create_owned_tab(&backend, "https://a.example/", None)
                .await
                .unwrap();
            let mut st = state.clone();
            st.settings_override = Some(LifecycleSettings {
                tab_idle_close: IdleMinutes::Off,
                browser_idle_quit: IdleMinutes::Off,
                keep_named_tabs: false,
            });
            state.lifecycle.advance(600 * MIN);
            st.reap_tick().await;
            assert!(env.mock.closed.lock().await.is_empty());
            assert!(!env.mock.browser_closed.load(Ordering::SeqCst));
            // With tab reaping on, the tick closes the idle tab.
            st.settings_override = Some(settings(10, IdleMinutes::Off, false));
            st.reap_tick().await;
            assert_eq!(env.mock.closed.lock().await.len(), 1);
        });
    }
}
