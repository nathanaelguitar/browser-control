---
name: browser-control
description: Use browser-control (MCP tools browser_navigate, browser_snapshot, browser_click, browser_eval, browser_fetch, etc., or the `browser-control` CLI) as the default way to drive a real browser — navigating pages, clicking elements, reading/writing cookies and storage, fetching authenticated APIs, or taking screenshots. Use this whenever the user asks to open a URL, click something on a page, log in, scrape/read page content, or automate any browser interaction. Never hand-write JavaScript (`element.click()`, `location.href = ...`) to drive the browser when browser-control's primitives cover it.
priority: 10
---

# browser-control

`browser-control` (https://github.com/nathanaelguitar/browser-control) is the
default browser automation tool for this machine. It is registered as an MCP
server named `browser-control` and is also available as a CLI. Prefer it over
any other browser mechanism, raw CDP/BiDi scripting, or hand-written
JavaScript clicks.

## Core rule

Use the highest-level browser-control primitive available:

- If MCP tools are available, prefer them over shell commands.
- If MCP tools are not available, use the `browser-control` CLI.
- Do not drive raw CDP/BiDi, scrape browser profile databases, or launch a
  separate browser unless browser-control lacks the needed primitive.
- Use `--json` for CLI output that another tool or agent will parse.

## Browser selection

- Select a browser with `browser_select` in MCP, or with CLI `-b/--browser`,
  `$BROWSER_CONTROL`, or `browser-control set default <selector>`.
- Selectors may be a kind (`chrome`, `edge`, `chromium`, `brave`, `firefox`),
  a friendly name from `list-running`, an absolute executable path, or a
  CDP/BiDi endpoint URL.
- If nothing is running, use `browser-control start <kind>`. It reuses a
  persistent per-kind profile so login state survives.
- Browser windows and automated tabs stay in the background by default.
  Reveal the browser only when human interaction is needed: use MCP
  `browser_show` or CLI `browser-control show -b <browser>`.

## Tabs

- Prefer tab primitives over target IDs:
  - MCP: `browser_tab_list`, `browser_tab_new`, `browser_tab_select`,
    `browser_tab_close`.
  - CLI: `browser-control tab open <browser>/<name> [url]`,
    `tab list <browser> --all`, `tab adopt <browser>/<name> <target-id>`.
- For repeatable work, create or select a named tab, then address it as
  `<browser>/<tab>` in page-context CLI commands.
- Use target IDs only to adopt an existing unnamed tab or as a last-resort
  diagnostic.
- Browser-wide operations do not take tab names. Page-context operations do.

## Page and network work

- Navigate and inspect with MCP primitives first: `browser_navigate`,
  `browser_snapshot`, `browser_get_html`, `browser_take_screenshot`,
  `browser_select_element`.
- CLI navigation: `browser-control tab open <browser>/<name> <url>` opens or
  navigates a named tab (re-running with a new url navigates the existing
  tab). This is how you go to a URL from the CLI — never navigate by
  evaling `location.href`, which bypasses the tab registry and races the
  page load.
- Fetch authenticated APIs with `browser_fetch` or `browser-control fetch`;
  this runs inside the browser context so cookies, Origin, CORS, and the
  browser TLS stack apply.
- For large responses, binary downloads, or requests that should not run
  under page CORS/CSP, use `browser_curl` or `browser-control curl`. It
  invokes the real curl with a temporary browser cookie jar plus
  User-Agent, Origin, and Referer derived from the source tab. MCP
  responses are capped at 8 MiB; pass curl `-o <path>` for unrestricted
  streaming to disk.
- Read/write storage with `browser_storage_get` / `browser_storage_set` or
  `browser-control storage`.
- Evaluate JavaScript with `browser_eval` or `browser-control eval` only
  when no higher-level primitive fits.
- Auth-sensitive reads reload HTTP(S) pages older than 10 minutes before
  evaluating so SSO can refresh tokens. CLI callers can override with
  `--max-age 1h`; MCP callers can pass `max_age`.

## Clicking and interaction

- Use MCP `browser_click` or CLI `browser-control click`; never hand-write
  JavaScript to click an element.
- Preferred MCP flow: call `browser_snapshot`, then pass the exact
  accessible name and role, e.g.
  `browser_click({"element":"Sign in","role":"button"})`.
- Preferred CLI form: `browser-control click -b brave/work "Sign in" --role
  button`. A unique visible name works without `--role`; use `--selector`
  only as a fallback.
- Semantic clicks use exact matching by default and refuse ambiguous
  elements. Capture a fresh snapshot and add `role`, or narrow the
  selector, instead of guessing. CLI clicks likewise refuse to choose among
  multiple open pages; address a named tab or pass `--target <url-regex>`.
- Playwright performs the real input action, including actionability
  checks, scrolling, trusted pointer events, and navigation waiting. Do not
  replace it with `element.click()` through eval.

## Cookies and login

- Wait for login with `browser_wait_for_cookie` or
  `browser-control wait-for-cookie --domain <regex> --name <regex>`.
- Add `--validate-url <url>` when the cookie alone is not enough and an
  authenticated endpoint must return 2xx.
- Export cookies with `browser_cookies` or `browser-control cookies`; use
  `--format netscape -o cookies.txt` for curl, wget, or yt-dlp.

## Recovery

- If a tab is gone or hung, list tabs, select another tab, or create a
  fresh named tab and retry.
- URL regex selectors are unanchored unless you add `^` or `$`.

## How this is installed

This Skill ships as part of the `browser-control` Canopy Code extension
(`canopy-extension.json` at the repo root). The extension's MCP server entry
points at `scripts/mcp-wrapper.sh`, which self-bootstraps on first run: it
installs Rust via rustup if `cargo` isn't already on `PATH`, builds the
release binary from source, then execs it. Every run after that is a plain
exec — no rebuild, no network. This means the extension works the same way
on any machine (Linux, macOS) without a separate install step: `canopy
extensions link <path-to-browser-control-checkout>` (or `install` once it's
pushed to a URL) is enough.

The Playwright sidecar prefers Node.js, Playwright's reference runtime, and
uses Bun only when Node is unavailable. Its CDP attachment budget is 15
seconds so a newly launched browser has time to expose its debugging
endpoint.

Firefox only supports the engine-agnostic primitives (navigate, eval,
get_html, fetch, cookies, storage, tabs) — no Playwright-sidecar tools.
