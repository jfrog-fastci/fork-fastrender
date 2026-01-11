# runtime-native

Native runtime library used by LLVM-generated code (planned `native-js` backend).

This crate is intentionally minimal today: it provides the milestone ABI surface (allocator, string
helpers, etc.) plus the **minimal async/await runtime ABI** needed to execute LLVM-generated
coroutine state machines with JS-correct microtask ordering.

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
