# Linux IPC: shared memory + FD passing (security checklist)

This note documents **how to safely combine shared memory and file-descriptor (FD) passing on Linux** for the browser↔renderer multiprocess architecture.

Assumption: the **renderer is untrusted / potentially compromised**. Anything that crosses the IPC boundary (bytes, FDs, sizes) must be treated as attacker-controlled.

---

## Use `memfd_create` (not `shm_open`) for shared memory

For this project, prefer `memfd_create(2)` over `shm_open(3)` / POSIX shared memory.

### Creation template (recommended)

Create memfds with:

- `memfd_create("…", MFD_CLOEXEC | MFD_ALLOW_SEALING)`

Footguns:

- If you forget `MFD_ALLOW_SEALING`, the file is created with the `F_SEAL_SEAL` seal already applied (meaning **you can’t add any other seals later**).
- If you forget `MFD_CLOEXEC`, the FD can leak across unrelated `execve()` in the creating process (or any thread racing an exec).

After creation, set the size with `ftruncate(2)` *before* `mmap(2)`, and treat size as a security boundary (tight upper bounds, overflow-checked calculations).

### Why `memfd_create` is a better default here

- **No global name / namespace**: `shm_open()` objects live in a global namespace (typically mounted at `/dev/shm`). If you need a name, you need a naming scheme, collision avoidance, and cleanup. `memfd_create()` gives you an anonymous file referenced only by an FD.
- **Plays well with sandboxing**: `shm_open()` depends on filesystem-ish machinery (`/dev/shm`, mount namespaces, permissions, etc.). A renderer sandbox that tries to remove filesystem access can accidentally break `shm_open()`. `memfd_create()` is a single syscall returning an FD; it’s much easier to allowlist in seccomp.
- **Automatic lifetime**: a `memfd` is freed when the last FD is closed. With `shm_open()`, crashes can leave behind persistent objects unless `shm_unlink()` is done correctly.
- **Supports file seals**: `memfd_create(MFD_ALLOW_SEALING)` enables seals (`fcntl(F_ADD_SEALS)`) that let the receiver trust that the size won’t change (and optionally that contents won’t change).

### The one time `shm_open` is reasonable

If you *cannot* pass an FD (no authenticated UNIX domain socket, unrelated processes), then `shm_open()` may be required. That’s not our architecture: browser and renderer already have a parent-established IPC channel, so **FD passing is available**.

If `shm_open()` is ever used anyway, treat it as a last-resort and apply the usual hardening:

- Use `O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC` (avoid races and FD leaks).
- Use restrictive permissions (e.g. `0600`) and a hard-to-guess name.
- Call `shm_unlink()` as soon as both sides have opened the object (so it can’t be reopened later).
- Still `ftruncate()` to a validated size and validate `st_size` before `mmap()`.

References:
- `memfd_create(2)`: https://man7.org/linux/man-pages/man2/memfd_create.2.html
- `ftruncate(2)`: https://man7.org/linux/man-pages/man2/ftruncate.2.html
- `shm_open(3)`: https://man7.org/linux/man-pages/man3/shm_open.3.html
- `shm_unlink(3)`: https://man7.org/linux/man-pages/man3/shm_unlink.3.html

---

## Seals: required policy and timing

### Always apply size-stability seals

When using a memfd for shared memory, **always** apply:

- `F_SEAL_SHRINK`
- `F_SEAL_GROW`

Rationale:
- Prevents a malicious peer from changing the file length after the receiver validated it.
- Avoids `SIGBUS` hazards when a mapped region is shrunk out from under the receiver.

Practical rule: apply these seals **after** `ftruncate()` and **before** sending the FD to the peer.

### Apply `F_SEAL_WRITE` when data must become immutable

Optionally apply `F_SEAL_WRITE` **once the writer is completely done** writing, *and* the buffer is intended to be immutable thereafter (e.g. a one-shot blob transfer, not a ring buffer that stays writable).

Important footgun from `memfd_create(2)`:
- To add `F_SEAL_WRITE`, you generally must first **unmap any shared writable mapping** of the file; otherwise `F_ADD_SEALS` may fail. (Alternatively, `F_SEAL_FUTURE_WRITE` exists, but the project policy is: use `F_SEAL_WRITE` when you truly want immutability.)

### Don’t set `F_SEAL_SEAL` too early

`F_SEAL_SEAL` prevents adding additional seals later. If there’s any chance you’ll want to add `F_SEAL_WRITE` in a later phase, **do not** apply `F_SEAL_SEAL` at creation time.

### If you don’t use `F_SEAL_WRITE`, assume the contents are mutable

With only size-stability seals (`F_SEAL_SHRINK|F_SEAL_GROW`), the peer can still modify the file
contents at any time. That’s fine for designs like ring buffers, but it means:

- shared memory contents are **attacker-controlled bytes**, and
- any structured data read from shared memory must be validated and/or copied out to avoid
  time-of-check-to-time-of-use issues.

References:
- `fcntl(2)` seals (`F_ADD_SEALS`, `F_GET_SEALS`): https://man7.org/linux/man-pages/man2/fcntl.2.html
- Kernel documentation on file seals: https://docs.kernel.org/userspace-api/file-seals.html

---

## Transport: prefer `AF_UNIX` + `SOCK_SEQPACKET`

Use UNIX-domain sockets for message passing + FD passing, and prefer:

- `socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, ...)`

Why `SOCK_SEQPACKET`:
- **Preserves message boundaries**: FD passing is per-message (`SCM_RIGHTS`). With message boundaries preserved, your protocol can be “one logical IPC message per `recvmsg()`”.
- **Prevents framing bugs** common with `SOCK_STREAM`: on a stream, you can accidentally read a partial header or partial payload while still receiving the ancillary FD(s), desynchronizing your parser.
- **Avoids stream “ancillary barrier” confusion**: on `SOCK_STREAM`, ancillary data forms a barrier for
  the byte stream (see `unix(7)`). If you aren’t extremely careful, you can accidentally associate
  an `SCM_RIGHTS` FD with the wrong logical message bytes. With `SOCK_SEQPACKET`, message boundaries
  are explicit and this class of bug largely goes away.
- **Truncation is explicit**: if your receive buffer is too small, the kernel sets `MSG_TRUNC` / `MSG_CTRUNC`. With `SOCK_STREAM`, there is no concept of message truncation.
- **Encourages bounded control messages**: keep large payloads (frames, blobs) out of the socket and in
  shared memory. This avoids `sendmsg()` size limits (`EMSGSIZE`) and makes it easier to enforce
  tight maximum message sizes.

`SOCK_DGRAM` also preserves boundaries, but `SOCK_SEQPACKET` is connection-oriented and often a better fit for structured protocols that want ordered reliable delivery with simpler lifecycle semantics.

### Sandbox friendliness: prefer inherited `socketpair()` endpoints

Avoid filesystem-backed UNIX sockets (paths under `/tmp`, etc.) inside the renderer:
- they require filesystem access, and
- they usually involve `bind(2)` / `connect(2)` / `listen(2)` / `accept(2)` which a renderer seccomp
  policy may intentionally block.

FastRender’s Linux renderer seccomp sandbox is already oriented toward this style: it allows
`socketpair(2)` for local IPC but denies many “network-like” socket syscalls. Prefer creating the
socketpair in the browser (trusted) process and inheriting/passing the connected FD into the
renderer before the sandbox is applied.

Note: FD passing itself requires `sendmsg(2)` / `recvmsg(2)` (for `SCM_RIGHTS`). If those syscalls
are blocked by the renderer sandbox, either:
- do FD passing before installing the seccomp filter, or
- extend the allowlist (see [`docs/seccomp_allowlist.md`](seccomp_allowlist.md)).

Also note: `send(2)` / `recv(2)` are typically implemented via the `sendto(2)` / `recvfrom(2)`
syscalls on Linux. If your sandbox denies `sendto/recvfrom` (FastRender’s renderer seccomp policy
currently does), prefer using `read(2)` / `write(2)` on a connected socket for steady-state IPC.

### Robustness footgun: avoid `SIGPIPE` killing the browser

If the peer exits or closes the socket, writes can fail with `EPIPE` and may raise `SIGPIPE`
depending on which syscall you use.

Browser-side IPC code should be resilient to renderer crashes; avoid letting a dead renderer trigger
process termination via `SIGPIPE`:

- Prefer `sendmsg(..., MSG_NOSIGNAL)` / `send(..., MSG_NOSIGNAL)` when writing to sockets.
- Alternatively, ignore `SIGPIPE` process-wide (common for network servers) and treat `EPIPE` as a
  normal error.

### FD passing footgun: include at least 1 byte of non-ancillary data

When sending `SCM_RIGHTS`, include at least **one byte** of real (non-ancillary) data in the same
`sendmsg()` call.

Reason:
- Linux requires this for `SOCK_STREAM`, and
- portable code should do it for all UNIX-domain socket types.

With `SOCK_SEQPACKET`, your protocol should already include a header/payload alongside the
ancillary FD(s), but this rule is worth stating explicitly to avoid “FD-only” messages.

References:
- `unix(7)` (socket types, `SCM_RIGHTS`): https://man7.org/linux/man-pages/man7/unix.7.html
- `socketpair(2)`: https://man7.org/linux/man-pages/man2/socketpair.2.html
- `sendmsg(2)`: https://man7.org/linux/man-pages/man2/sendmsg.2.html
- `send(2)`: https://man7.org/linux/man-pages/man2/send.2.html
- `recv(2)`: https://man7.org/linux/man-pages/man2/recv.2.html

---

## Mandatory receiver checks (FD passing)

Treat this as a **hard checklist** for any code that receives FDs from another process.

### 1) Use `recvmsg(MSG_CMSG_CLOEXEC)`

Always set `MSG_CMSG_CLOEXEC` so received FDs are **atomically** marked close-on-exec.

Reason: setting `FD_CLOEXEC` with a later `fcntl()` is a TOCTOU footgun (a different thread could `execve()` between the receive and the `fcntl()`).

Reference: `recvmsg(2)` https://man7.org/linux/man-pages/man2/recvmsg.2.html

### 2) Reject truncation (`MSG_TRUNC` / `MSG_CTRUNC`)

After `recvmsg()`, inspect `msghdr.msg_flags`:

- Reject if `msg_flags & MSG_TRUNC != 0` (payload truncated).
- Reject if `msg_flags & MSG_CTRUNC != 0` (control data truncated).

Rationale:
- Truncation means you did not receive what the sender actually sent.
- Especially with `SCM_RIGHTS`, truncation can drop some passed FDs. Even though Linux will close “excess” FDs in the receiver on truncation, your protocol state is now ambiguous → **treat as a protocol violation and fail closed**.

### 3) Bound FD count and close extras

When parsing `SCM_RIGHTS`:

- Enforce a strict **maximum expected FD count** (per-message).
- If more FDs are received than expected, **close the extras immediately** and treat the message as invalid (or at minimum treat it as “unexpected/ignore”).
- Reject malformed control messages (e.g. `cmsg_len` that is not a multiple of `sizeof(int)` for `SCM_RIGHTS`).
- On any validation/protocol failure *after receiving FDs*, **close all received FDs** before returning an error (avoid FD leaks in the browser).

Even though the kernel enforces a hard ceiling (`SCM_MAX_FD`, currently 253), that is far larger than any sane protocol message. “Accepting whatever count arrives” is an easy DoS vector.

Reference: `unix(7)` `SCM_RIGHTS` notes https://man7.org/linux/man-pages/man7/unix.7.html

### 4) `fstat` type + size validation before `mmap`

Before mapping or otherwise using a received FD:

- `fstat()` and verify the file type is what you expect.
  - For `memfd`, it should look like a **regular file** (`S_ISREG(st_mode)`).
  - Reject sockets, pipes, block devices, char devices, etc.
- Validate `st_size` is within a **tight upper bound** and matches the protocol expectation.
  - Example: if the message says `width`, `height`, `stride`, compute expected size with overflow checks and require `st_size == expected`.
  - Never `mmap()` “whatever size the sender picked” without bounds.

Only after this validation should you call `mmap()`.

When the buffer is intended to be read-only, map with `PROT_READ` (not `PROT_WRITE`), and if you
require immutability, also require `F_SEAL_WRITE` (see below).

References:
- `fstat(2)`: https://man7.org/linux/man-pages/man2/fstat.2.html
- `mmap(2)`: https://man7.org/linux/man-pages/man2/mmap.2.html

### 5) Size your control buffer correctly (to avoid accidental `MSG_CTRUNC`)

If you expect up to `N` file descriptors in a message, size the `msg_control` buffer using
`CMSG_SPACE(N * sizeof(int))` (not `CMSG_LEN`).

If the control buffer is too small:
- the kernel sets `MSG_CTRUNC`, and
- some FDs may be dropped (Linux will close “excess” FDs in the receiver, but your protocol state is
  now ambiguous).

Treat `MSG_CTRUNC` as a hard error (see above) and consider it a bug if it ever happens in normal
operation.

References:
- `cmsg(3)`: https://man7.org/linux/man-pages/man3/cmsg.3.html

---

## Strongly recommended receiver checks (defense-in-depth)

Not strictly required by the checklist above, but almost always correct for browser↔renderer IPC:

- Verify memfd seals with `fcntl(F_GET_SEALS)` and require at least:
  - `F_SEAL_SHRINK | F_SEAL_GROW`
  - and if the buffer is meant to be immutable: also require `F_SEAL_WRITE`
- Reject unexpected ancillary message types (anything other than `SCM_RIGHTS`, unless explicitly negotiated).
- Consider verifying peer identity:
  - If the browser spawns the renderer and uses `socketpair()`, peer identity is mostly implicit.
  - Otherwise, consider `SO_PEERCRED` / `SCM_CREDENTIALS` (`unix(7)`) to prevent confused-deputy issues.

---

## Avoid renderer → browser FD flow when possible

Every time the browser accepts an FD from the renderer, it must treat it as hostile and run the full validation checklist above.

Design preference: **browser allocates shared memory and passes it to the renderer**, rather than the renderer allocating and sending to the browser.

Practical pattern: *browser-allocated SHM ring buffer(s)*

- At renderer startup:
  - Browser creates one or more memfds (ring buffers) sized to a fixed upper bound.
  - Browser applies `F_SEAL_SHRINK|F_SEAL_GROW`.
  - Browser sends these FDs to the renderer once.
- During steady state:
  - Renderer writes into the ring and notifies the browser via small control messages (sequence number / offset / length).
  - No new FDs flow from renderer to browser during steady state.

Benefits:
- Reduces attack surface: no arbitrary FD injection into the browser.
- Eliminates per-frame FD passing overhead.
- Makes “FD receipt” a rare, auditable code path.
- Avoids kernel “in-flight FD” edge cases (e.g. `ETOOMANYREFS` when too many passed FDs are queued
  but not yet received), which is another reason to avoid per-frame `SCM_RIGHTS` traffic.

If renderer→browser FD flow is unavoidable (e.g. dynamic buffer resizing), treat it as a privileged operation:
- require explicit negotiation
- enforce tight bounds
- require seals
- fail closed on any mismatch
