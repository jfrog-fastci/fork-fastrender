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

### Implementation note (in-tree browser)

In the in-tree windowed browser, navigation and tab actions already exist as part of the UI↔worker
protocol (`UiToWorker::{CreateTab,CloseTab,SetActiveTab,Navigate,GoBack,GoForward,Reload,StopLoading}`
in [`src/ui/messages.rs`](../src/ui/messages.rs)). A renderer-chrome embedding is expected to
implement the JS bridge by dispatching to these host-side actions (or an equivalent browser-process
API), rather than exposing internal state directly.

---

## API reference (MVP)

All APIs below are exposed under the global `chrome` object.

### `chrome.navigation`

Imperative navigation on the **active tab**.

- `chrome.navigation.navigate(url)`
  - Navigate the active tab to `url`.
- `chrome.navigation.goBack()`
- `chrome.navigation.goForward()`
- `chrome.navigation.reload()`
- `chrome.navigation.stop()`

### `chrome.tabs`

Tab management within the current window (MVP).

- `chrome.tabs.newTab([url])`
  - Opens a new foreground tab.
  - If `url` is provided, the new tab navigates to it; otherwise the embedder chooses a default
    (commonly `about:newtab`).
- `chrome.tabs.closeTab(id)`
- `chrome.tabs.activateTab(id)`
  - `id` is an **opaque** tab identifier allocated by the host. Treat it as a `Number` that must be
    a safe integer (`Number.isSafeInteger(id) === true`).

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

---

## Errors and argument validation

The chrome bridge is privileged, but it still validates inputs:

- Wrong arity / wrong types throw **synchronous** JS exceptions (typically `TypeError`).
  - Example: `chrome.tabs.closeTab("not-a-number")` → throws.
- Tab ids must be finite safe integers. Non-integers / `NaN` / out-of-range values should throw
  (recommended: `RangeError`).
- Invalid/blocked URLs passed to `chrome.navigation.navigate(...)` should throw an exception rather
  than silently doing nothing. The embedder is expected to apply its scheme allowlist (e.g.
  reject `javascript:`).

The chrome bridge is primarily **command-oriented**. Chrome pages should not rely on return values;
instead, treat calls as “fire-and-forget” and reflect resulting state changes via events/state
snapshot APIs.

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
      const el = document.createElement("button");
      const isActive = tab.active ?? (tab.id === state.activeTabId);
      el.className = isActive ? "tab active" : "tab";
      el.textContent = tab.title || tab.url || "New tab";
      el.onclick = () => chrome.tabs.activateTab(tab.id);

      const close = document.createElement("button");
      close.textContent = "×";
      close.onclick = (e) => {
        e.stopPropagation();
        chrome.tabs.closeTab(tab.id);
      };

      el.appendChild(close);
      strip.appendChild(el);
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
