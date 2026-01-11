#ifndef ECMA_RS_RUNTIME_NATIVE_H
#define ECMA_RS_RUNTIME_NATIVE_H

#include <stdbool.h>
#include <stdint.h>

// Minimal stable C ABI surface for runtime-native.
//
// This header is intended for code generators / native glue code. Keep it small:
// only entrypoints that are part of the compiler/runtime ABI contract should live here.

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------------
// GC entrypoints (milestone runtime: mostly no-ops)
// -----------------------------------------------------------------------------
void rt_gc_safepoint(void);
void rt_write_barrier(uint8_t* obj, uint8_t* slot);
void rt_gc_collect(void);

// -----------------------------------------------------------------------------
// Opaque value model
// -----------------------------------------------------------------------------
//
// runtime-native does not yet implement a full JS value representation/GC.
// For now, values are passed as opaque pointers.
typedef void* ValueRef;

// -----------------------------------------------------------------------------
// Promise placeholder
// -----------------------------------------------------------------------------
typedef struct RtPromise RtPromise;
typedef RtPromise* PromiseRef;

PromiseRef rt_promise_new(void);
void rt_promise_resolve(PromiseRef p, ValueRef value);
void rt_promise_reject(PromiseRef p, ValueRef err);
void rt_promise_then(PromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);

// -----------------------------------------------------------------------------
// Coroutine ABI (LLVM-generated async/await state machines)
// -----------------------------------------------------------------------------
typedef enum RtCoroStatus {
  RT_CORO_DONE = 0,
  RT_CORO_PENDING = 1,
  RT_CORO_YIELD = 2,
} RtCoroStatus;

typedef struct RtCoroutineHeader RtCoroutineHeader;
typedef RtCoroStatus (*RtCoroResumeFn)(RtCoroutineHeader*);

// Generated coroutine frames are structs whose prefix is RtCoroutineHeader.
//
// Layout (x64):
//   0  : resume
//   8  : promise
//   16 : state
//   20 : await_is_error (0=value, 1=error)
//   24 : await_value
//   32 : await_error
struct RtCoroutineHeader {
  RtCoroResumeFn resume;
  PromiseRef promise;
  uint32_t state;
  uint32_t await_is_error;
  ValueRef await_value;
  ValueRef await_error;
};

// Spawn a coroutine and return its promise.
// Runs the coroutine synchronously until it completes or reaches its first `await`.
PromiseRef rt_async_spawn(RtCoroutineHeader* coro);

// Drive the async runtime. Returns true if any work was performed.
bool rt_async_poll(void);

// Suspend the coroutine on an awaited promise.
// Registers a continuation and sets `coro->state = next_state`.
void rt_coro_await(RtCoroutineHeader* coro, PromiseRef awaited, uint32_t next_state);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // ECMA_RS_RUNTIME_NATIVE_H

