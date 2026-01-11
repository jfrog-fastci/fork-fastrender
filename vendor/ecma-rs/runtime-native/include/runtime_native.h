#ifndef ECMA_RS_RUNTIME_NATIVE_H
#define ECMA_RS_RUNTIME_NATIVE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <limits.h>

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
// Stable persistent handle id (safe to store in OS event loop userdata like epoll_event.data.u64).
typedef uint64_t HandleId;

// Microtask callback scheduled onto the async runtime microtask queue.
//
// Contract:
// - `func` must be non-null.
// - `data` must remain valid until `func(data)` runs.
typedef struct Microtask {
  void (*func)(uint8_t* data);
  uint8_t* data;
} Microtask;

// runtime-native does not yet implement a full JS value representation/GC.
// For now, values are passed as opaque pointers.
typedef void* ValueRef;

// -----------------------------------------------------------------------------
// GC handle ABI helpers
// -----------------------------------------------------------------------------
// Under a moving GC, runtime entrypoints that may safepoint/GC must not accept raw
// GC pointers unless they are pinned. Instead they take a handle: a pointer to a
// caller-owned root slot (`*mut *mut u8` in Rust).
//
// `GcPtr` is an **object base pointer**: it points at the start of the GC header
// (`ObjHeader` in the Rust implementation), not to the payload after the header.
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

// TLS pointer to the current thread record (set by `rt_thread_attach`).
//
// Generated code may load this symbol directly to access per-thread state with
// minimal overhead.
#if defined(__cplusplus)
extern thread_local Thread* RT_THREAD;
#elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L)
extern _Thread_local Thread* RT_THREAD;
#elif defined(__GNUC__) || defined(__clang__)
extern __thread Thread* RT_THREAD;
#else
// No TLS storage specifier available; declare as a normal extern as a best-effort fallback.
// Consumers that rely on per-thread state must compile with TLS support.
extern Thread* RT_THREAD;
#endif

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
GcPtr rt_alloc(size_t size, RtShapeId shape);
// Allocate a pinned (non-moving) object. Pinned objects are intended for FFI /
// host embeddings that require stable addresses.
GcPtr rt_alloc_pinned(size_t size, RtShapeId shape);

// -----------------------------------------------------------------------------
// Arrays (`rt_alloc_array`)
// -----------------------------------------------------------------------------
// `rt_alloc_array` allocates a GC-managed array object with a fixed-size header
// followed by a contiguous payload of `len * elem_size` bytes.
//
// IMPORTANT:
// - `rt_alloc_array` returns the **object base pointer** (start of the header),
//   consistent with `rt_alloc`.
// - The payload starts at `RT_ARRAY_DATA_OFFSET` bytes after the returned base
//   pointer. Use `RT_ARRAY_DATA_PTR(base)` to compute it.
//
// Pointer element arrays:
// - By default, the runtime treats the payload as raw bytes (no interior GC
//   pointers), even if `elem_size == sizeof(void*)`.
// - To request an array whose payload is a `len`-long sequence of GC pointers,
//   set `RT_ARRAY_ELEM_PTR_FLAG` in the `elem_size` argument:
//     encoded = sizeof(void*) | RT_ARRAY_ELEM_PTR_FLAG
//   The runtime/GC will then trace + update each element slot as a `uint8_t*`
//   object pointer.
//
// This encoding exists so codegen can distinguish between:
// - "pointer-sized scalar bytes" (e.g. `uint64_t`, `double`) and
// - "GC pointer elements" (payload must be traced).

// High-bit encoding in the `elem_size` argument: indicates that the payload
// consists of GC pointers (and must therefore be traced and updated by the GC).
#define RT_ARRAY_ELEM_PTR_FLAG ((size_t)1u << (sizeof(size_t) * CHAR_BIT - 1u))

// Array header flag: payload is a `len`-long sequence of `uint8_t*` GC pointers.
#define RT_ARRAY_FLAG_PTR_ELEMS (1u << 0)

// FFI-stable array header layout. The object base pointer returned from
// `rt_alloc_array` points at the start of this header.
//
// The first two words are the runtime's internal `ObjHeader` (opaque to C):
// - `type_desc`: pointer to runtime type/shape metadata
// - `meta`:      per-object GC metadata bits / forwarding pointer
typedef struct RtArrayHeader {
  const void* type_desc;
  size_t meta;
  size_t len;
  uint32_t elem_size;
  uint32_t elem_flags;
#if defined(__cplusplus)
  // Flexible array members are not standard C++; use a 1-byte trailing field to
  // keep the header usable from C++ while still computing the correct payload
  // offset via `offsetof(RtArrayHeader, data)`.
  uint8_t data[1];
#else
  uint8_t data[];
#endif
} RtArrayHeader;

// Byte offset from the object base pointer to the start of the array payload.
#define RT_ARRAY_DATA_OFFSET offsetof(RtArrayHeader, data)

// Encode an `elem_size` value that requests a pointer-element array.
#define RT_ARRAY_ENCODE_PTR_ELEM_SIZE() (sizeof(void*) | RT_ARRAY_ELEM_PTR_FLAG)

// Compute the pointer to the element payload for an array base pointer.
#define RT_ARRAY_DATA_PTR(base) ((uint8_t*)(base) + RT_ARRAY_DATA_OFFSET)

GcPtr rt_alloc_array(size_t len, size_t elem_size);
uint8_t* rt_alloc_ptr_array(size_t len);

// -----------------------------------------------------------------------------
// Arrays
// -----------------------------------------------------------------------------
size_t rt_array_len(uint8_t* obj);
uint8_t* rt_array_data(uint8_t* obj);

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
void rt_keep_alive_gc_ref(GcPtr gc_ref);
// Generational write barrier for an object field store.
//
// Contract: `obj` must be the same object base pointer that was returned from
// `rt_alloc` (i.e. pointer to the start of the object's header), and `slot` must
// point to the field being written.
void rt_write_barrier(GcPtr obj, uint8_t* slot);
// Generational range write barrier for bulk writes.
//
// Contract: called after a bulk write into `obj`.
// - `start_slot` points within `obj` to the first written byte (typically the first pointer slot).
// - `len` is the number of bytes written starting at `start_slot`.
//
// This barrier is conservative and may over-mark cards (it does not inspect the written values).
void rt_write_barrier_range(GcPtr obj, uint8_t* start_slot, size_t len);
void rt_gc_collect(void);
// Bytes currently owned by non-moving `ArrayBuffer`/`TypedArray` backing stores (allocated outside
// the GC heap).
size_t rt_backing_store_external_bytes(void);

// -----------------------------------------------------------------------------
// LLVM stackmaps (precise stack scanning)
// -----------------------------------------------------------------------------
//
// Native code may be delivered as multiple DSOs (`dlopen`) or generated at
// runtime (JIT). Each module can expose its own `.llvm_stackmaps` blob and
// register it explicitly into the global runtime registry.
//
// The common ELF setup is to link the module with `runtime-native/link/stackmaps.ld`,
// which defines `__llvm_stackmaps_start` / `__llvm_stackmaps_end` symbols for the
// module's stackmaps output section.
bool rt_stackmaps_register(const uint8_t* start, const uint8_t* end);
bool rt_stackmaps_unregister(const uint8_t* start);

// Convenience helper for ELF DSOs: emit a constructor that registers this
// module's stackmaps at load time.
//
// Usage:
//   RT_STACKMAPS_AUTO_REGISTER();
//
// (Call once per module.)
#if defined(__GNUC__) && !defined(_WIN32)
#define RT_STACKMAPS_AUTO_REGISTER()                                                \
  static void __attribute__((constructor)) __rt_stackmaps_ctor(void) {              \
    extern uint8_t __llvm_stackmaps_start;                                          \
    extern uint8_t __llvm_stackmaps_end;                                            \
    (void)rt_stackmaps_register(&__llvm_stackmaps_start, &__llvm_stackmaps_end);    \
  }
#else
#define RT_STACKMAPS_AUTO_REGISTER()
#endif

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

// Register a global/static root slot (word-sized `usize` in Rust / `size_t` in C).
//
// This is intended for GC pointers stored in statics or long-lived runtime state. The GC will
// update `*slot` in place if it relocates the referenced object.
//
// Contract: `slot` must remain valid and writable until unregistered.
void rt_global_root_register(size_t* slot);
void rt_global_root_unregister(size_t* slot);

// Register an addressable root slot. `slot` must remain valid and writable
// until unregistered.
uint32_t rt_gc_register_root_slot(GcHandle slot);
void rt_gc_unregister_root_slot(uint32_t handle);
// Convenience: allocate an internal slot initialized to `ptr` and register it
// as a root. The returned handle must later be passed to `rt_gc_unpin`.
uint32_t rt_gc_pin(GcPtr ptr);
void rt_gc_unpin(uint32_t handle);
// Read/write the current pointer value stored in a root handle.
//
// This allows host/async code to store only the `uint32_t` handle across async boundaries
// (instead of a raw `GcPtr`, which may be relocated by the GC).
GcPtr rt_gc_root_get(uint32_t handle);
bool rt_gc_root_set(uint32_t handle, GcPtr ptr);

// -----------------------------------------------------------------------------
// Persistent handles (stable u64 ids)
// -----------------------------------------------------------------------------
// Persistent handles keep a GC-managed object alive and allow retrieving its (possibly relocated)
// pointer later. Intended for crossing async/OS/thread boundaries where raw pointers are unsafe.
HandleId rt_handle_alloc(GcPtr ptr);
void rt_handle_free(HandleId handle);
GcPtr rt_handle_load(HandleId handle);
void rt_handle_store(HandleId handle, GcPtr ptr);

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

// Expensive GC verification helpers (heap integrity checks, debug shape-table queries).
//
// These entrypoints are only available when `runtime-native` was built with the
// Cargo feature `gc_debug`. Define `RUNTIME_NATIVE_GC_DEBUG` to expose them in C.
#ifdef RUNTIME_NATIVE_GC_DEBUG
// Return the number of shapes registered via `rt_register_shape_table`.
size_t rt_debug_shape_count(void);
// Return a pointer to the registered descriptor for `id`, or NULL if invalid/out-of-bounds.
const RtShapeDescriptor* rt_debug_shape_descriptor(RtShapeId id);
// Run expensive heap validation checks (panics on failure).
void rt_debug_validate_heap(void);
#endif

// -----------------------------------------------------------------------------
// Weak references (weak handles)
// -----------------------------------------------------------------------------
uint64_t rt_weak_add(GcPtr value);
GcPtr rt_weak_get(uint64_t handle);
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
// available CPU parallelism; override it by setting `ECMA_RS_RUNTIME_NATIVE_THREADS` (or the
// legacy alias `RT_NUM_THREADS`) to a positive integer before first use.
// Schedule `task(data)` onto the runtime's global worker pool.
//
// Contract:
// - `data` must remain valid until the returned `TaskId` is passed to `rt_parallel_join`.
// - The returned `TaskId` must be joined exactly once.
TaskId rt_parallel_spawn(void (*task)(uint8_t*), uint8_t* data);
// Like `rt_parallel_spawn`, but `data` is a GC-managed object that the runtime
// will keep alive until `task` finishes executing.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until the task completes.
// - The returned `TaskId` must still be joined exactly once.
TaskId rt_parallel_spawn_rooted(void (*task)(uint8_t*), uint8_t* data);
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
bool rt_promise_try_fulfill(PromiseRef p);
void rt_promise_reject(PromiseRef p);
bool rt_promise_try_reject(PromiseRef p);
// Mark a promise as handled for unhandled-rejection tracking.
void rt_promise_mark_handled(PromiseRef p);
// Returns the payload pointer for promises created by `rt_parallel_spawn_promise`.
uint8_t* rt_promise_payload_ptr(PromiseRef p);

// -----------------------------------------------------------------------------
// Native coroutine ABI (async/await lowering)
// -----------------------------------------------------------------------------
typedef struct Coroutine Coroutine;
typedef struct CoroutineVTable CoroutineVTable;
typedef Coroutine* CoroutineRef;
// Stable handle to a coroutine frame.
//
// Coroutines may be relocated by a moving/compacting GC, and coroutine IDs may be stored in host
// queues / OS event-loop userdata across async boundaries. As a result, the native async ABI uses a
// `CoroutineId` handle instead of a raw `Coroutine*` pointer.
typedef uint64_t CoroutineId;

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

// Async ABI version tag for coroutine frames/vtables.
//
// Guard against RT_ASYNC_ABI_VERSION being defined as a macro by other headers; otherwise the
// preprocessor would substitute the name inside the enum definition.
#ifndef RT_ASYNC_ABI_VERSION
enum { RT_ASYNC_ABI_VERSION = 1 };
#endif

struct CoroutineVTable {
  CoroutineResumeFn resume;
  // Destroy (drop + deallocate) a coroutine frame.
  //
  // If the coroutine frame is runtime-owned (`CORO_FLAG_RUNTIME_OWNS_FRAME` set in `coro.flags`),
  // the runtime will call this exactly once after completion or cancellation.
  void (*destroy)(CoroutineRef coro);
  // Allocation size in bytes of the coroutine's result promise (`Promise<T>`).
  // Must be >= sizeof(PromiseHeader).
  uint32_t promise_size;
  // Allocation alignment of the coroutine's result promise (`Promise<T>`).
  // Must be a power of two and >= alignof(PromiseHeader).
  uint32_t promise_align;
  RtShapeId promise_shape_id;
  // Must equal `RT_ASYNC_ABI_VERSION`.
  uint32_t abi_version;
  // Reserved for future ABI extensions; must be zeroed by generated code.
  uintptr_t reserved[4];
};

// Generated coroutine frames are structs whose prefix is `Coroutine`.
struct Coroutine {
  const CoroutineVTable* vtable;
  PromiseRef promise;
  Coroutine* next_waiter;
  uint32_t flags;
};

// `Coroutine.flags` bitfield.
enum {
  // When set, the runtime owns the coroutine frame and will call `vtable->destroy(coro)` exactly
  // once after completion or cancellation.
  CORO_FLAG_RUNTIME_OWNS_FRAME = 1u << 0,
};

// Spawn an async coroutine and return its result promise.
//
// Contract:
// - `coro` is a stable handle to a coroutine frame whose prefix matches `struct Coroutine`.
// - The runtime *consumes* the handle: it keeps it alive while the coroutine is pending, and frees
//   the handle when the coroutine completes (or is cancelled).
PromiseRef rt_async_spawn(CoroutineId coro);
// Like rt_async_spawn, but enqueues the coroutine's first resume as a microtask instead of running
// synchronously. This is required for strict microtask semantics (e.g. queueMicrotask).
PromiseRef rt_async_spawn_deferred(CoroutineId coro);
// Cancel all queued runtime-owned coroutine frames.
void rt_async_cancel_all(void);

// Drive the native async scheduler (microtasks).
//
// This is a **non-blocking** poll: it drains currently queued microtasks (promise reaction jobs,
// `queueMicrotask`, deferred coroutine spawns, etc) but does not wait for timers or I/O readiness.
//
// Returns:
// - true  if it executed at least one microtask
// - false if there was no runnable microtask work
//
// To drive timers and I/O watchers, use `rt_async_poll_legacy` (JS-shaped event loop) or block in
// `rt_async_wait`.
bool rt_async_poll(void);

// Block until at least one async task becomes ready.
void rt_async_wait(void);

// Configure whether `await` on an already-settled promise yields (strict JS microtask semantics) or
// resumes synchronously (fast-path).
//
// Default is false.
void rt_async_set_strict_await_yields(bool strict);

// Drive the async runtime until there is no immediately-ready work remaining (microtask checkpoint).
// Returns true if any work was executed, false if already idle.
bool rt_async_run_until_idle(void);

// Block the current thread until the promise is settled.
void rt_async_block_on(PromiseRef p);

// -----------------------------------------------------------------------------
// Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates)
// -----------------------------------------------------------------------------
// Compatibility aliases (older codegen used unsuffixed names).
LegacyPromiseRef rt_promise_new(void);
void rt_promise_resolve(LegacyPromiseRef p, ValueRef value);
void rt_promise_then(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);

// Forward declare legacy coroutine headers so compatibility aliases can use the type.
typedef struct RtCoroutineHeader RtCoroutineHeader;
void rt_coro_await(RtCoroutineHeader* coro, LegacyPromiseRef awaited, uint32_t next_state);

LegacyPromiseRef rt_promise_new_legacy(void);
void rt_promise_resolve_legacy(LegacyPromiseRef p, ValueRef value);
void rt_promise_resolve_into_legacy(LegacyPromiseRef p, PromiseResolveInput value);
void rt_promise_resolve_promise_legacy(LegacyPromiseRef p, LegacyPromiseRef other);
void rt_promise_resolve_thenable_legacy(LegacyPromiseRef p, ThenableRef thenable);
void rt_promise_reject_legacy(LegacyPromiseRef p, ValueRef err);
void rt_promise_then_legacy(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);
void rt_promise_then_with_drop_legacy(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*));
void rt_promise_drop_legacy(LegacyPromiseRef p);

typedef enum RtCoroStatus {
  RT_CORO_DONE = 0,
  RT_CORO_PENDING = 1,
  RT_CORO_YIELD = 2,
} RtCoroStatus;

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

// Configure defensive limits for the async runtime.
//
// `max_queue_len == 0` disables the ready queue length limit.
void rt_async_set_limits(size_t max_steps, size_t max_queue_len);

// Take the last async runtime error message, if any.
//
// The returned string is allocated by the runtime and must be freed with
// `rt_async_free_c_string`.
char* rt_async_take_last_error(void);
void rt_async_free_c_string(char* s);

// -----------------------------------------------------------------------------
// Microtasks + timers (queueMicrotask/setTimeout/setInterval)
// -----------------------------------------------------------------------------

// Resolve a promise after `delay_ms` milliseconds.
PromiseRef rt_async_sleep(uint64_t delay_ms);
// Enqueue a microtask to run during the next microtask checkpoint (end of the current macrotask,
// or during `rt_async_poll_legacy` when the event loop is otherwise idle).
//
// Microtasks are executed FIFO in the same queue as promise reaction jobs (e.g. async/await
// coroutine wakeups).
void rt_queue_microtask(Microtask task);
void rt_queue_microtask_with_drop(void (*cb)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*));
// Like `rt_queue_microtask`, but `data` is a GC-managed object that the runtime
// will keep alive until `cb` runs.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until the microtask executes.
void rt_queue_microtask_rooted(void (*cb)(uint8_t*), uint8_t* data);
// Drain only the microtask queue (does not run timers/reactor/macrotasks).
// Returns true if any microtasks were executed.
bool rt_drain_microtasks(void);

// Timers. Timer callbacks are macrotasks; after each timer callback, `rt_async_poll_legacy` runs a
// microtask checkpoint. This is a minimal API surface; HTML-specific clamping (e.g. nested 4ms
// clamp) is handled at higher layers.
TimerId rt_set_timeout(void (*cb)(uint8_t*), uint8_t* data, uint64_t delay_ms);
// Like `rt_set_timeout`, but `data` is a GC-managed object that the runtime will keep alive until
// the timer fires (or is cleared).
TimerId rt_set_timeout_rooted(void (*cb)(uint8_t*), uint8_t* data, uint64_t delay_ms);
TimerId rt_set_timeout_with_drop(void (*cb)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*), uint64_t delay_ms);
TimerId rt_set_interval(void (*cb)(uint8_t*), uint8_t* data, uint64_t interval_ms);
// Like `rt_set_interval`, but `data` is a GC-managed object that the runtime will keep alive until
// the interval is cleared.
TimerId rt_set_interval_rooted(void (*cb)(uint8_t*), uint8_t* data, uint64_t interval_ms);
TimerId rt_set_interval_with_drop(void (*cb)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*), uint64_t interval_ms);
void rt_clear_timer(TimerId id);

// -----------------------------------------------------------------------------
// I/O watchers (epoll-backed readiness notifications)
// -----------------------------------------------------------------------------
//
// Contract:
// - `fd` must be set to `O_NONBLOCK` before registration.
// - `interests` must include `RT_IO_READABLE` and/or `RT_IO_WRITABLE` (it must not be 0).
// - Readiness notifications are edge-triggered; consumers must drain reads/writes
//   until they return `EAGAIN`/`WouldBlock`.
// - `rt_io_register` returns 0 on failure.
IoWatcherId rt_io_register(int32_t fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), uint8_t* data);
IoWatcherId rt_io_register_with_drop(int32_t fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), uint8_t* data, void (*drop_data)(uint8_t* data));
// Like `rt_io_register`, but `data` is a GC-managed object that the runtime will keep alive until
// the watcher is unregistered.
IoWatcherId rt_io_register_rooted(int32_t fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), uint8_t* data);
void rt_io_update(IoWatcherId id, uint32_t interests);
void rt_io_unregister(IoWatcherId id);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // ECMA_RS_RUNTIME_NATIVE_H
