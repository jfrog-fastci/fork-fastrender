# Chrome JS bridge (trusted UI pages)

FastRender’s **chrome JS bridge** is a small, privileged JavaScript API intended for **trusted**
“browser chrome” pages (tabs UI, address bar UI, etc). It is **not** the Chrome Extensions API.

This doc is for:

- **Chrome-page authors**: what globals exist and how to use them.
- **Maintainers**: what the trust boundary is and where the privileged surface is installed.

## Status

This bridge is part of the *renderer chrome* workstream (UI rendered by FastRender).
Embeddings that do not support renderer chrome will not install the `chrome` object, so chrome-page
code should be prepared for:

```js
typeof chrome === "undefined"
```

Repo reality (today): the in-tree `browser` binary still renders its chrome via **egui**, so it does
not load a chrome HTML/JS document and therefore does not install this bridge yet (see
[`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md)).

For the privileged internal URL schemes used by renderer-chrome (`chrome://` assets and
`chrome-action:` actions), see [`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md).

If you want a bootstrap interaction model **without JavaScript** (trusted HTML/CSS + `chrome-action:`
links/forms), see [`docs/renderer_chrome_non_js.md`](renderer_chrome_non_js.md).

---

## Purpose

Provide a **minimal, privileged** JS API for “chrome pages” so the browser UI can be written as
HTML/CSS/JS while still being able to:

- drive navigation (back/forward/reload/navigate),
- create/activate/close tabs,
- read a snapshot of browser UI state.

This is intentionally separated from the untrusted web platform surface: `chrome` is not a web
standard API and must not be exposed to arbitrary content.

---

## Trust boundary / security model

FastRender distinguishes between:

1. **Trusted chrome pages** (UI pages)
   - Authored by the embedding application (bundled assets or loaded from a trusted path).
   - Run with a **privileged** JS API (`globalThis.chrome`).
   - Intended to run in the **browser process** in a multiprocess architecture.

2. **Untrusted content pages** (web content)
   - Arbitrary HTML/JS from the network / filesystem.
   - Must **not** be able to call privileged APIs.
   - In a multiprocess architecture, should run in a **sandboxed renderer process**.

The `chrome` bridge is the trust boundary: it is effectively “native code capabilities exposed to JS”.

### Threat model notes (why this is strict)

- If an untrusted page can access `chrome`, it can generally:
  - navigate/close tabs,
  - read browser state,
  - potentially escalate to other privileged operations as the API grows.
- Chrome pages must not render or `innerHTML` untrusted strings without sanitization (XSS in chrome
  pages is privilege escalation).
  - Treat tab titles, URLs, error strings, and any other renderer-provided data as **attacker
    controlled** and render with `textContent` (or equivalent escaping).
- Chrome pages should not embed untrusted documents in a way that inherits privileges.
  - If the chrome UI needs to display untrusted HTML (e.g. remote content), it should be rendered in
    a separate, unprivileged realm/document where `globalThis.chrome` is **not** installed.

---

## Installation model (capability-based)

`globalThis.chrome` is **not** present by default.

An embedder must explicitly install the bridge into a specific JS realm (the chrome/UI realm)
by calling the `vm-js` installer:

```rust
use fastrender::js::chrome_api::{install_chrome_api_bindings_vm_js, ChromeApiHost, ChromeCommand};
use fastrender::js::WindowRealmHost;

struct MyHost;
impl WindowRealmHost for MyHost { /* ... */ }
impl ChromeApiHost for MyHost {
  fn chrome_dispatch(&mut self, cmd: ChromeCommand) -> Result<(), fastrender::error::Error> {
    /* apply cmd in the browser process / UI model */
    Ok(())
  }
}

install_chrome_api_bindings_vm_js::<MyHost>(vm, heap, realm)?;
```

The installer is **non-clobbering**: if `globalThis.chrome` already exists on the realm global
object, it returns `Ok(())` without modifying it.

At runtime, installed JS methods emit a [`ChromeCommand`](../src/js/vmjs/chrome_api.rs) and call
`ChromeApiHost::chrome_dispatch` on the embedder’s host state via the `vm-js` host hooks payload
(typically provided by `VmJsEventLoopHooks`).

Repo reality (today): the installer is implemented in-tree (see
[`src/js/vmjs/chrome_api.rs`](../src/js/vmjs/chrome_api.rs)) and covered by
[`tests/misc/chrome_api_tests.rs`](../tests/misc/chrome_api_tests.rs), but it is not currently
wired into the egui-based `browser` UI.

Default “content page” realms must **not** call this function, so that:

```js
typeof chrome === "undefined"
```

holds in untrusted content.

### Where this should be wired up (vm-js embedding)

In FastRender’s `vm-js` embedding, “environment installation” happens during realm construction.
For example, `WindowRealm` installs deterministic browser shims (`navigator`, `screen`,
`matchMedia`, etc) via `install_window_shims_vm_js` in
[`src/js/vmjs/window_realm.rs`](../src/js/vmjs/window_realm.rs).

The chrome bridge should be installed the same way, but **only** for the trusted chrome/UI realm.
Do not install it in the normal content-page realm.

### Implementation note (in-tree browser)

In the in-tree windowed browser, navigation and tab actions already exist as part of the UI↔worker
protocol (`UiToWorker::{CreateTab,CloseTab,SetActiveTab,Navigate,GoBack,GoForward,Reload,StopLoading}`
in [`src/ui/messages.rs`](../src/ui/messages.rs)). A renderer-chrome embedding is expected to
implement the JS bridge by dispatching to these host-side actions (or an equivalent browser-process
API), rather than exposing internal state directly.

Conceptual mapping (in-tree protocol names):

| Chrome JS | Browser host action |
|---|---|
| `chrome.navigation.navigate(url)` | `UiToWorker::Navigate { tab_id: active, url: normalized, reason: TypedUrl }` |
| `chrome.navigation.back()` | `UiToWorker::GoBack { tab_id: active }` |
| `chrome.navigation.forward()` | `UiToWorker::GoForward { tab_id: active }` |
| `chrome.navigation.reload()` | `UiToWorker::Reload { tab_id: active }` |
| `chrome.navigation.stop()` | `UiToWorker::StopLoading { tab_id: active }` |
| `chrome.tabs.activateTab(id)` | `UiToWorker::SetActiveTab { tab_id: id }` |
| `chrome.tabs.closeTab(id)` | `UiToWorker::CloseTab { tab_id: id }` |
| `chrome.tabs.newTab([url])` | Host allocates a new tab id, then `UiToWorker::CreateTab { .. }` + optional `Navigate` |

---

## API reference (MVP)

All APIs below are exposed under the global `chrome` object.

### `chrome.navigation`

Imperative navigation on the **active tab**.

- `chrome.navigation.navigate(url)`
  - Navigate the active tab based on an address-bar-like input string.
  - Embedders should treat `url` like typed omnibox input (not necessarily a fully-qualified URL):
    - `example.com` → `https://example.com/`
    - bare words → search URL
    - filesystem-looking paths → `file://...`
    - fragments like `#section` resolved against the current tab URL
  - Blocked/unknown schemes (e.g. `javascript:`) must be rejected; see
    [`validate_user_navigation_url_scheme`](../src/ui/url.rs) for the in-tree allowlist used by the
    egui chrome today.
  - In the in-tree egui browser, “typed navigation” normalization lives in
    `BrowserTabState::navigate_typed` in [`src/ui/browser_app.rs`](../src/ui/browser_app.rs) and uses
    [`resolve_omnibox_input`](../src/ui/url.rs) + `validate_user_navigation_url_scheme`.
- `chrome.navigation.back()`
- `chrome.navigation.forward()`
- `chrome.navigation.reload()`
- `chrome.navigation.stop()`
  - Cancel the current in-flight navigation and/or paint work for the active tab.
  - Intended to match the “Stop loading” chrome button behavior:
    - keep the currently committed document visible (do not navigate to an error page),
    - clear provisional/pending navigation state,
    - update loading state promptly so the UI can toggle stop → reload.

### `chrome.tabs`

Tab management within the current window (MVP).

- `chrome.tabs.newTab([url])`
  - Opens a new foreground tab.
  - If `url` is provided, the new tab navigates to it; otherwise the embedder chooses a default
    (commonly `about:newtab`).
  - `url` may be `undefined`/`null` to request the embedder’s default new-tab page.
  - Returns `undefined` (command-only).
  - Like `chrome.navigation.navigate(url)`, the argument should be treated as omnibox-style input
    and normalized/validated by the host (do not assume it is already a safe/absolute URL).
- `chrome.tabs.closeTab(id)`
- `chrome.tabs.activateTab(id)`
  - `id` is an **opaque** tab identifier allocated by the host.
  - Canonical JS representation: **Number safe integer**
    - `typeof id === "number"`
    - `Number.isSafeInteger(id) === true`
    - `0 <= id <= Number.MAX_SAFE_INTEGER` (`2^53 - 1`)
  - Values outside this range must throw a **TypeError** (no silent precision loss).
  - In the in-tree browser protocol, `TabId(0)` is reserved as an “invalid” value; real ids start at
    1 (see `TabId::new` in [`src/ui/messages.rs`](../src/ui/messages.rs)).

### State snapshot

Chrome UIs typically need an initial snapshot of tab state.

Repo reality (today): the in-tree `vm-js` chrome bridge is **command-only** and does not currently
expose a synchronous snapshot API (no `chrome.tabs.getAll()` / `chrome.getState()`).

Embeddings may additionally expose a higher-level snapshot API:

- `chrome.getState(): object` (optional / embedder-defined)
  - If present, returns a snapshot of the browser state needed for chrome UI (tabs, active tab id,
    active tab URL/title/loading, back/forward availability, etc).

Chrome pages should feature-detect which is available.

Example shape (illustrative):

```js
// chrome.getState() (example)
{
  activeTabId: 1,
  tabs: [
    {
      id: 1,
      active: true,
      title: "Example Domain",
      url: "https://example.com/",
      loading: false,
      error: null, // optional string for the last navigation failure
      canGoBack: false,
      canGoForward: false,
    }
  ],
}
```

---

## Event model (recommended; helper implemented in-tree)

Chrome pages may be notified of browser state changes via DOM events dispatched on `window`.

Chrome pages listen with:

```js
window.addEventListener("chrome-tabs", (e) => {
  // e.detail is embedder-defined; commonly a tabs snapshot or full chrome state.
});
```

Some embeddings may also dispatch additional events (e.g. `chrome-navigation`, `chrome-state`).

Host-side implementation (in-tree helper): embedders can dispatch `CustomEvent` asynchronously via:

```rust
use fastrender::js::chrome_events::dispatch_chrome_event_vm_js;

// detail_json is parsed via JSON.parse in the target realm (invalid JSON => detail: null).
dispatch_chrome_event_vm_js(host, event_loop, "chrome-tabs", r#"{"tabs":[...]}"#)?;
```

- `event_type` and `detail_json` are length-capped (DoS resistance / bounded queues).
- `detail_json` is parsed with `JSON.parse`; invalid JSON still dispatches the event with
  `detail: null`.

Recommended: `e.detail` should be the same shape as `chrome.getState()` (or at least contain
`{ tabs, activeTabId }`) so chrome pages can update with a single handler.

If an embedding cannot support events, consider adding an embedder-defined `chrome.getState()`
snapshot getter as a fallback.

---

## Errors and argument validation

The chrome bridge is privileged, but it still validates inputs:

- Wrong arity / wrong types throw **synchronous** JS exceptions (typically `TypeError`).
  - Example: `chrome.tabs.closeTab("not-a-number")` → throws.
- Tab ids must be finite safe integers. Non-integers / `NaN` / out-of-range values throw a
  **TypeError**.
- Unknown tab ids (well-typed but not present) are embedder-defined:
  - the bridge validates types/ranges, but the host may treat unknown ids as a no-op, close the
    active tab, or log/ignore depending on UI policy.
- Invalid/blocked URLs passed to `chrome.navigation.navigate(...)` should throw an exception rather
  than silently doing nothing. The embedder is expected to apply its scheme allowlist (e.g.
  reject `javascript:`).
- The in-tree `vm-js` binding also enforces a hard URL argument size cap (measured in UTF-16 code
  units) to prevent hostile pages from forcing large host allocations.
- If the embedder executes chrome page JS without a `VmJsHostHooksPayload`/host state installed,
  chrome calls throw a **TypeError** (`"chrome API host not available"`). This is an embedder bug.
- Unavailability is not an error:
  - `back()` / `forward()` should be no-ops if there is no history entry in that direction.
  - `stop()` should be a no-op if nothing is loading.
- Navigation failures that happen *after* dispatch (DNS failures, network errors, HTTP failures,
  etc) should generally **not** throw. They should be surfaced via state/events (for example via an
  `error` field in `chrome.getState()` or via a `chrome-navigation`/`chrome-state` event).

The chrome bridge is primarily **command-oriented**.

- Most methods return `undefined` and should be treated as “fire-and-forget”.
- Embedding-defined snapshot APIs (like `chrome.getState()`) should return plain objects/arrays.
- `chrome.tabs.newTab()` returns `undefined`.

---

## Maintainer guidance (extending the bridge)

When adding new APIs under `globalThis.chrome`:

- Treat every method as a **privileged capability**:
  - Only install it in the trusted chrome/UI realm.
  - Do not gate privileged behavior on URL strings or origins that can be influenced by content.
- Validate everything (even in chrome):
  - enforce type/arity checks,
  - cap string lengths and array sizes to avoid chrome-side memory abuse,
  - parse URLs with a strict allowlist (fail closed).
- Prefer returning **snapshots** (plain objects/arrays) over live handles or pointers to host state.
- Keep the surface small and composable:
  - command methods + events/state snapshots are easier to reason about than many synchronous getters.

---

## Minimal example: tab strip + address bar

This is a minimal, framework-free example of wiring a chrome page to the bridge.

```html
<div id="tab-strip"></div>
<button id="new-tab">New tab</button>
<input id="address" placeholder="Enter URL" />
<script>
  if (typeof chrome === "undefined") {
    throw new Error("chrome JS bridge is not installed in this realm");
  }

  let state = { tabs: [] };

  function renderTabs(state) {
    const strip = document.getElementById("tab-strip");
    strip.textContent = "";

    for (const tab of state.tabs || []) {
      const isActive = tab.active ?? (tab.id === state.activeTabId);
      const row = document.createElement("div");
      row.className = isActive ? "tab active" : "tab";

      const label = document.createElement("button");
      label.textContent = tab.title || tab.url || "New tab";
      label.onclick = () => chrome.tabs.activateTab(tab.id);

      const close = document.createElement("button");
      close.textContent = "×";
      close.onclick = (e) => {
        e.stopPropagation();
        chrome.tabs.closeTab(tab.id);
      };

      row.appendChild(label);
      row.appendChild(close);
      strip.appendChild(row);
    }
  }

  function refresh() {
    // Optional embedder-defined snapshot getter.
    if (typeof chrome.getState === "function") {
      state = chrome.getState() || state;
    }
    renderTabs(state);
  }

  // Address bar -> navigate active tab.
  document.getElementById("address").addEventListener("keydown", (e) => {
    if (e.key === "Enter") chrome.navigation.navigate(e.target.value);
  });

  document.getElementById("new-tab").onclick = () => chrome.tabs.newTab();

  // Event-driven updates when available.
  window.addEventListener("chrome-tabs", (e) => {
    state = e.detail || state;
    renderTabs(state);
  });
  window.addEventListener("chrome-state", (e) => {
    state = e.detail || state;
    renderTabs(state);
  });

  // Initial paint.
  refresh();
</script>
```
