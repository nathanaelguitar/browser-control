//! Loopback CDP front door for the Obscura supervisor.
//!
//! Serves the subset of Chrome's DevTools HTTP endpoints browser-control and
//! Playwright use (`/json/version`, `/json`, `/json/list`) and upgrades
//! `/devtools/...` WebSocket requests into [`Mux`] clients.
//!
//! Like Chrome (and like Obscura's own server), it refuses requests that
//! carry an `Origin` header or a non-loopback `Host`, so a web page in the
//! user's real browser cannot drive it via DNS rebinding or cross-site
//! WebSocket requests.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::mux::{ws_config, Mux};

const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Static facts reported by `/json/version`.
#[derive(Clone)]
pub struct VersionInfo {
    /// Upstream `/json/version` body (Obscura reports a Chrome-like product).
    pub upstream: Value,
    pub obscura_version: String,
}

/// Accept connections until the listener fails.
pub async fn serve(listener: TcpListener, mux: Arc<Mux>, version: VersionInfo) -> Result<()> {
    let port = listener.local_addr()?.port();
    loop {
        let (stream, _) = listener.accept().await?;
        let mux = Arc::clone(&mux);
        let version = version.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, port, mux, version).await {
                tracing::debug!(target = "obscura", %e, "connection ended with error");
            }
        });
    }
}

struct Head {
    len: usize,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Peek (without consuming) the HTTP request head so a WebSocket upgrade
/// can still be handed to tungstenite intact.
async fn peek_head(stream: &TcpStream) -> Result<Option<Head>> {
    let mut buf = vec![0u8; MAX_HEAD_BYTES];
    let deadline = tokio::time::Instant::now() + HEAD_TIMEOUT;
    loop {
        let n = tokio::time::timeout_at(deadline, stream.peek(&mut buf)).await??;
        if n == 0 {
            return Ok(None);
        }
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(parse_head(&buf[..end + 4]));
        }
        if n >= MAX_HEAD_BYTES {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn parse_head(bytes: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Some(Head {
        len: bytes.len(),
        method,
        path,
        headers,
    })
}

/// `Host` must name this loopback listener.
pub(crate) fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        // HTTP/1.0 clients may omit Host; they cannot be a browser page.
        return true;
    };
    let host = host.to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|h| host == *h || host == format!("{h}:{port}"))
}

async fn handle(
    mut stream: TcpStream,
    port: u16,
    mux: Arc<Mux>,
    version: VersionInfo,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let Some(head) = peek_head(&stream).await? else {
        return Ok(());
    };
    if head.header("origin").is_some() || !host_allowed(head.header("host"), port) {
        let mut sink = vec![0u8; head.len];
        stream.read_exact(&mut sink).await?;
        return respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"browser-origin and non-loopback requests are refused\n",
        )
        .await;
    }
    let is_upgrade = head
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_upgrade && head.path.starts_with("/devtools/") {
        let ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config())).await?;
        mux.serve_client(ws).await;
        return Ok(());
    }

    let mut sink = vec![0u8; head.len];
    stream.read_exact(&mut sink).await?;
    if head.method != "GET" && head.method != "PUT" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", b"").await;
    }
    let path = head.path.split('?').next().unwrap_or("");
    match path {
        "/json/version" | "/json/version/" => {
            let body = version_body(&version, port);
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                body.to_string().as_bytes(),
            )
            .await
        }
        "/json" | "/json/" | "/json/list" | "/json/list/" => {
            let targets = mux
                .call("Target.getTargets", json!({}), None)
                .await
                .unwrap_or(Value::Null);
            let body = list_body(&targets);
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                body.to_string().as_bytes(),
            )
            .await
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found\n").await,
    }
}

pub(crate) fn version_body(version: &VersionInfo, port: u16) -> Value {
    let mut body = match &version.upstream {
        Value::Object(_) => version.upstream.clone(),
        _ => json!({}),
    };
    body["webSocketDebuggerUrl"] = json!(format!("ws://127.0.0.1:{port}/devtools/browser"));
    body["browserControl"] = json!({
        "engine": "obscura",
        "obscuraVersion": version.obscura_version,
        "multiplexed": true,
    });
    body
}

fn list_body(targets: &Value) -> Value {
    let list: Vec<Value> = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    json!({
                        "id": t.get("targetId").cloned().unwrap_or(Value::Null),
                        "type": t.get("type").cloned().unwrap_or(json!("page")),
                        "title": t.get("title").cloned().unwrap_or(json!("")),
                        "url": t.get("url").cloned().unwrap_or(json!("")),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Value::Array(list)
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_check_allows_only_loopback_for_this_port() {
        assert!(host_allowed(Some("127.0.0.1:9222"), 9222));
        assert!(host_allowed(Some("localhost:9222"), 9222));
        assert!(host_allowed(Some("LOCALHOST"), 9222));
        assert!(host_allowed(Some("[::1]:9222"), 9222));
        assert!(host_allowed(None, 9222));
        assert!(!host_allowed(Some("127.0.0.1:9223"), 9222));
        assert!(!host_allowed(Some("evil.example:9222"), 9222));
        assert!(!host_allowed(Some("127.0.0.1.nip.io:9222"), 9222));
    }

    #[test]
    fn version_body_points_at_the_mux() {
        let v = VersionInfo {
            upstream: json!({ "Browser": "Chrome/145.0.0.0", "webSocketDebuggerUrl": "ws://127.0.0.1:1/devtools/browser" }),
            obscura_version: "0.2.3".into(),
        };
        let body = version_body(&v, 4242);
        assert_eq!(
            body["webSocketDebuggerUrl"],
            "ws://127.0.0.1:4242/devtools/browser"
        );
        assert_eq!(body["Browser"], "Chrome/145.0.0.0");
        assert_eq!(body["browserControl"]["engine"], "obscura");
    }

    #[test]
    fn parse_head_reads_request_line_and_headers() {
        let raw = b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:9\r\nUpgrade: websocket\r\n\r\n";
        let h = parse_head(raw).unwrap();
        assert_eq!(h.method, "GET");
        assert_eq!(h.path, "/json/version");
        assert_eq!(h.header("host"), Some("127.0.0.1:9"));
        assert_eq!(h.header("UPGRADE"), Some("websocket"));
        assert_eq!(h.len, raw.len());
    }
}
