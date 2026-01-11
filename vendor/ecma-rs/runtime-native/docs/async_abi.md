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
- `rt_async_spawn_deferred(coro: CoroutineRef) -> PromiseRef`
- `rt_async_cancel_all()`
- `rt_async_poll() -> bool`
- `rt_async_set_strict_await_yields(strict: bool)`

### Parallel → async payload promises

- `rt_parallel_spawn_promise(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8, layout: PromiseLayout) -> PromiseRef`
- `rt_promise_payload_ptr(p: PromiseRef) -> *mut u8`

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

## Awaiting parallel work via payload promises

CPU-bound parallel work must never block the async event-loop thread. Instead, parallel tasks return
results through a runtime promise:

- A coroutine awaits the promise (suspending via the runtime's await registration API).
- A worker thread writes the payload and settles the promise.
- Settlement wakes the awaiting coroutine back onto the async event loop.

### Relevant C ABI (parallel → promise)

```c
// Allocate a new pending promise and execute `task(data, promise)` on the work-stealing pool.
PromiseRef rt_parallel_spawn_promise(
  void (*task)(uint8_t* data, PromiseRef promise),
  uint8_t* data,
  PromiseLayout layout
);

// Get a writable pointer to the promise payload storage.
uint8_t* rt_promise_payload_ptr(PromiseRef promise);

// Settle the promise (exactly once).
void rt_promise_fulfill(PromiseRef promise);
void rt_promise_reject(PromiseRef promise);
```

`PromiseLayout` is:

```c
typedef struct PromiseLayout {
  size_t size;
  size_t align;
} PromiseLayout;
```

### Result retrieval

The settled promise does not carry an additional "value" beyond its state (`fulfilled`/`rejected`).
Instead, the worker publishes its result into the payload buffer described by `PromiseLayout`.
After awaiting, the coroutine reads the result from `rt_promise_payload_ptr(promise)`.

## Payload publication + state ordering contract

When settling a promise from a worker thread:

1. Write the payload into `rt_promise_payload_ptr(promise)` (respecting the `PromiseLayout`).
2. Call `rt_promise_fulfill(promise)` (or `rt_promise_reject`).

The runtime enqueues promise continuations onto the async microtask queue via a mutex-protected
queue, establishing a happens-before edge such that:

- All writes to the payload that occur before `rt_promise_fulfill/reject` are visible to the async
  event loop after the awaiting coroutine resumes.

## Wake semantics / fairness / determinism

- Promise settlement may occur on any worker thread.
- Settlement wakes all awaiting coroutines by enqueuing microtasks onto the async event loop.
- Promise continuations are stored in a FIFO list; continuations are enqueued in registration order.

## Panic / unwinding policy

All exported async runtime C ABI functions are **abort-on-panic**: if a Rust panic occurs while
executing an exported `extern "C"` runtime function, the runtime will abort the process rather than
attempting to unwind across the FFI boundary.

Generated code must treat runtime panics as fatal and must not assume it can recover from panics or
observe them as structured errors.

## Native coroutine execution model

### Core model (coroutines + promises)

- A **coroutine** is a native-generated state machine. The native ABI interacts with it through a
  `Coroutine` prefix placed at offset 0 of the coroutine frame.
- A coroutine produces a **result promise** (`PromiseRef`) that is returned to the JS world. The
  promise begins with a `PromiseHeader` at offset 0; the payload layout is owned by codegen.
- A coroutine suspends by returning `Await(p)` from its `resume` function; the runtime registers a
  microtask reaction on `p` to resume the coroutine when it settles.

### Coroutine frames

Every coroutine frame is a `#[repr(C)]` struct whose first field (at offset `0`) is a
[`Coroutine`](../src/async_abi.rs) header:

- `coro.vtable`: points to a static `CoroutineVTable` for the generated coroutine type.
- `coro.promise`: result promise (written by the runtime before first resume).
- `coro.next_waiter`: reserved for the runtime (e.g. intrusive wait lists); generated code should
  initialize this to null.
- `coro.flags`: runtime-controlled flags.

### Frame ownership (`coro.flags`)

The ownership of a coroutine frame is determined by the `CORO_FLAG_RUNTIME_OWNS_FRAME` bit in
`coro.flags`:

- **If `CORO_FLAG_RUNTIME_OWNS_FRAME` is set**:
  - The frame is **heap-owned by the runtime**.
  - The runtime will call `(*coro.vtable).destroy(coro)` **exactly once** after:
    - the coroutine completes (after `resume` returns `Complete`), or
    - the coroutine is cancelled via `rt_async_cancel_all`.
- **If `CORO_FLAG_RUNTIME_OWNS_FRAME` is not set**:
  - The frame is **caller-owned** (typically stack-temporary).
  - The runtime must **never** call `destroy`.

### Suspension rule (stack frames)

A stack/caller-owned coroutine frame must not be stored by the runtime across turns.

In practice this means:

- A coroutine that yields `CoroutineStepTag::Await` must have `CORO_FLAG_RUNTIME_OWNS_FRAME` set.
- In debug builds, `runtime-native` will abort with a clear message if a coroutine yields `Await`
  without the flag, because this would otherwise lead to use-after-return UB.

### Cancellation (`rt_async_cancel_all`)

`rt_async_cancel_all` is a teardown helper that destroys all runtime-owned coroutine frames that are
currently queued/owned by the runtime. It must not double-destroy frames even if:

- a coroutine is accidentally enqueued multiple times,
- cancellation is requested after completion,
- promise settlement schedules a stale resume.

### Spawning APIs

#### `rt_async_spawn`

```c
PromiseRef rt_async_spawn(CoroutineRef coro);
```

- Allocates/initializes the coroutine's result promise and writes it to `coro->promise`.
- **Immediately resumes** the coroutine during the call (until it completes or reaches its first
  `await`).

#### `rt_async_spawn_deferred` (microtask-style)

```c
PromiseRef rt_async_spawn_deferred(CoroutineRef coro);
```

- Allocates/initializes the coroutine's result promise and writes it to `coro->promise` (same as
  `rt_async_spawn`).
- Enqueues the coroutine's *first resume* as a **microtask**.
- **Does not resume the coroutine synchronously**. The first resume happens later when the host runs
  a microtask checkpoint (`rt_drain_microtasks`, `rt_async_run_until_idle`, or `rt_async_poll`).

This API exists for Web-standard semantics that require guaranteed asynchronous execution, including:

- `queueMicrotask`
- Promise job scheduling (ECMA-262 `HostEnqueuePromiseJob`, HTML microtask queue)
- Strict `await` semantics where reaching the first `await` must be asynchronous

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

- Allocates/initializes the coroutine's result promise and writes it to `coro->promise`.
- **Immediately resumes** the coroutine during the call (until it completes or reaches its first
  `await`).

This is the default \"eager start\" behavior.

#### `rt_async_spawn_deferred_legacy` (microtask-style)

```c
LegacyPromiseRef rt_async_spawn_deferred_legacy(RtCoroutineHeader* coro);
```

- Allocates/initializes the coroutine's result promise and writes it to `coro->promise` (same as
  `rt_async_spawn_legacy`).
- Enqueues the coroutine's *first resume* as a **microtask**.
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

## Unhandled promise rejections

The runtime tracks unhandled rejections in a JS/HTML-shaped way:

- When a promise is rejected while it has no rejection handlers, it is eligible to be reported as an
  `unhandledrejection` at a microtask checkpoint.
- If a previously-unhandled rejected promise later becomes handled, it is eligible to be reported as
  `rejectionhandled`.

Important: **`await` attaches a rejection handler**, even if the coroutine only propagates the error
to its own returned promise. Therefore, awaiting a promise counts as making it “handled” for the
purposes of unhandled-rejection detection.

This must happen even when the runtime takes a fast-path for already-settled promises (synchronous
resumption), because attaching `await` reactions after a rejection should still trigger
`rejectionhandled` behavior.

## Ordering guarantees

Microtasks are FIFO: coroutines enqueued earlier are resumed earlier (including those enqueued via
`rt_async_spawn_deferred_legacy` and those woken by promise resolution). If Task 301 (FIFO ordering
work) is implemented, this property is relied upon to match Web microtask ordering.
