# Async runtime ABI (runtime-native)

This document describes the **async** portion of the stable C ABI surface exposed by
`runtime-native`, plus additional execution model notes.

`include/runtime_native.h` is the authoritative ABI definition; this document exists to give
additional context for codegen and embedding implementations.

## Microtask checkpoint helpers

Two Rust entry points execute pending work without blocking:

- `rt_drain_microtasks() -> bool`
  - Drains the current microtask queue.
  - Returns `true` if it executed any microtasks.
- `rt_async_run_until_idle() -> bool`
  - Runs ready macrotasks + microtasks until both queues are empty.
  - Does **not** block in `epoll_wait` (timers and I/O readiness are not waited on).
  - Returns `true` if it executed any work.

Both functions are **non-reentrant** by design (HTML-style microtask checkpoint semantics). If
either function is called while a drain is already in progress (directly or indirectly, e.g. from
within a microtask), the nested call is treated as a **no-op** and returns `false`.

## Exported symbols (async)

### Native async/await ABI (PromiseHeader prefix)

- `rt_promise_init(p: PromiseRef)`
- `rt_promise_fulfill(p: PromiseRef)`
- `rt_promise_reject(p: PromiseRef)`
- `rt_async_spawn(coro: CoroutineRef) -> PromiseRef`
- `rt_async_poll() -> bool`

### Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates)

- `rt_promise_new_legacy() -> LegacyPromiseRef`
- `rt_promise_resolve_legacy(p: LegacyPromiseRef, value: ValueRef)`
- `rt_promise_reject_legacy(p: LegacyPromiseRef, err: ValueRef)`
- `rt_promise_then_legacy(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8)`
- `rt_async_spawn_legacy(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef`
- `rt_async_spawn_deferred_legacy(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef`
- `rt_async_poll_legacy() -> bool`
- `rt_async_sleep_legacy(delay_ms: u64) -> LegacyPromiseRef`
- `rt_coro_await_legacy(coro: *mut RtCoroutineHeader, awaited: LegacyPromiseRef, next_state: u32)`

### Microtasks + timers

- `rt_queue_microtask(cb: extern "C" fn(*mut u8), data: *mut u8)`
- `rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId`
- `rt_set_interval(cb: extern "C" fn(*mut u8), data: *mut u8, interval_ms: u64) -> TimerId`
- `rt_clear_timer(id: TimerId)`

### I/O readiness watchers

- `rt_io_register(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: *mut u8) -> IoWatcherId`
- `rt_io_update(id: IoWatcherId, interests: u32)`
- `rt_io_unregister(id: IoWatcherId)`

### Blocking thread pool

- `rt_spawn_blocking(task: extern "C" fn(*mut u8, LegacyPromiseRef), data: *mut u8) -> LegacyPromiseRef`

`PromiseRef`, `LegacyPromiseRef`, `ValueRef`, `CoroutineRef`, `RtCoroutineHeader`, `TimerId`, and
`IoWatcherId` are ABI-level opaque types; their layout is defined in `include/runtime_native.h`.

## Panic / unwinding policy

All exported async runtime C ABI functions are **abort-on-panic**: if a Rust panic occurs while
executing an exported `extern "C"` runtime function, the runtime will abort the process rather than
attempting to unwind across the FFI boundary.

Generated code must treat runtime panics as fatal and must not assume it can recover from panics or
observe them as structured errors.

## Legacy coroutine execution model

### Core model (coroutines + promises)

- A **coroutine** is a native-generated state machine. The legacy runtime interacts with it through
  an `RtCoroutineHeader` prefix placed at offset 0 of the coroutine frame.
- A coroutine produces a **result promise** (`LegacyPromiseRef`) that is returned to the JS world.
- A coroutine can suspend by yielding `RT_CORO_PENDING` after registering an await continuation via
  `rt_coro_await_legacy`.

### Spawning APIs

#### `rt_async_spawn_legacy`

```c
LegacyPromiseRef rt_async_spawn_legacy(RtCoroutineHeader* coro);
```

- Allocates/initializes the coroutine’s result promise and writes it to `coro->promise`.
- **Immediately resumes** the coroutine during the call (until it completes or reaches its first
  `await`).

This is the default “eager start” behavior.

#### `rt_async_spawn_deferred_legacy` (microtask-style)

```c
LegacyPromiseRef rt_async_spawn_deferred_legacy(RtCoroutineHeader* coro);
```

- Allocates/initializes the coroutine’s result promise and writes it to `coro->promise` (same as
  `rt_async_spawn_legacy`).
- Enqueues the coroutine’s *first resume* as a **microtask**.
- **Does not resume the coroutine synchronously**. The first resume happens later when the host runs
  a microtask checkpoint (`rt_drain_microtasks`, `rt_async_run_until_idle`, or `rt_async_poll_legacy`).

This API exists for Web-standard semantics that require guaranteed asynchronous execution, including:

- `queueMicrotask`
- Promise job scheduling (ECMA-262 `HostEnqueuePromiseJob`, HTML microtask queue)
- Strict `await` semantics where reaching the first `await` must be asynchronous

### Driving the runtime: `rt_async_poll_legacy`

```c
bool rt_async_poll_legacy(void);
```

Drives the full event loop for one turn:

- Executes at most one macrotask (timer/I/O/etc), then performs a microtask checkpoint.
- If there are no macrotasks, it drains microtasks directly.
- Blocks in `epoll_wait` when there is no ready work but there are pending I/O watchers or timers.

The return value indicates whether there is still pending work (timers, I/O watchers, microtasks,
macrotasks) after the turn.

## Ordering guarantees

Microtasks are FIFO: coroutines enqueued earlier are resumed earlier (including those enqueued via
`rt_async_spawn_deferred_legacy` and those woken by promise resolution). If Task 301 (FIFO ordering
work) is implemented, this property is relied upon to match Web microtask ordering.
