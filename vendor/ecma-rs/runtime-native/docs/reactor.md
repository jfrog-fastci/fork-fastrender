# runtime-native reactor contract

This document defines the **cross-platform reactor contract** for `runtime-native` across:

- Linux (`epoll`)
- macOS/BSD (`kqueue`)

The goal is that higher layers (async runtime, scheduler, I/O resources) can rely on **one set of
readiness semantics** and not contain platform-specific special cases.

## Registration API contract

The reactor exposes a small stateful API:

- `register(fd, token, interest)`
- `reregister(fd, token, interest)`
- `deregister(fd)`

The backend implementation **must provide the same observable behavior** on all platforms.

### Threading model

`Reactor` is a stateful, single-consumer object: `register`/`reregister`/`deregister`/`poll` all take
`&mut self` and are expected to be driven by a single reactor thread/event loop.

To interrupt a thread blocked in `poll()` from other threads, clone [`Waker`] and call
[`Waker::wake`].

### Interest cannot be empty

`interest` **must not** be empty.

- `register(..., Interest::empty())` returns `InvalidInput`.
- `reregister(..., Interest::empty())` returns `InvalidInput`.

To express "no longer interested", callers must use `deregister(fd)`.

### `register` when already registered

If `fd` is already registered with the same reactor, `register` fails with:

- `io::ErrorKind::AlreadyExists` (Linux: `EEXIST`)

The existing registration is left unchanged.

### `reregister` when not registered

If `fd` is not registered with the reactor, `reregister` fails with:

- `io::ErrorKind::NotFound` (Linux: `ENOENT`)

### `deregister` when not registered

If `fd` is not registered with the reactor, `deregister` fails with:

- `io::ErrorKind::NotFound` (Linux: `ENOENT`)

### File descriptor lifecycle / reuse

Registrations are tied to the **underlying OS object** (open file description), not the numeric file
descriptor value.

If a registered fd is **closed or replaced** (e.g. `close(fd)`, `dup2(other, fd)`), the reactor must
subsequently treat that numeric fd as **unregistered**:

- `deregister(fd)` / `reregister(fd, ...)` must fail with `io::ErrorKind::NotFound`.
- `register(fd, ...)` must succeed (it must not spuriously return `AlreadyExists`).

Callers are still encouraged to call `deregister` before closing to eagerly remove interest, but
correctness must not depend on it.

#### Implementation note (kqueue)

`epoll` tracks registrations in-kernel, so closing an fd automatically removes it from the epoll set.

`kqueue` requires the reactor to maintain a small user-space registration table in order to emulate
mio-style `EEXIST`/`ENOENT` errors. To make this robust to fd-number reuse, each kqueue registration
stores an `fstat` identity snapshot (`st_dev`, `st_ino`, and the file-type bits of `st_mode`) and
validates it on every `register`/`reregister`/`deregister`. If the current fd's identity differs (or
`fstat` fails with `EBADF`), the entry is treated as stale and removed, and the operation proceeds
as if the fd was never registered.

## Trigger mode: edge-triggered

The reactor is **edge-triggered** on all platforms:

- Linux uses `EPOLLET`.
- kqueue uses `EV_CLEAR`.

### Requirements for registered file descriptors

All file descriptors registered with the reactor **must be nonblocking** (`O_NONBLOCK`).

### Requirements for consumers

When an [`Event`] reports a source as readable or writable, consumers **must drain** the operation
until it returns `WouldBlock`:

- If `Event.readable == true`, call `read()` in a loop until it returns `WouldBlock` (or EOF / error).
- If `Event.writable == true`, call `write()` in a loop until it returns `WouldBlock` (or error).

Failing to drain can lead to stalls: the reactor may not emit another edge for the same readiness
state.

## Event aggregation / deduplication

Different OS backends report readiness differently:

- `epoll` reports a combined bitmask per fd.
- `kqueue` reports separate events per filter (`EVFILT_READ` and `EVFILT_WRITE`).

To provide a consistent abstraction, the reactor guarantees:

- Tokens are treated as the identity of a registration. **Callers must not register multiple
  fds with the same [`Token`] at the same time**, or events may be merged.
- **At most one [`Event`] per [`Token`] per [`Reactor::poll`] call.**
- If both read and write readiness are reported for the same token in a single poll, the reactor
  **merges them** into one `Event` with both `readable` and `writable` set.

## Error / HUP / EOF semantics

The reactor surfaces stream closure as readiness, plus an explicit direction flag:

- **EOF / peer close on the read side** is reported as:
  - `Event.readable == true`
  - `Event.read_closed == true`
- **Write-side close** is reported as:
  - `Event.writable == true`
  - `Event.write_closed == true`
- OS-reported error conditions are reported as:
  - `Event.error == true`

Note: on a full hangup, some platforms may report both read-side and write-side closure. In that
case the reactor may set both `read_closed` and `write_closed` (and therefore both `readable` and
`writable`) in the same [`Event`].

This mapping ensures that a consumer waiting for readability will be woken up to observe EOF.

## Timeout semantics

`Reactor::poll(events, timeout)` semantics:

- `timeout == None`: block indefinitely until at least one event or a wakeup occurs.
- `timeout == Some(d)`: wait for at most `d`, using **monotonic time** to compute the remaining
  timeout across retries.
- If the underlying syscall returns `EINTR`, the reactor retries, recomputing the remaining timeout.
- Extremely large timeouts may be **internally clamped/chunked** to fit OS syscall limits (e.g.
  `epoll_wait` takes an `i32` millisecond timeout, `kevent` takes a `time_t` second timeout). This
  is an implementation detail: the reactor still guarantees it will not block longer than `d`.

## Wake semantics

The reactor includes an internal cross-thread [`Waker`].

- `Waker::wake()` **may coalesce** (many wake calls may result in one wake event), but it must not
  lose wakeups in a way that can cause the reactor thread to block indefinitely.
- Calling `wake()` from **any thread** must cause a thread blocked in `poll()` to return promptly.
- Wakeups are surfaced to the caller as an `Event` with `token == Token::WAKE`.
  (`Token::WAKE` is reserved for this purpose.)

### kqueue wake implementation (EVFILT_USER vs pipe)

On kqueue platforms, the preferred wake mechanism is `EVFILT_USER`. Some environments may reject it
(older kernels, restricted sandboxes, or future kqueue ports), so the reactor can fall back to a
pipe-based wake.

#### `EVFILT_USER` (preferred)

- During reactor init, we attempt to `EV_ADD` an `EVFILT_USER` knote with `EV_CLEAR` so wake events
  behave like an edge-triggered notification.
- `Waker::wake()` triggers it via `NOTE_TRIGGER`.

#### Pipe fallback (portable)

- If `EVFILT_USER` registration fails with “not supported” errors (e.g. `ENOSYS`, `EINVAL`) **or**
  the crate is built with the `force_pipe_wake` feature, the reactor uses a nonblocking pipe.
- The read end is registered with `EVFILT_READ | EV_CLEAR` under `Token::WAKE`.
- `wake()` writes a single byte; `EAGAIN`/`WouldBlock` is ignored (wake already pending).
- When `poll()` observes `Token::WAKE`, it drains the pipe (read until `EAGAIN`) to re-arm
  `EV_CLEAR` edge semantics.

## Testing kqueue backends

Linux CI primarily exercises the `epoll` backend. To validate the kqueue implementation locally on
macOS/BSD, run:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  bash vendor/ecma-rs/scripts/cargo_agent.sh test -p runtime-native --test reactor_kqueue
```

To force and test the pipe-based wake fallback even on platforms where `EVFILT_USER` is available:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  bash vendor/ecma-rs/scripts/cargo_agent.sh test -p runtime-native \
    --test reactor_kqueue_pipe_wake --features force_pipe_wake
```
