#!/usr/bin/env node
// Playwright sidecar for browser-control's MCP server.
//
// Speaks NDJSON-over-stdio RPC. Holds one Playwright `Browser` connected
// over CDP for the duration of its run. Tabs are addressed by CDP
// `target_id` (the Rust side's source of truth); the sidecar maintains
// a `target_id -> Page` mapping internally, populated lazily via
// `BrowserContext.newCDPSession(page).send('Target.getTargetInfo')`.
//
// Protocol
// --------
// Request:  {"id": N, "method": "<name>", "params": {...}}
// Response: {"id": N, "result": <any>}
//           {"id": N, "error": {"message": "..."}}
//
// On unknown method: error with message "unknown method".
// On uncaught exception during handling: error with the exception
// message. The sidecar itself never crashes the JSON-RPC loop —
// errors only fail the current request.

import { chromium } from "playwright-core";
import readline from "node:readline";

// ---------------------------------------------------------------------------
// State.
// ---------------------------------------------------------------------------

let browser = null;
let context = null;
/** @type {Map<string, import('playwright-core').Page>} */
const pagesByTargetId = new Map();

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function ok(id, result) {
  send({ id, result });
}

function err(id, message) {
  send({ id, error: { message: String(message) } });
}

// Find a page by CDP target id. Probes pages we haven't seen yet by
// asking each unmapped page for its own target info via a transient
// CDP session. Caches once found.
async function getPage(targetId) {
  if (pagesByTargetId.has(targetId)) {
    return pagesByTargetId.get(targetId);
  }
  if (!context) throw new Error("not connected");
  for (const page of context.pages()) {
    if ([...pagesByTargetId.values()].includes(page)) continue;
    let session;
    try {
      session = await context.newCDPSession(page);
      const info = await session.send("Target.getTargetInfo");
      const id = info?.targetInfo?.targetId;
      if (id) pagesByTargetId.set(id, page);
      if (id === targetId) return page;
    } catch {
      // Page may have closed between iteration and lookup; skip.
    } finally {
      try {
        await session?.detach();
      } catch {
        /* ignore */
      }
    }
  }
  throw new Error(`page not found for target_id ${targetId}`);
}

// Drop a target_id from the cache (e.g. tab closed).
function forgetTarget(targetId) {
  pagesByTargetId.delete(targetId);
}

// ---------------------------------------------------------------------------
// Methods.
// ---------------------------------------------------------------------------

async function methodConnect(params) {
  const endpoint = params?.endpoint;
  if (!endpoint) throw new Error("missing 'endpoint'");
  const timeout = params?.timeout_ms ?? 5000;
  if (browser) {
    try {
      await browser.close();
    } catch {
      /* ignore */
    }
  }
  pagesByTargetId.clear();
  browser = await chromium.connectOverCDP(endpoint, { timeout });
  const contexts = browser.contexts();
  context = contexts.length > 0 ? contexts[0] : await browser.newContext();
  // Listen for new pages so future creations are caught early.
  context.on("page", async (page) => {
    try {
      const session = await context.newCDPSession(page);
      const info = await session.send("Target.getTargetInfo");
      const id = info?.targetInfo?.targetId;
      if (id) pagesByTargetId.set(id, page);
      await session.detach();
    } catch {
      /* ignore */
    }
  });
  return { ok: true, pages: context.pages().length };
}

async function methodDispose() {
  try {
    if (browser) await browser.close();
  } catch {
    /* ignore */
  }
  browser = null;
  context = null;
  pagesByTargetId.clear();
  return { ok: true };
}

async function methodSnapshot(params) {
  const page = await getPage(params.target_id);
  // `locator.ariaSnapshot()` returns a YAML-formatted accessibility tree
  // suitable for LLM consumption. Stable Playwright API since ~v1.45.
  const yaml = await page.locator("body").ariaSnapshot();
  return { snapshot: yaml };
}

// Roles that normally represent something a user can activate. When the
// caller supplies only a human-readable element name, search these first so
// `click "Sign in"` resolves the same way a person reading the accessibility
// snapshot would. We deliberately do not silently pick the first match: an
// ambiguous click is much more dangerous than an actionable error.
const CLICKABLE_ROLES = [
  "button",
  "link",
  "menuitem",
  "menuitemcheckbox",
  "menuitemradio",
  "tab",
  "checkbox",
  "radio",
  "switch",
  "option",
  "combobox",
  "textbox",
  "searchbox",
  "spinbutton",
  "slider",
  "treeitem",
];

async function requireUnique(locator, description, correction) {
  const count = await locator.count();
  if (count === 0) {
    throw new Error(
      `no element matched ${description}; capture a fresh browser_snapshot, ` +
        `then retry with its exact role and accessible name${correction}`,
    );
  }
  if (count > 1) {
    throw new Error(
      `${description} matched ${count} elements; refusing to guess. ` +
        `Capture a fresh browser_snapshot and narrow the click${correction}`,
    );
  }
  return locator;
}

async function resolveClickLocator(page, params) {
  const selector = params.selector;
  const element = params.element;
  const role = params.role;
  const exact = params.exact ?? true;

  if (selector && (element || role)) {
    throw new Error(
      "choose one click strategy: `selector`, or semantic `element` with optional `role`",
    );
  }
  if (role && !element) {
    throw new Error("`role` requires `element` (the accessible name)");
  }

  if (selector) {
    const locator = page.locator(selector);
    return {
      locator: await requireUnique(
        locator,
        `selector ${JSON.stringify(selector)}`,
        " with a more specific selector",
      ),
      strategy: "selector",
      selector,
    };
  }

  if (!element) {
    throw new Error(
      "missing click target: pass `element` (recommended, optionally with `role`) or `selector`",
    );
  }

  if (role) {
    const locator = page.getByRole(role, { name: element, exact });
    return {
      locator: await requireUnique(
        locator,
        `${role} named ${JSON.stringify(element)}`,
        " with a more exact accessible name or a selector",
      ),
      strategy: "role",
      role,
      element,
      exact,
    };
  }

  // A role/name pair is the most stable locator available from an aria
  // snapshot. If the caller omitted the role, infer it only when exactly one
  // interactive role matches. This keeps the one-argument CLI convenient
  // without turning duplicate labels into a random click.
  const roleMatches = [];
  for (const candidateRole of CLICKABLE_ROLES) {
    const locator = page.getByRole(candidateRole, { name: element, exact });
    const count = await locator.count();
    if (count > 0) roleMatches.push({ role: candidateRole, locator, count });
  }
  const roleMatchCount = roleMatches.reduce((sum, match) => sum + match.count, 0);
  if (roleMatchCount === 1) {
    const match = roleMatches[0];
    return {
      locator: match.locator,
      strategy: "inferred-role",
      role: match.role,
      element,
      exact,
    };
  }
  if (roleMatchCount > 1) {
    const summary = roleMatches.map((match) => `${match.role}:${match.count}`).join(", ");
    throw new Error(
      `element ${JSON.stringify(element)} matched ${roleMatchCount} interactive elements ` +
        `(${summary}); refusing to guess. Retry with \`role\` from browser_snapshot or use \`selector\``,
    );
  }

  // Some clickable controls have no useful computed role (custom widgets and
  // click handlers on ordinary elements). Visible text is a safe last semantic
  // fallback as long as the result is unique.
  const textLocator = page.getByText(element, { exact });
  return {
    locator: await requireUnique(
      textLocator,
      `visible text ${JSON.stringify(element)}`,
      " with `role`, a more exact name, or `selector`",
    ),
    strategy: "text",
    element,
    exact,
  };
}

async function methodClick(params) {
  const page = await getPage(params.target_id);
  const resolved = await resolveClickLocator(page, params);
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  if (params.button !== undefined) opts.button = params.button;
  if (params.modifiers !== undefined) opts.modifiers = params.modifiers;

  const preview = await resolved.locator.evaluate((node) => ({
    tag: node.tagName.toLowerCase(),
    text: (node.innerText || node.textContent || "").trim().replace(/\s+/g, " ").slice(0, 160),
    aria_label: node.getAttribute("aria-label"),
  }));

  if (params.double_click) {
    await resolved.locator.dblclick(opts);
  } else {
    await resolved.locator.click(opts);
  }

  let title = "";
  try {
    title = await page.title();
  } catch {
    // The click may intentionally close or replace the page. The successful
    // input action is still useful; leave title blank in that case.
  }
  return {
    ok: true,
    strategy: resolved.strategy,
    role: resolved.role,
    element: resolved.element,
    selector: resolved.selector,
    matched: preview,
    url: page.url(),
    title,
  };
}

async function methodType(params) {
  const page = await getPage(params.target_id);
  const selector = params.selector;
  const text = params.text;
  if (!selector) throw new Error("missing 'selector'");
  if (text === undefined) throw new Error("missing 'text'");
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  // Use `fill` for typical input fields; `pressSequentially` if simulating keystrokes.
  if (params.press_sequentially) {
    await page.locator(selector).pressSequentially(text, opts);
  } else {
    await page.locator(selector).fill(text, opts);
  }
  return { ok: true };
}

async function methodHover(params) {
  const page = await getPage(params.target_id);
  const selector = params.selector;
  if (!selector) throw new Error("missing 'selector'");
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  await page.locator(selector).hover(opts);
  return { ok: true };
}

async function methodDrag(params) {
  const page = await getPage(params.target_id);
  const source = params.source_selector;
  const target = params.target_selector;
  if (!source || !target) throw new Error("missing 'source_selector' or 'target_selector'");
  await page.locator(source).dragTo(page.locator(target));
  return { ok: true };
}

async function methodPressKey(params) {
  const page = await getPage(params.target_id);
  const key = params.key;
  if (!key) throw new Error("missing 'key'");
  await page.keyboard.press(key);
  return { ok: true };
}

async function methodWaitFor(params) {
  const page = await getPage(params.target_id);
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  if (params.selector) {
    const state = params.state || "visible";
    await page.locator(params.selector).waitFor({ state, ...opts });
  } else if (params.url_regex) {
    await page.waitForURL(new RegExp(params.url_regex), opts);
  } else if (params.load_state) {
    await page.waitForLoadState(params.load_state, opts);
  } else {
    throw new Error("must supply one of: selector, url_regex, load_state");
  }
  return { ok: true };
}

async function methodPdf(params) {
  const page = await getPage(params.target_id);
  const buf = await page.pdf();
  return { pdf_base64: buf.toString("base64") };
}

async function methodForgetTarget(params) {
  forgetTarget(params.target_id);
  return { ok: true };
}

const METHODS = {
  connect: methodConnect,
  dispose: methodDispose,
  snapshot: methodSnapshot,
  click: methodClick,
  type: methodType,
  hover: methodHover,
  drag: methodDrag,
  press_key: methodPressKey,
  wait_for: methodWaitFor,
  pdf: methodPdf,
  forget_target: methodForgetTarget,
};

// ---------------------------------------------------------------------------
// JSON-RPC loop.
// ---------------------------------------------------------------------------

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  if (!line.trim()) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch (e) {
    send({ id: null, error: { message: `parse error: ${e.message}` } });
    return;
  }
  const id = req.id ?? null;
  const method = req.method;
  const params = req.params || {};
  const fn = METHODS[method];
  if (!fn) {
    err(id, `unknown method: ${method}`);
    return;
  }
  try {
    const result = await fn(params);
    ok(id, result);
  } catch (e) {
    err(id, e?.message ?? String(e));
  }
});

rl.on("close", () => {
  // Best-effort cleanup if the host disconnects.
  if (browser) {
    browser.close().catch(() => {});
  }
  process.exit(0);
});
