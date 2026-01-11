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
// GC handle ABI helpers
// -----------------------------------------------------------------------------
// Under a moving GC, runtime entrypoints that may safepoint/GC must not accept raw
// GC pointers unless they are pinned. Instead they take a handle: a pointer to a
// caller-owned root slot (`*mut *mut u8` in Rust).
typedef uint8_t* GcPtr;
typedef uint8_t** GcHandle;

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

// -----------------------------------------------------------------------------
// Promise resolution ABI (PromiseResolve / thenable assimilation)
// -----------------------------------------------------------------------------

// Tag for PromiseResolveInput.
typedef uint8_t PromiseResolveKind;
enum {
  RT_PROMISE_RESOLVE_VALUE = 0,
  RT_PROMISE_RESOLVE_PROMISE = 1,
  RT_PROMISE_RESOLVE_THENABLE = 2,
};

typedef struct PromiseResolveInput PromiseResolveInput;

// Thenable ("PromiseLike") vtable.
typedef struct ThenableVTable {
  // Call `thenable.then(on_fulfilled, on_rejected)`.
  //
  // Returns non-null ValueRef if calling `then` synchronously "throws".
  ValueRef (*call_then)(
    uint8_t* thenable,
    void (*on_fulfilled)(uint8_t* data, PromiseResolveInput value),
    void (*on_rejected)(uint8_t* data, ValueRef reason),
    uint8_t* data
  );
} ThenableVTable;

typedef struct ThenableRef {
  const ThenableVTable* vtable;
  uint8_t* ptr;
} ThenableRef;

typedef union PromiseResolvePayload {
  ValueRef value;
  LegacyPromiseRef promise;
  ThenableRef thenable;
} PromiseResolvePayload;

struct PromiseResolveInput {
  PromiseResolveKind kind;
  PromiseResolvePayload payload;
};
// Payload layout for promises returned from `rt_parallel_spawn_promise`.
//
// The runtime allocates a payload buffer described by this struct. The worker
// task writes the result into `rt_promise_payload_ptr(promise)` and then calls
// `rt_promise_fulfill` (or `rt_promise_reject`).
typedef struct PromiseLayout {
  size_t size;
  size_t align;
} PromiseLayout;

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
// Compatibility aliases for earlier codegen prototypes.
void rt_register_thread(void);
void rt_unregister_thread(void);

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
//
// Alignment: the returned pointer is aligned to at least the registered shape
// descriptor's `align` value (`RtShapeDescriptor.align`).
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

// Cheap leaf poll used by compiler-inserted loop backedge safepoints.
// Returns true if a stop-the-world GC is currently requested.
bool rt_gc_poll(void);
void rt_gc_safepoint(void);
// Enter a safepoint and return the (possibly relocated) pointer stored in `slot`.
//
// This is an ABI helper for `may_gc` runtime entrypoints that accept GC-managed pointers as
// pointer-to-slot handles.
GcPtr rt_gc_safepoint_relocate_h(GcHandle slot);
// Safepoint slow path entered only when `RT_GC_EPOCH` is odd (stop-the-world requested).
// Callers should pass the observed odd epoch value.
void rt_gc_safepoint_slow(uint64_t epoch);
// Prevent the compiler from considering `gc_ref` dead before a raw pointer derived
// from it is finished being used.
//
// This is used when generated/native code derives a non-GC pointer (e.g. an
// ArrayBuffer backing-store `uint8_t*`) from a GC-managed object header and then
// may hit a safepoint/GC before its last use.
void rt_keep_alive_gc_ref(uint8_t* gc_ref);
// Generational write barrier for an object field store.
//
// Contract: `obj` must be the same object base pointer that was returned from
// `rt_alloc` (i.e. pointer to the start of the object's header), and `slot` must
// point to the field being written.
void rt_write_barrier(uint8_t* obj, uint8_t* slot);
// Generational range write barrier for bulk writes.
//
// Contract: called after a bulk write into `obj`.
// - `start_slot` points within `obj` to the first written byte (typically the first pointer slot).
// - `len` is the number of bytes written starting at `start_slot`.
//
// This barrier is conservative and may over-mark cards (it does not inspect the written values).
void rt_write_barrier_range(uint8_t* obj, uint8_t* start_slot, size_t len);
void rt_gc_collect(void);
// Bytes currently owned by non-moving `ArrayBuffer`/`TypedArray` backing stores (allocated outside
// the GC heap).
size_t rt_backing_store_external_bytes(void);

// -----------------------------------------------------------------------------
// GC roots / handles
// -----------------------------------------------------------------------------
// LLVM stackmaps cover mutator stack/register roots, but the runtime must also
// track:
// - temporary roots created by runtime-native/FFI code (shadow stack), and
// - global/static roots and long-lived handles.
//
// Temporary roots on the current thread (shadow stack).
//
// `slot` must point to a caller-owned `GcPtr` slot and must be popped in strict
// LIFO order.
void rt_root_push(GcHandle slot);
void rt_root_pop(GcHandle slot);

// Register an addressable root slot. `slot` must remain valid and writable
// until unregistered.
uint32_t rt_gc_register_root_slot(GcHandle slot);
void rt_gc_unregister_root_slot(uint32_t handle);
// Convenience: allocate an internal slot initialized to `ptr` and register it
// as a root. The returned handle must later be passed to `rt_gc_unpin`.
uint32_t rt_gc_pin(GcPtr ptr);
void rt_gc_unpin(uint32_t handle);

// Update the active nursery (young generation) address range used by the write barrier.
// Must be called by the GC at initialization and after each nursery flip/resize.
void rt_gc_set_young_range(uint8_t* start, uint8_t* end);

// Debug/test helper: read the current young-space range.
void rt_gc_get_young_range(GcPtr* out_start, GcPtr* out_end);

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
// Threading / safepoints
// -----------------------------------------------------------------------------

// Runtime thread kind values for rt_thread_register(uint32_t kind).
//
// These are part of the stable compiler/runtime ABI contract:
// - 0: Main
// - 1: Worker
// - 2: Io
// - 3: External
#define RT_THREAD_KIND_MAIN 0u
#define RT_THREAD_KIND_WORKER 1u
#define RT_THREAD_KIND_IO 2u
#define RT_THREAD_KIND_EXTERNAL 3u

// Register the current OS thread with the runtime thread registry (idempotent).
// Returns a stable runtime-assigned thread id.
uint64_t rt_thread_register(uint32_t kind);

// Unregister the current OS thread from the runtime thread registry.
void rt_thread_unregister(void);

// Mark/unmark the current thread as parked (idle) inside the runtime.
//
// IMPORTANT: When `parked == false` (unparking), this function performs a safepoint poll
// before returning (fast path if no stop-the-world is requested).
void rt_thread_set_parked(bool parked);

// -----------------------------------------------------------------------------
// Strings
// -----------------------------------------------------------------------------
StringRef rt_string_concat(const uint8_t* a, size_t a_len, const uint8_t* b, size_t b_len);
InternedId rt_string_intern(const uint8_t* s, size_t len);
void rt_string_pin_interned(InternedId id);

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
// Parallel-for convenience API.
//
// Executes `body(i, data)` for each `i` in `[start, end)`. If `end <= start` it is a no-op.
//
// The runtime may fall back to sequential execution for small ranges (to avoid task overhead) or
// when configured with a single worker thread.
//
// Adaptive chunking:
// - target chunks: workers * 4
// - minimum iterations per task: RT_PAR_FOR_MIN_GRAIN (default: 1024)
void rt_parallel_for(size_t start, size_t end, void (*body)(size_t, uint8_t*), uint8_t* data);

// Spawn CPU-bound work on the parallel worker pool and return a promise that can
// be awaited by the async runtime.
PromiseRef rt_parallel_spawn_promise(void (*task)(uint8_t*, PromiseRef), uint8_t* data, PromiseLayout layout);

// -----------------------------------------------------------------------------
// Blocking thread pool
// -----------------------------------------------------------------------------
// Run `task(data, promise)` on the runtime's dedicated blocking thread pool (for I/O, crypto,
// etc.). The task must resolve/reject `promise` via `rt_promise_resolve_legacy` /
// `rt_promise_reject_legacy`.
//
// Blocking tasks execute in a GC-safe ("NativeSafe") region: they must not touch the GC heap (no
// GC allocations, no write barriers, and no dereferencing GC-managed pointers unless pinned via a
// stable handle).
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
// Returns the payload pointer for promises created by `rt_parallel_spawn_promise`.
uint8_t* rt_promise_payload_ptr(PromiseRef p);

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
  RtShapeId promise_shape_id;
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
// Like rt_async_spawn, but enqueues the coroutine's first resume as a microtask instead of running
// synchronously. This is required for strict microtask semantics (e.g. queueMicrotask).
PromiseRef rt_async_spawn_deferred(CoroutineRef coro);

// Drive the async runtime. Returns true if any work was performed.
bool rt_async_poll(void);

// Configure whether `await` on an already-settled promise yields (strict JS microtask semantics) or
// resumes synchronously (fast-path).
//
// Default is false.
void rt_async_set_strict_await_yields(bool strict);

// -----------------------------------------------------------------------------
// Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates)
// -----------------------------------------------------------------------------
LegacyPromiseRef rt_promise_new_legacy(void);
void rt_promise_resolve_legacy(LegacyPromiseRef p, ValueRef value);
void rt_promise_resolve_into_legacy(LegacyPromiseRef p, PromiseResolveInput value);
void rt_promise_resolve_promise_legacy(LegacyPromiseRef p, LegacyPromiseRef other);
void rt_promise_resolve_thenable_legacy(LegacyPromiseRef p, ThenableRef thenable);
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
// Like rt_async_spawn_legacy, but enqueues the first resume as a microtask instead of running
// synchronously. This is required for strict microtask semantics (e.g. queueMicrotask).
LegacyPromiseRef rt_async_spawn_deferred_legacy(RtCoroutineHeader* coro);
bool rt_async_poll_legacy(void);
LegacyPromiseRef rt_async_sleep_legacy(uint64_t delay_ms);
void rt_coro_await_legacy(RtCoroutineHeader* coro, LegacyPromiseRef awaited, uint32_t next_state);
void rt_coro_await_value_legacy(RtCoroutineHeader* coro, PromiseResolveInput awaited, uint32_t next_state);

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
