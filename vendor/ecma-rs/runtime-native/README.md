# runtime-native

Native runtime library used by LLVM-generated code (planned `native-js` backend).

This crate provides the runtime-side pieces of the compiler/runtime ABI contract:
allocator entrypoints, string helpers, GC safepoints, and the **minimal async/await runtime ABI**
needed to execute LLVM-generated coroutine state machines with JS-correct microtask ordering.

See also:
* `include/runtime_native.h` — stable C ABI surface
* `docs/safepoint_abi.md` — thread registration + parked/unparked safepoint protocol
* `docs/reactor.md` — cross-platform reactor contract (epoll/kqueue)
* `docs/write_barrier.md` — generational write barrier contract

## Frame-pointer ABI contract

The GC/stack map runtime walks the stack using a *frame-pointer chain* (e.g. `rbp`
on x86_64). This is intentionally simple and fast, but it only works if both:

1. **Rust runtime code** (`runtime-native`) is compiled with frame pointers.
2. **LLVM-generated managed code** is compiled with frame pointers.

### Enforcement (Rust / runtime-native)

`runtime-native` must be compiled with:

```bash
-C force-frame-pointers=yes
```

This repo enforces that in two ways:

- `scripts/cargo_llvm.sh` appends the required `RUSTFLAGS`.
- `runtime-native/build.rs` fails the build if the flag is missing.

### Enforcement (LLVM managed code)

When invoking `llc` directly, **always** compile with:

```bash
--frame-pointer=all
```

This repo provides `scripts/llc_fp.sh`, a tiny wrapper that injects
`--frame-pointer=all` unless already specified.

If/when the pipeline switches to LLVM APIs (TargetMachine) instead of invoking
`llc`, the equivalent requirement is: disable frame pointer elimination via
TargetOptions / TargetMachine settings.

### Regression tests

`runtime-native/tests/frame_pointers.rs` builds optimized objects and asserts
that the expected frame-pointer prologue exists (x86_64 host).

## Reactor contract (epoll + kqueue)

The low-level cross-platform reactor contract is documented in `docs/reactor.md` and enforced by
`tests/reactor_conformance.rs`.

Key guarantees include:

- **edge-triggered** readiness on all platforms (epoll `EPOLLET`, kqueue `EV_CLEAR`)
- **at most one event per token per poll**, with read+write readiness merged when both are observed
- a cross-thread `Waker` that interrupts a blocking poll

## Panic policy (FFI safety)

**Rust panics must never unwind across an `extern "C"` boundary.** Unwinding across FFI is
undefined behaviour and can be miscompiled by LLVM.

`runtime-native` uses an **abort-on-panic** policy:

- All `#[no_mangle] extern "C" fn rt_*` exports are guarded; if a panic occurs while executing an
  export, the process is aborted.
- All internal dispatch sites that invoke embedder-provided callbacks (microtasks, timers, parallel
  tasks, blocking tasks, thenables, etc.) are guarded. If a callback panics, the runtime prints a
  short diagnostic containing the stable substring `runtime-native: panic in callback` and aborts.

### Implications for embedders

- Any panic inside `runtime-native` (including a panic originating from Rust code invoked via a
  callback/task function pointer) is treated as a **fatal runtime bug** and aborts the process.
- If you need recoverable error handling, do not use panics; plumb errors through explicit return
  values and handles instead.
---

## GC heap (overview)

The GC is a **precise**, **generational** collector with multiple spaces:

- **Nursery (young generation)**: bump-pointer allocation; collected with a copying *minor GC*
  (evacuation).
- **Immix (old generation)**: block/line based allocator designed to reduce fragmentation. Major
  collections use a mark-region style algorithm over Immix blocks.
- **LOS (large object space)**: large objects are allocated separately and swept during major GC.

The exact collection policy can evolve, but the **object representation is stable** and is the
source of truth for all GC subsystems and for native codegen.

## Process-global GC configuration (`rt_alloc*` / `rt_gc_collect`)

The exported allocator/collector (`rt_alloc`, `rt_alloc_array`, `rt_gc_collect`, etc.) operate on a
single **process-global** GC heap.

Embedders (and tests) can tune GC policy and sizing before first use:

- `rt_gc_set_config(&cfg)` sets collection policy / thresholds.
- `rt_gc_set_limits(&limits)` sets hard caps (`max_heap_bytes`, `max_total_bytes`).

**Important:** these setters must be called **before** the heap is initialized (i.e. before
`rt_thread_init`, any `rt_alloc*`, `rt_gc_collect`, etc.). If called after initialization, they
return `false` and have no effect.

For debugging, `rt_gc_get_config` / `rt_gc_get_limits` snapshot the current effective settings.

### Environment variable overrides

The process-global heap reads these environment variables **once** at heap initialization time
(integer MiB values):

- `ECMA_RS_GC_NURSERY_MB`
- `ECMA_RS_GC_MAX_HEAP_MB`
- `ECMA_RS_GC_MAX_TOTAL_MB`

Env overrides apply only when the corresponding `rt_gc_set_*` setter was not called (i.e. they
override defaults, not explicit embedder-provided config).

## GC object representation (authoritative)

### Object pointer convention

**A GC object reference (`GcPtr` / `*mut u8`) points to the start of the object header (`gc::ObjHeader`).**

This address is the **object base** for:

- `TypeDescriptor` pointer offsets (field locations)
- nursery/Immix/LOS membership checks
- forwarding pointer installation during evacuation / compaction

For fixed-size objects, the payload begins immediately after the header (`gc::OBJ_HEADER_SIZE` bytes).

```text
obj (= ObjHeader ptr)
  │
  ▼
  ┌─────────────────────────────── allocation ───────────────────────────────┐
  │ ObjHeader (2 words) │                    payload ...                      │
  │   word0: TypeDescriptor*                                                   │
  │   word1: meta (flags + card-table ptr OR forwarding ptr)                   │
  └─────────────────────┴─────────────────────────────────────────────────────┘
```

### Header layout contract (`ObjHeader`)

The header is exactly two machine words (`usize` on 64-bit):

- **Word 0 (`type_desc`)**: pointer to the object's `TypeDescriptor`.
- **Word 1 (`meta`)**: an atomic word that stores low-bit GC flags plus either:
  - an optional per-object card table pointer (non-forwarded objects), or
  - a tagged forwarding pointer (forwarded objects).

#### Evacuation state machine (`meta` dual-use)

During nursery evacuation (and optional major compaction), the `meta` word is temporarily repurposed as a
tagged forwarding pointer:

1. Read the header and check the `FORWARDED` bit.
2. If forwarded, read the new object base pointer from `meta` and return it.
3. Otherwise compute the object size:
   - fixed-size objects: `TypeDescriptor.size`
   - arrays: derived from the `RtArrayHeader` fields
4. Allocate + copy the object to the new location.
5. Overwrite `meta` with the new object base pointer and set the `FORWARDED` bit.

After step (5), the `meta` word must only be interpreted as a forwarding pointer.

### Flag semantics

Flags are stored in the low bits of `ObjHeader.meta`.

Current flags:

- `FORWARDED`: `meta` is a tagged forwarding pointer.
- `MARK_EPOCH`: 1-bit epoch used by major GC marking (toggled each major GC).
- `REMEMBERED`: object is in the remembered set (old→young edges).
- `PINNED`: object must not be moved (currently allocated in LOS).

When `FORWARDED` is set, only the forwarding-pointer interpretation is valid; other flags and any
card-table pointer bits are not meaningful.

### Variable-size objects: `RtArrayHeader`

The runtime has one dynamic-size object kind today: `RtArrayHeader` (see `src/array.rs`).

- The object base pointer points to the start of `RtArrayHeader`, which begins with an `ObjHeader`
  prefix at offset 0.
- The header's type descriptor pointer is `array::RT_ARRAY_TYPE_DESC`.
- The element payload begins at `RT_ARRAY_DATA_OFFSET` and is `len * elem_size` bytes, where `len`,
  `elem_size`, and `elem_flags` live in the `RtArrayHeader` fields.
- If `elem_flags` indicates pointer elements (`RT_ARRAY_FLAG_PTR_ELEMS`), the GC traces and updates
  the element slots. Otherwise the payload is treated as raw bytes (no interior pointers).

Total size is `RT_ARRAY_DATA_OFFSET + len * elem_size` (with overflow checks).

## Build (static library)

From the `vendor/ecma-rs/` workspace root:

```bash
bash scripts/cargo_llvm.sh build --release -p runtime-native
```

From the superproject repo root (or any cwd):

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p runtime-native --release
```

If you don't have LLVM 18 installed, you can still build by providing the required
rustc flag directly (the `scripts/cargo_agent.sh` wrapper injects it automatically):

```bash
bash scripts/cargo_agent.sh build --release -p runtime-native
```

Or via the helper script (prints include/lib paths for downstream build systems and
ensures frame pointers are enabled):

```bash
bash scripts/build_runtime_native.sh
```

Expected artifacts:

- Static library: `target/release/libruntime_native.a`
- C header: `runtime-native/include/runtime_native.h`

## Link from C / clang

The stackmaps linker-script fragment you need depends on whether you're producing **PIE** output and
which linker your C toolchain drives:

- non-PIE executables (`-no-pie`, default policy): `runtime-native/link/stackmaps_nopie.ld`
- PIE executables / DSOs (`-pie`):
  - lld: `runtime-native/link/stackmaps.ld`
  - GNU ld: `runtime-native/link/stackmaps_gnuld.ld`

To check which linker `cc` uses:

```bash
cc -Wl,--version
```

Example (non-PIE, lld; explicitly selected; from the workspace root):

```bash
clang-18 -fuse-ld=lld-18 \
  -no-pie \
  -I runtime-native/include \
  -Wl,-T,runtime-native/link/stackmaps_nopie.ld \
  /path/to/program.c \
  target/release/libruntime_native.a \
  -o program
```

Example (PIE, GNU ld; from the workspace root):

```bash
cc -std=c99 \
  -pie \
  -I runtime-native/include \
  -Wl,-T,runtime-native/link/stackmaps_gnuld.ld \
  /path/to/program.c \
  target/release/libruntime_native.a \
  -o program
```

If your program uses LLVM statepoints / stackmaps (i.e. it contains an
in-memory `.llvm_stackmaps` section) and you want the runtime to be able to
locate it for stack walking, you must also export stackmap boundary symbols, e.g.:

- `__start_llvm_stackmaps` / `__stop_llvm_stackmaps`
- `__stackmaps_start` / `__stackmaps_end` (generic alias used by tooling)

Note: when linking multiple object files that each contain `.llvm_stackmaps`,
ELF linkers concatenate the section payloads. The resulting output section can
contain **multiple independent StackMap v3 blobs** back-to-back. The runtime’s
parser (`runtime_native::stackmaps::StackMaps::parse`) handles this by scanning
all blobs and building one callsite index.

The `runtime-native/link/stackmaps_nopie.ld` (non-PIE), `runtime-native/link/stackmaps.ld` (PIE, lld),
and `runtime-native/link/stackmaps_gnuld.ld` (GNU ld PIE) linker script fragments define all of these
symbols and also provide legacy aliases:

- `__fastr_stackmaps_{start,end}` and `__llvm_stackmaps_{start,end}`

`runtime-native/stackmaps.ld` is kept for backwards compatibility with older build scripts.

When the section is absent, the symbols still define an empty range (`start == stop`).

Note: lld does not auto-define GNU ld-style `__start_<section>` / `__stop_<section>`
symbols, so the linker script (or an equivalent mechanism) is required.

When linking from C/clang, pass the appropriate fragment explicitly:

 ```bash
 # non-PIE:
 cc ... -no-pie -Wl,-T,runtime-native/link/stackmaps_nopie.ld ...
 
 # PIE + lld (requires rewriting `.llvm_*` to `.data.rel.ro.llvm_*` in input objects):
 cc ... -pie -Wl,-T,runtime-native/link/stackmaps.ld ...

# PIE + GNU ld:
cc ... -pie -Wl,-T,runtime-native/link/stackmaps_gnuld.ld ...
```

When linking from Rust, you still need to pass the script to the final link step
(e.g. via `RUSTFLAGS` or your build system):

 ```bash
 RUSTFLAGS="\
   -C force-frame-pointers=yes \
   -C linker=clang-18 \
   -C link-arg=-fuse-ld=lld-18 \
   -C link-arg=-Wl,-T,$PWD/runtime-native/link/stackmaps_nopie.ld" \
   bash scripts/cargo_agent.sh build
 ```

For `rustc`/Cargo consumers that don't use the feature-based build script hook, the equivalent is:

 ```bash
 # Example:
 #   RUSTFLAGS="-C link-arg=-Wl,-T,/abs/path/to/runtime-native/link/stackmaps_nopie.ld" bash scripts/cargo_agent.sh build ...
 ```

PIE note (Linux): LLVM `.llvm_stackmaps` contains absolute code addresses, which become runtime
relocations under PIE. If stackmaps end up in a read-only segment, this can lead to `DT_TEXTREL`
warnings (GNU ld) or hard link failures (lld).

- Default policy in this repo: link AOT binaries as **non-PIE** (`-no-pie`) for maximum toolchain
  compatibility.
- If you require PIE, use the objcopy-based “no textrel” approach described in:
  - `docs/gc_statepoints.md` (“Linux linking policy for .llvm_stackmaps”)
  - `scripts/native_link.sh` (set `ECMA_RS_NATIVE_PIE=1`)
  - `scripts/test_stackmaps_pie_link.sh` (regression test)
  - Note (GNU ld): if stackmaps are made writable for PIE relocation and the linker script inserts
    them immediately after `.text`, GNU ld can produce an RWX LOAD segment. Prefer
    `runtime-native/link/stackmaps_gnuld.ld` (or use `scripts/native_link.sh`, which selects it
    automatically for `ECMA_RS_NATIVE_LINKER=ld ECMA_RS_NATIVE_PIE=1`).

Note: if you use `-L ... -lruntime_native` instead of passing the `.a` file directly,
ensure the search path points at `target/release`.

## Stack walking + stack bounds

`runtime-native` interprets LLVM stackmaps by **walking frame pointers** to recover caller frame
state (return addresses + caller stack pointers). When available, each registered thread also
captures its stack bounds (`[lo, hi)`) so stack walking and conservative scanning can validate frame
and slot addresses stay within the stack mapping.

Stack bounds capture is supported on:

- Linux/Android: `pthread_getattr_np` / `pthread_attr_getstack`
- macOS: `pthread_get_stackaddr_np` / `pthread_get_stacksize_np`

## Pinned allocations

Some embeddings require stable object addresses (FFI / host references). The runtime exposes
`rt_alloc_pinned`, which is intended to allocate objects whose address is stable across GC cycles.
Pinned objects are allocated in the GC heap's non-moving large-object space (LOS): they are still
traced and reclaimed when unreachable, but are never relocated.

## Native async ABI (CoroutineId handles + PromiseHeader prefix)

`runtime-native` exposes a stable native async/await ABI intended for LLVM-lowered `async` state
machines (see `docs/async_abi.md` and `include/runtime_native.h`):

- Coroutines are `#[repr(C)]` frames prefixed by `struct Coroutine` at offset 0.
- Result promises are allocations prefixed by `PromiseHeader` at offset 0 (payload layout is owned by
  codegen; the runtime only relies on the header prefix).

### Why `CoroutineId` (handle) instead of `Coroutine*` (pointer)?

Coroutine frames may be relocated by a moving/compacting GC and may be stored in runtime queues
(microtasks, promise reaction lists) across turns. As a result, the ABI uses a stable `CoroutineId`
(`u64`) handle:

- Allocate a handle via the persistent handle table:
  `CoroutineId id = rt_handle_alloc((GcPtr)coro_ptr);`
- The runtime **consumes** the handle passed to `rt_async_spawn*` and frees it on completion or
  cancellation.
- All runtime scheduling sites reload the coroutine pointer via `rt_handle_load(id)` before resuming.

### Coroutine frame ownership

If `CORO_FLAG_RUNTIME_OWNS_FRAME` is set in `coro.flags`, the runtime calls `vtable->destroy(coro)`
exactly once on completion or cancellation. Stack/caller-owned frames must not suspend; they must
complete synchronously and are never destroyed by the runtime.

## Legacy async runtime GC roots

The crate currently contains a **legacy** async runtime (`rt_async_spawn_legacy`) that is used by
tests and older codegen prototypes. The JS-shaped event loop is driven by `rt_async_poll`
(`rt_async_poll_legacy` is a compatibility alias).

That runtime stores coroutine pointers in runtime-owned queues (macrotasks/microtasks) and in
promise reaction lists. When coroutine frames are allocated in the GC heap, these runtime-held
references must participate in GC root enumeration.

Contract for legacy coroutine frames:

- The pointer passed to the legacy async ABI (`*mut RtCoroutineHeader`) is a **derived pointer** to
  the coroutine frame payload stored immediately after the GC [`ObjHeader`] prefix.
- The GC object base pointer is `coro_ptr - OBJ_HEADER_SIZE`.
- While a coroutine is suspended (queued as a macrotask or attached as a promise reaction), the
  runtime registers the **base pointer** as a strong root and re-derives the coroutine pointer when
  resuming.

## ArrayBuffer / TypedArray backing stores (stable I/O buffers)

Native I/O APIs require buffer pointers remain valid at a stable address (e.g. until an `io_uring`
submission completes). Under a moving GC this means `ArrayBuffer` bytes cannot live in the moving
heap.

`runtime-native` provides a movable header + non-moving backing store split for JS buffer types in
`src/buffer/`:

- `buffer::ArrayBuffer` — movable header containing length + backing store handle.
- `buffer::Uint8Array` — bounds-checked view with `as_ptr_range()` for synchronous access and
  `pin()` for pointer stability (enforces detach/transfer/resize pin-count checks).
- `buffer::BackingStoreAllocator` — allocator abstraction for stable, non-moving byte storage.

For async I/O, prefer the `io::` layer (`IoOp`, `IoRuntime`, io_uring helpers), which pins **and**
borrows backing stores for the duration of the op (to preserve a sound aliasing model).

Design notes and invariants are documented in `docs/buffers-and-io.md` and
`../../docs/runtime-native/buffers-and-io.md`.

## Safepoint ABI

The runtime coordinates stop-the-world GC using an exported global epoch,
`RT_GC_EPOCH` (declared in `include/runtime_native.h`):

* **even**: no stop-the-world requested
* **odd**: stop-the-world requested

The recommended safepoint poll pattern for compiler-generated code is:

1. Inline poll: load `RT_GC_EPOCH`.
2. If the loaded epoch is odd, call `rt_gc_safepoint_slow(epoch)` (passing the **observed odd**
   epoch value).

In pseudocode:

```c
uint64_t epoch = RT_GC_EPOCH; // load (Acquire)
if (epoch & 1) {
  rt_gc_safepoint_slow(epoch);
}
```

`rt_gc_safepoint()` is a convenience wrapper that performs the same poll + slow-path call; it is
useful for embeddings/tests, but codegen should prefer the inline poll so the slow path captures
the callsite context correctly.

## Parallel ABI

The AOT compiler may emit calls to `rt_parallel_spawn` / `rt_parallel_join` for parallel work
splitting. The ABI shape is stable:

- `TaskId` is a fixed-width 64-bit identifier (`uint64_t` in C), independent of pointer width.
- `rt_parallel_spawn` returns a `TaskId`.
- `rt_parallel_join` takes a `TaskId*` + `size_t` count.

The scheduler implementation is a **work-stealing pool** suitable for compiler-inserted parallel
regions; the ABI is the contract.

### GC-safe userdata (`*_rooted`, `*_rooted_h`)

Parallel scheduling APIs take an opaque `data` pointer that is passed through to worker callbacks.

- The **unrooted** forms (`rt_parallel_spawn`, `rt_parallel_for`, `rt_parallel_spawn_promise`) require
  `data` to remain valid until the worker finishes. This is fine for non-GC memory (malloc/stack-owned
  state managed by the caller), but it is unsafe for **movable GC pointers** unless the caller
  separately pins/roots them.
- The **rooted** forms keep a GC-managed object alive while it is queued/executing and pass the
  relocated pointers to callbacks:
  - `rt_parallel_spawn_rooted` / `rt_parallel_spawn_rooted_h`
  - `rt_parallel_for_rooted` / `rt_parallel_for_rooted_h`
  - `rt_parallel_spawn_promise_rooted` / `rt_parallel_spawn_promise_rooted_h`
  - `rt_parallel_spawn_promise_with_shape_rooted` / `rt_parallel_spawn_promise_with_shape_rooted_h`

Rooted contract:

- `data` is the GC **object base pointer** (`GcPtr`): it must point at the start of the object
  header (the same kind of pointer returned by `rt_alloc`), not an interior pointer into the payload.
- The runtime holds a strong root for the object until the work completes (or, for
  `rt_parallel_for_rooted`, until the call returns).
- The callback receives the current pointer after any GC relocation.

The `_h` variants take a `GcHandle` (pointer-to-slot). Under a moving GC they are preferred because
the runtime may need to acquire locks while registering roots; by taking a handle it can reload the
pointer after any safepoint and avoid TOCTOU races between "load pointer" and "register root".

Example (GC-managed payload scheduled on the parallel pool, returning a promise for async/await):

```c
#include "runtime_native.h"

static void worker_task(uint8_t* data, PromiseRef promise) {
  // `data` is the relocated GC object base pointer.
  uint64_t* out = (uint64_t*)rt_promise_payload_ptr(promise);
  *out = 42;
  rt_promise_fulfill(promise);
}

PromiseRef schedule(GcPtr obj_base) {
  // Store the pointer in an addressable root slot and pass it as a `GcHandle`.
  GcPtr slot = obj_base;

  PromiseLayout layout = {
    .size = sizeof(uint64_t),
    .align = _Alignof(uint64_t),
  };

  return rt_parallel_spawn_promise_rooted_h(worker_task, &slot, layout);
}
```

Note: `rt_parallel_spawn_promise` uses an **out-of-line** payload buffer that is treated as raw
bytes (not traced by the GC). If the payload contains GC pointers, use
`rt_parallel_spawn_promise_with_shape{,_rooted,_rooted_h}` so the promise itself is GC-managed and
the payload is traced via the provided `RtShapeId`.

### Scheduler design

- Fixed number of worker threads (default: one per CPU, override via
  `ECMA_RS_RUNTIME_NATIVE_THREADS`).
- Each worker has a local deque (LIFO pop/push for cache locality).
- External submissions go into a global injector queue.
- Workers run: pop local → steal from injector → steal from other workers → spin → sleep.

`rt_parallel_join` is *helping*: the joining thread may also steal and execute tasks while waiting,
reducing idle time and avoiding deadlocks in nested spawn/join patterns.

`TaskId` handles are **one-shot**: each spawned task must be joined exactly once. Passing duplicate
task ids (or otherwise invalid ids) to `rt_parallel_join` is treated as an ABI contract violation
and aborts the process.

### Granularity control

`rt_parallel_for` exists for loop parallelization and includes basic granularity control:

- Default chunk size targets ~`workers * 4` chunks, bounded by `RT_PAR_FOR_MIN_GRAIN` (default 1024).
- A (currently stub) cost-model hook is exposed to Rust via `runtime_native::parallel::set_cost_model`.

### Rust convenience APIs

For Rust-side callers (tests/benchmarks/future compiler helpers), the `runtime_native::parallel`
module exposes:

- `spawn(FnOnce() + Send) -> TaskId`
- `join(&[TaskId])`
- `parallel_for(range, body, chunking)`

## Blocking thread pool ABI (`rt_spawn_blocking`)

Many host APIs are inherently blocking (filesystem, DNS, crypto, etc.). To preserve async semantics
without blocking the event loop, `runtime-native` exposes a dedicated blocking thread pool.

`rt_spawn_blocking` runs `task(data, promise)` on the blocking pool and returns the allocated
`LegacyPromiseRef`:

```c
LegacyPromiseRef rt_spawn_blocking(void (*task)(uint8_t* data, LegacyPromiseRef promise), uint8_t* data);
```

Contract:

- The runtime allocates a new pending promise and passes it to the task.
- The task must settle the promise via `rt_promise_resolve_legacy` / `rt_promise_reject_legacy`.
- `data` must remain valid for the duration of the task and must be safe to access from a blocking
  worker thread.
- Blocking tasks execute in a GC-safe ("NativeSafe") region and must not touch the GC heap.

Pool sizing:

- Default: `min(std::thread::available_parallelism(), 32)`
- Override: set `ECMA_RS_RUNTIME_NATIVE_BLOCKING_THREADS` to a positive integer before first use
  (`RT_BLOCKING_THREADS` is also supported as a legacy alias).

## C ABI notes

The stable C ABI surface is declared in [`include/runtime_native.h`](include/runtime_native.h).

### Shape IDs (`RtShapeId`)

The runtime does not take the compiler's semantic `ShapeId` (`u128`) directly. Instead, compiled
code passes a compact `RtShapeId` (`uint32_t`) which is a runtime-local index into the shape
descriptor table registered via `rt_register_shape_table`.

## Legacy coroutine ABI (`RtCoroutineHeader`)

Generated coroutine frames are `#[repr(C)]` structs whose **first field** (prefix) is
[`RtCoroutineHeader`](src/abi.rs). The runtime and generated code communicate only via this header.

### `RtCoroutineHeader` layout

```c
struct RtCoroutineHeader {
  RtCoroStatus (*resume)(struct RtCoroutineHeader*); // +0
  LegacyPromiseRef promise;                          // +8
  uint32_t state;                                    // +16
  uint32_t await_is_error;                            // +20 (0=value, 1=error)
  ValueRef await_value;                               // +24
  ValueRef await_error;                               // +32
};
```

`resume` is provided by the compiler and implements a state machine that switches on `state`.

### Coroutine status

`resume` returns an [`RtCoroStatus`](src/abi.rs):

* `Done`: coroutine is complete (it should have resolved/rejected `coro->promise`).
* `Pending`: coroutine suspended on an `await` (the runtime must stop executing it now).
* `Yield`: cooperative yield (runtime schedules the coroutine to resume later).

## Key semantic requirement (`rt_async_spawn_legacy`)

`rt_async_spawn_legacy` must run the coroutine **synchronously** on the calling thread until it either:

* completes (`Done`), or
* reaches its first suspension point (`Pending` / `await`).

This matches JavaScript:

```js
async function f() { side_effect(); await 0; }
f(); // side_effect happens immediately
```

## Legacy promise placeholder

The runtime provides a minimal `Promise` implementation sufficient for async/await:

* create a pending promise (`rt_promise_new_legacy`)
* resolve/reject it (`rt_promise_resolve_legacy` / `rt_promise_reject_legacy`)
* register a continuation:
  * `rt_promise_then_legacy` (`data` is opaque; caller owns the lifetime)
  * `rt_promise_then_rooted_legacy` (`data` is a GC-managed object base pointer rooted until invoked)
  * `rt_promise_then_with_drop_legacy` (`data` is owned callback state; runtime invokes `drop_data` on discard)

Continuations are always scheduled onto the async runtime **microtask** queue and are executed
FIFO by driving the event loop (e.g. `rt_async_poll()`; `rt_async_poll_legacy()` is a compatibility
alias).

Note: `runtime-native` is migrating to a native Promise/Coroutine ABI based on a `PromiseHeader`
prefix. The native ABI entrypoints (`rt_async_spawn*`) and the legacy entrypoints
(`rt_async_spawn*_legacy`) are driven by the same JS-shaped event loop (`rt_async_poll` /
`rt_async_poll_legacy` are identical).

## Microtask ABI (queueMicrotask)

In addition to promise continuations, embedders and stdlib bindings can enqueue lightweight
queueMicrotask-style jobs directly via:

- `rt_queue_microtask(Microtask task)`
- `rt_queue_microtask_with_drop(cb, data, drop_data)` (drop hook runs if discarded without executing)
- `rt_queue_microtask_rooted(cb, data)` (GC-managed `data` kept alive + relocation-safe)
- `rt_drain_microtasks() -> bool`

See `docs/async_abi.md` for details.

## Threading + GC safepoints

The JS-shaped async runtime is **single-driver**: at most one OS thread may *drive* the event loop
at a time via the "driving" entrypoints (e.g. `rt_async_poll` / `rt_async_poll_legacy`,
`rt_async_wait`, `rt_drain_microtasks`, `rt_async_run_until_idle`, `rt_async_block_on`,
`rt_async_cancel_all`).

The first thread to drive becomes the runtime's **event-loop thread** and is registered as
`ThreadKind::Main` so stop-the-world GC can wait for it and scan its roots.

If another thread attempts to drive concurrently, the runtime aborts (fail-fast). Other threads may
still enqueue work into the runtime (it is multi-producer); an `eventfd` wakeup is used to interrupt
a blocking `epoll_wait`.

`rt_async_poll` (and `rt_async_poll_legacy`) also polls the GC safepoint barrier:

* at the start of each poll turn
* immediately before entering `epoll_wait`
* immediately after returning from `epoll_wait` (before running callbacks)

## Benchmarks

From the repository root:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh bench -p runtime-native
```

To enable trace counters during a run:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh bench -p runtime-native --features rt-trace
```

### Bench suite

- `parallel_spawn_join`: spawn + join overhead across varying task counts / payload sizes.
- `scheduler_throughput`: tasks/sec for empty tasks and small CPU loops.
- `microtasks`: enqueue + drain rate of the async runtime microtask queue.
- `async_timers`: timer heap insert + dispatch costs; plus a small timer accuracy probe.

### Interpreting results

- Prefer comparisons **between commits** on the same machine/configuration.
- For throughput benches, Criterion prints **elements/sec**; higher is better.
- For the timer accuracy probe, the measured iteration time should be close to the requested
  delay (large drift indicates scheduling jitter or timer implementation issues).

## Trace counters (`rt-trace`)

When compiled with `--features rt-trace`, the runtime collects a small set of global counters
intended for lightweight regression detection in tests/benches.

Use:

```rust
let snap = runtime_native::rt_debug_snapshot_counters();
```

When `rt-trace` is not enabled, all values are always `0`.

## GC-safe host queues (persistent roots)

Host-owned work queues (async tasks, I/O watchers, OS event loop userdata, etc.) are **not**
automatically traced by the GC. Any queued work that captures GC-managed objects must keep those
objects alive explicitly, and must be able to discard queued work without leaking roots.

This crate provides [`gc::HandleTable`], a generational handle table intended to act like a
*persistent root set*:

- Hosts store a stable [`gc::HandleId`] (convertible to/from `u64`) in their queues or OS userdata.
- The table stores a relocatable `NonNull<T>` pointer.
- During relocation/compaction the GC updates pointers in-place via
  [`gc::HandleTable::with_stw_update`] under a stop-the-world (STW) pause.
- When host work is canceled/dropped, callers must `free` the handle to allow collection.

The GC-managed objects themselves remain movable; only the handle IDs and handle table slots are
stable.

## Stackmaps from multiple modules (dlopen / JIT)

Precise stack scanning relies on LLVM's `.llvm_stackmaps` sections. In real
deployments native code may live in multiple shared libraries (loaded via
`dlopen`) or be JITed into memory; each module may have its own stackmaps blob.

`runtime-native` supports this via explicit registration into a global registry:

```c
bool rt_stackmaps_register(const uint8_t* start, const uint8_t* end);
bool rt_stackmaps_unregister(const uint8_t* start);
```

Modules should call `rt_stackmaps_register(__llvm_stackmaps_start, __llvm_stackmaps_end)` at load
time (e.g. via an ELF constructor). `runtime-native/include/runtime_native.h` provides a helper:

```c
RT_STACKMAPS_AUTO_REGISTER();
```

> Note: if the module calls into the *host executable* (rather than a shared
> `libruntime_native.so`), the host must export its symbols into the dynamic
> symbol table (ELF `-rdynamic` / `--export-dynamic`) so `rt_stackmaps_register`
> can be resolved at `dlopen` time.

As a fallback (Linux-only), you can discover and register stackmaps from all
currently loaded ELF images:

```rust
runtime_native::global_stackmap_registry()
  .write()
  .load_all_loaded_modules()?;
```
