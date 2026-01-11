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

Note: if you use `-L ... -lruntime_native` instead of passing the `.a` file directly,
ensure the search path points at `target/release`.

## Pinned allocations

Some embeddings require stable object addresses (FFI / host references). The runtime exposes
`rt_alloc_pinned`, which is intended to allocate objects whose address is stable across GC cycles.
Pinned objects are still expected to be traced and collectible when the GC-backed allocator is
wired up.

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
