# Reliable clicking

`browser-control` exposes clicking as a first-class operation so agents do not
have to construct JavaScript such as `document.querySelector(...).click()`.
The click is executed through Playwright, which waits for the element to be
attached, visible, stable, enabled, and able to receive pointer events. It then
scrolls as needed and sends trusted browser input.

## Recommended MCP workflow

1. Capture the page accessibility tree:

   ```text
   browser_snapshot({})
   ```

2. Copy the element's role and accessible name into `browser_click`:

   ```json
   {
     "element": "Sign in",
     "role": "button"
   }
   ```

3. Use the returned structured details (`strategy`, matched element preview,
   URL, and title) to confirm where the click landed.

If the accessible name is unique, `element` can be used alone:

```json
{ "element": "Continue" }
```

With no role, browser-control first looks for one uniquely named interactive
control. It falls back to unique visible text only when no interactive role
matches. It never silently takes the first of several matches.

## CLI workflow

The same operation is available without writing an MCP call:

```sh
# Best: a named tab plus role/accessibility name.
browser-control click -b brave/checkout "Place order" --role button

# Convenient when the name or visible text is unique.
browser-control click -b brave/checkout "Continue"

# URL routing for an existing unnamed page.
browser-control click -b brave --target '^https://shop\.example/checkout' \
  "Place order" --role button

# Selector fallback.
browser-control click -b brave/checkout --selector '[data-testid="place-order"]'
```

CLI clicks require exactly one page. A named tab is the safest choice for
repeatable automation. With a bare browser selector, the command works only
when that browser has one live page. If several pages are open, use
`-b browser/tab` or `--target URL_REGEX`; ambiguous URL regexes also fail.

Use `--json` to return the complete click result. Exact semantic matching is
the default; use `--fuzzy` only when the page's name is dynamic. Other optional
gestures are available through `--double-click`, `--button left|right|middle`,
and repeatable `--modifier` flags.

## Selector fallback

Semantic role/name targeting is more resilient to generated class names and
DOM refactors. When the accessibility tree is incomplete, pass a CSS or
Playwright selector instead:

```json
{
  "selector": "[data-testid='save']"
}
```

Do not combine `selector` with `element` or `role`; they are separate targeting
strategies. A selector must also resolve to exactly one element.

## Correcting failures

`no element matched`

: Capture a fresh `browser_snapshot`. The page may have re-rendered, or the
  accessible name may differ from the visible text. Retry with the exact name
  and role shown in the snapshot.

`matched N elements; refusing to guess`

: Add the element's `role`, make the name exact, or use a more specific
  selector. Do not work around this by taking the first DOM match.

`matched N live pages; refusing to guess`

: Route the CLI call to a named tab (`-b brave/checkout`) or add a narrower
  `--target` URL regex.

Playwright actionability timeout

: The element exists but is hidden, disabled, moving, or covered by another
  element. Capture a screenshot/snapshot, wait for the page state if needed,
  and retry. Avoid forcing the click unless the site-specific behavior is
  understood.

## Compatibility

CLI `click` and the Playwright-sidecar MCP interaction tools currently require
a Chromium-family browser (`chrome`, `edge`, `chromium`, or `brave`) and either
Bun or Node.js + npm. Engine-agnostic reads, navigation, screenshots, fetch,
cookies, storage, and tab operations continue to work over Firefox BiDi.
