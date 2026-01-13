# IPC transport (framing, FD passing, shared memory) — security invariants

This document is **developer-facing** and **normative**: it describes the hard constraints the
FastRender multiprocess IPC layer must preserve.

If you change IPC code, **re-read this doc** and make sure you are not weakening:

- bounded message sizes (no unbounded allocations)
- strict file-descriptor (FD) association (no “FD confusion”)
- strict shared-memory validation (no SIGBUS / OOM footguns)

> Threat model reminder: the renderer process and the network process can be **malicious** (compromised
> by web content, bugs, or deliberate fuzzing). The browser process is the security boundary and must
> treat IPC bytes + FDs as hostile inputs.

Related:
- Linux FD-passing + shared-memory checklist: [`ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md)
- Multiprocess threat model (trust boundary statement): [`multiprocess_threat_model.md`](multiprocess_threat_model.md)
- Network process overview + IPC surface: [`network_process.md`](network_process.md)
- Renderer sandbox entrypoint (all platforms): [`renderer_sandbox.md`](renderer_sandbox.md)
- Renderer sandboxing overview (platform notes, seccomp/AppContainer/etc): [`sandboxing.md`](sandboxing.md)
- Linux renderer sandbox deep dive (rlimits/fd hygiene/namespaces/Landlock/seccomp): [`security/sandbox.md`](security/sandbox.md)
- Linux renderer seccomp allowlist workflow: [`seccomp_allowlist.md`](seccomp_allowlist.md)

If you add/modify IPC code, treat this as a *non-optional* checklist:

- Preserve/introduce **hard caps** (frame bytes, decode bytes, per-field bytes, SHM bytes).
  - If you add a new cap constant, add it to the “Current hard caps” / “Additional size limits”
    tables below.
- Define **FD arity** per message (`expected_fds()`), and enforce it at receive sites.
  - Payload bytes + attached FDs must be sent/received atomically (single `sendmsg`/`recvmsg`).
- For stream transports, enforce the frame cap **before allocating** and reject zero-length frames.
- Decode must be **bounded** and must **consume the entire frame** (reject trailing bytes).
- For JSON IPC, prefer `#[serde(deny_unknown_fields)]` on protocol types so unknown fields fail closed.
- For shared memory, validate FD type/size/seals before `mmap` (see `src/ipc/validate.rs`).

---

## Process model

Target model (see [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md)):

- **Browser process (trusted)**:
  - owns UI / window management
  - owns persistent user state (profile, cookies, history, bookmarks, …)
  - spawns and supervises children
  - is the only process allowed to make “privileged” decisions

- **Renderer process (untrusted)**:
  - parses/executes untrusted HTML/CSS/JS
  - produces pixels (usually via shared memory)
  - must be sandboxed; assume it may send arbitrary malformed messages

- **Network process (untrusted-ish / less-trusted than browser)**:
  - performs network I/O on behalf of browser/renderer
  - should be sandboxed separately from both
  - must be treated as malicious for IPC purposes as well

IPC links (each is a distinct connection):

- **browser ↔ renderer**: navigation + input + frame submission
- **browser ↔ network**: fetch requests/responses, DNS, cookie mediation
- **renderer ↔ network**: fetch/WebSocket proxying, event streams

The browser must be able to kill/restart renderer/network processes without risking browser memory
corruption or unbounded resource consumption.

---

## Transport (repo reality + target)

### Repo reality (today)

FastRender currently has **multiple IPC-like channels**, and they are not all using the same
transport/codec yet:

- **Browser ↔ renderer (development / test harness):**
  - `crates/fastrender-renderer` uses a *byte-stream* transport over stdio (pipes), framed as
    `u32_le length` + payload, serialized with `bincode`.
  - See: [`crates/fastrender-renderer/src/main.rs`](../crates/fastrender-renderer/src/main.rs) and
    the size cap [`crates/fastrender-ipc/src/lib.rs`](../crates/fastrender-ipc/src/lib.rs).
- **Browser ↔ renderer (in-tree multiprocess IPC, under active development):**
  - Framing + bounded `bincode` helpers live in [`src/ipc/framing.rs`](../src/ipc/framing.rs).
  - The intended browser↔renderer *schema* lives in
    [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) and is explicitly designed to:
    - keep control messages small (separate `bincode` decode limit),
    - carry large pixel buffers out-of-band as FD attachments (`SCM_RIGHTS`).
  - Linux SHM primitives live in [`src/ipc/shm.rs`](../src/ipc/shm.rs) (`memfd_create` + `mmap` +
    best-effort seals).
- **Renderer ↔ network (today / legacy JSON ResourceFetcher proxy):**
  - `src/resource/ipc_fetcher.rs` implements `IpcResourceFetcher`, a renderer-side `ResourceFetcher`
    proxy that connects to a local “network process” endpoint over `TcpStream`, framed as
    `u32_le length` + payload, serialized with JSON (`serde_json`).
  - It performs an auth-token handshake (`IPC_AUTH_TOKEN_ENV`) and enforces per-direction frame caps
    (`IPC_MAX_INBOUND_FRAME_BYTES`, `IPC_MAX_OUTBOUND_FRAME_BYTES`) plus per-field limits.
  - See: [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs).
- **Browser ↔ network (prototype `network` subprocess):**
  - The `network` binary ([`src/bin/network.rs`](../src/bin/network.rs)) uses JSON framing helpers in
    [`src/network_process/ipc.rs`](../src/network_process/ipc.rs) (`u32_be length` + JSON).
  - It begins with a `Hello { token }` handshake and enforces:
    - per-direction frame caps (`MAX_INBOUND_FRAME_BYTES`, `MAX_OUTBOUND_FRAME_BYTES`) **before**
      allocating, and
    - per-field caps like `MAX_URL_BYTES` / `MAX_AUTH_TOKEN_BYTES`.
  - Protocol structs are `#[serde(deny_unknown_fields)]` so unknown fields fail closed.
- **Browser ↔ network (in-tree protocol schema, under development):**
  - The intended browser↔network message types live in
    [`src/ipc/protocol/network.rs`](../src/ipc/protocol/network.rs) (with validation helpers and
    `expected_fds()` for future FD-backed body transfer).
- **Shared framing helper (in-tree):**
  - `src/ipc/framing.rs` provides a length-prefixed framing layer with a hard maximum frame size.
  - See: [`src/ipc/framing.rs`](../src/ipc/framing.rs).
- **In-tree JSON IPC connection helper (bounded):**
  - [`src/ipc/connection.rs`](../src/ipc/connection.rs) provides `IpcConnection`, a wrapper that
    sends/receives **length-prefixed** JSON messages on top of `src/ipc/framing.rs` while enforcing
    `MAX_IPC_MESSAGE_BYTES` on both send and receive.
  - Security note: `serde` ignores unknown struct fields by default. IPC protocol structs intended
    for a trust boundary should use `#[serde(deny_unknown_fields)]` (including the **top-level**
    message types, not just nested structs) and should have regression tests asserting unknown fields
    are rejected (see `src/ipc/connection.rs`).
- **Network-process IPC (in-tree, under active development):**
  - `src/net/transport.rs` defines a binary protocol with explicit per-field limits and uses
    `src/ipc/framing.rs` for bounded framing.
  - See: [`src/net/transport.rs`](../src/net/transport.rs).

All of the above are **stream transports**: message boundaries are *not* preserved by the kernel, so
framing must be explicit and allocation-bounded.

Also relevant: [`src/ipc/bootstrap.rs`](../src/ipc/bootstrap.rs) provides a Unix-domain `socketpair()`
bootstrap helper for spawning child processes with an **already-connected** IPC socket. It prefers
`SOCK_SEQPACKET` (and falls back to `SOCK_STREAM` where needed) and is careful about `CLOEXEC` so the
parent does not leak IPC sockets into unrelated `exec()` calls.

### Target transport for FD passing: Unix domain sockets

For the “real” sandboxed multiprocess architecture we ultimately need:

- **low overhead** message passing (high-frequency events + acks)
- **FD passing** (`SCM_RIGHTS`) for shared memory (`memfd`) and other capability-style handles
- **backpressure** (if a child misbehaves, the browser must not buffer unbounded data)

Unix domain sockets are the simplest primitive that provides all of the above.

### `SOCK_SEQPACKET` vs `SOCK_STREAM` (for the real multiprocess transport)

**Preferred**: `AF_UNIX` + `SOCK_SEQPACKET` (e.g. created via `socketpair`).

Why `SOCK_SEQPACKET`:

- **Message boundaries are preserved** by the kernel (one send = one receive).
  - This removes an entire class of framing bugs where stream reads coalesce/split messages.
- **Ancillary data (FDs via `SCM_RIGHTS`) stays attached to the packet**.
  - This is critical: it prevents “FD confusion” where a malicious peer tries to make the receiver
    associate an FD with the wrong logical message.
- Receivers can detect oversize packets via `MSG_TRUNC` and treat it as a protocol violation
  without attempting to allocate a giant buffer.

If a platform can’t use `SOCK_SEQPACKET`, a `SOCK_STREAM` fallback is allowed, **but** it must use
the explicit framing rules in the next section and must continue to enforce the same size/FD limits.

Repo reality:

- [`src/ipc/unix_seqpacket.rs`](../src/ipc/unix_seqpacket.rs) contains a Linux-only `UnixSeqpacket`
  wrapper that sends/receives one **atomic** seqpacket message at a time with optional `SCM_RIGHTS`
  FD attachments. It demonstrates robust hardening patterns (`MSG_TRUNC`/`MSG_CTRUNC` handling,
  `MSG_CMSG_CLOEXEC`, `MSG_NOSIGNAL`, strict FD count enforcement).
- [`src/ipc/frame_slots.rs`](../src/ipc/frame_slots.rs) demonstrates a “browser allocates SHM slots
  once; subsequent messages are control-only” design and is another good source of hardening
  patterns (exact message lengths, FD count checks, truncation handling).
- [`src/ipc/bootstrap.rs`](../src/ipc/bootstrap.rs) contains an exec-safe “parent creates socketpair,
  child inherits FD 3” helper designed to avoid CLOEXEC races in multithreaded parents.

---

## Message framing + max message size (hard limit)

**Security invariant:** the browser must never allocate based on untrusted length fields without a
hard cap.

### Stream framing (current): 4-byte length prefix (`u32`) + payload

Most current stream-based IPC channels use a simple frame format:

```
u32 length
[length bytes payload]
```

Endianness is part of the per-channel contract. Prefer `u32_le` for new code (and reuse
`src/ipc/framing.rs`), but note that some legacy code uses `u32_be` (e.g. `src/network_process/ipc.rs`).

Here, `MAX_MESSAGE_BYTES` is the per-channel hard cap (see table below).

Receiver rules (mandatory):

1. Read exactly 4 bytes (handling partial reads).
2. Parse `length`.
3. Reject `length == 0`.
4. Reject `length > MAX_MESSAGE_BYTES` (**before** allocating).
5. Read exactly `length` bytes (or use a bounded reader like `Read::take(length)`).

### Current hard caps (repo reality)

These caps are **security limits**; do not increase casually. Keep the sender + receiver consistent
(same constant, same meaning).

| Channel | Max payload bytes | Where enforced |
|---|---:|---|
| Browser ↔ renderer (stdio + bincode, dev) | 64 MiB | `fastrender_ipc::MAX_IPC_MESSAGE_BYTES` in [`crates/fastrender-ipc/src/lib.rs`](../crates/fastrender-ipc/src/lib.rs) (checked in [`crates/fastrender-renderer/src/main.rs`](../crates/fastrender-renderer/src/main.rs)) |
| Generic framing helper (`read_frame`/`write_frame`) | 8 MiB | `crate::ipc::MAX_IPC_MESSAGE_BYTES` in [`src/ipc/limits.rs`](../src/ipc/limits.rs) (enforced by [`src/ipc/framing.rs`](../src/ipc/framing.rs)) |
| Renderer ↔ network (`IpcResourceFetcher`, JSON over TCP) | 8 MiB inbound / 80 MiB outbound | `IPC_MAX_INBOUND_FRAME_BYTES` / `IPC_MAX_OUTBOUND_FRAME_BYTES` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |
| Browser ↔ network (`network` subprocess prototype, JSON over TCP) | 8 MiB inbound / 80 MiB outbound | `MAX_INBOUND_FRAME_BYTES` / `MAX_OUTBOUND_FRAME_BYTES` in [`src/network_process/ipc.rs`](../src/network_process/ipc.rs) |

Repo reality note: both `src/network_process/ipc.rs` and `src/resource/ipc_fetcher.rs` implement
length-prefixed JSON transports and enforce their frame caps **before allocating**. Treat these caps
as security limits; do not remove the pre-allocation checks.

Important: the 64 MiB browser↔renderer cap is intentionally large enough to carry early-development
pixel buffers inline (see the comment in `crates/fastrender-ipc`). **Long-term, frame transfers
should move to shared memory**, but the hard cap must remain enforced either way.

Additional (important) size limits that sit *on top* of framing:

| Purpose | Limit | Where enforced |
|---|---:|---|
| Browser↔renderer control-message decode budget | 256 KiB | `RENDERER_IPC_DECODE_LIMIT_BYTES` in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) (`bincode_options().with_limit(...)`) |
| Renderer IPC URL string max | 8 KiB | `MAX_URL_BYTES` / `UrlString` (`BoundedString`) in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |
| Untrusted URL string max (stdio prototype) | 8 KiB | `MAX_UNTRUSTED_URL_BYTES` (alias `MAX_SITE_KEY_URL_BYTES`) in [`crates/fastrender-ipc/src/lib.rs`](../crates/fastrender-ipc/src/lib.rs) |
| File input / drag-and-drop max files per message (browser→renderer) | 16 | `FILE_INPUT_MAX_FILES` in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |
| File input / drag-and-drop file name bytes (browser→renderer) | 256 bytes | `FILE_INPUT_MAX_NAME_BYTES` in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |
| File input / drag-and-drop total file size metadata (browser→renderer) | 512 MiB | `FILE_INPUT_MAX_TOTAL_BYTES_META` in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |
| Renderer↔network URL string max (`IpcResourceFetcher`) | 1 MiB | `IPC_MAX_URL_BYTES` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |
| Renderer↔network header count max (`IpcResourceFetcher`) | 1024 | `IPC_MAX_HEADER_COUNT` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |
| Renderer↔network header name bytes (`IpcResourceFetcher`) | 1024 bytes | `IPC_MAX_HEADER_NAME_BYTES` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |
| Renderer↔network header value bytes (`IpcResourceFetcher`) | 16 KiB | `IPC_MAX_HEADER_VALUE_BYTES` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |
| Renderer↔network auth token bytes (`IpcResourceFetcher`) | 1024 bytes | `IPC_MAX_AUTH_TOKEN_BYTES` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |
| Browser↔network URL string max (network subprocess prototype) | 1 MiB | `MAX_URL_BYTES` in [`src/network_process/ipc.rs`](../src/network_process/ipc.rs) |
| Browser↔network auth token bytes (network subprocess prototype) | 1024 bytes | `MAX_AUTH_TOKEN_BYTES` in [`src/network_process/ipc.rs`](../src/network_process/ipc.rs) |
| Browser↔network URL string max (in-tree protocol schema) | 1 MiB | `MAX_URL_BYTES` in [`src/ipc/protocol/network.rs`](../src/ipc/protocol/network.rs) |
| Browser↔network cookie string max (in-tree protocol schema) | 4 KiB | `MAX_COOKIE_STRING_BYTES` in [`src/ipc/protocol/network.rs`](../src/ipc/protocol/network.rs) |
| Linux shared memory hard ceiling | 256 MiB | `MAX_SHM_SIZE` in [`src/ipc/shm.rs`](../src/ipc/shm.rs) |
| Default max un-acked frames in flight (renderer-side flow control) | 2 | `DEFAULT_MAX_FRAMES_IN_FLIGHT` in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |
| Browser-side compositor max subframes per frame (stdio prototype) | 256 | `MAX_SUBFRAMES_PER_FRAME` in [`crates/fastrender-ipc/src/lib.rs`](../crates/fastrender-ipc/src/lib.rs) |
| Browser-side compositor max clip items per subframe (stdio prototype) | 64 | `MAX_SUBFRAME_CLIP_STACK_DEPTH` in [`crates/fastrender-ipc/src/lib.rs`](../crates/fastrender-ipc/src/lib.rs) |
| WebSocket URL bytes (renderer→network) | 8 KiB | `MAX_WEBSOCKET_URL_BYTES` in [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) |
| WebSocket protocol count (renderer→network) | 32 | `MAX_WEBSOCKET_PROTOCOLS` in [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) |
| WebSocket protocol string bytes (renderer→network) | 1 KiB | `MAX_WEBSOCKET_PROTOCOL_BYTES` in [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) |
| WebSocket message payload (renderer→network) | 4 MiB | `MAX_WEBSOCKET_MESSAGE_BYTES` in [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) |
| WebSocket close reason bytes (renderer→network) | 123 bytes | `MAX_WEBSOCKET_CLOSE_REASON_BYTES` in [`src/ipc/websocket.rs`](../src/ipc/websocket.rs) |
| WebSocket concurrent connections (per renderer, network-process side) | 256 | `WebSocketManagerLimits::max_active_per_renderer` in [`src/network_process/websocket_manager.rs`](../src/network_process/websocket_manager.rs) |
| WebSocket concurrent connections (global, network-process side) | 4096 | `WebSocketManagerLimits::max_active_total` in [`src/network_process/websocket_manager.rs`](../src/network_process/websocket_manager.rs) |
| WebSocket bufferedAmount cap (per connection, network-process side) | 16 MiB | `MAX_WEBSOCKET_BUFFERED_AMOUNT_BYTES` in [`src/network_process/websocket_manager.rs`](../src/network_process/websocket_manager.rs) |

Network IPC (renderer↔network process) also enforces per-field caps via
`NetworkMessageLimits` in [`src/net/transport.rs`](../src/net/transport.rs) (defaults today):

- `max_url_bytes`: 1 MiB
- `max_header_count`: 1024
- `max_total_header_bytes`: 256 KiB
- `max_request_body_bytes`: 10 MiB
- `max_response_body_bytes`: 50 MiB
- `max_event_bytes`: 4 MiB

Important: these are **semantic** limits; the outer `MAX_IPC_MESSAGE_BYTES` framing cap still applies
(8 MiB today). If a field-level limit exceeds the frame cap, the data must be chunked or moved to
shared memory; do not “just raise the frame cap” without security review.

Prefer enforcing this invariant mechanically. Repo reality example:
`src/ipc/protocol/network.rs` has a compile-time guard that panics if `MAX_URL_BYTES` /
`MAX_COOKIE_STRING_BYTES` exceed `crate::ipc::MAX_IPC_MESSAGE_BYTES`.

### Framing with `SOCK_SEQPACKET` (target)

With `SOCK_SEQPACKET` the kernel already gives us frames:

- **one packet = one message**
- receiver calls `recvmsg` with:
  - a fixed-size data buffer of `MAX_MESSAGE_BYTES` (the per-connection hard cap)
  - a fixed-size control buffer sized for `MAX_FDS_PER_MESSAGE`

Receiver rules:

- If the received length is `> MAX_MESSAGE_BYTES`, treat as **protocol violation**.
  - In practice you detect this via `MSG_TRUNC` when the provided buffer is too small.
- If `MSG_TRUNC` is set: the sender attempted an oversize message → **close the connection**.
- If `MSG_CTRUNC` is set: ancillary data was truncated → **close the connection** (FDs may be lost or
  mismatched).

### Framing with `SOCK_STREAM` (fallback)

If using `SOCK_STREAM`, messages must be framed as:

```
u32_le length
[length bytes payload]
```

(`u32_le` is the preferred on-wire format for new FastRender IPC stream framing; keep it consistent
with `src/ipc/framing.rs` unless you have a strong reason to diverge.)

Receiver rules:

1. Read exactly 4 bytes (handling partial reads).
2. Parse `length`.
3. If `length > MAX_MESSAGE_BYTES`, treat as protocol violation → close.
4. Read exactly `length` bytes into a bounded buffer.

**Never** use `read_to_end`, `Vec::reserve(length)` without a cap, or any decode API that can grow a
buffer based on peer-controlled sizes.

---

## Serialization format + bincode decode limit

**Serialization:** IPC payloads are binary, serde-serializable structs/enums.

**Security invariant:** decode must not cause unbounded allocation, even if the peer lies about
vector/string lengths.

Rules:

- Use `bincode` (or an equivalent bounded binary codec) **only** with a decode limit:
  - The limit must be `<= MAX_MESSAGE_BYTES` for the relevant channel (e.g.
    `fastrender_ipc::MAX_IPC_MESSAGE_BYTES`).
- Reject trailing bytes after decode (decoder must consume the entire frame payload). Trailing bytes
  indicate either:
  - protocol desynchronization (wrong framing/length), or
  - an attempted “smuggling” ambiguity (multiple logical messages in one frame).
- Treat any decode error as a **protocol violation** (malformed input from an untrusted peer).
  - The safe default is to close the connection and tear down the child process.

Rationale:

- With serde formats, a malicious peer can encode “a vector of length 10^12” and trigger allocation
  unless the decoder enforces a hard cap.

Repo reality:

- The stdio renderer transport does the right thing today:
  - validates `len <= fastrender_ipc::MAX_IPC_MESSAGE_BYTES`
  - decodes with `bincode::DefaultOptions::new().with_limit(len as u64)`
  - reads from a bounded reader (`Read::take(len as u64)`)
  - rejects trailing bytes after decode
  - See: [`crates/fastrender-renderer/src/main.rs`](../crates/fastrender-renderer/src/main.rs).
- The in-tree framing helper also enforces a hard `bincode` size limit (8 MiB) and has regression
  tests that must remain:
  - See: [`src/ipc/framing.rs`](../src/ipc/framing.rs).
- The browser↔renderer protocol intentionally enforces a **smaller** decode limit for control
  messages (256 KiB), independent of the outer frame length:
  - See: `RENDERER_IPC_DECODE_LIMIT_BYTES` + `bincode_options()` in
    [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs).
- Some IPC surfaces use `serde_json` rather than `bincode` (e.g. `src/resource/ipc_fetcher.rs`,
  `src/ipc/connection.rs`).
  - The same rule still applies: enforce the hard frame cap **before** deserializing.
  - Prefer `#[serde(deny_unknown_fields)]` on protocol structs/enums at a security boundary so
    unexpected fields are rejected rather than silently ignored (see the note + regression test in
    [`src/ipc/connection.rs`](../src/ipc/connection.rs)).

---

## FD passing rules (`SCM_RIGHTS`)

**FDs are capabilities.** Passing an FD is equivalent to granting the receiver access to a resource.

For Linux-specific pitfalls and a lower-level checklist, also see:
[ipc_linux_fd_passing.md](ipc_linux_fd_passing.md).

Security invariants:

1. **Only `SCM_RIGHTS` is allowed.** Reject other ancillary message types.
2. **Bound the count:** accept at most `MAX_FDS_PER_MESSAGE` per message.
   - For most message types this should be **0 or 1**.
   - Some message types legitimately need more:
     - Browser→renderer file-input messages (`DropFiles`, `FilePickerChoose`) attach `files.len()` FDs
       and are bounded by `FILE_INPUT_MAX_FILES = 16` in
       [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs).
   - Recommended global cap: **≤ 16** (the current maximum required by in-tree protocols). Keep this
     value small and revisit with security review if a new message type needs more.
   - Repo reality: `src/ipc/fd_passing.rs` provides a defensive `recv_msg(sock_fd, max_fds)` helper
     that enforces `max_fds` (and has an internal absolute ceiling). Keep `max_fds` small at call
     sites.
3. **Message types must define the FD arity.**
   - For a given message type, the receiver knows exactly how many FDs to expect.
     - Usually this is a fixed small number (0 or 1).
     - Variable-arity messages are allowed only when the arity is derived from **validated** fields
       in the decoded message (e.g. `files.len()` with a hard cap).
   - Repo reality:
     - The browser↔renderer protocol encodes this explicitly via `expected_fds()`:
       [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs).
     - The browser↔network protocol schema also uses `expected_fds()` for the same reason:
       [`src/ipc/protocol/network.rs`](../src/ipc/protocol/network.rs).
   - **Do not send the FD “out of band” in a separate write.** For messages like
     `RendererToBrowser::FrameReady`, the metadata and its FD must be sent in the *same* `sendmsg`
     so the receiver cannot accidentally associate the FD with the wrong message.
4. **Close unexpected FDs immediately.**
   - If a message arrives with too many FDs, close *all* of them (including the “expected” ones) and
     treat it as a protocol violation. This avoids subtle “keep the first N” bugs that let a
     malicious peer smuggle in extra capabilities.
5. **No FD leaks.**
   - After parsing a message, every received FD must be either:
     - stored in a clearly-owned structure, or
     - closed before returning from the receive handler.
6. **CLOEXEC everywhere.**
   - Create sockets with `SOCK_CLOEXEC` where possible.
   - Prefer receiving FDs with `recvmsg(MSG_CMSG_CLOEXEC)` so `FD_CLOEXEC` is applied **atomically**.
     - Do not rely on a follow-up `fcntl(FD_CLOEXEC)` in another step (TOCTOU footgun).
   - Repo reality:
     - `src/ipc/ancillary.rs::recv_fd` uses `MSG_CMSG_CLOEXEC` on Linux for single-FD transfers.
     - `src/ipc/frame_slots.rs` uses `MSG_CMSG_CLOEXEC` for seqpacket messages with FD sets.
     - `src/ipc/fd_passing.rs` uses `MSG_CMSG_CLOEXEC` on Linux/Android and sets `FD_CLOEXEC`
       best-effort on other Unix platforms.
7. **Include at least one byte of real payload data when sending FDs.**
    - On Linux, `SCM_RIGHTS` control messages are associated with a received datagram/packet. Sending
      “FD-only” control messages without accompanying payload bytes is a well-known footgun; always
      include at least one byte of non-ancillary data.
    - Receiver rule: if a message arrives with one or more FDs but **zero** payload bytes, treat it
      as a protocol violation and close the connection (this likely indicates a sender bug).
    - See also: [ipc_linux_fd_passing.md](ipc_linux_fd_passing.md).

Why this matters:

- A malicious renderer can try to exhaust the browser’s FD table by spamming `SCM_RIGHTS`.
- A malicious renderer can try to exploit “FD confusion” (attaching FDs to unexpected message
  boundaries) to trick the browser into treating an FD as a different resource than intended.

---

## Shared memory rules (`memfd`)

Long-term, large IPC payloads (pixels, blobs, network bodies, etc.) should not be transferred inline.
Use shared memory and pass the `memfd` via `SCM_RIGHTS`.

### Repo reality (today): two SHM backends exist

#### A) Tempfile-backed mappings (cross-platform, but not sandbox-friendly)

The in-tree multiprocess frame-buffer pool currently uses **temporary files + `memmap2`** as a
cross-platform shared-memory stand-in:

- Browser creates/sizes/maps temp files and sends descriptors to the renderer:
  [`src/ipc/frame_pool.rs`](../src/ipc/frame_pool.rs)
- Renderer opens/maps the same paths and validates mapping size matches the descriptor.

The pool is intentionally **small** (double/triple buffering). The browser must explicitly
**ack/release** each submitted frame once it has either:

- uploaded/copied the pixels out of the shared buffer (e.g. into a GPU texture), or
- decided to drop the frame as stale.

Without an ack, the renderer must treat the buffer as **in use** and must not overwrite it. This is
both a correctness rule (avoid tearing / use-after-free) and a flow-control rule (bounded buffering
prevents the renderer from blocking indefinitely when the browser/UI is slow).

See also: `RendererToBrowser::FrameReady { frame_seq, .. }` ↔ `BrowserToRenderer::FrameAck { frame_seq }`
in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs).
The protocol module also provides a small renderer-side helper (`FrameInFlightCounter`) with a
conservative default limit `DEFAULT_MAX_FRAMES_IN_FLIGHT = 2`.

Security invariants that must remain true (even before we migrate to `memfd`):

- **Browser allocates shared buffers**; renderer should not be able to cause the browser to accept
  arbitrary files.
- **Bounded buffer count:** the number of frame buffers is capped (`MAX_FRAME_BUFFERS = 8` in
  [`src/ipc/frame_pool.rs`](../src/ipc/frame_pool.rs)). Keep this low so a compromised renderer
  cannot induce unbounded mappings/FD/path state even if it can trigger pool recreation.
- **Exact-size mapping:** after mapping, the receiver validates `mmap.len() == expected_len` and
  treats mismatches as protocol violations.
- **Size caps:** frame buffer size is capped by pixmap policy (`MAX_PIXMAP_BYTES` in
  [`src/paint/pixmap.rs`](../src/paint/pixmap.rs)) and by pool sizing logic.
  - Do not remove these caps; they prevent pathological allocations from a malicious peer.

This is *not* the final design: a sandboxed renderer should not rely on arbitrary filesystem access.
On Linux we expect to move to `memfd` + FD passing (see next section and
[ipc_linux_fd_passing.md](ipc_linux_fd_passing.md)).

#### B) Linux `memfd` shared memory (intended long-term primitive)

`src/ipc/shm.rs` implements:

- `OwnedShm`: producer-side `memfd_create` + `mmap(PROT_READ|PROT_WRITE)` with a hard cap
  `MAX_SHM_SIZE` (256 MiB).
- `OwnedShm::seal_readonly()`: best-effort sealing and a local read-only transition.
- `ReceivedShm::from_fd(...)`: consumer-side `fstat` size validation + `mmap(PROT_READ)`.

This is the intended building block for secure multiprocess frame/body transfers (with FD passing).

### memfd creation rules (sender)

When creating shared memory to send to another process:

1. Create with sealing support:
   - `memfd_create(..., MFD_CLOEXEC | MFD_ALLOW_SEALING)` (fall back only if sealing is acceptable
     to degrade; see `src/ipc/shm.rs`)
2. Set the size **exactly** (e.g. `ftruncate`) and enforce a hard cap:
   - Enforce both:
     - a **global** cap: `size_bytes <= shm::MAX_SHM_SIZE` (256 MiB today; see
       [`src/ipc/shm.rs`](../src/ipc/shm.rs))
     - a **message-type** cap: `size_bytes <= max_size` chosen by the protocol (passed to
       `ReceivedShm::from_fd(..., max_size)`).
   - For frame buffers, the per-message cap should also respect pixmap policy (`MAX_PIXMAP_BYTES` in
     [`src/paint/pixmap.rs`](../src/paint/pixmap.rs)).
3. Write the contents.
4. Apply seals **before sending the FD**:
   - Always: `F_SEAL_SHRINK | F_SEAL_GROW`
     - prevents size changes (shrink/grow) that can cause SIGBUS in the receiver’s mapping
   - If the mapping is intended to be immutable after send: also `F_SEAL_WRITE`
     - apply only once the writer is completely done (Linux may require unmapping writable mappings)
    - Optional: `F_SEAL_SEAL`, but only **after** applying all other seals you need.
      - Do **not** set `F_SEAL_SEAL` “early” if you might later need to add `F_SEAL_WRITE`.
      - For pooled/reusable buffers that must remain writable (e.g. browser-allocated frame slots),
        it can still be correct to apply `F_SEAL_SEAL` after `F_SEAL_SHRINK|F_SEAL_GROW` to prevent an
        untrusted peer from later adding `F_SEAL_WRITE` and breaking reuse.

### Validation rules (receiver)

On receiving a `memfd` FD from another process, the browser must treat it like untrusted input.

Rules:

1. **Size validation:**
   - `fstat` the FD and read `st_size`.
   - Reject if `st_size == 0` (unless explicitly allowed for a message type).
   - Reject if `st_size > max_size` (the per-message-type cap).
   - If the control message includes an expected length, it must match `st_size` exactly.
2. **Seal validation:**
   - Query seals via `fcntl(F_GET_SEALS)`.
   - Reject unless the FD has at least `F_SEAL_SHRINK | F_SEAL_GROW`.
   - If the message expects immutability, also require `F_SEAL_WRITE`.
3. **Mapping:**
   - `mmap` only the validated length.
   - Prefer `PROT_READ` in the browser unless a writeable mapping is explicitly required.
4. **Ownership / cleanup:**
   - Close the FD as soon as you no longer need the file descriptor (after `mmap` is usually fine).

Why seals are non-negotiable:

- If a malicious sender can `ftruncate` (shrink) after the receiver maps the file, the receiver can
  SIGBUS when reading beyond the new end-of-file. That’s a reliable browser crash primitive.

Repo reality note:

- `src/ipc/shm.rs` makes sealing **best-effort** and returns a `SealStatus`:
  - If you are accepting SHM from an **untrusted** peer, treat `SealStatus::Unsupported` as a
    protocol violation (or redesign so the browser allocates + seals the buffer before handing it to
    the renderer).
- `src/ipc/validate.rs` provides hardened, reusable validation helpers for untrusted shared-memory FDs
  (Linux-focused):
  - `validate_shm_fd(...)` checks fd type (`S_IFREG`), size bounds, and (on Linux) requires
    `F_SEAL_SHRINK|F_SEAL_GROW` and rejects non-sealable fds (fails closed).
  - `rgba_len(width, height)` computes expected RGBA8 sizes with checked arithmetic and enforces the
    pixmap hard cap (`MAX_PIXMAP_BYTES`).

### Frame pixels via FD attachment (`FrameReady`)

For browser↔renderer rendering, the in-tree protocol schema is designed so that **pixel bytes are not
serialized inline**:

- `RendererToBrowser::FrameReady { frame: SharedFrameDescriptor, ... }` must be accompanied by
  exactly **one** FD (see `expected_fds()` in
  [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs)).
- The FD contents are `frame.byte_len` bytes of **premultiplied RGBA8** pixels, row-major, with
  `frame.stride_bytes` bytes per row.

Receiver invariants (browser-side):

1. **FD arity check:** enforce `expected_fds() == 1` for `FrameReady`; reject and close any message
   that comes with the wrong FD count.
2. **Checked arithmetic only:** treat all dimensions/strides as attacker-controlled and compute
   expected sizes with overflow checks:
   - `min_stride = width_px * 4`
   - require `stride_bytes >= min_stride`
   - `expected_len = stride_bytes * height_px`
   - require `expected_len == byte_len` (or, if you intentionally allow padding, require
     `expected_len <= byte_len` and clearly document the padding rule)
3. **Hard caps:** `expected_len` must be <=:
    - the global SHM cap (`shm::MAX_SHM_SIZE`), and
    - your compositor/pixmap policy cap (e.g. `MAX_PIXMAP_BYTES`)
4. **FD validation before `mmap`:** validate the FD is the expected type and size (and, on Linux,
   that it is sealed) before mapping:
   - `fstat` the FD and require `st_size == expected_len`, then map only that length.
   - Prefer using the shared helper `validate_shm_fd(...)` in
     [`src/ipc/validate.rs`](../src/ipc/validate.rs) (Linux-focused) rather than re-implementing these
     checks.
5. **Prefer read-only mapping:** the browser should map with `PROT_READ` unless there is a strong
   reason to allow writes.

This is the main place where “IPC bytes → allocation” and “IPC metadata → mapping length” meet. Any
relaxation here is a potential browser crash/DoS primitive.

---

## Protocol versioning + upgrade strategy

**Bincode is not self-describing**: adding/removing enum variants or fields can break decoding.

Even for self-describing formats like JSON, our security posture is to **fail closed** on unexpected
fields (via `#[serde(deny_unknown_fields)]`), which means additive schema changes are *also*
effectively breaking unless explicitly negotiated.

Therefore versioning must be explicit and checked **before** attempting to decode arbitrary payloads.

Repo reality:

- The intended browser↔renderer multiprocess protocol in
  [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) defines:
  - `RENDERER_PROTOCOL_VERSION`
  - a `Hello { version, capabilities }` / `HelloAck` handshake
  - `expected_fds()` so the receiver can enforce FD arity per message.
- The current stdio+`bincode` dev transport in `crates/fastrender-renderer` assumes browser+renderer
  are built together (no explicit version negotiation). Treat mismatches as unsupported.

Rules (for any long-lived IPC connection):

1. Begin with a **handshake** that includes a protocol version (or enforce “exact build match”).
2. The browser is authoritative:
   - If the child proposes an unsupported version, the browser closes the connection and restarts
     the child.
3. Version numbers must be bumped intentionally:
   - **Major**: breaking change (old peers cannot communicate)
   - **Minor**: additive change that can be negotiated (only if decoding remains compatible)
4. Upgrade strategy:
   - During development, keep browser and child binaries matched (same build).
   - When introducing a breaking change, land it as:
     - browser supports `{old, new}` temporarily (if feasible), then
     - remove `{old}` support once the tree is fully migrated.

Minimal safe policy (recommended unless we implement explicit negotiation): **exact version match**.

---

## Threat model checklist (what the browser must defend against)

Assume renderer/network can attempt:

- send arbitrarily large payloads to cause OOM
- claim huge vector/string lengths to trigger allocations during decode
- spam `SCM_RIGHTS` to exhaust browser FDs
- send unsealed/truncatable `memfd` to induce SIGBUS in the browser
- send malformed frames / truncated ancillary data to desynchronize FD/message association

Browser invariants:

- never allocate > caps from IPC (message bytes, decode, shared memory)
- treat any malformed IPC as a protocol violation → close connection and kill the child
- close unexpected FDs immediately (no leaks; no smuggling)
- validate `memfd` size + seals before mapping
