//! Obscura backend support.
//!
//! Obscura (<https://github.com/h4ckf0r0day/obscura>) is a headless Rust/V8
//! browser engine with a CDP server. Two of its behaviours do not fit the
//! shared-browser model the rest of browser-control assumes:
//!
//! * **Targets are scoped to the CDP connection that created them.** A page
//!   created on one WebSocket is invisible to every other WebSocket, and it
//!   goes away when that WebSocket closes. browser-control talks to a browser
//!   over several connections (the native backend, the Playwright sidecar,
//!   and one connection per short-lived CLI command), so with a bare
//!   `obscura serve` the sidecar never finds the page the native backend
//!   created, and named tabs do not survive between CLI calls.
//! * **JavaScript dialogs are silently accepted.** `confirm()` returns `true`
//!   and no `Page.javascriptDialogOpening` event is emitted, so an agent
//!   would approve destructive confirmations without seeing them.
//!
//! [`supervisor`] therefore runs `obscura serve` on a private loopback port
//! and puts a [`mux::Mux`] in front of it: one upstream CDP connection that
//! every downstream client shares, with Chrome-like per-client auto-attach,
//! discovery and session ownership emulated on top. The registry records
//! the supervisor's port and PID, so every other part of browser-control
//! sees an ordinary CDP browser.

pub mod mux;
pub mod server;
pub mod supervisor;

/// Injected into every page (current document and future navigations).
///
/// Obscura auto-accepts `confirm()`; Chrome driven through Playwright
/// auto-dismisses dialogs instead. This shim restores the safe behaviour:
/// `confirm()` returns `false`, `prompt()` returns `null`, `alert()` is a
/// no-op, and each dialog is recorded on `window.__browserControlDialogs`
/// and logged with `console.warn` so an agent can see what was dismissed.
pub const DIALOG_SHIM_JS: &str = r#"(() => {
  try {
    if (window.__browserControlDialogShim) return;
    Object.defineProperty(window, "__browserControlDialogShim", { value: true });
    const record = (type, message) => {
      try {
        const text = message === undefined ? "" : String(message);
        (window.__browserControlDialogs = window.__browserControlDialogs || []).push({
          type, message: text, at: Date.now(),
        });
        console.warn(`[browser-control] ${type}() dismissed (headless obscura): ${text}`);
      } catch (_) {}
    };
    window.alert = function (message) { record("alert", message); };
    window.confirm = function (message) { record("confirm", message); return false; };
    window.prompt = function (message) { record("prompt", message); return null; };
  } catch (_) {}
})();"#;
