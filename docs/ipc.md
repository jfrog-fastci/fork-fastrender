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
- **Browser ↔ network (today):**
  - `src/resource/ipc_fetcher.rs` uses a `TcpStream` to a local “network process” endpoint, framed
    as `u32_le length` + payload, serialized with JSON (`serde_json`).
  - See: [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs).
- **Shared framing helper (in-tree):**
  - `src/ipc/framing.rs` provides a length-prefixed framing layer with a hard maximum frame size.
  - See: [`src/ipc/framing.rs`](../src/ipc/framing.rs).

All of the above are **stream transports**: message boundaries are *not* preserved by the kernel, so
framing must be explicit and allocation-bounded.

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

---

## Message framing + max message size (hard limit)

**Security invariant:** the browser must never allocate based on untrusted length fields without a
hard cap.

### Stream framing (current): `u32_le length` + payload

All current stream-based IPC channels use a simple frame format:

```
u32_le length
[length bytes payload]
```

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
| Generic framing helper (`read_frame`/`write_frame`) | 8 MiB | `crate::ipc::framing::MAX_IPC_MESSAGE_BYTES` in [`src/ipc/framing.rs`](../src/ipc/framing.rs) |
| Browser ↔ network (`IpcResourceFetcher`, JSON over TCP) | 128 MiB | `IPC_MAX_FRAME_BYTES` in [`src/resource/ipc_fetcher.rs`](../src/resource/ipc_fetcher.rs) |

Important: the 64 MiB browser↔renderer cap is intentionally large enough to carry early-development
pixel buffers inline (see the comment in `crates/fastrender-ipc`). **Long-term, frame transfers
should move to shared memory**, but the hard cap must remain enforced either way.

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

---

## FD passing rules (`SCM_RIGHTS`)

**FDs are capabilities.** Passing an FD is equivalent to granting the receiver access to a resource.

For Linux-specific pitfalls and a lower-level checklist, also see:
[ipc_linux_fd_passing.md](ipc_linux_fd_passing.md).

Security invariants:

1. **Only `SCM_RIGHTS` is allowed.** Reject other ancillary message types.
2. **Bound the count:** accept at most `MAX_FDS_PER_MESSAGE` per message.
   - For most message types this should be **0 or 1**.
   - Recommended global cap: **≤ 4**.
   - Repo reality: `src/ipc/fd_passing.rs` provides a defensive `recv_msg(sock_fd, max_fds)` helper
     that enforces `max_fds` (and has an internal absolute ceiling). Keep `max_fds` small at call
     sites.
3. **Message types must define the FD arity.**
   - For a given message type, the receiver knows exactly how many FDs to expect (usually 0 or 1).
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
   - Repo reality note: `src/ipc/fd_passing.rs` currently calls `recvmsg` with flags `0` (no
     `MSG_CMSG_CLOEXEC`). Until that is upgraded, callers must ensure passed FDs are already CLOEXEC
     (e.g. created with `O_CLOEXEC` / `MFD_CLOEXEC`) or set `FD_CLOEXEC` immediately after receipt.
7. **Include at least one byte of real payload data when sending FDs.**
   - On Linux, `SCM_RIGHTS` control messages are associated with a received datagram/packet. Sending
     “FD-only” control messages without accompanying payload bytes is a well-known footgun; always
     include at least one byte of non-ancillary data.
   - See also: [ipc_linux_fd_passing.md](ipc_linux_fd_passing.md).

Why this matters:

- A malicious renderer can try to exhaust the browser’s FD table by spamming `SCM_RIGHTS`.
- A malicious renderer can try to exploit “FD confusion” (attaching FDs to unexpected message
  boundaries) to trick the browser into treating an FD as a different resource than intended.

---

## Shared memory rules (`memfd`)

Long-term, large IPC payloads (pixels, blobs, network bodies, etc.) should not be transferred inline.
Use shared memory and pass the `memfd` via `SCM_RIGHTS`.

### Repo reality (today): tempfile-backed shared memory (no FD passing in this path yet)

The in-tree multiprocess frame-buffer pool currently uses **temporary files + `memmap2`** as a
cross-platform shared-memory stand-in:

- Browser creates/sizes/maps temp files and sends descriptors to the renderer:
  [`src/ipc/frame_pool.rs`](../src/ipc/frame_pool.rs)
- Renderer opens/maps the same paths and validates mapping size matches the descriptor.

Security invariants that must remain true (even before we migrate to `memfd`):

- **Browser allocates shared buffers**; renderer should not be able to cause the browser to accept
  arbitrary files.
- **Exact-size mapping:** after mapping, the receiver validates `mmap.len() == expected_len` and
  treats mismatches as protocol violations.
- **Size caps:** frame buffer size is capped by pixmap policy (`MAX_PIXMAP_BYTES` in
  [`src/paint/pixmap.rs`](../src/paint/pixmap.rs)) and by pool sizing logic.
  - Do not remove these caps; they prevent pathological allocations from a malicious peer.

This is *not* the final design: a sandboxed renderer should not rely on arbitrary filesystem access.
On Linux we expect to move to `memfd` + FD passing (see next section and
[ipc_linux_fd_passing.md](ipc_linux_fd_passing.md)).

### memfd creation rules (sender)

When creating shared memory to send to another process:

1. Create with sealing support:
   - `memfd_create(..., MFD_CLOEXEC | MFD_ALLOW_SEALING)`
2. Set the size **exactly** (e.g. `ftruncate`) and enforce a hard cap:
   - `size_bytes <= MAX_SHM_BYTES` where `MAX_SHM_BYTES` is a per-message-type security limit.
     - For frame buffers, this should be **≤ `MAX_PIXMAP_BYTES`** (see
       [`src/paint/pixmap.rs`](../src/paint/pixmap.rs)).
3. Write the contents.
4. Apply seals **before sending the FD**:
   - Always: `F_SEAL_SHRINK | F_SEAL_GROW`
     - prevents size changes (shrink/grow) that can cause SIGBUS in the receiver’s mapping
   - If the mapping is intended to be immutable after send: also `F_SEAL_WRITE`
     - apply only once the writer is completely done (Linux may require unmapping writable mappings)
   - Optional: `F_SEAL_SEAL`, but only **after** applying all other seals you need.
     - Do **not** set `F_SEAL_SEAL` “early” if you might later need to add `F_SEAL_WRITE`.

### Validation rules (receiver)

On receiving a `memfd` FD from another process, the browser must treat it like untrusted input.

Rules:

1. **Size validation:**
   - `fstat` the FD and read `st_size`.
   - Reject if `st_size == 0` (unless explicitly allowed for a message type).
   - Reject if `st_size > MAX_SHM_BYTES` (same per-message-type cap as above).
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

---

## Protocol versioning + upgrade strategy

**Bincode is not self-describing**: adding/removing enum variants or fields can break decoding.
Therefore versioning must be explicit and checked **before** attempting to decode arbitrary payloads.

Repo reality:

- The in-tree multiprocess protocol in [`src/ipc/protocol.rs`](../src/ipc/protocol.rs) uses an
  explicit version handshake:
  - browser sends `BrowserToRenderer::Hello { protocol_version: IPC_PROTOCOL_VERSION }`
  - renderer replies with `RendererToBrowser::HelloAck { protocol_version }`
  - both sides reject mismatches.
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
