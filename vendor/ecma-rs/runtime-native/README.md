# runtime-native

Native runtime library used by LLVM-generated code (planned `native-js` backend).

This crate provides the runtime-side pieces of the compiler/runtime ABI contract:
allocator entrypoints, string helpers, GC safepoints, and the **minimal async/await runtime ABI**
needed to execute LLVM-generated coroutine state machines with JS-correct microtask ordering.

See also:
* `include/runtime_native.h` — stable C ABI surface
* `docs/safepoint_abi.md` — thread registration + parked/unparked safepoint protocol
* `docs/write_barrier.md` — generational write barrier contract

## Reactor contract (epoll + kqueue)

The low-level cross-platform reactor contract is documented in `docs/reactor.md` and enforced by
`tests/reactor_conformance.rs`.

Key guarantees include:

- **edge-triggered** readiness on all platforms (epoll `EPOLLET`, kqueue `EV_CLEAR`)
- **at most one event per token per poll**, with read+write readiness merged when both are observed
- a cross-thread `Waker` that interrupts a blocking poll

## Build (static library)

From the `vendor/ecma-rs/` workspace root:

```bash
bash scripts/cargo_agent.sh build --release -p runtime-native
```

From the superproject repo root (or any cwd):

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p runtime-native --release
```

Or via the helper script (prints include/lib paths for downstream build systems):

```bash
bash scripts/build_runtime_native.sh
```

Expected artifacts:

- Static library: `target/release/libruntime_native.a`
- C header: `runtime-native/include/runtime_native.h`

## Link from C / clang

Example (from the workspace root):

```bash
cc -std=c99 \
  -I runtime-native/include \
  -Wl,-T,runtime-native/link/stackmaps.ld \
  /path/to/program.c \
  target/release/libruntime_native.a \
  -o program
```

If you want to force LLVM lld explicitly:

```bash
clang-18 -fuse-ld=lld-18 \
  -I runtime-native/include \
  -Wl,-T,runtime-native/link/stackmaps.ld \
  /path/to/program.c \
  target/release/libruntime_native.a \
  -o program
```

If your program uses LLVM statepoints / stackmaps (i.e. it contains an
in-memory `.llvm_stackmaps` section) and you want the runtime to be able to
locate it for stack walking, you must also export the boundary symbols:

- `__start_llvm_stackmaps`
- `__stop_llvm_stackmaps`

The `runtime-native/link/stackmaps.ld` linker script fragment defines these symbols and also
provides aliases:

- `__stackmaps_{start,end}` (generic alias)
- `__fastr_stackmaps_{start,end}` and `__llvm_stackmaps_{start,end}` (legacy aliases)

`runtime-native/stackmaps.ld` is kept for backwards compatibility with older build scripts.

When the section is absent, the symbols still define an empty range (`start == stop`).

Note: lld does not auto-define GNU ld-style `__start_<section>` / `__stop_<section>`
symbols, so the linker script (or an equivalent mechanism) is required.

When linking from C/clang, pass it explicitly:

```bash
cc ... -Wl,-T,runtime-native/link/stackmaps.ld ...
```

When linking from Rust, you still need to pass the script to the final link step
(e.g. via `RUSTFLAGS` or your build system):

```bash
RUSTFLAGS="\
  -C linker=clang-18 \
  -C link-arg=-fuse-ld=lld \
  -C link-arg=-Wl,-T,$PWD/runtime-native/link/stackmaps.ld" \
  bash scripts/cargo_agent.sh build
```

For `rustc`/Cargo consumers that don't use the feature-based build script hook, the equivalent is:

```bash
# Example:
#   RUSTFLAGS="-C link-arg=-Wl,-T,/abs/path/to/runtime-native/link/stackmaps.ld" cargo build ...
```

PIE note: current LLVM 18 experiments may emit `TEXTREL` warnings due to relocations in
`.llvm_stackmaps`. Linking with `-no-pie` avoids this in a minimal setup.

Note: if you use `-L ... -lruntime_native` instead of passing the `.a` file directly,
ensure the search path points at `target/release`.

## Pinned allocations

Some embeddings require stable object addresses (FFI / host references). The runtime exposes
`rt_alloc_pinned`, which is intended to allocate objects whose address is stable across GC cycles.
Pinned objects are still expected to be traced and collectible when the GC-backed allocator is
wired up.

## ArrayBuffer / TypedArray backing stores (stable I/O buffers)

Native I/O APIs require buffer pointers remain valid at a stable address (e.g. until an `io_uring`
submission completes). Under a moving GC this means `ArrayBuffer` bytes cannot live in the moving
heap.

`runtime-native` provides a movable header + non-moving backing store split for JS buffer types in
`src/buffer/`:

- `buffer::ArrayBuffer` — movable header containing length + backing store handle.
- `buffer::Uint8Array` — bounds-checked view with `as_ptr_range()` for synchronous access and
  `pin()` for async I/O pinning (enforces detach/transfer/resize pin-count checks).
- `buffer::BackingStoreAllocator` — allocator abstraction for stable, non-moving byte storage.

Design notes and invariants are documented in `docs/buffers-and-io.md`.

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
`PromiseRef`:

```c
PromiseRef rt_spawn_blocking(void (*task)(uint8_t* data, PromiseRef promise), uint8_t* data);
```

Contract:

- The runtime allocates a new pending promise and passes it to the task.
- The task must settle the promise via `rt_promise_resolve` / `rt_promise_reject`.
- `data` must remain valid for the duration of the task and must be safe to access from a blocking
  worker thread.

Pool sizing:

- Default: `min(std::thread::available_parallelism(), 32)`
- Override: set `ECMA_RS_RUNTIME_NATIVE_BLOCKING_THREADS` to a positive integer before first use
  (`RT_BLOCKING_THREADS` is also supported as a legacy alias).

## Coroutine ABI

Generated coroutine frames are `#[repr(C)]` structs whose **first field** (prefix) is
[`RtCoroutineHeader`](src/abi.rs). The runtime and generated code communicate only via this header.

### `RtCoroutineHeader` layout

```c
struct RtCoroutineHeader {
  RtCoroStatus (*resume)(struct RtCoroutineHeader*); // +0
  PromiseRef promise;                                // +8
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

## Key semantic requirement (`rt_async_spawn`)

`rt_async_spawn` must run the coroutine **synchronously** on the calling thread until it either:

* completes (`Done`), or
* reaches its first suspension point (`Pending` / `await`).

This matches JavaScript:

```js
async function f() { side_effect(); await 0; }
f(); // side_effect happens immediately
```

## Promise placeholder

The runtime provides a minimal `Promise` implementation sufficient for async/await:

* create a pending promise (`rt_promise_new`)
* resolve/reject it (`rt_promise_resolve` / `rt_promise_reject`)
* register a continuation (`rt_promise_then`)

Continuations are always scheduled onto the async runtime **microtask** queue and are executed
FIFO by calling `rt_async_poll()`.

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
  [`gc::HandleTable::update`] / [`gc::HandleTable::iter_live_mut`] under a stop-the-world (STW)
  pause.
- When host work is canceled/dropped, callers must `free` the handle to allow collection.

The GC-managed objects themselves remain movable; only the handle IDs and handle table slots are
stable.
