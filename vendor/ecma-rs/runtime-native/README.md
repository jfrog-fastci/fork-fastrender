# runtime-native

Native runtime library used by LLVM-generated code (planned `native-js` backend).

This crate provides the runtime-side pieces of the compiler/runtime ABI contract:
allocator entrypoints, string helpers, GC safepoints, and the **minimal async/await runtime ABI**
needed to execute LLVM-generated coroutine state machines with JS-correct microtask ordering.

## Build (static library)

From the `vendor/ecma-rs/` workspace root:

```bash
bash scripts/cargo_agent.sh build --release -p runtime-native
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
  /path/to/program.c \
  target/release/libruntime_native.a \
  -o program
```

If your program uses LLVM statepoints / stackmaps (i.e. it contains an
in-memory `.llvm_stackmaps` section) and you want the runtime to be able to
locate it for stack walking, you must also export the boundary symbols:

- `__fastr_stackmaps_start`
- `__fastr_stackmaps_end`

The `runtime-native/stackmaps.ld` linker script fragment defines these symbols
(and legacy `__llvm_stackmaps_{start,end}` aliases). When the section is
absent, the symbols still define an empty range (`start == end`).

When linking from C/clang, pass it explicitly:

```bash
cc ... -Wl,-T,runtime-native/stackmaps.ld ...
```

When linking Rust binaries, `runtime-native/build.rs` injects `stackmaps.ld`
only when the `llvm_stackmaps_linker` Cargo feature is enabled.

Note: if you use `-L ... -lruntime_native` instead of passing the `.a` file directly,
ensure the search path points at `target/release`.

## Pinned allocations

Some embeddings require stable object addresses (FFI / host references). The runtime exposes
`rt_alloc_pinned`, which is intended to allocate objects whose address is stable across GC cycles.
Pinned objects are still expected to be traced and collectible when the GC-backed allocator is
wired up.

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

## Parallel ABI (placeholder)

The AOT compiler may emit calls to `rt_parallel_spawn` / `rt_parallel_join` for parallel work
splitting. The ABI shape is stable:

- `TaskId` is a fixed-width 64-bit identifier (`uint64_t` in C), independent of pointer width.
- `rt_parallel_spawn` returns a `TaskId`.
- `rt_parallel_join` takes a `TaskId*` + `size_t` count.

The scheduler implementation itself is intentionally minimal for now; the ABI is the contract.

Current behavior: `rt_parallel_spawn` enqueues work onto a global FIFO queue serviced by a small
worker pool (default: one worker per CPU). `rt_parallel_join` waits for completion and may
opportunistically execute queued tasks on the joining thread to reduce idle time.

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
