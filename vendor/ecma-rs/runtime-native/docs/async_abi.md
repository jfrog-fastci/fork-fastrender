# Async runtime ABI (runtime-native)

This document describes the **async** portion of the stable C ABI surface exposed by
`runtime-native`, plus additional execution model notes.

`include/runtime_native.h` is the authoritative ABI definition; this document exists to give
additional context for codegen and embedding implementations.

## Microtask draining

Two Rust entry points execute pending work:

- `rt_drain_microtasks() -> bool`
- `rt_async_run_until_idle() -> bool`

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
