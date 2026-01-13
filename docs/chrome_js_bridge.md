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
not load a chrome HTML/JS document and does not install this bridge yet (see
[`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md)).

For the privileged internal URL schemes used by renderer-chrome (`chrome://` assets and
`chrome-action:` actions), see [`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md).

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
by calling:

```rust
install_chrome_api_bindings_vm_js(/* realm + host state */);
```

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
| `chrome.navigation.goBack()` | `UiToWorker::GoBack { tab_id: active }` |
| `chrome.navigation.goForward()` | `UiToWorker::GoForward { tab_id: active }` |
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
- `chrome.navigation.goBack()`
- `chrome.navigation.goForward()`
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

### State snapshot (optional)

Chrome UIs typically need an initial snapshot of tab state. Depending on the embedder build, one of
these may exist:

- `chrome.getState(): object`
  - Returns a best-effort snapshot of the browser state needed for chrome UI (tabs, active tab id,
    active tab URL/title/loading, back/forward availability, etc).

or:

- `chrome.tabs.getAll(): Array<object>`
  - Returns a snapshot of open tabs (including which one is active).

Chrome pages should feature-detect which is available.

Recommended minimal shape (embedder-defined, but should be stable within an embedding):

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
      canGoBack: false,
      canGoForward: false,
    }
  ],
}
```

---

## Event model (optional)

Chrome pages may be notified of browser state changes via DOM events dispatched on `window`.

If implemented by the embedder, listen with:

```js
window.addEventListener("chrome-tabs", (e) => {
  // e.detail is embedder-defined; commonly a tabs snapshot or full chrome state.
});
```

Some embeddings may also dispatch additional events (e.g. `chrome-navigation`, `chrome-state`).

If events are not available, chrome pages can fall back to polling via `chrome.getState()` /
`chrome.tabs.getAll()`.

Recommended: `e.detail` should be the same shape as `chrome.getState()` (or at least contain
`{ tabs, activeTabId }`) so chrome pages can update with a single handler.

Recommended implementation: dispatch `CustomEvent`:

```js
window.dispatchEvent(new CustomEvent("chrome-tabs", { detail: chromeState }));
```

---

## Errors and argument validation

The chrome bridge is privileged, but it still validates inputs:

- Wrong arity / wrong types throw **synchronous** JS exceptions (typically `TypeError`).
  - Example: `chrome.tabs.closeTab("not-a-number")` → throws.
- Tab ids must be finite safe integers. Non-integers / `NaN` / out-of-range values throw a
  **TypeError**.
- Unknown tab ids (well-typed but not present) should throw a **RangeError** to help surface bugs in
  chrome UI logic.
- Invalid/blocked URLs passed to `chrome.navigation.navigate(...)` should throw an exception rather
  than silently doing nothing. The embedder is expected to apply its scheme allowlist (e.g.
  reject `javascript:`).

The chrome bridge is primarily **command-oriented**. Chrome pages should not rely on return values;
instead, treat calls as “fire-and-forget” and reflect resulting state changes via events/state
snapshot APIs.

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
  function getChromeState() {
    if (typeof chrome === "undefined") {
      throw new Error("chrome JS bridge is not installed in this realm");
    }
    if (typeof chrome.getState === "function") {
      return chrome.getState();
    }
    if (chrome.tabs && typeof chrome.tabs.getAll === "function") {
      const tabs = chrome.tabs.getAll();
      return { tabs };
    }
    return { tabs: [] };
  }

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
    renderTabs(getChromeState());
  }

  // Address bar -> navigate active tab.
  document.getElementById("address").addEventListener("keydown", (e) => {
    if (e.key === "Enter") chrome.navigation.navigate(e.target.value);
  });

  document.getElementById("new-tab").onclick = () => chrome.tabs.newTab();

  // Event-driven updates when available.
  window.addEventListener("chrome-tabs", refresh);
  window.addEventListener("chrome-navigation", refresh);

  // Initial paint.
  refresh();
</script>
```
