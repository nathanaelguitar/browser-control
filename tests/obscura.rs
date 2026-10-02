//! End-to-end test against a real Obscura binary.
//!
//! Runs only when Obscura is available: `BROWSER_CONTROL_OBSCURA` pointing at
//! an existing binary, or `obscura` on `PATH`. Otherwise it prints a skip
//! notice and passes. Everything uses throwaway data/config dirs; the only
//! processes it stops are the supervisor it started (SIGTERM by PID).

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use tempfile::TempDir;

fn obscura_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("BROWSER_CONTROL_OBSCURA") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    which::which("obscura").ok()
}

/// Serve one fixed HTML page on a loopback port until the process exits.
fn serve_page() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = "<!doctype html><title>bc-obscura-e2e</title><p id=x>hello</p>";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

struct Env {
    data: TempDir,
    cfg: TempDir,
    obscura: PathBuf,
    supervisor_pid: Option<u32>,
}

impl Env {
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("browser-control").unwrap();
        c.env("BROWSER_CONTROL_DATA_DIR", self.data.path())
            .env("BROWSER_CONTROL_CONFIG_DIR", self.cfg.path())
            .env("BROWSER_CONTROL_OBSCURA", &self.obscura)
            .env_remove("BROWSER_CONTROL")
            .env_remove("BROWSER_CONTROL_MCP_DEFAULT")
            .timeout(Duration::from_secs(60));
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(pid) = self.supervisor_pid.take() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn http_get(port: u16, path: &str, extra_headers: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra_headers}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn obscura_start_share_targets_across_processes_and_stop() {
    let Some(obscura) = obscura_binary() else {
        eprintln!("skipping: obscura not found (set BROWSER_CONTROL_OBSCURA or put it on PATH)");
        return;
    };
    let mut env = Env {
        data: TempDir::new().unwrap(),
        cfg: TempDir::new().unwrap(),
        obscura,
        supervisor_pid: None,
    };

    // list-installed reports it.
    let out = env
        .cmd()
        .args(["list-installed", "--json"])
        .output()
        .unwrap();
    let installed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        installed
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "obscura"),
        "{installed}"
    );

    // Start: always headless, registered with the supervisor's PID/port.
    let out = env
        .cmd()
        .args(["start", "obscura", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let started: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    env.supervisor_pid = Some(started["pid"].as_u64().unwrap() as u32);
    assert_eq!(started["kind"], "obscura");
    assert_eq!(started["headless"], true);
    let endpoint = started["endpoint"].as_str().unwrap().to_string();
    let port: u16 = endpoint
        .trim_start_matches("ws://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    // Second start reuses it.
    let out = env
        .cmd()
        .args(["start", "obscura", "--json"])
        .output()
        .unwrap();
    let again: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(again["reused"], true);
    assert_eq!(again["pid"], started["pid"]);

    // /json/version is served by the mux and points back at it.
    let version = http_get(port, "/json/version", "");
    assert!(version.starts_with("HTTP/1.1 200"), "{version}");
    assert!(version.contains("\"obscura\""), "{version}");
    assert!(version.contains(&endpoint), "{version}");
    // Browser-origin requests are refused.
    let refused = http_get(port, "/json/version", "Origin: https://evil.example\r\n");
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");

    // A tab opened by one process is visible to the next ones (Obscura on
    // its own scopes targets to a single CDP connection).
    let page = serve_page();
    let url = format!("http://127.0.0.1:{page}/");
    env.cmd()
        .args(["tab", "open", "obscura/e2e", &url])
        .assert()
        .success();
    let out = env
        .cmd()
        .args(["eval", "-b", "obscura/e2e", "document.title"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("bc-obscura-e2e"),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Dialogs are dismissed, not silently accepted.
    let out = env
        .cmd()
        .args(["eval", "-b", "obscura/e2e", "confirm('really?')"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "false");

    // `show` has nothing to reveal on a headless engine.
    env.cmd()
        .args(["show", "-b", "obscura"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not supported"));

    // SIGTERM stops the supervisor (and with it Obscura).
    let pid = env.supervisor_pid.take().unwrap();
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !pid_alive(pid),
        "supervisor {pid} still running after SIGTERM"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port {port} still accepting after shutdown"
    );
}

#[test]
fn start_obscura_without_binary_explains_where_to_get_it() {
    let data = TempDir::new().unwrap();
    let cfg = TempDir::new().unwrap();
    let missing = data.path().join("no-such-obscura");
    Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", data.path())
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .env("BROWSER_CONTROL_OBSCURA", &missing)
        .args(["start", "obscura"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("obscura is not installed"))
        .stderr(predicates::str::contains(
            "https://github.com/h4ckf0r0day/obscura/releases",
        ));
}
