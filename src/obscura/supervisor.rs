//! `browser-control obscura-supervisor`: owns one `obscura serve` process.
//!
//! The supervisor is what the registry records as the "browser" process for
//! an Obscura entry. It
//!
//! 1. starts `obscura serve` on a private, OS-assigned loopback port with its
//!    own storage directory under the browser-control profile,
//! 2. holds the single upstream CDP connection through a [`Mux`], and
//! 3. serves the public loopback port that every browser-control client
//!    (native backend, Playwright sidecar, CLI) connects to.
//!
//! It exits when Obscura exits or the upstream connection drops, and on
//! SIGTERM/SIGINT it stops Obscura gracefully (SIGTERM, so Obscura flushes
//! cookies and localStorage to its storage dir) before exiting. On Linux the
//! child also gets `PR_SET_PDEATHSIG`, so a SIGKILLed supervisor does not
//! leave Obscura running; on macOS a SIGKILLed supervisor orphans it.
//!
//! Flags passed to Obscura, and why:
//! * `--host 127.0.0.1`: loopback only.
//! * `--storage-dir <profile>/storage`: a profile owned by browser-control,
//!   persistent across restarts like the Chromium profiles.
//! * `--allow-private-network`: agents routinely drive `localhost` dev
//!   servers; Obscura blocks loopback/RFC1918 fetches by default.
//! * `--allow-file-access`: needed for `DOM.setFileInputFiles`
//!   (`browser_set_input_files`) and `file://` navigation, which Chrome
//!   allows too. The CDP port is loopback-only and Origin/Host-checked.
//! * `--stealth` is deliberately NOT passed (see docs/obscura.md).
//!
//! `OBSCURA_TIMEZONE` defaults to the host zone (Obscura defaults to
//! `Europe/Berlin` otherwise) unless the caller already set it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

use super::mux::Mux;
use super::server::{self, VersionInfo};

/// How long to wait for `obscura serve` to answer `/json/version`.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Grace period between SIGTERM and SIGKILL when stopping Obscura (it
/// waits up to 3 s for open connections before flushing cookies).
const STOP_GRACE: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct SupervisorOpts {
    pub obscura: PathBuf,
    pub port: u16,
    pub profile_dir: PathBuf,
}

/// Arguments for `obscura serve` (exposed for tests and docs).
pub fn obscura_args(internal_port: u16, storage_dir: &Path) -> Vec<String> {
    vec![
        "serve".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        internal_port.to_string(),
        "--storage-dir".into(),
        storage_dir.display().to_string(),
        "--allow-private-network".into(),
        "--allow-file-access".into(),
    ]
}

/// IANA zone of the host, from `$TZ` or the `/etc/localtime` symlink.
pub fn host_timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':').trim();
        if !tz.is_empty() && !tz.starts_with('/') {
            return Some(tz.to_string());
        }
    }
    let target = std::fs::read_link("/etc/localtime").ok()?;
    zone_from_localtime_target(&target)
}

fn zone_from_localtime_target(target: &Path) -> Option<String> {
    let s = target.to_string_lossy();
    let idx = s.find("zoneinfo/")?;
    let zone = &s[idx + "zoneinfo/".len()..];
    (!zone.is_empty()).then(|| zone.to_string())
}

pub async fn run(opts: SupervisorOpts) -> Result<()> {
    let internal_port = crate::launch::allocate_free_port().context("allocating obscura port")?;
    let storage = opts.profile_dir.join("storage");
    std::fs::create_dir_all(&storage).with_context(|| format!("creating {}", storage.display()))?;

    let mut cmd = Command::new(&opts.obscura);
    cmd.args(obscura_args(internal_port, &storage))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if std::env::var_os("OBSCURA_TIMEZONE").is_none() {
        if let Some(tz) = host_timezone() {
            cmd.env("OBSCURA_TIMEZONE", tz);
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl is async-signal-safe; this only asks the kernel to
        // signal the child when the supervisor dies.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", opts.obscura.display()))?;

    let result = supervise(&opts, internal_port, &mut child).await;
    stop_child(&mut child).await;
    result
}

async fn supervise(opts: &SupervisorOpts, internal_port: u16, child: &mut Child) -> Result<()> {
    let upstream_version = wait_for_obscura(internal_port, child).await?;
    let upstream_ws = upstream_version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("ws://127.0.0.1:{internal_port}/devtools/browser"));
    let (mux, upstream_closed) = Mux::connect(&upstream_ws, Some(super::DIALOG_SHIM_JS)).await?;
    let result = serve_until_done(
        opts,
        internal_port,
        child,
        upstream_version,
        &mux,
        upstream_closed,
    )
    .await;
    // Release the upstream connection first so Obscura's SIGTERM handler
    // does not wait on it before persisting cookies.
    mux.close();
    result
}

async fn serve_until_done(
    opts: &SupervisorOpts,
    internal_port: u16,
    child: &mut Child,
    upstream_version: Value,
    mux: &Arc<Mux>,
    upstream_closed: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", opts.port))?;
    let version = VersionInfo {
        upstream: upstream_version,
        obscura_version: obscura_version(&opts.obscura).unwrap_or_else(|| "unknown".into()),
    };
    eprintln!(
        "browser-control: obscura supervisor pid {} serving ws://127.0.0.1:{}/devtools/browser (obscura pid {:?}, internal port {internal_port})",
        std::process::id(),
        opts.port,
        child.id()
    );

    tokio::select! {
        r = server::serve(listener, Arc::clone(mux), version) => {
            r.context("serving CDP clients")?;
        }
        status = child.wait() => {
            let status = status.context("waiting for obscura")?;
            bail!("obscura exited ({status})");
        }
        _ = upstream_closed => {
            bail!("obscura closed the CDP connection");
        }
        sig = shutdown_signal() => {
            eprintln!("browser-control: obscura supervisor received {sig}; stopping obscura");
        }
    }
    Ok(())
}

async fn wait_for_obscura(port: u16, child: &mut Child) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()?;
    let url = format!("http://127.0.0.1:{port}/json/version");
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("obscura exited before its CDP server came up ({status})");
        }
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<Value>().await {
                    return Ok(v);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for obscura on port {port}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn obscura_version(exe: &Path) -> Option<String> {
    use crate::detect::Probe;
    crate::detect::RealProbe.run_version(exe)
}

async fn stop_child(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on our own child.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
        _ = hup.recv() => "SIGHUP",
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl-C"
}

/// Path of the executable to run as the supervisor: the current
/// browser-control binary, or `BROWSER_CONTROL_SUPERVISOR_EXE` (used by
/// in-process tests whose `current_exe` is the test harness).
pub fn supervisor_exe() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("BROWSER_CONTROL_SUPERVISOR_EXE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    std::env::current_exe().map_err(|e| anyhow!("locating browser-control executable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_bind_loopback_and_never_enable_stealth() {
        let args = obscura_args(4100, Path::new("/tmp/p/storage"));
        assert_eq!(args[0], "serve");
        let joined = args.join(" ");
        assert!(joined.contains("--host 127.0.0.1"));
        assert!(joined.contains("--port 4100"));
        assert!(joined.contains("--storage-dir /tmp/p/storage"));
        assert!(joined.contains("--allow-private-network"));
        assert!(joined.contains("--allow-file-access"));
        assert!(!joined.contains("stealth"));
        assert!(!joined.contains("proxy"));
    }

    #[test]
    fn zone_from_symlink_target() {
        assert_eq!(
            zone_from_localtime_target(Path::new("/var/db/timezone/zoneinfo/America/Chicago"))
                .as_deref(),
            Some("America/Chicago")
        );
        assert_eq!(
            zone_from_localtime_target(Path::new("../usr/share/zoneinfo/Europe/Paris")).as_deref(),
            Some("Europe/Paris")
        );
        assert_eq!(zone_from_localtime_target(Path::new("/etc/foo")), None);
    }
}
