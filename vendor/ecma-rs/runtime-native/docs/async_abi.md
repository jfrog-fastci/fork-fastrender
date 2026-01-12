# Async ABI contract (`runtime-native`)

This document describes the **async** portion of the stable compiler/runtime ABI surface exposed by
`runtime-native`.

`include/runtime_native.h` is the authoritative ABI definition for exported symbols and C-facing
struct definitions. This document provides additional semantics and invariants (layout diagrams,
state machines, memory ordering, scheduling, and GC constraints) that are relied upon by codegen and
embedders.

Guardrails live in three places:

- `runtime-native/src/async_abi.rs`: ABI types + compile-time layout assertions
- `runtime-native/tests/abi_layout.rs`: field-offset/size conformance tests
- this document: semantics, ordering, scheduling, and GC constraints

## Object layouts

### `Promise<T>`

**Definition:**

```text
Promise<T> := PromiseHeader + payload(T)
```

`PromiseHeader` is a stable prefix at **offset 0** of every promise allocation.

```text
┌──────────────────────────────────────────────┐
│ Promise<T>                                   │
├──────────────────────────────────────────────┤
│ PromiseHeader                                │  <── PromiseRef points here
│  - state     : AtomicU8                      │
│  - waiters   : AtomicUsize                   │  (0 / ptr(head of PromiseReactionNode list))
│  - flags     : AtomicU8                      │
├──────────────────────────────────────────────┤
│ payload(T)                                   │  (written before fulfill/reject)
└──────────────────────────────────────────────┘
```

**Key layout invariants:**

- `PromiseHeader` is `#[repr(C, align(8))]`.
- `payload(T)` begins at `addr(PromiseRef) + size_of::<PromiseHeader>()`.
- `size_of::<PromiseHeader>()` is guaranteed to be a multiple of `align_of::<PromiseHeader>()`.

### `CoroutineFrame`

**Definition:**

```text
CoroutineFrame := Coroutine + locals
```

`Coroutine` is a stable prefix at **offset 0** of every coroutine frame allocation.

```text
┌───────────────────────────────────────────┐
│ CoroutineFrame (heap allocated when live) │
├───────────────────────────────────────────┤
│ Coroutine                                 │  <── CoroutineRef points here
│  - vtable      : *const CoroutineVTable   │  (must be first field)
│  - promise     : PromiseRef               │  (written by rt_async_spawn*)
│  - next_waiter : *mut Coroutine           │  (runtime-only intrusive list)
│  - flags       : u32                      │
├───────────────────────────────────────────┤
│ locals / state machine slots              │  (compiler-generated)
└───────────────────────────────────────────┘
```

### Coroutine vtable

`CoroutineVTable` is a stable layout; generated code must populate it exactly.

```text
CoroutineVTable {
  resume: unsafe extern "C" fn(*mut Coroutine) -> CoroutineStep,
  destroy: unsafe extern "C" fn(CoroutineRef),

  promise_size: u32,
  promise_align: u32,
  promise_shape_id: RtShapeId,

  abi_version: u32,
  reserved: [usize; 4],
}
```

- `resume` runs the coroutine state machine until the next suspension point.
- `destroy` destroys (drops + deallocates) a coroutine frame. The runtime calls this **only** when
  the coroutine frame is runtime-owned (see `CORO_FLAG_RUNTIME_OWNS_FRAME`).
- The `promise_*` fields describe the result promise allocation that `rt_async_spawn*` creates and
  stores into `coro.promise`.

## Promise state machine

`PromiseHeader.state` contains a `PromiseState` value (`u8`):

- `PENDING   = 0`
- `FULFILLED = 1`
- `REJECTED  = 2`

State transitions:

```text
Pending  -- fulfill --> Fulfilled
   |
   '-- reject  -----> Rejected
```

Rules:

- The only legal transitions are `Pending → Fulfilled` and `Pending → Rejected`.
- `Fulfilled` and `Rejected` are terminal.

## Waiter / reaction registration + wake algorithm

Promises maintain a lock-free, intrusive stack of *reaction nodes*. Reactions are the unified
mechanism for:

- resuming an `await` continuation, and
- running `.then(...)` callbacks.

### Representation

`PromiseHeader.waiters` is an `AtomicUsize` containing one of:

- `0` (no reactions yet), or
- a `PromiseReactionNode*` cast to `usize` (head of an intrusive singly-linked list).

The list nodes are `PromiseReactionNode`s with a `next` pointer. The list is pushed in LIFO order
for low-overhead registration; it is drained in FIFO order by reversing the list before scheduling
jobs onto the microtask queue.

### Registering a reaction (conceptual pseudocode)

```text
register_reaction(promise, node):
  // Push onto the intrusive list.
  loop:
    head = promise.waiters.load(Acquire)
    node.next = head
    if promise.waiters.compare_exchange_weak(head, node, AcqRel, Acquire):
      break

  // Race fix (post-CAS recheck):
  //
  // The promise might have settled after we pushed but before the settler drained the list. Recheck
  // state and, if settled, drain reactions again to avoid a lost wake.
  if promise.state.load(Acquire) != PENDING:
    drain_reactions(promise)
```

### Settling a promise + draining reactions (conceptual pseudocode)

```text
resolve(promise, new_state):
  // Payload must already have been written by the *winning* resolver.
  //
  // Settlement is first-wins and thread-safe: only the caller that transitions
  // `PENDING -> {FULFILLED,REJECTED}` drains reactions.
  if !promise.state.compare_exchange(PENDING, new_state, AcqRel, Acquire):
    return
  drain_reactions(promise)

drain_reactions(promise):
  head = promise.waiters.swap(0, AcqRel)
  head = reverse_list(head)   // FIFO order

  while head != null:
    next = head.next
    head.next = null
    enqueue_microtask(run_reaction(head, promise))
    head = next
```

Key invariant:

- `drain_reactions` is idempotent and safe to call multiple times. If it races with concurrent
  registrations, the post-CAS recheck ensures a subsequent drain will pick up newly-added nodes.

## Memory ordering contract

This contract prevents “heisenbugs” across CPU cores.

### Producer (generated code)

When resolving a promise:

1. Write `payload(T)` (ordinary writes).
2. Call `rt_promise_fulfill(promise)` or `rt_promise_reject(promise)`.

If multiple producers may race to settle the same promise (e.g. host callbacks, duplicate
registrations), generated code must ensure **only the winning settle call writes the payload**. Use
`rt_promise_try_fulfill` / `rt_promise_try_reject` to determine the winner when needed.

### Runtime (fulfill/reject)

The runtime must publish the payload by atomically transitioning `PENDING → {FULFILLED,REJECTED}`
using an atomic compare-and-swap (CAS) with **Release** (or stronger) semantics. Only the CAS winner
drains reactions and schedules wakeups.

### Consumer (awaiting code)

Before reading `payload(T)`, the awaiting side must:

- load `PromiseHeader.state` with **Acquire** semantics and observe a non-`Pending` state.

That Acquire load synchronizes-with the runtime’s Release state transition, making the payload writes
visible.

## Scheduling semantics (`rt_async_*`)

These are the guarantees codegen is allowed to rely on.

### `rt_async_spawn(coro: CoroutineId) -> PromiseRef`

- Takes ownership of the coroutine handle (`CoroutineId`), allocated via the persistent handle ABI
  (`rt_handle_alloc` / `rt_handle_alloc_h`). The runtime consumes the handle and frees it when the
  coroutine completes (or is cancelled).
- Allocates the result promise described by the coroutine frame's
  `CoroutineVTable.{promise_size,promise_align,promise_shape_id}`.
- Stores the promise pointer into the coroutine frame's `promise` field.
- **Immediately resumes** the coroutine during the call (until it completes or reaches its first
  `await`).
- Returns the coroutine's promise handle.

`CoroutineId` is an ABI-stable `u64` (backed by the persistent handle table). The runtime resolves
the handle to a `Coroutine*` each time it needs to resume, and frees the handle on
completion/cancellation (destroying the frame if `CORO_FLAG_RUNTIME_OWNS_FRAME` is set).

### `rt_async_spawn_deferred(coro: CoroutineId) -> PromiseRef`

- Same allocation/initialization and ownership semantics as `rt_async_spawn`.
- Enqueues the coroutine’s *first resume* as a **microtask** (does not resume synchronously).

### `rt_async_poll() -> bool`

Drive the runtime's async/event-loop queues for one turn.

- Runs at most one macrotask (after promoting due timers), then performs a microtask checkpoint.
- May block waiting for timer deadlines, I/O readiness, or external wakeups when the runtime has
  pending work but nothing is immediately runnable (e.g. while a `rt_parallel_spawn_promise` task is
  still outstanding).
- Returns `true` iff there is still pending work after this turn (queued microtasks/macrotasks,
  active timers, I/O watchers, or outstanding external work such as a promise returned by
  `rt_parallel_spawn_promise` that has not yet settled). Returns `false` when the runtime is fully
  idle.

`rt_async_poll_legacy` is a compatibility alias with identical behavior.
For a non-blocking microtask-only checkpoint, use `rt_drain_microtasks`.

### `rt_async_wait()`

Blocks the current thread until at least one async task becomes ready. This is intended for an event
loop thread to park when the runtime is idle (no timers/I/O ready) and be woken by promise settlement
or other cross-thread enqueues.

### `rt_async_cancel_all()`

Teardown helper that discards **all pending async work** without running it.

This clears:

- queued microtasks (promise reaction jobs, `queueMicrotask` callbacks, deferred coroutine resumes),
- queued macrotasks (timers, I/O callbacks),
- registered timers and I/O watchers,
- pending promise reactions stored on unresolved promises,
- and any internal rejection-tracking bookkeeping.

Drop hooks:

- For **runtime-owned** native async-ABI coroutine frames (frames where `CORO_FLAG_RUNTIME_OWNS_FRAME`
  is set), the runtime calls `(*coro.vtable).destroy(coro)` exactly once.
- For queued microtasks, if `Microtask.drop` is non-null, the runtime calls `drop(data)` when the
  microtask is discarded without running.

## Microtask draining (`rt_drain_microtasks` / `rt_async_run_until_idle`)

## Microtask queue semantics

`runtime-native` maintains a **single FIFO microtask queue** (JS-like semantics).

All microtask sources enqueue into this same queue:

- Promise reaction jobs (including async/await coroutine wakeups, and `then`-style callbacks).
- `rt_queue_microtask` callbacks (JS `queueMicrotask`).
- Deferred coroutine scheduling via:
  - `rt_async_spawn_deferred` (native coroutine ABI), and
  - `rt_async_spawn_deferred_legacy` (legacy coroutine ABI).

### Draining / microtask checkpoint

Microtasks are drained **to exhaustion** at a microtask checkpoint. Jobs enqueued while draining are
appended to the same FIFO queue and are executed in the **same** checkpoint.

### Thread-safety

Enqueuing microtasks is **thread-safe**:

- `rt_queue_microtask` (and related helpers) may be called from any OS thread.
- If the event-loop thread is blocked in `rt_async_wait` / the reactor wait syscall, a cross-thread
  enqueue will wake it so pending microtasks can be observed.

The native async runtime provides a small, JS-shaped event loop:

- **macrotasks** (timers / I/O callbacks)
- **microtasks** (promise continuations / `queueMicrotask`)
- **coroutines** (LLVM-lowered `async`/`await`, resumed via microtasks/macrotasks)

## Single-driver execution model (JS-like)

Running microtasks/coroutines is **single-threaded**. Only one OS thread may *drive* the event loop
at a time.

### Driving entrypoints

The following APIs execute queued microtasks/macrotasks and therefore are considered **driving**
entrypoints:

- `rt_async_poll` / `rt_async_poll_legacy`
- `rt_async_run_until_idle`
- `rt_drain_microtasks`
- `rt_async_block_on`
- `rt_async_cancel_all`

### Policy

- If a driving entrypoint is called **concurrently** from a different thread while another thread
  is already driving, the runtime **aborts** with a clear error message.
- If a driving entrypoint is called **re-entrantly** from the *same* thread (e.g. a microtask calls
  `rt_async_poll`), the call is treated as a **no-op** and returns `false` (for `bool`-returning
  functions).

This avoids subtle races (e.g. resuming a coroutine concurrently) and prevents deadlocks from
recursive event-loop driving.

## Thread-safe producers

Other threads are allowed to interact with the async runtime via **multi-producer** APIs:

- resolving/rejecting promises
- registering continuations
- enqueueing microtasks (`rt_queue_microtask*`)
- scheduling timers (`rt_set_timeout*` / `rt_set_interval*` / `rt_clear_timer`)
- I/O watcher registration/update/unregistration (`rt_io_register*` / `rt_io_update` / `rt_io_unregister`)

These operations are thread-safe and will wake a driver blocked inside `rt_async_poll` (via the
reactor wake mechanism).

## Microtask checkpoint helpers

Two Rust entry points execute pending work without blocking:

- `rt_drain_microtasks() -> bool`
  - Drains the current microtask queue.
  - Returns `true` if it executed any microtasks.
- `rt_async_run_until_idle() -> bool`
  - Runs ready macrotasks + microtasks until both queues are empty.
  - Does **not** block in the platform reactor wait syscall (`epoll_wait`/`kevent`)
    (timers and I/O readiness are not waited on).
  - Returns `true` if it executed any work.

Both functions are **non-reentrant** by design (HTML-style microtask checkpoint semantics). If
either function is called while a drain is already in progress (directly or indirectly, e.g. from
within a microtask), the nested call is treated as a **no-op** and returns `false`.

At the end of a microtask checkpoint the runtime:

- processes promise rejection tracking (unhandled rejections), and
- runs an optional checkpoint-end hook (used by tests; see `test_util::set_microtask_checkpoint_end_hook`).

### `rt_drain_microtasks` semantics

`rt_drain_microtasks` runs *only* microtasks (no timers/reactor/macrotasks). Like the web platform,
microtasks scheduled while microtasks are executing are appended and will run in the same drain call
until the queue is empty (or runaway limits are hit).

### Runaway limits

JavaScript semantics drain microtasks "until empty", but embedders need a guardrail against unbounded
microtask loops (e.g. a microtask that keeps re-queueing itself). The runtime enforces a fixed
maximum number of microtasks per checkpoint (see `src/async_rt/event_loop.rs`). Exceeding this limit
aborts the process with a diagnostic rather than livelocking forever.

In addition, the runtime exposes configurable *soft* limits for embedders that want to stop execution
without aborting:

- `rt_async_set_limits(max_ready_steps_per_poll, max_ready_queue_len)`
  - If either limit is exceeded, the runtime enters an **error state** and stops making forward
    progress.
  - The error message can be retrieved with `rt_async_take_last_error()` (which clears the error
    flag).

If the runtime hits a runaway/error condition and you need to abandon the event loop early (or you
plan to drop any host data that may be referenced by queued jobs), call `rt_async_cancel_all()` to
discard pending work and run discard drop hooks. Note: `rt_async_cancel_all()` clears the last-error
state as part of resetting the runtime for reuse, so call `rt_async_take_last_error()` first if you
need the diagnostic.

### Mapping `queueMicrotask(cb)` to the ABI

Bindings should implement `queueMicrotask(cb)` by:

1. Allocating a small payload containing whatever is needed to call `cb` (e.g. function handle +
   realm/global).
2. Calling `rt_queue_microtask(Microtask { func: <trampoline>, data: <payload>, drop: <drop_fn or NULL> })`.
3. Having the trampoline invoke `cb` and then free the payload (or otherwise manage its lifetime).
   If a drop hook is provided, it is called only if the microtask is discarded without running (e.g.
   `rt_async_cancel_all`).

This avoids allocating a promise/coroutine frame for simple callbacks while still integrating with
the runtime's single-consumer microtask checkpoint semantics.

### GC-managed microtask payloads (`rt_queue_microtask_rooted`)

`rt_queue_microtask` takes an opaque `data` pointer that must remain valid until the callback runs.
If `data` is a **GC-managed object pointer** (and the GC is moving), embedders must keep the object
alive *and* tolerate relocation.

For this use-case, the runtime also exposes:

- `rt_queue_microtask_rooted(cb: extern "C" fn(*mut u8), data: *mut u8)`
- `rt_queue_microtask_rooted_h(cb: extern "C" fn(*mut u8), data: GcHandle)`

Contract:

- `data` must be a pointer to the **base** of a GC-managed object (start of its header).
- The runtime registers a strong GC root for `data` until the microtask executes.
- If the GC relocates the object, the callback receives the **updated** pointer.

The `_h` ("handle") variant is preferred under a moving GC: it accepts a pointer-to-slot handle so
the runtime can reload `data` after any potentially blocking lock acquisition while registering the
persistent root.

### Persistent-handle microtask payloads (`rt_queue_microtask_handle`)

For embedders that already represent GC-managed objects as persistent handles (`HandleId`/`u64`,
allocated via `rt_handle_alloc`), the runtime also exposes handle-based microtask helpers:

- `rt_queue_microtask_handle(cb: extern "C" fn(*mut u8), data: u64)`
- `rt_queue_microtask_handle_with_drop(cb: extern "C" fn(*mut u8), data: u64, drop_data: extern "C" fn(*mut u8))`

Ownership contract:

- The runtime consumes `data` and treats it as a strong GC root until the microtask runs (or is
  discarded via `rt_async_cancel_all`).
- For the `_with_drop` variant, `drop_data` is invoked exactly once when the microtask is torn down,
  and runs before the runtime frees the handle.
- If `data` is stale (freed), the callback is treated as a no-op.

### Persistent-handle timer payloads (`rt_set_timeout_handle` / `rt_set_interval_handle`)

The timer APIs have equivalent handle-based variants for persistent-handle userdata:

- `rt_set_timeout_handle(cb: extern "C" fn(*mut u8), data: u64, delay_ms: u64) -> TimerId`
- `rt_set_timeout_handle_with_drop(cb: extern "C" fn(*mut u8), data: u64, drop_data: extern "C" fn(*mut u8), delay_ms: u64) -> TimerId`
- `rt_set_interval_handle(cb: extern "C" fn(*mut u8), data: u64, interval_ms: u64) -> TimerId`
- `rt_set_interval_handle_with_drop(cb: extern "C" fn(*mut u8), data: u64, drop_data: extern "C" fn(*mut u8), interval_ms: u64) -> TimerId`

Ownership contract:

- The runtime consumes `data` and treats it as a strong GC root while the timer is active.
  - For timeouts: until the timeout fires or is cleared.
  - For intervals: until the interval is cleared.
- For the `_with_drop` variants, `drop_data` is invoked exactly once when the timer is torn down
  (including via `rt_async_cancel_all`), and runs before the runtime frees the handle.
- If `data` is stale (freed), the callback is treated as a no-op.

## Exported symbols (async)

### Native async/await ABI (PromiseHeader prefix)

- `rt_promise_init(p: PromiseRef)`
- `rt_promise_fulfill(p: PromiseRef)`
- `rt_promise_try_fulfill(p: PromiseRef) -> bool`
- `rt_promise_reject(p: PromiseRef)`
- `rt_promise_try_reject(p: PromiseRef) -> bool`
- `rt_promise_mark_handled(p: PromiseRef)`
- `rt_async_spawn(coro: CoroutineId) -> PromiseRef`
- `rt_async_spawn_deferred(coro: CoroutineId) -> PromiseRef`
- `rt_async_cancel_all()`
- `rt_async_poll() -> bool`
- `rt_async_set_strict_await_yields(strict: bool)`

### Parallel → async payload promises

- `rt_parallel_spawn_promise(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8, layout: PromiseLayout) -> PromiseRef`
- `rt_parallel_spawn_promise_rooted(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8, layout: PromiseLayout) -> PromiseRef`
- `rt_parallel_spawn_promise_rooted_h(task: extern "C" fn(*mut u8, PromiseRef), data: GcHandle, layout: PromiseLayout) -> PromiseRef`
- `rt_parallel_spawn_promise_with_shape(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8, promise_size: usize, promise_align: usize, promise_shape: RtShapeId) -> PromiseRef`
- `rt_parallel_spawn_promise_with_shape_rooted(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8, promise_size: usize, promise_align: usize, promise_shape: RtShapeId) -> PromiseRef`
- `rt_parallel_spawn_promise_with_shape_rooted_h(task: extern "C" fn(*mut u8, PromiseRef), data: GcHandle, promise_size: usize, promise_align: usize, promise_shape: RtShapeId) -> PromiseRef`
- `rt_promise_payload_ptr(p: PromiseRef) -> *mut u8`

#### Rooted vs unrooted task userdata

`rt_parallel_spawn_promise` is the lowest-overhead option for passing task userdata, but it is
**unrooted**: the caller must keep `data` valid until the worker finishes. Under a moving GC this
means you must not pass a raw movable GC pointer as `data` unless you separately pin/root it.

For GC-managed userdata, use `rt_parallel_spawn_promise_rooted{,_h}`:

- The rooted variants treat `data` as a GC **object base pointer** and keep it alive across worker
  execution, passing the relocated pointer to the task callback.
- The `_h` variant takes a `GcHandle` (pointer-to-slot) and is preferred under a moving GC to avoid a
  TOCTOU race between loading a pointer and registering the runtime root.

### Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates)

- `rt_promise_new_legacy() -> LegacyPromiseRef`
- `rt_promise_resolve_legacy(p: LegacyPromiseRef, value: ValueRef)`
- `rt_promise_reject_legacy(p: LegacyPromiseRef, err: ValueRef)`
- `rt_promise_then_legacy(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8)`
- `rt_promise_then_rooted_legacy(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8)`
- `rt_promise_then_rooted_h_legacy(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: GcHandle)`
- `rt_promise_then_with_drop_legacy(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8, drop_data: extern "C" fn(*mut u8))`
- `rt_async_spawn_legacy(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef`
- `rt_async_spawn_deferred_legacy(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef`
- `rt_async_poll_legacy() -> bool`
- `rt_async_sleep_legacy(delay_ms: u64) -> LegacyPromiseRef`
- `rt_coro_await_legacy(coro: *mut RtCoroutineHeader, awaited: LegacyPromiseRef, next_state: u32)`

### Microtasks + timers

- `rt_async_sleep(delay_ms: u64) -> PromiseRef`
- `rt_queue_microtask(task: Microtask)`
- `rt_queue_microtask_with_drop(cb: extern "C" fn(*mut u8), data: *mut u8, drop_data: extern "C" fn(*mut u8))`
- `rt_queue_microtask_rooted(cb: extern "C" fn(*mut u8), data: *mut u8)`
- `rt_queue_microtask_rooted_h(cb: extern "C" fn(*mut u8), data: GcHandle)`
- `rt_queue_microtask_handle(cb: extern "C" fn(*mut u8), data: u64)`
- `rt_queue_microtask_handle_with_drop(cb: extern "C" fn(*mut u8), data: u64, drop_data: extern "C" fn(*mut u8))`
- `rt_drain_microtasks() -> bool`
- `rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId`
- `rt_set_timeout_with_drop(cb: extern "C" fn(*mut u8), data: *mut u8, drop_data: extern "C" fn(*mut u8), delay_ms: u64) -> TimerId`
- `rt_set_timeout_rooted(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId`
- `rt_set_timeout_rooted_h(cb: extern "C" fn(*mut u8), data: GcHandle, delay_ms: u64) -> TimerId`
- `rt_set_timeout_handle(cb: extern "C" fn(*mut u8), data: u64, delay_ms: u64) -> TimerId`
- `rt_set_timeout_handle_with_drop(cb: extern "C" fn(*mut u8), data: u64, drop_data: extern "C" fn(*mut u8), delay_ms: u64) -> TimerId`
- `rt_set_interval(cb: extern "C" fn(*mut u8), data: *mut u8, interval_ms: u64) -> TimerId`
- `rt_set_interval_with_drop(cb: extern "C" fn(*mut u8), data: *mut u8, drop_data: extern "C" fn(*mut u8), interval_ms: u64) -> TimerId`
- `rt_set_interval_rooted(cb: extern "C" fn(*mut u8), data: *mut u8, interval_ms: u64) -> TimerId`
- `rt_set_interval_rooted_h(cb: extern "C" fn(*mut u8), data: GcHandle, interval_ms: u64) -> TimerId`
- `rt_set_interval_handle(cb: extern "C" fn(*mut u8), data: u64, interval_ms: u64) -> TimerId`
- `rt_set_interval_handle_with_drop(cb: extern "C" fn(*mut u8), data: u64, drop_data: extern "C" fn(*mut u8), interval_ms: u64) -> TimerId`
- `rt_clear_timer(id: TimerId)`

### I/O readiness watchers

I/O watchers register a file descriptor with the runtime's process-global reactor and deliver
edge-triggered readiness notifications back to the async event loop thread.

Contract:

- `fd` must already be set to `O_NONBLOCK` before registration/update and must remain `O_NONBLOCK`
  for the lifetime of the registration. `runtime-native` does not implicitly modify caller-owned fd
  flags.
- `interests` must include `RT_IO_READABLE` and/or `RT_IO_WRITABLE` (it must not be 0).
- Readiness notifications are **edge-triggered**. Consumers must drain reads/writes until the
  operation returns `EAGAIN`/`WouldBlock`; otherwise the reactor may not deliver another edge.
- `rt_io_register*` returns 0 on failure; errors are not returned over the stable C ABI.
- Handle-based variants (`rt_io_register_handle*`) use persistent-handle userdata:
  - The runtime consumes `data` and treats it as a strong GC root while the watcher is registered.
  - The runtime frees the handle exactly once when the watcher is unregistered (or if registration
    fails).
  - For `rt_io_register_handle_with_drop`, `drop_data` is invoked exactly once when the watcher is
    torn down (including on registration failure), and runs before the handle is freed.
  - If `data` is stale (freed), readiness callbacks are treated as no-ops.

- `rt_io_register(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: *mut u8) -> IoWatcherId`
- `rt_io_register_with_drop(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: *mut u8, drop_data: extern "C" fn(*mut u8)) -> IoWatcherId`
- `rt_io_register_rooted(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: *mut u8) -> IoWatcherId`
- `rt_io_register_handle(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: u64) -> IoWatcherId`
- `rt_io_register_handle_with_drop(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: u64, drop_data: extern "C" fn(*mut u8)) -> IoWatcherId`
- `rt_io_register_rooted_h(fd: i32, interests: u32, cb: extern "C" fn(u32, *mut u8), data: GcHandle) -> IoWatcherId`
- `rt_io_update(id: IoWatcherId, interests: u32)`
- `rt_io_unregister(id: IoWatcherId)`

### Blocking thread pool

- `rt_spawn_blocking(task: extern "C" fn(*mut u8, LegacyPromiseRef), data: *mut u8) -> LegacyPromiseRef`

Blocking tasks execute in a GC-safe ("NativeSafe") region and must not touch the GC heap (no GC
allocations, no write barriers, and no dereferencing GC-managed pointers). There is intentionally no
rooted `rt_spawn_blocking_*` API: blocking tasks may block in syscalls or long waits and must
therefore always run NativeSafe. If GC-managed state is required, copy it out of the GC heap before
spawning the task (or resume on the event-loop thread).

`PromiseRef`, `LegacyPromiseRef`, `ValueRef`, `CoroutineId`, `CoroutineRef`, `RtCoroutineHeader`,
`TimerId`, and `IoWatcherId` are ABI-level opaque types; their layout is defined in
`include/runtime_native.h`.

## Awaiting parallel work via payload promises

CPU-bound parallel work must never block the async event-loop thread. Instead, parallel tasks return
results through a runtime promise:

- A coroutine awaits the promise (suspending via the runtime's await registration API).
- A worker thread writes the payload and settles the promise.
- Settlement wakes the awaiting coroutine back onto the async event loop.

While a `rt_parallel_spawn_promise` task is outstanding, the runtime counts the returned promise as
**external pending** work. This ensures `rt_async_poll` / `rt_async_poll_legacy` do not report the
runtime as fully idle while background worker tasks are still running (and allows the event loop to
block in the reactor wait syscall until the promise settles).

### Relevant C ABI (parallel → promise)

```c
// Allocate a new pending promise and execute `task(data, promise)` on the work-stealing pool.
// Unrooted userdata: `data` must remain valid until `task` finishes.
PromiseRef rt_parallel_spawn_promise(
  void (*task)(uint8_t* data, PromiseRef promise),
  uint8_t* data,
  PromiseLayout layout
);

// Rooted userdata: `data` must be a GC object base pointer. The runtime keeps it alive and passes the
// relocated pointer to `task`.
PromiseRef rt_parallel_spawn_promise_rooted(
  void (*task)(uint8_t* data, PromiseRef promise),
  uint8_t* data,
  PromiseLayout layout
);

// Handle-based rooted userdata (preferred under a moving GC): `data` is a pointer-to-slot (`GcHandle`)
// so the runtime can reload it after any safepoint while registering roots.
PromiseRef rt_parallel_spawn_promise_rooted_h(
  void (*task)(uint8_t* data, PromiseRef promise),
  GcHandle data,
  PromiseLayout layout
);

// Allocate a GC-managed promise (payload is inline after PromiseHeader) and execute
// `task(data, promise)` on the work-stealing pool.
//
// The promise allocation is `rt_alloc(promise_size, promise_shape)`, which means the promise
// payload may contain GC pointers that are traced/updated according to the registered shape
// descriptor.
PromiseRef rt_parallel_spawn_promise_with_shape(
  void (*task)(uint8_t* data, PromiseRef promise),
  uint8_t* data,
  size_t promise_size,
  size_t promise_align,
  RtShapeId promise_shape
);
PromiseRef rt_parallel_spawn_promise_with_shape_rooted(
  void (*task)(uint8_t* data, PromiseRef promise),
  uint8_t* data,
  size_t promise_size,
  size_t promise_align,
  RtShapeId promise_shape
);
PromiseRef rt_parallel_spawn_promise_with_shape_rooted_h(
  void (*task)(uint8_t* data, PromiseRef promise),
  GcHandle data,
  size_t promise_size,
  size_t promise_align,
  RtShapeId promise_shape
);

// Get a writable pointer to the promise payload storage.
//
// For `rt_parallel_spawn_promise` promises, this returns the out-of-line payload buffer.
// For GC-managed promises (native async ABI, including `rt_parallel_spawn_promise_with_shape`), this
// returns the inline payload pointer immediately after `PromiseHeader`.
//
// Note: for GC-managed promises, this is an interior pointer into a movable GC allocation. It is
// only valid until the next GC/safepoint; do not cache it across `MayGC` calls. Keep the promise
// itself live/rooted and reload the payload pointer after any safepoint.
uint8_t* rt_promise_payload_ptr(PromiseRef promise);

// Settle the promise (exactly once).
void rt_promise_fulfill(PromiseRef promise);
bool rt_promise_try_fulfill(PromiseRef promise);
void rt_promise_reject(PromiseRef promise);
bool rt_promise_try_reject(PromiseRef promise);
```

`PromiseLayout` is:

```c
typedef struct PromiseLayout {
  size_t size;
  size_t align;
} PromiseLayout;
```

Note: `PromiseLayout` is only used by `rt_parallel_spawn_promise{,_rooted,_rooted_h}` and describes an
**out-of-line** payload buffer treated as raw bytes (not GC-traced). For GC-traceable payloads, use
`rt_parallel_spawn_promise_with_shape{,_rooted,_rooted_h}` instead: the payload is inline in the
promise allocation and traced according to the `RtShapeId`.

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

## Model checking the waiter/wake protocol (Loom)

The promise continuation registration + wake-up protocol is a small but concurrency-sensitive
lock-free algorithm (Treiber-stack registration + drain-on-settle + waiter-side post-push recheck).

To catch lost-wakeup / double-wake bugs early, `runtime-native` includes Loom model-checking tests:

- `runtime-native/tests/loom_promise_waiters.rs`
- harness: `runtime-native/src/loom_promise_waiters.rs`

Run them with the `loom` feature enabled:

```bash
# From `vendor/ecma-rs/`:
bash scripts/cargo_llvm.sh test -p runtime-native --features loom --test loom_promise_waiters
```

## Promise settlement (thread-safe, idempotent)

Promises have a `PromiseHeader.state`:

- `PENDING`
- `FULFILLED`
- `REJECTED`

Settlement is **first-wins** and **idempotent**:

- `rt_promise_fulfill` / `rt_promise_reject` are safe to call concurrently from multiple OS
  threads (duplicate callbacks, buggy code, etc.).
- The transition from `PENDING → (FULFILLED | REJECTED)` happens exactly once using an atomic
  compare-and-swap (CAS) on `PromiseHeader.state`.
- Only the winning caller performs the reaction drain/wakeup. Losing callers are no-ops (no
  additional wakeups, no payload overwrite).

If the caller needs to know whether it won the settle race, use:

- `rt_promise_try_fulfill(...) -> bool`
- `rt_promise_try_reject(...) -> bool`

Exactly one concurrent settle call will observe `true`.

## Payload ownership

Generated promises are laid out as:

- `PromiseHeader` prefix at offset 0, followed immediately by the payload `T`.

The runtime settlement APIs do not write the payload: generated code is responsible for writing it
into the promise allocation before settling.

Because settlement is first-wins, generated code must ensure only the winning settle call writes
the payload. Losing settle calls are ignored by the runtime (no additional wakeups), but
unsynchronized payload writes are still a data race at the machine level.

## Panic / unwinding policy

All exported async runtime C ABI functions are **abort-on-panic**: if a Rust panic occurs while
executing an exported `extern "C"` runtime function, the runtime will abort the process rather than
attempting to unwind across the FFI boundary.

Likewise, **callbacks invoked by the runtime** (microtasks/macrotasks, timer callbacks, I/O watcher
callbacks, blocking-pool work items, parallel work items, thenable vtable calls, etc.) are treated as
**must-not-panic**. If a callback panics, the runtime prints a short diagnostic (including the stable
substring `runtime-native: panic in callback`) and aborts the process deterministically.

Generated code must treat runtime panics as fatal and must not assume it can recover from panics or
observe them as structured errors.

## Native coroutine execution model

### Core model (coroutines + promises)

- A **coroutine** is a native-generated state machine. The native ABI interacts with it through a
  `Coroutine` prefix placed at offset 0 of the coroutine frame.
  - Coroutine frames are normal **GC objects**: the `CoroutineRef` passed across the ABI is the
    object base pointer (start of the runtime GC header / `ObjHeader`).
  - In C, the ABI exposes this as an opaque fixed-size prefix `RtGcPrefix` at the start of
    `struct Coroutine` (see `include/runtime_native.h`).
- A coroutine produces a **result promise** (`PromiseRef`) that is returned to the JS world. The
  promise begins with a `PromiseHeader` at offset 0 (and that header embeds the GC `ObjHeader` at
  offset 0); the payload layout is owned by codegen.
  - The promise payload begins immediately after `PromiseHeader` (at offset
    `size_of::<PromiseHeader>()` from the object base).
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

### ABI versioning (`CoroutineVTable::abi_version`)

The native coroutine ABI is versioned via `RT_ASYNC_ABI_VERSION` (currently `2`).

Codegen must set `CoroutineVTable::abi_version` to exactly this value. The runtime validates the
version (and basic promise layout metadata) before dereferencing the vtable or calling into
compiler-provided function pointers.

Specifically, `rt_async_spawn` / `rt_async_spawn_deferred` validate:

- `coro` and `coro.vtable` are non-null and correctly aligned.
- `CoroutineVTable::abi_version == RT_ASYNC_ABI_VERSION`
- `CoroutineVTable::promise_size >= sizeof(PromiseHeader)`
- `CoroutineVTable::promise_align` is a power of two and `>= alignof(PromiseHeader)`
- `CoroutineVTable::reserved` is all zeros

Any validation failure is treated as a **fatal error**: the runtime calls `abort()` to fail fast and
avoid undefined behavior if the compiler and runtime evolve independently.

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

### Allocation + shape IDs

- **Promises** are allocated by the runtime via `rt_alloc(size, shape)` using:
  - `CoroutineVTable::promise_size` / `promise_align`, and
  - `CoroutineVTable::promise_shape_id` (a runtime-local `RtShapeId` index into the registered shape
    table).
- **Coroutine frames** are allocated by codegen (not the runtime). Codegen should allocate coroutine
  frames via `rt_alloc` (or `rt_alloc_pinned` when stable addresses are required) using a
  codegen-provided `RtShapeId` for the coroutine frame shape.

### Spawning APIs

#### `rt_async_spawn`

```c
PromiseRef rt_async_spawn(CoroutineId coro);
```

- Allocates/initializes the coroutine’s result promise and writes it to the coroutine frame’s
  `promise` field.
- **Immediately resumes** the coroutine during the call (until it completes or reaches its first
  `await`).
- The runtime **consumes** the coroutine handle and frees it when the coroutine completes (or is
  cancelled).

#### `rt_async_spawn_deferred` (microtask-style)

```c
PromiseRef rt_async_spawn_deferred(CoroutineId coro);
```

- Allocates/initializes the coroutine’s result promise and writes it to the coroutine frame’s
  `promise` field (same as `rt_async_spawn`).
- Enqueues the coroutine’s *first resume* as a **microtask**.
- **Does not resume the coroutine synchronously**. The first resume happens later when the host runs
  the runtime (e.g. `rt_async_poll`, or a microtask-only checkpoint via `rt_drain_microtasks` /
  `rt_async_run_until_idle`).
- The runtime **consumes** the coroutine handle and frees it when the coroutine completes (or is
  cancelled).

This API exists for Web-standard semantics that require guaranteed asynchronous execution, including:

- `queueMicrotask`
- Promise job scheduling (ECMA-262 `HostEnqueuePromiseJob`, HTML microtask queue)
- Strict `await` semantics where reaching the first `await` must be asynchronous

### Why `CoroutineId` (handle) instead of `CoroutineRef` (pointer)?

Coroutines are long-lived and may be:
- captured in promise reactions (to resume later),
- stored in host work queues,
- stored in OS event-loop userdata, and
- moved between threads.

Under a moving/compacting GC, coroutine frames may relocate. Raw pointers (`CoroutineRef`) cannot be
stored safely across these async boundaries because the Rust runtime itself is not compiled with LLVM
statepoints/stackmaps and therefore cannot have its raw pointers auto-relocated during GC.

Instead the ABI uses a stable `CoroutineId` (`u64`) handle. The runtime resolves the ID to the
current coroutine pointer each time it needs to resume, and treats invalid/stale IDs as a no-op
resume (never UB).

`CoroutineId` is currently backed by the same persistent handle table as `HandleId`, allocated via
`rt_handle_alloc` and freed via `rt_handle_free`.

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
  the runtime (e.g. `rt_async_poll` / `rt_async_poll_legacy`, or a microtask-only checkpoint via
  `rt_drain_microtasks` / `rt_async_run_until_idle`).

This API exists for Web-standard semantics that require guaranteed asynchronous execution, including:

- `queueMicrotask`
- Promise job scheduling (ECMA-262 `HostEnqueuePromiseJob`, HTML microtask queue)
- Strict `await` semantics where reaching the first `await` must be asynchronous

### Driving the runtime: `rt_async_poll` / `rt_async_poll_legacy`

```c
bool rt_async_poll(void);
bool rt_async_poll_legacy(void);
```

`rt_async_poll_legacy` is a compatibility alias for `rt_async_poll` (identical behavior).

Drives the full event loop for one turn:

- Executes at most one macrotask (timer/I/O/etc), then performs a microtask checkpoint.
- If there are no macrotasks, it drains microtasks directly.
- Blocks in the platform reactor wait syscall (`epoll_wait`/`kevent`) when there is no ready work but
  there are pending I/O watchers, timers, or outstanding external work (e.g. a `rt_parallel_spawn_promise`
  task that has not yet settled its promise).

The return value indicates whether there is still pending work (timers, I/O watchers, microtasks,
macrotasks, or outstanding external work) after the turn.

## Unhandled promise rejections

The runtime tracks unhandled rejections in a JS/HTML-shaped way:

- When a promise is rejected while it has no rejection handlers, it is eligible to be reported as an
  `unhandledrejection` at a microtask checkpoint.
- If a previously-unhandled rejected promise later becomes handled, it is eligible to be reported as
  `rejectionhandled`.

### Marking promises as handled

The runtime uses `PromiseHeader.flags` bit 0 (`PromiseHeader::FLAG_HANDLED` on the Rust side) to track
whether a promise has at least one rejection handler attached.

- The runtime automatically sets this flag when it attaches internal reactions (e.g. via `await`).
- Hosts/embedders that attach external handlers (e.g. JS `then`/`catch`) must call
  `rt_promise_mark_handled(p)` to set the flag and trigger `rejectionhandled` behavior when
  appropriate.

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

## GC constraints

The runtime uses a precise GC. To keep tracing correct and simple, the async ABI must follow these
rules.

### No tagged/sentinel pointers in pointer fields

Any **pointer-typed** field (e.g. `Coroutine.vtable`, `Coroutine.promise`, `Coroutine.next_waiter`)
must contain:

- `null`, or
- a properly aligned pointer to a valid allocation (heap object, stack/root record, or static data).

Do not store tagged pointers (low-bit tagging) or non-null sentinel integers in pointer fields.

**Note:** `PromiseHeader.waiters` stores reaction-node pointers as `usize`. This is intentional: it
is not a pointer field, so the GC (or any tracer) must treat it explicitly when walking promise
metadata.

### Rooting rules

- A suspended coroutine frame must be considered live (GC-rooted) while:
  - it is referenced by any pending promise’s reaction list, or
  - it is in the scheduler’s queues.
- Promise payloads may contain GC pointers. Codegen/runtime must ensure those pointers are traced by
  the promise’s shape descriptor (`promise_shape_id`).

## Spec deviations / supported behavior

This project supports both behaviors for `await`:

- **Fast-path (default):** if the awaited promise is already settled, the continuation may run
  synchronously without forcing a microtask turn.
- **Strict yield mode:** always yield on `await`, matching ECMAScript’s microtask timing.

The runtime exposes this as a configuration knob (`set_strict_await_yields` in `runtime-native`).

## Rejection tracking (implementation notes)

Unhandled rejection reporting is processed at the **end of a microtask checkpoint**, matching the
shape of HTML’s algorithms:

- rejecting an unhandled promise adds it to an internal “about-to-be-notified” list
- after draining microtasks, the runtime promotes remaining entries to the “unhandled” set
- attaching a handler after that promotion produces a “rejection handled” notification

The Rust test harness can observe these events via:

- `runtime_native::test_util::drain_promise_rejection_events()` (legacy promises), and
- `runtime_native::promise_api::{rt_take_unhandled_rejections, rt_take_rejection_handled}` (promise_api promises).

## Parallelism

Parallelism is expressed explicitly via:

- `rt_parallel_spawn` / `rt_parallel_join`
- `rt_parallel_spawn_promise`
- `rt_spawn_blocking`

It is **not** achieved by running the async driver on multiple threads.
