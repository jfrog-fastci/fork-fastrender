# Multiprocess threat model: untrusted renderer IPC

This document defines the **security boundary** we are building toward for the multiprocess browser.
Its purpose is to prevent “drift”: future code changes should consistently treat **renderer-originated
data as untrusted**.

Related:
- Site isolation process model (process assignment + OOPIF semantics): [`docs/site_isolation.md`](site_isolation.md)
- Linux IPC checklist (shared memory + FD passing): [`docs/ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md)

**Status / repo reality (today):**

- The windowed `browser` app currently runs a “renderer” on a dedicated worker thread, not a separate
  OS process.
- The UI↔worker protocol already exists as enums:
  - `UiToWorker` / `WorkerToUi` in [`src/ui/messages.rs`](../src/ui/messages.rs)
  - implementation in [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)

The long-term goal is to move the worker into a **sandboxed renderer process** with the same
message protocol. For that reason, this doc treats the worker/renderer as if it were already a
separate, compromised process.

---

## Attacker model

- The attacker controls **web content**:
  - HTML/CSS/JS
  - network responses (including redirects)
  - image/font/media bytes
- The attacker may exploit a bug in the renderer and gain **arbitrary code execution inside the
  renderer sandbox**.

### Assets to protect

- **Browser integrity:** address bar, tab strip, UI actions (no spoofing via renderer-controlled data)
- **Privileged user data:** bookmarks, history, downloads path, profile/session state (and in the
  future: cookies, credentials)
- **Host system:** filesystem and arbitrary network access (once sandboxing lands)
- **Other tabs:** crash/compromise isolation per tab/process

---

## Process model and trust levels

Target model (conceptual):

| Component | Trust | Owns / is allowed to do |
|---|---|---|
| **Browser process** | Trusted | Window/UI, navigation policy, profile state (bookmarks/history/session), OS integration |
| **Renderer process** (per tab or site) | **Untrusted + sandboxed** | Parse/layout/JS/paint. Must be survivable if compromised. |
| (Future) Network process | Restricted | HTTP(S) fetch, cookie policy, redirects, response limits |

### Trusted chrome UI and privileged JS

In the target architecture, the browser process may render its own chrome UI using FastRender
(`renderer-chrome`). Those UI pages are **trusted** and may be given additional capabilities via a
privileged JS bridge (the `globalThis.chrome` API). This bridge must never be installed in untrusted
content realms.

See [`docs/chrome_js_bridge.md`](chrome_js_bridge.md) for the API surface and its capability-based
installation model.

### Trust boundary

**All messages from renderer → browser are untrusted.**

In the current codebase, this corresponds to treating all `WorkerToUi` fields as attacker-controlled
even though they are delivered over in-process channels today.

---

## IPC surfaces and required invariants

### IPC directions

- **Browser → renderer**: `UiToWorker` (trusted by definition; still validate to avoid self-DoS)
- **Renderer → browser**: `WorkerToUi` (**untrusted**; must be validated defensively)

### Required invariants for *renderer → browser* messages

When adding/changing any `WorkerToUi` message or handling its fields in the UI/browser side:

1. **Bounded message sizes (no unbounded allocations)**
   - Every payload must have a hard maximum size and must be rejected/clamped if exceeded.
   - Examples in-tree:
     - Viewport/pixmap size clamping: [`src/ui/browser_limits.rs`](../src/ui/browser_limits.rs)
     - Favicon payload bounds: `FAVICON_MAX_EDGE_PX` / `MAX_FAVICON_BYTES` in
       [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)
   - For any new `Vec`/`String`/blob fields: introduce an explicit `MAX_*` constant and enforce it on
     both ends of the protocol.

2. **URL scheme allowlist (treat URLs as capabilities)**
   - Any URL originating from the renderer (e.g. “open in new tab”, “download this”, “navigate to
     …”) must be parsed and validated by the browser process before being acted on.
   - Schemes like `javascript:`, `data:`, and other opaque/non-hierarchical schemes must be rejected.
   - Typed/user navigations use a scheme allowlist already (see
     [`src/ui/url.rs::validate_user_navigation_url_scheme`](../src/ui/url.rs)); the same approach
     applies to renderer-provided URLs.

3. **String sanitization for UI/logging**
   - Renderer-controlled strings (page title, hovered URL, error strings, debug log lines, etc.)
     must not be blindly trusted as “safe to display”.
   - At minimum:
     - enforce a maximum length (avoid memory/UI abuse),
     - strip or replace control characters and newlines where they could corrupt UI/logs,
     - avoid panics if the data is malformed (in the future: during IPC decode).
   - When embedding user-derived strings into HTML (internal pages), they must be escaped; see
     `escape_html` in [`src/ui/about_pages.rs`](../src/ui/about_pages.rs).

4. **No panics on decode / parse**
   - IPC decode must be fully fallible: never `unwrap()`/panic on untrusted bytes.
   - Unknown/invalid messages should be rejected without crashing the browser.
   - Prefer “drop message / kill renderer / show crash UI” over propagating a panic across the trust
     boundary.

---

## `about:` pages, bookmarks, and history (trusted-only)

### `about:` pages are internal

`about:*` pages are **not fetched from the network**. They exist to expose browser features like
history and bookmarks.

Relevant implementation:
- Templates + escaping live in [`src/ui/about_pages.rs`](../src/ui/about_pages.rs)
- `ABOUT_BASE_URL` is `about:blank` to avoid accidentally resolving relative URLs against the last
  network origin.

### Bookmarks/history are browser-owned state

- Bookmarks store: [`src/ui/bookmarks.rs`](../src/ui/bookmarks.rs)
- Global history store: [`src/ui/global_history.rs`](../src/ui/global_history.rs)
  - `about:` pages are excluded from history, and history recording uses a conservative scheme
    allowlist (`http`/`https`/`file`).

**Trust rule:** the renderer must never be the authority for bookmarks/history contents, nor should
it be able to request privileged browser state via untrusted IPC.

In practice, this means:
- Only the browser process reads/writes the on-disk profile state.
- Internal pages that display profile data must be generated from **browser-owned snapshots** and
  HTML-escaped (not directly from renderer-supplied strings).

---

## Crash isolation expectations

Renderer crashes are expected and must be contained:

- A renderer crash/hang must **not** crash the browser process.
- Browser should detect renderer failure (IPC channel close, timeout, invalid message) and:
  - mark only that tab/site instance as crashed,
  - show a deterministic “tab crashed” UI,
  - allow the user to reload, which creates a fresh renderer instance.
- Profile state (bookmarks/history/session) must remain consistent and saved by the browser process
  even if renderers crash repeatedly.

---

## Checklist for IPC changes (use this in reviews)

- Does this add a new renderer→browser payload? If yes:
  - What is the **maximum size**? Where is it enforced?
  - Is there a **URL allowlist** if it contains a URL?
  - Are strings **sanitized/truncated** before UI display/logging?
  - Can decode/parse **ever panic**?
- Does this message grant the renderer a new capability over the browser (filesystem, network,
  profile state)? If yes: redesign; the browser must remain the policy/authority.
