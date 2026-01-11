# ADR 0360: I/O buffers and GC-safe blocking syscalls

## Context

`runtime-native` is planned to use a **moving, precise** GC (LLVM statepoints + stack maps).
In such a collector, *all runtime threads that may touch the GC heap* must reach safepoints so the
GC can scan and relocate pointers precisely.

The async runtime introduces threads that may block in the kernel for an unbounded time:

- `epoll_wait`
- `io_uring_enter(..., IORING_ENTER_GETEVENTS, ...)`
- blocking file I/O (`read`, `write`, ...)

If a thread blocks while still holding GC pointers on its stack/registers, an STW GC can deadlock
waiting for that thread, or relocate objects without updating its stale pointers (memory
corruption).

This is especially important for I/O ops which hold:

- **Pinned buffers** (memory used directly by the kernel/DMA and therefore not movable)
- Promise/coroutine references needed to resume tasks when completions arrive

## Decision

`runtime-native` enforces the following rule:

> **Any thread that may block in the kernel for an unbounded time must not hold raw GC pointers on
> its stack across the blocking call boundary.**

Concretely:

1. Threads are classified as either:
   - **Mutators**: execute GC-aware code and may hold GC pointers in registers/stack at normal
     safepoints.
   - **Blocking/runtime threads**: may block in the kernel for long periods. These threads must
     ensure they do **not** keep raw GC pointers live across the blocking syscall boundary.

2. Blocking syscalls must be wrapped in an explicit "not-a-mutator" transition.
   In `runtime-native` today this is represented by entering a
   `runtime_native::threading::ParkedGuard` (or using `threading::park_while(..)`), which marks the
   thread as parked immediately before the blocking syscall.

   When a thread is parked, the safepoint coordinator treats it as already quiescent; therefore it
   is a bug for the thread to have any live, untracked GC pointers at this boundary. We enforce this
   with debug-time assertions (see `threading::set_parked`).

3. If a blocking operation must retain references to GC-managed objects while it is in flight, it
   must store them **only as stable handles** in the global handle/root tables (Task 370), e.g.:
   - `runtime_native::gc::RootHandle` / `runtime_native::gc::RootHandles`
   - `runtime_native::roots::RootRegistry` (global root slots)

   This keeps raw GC pointers out of syscall registers/stack while still allowing the GC to update
   roots during relocation.

4. STW GC wakeups:
   - The reactor/event-loop installs a wakeup callback via `threading::register_reactor_waker`.
   - The GC coordinator invokes registered wakers when requesting stop-the-world, waking threads
     blocked in `epoll_wait` / `io_uring_enter(GETEVENTS)`.
   - On Linux, wakeups are implemented with `eventfd` (see `runtime-native` reactor docs).

## Consequences

- STW GC progress does not depend on kernel-blocked threads returning naturally.
- The runtime must treat "blocking while holding GC pointers" as a bug (debug assertion/panic).
- I/O op state structs should store GC references as stable handles (not raw pointers) and pinned
  buffers as explicit pinned allocations.
