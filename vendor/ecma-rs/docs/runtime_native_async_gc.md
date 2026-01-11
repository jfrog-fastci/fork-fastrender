# GC integration for the runtime-native async runtime

This document describes the minimum GC-facing invariants required for the **runtime-native async
runtime** (event loop + suspended coroutines + Promise jobs) described in
[`vendor/ecma-rs/EXEC.plan.md`](../EXEC.plan.md) (§5.5 “Async Runtime”).

The target is a **moving, precise GC** (compacting / generational) where compiled JS/TS uses LLVM
statepoints, while runtime-native code (Rust) participates via explicit rooting APIs.

## Why async lowering must be stackless (heap frame)

`EXEC.plan.md` commits us to “**stackless coroutines + event loop**” with “state stored in a
heap-allocated frame”.

This is not just an implementation preference; it is a GC requirement:

* **Stackful async (fibers / green threads)** implies there are *parked stacks* containing GC
  references while the fiber is suspended.
  * A moving GC would need to either **scan and update every parked stack** during collection, or
    prohibit movement/pinning for any object referenced by parked stacks.
  * Both approaches are expensive and complicate safepoints (parked stacks are not “at a safepoint”
    in the statepoint sense).
* **Stackless async** lowers each `async fn` into a state machine whose live locals are stored in a
  **heap coroutine frame**.
  * When the coroutine suspends, its native stack unwinds completely (no parked stack).
  * The suspended coroutine is now just another heap object reachable from explicit roots / other
    heap objects, so the GC treats it like ordinary graph data.

**Result:** GC work scales with the number of *threads* (active stacks to scan) and the number of
*heap objects*, not with the number/size of suspended stacks.

## Moving vs pinning: what is allowed to move

**Explicit rule:** **coroutine frames and Promise objects are movable GC objects by default.**

Implications:

* Their addresses are not stable across `rt_gc_safepoint` / allocation.
* No runtime subsystem may store a raw pointer to a frame/Promise in memory that outlives a
  safepoint, unless that pointer is itself treated as a GC root and updated by the GC.

### What must be stable/pinned

The *only* things that must be stable across collections are **handles** used by:

1. **OS event loop userdata** (e.g. `epoll_event.data.u64`, `kqueue udata`)
2. **Cross-thread wakeups** (threadpool → scheduler → main loop)

These stable handles must be **non-GC memory** (or otherwise pinned) and must not directly encode
raw GC pointers. They are typically:

* a small integer `HandleId` (possibly with a generation counter), or
* a pointer to a pinned “token” struct that contains a `HandleId` (useful when the OS API only
  supports `void*`).

## Root sets for async execution

Moving GC requires a complete inventory of roots that can keep suspended work alive and allow the GC
to update references.

### 1) Per-thread shadow roots (runtime-native Rust code)

Runtime-native code (event loop, scheduler, I/O drivers) is not compiled with LLVM GC statepoints.
Therefore it must expose its live GC references explicitly.

**Expectation:** each runtime thread maintains a **shadow root stack** that holds either:

* direct GC pointers stored in GC-updatable slots, or
* handle IDs that the GC can enumerate as roots.

The important property is: **if Rust code can observe a GC object after a safepoint, it must have a
rooted representation of it.**

### 2) Persistent handle table entries (queued/suspended work)

Any work item that can outlive the current call stack needs a persistent root.

Examples:

* ready/run queues of runnable coroutines
* timer wheel entries
* epoll/kqueue registrations (I/O waiters)
* cross-thread completion queues (work-stealing / blocking pool)

**Design point:** queue entries should hold **stable handle IDs**, not raw pointers to movable GC
objects. A handle table entry acts like `vm-js`’s persistent `RootId`: it keeps the referenced heap
object alive and is updated when the GC moves objects.

### 3) (Optional) Runtime structures storing raw pointers

This design should generally be avoided, but if a subsystem chooses to store raw pointers to GC
objects (e.g. for performance), then:

* those pointers must live in a GC-enumerated root structure, **and**
* the GC must update them during compaction (exactly like stack roots).

If the pointer cannot be updated, the pointed-to object must be pinned — which conflicts with the
“movable by default” rule above and should be treated as an exceptional escape hatch.

## Discard / cancellation rules (no implicit dropping)

Async runtimes frequently have “give up” paths:

* deadline exceeded / termination
* queue overflows
* shutdown
* cancellation via API (`AbortSignal`, `clearTimeout`, etc.)

GC-safety requires that **queued work owning handles is never dropped implicitly**.

### Rule: if a queue entry owns handles, it must have explicit teardown

Any queued/suspended work item that owns handles must support:

* `run(self)` — consumes and executes work, and releases owned handles
* `discard(self)` — consumes without executing, but still releases owned handles

This mirrors the existing `vm-js` pattern:

* [`vm-js/src/jobs.rs`](../vm-js/src/jobs.rs): `Job::run(..)` and `Job::discard(..)` both clean up
  persistent roots, and `Drop` asserts that roots were not leaked.
* [`vm-js/src/microtasks.rs`](../vm-js/src/microtasks.rs): termination aborts the checkpoint but
  **tears down** remaining jobs so roots are cleaned up.

FastRender has an analogous queue-integrity rule:

* [`src/js/event_loop.rs`](../../../src/js/event_loop.rs) checks deadlines **before popping**
  microtasks/tasks so that “stop” does not accidentally drop queued jobs (see the inline
  `IMPORTANT: check before popping` comments).

### Shutdown/termination must drain + free handles

On shutdown/termination, the runtime must:

1. stop producing new work,
2. **drain/teardown** every queue (ready queue, timers, I/O waiters, cross-thread queues),
3. unregister OS resources (epoll/kqueue fds, timerfds, etc.),
4. free all handle table entries that were rooting queued/suspended work.

Do not rely on Rust `Drop` of queue containers to “eventually” clean up: dropping without calling
`discard` is exactly how handle leaks happen.

## Derived pointer guidance (moving GC)

Avoid storing **interior/derived pointers** (e.g. `base + offset`) inside heap objects or in
long-lived runtime structures.

Instead:

* store the **base** reference as a GC pointer/handle, plus an **offset** (byte offset or index),
* re-derive the interior pointer at the use site **after** loading/rooting the base, and
* ensure no safepoint can occur between deriving and using the pointer (or simply re-derive after
  every safepoint).

This rule applies equally to coroutine frames (which may hold references into other objects) and to
runtime-native metadata structs.

## Pseudocode: one `await` suspension (handle creation + free)

Below is a representative pattern for a single suspension point in a stackless coroutine. The key
idea: **create a stable handle before registering with the OS/threadpool**, and **free it on resume
or discard**.

```text
// Types (conceptual):
//   GcPtr<T>     - movable pointer to a GC object **base** (points at `ObjHeader`, not payload; invalid after a GC unless reloaded)
//   HandleId     - stable ID for a handle table entry (safe to store in OS userdata)
//   Frame        - heap coroutine frame holding locals + state machine PC

fn poll_frame(frame: GcPtr<Frame>) -> Poll<Result> {
  match frame.pc {
    0 => {
      // Start an async operation that will complete later (I/O, timer, threadpool).
      let op = start_io_operation(...);

      // IMPORTANT: Root the frame *persistently* before parking.
      // This handle table entry is part of the GC root set for queued/suspended work.
      let h: HandleId = rt_handle_alloc(frame);
      frame.wait_handle = h;

      // Register completion with the OS/event-loop, using *only* the stable handle.
      // epoll/kqueue userdata == HandleId (or token containing it).
      os_register(op, userdata = h);

      frame.pc = 1;
      rt_async_park(h);   // "this coroutine is now suspended"
      return Pending;
    }

    1 => {
      // We were woken by the event loop with the same HandleId.
      // Reload the (possibly moved) frame pointer via the handle table.
      let frame: GcPtr<Frame> = rt_handle_load(frame.wait_handle);

      // Tear down the persistent root now that we're running again.
      // (If we suspend again, we'll allocate a new handle or reuse a slot.)
      rt_handle_free(frame.wait_handle);
      frame.wait_handle = INVALID;

      let result = take_io_result(frame);
      frame.pc = 2;
      return Ready(result);
    }
  }
}

// OS completion path (runs on event loop thread or threadpool):
fn on_os_event(userdata: HandleId) {
  // Enqueue resumption by stable handle ID; do not dereference GC pointers here.
  rt_async_wake(userdata);
}

// Cancellation path (deadline/shutdown):
fn discard_waiting(frame_handle: HandleId) {
  os_unregister(userdata = frame_handle);  // must happen before freeing handle
  rt_handle_free(frame_handle);
}
```

Notes:

* `rt_handle_load` is shown for clarity; an implementation may inline the table lookup.
* If the OS can deliver stale events after `os_unregister`, use generations or explicit “armed”
  flags in the waiter to ignore events for freed handles.

## ABI hooks needed (minimum contract)

Names are provisional; the important part is the behavior.

### `rt_gc_safepoint()`

* Called by mutator threads at compiler-inserted safepoints (or allocation slow paths).
* Coordinates “stop-the-world” (or STW phases) so that:
  * all threads reach a known point, and
  * the GC can enumerate roots (LLVM stack maps for compiled code + shadow roots + handle tables).

### Handle table API

Handles provide two things:

1. **Rooting**: a handle entry can keep an object alive while it is stored in host-owned queues.
2. **Indirection for moving GC**: the GC updates the handle’s referent when objects move.

Minimum operations:

* `rt_handle_alloc(obj: GcPtr<T>) -> HandleId`  
  Allocates a persistent handle table entry that roots `obj`.
* `rt_handle_free(id: HandleId)`  
  Releases the handle table entry (no longer roots the object).
* `rt_handle_update(id: HandleId, new_obj: GcPtr<T>)` (or equivalent internal mechanism)  
  Used by the GC to update handle referents after moving/forwarding.

In practice, a usable ABI usually also needs:

* `rt_handle_load(id: HandleId) -> GcPtr<T>`
* `rt_handle_store(id: HandleId, obj: GcPtr<T>)`

### Current implementation note

`runtime-native` implements `rt_handle_*` using the process-global [`RootRegistry`](../runtime-native/src/roots/registry.rs):

* `rt_handle_alloc` allocates a rooted slot (equivalent to `rt_gc_pin`) and returns a `u64` handle ID.
* `rt_handle_load`/`rt_handle_store` load/store through that slot (GC updates the slot during
  relocation).
* `rt_handle_free` unregisters the slot.

Handle values are returned as `u64` for easy storage in OS userdata fields, but are currently encoded
using the registry’s existing 32-bit `{ index, generation }` scheme widened to `u64`.

### Async park/wake API

These APIs are the boundary where *stable* identifiers cross into the OS or other threads.

* `rt_async_park(task: HandleId)`  
  Declares that `task` is no longer runnable and will be resumed only via `rt_async_wake`.
* `rt_async_wake(task: HandleId)`  
  Enqueues the coroutine identified by `task` onto a runnable queue (or equivalent), eventually
  causing `poll_frame(..)` to run again.

**Constraint:** anything crossing into epoll/kqueue or cross-thread channels must be a `HandleId`
(or token containing it), never a raw `GcPtr`.

## Analogous patterns in existing code

* `vm-js` persistent-rooted queued work:
  * [`vendor/ecma-rs/vm-js/src/jobs.rs`](../vm-js/src/jobs.rs)
  * [`vendor/ecma-rs/vm-js/src/promise_jobs.rs`](../vm-js/src/promise_jobs.rs)
  * [`vendor/ecma-rs/vm-js/src/microtasks.rs`](../vm-js/src/microtasks.rs)
* FastRender queue integrity / deadline-before-pop:
  * [`src/js/event_loop.rs`](../../../src/js/event_loop.rs)
