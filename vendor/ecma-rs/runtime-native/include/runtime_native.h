#ifndef ECMA_RS_RUNTIME_NATIVE_H
#define ECMA_RS_RUNTIME_NATIVE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// Stable C ABI surface for runtime-native.
//
// This header is intended for code generators / native glue code. Keep it small:
// only entrypoints that are part of the compiler/runtime ABI contract should live here.

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------------
// Core ABI types
// -----------------------------------------------------------------------------

#ifndef __SIZEOF_INT128__
#error "runtime-native requires a compiler with __int128 support"
#endif
typedef unsigned __int128 ShapeId;

typedef uint32_t InternedId;
typedef uint64_t TaskId;

// runtime-native does not yet implement a full JS value representation/GC.
// For now, values are passed as opaque pointers.
typedef void* ValueRef;

// Minimal promise placeholder.
typedef struct RtPromise RtPromise;
typedef RtPromise* PromiseRef;

// An FFI-friendly UTF-8 byte string reference.
typedef struct StringRef {
  const uint8_t* ptr;
  size_t len;
} StringRef;

// -----------------------------------------------------------------------------
// Thread registration
// -----------------------------------------------------------------------------
// Register the current OS thread with the runtime.
//
// Any thread that may execute compiled code and participate in GC safepoints
// must call `rt_thread_init` before running mutator code, and `rt_thread_deinit`
// before exiting.
//
// `kind` is a best-effort hint used for diagnostics only:
//   0 = main
//   1 = worker
//   2 = io
//   3 = external/unknown
void rt_thread_init(uint32_t kind);
void rt_thread_deinit(void);

// -----------------------------------------------------------------------------
// Memory
// -----------------------------------------------------------------------------
// Allocate a GC-managed object.
//
// Contract: returns the **object base pointer** (points to the start of the
// runtime GC header / ObjHeader). `size` is the total allocation size in bytes
// including the header and payload.
uint8_t* rt_alloc(size_t size, ShapeId shape);
// Allocate a pinned (non-moving) object. Pinned objects are intended for FFI /
// host embeddings that require stable addresses.
uint8_t* rt_alloc_pinned(size_t size, ShapeId shape);
uint8_t* rt_alloc_array(size_t len, size_t elem_size);

// -----------------------------------------------------------------------------
// GC entrypoints (milestone runtime: mostly no-ops)
// -----------------------------------------------------------------------------
void rt_gc_safepoint(void);
// Generational write barrier for an object field store.
//
// Contract: `obj` must be the same object base pointer that was returned from
// `rt_alloc` (i.e. pointer to the start of the object's header), and `slot` must
// point to the field being written.
void rt_write_barrier(uint8_t* obj, uint8_t* slot);
void rt_write_barrier_range(uint8_t* obj, uint8_t* start_slot, size_t len);
void rt_gc_collect(void);

// Update the active nursery (young generation) address range used by the write barrier.
// Must be called by the GC at initialization and after each nursery flip/resize.
void rt_gc_set_young_range(uint8_t* start, uint8_t* end);

// Debug/test helper: read the current young-space range.
void rt_gc_get_young_range(uint8_t** out_start, uint8_t** out_end);

// Optional GC/runtime stats APIs.
//
// These entrypoints are only available when `runtime-native` was built with the
// Cargo feature `gc_stats`. Define `RUNTIME_NATIVE_GC_STATS` to expose them in C.
#ifdef RUNTIME_NATIVE_GC_STATS
typedef struct RtGcStatsSnapshot {
  uint64_t alloc_calls;
  size_t alloc_bytes;
  uint64_t alloc_array_calls;
  size_t alloc_array_bytes;
  uint64_t gc_collect_calls;
  uint64_t safepoint_calls;
  uint64_t write_barrier_calls;
  uint64_t write_barrier_range_calls;
  uint64_t set_young_range_calls;
  uint64_t thread_init_calls;
  uint64_t thread_deinit_calls;
} RtGcStatsSnapshot;

void rt_gc_stats_snapshot(RtGcStatsSnapshot* out);
void rt_gc_stats_reset(void);
#endif

// -----------------------------------------------------------------------------
// Weak references (weak handles)
// -----------------------------------------------------------------------------
uint64_t rt_weak_add(uint8_t* value);
uint8_t* rt_weak_get(uint64_t handle);
void rt_weak_remove(uint64_t handle);

// -----------------------------------------------------------------------------
// Strings
// -----------------------------------------------------------------------------
StringRef rt_string_concat(const uint8_t* a, size_t a_len, const uint8_t* b, size_t b_len);
InternedId rt_string_intern(const uint8_t* s, size_t len);

// -----------------------------------------------------------------------------
// Parallel
// -----------------------------------------------------------------------------
TaskId rt_parallel_spawn(void (*task)(uint8_t*), uint8_t* data);
void rt_parallel_join(const TaskId* tasks, size_t count);
void rt_parallel_for(size_t start, size_t end, void (*body)(size_t, uint8_t*), uint8_t* data);

// -----------------------------------------------------------------------------
// Promise placeholder
// -----------------------------------------------------------------------------
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
