# Safepoint + thread registry ABI

`runtime-native` uses a global **mutator thread registry** to coordinate stop-the-world (STW)
GC safepoints across multiple OS threads.

LLVM-generated code (and any embedding code that calls into generated code) must follow the
contracts below.

## Thread registration

Every OS thread that may execute managed code must register itself with the runtime:

```c
uint64_t rt_thread_register(uint32_t kind);
void rt_thread_unregister(void);
```

`rt_thread_register` is **idempotent** for the current OS thread and returns a stable runtime
thread id for the lifetime of the registration.

`kind` values (stable ABI):

| `kind` | Meaning |
|--------|---------|
| 0 | Main |
| 1 | Worker |
| 2 | Io |
| 3 | External |

Notes:

* Registration is required so the GC can stop and (in the future) precisely scan mutator stacks.
* Threads must unregister before exiting; otherwise the registry will retain stale entries.

## `parked` semantics

The runtime may mark a registered thread as **parked** while it is idle and blocked inside the
runtime scheduler:

```c
void rt_thread_set_parked(bool parked);
```

When `parked == true`, the STW coordinator treats the thread as already quiescent and does not
need it to actively poll a safepoint.

### Critical invariant

The runtime must only set `parked == true` at a safepoint where the thread's stack contains no
untracked GC pointers.

### Mandatory poll on unpark

When transitioning back to `parked == false` (unparking), `rt_thread_set_parked(false)` performs
a safepoint poll before returning (fast path if no STW is requested). This prevents a parked
thread from waking up in the middle of an active STW and running mutator work without observing
the stop epoch.

Because unparking can block on an in-progress STW, callers should avoid holding runtime locks
while calling `rt_thread_set_parked(false)`.

