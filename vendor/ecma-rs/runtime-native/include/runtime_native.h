#ifndef ECMA_RS_RUNTIME_NATIVE_H
#define ECMA_RS_RUNTIME_NATIVE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// Stable C ABI surface for runtime-native.
//
// This header is intended for code generators / native glue code. Keep it small:
// only entrypoints that are part of the compiler/runtime ABI contract should live here.

typedef uint64_t IoWatcherId;
typedef int32_t RtFd;

enum {
  RT_IO_READABLE = 0x1,
  RT_IO_WRITABLE = 0x2,
  RT_IO_ERROR = 0x4,
};

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------------
// Core ABI types
// -----------------------------------------------------------------------------

// Shapes are registered into a global runtime table and referenced by a compact
// runtime-local id.
typedef uint32_t RtShapeId;

// FFI-stable shape descriptor used for precise GC tracing.
typedef struct RtShapeDescriptor {
  // Total object size in bytes (including ObjHeader).
  uint32_t size;
  // Object alignment in bytes (power of two).
  uint16_t align;
  // Reserved flags (must be 0 for now).
  uint16_t flags;
  // Pointer slot byte offsets from the object base pointer (start of ObjHeader).
  const uint32_t* ptr_offsets;
  uint32_t ptr_offsets_len;
  // Reserved for future expansion (must be 0).
  uint32_t reserved;
} RtShapeDescriptor;

typedef uint32_t InternedId;
typedef uint64_t TaskId;
typedef uint64_t TimerId;

// runtime-native does not yet implement a full JS value representation/GC.
// For now, values are passed as opaque pointers.
typedef void* ValueRef;

// -----------------------------------------------------------------------------
// Native async/await ABI (Promise/Coroutine)
// -----------------------------------------------------------------------------
// Native codegen represents `async` functions as coroutines that produce a
// GC-allocated `Promise<T>`.
//
// Each `Promise<T>` begins with a `PromiseHeader` prefix at offset 0. The
// promise payload begins immediately after the header (layout chosen by the
// compiler).
//
// The `PromiseHeader` layout is defined by the Rust ABI types in
// `runtime-native/src/async_abi.rs`; for C callers/codegen this header treats it
// as opaque.
typedef struct PromiseHeader PromiseHeader;
typedef PromiseHeader* PromiseRef;

// Legacy promise placeholder (used by older runtime-native tests/utilities).
typedef struct RtPromise RtPromise;
typedef RtPromise* LegacyPromiseRef;

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
// Convenience registration API: equivalent to `rt_thread_init(1 /* worker */)`.
void rt_register_current_thread(void);
void rt_unregister_current_thread(void);

// -----------------------------------------------------------------------------
// Thread attach/detach (per-runtime thread registry)
// -----------------------------------------------------------------------------
// Opaque runtime/thread records used by native codegen and embedders.
typedef struct Runtime Runtime;
typedef struct Thread Thread;

Thread* rt_thread_attach(Runtime* runtime);
void rt_thread_detach(Thread* thread);

// -----------------------------------------------------------------------------
// Memory
// -----------------------------------------------------------------------------
// Allocate a GC-managed object.
//
// Contract: returns the **object base pointer** (points to the start of the
// runtime GC header / ObjHeader). `size` is the total allocation size in bytes
// including the header and payload.
uint8_t* rt_alloc(size_t size, RtShapeId shape);
// Allocate a pinned (non-moving) object. Pinned objects are intended for FFI /
// host embeddings that require stable addresses.
uint8_t* rt_alloc_pinned(size_t size, RtShapeId shape);
uint8_t* rt_alloc_array(size_t len, size_t elem_size);

// Register the global shape table used by `RtShapeId`.
//
// This must be called exactly once at program initialization, before any
// allocations that participate in GC tracing.
void rt_register_shape_table(const RtShapeDescriptor* table, size_t len);

// -----------------------------------------------------------------------------
// GC entrypoints (milestone runtime: mostly no-ops)
// -----------------------------------------------------------------------------

// Global GC/safepoint epoch (monotonically increasing).
//
// Semantics:
//   - even: no stop-the-world requested
//   - odd:  stop-the-world requested
//
// Generated code should inline the fast safepoint poll as:
//   epoch = RT_GC_EPOCH (load); if (epoch & 1) rt_gc_safepoint_slow(epoch);
#if defined(__cplusplus)
extern uint64_t RT_GC_EPOCH;
#elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L) && !defined(__STDC_NO_ATOMICS__)
extern _Atomic uint64_t RT_GC_EPOCH;
#else
extern uint64_t RT_GC_EPOCH;
#endif

void rt_gc_safepoint(void);
// Safepoint slow path entered only when `RT_GC_EPOCH` is odd (stop-the-world requested).
// Callers should pass the observed odd epoch value.
void rt_gc_safepoint_slow(uint64_t epoch);
// Cheap leaf poll used by compiler-inserted loop backedge safepoints.
// Returns true if a stop-the-world GC is currently requested.
bool rt_gc_poll(void);
// Generational write barrier for an object field store.
//
// Contract: `obj` must be the same object base pointer that was returned from
// `rt_alloc` (i.e. pointer to the start of the object's header), and `slot` must
// point to the field being written.
void rt_write_barrier(uint8_t* obj, uint8_t* slot);
void rt_write_barrier_range(uint8_t* obj, uint8_t* start_slot, size_t len);
void rt_gc_collect(void);

// -----------------------------------------------------------------------------
// GC roots / handles (non-stack roots)
// -----------------------------------------------------------------------------
// LLVM stackmaps cover mutator stack/register roots, but the runtime must also
// track global/static roots and long-lived handles.
//
// Register an addressable root slot. `slot` must remain valid and writable
// until unregistered.
uint32_t rt_gc_register_root_slot(uint8_t** slot);
void rt_gc_unregister_root_slot(uint32_t handle);
// Convenience: allocate an internal slot initialized to `ptr` and register it
// as a root. The returned handle must later be passed to `rt_gc_unpin`.
uint32_t rt_gc_pin(uint8_t* ptr);
void rt_gc_unpin(uint32_t handle);

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
// The runtime maintains a process-global worker pool. By default the pool size matches the
// available CPU parallelism; override it by setting `ECMA_RS_RUNTIME_NATIVE_THREADS` to a positive
// integer before first use.
// Schedule `task(data)` onto the runtime's global worker pool.
//
// Contract:
// - `data` must remain valid until the returned `TaskId` is passed to `rt_parallel_join`.
// - The returned `TaskId` must be joined exactly once.
TaskId rt_parallel_spawn(void (*task)(uint8_t*), uint8_t* data);
void rt_parallel_join(const TaskId* tasks, size_t count);
void rt_parallel_for(size_t start, size_t end, void (*body)(size_t, uint8_t*), uint8_t* data);

// -----------------------------------------------------------------------------
// Blocking thread pool
// -----------------------------------------------------------------------------
// Run `task(data, promise)` on the runtime's dedicated blocking thread pool (for I/O, crypto,
// etc.). The task must resolve/reject `promise` via `rt_promise_resolve_legacy` /
// `rt_promise_reject_legacy`.
//
// Pool size:
// - default: min(available_parallelism, 32)
// - override: set `ECMA_RS_RUNTIME_NATIVE_BLOCKING_THREADS` (or legacy `RT_BLOCKING_THREADS`)
LegacyPromiseRef rt_spawn_blocking(void (*task)(uint8_t*, LegacyPromiseRef), uint8_t* data);

// -----------------------------------------------------------------------------
// Native promise ABI (PromiseHeader prefix)
// -----------------------------------------------------------------------------
void rt_promise_init(PromiseRef p);
void rt_promise_fulfill(PromiseRef p);
void rt_promise_reject(PromiseRef p);

// -----------------------------------------------------------------------------
// Native coroutine ABI (async/await lowering)
// -----------------------------------------------------------------------------
typedef struct Coroutine Coroutine;
typedef struct CoroutineVTable CoroutineVTable;
typedef Coroutine* CoroutineRef;

typedef enum CoroutineStepTag {
  RT_CORO_STEP_AWAIT = 0,
  RT_CORO_STEP_COMPLETE = 1,
} CoroutineStepTag;

typedef struct CoroutineStep {
  CoroutineStepTag tag;
  // For `RT_CORO_STEP_AWAIT`, the promise being awaited. For `RT_CORO_STEP_COMPLETE`, this is NULL.
  PromiseRef await_promise;
} CoroutineStep;

typedef CoroutineStep (*CoroutineResumeFn)(Coroutine*);

struct CoroutineVTable {
  CoroutineResumeFn resume;
  uint32_t promise_size;
  uint32_t promise_align;
  uint32_t promise_shape_id;
  uint32_t abi_version;
  uintptr_t reserved[4];
};

// Generated coroutine frames are structs whose prefix is `Coroutine`.
struct Coroutine {
  const CoroutineVTable* vtable;
  PromiseRef promise;
  Coroutine* next_waiter;
  uint32_t flags;
};

PromiseRef rt_async_spawn(CoroutineRef coro);

// Drive the async runtime. Returns true if any work was performed.
bool rt_async_poll(void);

// -----------------------------------------------------------------------------
// Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates)
// -----------------------------------------------------------------------------
LegacyPromiseRef rt_promise_new_legacy(void);
void rt_promise_resolve_legacy(LegacyPromiseRef p, ValueRef value);
void rt_promise_reject_legacy(LegacyPromiseRef p, ValueRef err);
void rt_promise_then_legacy(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);

typedef enum RtCoroStatus {
  RT_CORO_DONE = 0,
  RT_CORO_PENDING = 1,
  RT_CORO_YIELD = 2,
} RtCoroStatus;

typedef struct RtCoroutineHeader RtCoroutineHeader;
typedef RtCoroStatus (*RtCoroResumeFn)(RtCoroutineHeader*);

// Legacy generated coroutine frames: prefix is RtCoroutineHeader.
struct RtCoroutineHeader {
  RtCoroResumeFn resume;
  LegacyPromiseRef promise;
  uint32_t state;
  uint32_t await_is_error;
  ValueRef await_value;
  ValueRef await_error;
};

LegacyPromiseRef rt_async_spawn_legacy(RtCoroutineHeader* coro);
bool rt_async_poll_legacy(void);
LegacyPromiseRef rt_async_sleep_legacy(uint64_t delay_ms);
void rt_coro_await_legacy(RtCoroutineHeader* coro, LegacyPromiseRef awaited, uint32_t next_state);

// -----------------------------------------------------------------------------
// Microtasks + timers (queueMicrotask/setTimeout/setInterval)
// -----------------------------------------------------------------------------
// Enqueue a microtask to run during the next microtask checkpoint (end of the current macrotask,
// or during `rt_async_poll_legacy` when the event loop is otherwise idle).
void rt_queue_microtask(void (*cb)(uint8_t*), uint8_t* data);

// Timers. Timer callbacks are macrotasks; after each timer callback, `rt_async_poll_legacy` runs a
// microtask checkpoint. This is a minimal API surface; HTML-specific clamping (e.g. nested 4ms
// clamp) is handled at higher layers.
TimerId rt_set_timeout(void (*cb)(uint8_t*), uint8_t* data, uint64_t delay_ms);
TimerId rt_set_interval(void (*cb)(uint8_t*), uint8_t* data, uint64_t interval_ms);
void rt_clear_timer(TimerId id);

// -----------------------------------------------------------------------------
// I/O watchers (epoll-backed readiness notifications)
// -----------------------------------------------------------------------------
IoWatcherId rt_io_register(
  int32_t fd,
  uint32_t interests,
  void (*cb)(uint32_t events, uint8_t* data),
  uint8_t* data
);
void rt_io_update(IoWatcherId id, uint32_t interests);
void rt_io_unregister(IoWatcherId id);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // ECMA_RS_RUNTIME_NATIVE_H
