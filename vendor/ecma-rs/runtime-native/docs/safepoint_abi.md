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
* `rt_thread_unregister` may **block** during an active stop-the-world epoch. A thread is not
  removed from the global mutator set while `RT_GC_EPOCH` is odd. Callers must not hold runtime
  locks while unregistering.

## `rt_thread_attach` / `rt_thread_detach`

`runtime-native` also exposes a per-`Runtime` attach/detach API:

```c
Thread* rt_thread_attach(Runtime* runtime);
void rt_thread_detach(Thread* thread);
```

This API exists for embedder compatibility and for creating the per-thread `RT_THREAD` TLS record
used by older native codegen prototypes.

**Canonical mutator contract:** stop-the-world safepoints and GC root enumeration use the **global**
thread registry described above. As a result:

* `rt_thread_attach` will ensure the calling OS thread is registered in the global registry
  (as `External`) if it is not already registered.
* If the thread was already registered (e.g. via `rt_thread_register` / `rt_thread_init`),
  `rt_thread_attach` is idempotent with respect to the global registry (no double-register).
* `rt_thread_detach` will unregister the thread from the global registry **only if**
  `rt_thread_attach` performed the registration.

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

## Safepoint poll ABI (stop-the-world protocol)

The runtime coordinates stop-the-world GC using an exported global epoch:

```c
// Declared in `runtime-native/include/runtime_native.h`.
extern _Atomic uint64_t RT_GC_EPOCH;
```

Epoch semantics:

* **even**: no stop-the-world GC requested
* **odd**: stop-the-world GC requested

### Recommended poll pattern for compiler-generated code

The recommended safepoint poll pattern for compiler-generated code is:

1. Inline poll: load `RT_GC_EPOCH` with **Acquire** ordering.
2. If the observed epoch is odd, call `rt_gc_safepoint_slow(epoch)` (passing the observed odd epoch).

In pseudocode:

```c
uint64_t epoch = RT_GC_EPOCH; // load (Acquire)
if (epoch & 1) {
  rt_gc_safepoint_slow(epoch);
}
```

This ensures that:

* the fast path is a load+branch, and
* the slow-path call can be rewritten into an LLVM statepoint at the *managed* callsite so stackmaps
  and published safepoint context line up.

`rt_gc_safepoint()` is a convenience wrapper that performs the same inline poll + slow-path call. It
is useful for runtime/embedding code, but compiler-generated code should prefer the inline poll.

### `gc.safepoint_poll` (LLVM `place-safepoints`)

LLVM’s `place-safepoints` pass inserts calls to a symbol named:

```llvm
declare void @gc.safepoint_poll()
```

`runtime-native` provides this symbol. It is implemented in per-architecture assembly so it can
inline the `RT_GC_EPOCH` load on the fast path and capture the managed caller context on the slow
path (so stackmap return PCs match).

## Stack walking invariants (statepoint stackmaps)

The stop-the-world GC needs to enumerate GC roots precisely. `runtime-native` uses LLVM **statepoint**
stackmaps plus a first-milestone **frame-pointer-based** stack walker to do this.

This requires a stable frame chain across *both* generated code and runtime-native code:

* LLVM-generated code must keep frame pointers and avoid tail calls (`frame-pointer="all"`,
  `disable-tail-calls="true"`; see `native-js/docs/gc_stack_walking.md`).
* The Rust runtime (and any other Rust code that can run on GC-managed threads) must be compiled with
  frame pointers enabled (`-C force-frame-pointers=yes`).

In this repository, the wrapper scripts automatically inject the Rust flag:

```bash
# From the monorepo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p runtime-native

# Or from vendor/ecma-rs:
bash scripts/cargo_llvm.sh test -p runtime-native
```

### Retaining `.llvm_stackmaps` in the final binary (Linux)

On Linux, linkers may discard `.llvm_stackmaps` under `--gc-sections` unless it is explicitly kept.

The final link step should apply `runtime-native/link/stackmaps.ld` (or the compatibility alias
`runtime-native/stackmaps.ld`) to:
 
* `KEEP` the stackmap section
* and define stable in-memory boundary symbols (`__start_llvm_stackmaps` / `__stop_llvm_stackmaps`)

This allows the runtime to load stackmaps without scanning memory or parsing `/proc/self/exe`.

Hardening notes:

* `runtime-native/link/stackmaps.ld` is written for lld/GNU ld linker scripts (it uses `SECTIONS` +
  `INSERT`). Linkers like **mold** do not support these scripts; use GNU ld or lld (e.g. `-fuse-ld=lld-18`)
  when injecting the fragment.
* For **GNU ld** in **PIE/DSO** mode, inserting the stackmaps output section “after `.text`” can
  place it in the same LOAD segment as `.text`, producing an RWX segment once the stackmaps need to
  be writable for relocations. Use `runtime-native/link/stackmaps_gnuld.ld` (or the
  `scripts/native_link.sh` helper, which selects the appropriate fragment) to keep stackmaps in the
  RELRO/data region without RWX.

Notes:

* Enabling the `runtime-native` crate feature `llvm_stackmaps_linker` causes `runtime-native/build.rs`
  to pass the linker script when linking artifacts produced by the `runtime-native` package itself
  (tests / cdylib on Linux).
* Cargo does **not** automatically propagate linker-script args from dependencies into downstream Rust
  binaries. For executables that *depend on* `runtime-native`, you must still pass the linker script
  at the final link step (e.g. via `RUSTFLAGS`), or use helpers like `native_js::link` /
  `scripts/native_link.sh` which always inject it.
