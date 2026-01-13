# Multiprocess threat model: browser / renderer / network (untrusted renderer IPC)

This document defines the **security boundary** we are building toward for the multiprocess browser.
Its purpose is to prevent drift: future code changes should consistently treat
**renderer-originated data as untrusted**.

Related:
- Site isolation process model (process assignment + OOPIF semantics): [`docs/site_isolation.md`](site_isolation.md)
- Network process & IPC surface (HTTP, cookies, WebSocket, downloads): [`docs/network_process.md`](network_process.md)
- Linux IPC checklist (shared memory + FD passing): [`docs/ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md)
- OS sandbox entrypoint / platform docs: [`docs/renderer_sandbox.md`](renderer_sandbox.md)
- OS sandbox policy overview (seccomp/AppContainer/etc): [`docs/sandboxing.md`](sandboxing.md)
- Linux sandbox design (rlimits/fd hygiene/namespaces/Landlock/seccomp): [`docs/security/sandbox.md`](security/sandbox.md)
- Windows renderer sandbox boundary (Job/AppContainer details): [`docs/windows_sandbox.md`](windows_sandbox.md)
- IPC transport invariants (framing + size caps + shared memory safety): [`docs/ipc.md`](ipc.md)

## Status / repo reality (today)

FastRender is currently **single-process**. The “renderer” work for the windowed browser runs on a
dedicated worker thread, not a separate OS process.

However, the UI↔worker protocol already exists and should be treated as the future IPC surface:

- `UiToWorker` / `WorkerToUi` in [`src/ui/messages.rs`](../src/ui/messages.rs)
- Canonical implementation in [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)

Resource fetching is currently also in-process and is the natural seam for a future network process:

- `ResourceFetcher` / `HttpFetcher` / `ResourceAccessPolicy` in [`src/resource.rs`](../src/resource.rs)

This doc intentionally treats the worker/renderer as if it were already a separate, compromised
process.

---

## Attacker model

- The attacker controls **web content**:
  - HTML/CSS/JS
  - network responses (including redirects)
  - image/font/media bytes
- The attacker may exploit a bug in the renderer and gain **arbitrary code execution inside the
  renderer sandbox**.

### Assets to protect

These are the “must not lose” assets even under a renderer compromise:

- **Address bar / chrome integrity**
  - No renderer-controlled address bar spoofing (committed URL is browser/network authority).
  - Renderer cannot draw outside its allocated content viewport.
- **Browser profile data**
  - Bookmarks, history, session state (`src/ui/bookmarks.rs`, `src/ui/history.rs`, `src/ui/session.rs`)
  - Cookies and other origin storage (today largely in `src/resource.rs`; future network process)
  - Passwords / credentials / autofill (not fully implemented yet, but a core target)
- **Host system**
  - Filesystem confidentiality/integrity (no arbitrary reads/writes by renderer)
  - Network credentials and identity (cookies, proxy creds, TLS client auth)
- **Other tabs / sites**
  - Crash isolation and (eventually) site isolation: one tab/site compromise should not imply
    compromise of others.

---

## Process model and trust levels

Target model (conceptual):

| Component | Trust | Owns / is allowed to do |
|---|---|---|
| **Browser process** | Trusted | Window/UI, navigation policy, profile state (bookmarks/history/session), OS integration |
| **Renderer process** (per tab or site) | **Untrusted + sandboxed** | Parse/layout/JS/paint. Must be survivable if compromised. |
| **Network process** | Less trusted + sandboxed | HTTP(S) fetch, redirects, cookie policy, response limits, decompression |

Notes:

- “Network process” is intentionally **less trusted** than the browser process: it parses huge amounts
  of attacker-controlled bytes. If compromised, it should still not be able to alter chrome UI state
  or access browser profile secrets.
- In site isolation modes where a renderer process is assigned to a single `SiteKey`/origin, the
  renderer should enforce a **process-level SiteLock** (browser-provided) so a buggy browser or
  compromised renderer cannot silently commit a cross-site navigation inside a locked process. See
  [`docs/site_isolation.md`](site_isolation.md) for details.
- In repo reality today, the network surface is implemented by `src/resource.rs` (e.g. `HttpFetcher`),
  and some “network + filesystem write” behavior exists in the browser worker (`src/ui/render_worker.rs`
  downloads). Multiprocess work should move these responsibilities out of the renderer.

### Trusted chrome UI and privileged JS

In the target architecture, the browser process may render its own chrome UI using FastRender
(`renderer-chrome`). Those UI pages are **trusted** and may be given additional capabilities via a
privileged JS bridge (the `globalThis.chrome` API). This bridge must never be installed in untrusted
content realms.

See [`docs/chrome_js_bridge.md`](chrome_js_bridge.md) for the API surface and its capability-based
installation model.

Renderer-chrome also introduces privileged internal URL schemes (`chrome://` for built-in assets and
`chrome-action:` for browser actions). These schemes must be rejected in untrusted renderer/content
contexts; see [`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md).

### Trust boundary statement

**All messages from renderer → browser are untrusted.**

In the current codebase, this corresponds to treating all `WorkerToUi` fields as attacker-controlled
even though they are delivered over in-process channels today.

---

## Attack surfaces and required invariants

### IPC directions (browser ↔ renderer)

- **Browser → renderer**: `UiToWorker`
  - Trusted by definition, but still validate/clamp to avoid self-DoS (e.g. absurd viewport sizes).
- **Renderer → browser**: `WorkerToUi`
  - **Untrusted**; must be validated defensively before any UI/OS effect.

### Required invariants for *renderer → browser* messages

When adding/changing any `WorkerToUi` message or handling its fields in the UI/browser side:

1. **Chrome integrity (no address bar spoofing)**
   - Renderer must not be authoritative for:
     - what URL is shown in the address bar
     - whether a navigation is “committed”
   - Renderer output must be clipped to the content viewport (no drawing into chrome regions).

2. **Bounded message sizes (no unbounded allocations)**
   - Every payload must have a hard maximum size and must be rejected/clamped if exceeded.
   - Examples in-tree:
     - Viewport/pixmap size clamping: [`src/ui/browser_limits.rs`](../src/ui/browser_limits.rs)
     - Favicon payload bounds: `FAVICON_MAX_EDGE_PX` / `MAX_FAVICON_BYTES` in
       [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)
   - For any new `Vec`/`String`/blob fields: introduce an explicit `MAX_*` constant and enforce it on
     both ends of the protocol.

3. **URL scheme allowlist (treat URLs as capabilities)**
   - Any URL originating from the renderer (e.g. “open in new tab”, “download this”, “navigate to
     …”) must be parsed and validated by the browser process before being acted on.
   - Typed/user navigations already apply a scheme allowlist (see
     [`src/ui/url.rs::validate_user_navigation_url_scheme`](../src/ui/url.rs)); the same approach
     applies to renderer-provided URLs.

4. **String sanitization for UI/logging**
   - Renderer-controlled strings (page title, hovered URL, error strings, debug log lines, etc.)
     must not be blindly trusted as “safe to display”.
   - At minimum:
     - enforce a maximum length (avoid memory/UI abuse),
     - strip or replace control characters and newlines where they could corrupt UI/logs.
   - When embedding user-derived strings into HTML (internal pages), they must be escaped; see
     `escape_html` in [`src/ui/about_pages.rs`](../src/ui/about_pages.rs).

5. **Strict IPC allowlist + no panics on decode / parse**
   - IPC decode must be fully fallible: never `unwrap()`/panic on untrusted bytes.
   - Unknown/invalid messages should be rejected without crashing the browser.
   - Prefer “drop message / kill renderer / show crash UI” over propagating a panic across the trust
     boundary.

### Shared memory frame buffers (renderer → browser)

Today, the in-process worker sends an owned `tiny_skia::Pixmap` in `WorkerToUi::FrameReady`
(`RenderedFrame` in `src/ui/messages.rs`).

In a multiprocess design, this becomes a shared-memory handle (or similar). Attack surface includes:

- Size/stride/format mismatches (integer overflow → out-of-bounds reads in compositor)
- Too-large allocations (DoS)
- Lifetime/ownership bugs (use-after-free across processes)

Required invariants:

- The browser process validates dimensions/stride against hard caps *before* mapping/reading.
- Prefer browser-allocated shared memory (browser chooses size; renderer fills it) over
  renderer-allocated buffers.

### Renderer ↔ network surface (resource fetching)

In repo reality today, the renderer can fetch resources directly through `src/resource.rs`, including:

- Network I/O (`HttpFetcher`)
- Cookie plumbing (`ResourceFetcher::cookie_header_value`, `store_cookie_from_document`)
- Decompression/parsing of response bodies
- Potential `file://` reads (depending on policy and call site)

In multiprocess:

- The renderer process must not open sockets.
- The renderer should use a `ResourceFetcher` implementation that is *pure IPC* to a network process.
- The network process becomes the authority for:
  - request policy (`ResourceAccessPolicy` in `src/resource.rs`)
  - CORS enforcement (`src/resource/cors.rs`)
  - credentials/cookies attachment
  - response size limits and bounded decoding

### URL handling and scheme policy

URL parsing and normalization influences security UI (what origin the user thinks they’re on) and
capability surfaces (`file://`, disallowed schemes, etc.).

Repo reality:

- Address-bar normalization and scheme allowlisting lives in `src/ui/url.rs` (e.g.
  `validate_user_navigation_url_scheme`).

Invariants:

- The browser process applies scheme allowlists for user-typed input.
- Renderer-originated navigations (link clicks, redirects) are treated as **requests**, not authority.

### File picker / file drop

The existing protocol passes OS file paths across the UI→worker boundary:

- `UiToWorker::FilePickerChoose`
- `UiToWorker::DropFiles`

(See `PathBuf` fields in [`src/ui/messages.rs`](../src/ui/messages.rs).)

In multiprocess, renderer must not gain ambient filesystem access. Prefer capability-based handles:

- Browser returns scoped read handles/tokens, not raw paths.
- Avoid leaking full local paths to web content when not necessary (paths can be identifying).

### Downloads

Downloads cross both sensitive boundaries:

- Network (attacker-controlled bytes)
- Filesystem writes (persistent side effects)

Repo reality:

- Download initiation is `UiToWorker::StartDownload` (`src/ui/messages.rs`).
- Current implementation performs HTTP and writes to disk in `src/ui/render_worker.rs`, using helpers
  in `src/ui/downloads.rs` (filename sanitization, `.part` files).

In multiprocess:

- Renderer may request “download this URL”, but the browser process chooses destination and writes.
- Network process performs fetch; browser (or a dedicated download service) performs write with a
  tightly-scoped filesystem sandbox.

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

## Testing strategy (encode the invariants)

These tests should be treated as *security regression tests*, not just stability checks.

1. **Crash isolation**
   - Spawn a renderer process, force a crash (panic/abort/SIGKILL), and assert:
     - browser process remains alive
     - other tabs (other renderers) keep working
     - crashed tab transitions to a deterministic “tab crashed” UI state

2. **Filesystem denial tests**
   - In a sandboxed renderer, attempt to:
     - open a known file (e.g. `/etc/passwd` on Unix)
     - create/write a file in a temp directory
   - Assert denial at the OS sandbox layer (not “we didn’t call that API”).

3. **Network denial tests**
   - In a sandboxed renderer, attempt to open a socket / connect to localhost.
   - Assert denial at the OS sandbox layer.

4. **IPC fuzzing**
   - Fuzz browser↔renderer and renderer↔network IPC decoding:
     - random bytes → decode → ensure no panics, no OOM, bounded allocations
     - structured fuzzing for URLs/headers/sizes/enum tags
   - The existing `fuzz/` harness is a natural home for IPC fuzz targets alongside existing fuzzers
     (e.g. `fuzz/fuzz_targets/image_decoding.rs`).

5. **Boundary size / quota tests**
    - Explicit tests that overlarge frames / URLs / favicon payloads are rejected (DoS resistance).
    - Explicit tests that per-renderer **WebSocket connection caps** are enforced in the network
      process, and that capacity is released when sockets close / renderers disconnect.
    - Include integer-overflow style cases (e.g. `width * height * 4` overflow).

---

## Checklist for IPC changes (use this in reviews)

- Does this add a new renderer→browser payload? If yes:
  - What is the **maximum size**? Where is it enforced?
  - Is there a **URL allowlist** if it contains a URL?
  - Are strings **sanitized/truncated** before UI display/logging?
  - Can decode/parse **ever panic**?
  - If it introduces a new shared-memory handle: who allocates it and who validates size/stride?
- Does this message grant the renderer a new capability over the browser (filesystem, network,
  profile state)? If yes: redesign; the browser must remain the policy/authority.
