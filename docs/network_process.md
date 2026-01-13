# Network process & IPC surface

In FastRender’s **multiprocess mode**, network access is split out of the renderer into a dedicated
**network process**.

This is primarily a **security boundary**:

- The **renderer process** executes untrusted page content (HTML/CSS/JS) and must not be able to open
  sockets or exfiltrate data directly.
- The **network process** is the only place where outbound network access is allowed.
- All network operations are exposed to the renderer via a **narrow, validated IPC protocol** and
  ergonomic **proxy objects**.

This document is aimed at new contributors: it should make it obvious where network code is allowed,
where it is forbidden, and how to extend the network IPC surface safely.

## Repo reality (today)

FastRender is actively transitioning to a multiprocess architecture; some pieces are already in-tree,
and others are scaffolding. A few important “don’t get surprised” points:

- The default feature set enables **in-process networking** and **in-process WebSockets**:
  - Cargo features: `direct_network`, `direct_websocket`, `direct_filesystem` (see `Cargo.toml`).
  - Sandboxed renderer builds must disable these and instead use IPC proxies.
  - CI has a “no in-process networking” build configuration via
    `--no-default-features --features renderer_minimal`.
- There is a minimal `network` subprocess today:
  - Binary: [`src/bin/network.rs`](../src/bin/network.rs)
  - Spawn helper (library): [`src/network_process/client.rs`](../src/network_process/client.rs)
    (re-exported via [`src/network_process/mod.rs`](../src/network_process/mod.rs))
  - Current protocol is intentionally tiny: `Hello { token, role }` + `Fetch { url }` +
    `DownloadStart { url }` (browser-only) + `Shutdown` (browser-only) (see
    `fastrender::network_process::ipc` in [`src/network_process/ipc.rs`](../src/network_process/ipc.rs)).
    - Security note: the role is **not** trusted from the client. The spawn helper provisions
      separate auth tokens for browser vs renderer connections, and the network process validates
      that the claimed `role` matches the token-derived role (a compromised renderer cannot claim
      `Browser` to access download streaming).
    - Security note: the `network_process::ipc` framing helpers are a prototype, but they *do*
      enforce per-direction frame caps (`MAX_INBOUND_FRAME_BYTES`, `MAX_OUTBOUND_FRAME_BYTES`) and
      deny unknown fields. Treat these limits as security-sensitive; do not remove them.
- There are additional (more complete) IPC protocols already defined, even if not yet wired up
  end-to-end:
  - Browser ↔ network protocol schema (serde messages + validation + `expected_fds()` planning):
    [`src/ipc/protocol/network.rs`](../src/ipc/protocol/network.rs)
  - Full `ResourceFetcher` proxy protocol (JSON over TCP): [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs)
  - Bounded binary network transport with explicit per-field limits (request/response/events):
    [`src/net/transport.rs`](../src/net/transport.rs)
  - WebSocket IPC message schema + validation helpers:
    [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) and [`src/ipc/network.rs`](../src/ipc/network.rs)

If you are changing IPC framing/limits, also read:
- IPC transport invariants (framing + size caps + shared memory safety): [`docs/ipc.md`](ipc.md)
- Renderer/network threat model: [`docs/multiprocess_threat_model.md`](multiprocess_threat_model.md)
- Renderer sandbox entrypoint (all platforms): [`docs/renderer_sandbox.md`](renderer_sandbox.md)
- Linux renderer sandbox deep dive (IPC assumptions + seccomp policy): [`docs/security/sandbox.md`](security/sandbox.md)

## Process roles

### Browser process (trusted)

Responsibilities:

- UI / window management (chrome)
- Navigation decisions and tab orchestration
- Persistent user state (profile: history/bookmarks/settings, and cookie persistence when enabled)
- Spawning and supervising renderer + network processes
- Acting as the security “root of trust” for IPC policy decisions

Trust model:

- Not sandboxed (or less sandboxed than renderer/network).
- Treated as trusted code: bugs here are security-critical.

### Renderer process (untrusted / sandboxed)

Responsibilities:

- Parsing/layout/paint and running author JavaScript.
- Calling into the network process via IPC proxies for:
  - `fetch()` / XHR / subresource loads
  - `WebSocket`
  - download requests initiated by user gestures
  - cookie access (`document.cookie`, when enabled)

Hard rule:

- **No direct network I/O in sandboxed builds.** When building a renderer intended to run in a
  sandboxed OS process, it must not open sockets or link in network stacks directly.
  - Enforced by Cargo features: disable `direct_network`/`direct_websocket` (see `Cargo.toml`) and
    provide IPC-backed proxies instead.
  - In code review: renderer-side code should not be reaching for `std::net`, `reqwest`, `ureq`,
    `tungstenite`, etc. unless it is behind one of the “direct_*” feature gates.

### Network process (sandboxed, network-only)

Responsibilities:

- Performing outbound network I/O (HTTP(S), WebSocket handshakes, streaming responses).
- Owning and enforcing the HTTP cookie store.
- Enforcing Fetch/CORS rules that must not be bypassable from the renderer.
- Providing a narrow “download” primitive that can be mediated by the browser process.

Security posture:

- Should be sandboxed with a **different** capability profile than the renderer:
  - Network syscalls allowed (outbound sockets).
  - File system access ideally restricted to:
    - *none* (preferred; browser writes downloads and persists cookie DB), or
    - a narrow allowlist (e.g. a dedicated download directory).

## IPC model overview

The renderer never receives a raw network handle. Instead it talks to a **proxy** object that:

1. Converts high-level API calls (Fetch/WebSocket/Download/Cookies) into IPC messages.
2. Validates/normalizes obvious footguns on the renderer side (bounds, string sizes, forbidden
   headers), so malformed messages are caught early.
3. Sends the request to the network process and waits for a structured reply (or streams events).

The network process:

1. Treats every incoming message as attacker-controlled input.
2. Re-validates constraints (never trust renderer-side validation).
3. Executes the operation using the in-process network stack.
4. Returns a response/event stream that is already policy-filtered (e.g. CORS, cookie filtering).

### Where the proxies plug in (code integration)

FastRender’s renderer is already structured to fetch external bytes through an abstraction:

- [`crate::resource::ResourceFetcher`](../src/resource.rs) is the primary “fetch bytes by URL” trait
  used by subresource loading and the Fetch/Web APIs adapter layer.

In multiprocess mode, the renderer process is configured with an IPC-backed implementation of this
trait during process startup. That IPC fetcher is then injected into the renderer via:

- `FastRender::builder().fetcher(...)` (library-style construction), or
- `FastRenderPoolConfig::with_fetcher(...)` / `FastRenderFactory` (browser/tab-style construction).

This keeps the majority of the renderer code agnostic to whether bytes came from an in-process HTTP
client or the network process.

### Quick start: using the network subprocess as a fetch backend

The minimal `network` process can already be used as a “fetch bytes for URL” service:

```rust,no_run
use fastrender::network_process::{spawn_network_process, NetworkProcessConfig};
use fastrender::FastRender;

let network = spawn_network_process(NetworkProcessConfig::default());
let fetcher = network.connect_client().resource_fetcher();

let mut renderer = FastRender::builder()
  .fetcher(fetcher)
  .build()?;
# Ok::<(), fastrender::Error>(())
```

This is useful for tests and for validating the process/IPC plumbing. As the multiprocess workstream
expands, the same pattern will apply: the renderer is always configured with a `ResourceFetcher`,
which may be IPC-backed.

### Surfaces exposed over IPC

At a high level, the network IPC surface is:

- **HTTP**: request/response with redirect handling, response header filtering, and body bytes.
- **Cookies**: get/set operations scoped to a requesting origin; HTTP-only cookies are never exposed
  to the renderer.
- **WebSocket**: connect/send/close plus an event stream for incoming frames and close/error events.
- **Downloads**: start/cancel/progress with explicit user-gesture mediation (browser-controlled).

Status note (repo reality): the *minimal* `network` subprocess currently only implements the HTTP
`Fetch { url }` round-trip. The other bullets above are the intended full surface and already have
in-tree protocol definitions, but may not yet be wired end-to-end.

## HTTP IPC (Fetch / subresources)

The HTTP IPC protocol exists to support multiple call sites:

- HTML/CSS subresource loading (images, fonts, stylesheets)
- JavaScript `fetch()` and XHR
- Browser chrome features that need network access (downloads, favicon fetch)

The request payload typically includes:

- URL (validated, length-limited)
- Method
- Request headers (validated and sanitized)
- Optional body (length-limited)
- Client context:
  - initiating origin (for CORS and cookie decisions)
  - destination / “fetch mode” (`navigate`, `image`, `font`, `fetch`, etc.)
  - credentials mode (`omit` / `same-origin` / `include`)
  - referrer and referrer policy
  - redirect mode (`follow` / `error` / `manual`)

The response payload typically includes:

- Final URL (after redirects)
- Status code
- Response headers (already filtered appropriately for CORS and internal-only headers)
- Body bytes (or a streamed body/event protocol for large responses)
- Structured network error information (timeout, DNS failure, TLS failure, etc.)

### Code: current prototype vs target protocol

There are currently multiple HTTP IPC shapes in-tree:

- **Prototype (`network` binary):** [`src/bin/network.rs`](../src/bin/network.rs) implements a tiny
  request set (`Hello { token, role }` / `Fetch { url }` / `DownloadStart { url }` / `Shutdown`) defined in
  [`src/network_process/ipc.rs`](../src/network_process/ipc.rs)
  (`fastrender::network_process::ipc`).
  - Transport: TCP on localhost, length-prefixed JSON (`u32_be` length).
    - Framing helpers: `write_request_frame` / `read_request_frame` and
      `write_response_frame` / `read_response_frame` (all enforce `MAX_{INBOUND,OUTBOUND}_FRAME_BYTES`
      **before allocating**).
    - The server rejects unknown JSON fields via `#[serde(deny_unknown_fields)]`.
  - Limitation: the protocol is intentionally minimal; it does not yet represent the full
    cookie/CORS/download semantics we eventually need in a network process.
- **Full `ResourceFetcher` proxy protocol:** [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs)
  defines `IpcRequest`/`IpcResponse` messages that cover the broader `ResourceFetcher` surface
  (including cookies and cache artifacts).
  - Detailed message-level documentation (framing, schema, chunked body transfer, limits) lives in:
    [`docs/ipc/network_fetch_protocol.md`](ipc/network_fetch_protocol.md).
- **Bounded binary network protocol (long-term):** [`src/net/transport.rs`](../src/net/transport.rs)
  defines a request/response/events protocol with explicit per-field limits and streaming event
  types for WebSockets and downloads.

### Forbidden headers and “policy lives in the network process”

The renderer must not be able to smuggle privileged headers via IPC. The network process must
enforce (at minimum):

- Renderer-supplied `Cookie` / `Set-Cookie` are ignored or rejected.
- Renderer-supplied `Host`, `Origin`, `Referer`, `Sec-Fetch-*`, and other security-relevant headers
  are either forbidden or overwritten with canonical values derived from the request context.

FastRender’s in-process fetch stack already has helper logic for this:

- Request header filtering is implemented in [`src/resource.rs`](../src/resource.rs) (see
  `fetch_http_request_header_forbidden` and the request header merge logic).

In multiprocess mode, the network process must be the *final* authority.

## CORS enforcement (network process)

**CORS must be enforced in the network process**, not in the renderer.

Reason: if the renderer can see the raw response, a compromised renderer can simply ignore CORS and
leak cross-origin data.

FastRender enforces CORS in two related places:

1. Subresource CORS checks (e.g. `Access-Control-Allow-Origin` required for cross-origin web fonts
   and `<img crossorigin>` images).
2. Fetch API / XHR CORS behavior (including preflights and response header filtering).

Implementation notes:

- Subresource ACAO validation lives in [`src/resource/cors.rs`](../src/resource/cors.rs) (see
  `validate_cors_allow_origin`).
- Fetch/XHR CORS behavior is implemented in the Web Fetch adapter
  [`src/resource/web_fetch/adapter.rs`](../src/resource/web_fetch/adapter.rs) and relies on the
  underlying resource fetcher for the actual HTTP bytes.

Status note: today, much of this logic still lives in the in-process resource stack. As the network
process is wired up, any CORS checks that gate what bytes/headers are visible to untrusted code must
move to (or be duplicated in) the network process so a compromised renderer cannot bypass them.

Runtime toggle:

- `FASTR_FETCH_ENFORCE_CORS=0|false|no|off` disables CORS enforcement (default is enabled). See
  [env-vars.md](env-vars.md).

## Cookie jar ownership, sharing, and persistence

### Ownership

Target design: the HTTP cookie store (RFC 6265-ish) is owned by the **network process**. The renderer
must not:

- Read raw cookies for unrelated origins.
- Read `HttpOnly` cookies.
- Observe `Set-Cookie` headers directly.

The network process:

- Attaches cookies to outgoing requests based on the request URL + credentials mode.
- Applies `Set-Cookie` responses to its store.
- Exposes a **scoped cookie API** to the renderer for `document.cookie` (if/when enabled).

Repo reality (today):

- Cookie plumbing is still largely in-process:
  - HTTP cookie attachment is implemented by `HttpFetcher` in [`src/resource.rs`](../src/resource.rs)
    (it uses a `reqwest::cookie::Jar` internally when `direct_network` is enabled).
  - The JS `document.cookie` surface has an MVP in-memory store in
    [`src/js/cookie_jar.rs`](../src/js/cookie_jar.rs).
- The `ResourceFetcher` trait already has explicit cookie hooks (`cookie_header_value` and
  `store_cookie_from_document`) which are the intended seam for moving cookie state out-of-process.
  The corresponding IPC message types exist in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs).

### Sharing

Cookies are shared across tabs the same way real browsers do: they are global (within a profile)
state, not per-tab state.

In practice this means:

- A renderer process does **not** have its own cookie jar.
- Multiple renderers talking to the same network process observe a shared cookie store.

### Persistence

Cookie persistence is a browser responsibility: it is profile state and must not be writable by the
renderer.

The exact on-disk persistence mechanism may evolve, but the intended invariant is:

- Renderer cannot write arbitrary files.
- Network process should not be able to write arbitrary files.
- The browser process mediates persistence (load on startup; save on shutdown / periodically).

If you are modifying cookie persistence, treat it as a security boundary change: update this doc and
add tests that assert renderer isolation.

## WebSocket IPC

WebSockets are stateful, long-lived network connections. In multiprocess mode:

- The renderer requests `WebSocket` creation over IPC.
- The network process performs the handshake and owns the socket.
- Incoming frames are delivered to the renderer via IPC “events”.
- Outgoing frames are sent to the network process via IPC “commands”.

Design constraints:

- The renderer must not be able to bypass origin/policy checks by constructing a raw socket.
- The network process must apply appropriate bounds (frame sizes, message queue limits) and should
  surface backpressure to avoid unbounded memory growth.
- The network process must enforce **hard caps** on concurrently-active connections so a compromised
  renderer cannot exhaust network-process resources by opening unbounded WebSockets.
  - See `crates/fastrender-ipc/src/lib.rs` (`NetworkWebSocketManagerLimits`).
  - Default limits: **256 active WebSockets per renderer**, and **4096 total** across all renderers.
  - Over-limit `Connect` is rejected deterministically (`Error` + `Close`) without spawning per-socket
    work.

Code pointers:

- JS bindings live in [`src/js/vmjs/window_websocket.rs`](../src/js/vmjs/window_websocket.rs).
  - In-process mode (default) is gated by the Cargo feature `direct_websocket` (links `tungstenite`).
  - There is also an IPC-backed install path (`install_window_websocket_ipc_bindings`) that allows a
    host embedding to route WebSocket commands/events across a process boundary.
- IPC message schema + validation helpers (intended for renderer→network hardening):
  [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) and the renderer↔network envelope
  [`src/ipc/network.rs`](../src/ipc/network.rs).
- Network-process resource caps (defense in depth):
  [`src/network_process/websocket_manager.rs`](../src/network_process/websocket_manager.rs).

## Downloads IPC

Downloads are special because they combine:

- network I/O
- file system writes
- user intent (should be gated by a user gesture / browser UI policy)

The browser process is the right place to enforce “is this allowed?” decisions (prompting, download
directory choice, safe filename handling, etc.), but the network bytes still come from the network
process.

The typical flow is:

1. Renderer requests a download (usually from a user gesture like “Save link as…”).
2. Browser process decides:
   - whether the download is allowed
   - where it will be written
   - what the final filename should be
3. Network process performs the HTTP transfer and reports progress.
4. Browser process writes to disk (or passes a restricted handle/path to the network process, if the
   sandbox model permits it).

The desktop browser UI already has filename/path helpers (even in single-process mode) in
[`src/ui/downloads.rs`](../src/ui/downloads.rs); in multiprocess mode these same constraints should
apply.

Repo reality / code pointers:

- The windowed browser’s current download implementation runs in the browser worker thread:
  - Messages: `UiToWorker::StartDownload` / `CancelDownload` in [`src/ui/messages.rs`](../src/ui/messages.rs)
  - Implementation: [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)
  - Filename/path helpers: [`src/ui/downloads.rs`](../src/ui/downloads.rs)
- The long-term multiprocess design expects the network process to stream download body chunks over
  IPC and the browser to write them to disk. The event type for this already exists in
  [`src/net/transport.rs`](../src/net/transport.rs) (`NetworkEvent::DownloadChunk`).

## Debugging tips

### Network logging / compatibility

Many fetch-debug knobs are already exposed as runtime environment variables (see
[env-vars.md](env-vars.md)). The most relevant for diagnosing issues in the network process are:

- `FASTR_HTTP_BACKEND=auto|ureq|reqwest|curl`
- `FASTR_HTTP_LOG_RETRIES=1`
- `FASTR_HTTP_BROWSER_HEADERS=0|1`
- `FASTR_FETCH_ENFORCE_CORS=0|1`

Because the network process is a separate OS process, make sure you’re looking at the right logs:

- In development, run the browser from a terminal and capture stdout/stderr (the network process
  should inherit the parent’s stdio unless explicitly redirected).
- Enable `RUST_BACKTRACE=1` when diagnosing crashes.
- The `NetworkProcessConfig` used by `spawn_network_process` has an `inherit_stderr` knob; see
  [`src/network_process/client.rs`](../src/network_process/client.rs).

### Feature gates (ensuring the renderer has no direct network access)

When validating “renderer cannot do direct network I/O” invariants, use the feature gates:

- `direct_network`: in-process HTTP stack (`reqwest`/`ureq`)
- `direct_websocket`: in-process WebSocket stack (`tungstenite`)
- `direct_filesystem`: in-process `file://` fetch support

CI uses the feature set `renderer_minimal` to ensure a renderer build can link without any in-process
network stacks.

### Build/run reminders

Do not run raw `cargo` in docs/examples. Use the repo wrappers:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh build --features browser_ui
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

## Adding a new network capability (checklist)

When adding any new network-exposed feature:

1. **Decide the owner**: should this run in the network process, or is it browser-only state?
2. **Add an IPC message** with explicit bounds (max URL bytes, max header bytes, max body bytes,
   timeouts).
3. **Validate twice**: renderer-side for ergonomics, network-side for security.
4. **Treat headers/cookies as privileged**: never let the renderer set security-relevant headers.
5. **Write tests**:
   - one test for the happy path
   - one test asserting a policy boundary (CORS/cookies/forbidden headers) can’t be bypassed
