# runtime-native reactor contract

This document defines the **cross-platform reactor contract** for `runtime-native` across:

- Linux (`epoll`)
- macOS/BSD (`kqueue`)

The goal is that higher layers (async runtime, scheduler, I/O resources) can rely on **one set of
readiness semantics** and not contain platform-specific special cases.

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

This mapping ensures that a consumer waiting for readability will be woken up to observe EOF.

## Timeout semantics

`Reactor::poll(events, timeout)` semantics:

- `timeout == None`: block indefinitely until at least one event or a wakeup occurs.
- `timeout == Some(d)`: wait for at most `d`, using **monotonic time** to compute the remaining
  timeout across retries.
- If the underlying syscall returns `EINTR`, the reactor retries, recomputing the remaining timeout.

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
