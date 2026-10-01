# Obscura

[Obscura](https://github.com/h4ckf0r0day/obscura) (Apache-2.0) is a headless
browser engine written in Rust on V8 with a Chrome DevTools Protocol server.
browser-control supports it as the `obscura` browser kind, and the Canopy Code
extension makes it the browser new MCP sessions start on.

Evaluated against Obscura **v0.2.3**.

## Install

browser-control does not download Obscura. Get the **non-stealth** build for
your platform from the [releases page](https://github.com/h4ckf0r0day/obscura/releases)
(`obscura-<arch>-<os>.tar.gz`; the `-stealth` archives add TLS impersonation
and tracker blocking, which browser-control does not want), extract `obscura`
and `obscura-worker`, and make it discoverable in one of these ways (first
match wins):

1. `BROWSER_CONTROL_OBSCURA=/path/to/obscura`
2. `obscura` on `PATH`
3. `<data dir>/bin/obscura` (macOS: `~/Library/Application Support/browser-control/bin/obscura`,
   Linux: `~/.local/share/browser-control/bin/obscura`)

The Canopy MCP server does not inherit your shell environment, so prefer 2 or
3 there. `browser-control list-installed` shows whether it was found; `start
obscura` without it prints where to get it.

The release assets carry no signatures or attestations. GitHub publishes a
server-side SHA-256 digest per asset (`gh api
repos/h4ckf0r0day/obscura/releases/tags/v0.2.3 --jq '.assets[] | .name + " " + .digest'`);
compare it with `shasum -a 256` before installing.

## Default and opting out

| Situation | Browser used by `browser-control mcp` |
| --- | --- |
| `-b`/`BROWSER_CONTROL` given | that browser |
| `browser-control set mcp-default <browser>` | that browser |
| `BROWSER_CONTROL_MCP_DEFAULT=obscura` (set by `canopy-extension.json`) and Obscura found | Obscura |
| Obscura missing or fails to start | logged to stderr, then the regular chain: `default` setting, most recent live browser, first installed Chromium-family browser |

* Opt out for MCP sessions: `browser-control set mcp-default chrome`
  (any selector works), or `browser-control set mcp-default inherit` to use
  your regular `default`. `browser-control unset mcp-default` restores the
  extension's choice.
* Switch for one task: the agent calls `browser_select` with `chrome` (or
  `browser_start` with `browser: "chrome"`). Cookies and tabs are not shared
  between browsers.
* CLI commands are unaffected; only `browser-control mcp` reads the MCP
  preference. `browser_start` without a `browser` argument starts the MCP
  preference when it is installed.

## How it is wired

```
 native backend ─┐
 Playwright      ├─ ws://127.0.0.1:<port> ─ obscura-supervisor (mux) ─ one CDP connection ─ obscura serve (127.0.0.1:<private port>)
 CLI commands  ──┘                          registry: pid + port
```

`browser-control start obscura` launches the hidden `obscura-supervisor`
subcommand, which runs:

```
obscura serve --host 127.0.0.1 --port <random> --storage-dir <profile>/storage \
  --allow-private-network --allow-file-access
```

with `OBSCURA_TIMEZONE` set to the host zone (Obscura defaults to
`Europe/Berlin`). The profile lives at `<config dir>/profiles/obscura/default`
like the other kinds; cookies persist in `storage/cookies.json`.

The supervisor exists because **Obscura scopes pages to the CDP connection
that created them**: a second connection does not see them, and they are
dropped when their connection closes. browser-control uses several
connections (native backend, Playwright sidecar, one per CLI command), so on a
bare `obscura serve` every Playwright tool fails with `page not found`. The
supervisor's multiplexer holds one upstream connection and emulates Chrome's
per-client behaviour on top of it: request ids are rewritten, sessions belong
to the client that attached them, `Target.setAutoAttach` and
`Target.setDiscoverTargets` are emulated per client (including on Playwright's
browser session), a `createTarget` reply waits for the auto-attach events,
`Target.getTargetInfo` on a page session is pinned to that page, and
`Browser.close` from a client is ignored. Every page also gets a dialog shim
(see Risks).

The supervisor serves `/json/version` and `/json/list`, refuses requests with
an `Origin` header or a non-loopback `Host`, and stops Obscura with SIGTERM
(after closing the upstream connection so Obscura flushes cookies) when it
gets SIGTERM/SIGINT/SIGHUP. Stop it with `kill <pid>` using the PID from
`browser-control list-running`. On Linux Obscura also dies with a SIGKILLed
supervisor (`PR_SET_PDEATHSIG`); on macOS that would orphan it.

Trade-off: one upstream connection means one V8 thread serves every page.
Obscura parallelises per connection, so concurrent heavy pages are slower than
they would be on separate connections. That is fine for an agent tool that
works one step at a time.

## Compatibility matrix

Measured with the browser-control MCP tools on a local test site (form, select,
file input, `confirm()`, hover/drag targets, same-origin iframe, module-script
SPA, login form that sets a cookie) plus example.com and a Wikipedia article.
Raw CDP gaps were probed separately. Headless Chromium 145 ran the same suite
as a control and passed everything except timing-related steps of the
harness.

| MCP tool / scenario | Obscura 0.2.3 via browser-control | Detail (CDP method or cause) |
| --- | --- | --- |
| `browser_navigate` | works | `Page.navigate` |
| `browser_eval` | works | `Runtime.evaluate` (incl. `awaitPromise`) |
| `browser_get_html` | works | |
| `browser_snapshot` | works | Playwright `ariaSnapshot`; `Accessibility.getFullAXTree` also answers |
| `browser_click` (selector, role + name) | works | |
| `browser_type` | works | |
| `browser_press_key` | works | `Input.dispatchKeyEvent` |
| `browser_select_option` | works (fixed here) | Playwright's label match failed: `HTMLOptionElement.label` is `null` in Obscura and no option is selected by default; the sidecar now resolves label to value in the page |
| `browser_wait_for` (selector, URL, load state) | works | |
| `browser_set_input_files` | works (fixed here) | Playwright needs `DataTransfer` (undefined in Obscura); falls back to `DOM.setFileInputFiles`, which requires `--allow-file-access` |
| `browser_hover` | **not supported** (refused) | `Input.dispatchMouseEvent` is accepted but no `mouseover`/`mouseenter`/`mousemove` DOM events fire |
| `browser_drag` | **not supported** (refused) | `Input.setInterceptDrags` unknown, no `DataTransfer`, no drag events |
| `browser_show` | **not supported** (refused) | `Page.bringToFront` unknown; headless, nothing to show |
| `browser_select_element` | **not supported** (refused) | waits for a human click on an overlay |
| JavaScript dialogs | partial (shimmed) | no `Page.javascriptDialogOpening`, `Page.handleJavaScriptDialog` unknown, `confirm()` returns `true` natively; the supervisor shim makes it `false` |
| iframes | partial | same-origin `browser_eval` via `iframe.contentDocument` works; Playwright frame locators fail (`Failed to find frame for selector`); iframe content is not painted in screenshots |
| `browser_take_screenshot` (viewport, element, full page) | partial | `Page.captureScreenshot` works; layout and text are good, but native button chrome, file inputs and iframes are not painted |
| `browser_pdf_save` | works (raster) | `Page.printToPDF`; text is not selectable |
| `browser_cookies`, `browser_wait_for_cookie` | works | `Network.getAllCookies`, `Storage.getCookies` |
| `browser_storage_get` / `_set` (local, session) | works | |
| `browser_fetch` | works | in-page `fetch` |
| `browser_curl` | works | sends the browser's cookies and its (Windows Chrome) User-Agent |
| tabs: `browser_tab_list/new/select/close`, named tabs, `list_targets` | works | through the supervisor; on a bare `obscura serve` tabs are per connection |
| SPA (module script, `fetch`, `pushState`) | works | |
| login-style form (type, submit, redirect, cookie) | works | |
| example.com, Wikipedia article | works | Wikipedia renders well; navigation to it took ~2 s on the Mac |

Totals on the MCP suite (59 steps): bare `obscura serve` 28, Obscura behind the
supervisor 52 (the 7 failures are the refusals above plus the iframe click),
headless Chromium 56 (3 harness timing failures).

## Footprint

DGX Spark (aarch64 Linux), median of 5 runs, fresh profile each run, bare
engines (the supervisor adds about 15 MB RSS). "3 pages" = example.com, the
Wikipedia "Web browser" article and the local SPA, each in its own tab.

| | Obscura 0.2.3 | headless Chromium 145 |
| --- | --- | --- |
| spawn to CDP endpoint | 14 ms | 164 ms |
| spawn to first page loaded (example.com) | 162 ms | 476 ms |
| memory after first page | 35 MB RSS (1 process) | 884 MB RSS summed over 11 processes |
| memory after 3 pages | 78 MB RSS / 76 MB PSS | 1181 MB RSS summed / 453 MB PSS |

## Risks

* **Identity masquerade is always on.** Even without `--stealth`, Obscura
  presents itself as Chrome: `navigator.userAgent` is Windows Chrome 143,
  `navigator.platform` is `Win32`, `navigator.webdriver` is `false`, `window.chrome`
  exists, the screen is 3840x2160, WebGL is absent, and `/json/version`
  reports `Chrome/145`. This comes from its built-in "consistent browser
  profile" and cannot be turned off; only the User-Agent string can be
  overridden. Sites therefore see a regular Windows Chrome, not an automation
  tool. Agents using it must follow the same rules as with any browser: no
  credential stuffing, no scraping that violates a site's terms, and stop
  when a site challenges automation instead of trying to get around it.
* **Stealth mode is off.** browser-control never passes `--stealth` (TLS
  fingerprint impersonation plus tracker blocking), never sets proxies,
  profile rotation or geolocation, and recommends the non-stealth release
  archive. An agent tool should not be in the business of evading bot
  detection.
* **Guards relaxed to match Chrome.** `--allow-private-network` lets pages
  reach `localhost`/RFC1918 (needed for local dev servers), and
  `--allow-file-access` allows `file://` and file uploads. Both are what
  Chrome allows; the CDP port is loopback-only and Origin/Host-checked.
* **Dialogs.** Natively Obscura accepts `confirm()` without telling the
  client, so an agent would approve "Delete everything?" prompts unseen. The
  supervisor injects a shim that dismisses dialogs (as Playwright does on
  Chrome) and records them in `window.__browserControlDialogs`. A page that
  replaces `window.confirm` itself is not affected by the shim.
* **Partial web platform.** Missing events and APIs can make pages behave
  differently from Chrome without any error. When results look wrong, retry in
  Chrome.
* **Shared registry.** browser-control binaries older than this change fail
  `list-running`, `browser_list` and the "most recent live browser" fallback
  once the shared registry holds a `kind = obscura` row. Update every copy
  that shares the data dir (for example a `~/.local/bin` or Homebrew install
  used by another agent host). From this version on, rows with unknown kinds
  are skipped with a warning instead.
