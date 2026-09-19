//! `browser-control curl` — invoke the system curl with browser credentials.
//!
//! Unlike [`crate::cli::fetch`], this request does not execute in a renderer.
//! The selected browser supplies a snapshot of its cookie jar and User-Agent;
//! the real curl process supplies transport, streaming, redirects, and every
//! curl CLI option. Browser cookies are written to a mode-0600 temporary
//! Netscape jar which is removed when the command finishes.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use regex::RegexBuilder;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::cli::trace::CommandTrace;
use crate::session::backend::{open_backend, TabBackend};

/// Maximum raw curl stdout returned inside an MCP tool result. File output
/// selected by curl's `-o`/`--output` does not pass through this buffer and is
/// therefore unrestricted.
pub const MCP_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// Maximum model-visible body size for an automatic HTML projection. The full
/// response remains available through explicit curl file output (`-o`), while
/// ordinary MCP calls receive useful page text instead of raw markup/scripts.
pub const MCP_HTML_PREVIEW_LIMIT: usize = 32 * 1024;

/// Keep diagnostic stderr useful without allowing verbose curl traces to
/// create another unbounded MCP response.
const MCP_STDERR_LIMIT: usize = 256 * 1024;

pub(crate) struct PreparedCurl {
    cookie_jar: tempfile::NamedTempFile,
    user_agent: String,
    origin: Option<String>,
    referer: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CurlOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stderr_truncated: bool,
    pub(crate) request_url: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) http_status: Option<u16>,
    pub(crate) summarize_html: bool,
}

impl PreparedCurl {
    fn command<I, S>(&self, args: I) -> Result<tokio::process::Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.command_with_header(args, None)
    }

    fn command_with_header<I, S>(
        &self,
        args: I,
        header_path: Option<&Path>,
    ) -> Result<tokio::process::Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let executable = which::which("curl").context("curl executable not found in PATH")?;
        let mut command = tokio::process::Command::new(executable);
        // Browser-derived values are defaults. User argv follows unchanged,
        // so curl's normal last-option-wins behavior can override User-Agent
        // and augment other cookie sources when explicitly requested.
        command
            .arg("--cookie")
            .arg(self.cookie_jar.path())
            .arg("--user-agent")
            .arg(&self.user_agent);
        if let Some(origin) = &self.origin {
            command.arg("--header").arg(format!("Origin: {origin}"));
        }
        if let Some(referer) = &self.referer {
            command.arg("--referer").arg(referer);
        }
        // Put automatic header capture before user argv so an explicit -D/-i
        // remains last-option-wins and preserves the caller's raw workflow.
        if let Some(header_path) = header_path {
            command.arg("--dump-header").arg(header_path);
        }
        command.args(args);
        Ok(command)
    }
}

/// Snapshot browser credentials into the short-lived inputs consumed by
/// curl. `target_id` is optional because cookies are browser-wide; when set it
/// is used to read a target-specific `navigator.userAgent` override.
pub(crate) async fn prepare(backend: &TabBackend, target_id: Option<&str>) -> Result<PreparedCurl> {
    let cookies = backend.cookies().await?;
    let live_targets = backend.live_targets().await?;
    let context_target_id = target_id
        .filter(|target_id| live_targets.iter().any(|target| target.id == *target_id))
        .map(String::from)
        .or_else(|| live_targets.first().map(|target| target.id.clone()));
    let source_url = context_target_id.as_deref().and_then(|target_id| {
        live_targets
            .iter()
            .find(|target| target.id == target_id)
            .map(|target| target.url.clone())
    });
    let user_agent = backend.user_agent(context_target_id.as_deref()).await?;
    let (origin, referer) = source_url
        .as_deref()
        .map(request_context_headers)
        .unwrap_or((None, None));
    let mut cookie_jar =
        tempfile::NamedTempFile::new().context("creating temporary browser cookie jar for curl")?;
    cookie_jar
        .write_all(crate::cli::cookies::format_netscape(&cookies).as_bytes())
        .context("writing temporary browser cookie jar for curl")?;
    cookie_jar
        .flush()
        .context("flushing temporary browser cookie jar for curl")?;
    Ok(PreparedCurl {
        cookie_jar,
        user_agent,
        origin,
        referer,
    })
}

fn request_context_headers(source_url: &str) -> (Option<String>, Option<String>) {
    let Ok(mut parsed) = url::Url::parse(source_url) else {
        return (None, None);
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return (None, None);
    }
    let origin = parsed.origin().ascii_serialization();
    parsed.set_fragment(None);
    (Some(origin), Some(parsed.to_string()))
}

/// CLI entry point. Curl owns stdin/stdout/stderr, so ordinary streaming and
/// `-o` downloads behave exactly like invoking curl directly.
pub async fn run(browser: Option<String>, args: Vec<OsString>) -> Result<()> {
    if args.is_empty() {
        bail!("curl requires arguments; pass curl options and at least one URL");
    }
    let mut trace = CommandTrace::new("curl");
    let result = run_inner(browser, args, &mut trace).await;
    trace.finish(result)
}

async fn run_inner(
    browser: Option<String>,
    args: Vec<OsString>,
    trace: &mut CommandTrace,
) -> Result<()> {
    let route = crate::cli::route::preamble(browser, None, trace).await?;
    let backend = open_backend(&route.resolved.endpoint, route.resolved.engine).await?;
    let target_id = match route.tab_name.as_deref() {
        Some(tab_name) => {
            trace.route("named-tab").tab_name(tab_name);
            let browser_name = match &route.resolved.source {
                crate::cli::env_resolver::Source::Registered { name } => name.clone(),
                crate::cli::env_resolver::Source::External => {
                    bail!("named tabs (`<browser>/<name>`) require a registered browser")
                }
            };
            let row =
                crate::session::resolve_tab(&backend, &route.registry, &browser_name, tab_name)
                    .await?
                    .ok_or_else(|| crate::errors::SessionError::TabNotFound {
                        browser: browser_name,
                        name: tab_name.to_string(),
                    })?;
            trace.target_id(&row.target_id);
            Some(row.target_id)
        }
        None => {
            trace.route("browser-wide");
            None
        }
    };

    let prepared = prepare(&backend, target_id.as_deref()).await?;
    // Curl no longer needs the browser protocol connection or registry lock.
    // Release both before a potentially long download.
    drop(backend);
    drop(route);

    let mut command = prepared.command(args.iter())?;
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command.status().await.context("running curl")?;
    if !status.success() {
        bail!(
            "curl exited with status {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated by signal".to_string())
        );
    }
    Ok(())
}

/// Execute curl for MCP. Stdout is read incrementally and the child is killed
/// as soon as the response would exceed `MCP_RESPONSE_LIMIT`; stderr is always
/// drained concurrently and retained only up to `MCP_STDERR_LIMIT`.
pub(crate) async fn execute_mcp(prepared: &PreparedCurl, args: &[String]) -> Result<CurlOutput> {
    if args.is_empty() {
        bail!("`args` must contain curl options and at least one URL");
    }
    // Header capture is only an internal aid for the normal MCP path. If the
    // caller explicitly asked curl to emit headers, write output, or append a
    // custom trailer, leave the byte stream exactly as requested.
    let summarize_html = !has_explicit_raw_output(args);
    let header_file = if summarize_html {
        Some(tempfile::NamedTempFile::new().context("creating curl header capture")?)
    } else {
        None
    };
    let mut command =
        prepared.command_with_header(args, header_file.as_ref().map(|file| file.path()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("running curl")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture curl stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture curl stderr"))?;
    let stderr_task = tokio::spawn(read_bounded_and_drain(stderr, MCP_STDERR_LIMIT));

    let mut body = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let n = stdout
            .read(&mut chunk)
            .await
            .context("reading curl stdout")?;
        if n == 0 {
            break;
        }
        if body.len().saturating_add(n) > MCP_RESPONSE_LIMIT {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_task.await;
            bail!(
                "curl response exceeded the 8 MiB MCP limit; retry with `-o <path>` or `--output <path>` to stream it directly to a file"
            );
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let status = child.wait().await.context("waiting for curl")?;
    let (stderr, stderr_truncated) = stderr_task.await.context("joining curl stderr reader")??;
    let (http_status, content_type) = header_file
        .as_ref()
        .and_then(|file| std::fs::read(file.path()).ok())
        .map(|headers| parse_response_headers(&headers))
        .unwrap_or((None, None));
    Ok(CurlOutput {
        exit_code: status.code(),
        stdout: body,
        stderr,
        stderr_truncated,
        request_url: request_url(args),
        content_type,
        http_status,
        summarize_html,
    })
}

fn has_explicit_raw_output(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-i" | "-I"
                | "-D"
                | "--include"
                | "--dump-header"
                | "--head"
                | "-O"
                | "-w"
                | "--write-out"
                | "--output"
                | "--remote-name"
                | "--remote-name-all"
                | "--output-dir"
        ) || arg.starts_with("-o")
            || arg.starts_with("-D")
            || arg.starts_with("-w")
            || arg.starts_with("--output=")
            || arg.starts_with("--dump-header=")
            || arg.starts_with("--write-out=")
    })
}

fn request_url(args: &[String]) -> Option<String> {
    let mut result = None;
    let mut takes_value = false;
    let mut takes_url_value = false;
    for arg in args {
        if takes_value {
            if takes_url_value && (arg.starts_with("http://") || arg.starts_with("https://")) {
                result = Some(arg.clone());
            }
            takes_value = false;
            takes_url_value = false;
            continue;
        }
        if matches!(
            arg.as_str(),
            "-H" | "--header"
                | "-A"
                | "--user-agent"
                | "-e"
                | "--referer"
                | "-o"
                | "--output"
                | "-D"
                | "--dump-header"
                | "-w"
                | "--write-out"
                | "--url"
        ) {
            takes_value = true;
            takes_url_value = arg == "--url";
            continue;
        }
        if let Some(url) = arg.strip_prefix("--url=") {
            result = Some(url.to_string());
        } else if arg.starts_with("http://") || arg.starts_with("https://") {
            result = Some(arg.clone());
        }
    }
    result
}

fn parse_response_headers(headers: &[u8]) -> (Option<u16>, Option<String>) {
    let text = String::from_utf8_lossy(headers);
    let mut status = None;
    let mut content_type = None;
    for line in text.lines() {
        if line.to_ascii_lowercase().starts_with("http/") {
            status = line
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u16>().ok());
            content_type = None;
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_string());
            }
        }
    }
    (status, content_type)
}

async fn read_bounded_and_drain<R>(mut reader: R, limit: usize) -> Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let take = remaining.min(n);
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < n;
    }
    Ok((kept, truncated))
}

/// Convert a completed curl invocation to an MCP tool result. Ordinary HTML
/// responses are projected to bounded readable text; explicit raw-output
/// workflows and arbitrary bytes retain their original representation.
pub(crate) fn mcp_result(output: CurlOutput) -> Value {
    let success = output.exit_code == Some(0);
    let mut content = Vec::new();
    let mut html_preview_bytes = None;
    let mut html_preview_truncated = false;
    if !output.stdout.is_empty() {
        match std::str::from_utf8(&output.stdout) {
            Ok(text)
                if success
                    && output.summarize_html
                    && is_html_content_type(output.content_type.as_deref()) =>
            {
                let (preview, truncated) = html_preview(
                    text,
                    output.request_url.as_deref(),
                    output.http_status,
                    output.content_type.as_deref(),
                    output.stdout.len(),
                );
                html_preview_bytes = Some(preview.len());
                html_preview_truncated = truncated;
                content.push(json!({ "type": "text", "text": preview }));
            }
            Ok(text) => content.push(json!({ "type": "text", "text": text })),
            Err(_) => content.push(json!({
                "type": "resource",
                "resource": {
                    "uri": "browser-control://curl/response",
                    "mimeType": "application/octet-stream",
                    "blob": base64::engine::general_purpose::STANDARD.encode(&output.stdout),
                }
            })),
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    content.push(json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({
            "exit_code": output.exit_code,
            "stdout_bytes": output.stdout.len(),
            "stderr": stderr,
            "stderr_truncated": output.stderr_truncated,
            "url": output.request_url,
            "http_status": output.http_status,
            "content_type": output.content_type,
            "html_preview_bytes": html_preview_bytes,
            "html_preview_truncated": html_preview_truncated,
        })).expect("serializing curl metadata cannot fail")
    }));
    json!({
        "content": content,
        "isError": !success,
    })
}

fn is_html_content_type(content_type: Option<&str>) -> bool {
    content_type
        .map(|value| {
            let mime = value.split(';').next().unwrap_or(value).trim();
            mime.eq_ignore_ascii_case("text/html")
                || mime.eq_ignore_ascii_case("application/xhtml+xml")
        })
        .unwrap_or(false)
}

fn html_preview(
    html: &str,
    url: Option<&str>,
    http_status: Option<u16>,
    content_type: Option<&str>,
    original_bytes: usize,
) -> (String, bool) {
    let title = extract_html_title(html);
    let mut body = html.to_string();
    for tag in ["script", "style", "noscript", "template", "svg", "head"] {
        let pattern = format!(r"<{tag}\b[^>]*>.*?</{tag}\s*>");
        if let Ok(regex) = RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .dot_matches_new_line(true)
            .build()
        {
            body = regex.replace_all(&body, "").into_owned();
        }
    }

    if let Ok(anchors) =
        RegexBuilder::new(r#"<a\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a\s*>"#)
            .case_insensitive(true)
            .dot_matches_new_line(true)
            .build()
    {
        body = anchors
            .replace_all(&body, |captures: &regex::Captures<'_>| {
                let label = strip_html_fragment(captures.get(2).map_or("", |m| m.as_str()));
                let href = captures.get(1).map_or("", |m| m.as_str()).trim();
                if label.trim().is_empty() {
                    format!("[{href}]")
                } else if href.is_empty() {
                    label
                } else {
                    format!("{} [{href}]", label.trim())
                }
            })
            .into_owned();
    }

    if let Ok(list_items) = RegexBuilder::new(r"<li\b[^>]*>")
        .case_insensitive(true)
        .build()
    {
        body = list_items.replace_all(&body, "\n- ").into_owned();
    }
    if let Ok(blocks) = RegexBuilder::new(
        r"</?(?:address|article|aside|blockquote|br|dd|div|dl|dt|fieldset|figcaption|figure|footer|form|h[1-6]|header|hr|main|nav|ol|p|section|table|tbody|td|tfoot|th|thead|tr|ul)\b[^>]*>",
    )
    .case_insensitive(true)
    .build()
    {
        body = blocks.replace_all(&body, "\n").into_owned();
    }
    if let Ok(tags) = RegexBuilder::new(r"<[^>]*>").case_insensitive(true).build() {
        body = tags.replace_all(&body, "").into_owned();
    }

    let body = normalize_html_text(&body);
    let mut result = String::from("HTML page summary\n");
    if let Some(url) = url.filter(|value| !value.is_empty()) {
        result.push_str(&format!("URL: {url}\n"));
    }
    if let Some(status) = http_status {
        result.push_str(&format!("HTTP status: {status}\n"));
    }
    if let Some(content_type) = content_type.filter(|value| !value.is_empty()) {
        result.push_str(&format!("Content-Type: {content_type}\n"));
    }
    if !title.is_empty() {
        result.push_str(&format!("Title: {title}\n"));
    }
    result.push_str(&format!("Original response: {original_bytes} bytes\n\n"));
    result.push_str(if body.is_empty() {
        "(The HTML response contained no readable text.)"
    } else {
        &body
    });

    let notice = format!(
        "\n\n[HTML preview truncated at {} bytes; use browser_curl with -o <path> to retrieve the full response.]",
        MCP_HTML_PREVIEW_LIMIT
    );
    if result.len() <= MCP_HTML_PREVIEW_LIMIT {
        return (result, false);
    }
    let budget = MCP_HTML_PREVIEW_LIMIT.saturating_sub(notice.len());
    let end = floor_char_boundary(&result, budget);
    let mut bounded = result[..end].trim_end().to_string();
    bounded.push_str(&notice);
    (bounded, true)
}

fn extract_html_title(html: &str) -> String {
    let Ok(title) = RegexBuilder::new(r"<title\b[^>]*>(.*?)</title\s*>")
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .build()
    else {
        return String::new();
    };
    title
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| normalize_html_text(&strip_html_fragment(value.as_str())))
        .unwrap_or_default()
}

fn strip_html_fragment(fragment: &str) -> String {
    let Ok(tags) = RegexBuilder::new(r"<[^>]*>").case_insensitive(true).build() else {
        return fragment.to_string();
    };
    tags.replace_all(fragment, "").into_owned()
}

fn normalize_html_text(text: &str) -> String {
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn floor_char_boundary(text: &str, requested: usize) -> usize {
    let mut end = requested.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_result_returns_utf8_as_text() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: b"hello".to_vec(),
            stderr: Vec::new(),
            stderr_truncated: false,
            request_url: None,
            content_type: None,
            http_status: None,
            summarize_html: false,
        });
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "hello");
    }

    #[test]
    fn mcp_result_returns_binary_as_embedded_resource() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: vec![0, 159, 146, 150],
            stderr: Vec::new(),
            stderr_truncated: false,
            request_url: None,
            content_type: None,
            http_status: None,
            summarize_html: false,
        });
        assert_eq!(result["content"][0]["type"], "resource");
        assert_eq!(result["content"][0]["resource"]["blob"], "AJ+Slg==");
    }

    #[test]
    fn mcp_result_marks_nonzero_curl_exit_as_tool_error() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(22),
            stdout: b"not found".to_vec(),
            stderr: b"curl: (22) HTTP response code said error".to_vec(),
            stderr_truncated: false,
            request_url: None,
            content_type: None,
            http_status: None,
            summarize_html: false,
        });
        assert_eq!(result["isError"], true);
        assert!(result["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"exit_code\": 22"));
    }

    #[test]
    fn request_context_headers_use_tab_origin_and_fragmentless_url() {
        let (origin, referer) =
            request_context_headers("https://app.example.com:8443/work?q=1#section");
        assert_eq!(origin.as_deref(), Some("https://app.example.com:8443"));
        assert_eq!(
            referer.as_deref(),
            Some("https://app.example.com:8443/work?q=1")
        );
    }

    #[test]
    fn request_context_headers_ignore_non_http_tabs() {
        assert_eq!(request_context_headers("about:blank"), (None, None));
    }

    #[test]
    fn html_mcp_results_are_bounded_and_keep_page_metadata() {
        let html = br#"<!doctype html>
            <html><head><title>Example account</title><style>.secret{display:none}</style>
            <script>window.secret = 'do not show';</script></head>
            <body><h1>Welcome</h1><p>Visible content.</p>
            <a href="https://example.com/help">Help center</a>
            <ul><li>First item</li><li>Second item</li></ul></body></html>"#;
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: html.to_vec(),
            stderr: Vec::new(),
            stderr_truncated: false,
            request_url: Some("https://example.com/account".into()),
            content_type: Some("text/html; charset=utf-8".into()),
            http_status: Some(200),
            summarize_html: true,
        });
        let preview = result["content"][0]["text"].as_str().unwrap();
        assert!(preview.contains("HTML page summary"));
        assert!(preview.contains("Example account"));
        assert!(preview.contains("Welcome"));
        assert!(preview.contains("Help center [https://example.com/help]"));
        assert!(preview.contains("Original response:"));
        assert!(!preview.contains("window.secret"));
        assert!(!preview.contains("<script>"));
        assert!(result["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"http_status\": 200"));
    }

    #[test]
    fn explicit_raw_html_workflow_is_not_projected() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: b"<html><body>raw</body></html>".to_vec(),
            stderr: Vec::new(),
            stderr_truncated: false,
            request_url: Some("https://example.com".into()),
            content_type: Some("text/html".into()),
            http_status: Some(200),
            summarize_html: false,
        });
        assert_eq!(
            result["content"][0]["text"],
            "<html><body>raw</body></html>"
        );
    }

    #[test]
    fn html_preview_is_bounded_and_marks_omitted_content() {
        let html = format!(
            "<html><head><title>Long page</title></head><body>{}</body></html>",
            "visible content ".repeat(MCP_HTML_PREVIEW_LIMIT),
        );
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: html.into_bytes(),
            stderr: Vec::new(),
            stderr_truncated: false,
            request_url: Some("https://example.com/long".into()),
            content_type: Some("text/html".into()),
            http_status: Some(200),
            summarize_html: true,
        });
        let preview = result["content"][0]["text"].as_str().unwrap();
        assert!(preview.len() <= MCP_HTML_PREVIEW_LIMIT);
        assert!(preview.contains("HTML preview truncated"));
        assert!(result["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"html_preview_truncated\": true"));
    }

    #[test]
    fn response_headers_use_the_last_redirect_response() {
        let headers = b"HTTP/1.1 302 Found\r\nContent-Type: text/html\r\n\r\nHTTP/2 200\r\nContent-Type: application/json\r\n\r\n";
        assert_eq!(
            parse_response_headers(headers),
            (Some(200), Some("application/json".into()))
        );
    }

    #[test]
    fn raw_output_options_disable_automatic_projection() {
        assert!(has_explicit_raw_output(&[
            "-i".into(),
            "https://example.com".into()
        ]));
        assert!(has_explicit_raw_output(&[
            "-o".into(),
            "/tmp/page.html".into()
        ]));
        assert!(has_explicit_raw_output(&[
            "--write-out=%{http_code}".into(),
            "https://example.com".into()
        ]));
        assert!(!has_explicit_raw_output(&[
            "-L".into(),
            "https://example.com".into()
        ]));
    }
}
