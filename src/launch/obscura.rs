//! Obscura launcher: starts the `obscura-supervisor` (which runs `obscura
//! serve` behind a CDP multiplexer, see [`crate::obscura`]) and waits for
//! the public loopback endpoint.
//!
//! Obscura is always headless; `LaunchOpts::headless` is ignored. Nothing
//! here touches other browsers: the supervisor only ever starts its own
//! `obscura` child on an OS-assigned port with its own storage directory.

use std::fs::File;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{
    allocate_free_port, configure_session_detachment, wait_for_endpoint, LaunchOpts, LaunchedHandle,
};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating CDP port")?;
    std::fs::create_dir_all(&opts.profile_dir)
        .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;

    let log_path = opts.profile_dir.join("browser.log");
    let log_file =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let log_clone = log_file
        .try_clone()
        .context("cloning log file handle for stderr")?;

    let exe = crate::obscura::supervisor::supervisor_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("obscura-supervisor")
        .arg("--obscura")
        .arg(&installed.executable)
        .arg("--port")
        .arg(port.to_string())
        .arg("--profile-dir")
        .arg(&opts.profile_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_clone))
        .kill_on_drop(false);
    configure_session_detachment(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning obscura supervisor {}", exe.display()))?;
    let pid = child.id().context("supervisor has no pid")?;
    let endpoint = wait_for_endpoint(port, &mut child, &log_path).await?;

    Ok(LaunchedHandle {
        pid,
        port,
        endpoint,
        engine: Engine::Cdp,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}
