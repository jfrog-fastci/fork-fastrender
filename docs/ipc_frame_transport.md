# IPC protocol: browser↔renderer frame transport (spec)

This document specifies the **browser ↔ renderer** IPC rules for transporting rendered frames via
shared memory / FD-backed buffers, including **security invariants**.

This is a **normative** developer spec. When changing IPC code, keep this document and the code in
sync.

Related (transport-wide) invariants are specified in [`docs/ipc.md`](ipc.md) and the Linux-specific
FD-passing checklist lives in [`docs/ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md).

## Scope (repo reality)

This spec covers the browser↔renderer *frame transport* path implemented by:

- Framing format + max frame size: [`src/ipc/framing.rs`](../src/ipc/framing.rs)
- Frame transport message schemas: [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs)
- Shared memory primitives (`memfd` + seals on Linux): [`src/ipc/shm.rs`](../src/ipc/shm.rs)
- Tempfile-backed shared buffers (fallback / dev): [`src/ipc/frame_pool.rs`](../src/ipc/frame_pool.rs)
- Ack-on-drop helper: [`src/ipc/received_frame.rs`](../src/ipc/received_frame.rs)

## Constants (security limits)

### Transport hard caps (`src/ipc/limits.rs`)

Transport-level hard limits are centralized in [`src/ipc/limits.rs`](../src/ipc/limits.rs):

| Constant | Value | Used for |
|---|---:|---|
| `MAX_IPC_MESSAGE_BYTES` | `8 MiB` | Hard cap on a single length-prefixed IPC payload (`read_frame` / `write_frame`). |
| `BYTES_PER_PIXEL` | `4` | Pixel format is fixed to **premultiplied RGBA8**. |
| `MAX_FRAME_BUFFERS` | `8` | Hard cap for the tempfile-backed `frame_pool` buffer count. |

### Protocol- and SHM-specific caps (other modules)

Some security limits live next to their protocol definitions:

| Constant | Value | Where |
|---|---:|---|
| `RENDERER_PROTOCOL_VERSION` | `2` | [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |
| `RENDERER_IPC_DECODE_LIMIT_BYTES` | `256 KiB` | [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) (`bincode_options().with_limit(...)`) |
| `MAX_URL_BYTES` | `8 KiB` | [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) (`UrlString` / `BoundedString`) |
| `MAX_SHM_SIZE` | `256 MiB` | [`src/ipc/shm.rs`](../src/ipc/shm.rs) |
| `DEFAULT_MAX_FRAMES_IN_FLIGHT` | `2` | [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs) |

## Attack surface (what is untrusted)

The browser process is the security boundary. Treat the renderer as **malicious**.

Untrusted inputs crossing the boundary:

1. **IPC bytes** read from the renderer (framed message payloads).
   - Validated by: length-prefix checks in `read_frame` and bounded `bincode` decode.
2. **Decoded renderer messages** (`ipc::protocol::renderer::RendererToBrowser`).
   - Validated by: per-message invariants (below) + `#[serde(deny_unknown_fields)]` on protocol
     structs/enums (unknown fields must be rejected).
3. **Attached file descriptors** (FDs) and their **shared-memory contents** (pixel bytes).
   - Validated by: per-message FD-arity checks (`expected_fds()`), `fstat` size checks, and (on
     Linux) `memfd` seal checks before mapping.

Safe default for any validation failure: **treat as protocol violation**, tear down the IPC channel,
and kill/restart the renderer process.

## Framing format and size limits

Framing is defined in [`src/ipc/framing.rs`](../src/ipc/framing.rs): a `u32` length prefix followed
by an opaque payload.

Wire format:

```
u32_le payload_len
[payload_len bytes payload]
```

Receiver requirements:

- `payload_len` **MUST** be `1..=MAX_IPC_MESSAGE_BYTES`.
- The receiver **MUST NOT** allocate based on `payload_len` until after checking the cap.
- EOF while reading the prefix or payload is an error (`UnexpectedEof`).

In addition to the outer framing cap, the browser↔renderer protocol uses a **separate** bincode decode
budget (`RENDERER_IPC_DECODE_LIMIT_BYTES`) so control messages stay allocation-bounded even if the
outer transport cap changes.

## Message schemas and required ordering

Message schemas are defined in [`src/ipc/protocol/renderer.rs`](../src/ipc/protocol/renderer.rs):

- `BrowserToRenderer` (trusted → untrusted)
- `RendererToBrowser` (untrusted → trusted)

### Connection handshake

Ordering is strict:

1. **Browser → Renderer**: `BrowserToRenderer::Hello { version: RENDERER_PROTOCOL_VERSION, capabilities }`
2. **Renderer → Browser**: `RendererToBrowser::HelloAck {}`

No other messages are valid before the handshake completes.

### Submitting a frame (renderer → browser)

The renderer publishes a completed frame as:

```rust
RendererToBrowser::FrameReady { frame_seq: u64, frame: SharedFrameDescriptor, ... }
```

and the message **must** be accompanied by exactly **one** attached FD containing the pixel bytes.

Browser-side validation requirements before mapping/reading the FD:

- FD-arity: `RendererToBrowser::FrameReady.expected_fds() == 1` and the received message actually
  carried exactly one FD (close extras; treat mismatch as protocol violation).
- `frame.width_px/height_px` are non-zero.
- `min_stride = width_px * BYTES_PER_PIXEL` (checked multiply).
- Require `frame.stride_bytes >= min_stride`.
- `expected_len = stride_bytes * height_px` (checked multiply).
- Require `expected_len == frame.byte_len` (the current protocol does not allow per-row padding).
- `fstat(fd).st_size == expected_len` and `expected_len <= shm::MAX_SHM_SIZE` (and any compositor
  policy cap such as `MAX_PIXMAP_BYTES`).

### Acknowledging a frame (browser → renderer)

After the browser has either:

- copied/uploaded the pixels out of the FD-backed buffer, or
- decided to drop the frame,

it **must** send:

```rust
BrowserToRenderer::FrameAck { frame_seq }
```

The renderer must treat the frame buffer as **in use** until it receives `FrameAck` for that
`frame_seq`.

Flow-control invariant: the renderer must cap the number of un-acked frames in flight (see
`DEFAULT_MAX_FRAMES_IN_FLIGHT` and `FrameInFlightCounter` in `src/ipc/protocol/renderer.rs`).

## Frame buffer lifecycle (FD-backed shared memory)

This section describes the lifecycle for the FD attached to `RendererToBrowser::FrameReady`.

### Renderer-side lifecycle

1. Allocate a backing store for the frame pixels (commonly a `memfd` on Linux via `ipc::shm`, or a
   tempfile-backed mapping in early/dev setups).
2. Write premultiplied RGBA8 pixels into the backing store.
3. Send `RendererToBrowser::FrameReady { frame_seq, frame: SharedFrameDescriptor, ... }` **and**
   attach the FD in the same `sendmsg`/packet.
4. Mark the backing store **in use** until the browser acks `frame_seq`.
5. When `BrowserToRenderer::FrameAck { frame_seq }` arrives, reclaim or reuse the backing store.

### Browser-side lifecycle

1. Receive a `FrameReady` + its attached FD.
2. Validate `(width_px, height_px, stride_bytes, byte_len)` and the FD size **before** mapping.
3. Map the FD read-only (preferred), read/copy pixels, then unmap/close as soon as practical.
4. Send `FrameAck { frame_seq }` (even if dropping the frame as stale).

## Shared memory identifier security constraints (cross-platform)

The current frame transport protocol passes pixel buffers by FD attachment (no string `shmem_id` on
the wire). However, platform backends that use **named** shared memory must preserve:

1. **Unpredictability**: names/paths must be hard to guess to prevent unrelated processes from
   opening the mapping.
   - Tempfile-backed implementations should rely on `tempfile` randomization.
2. **Access control**:
   - On Unix, create backing files/objects with restrictive permissions (e.g. `0600`).
   - On Windows, named objects/files must use a restrictive **DACL** so only the intended
     principals (browser + renderer sandbox identity) can open the mapping.
3. **macOS name length**:
   - If using POSIX shared memory (`shm_open`) on macOS, the OS enforces a very small name limit
     (historically `PSHMNAMLEN = 31` bytes). Backends must keep names within that limit.
