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

// Opaque fixed-size prefix matching the runtime's `ObjHeader` layout.
//
// The runtime GC header is currently two machine words:
//   { type_desc: *const TypeDescriptor, meta: AtomicUsize }
//
// C codegen must treat this as opaque and must not read/write it directly; it is initialized by
// `rt_alloc` / `rt_alloc_pinned`.
typedef struct RtGcPrefix {
  uintptr_t _opaque[2];
} RtGcPrefix;

typedef uint32_t InternedId;
// Reserved invalid/sentinel InternedId value.
//
// This value is never returned by `rt_string_intern`. It can be used by callers as a "no interned
// string" marker, and `rt_string_lookup(RT_INTERNED_ID_INVALID)` will return `{ptr = NULL, len = 0}`.
#define RT_INTERNED_ID_INVALID ((InternedId)UINT32_MAX)
typedef uint64_t TaskId;
typedef uint64_t TimerId;
// Stable persistent handle id (safe to store in OS event loop userdata like epoll_event.data.u64).
typedef uint64_t HandleId;

// Microtask callback scheduled onto the async runtime microtask queue.
//
// Contract:
// - `func` must be non-null.
// - `data` must remain valid until `func(data)` runs.
// - If the microtask is discarded without running (e.g. `rt_async_cancel_all`), the runtime calls
//   `drop(data)` if non-null.
typedef struct Microtask {
  void (*func)(uint8_t* data);
  uint8_t* data;
  void (*drop)(uint8_t* data);
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
// GC-allocated `Promise<T>` objects and coroutine frames.
//
// Promise objects and coroutine frames are normal GC objects: the pointer passed around in the ABI
// is the **object base pointer** (start of the runtime GC header / ObjHeader).
//
// Each `Promise<T>` begins with a `PromiseHeader` prefix at offset 0 (and that prefix itself begins
// with the GC header). The promise payload begins immediately after `PromiseHeader` (layout chosen
// by the compiler/codegen).
//
// The `PromiseHeader` layout is defined by the Rust ABI types in `runtime-native/src/async_abi.rs`;
// for C callers/codegen this header treats it as opaque.
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
//
// Note: this payload buffer is treated as raw bytes and is **not traced by the GC**. If the payload
// contains GC pointers, use `rt_parallel_spawn_promise_with_shape` instead.
typedef struct PromiseLayout {
  size_t size;
  size_t align;
} PromiseLayout;

// An FFI-friendly UTF-8 byte string reference.
//
// IMPORTANT: `StringRef` values may be either:
// - owned buffers allocated by the runtime (must be freed via `rt_string_free`), or
// - borrowed views into runtime-managed memory (must NOT be freed).
//
// The ownership/lifetime contract is defined by the API that produced the `StringRef` value.
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
// `kind` is a best-effort hint used for diagnostics only. Prefer passing a
// `RtThreadKind` value and use `rt_thread_register` when possible.
typedef enum RtThreadKind {
  RT_THREAD_MAIN = 0,
  RT_THREAD_WORKER = 1,
  RT_THREAD_IO = 2,
  RT_THREAD_EXTERNAL = 3,
} RtThreadKind;

void rt_thread_init(uint32_t kind);
void rt_thread_deinit(void);
// Convenience registration API: equivalent to `rt_thread_init(1 /* worker */)`.
void rt_register_current_thread(void);
void rt_unregister_current_thread(void);
// Compatibility aliases for earlier codegen prototypes.
void rt_register_thread(void);
void rt_unregister_thread(void);

// Register the current OS thread with the runtime and return a runtime-assigned
// thread id (stable for the lifetime of the registration).
uint64_t rt_thread_register(RtThreadKind kind);
void rt_thread_unregister(void);

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
//
// Note: when linking against `libruntime_native.so`, some toolchains may not
// export TLS variables like `RT_THREAD` into the dynamic symbol table. Prefer
// storing the `Thread*` returned by `rt_thread_attach` in embedder-owned
// TLS/state, or use `rt_thread_current()` as a dynamic-link-friendly accessor.
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

// Attach/detach is a compatibility API that also participates in the global mutator thread
// registry:
// - If the current OS thread is not yet registered (via `rt_thread_register` / `rt_thread_init`),
//   `rt_thread_attach` will register it as `External`.
// - `rt_thread_detach` will unregister only if attach performed the registration.
Thread* rt_thread_current(void);
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
// Allocation policy (high level):
// - Small objects are allocated into the moving nursery (young generation).
// - Objects promoted out of the nursery reside in the old generation (Immix).
// - Large objects are allocated in the large-object space (LOS).
//
// Alignment: the returned pointer is aligned to at least the registered shape
// descriptor's `align` value (`RtShapeDescriptor.align`).
GcPtr rt_alloc(size_t size, RtShapeId shape);
// Allocate a pinned (non-moving) object. Pinned objects are intended for FFI /
// host embeddings that require stable addresses.
//
// Pinned objects are allocated in the LOS and are never moved by the GC. They are still traced and
// reclaimed when unreachable.
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
// - Arrays follow the same allocation policy as `rt_alloc`: they may be allocated in the moving
//   nursery and therefore must be treated as relocatable across `MayGC` calls.
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

// Append additional shapes to the global runtime shape registry.
//
// This is intended for dlopen/JIT-style embeddings that load additional native
// modules after process initialization.
//
// The runtime copies all descriptor metadata into process-owned memory so the
// caller does not need to keep `table` (or any of its `ptr_offsets` arrays)
// alive after this call returns.
//
// Returns the first assigned shape id for the appended block:
//   base, base+1, ..., base+len-1
RtShapeId rt_register_shape_table_extend(const RtShapeDescriptor* table, size_t len);

// Convenience wrapper: register a single shape descriptor and return its id.
RtShapeId rt_register_shape(const RtShapeDescriptor* desc);

// Register a shape table by appending it to the process-global shape-id space.
//
// Returns the base id assigned to the first descriptor in this table (1-indexed).
// A module's local shape index `i` (0-based) maps to global `RtShapeId(base + i)`.
//
// Note: `rt_register_shape_table_append` and `rt_register_shape_table_extend` are equivalent;
// prefer the `*_extend` name for new code.
RtShapeId rt_register_shape_table_append(const RtShapeDescriptor* table, size_t len);

// -----------------------------------------------------------------------------
// GC heap configuration (process-global heap)
// -----------------------------------------------------------------------------
// These APIs allow embedders to tune GC sizing/policy for the process-global heap
// used by `rt_alloc*` and `rt_gc_collect`.
//
// Configuration must be applied before the process-global heap is initialized
// (i.e. before the first `rt_thread_init`, `rt_alloc`, `rt_gc_collect`, etc).
// If the heap has already been initialized, setters return false.
//
// Optional environment-variable overrides (read once at heap initialization).
//
// These override the runtime defaults only; if an embedder successfully calls `rt_gc_set_config` /
// `rt_gc_set_limits` before heap initialization, the corresponding env vars are ignored.
// - ECMA_RS_GC_NURSERY_MB
// - ECMA_RS_GC_MAX_HEAP_MB
// - ECMA_RS_GC_MAX_TOTAL_MB
typedef struct RtGcConfig {
  // Size of the nursery (young generation), in bytes.
  size_t nursery_size_bytes;
  // Allocation size threshold above which objects go to the large object space (LOS), in bytes.
  size_t los_threshold_bytes;
  // Trigger a minor collection when nursery usage exceeds this percentage (0..=100).
  uint8_t minor_gc_nursery_used_percent;
  // Trigger a major collection when old-generation live bytes exceed this threshold, in bytes.
  size_t major_gc_old_bytes_threshold;
  // Trigger a major collection when the old generation owns more than this number of Immix blocks.
  size_t major_gc_old_blocks_threshold;
  // Trigger a major collection when external (non-GC) bytes exceed this threshold, in bytes.
  size_t major_gc_external_bytes_threshold;
  // Promotion policy: promote an object after it has survived this many minor collections (>= 1).
  uint8_t promote_after_minor_survivals;
} RtGcConfig;

typedef struct RtGcLimits {
  // Hard cap on GC heap usage, in bytes.
  size_t max_heap_bytes;
  // Hard cap on total memory usage including external (non-GC) allocations, in bytes.
  size_t max_total_bytes;
} RtGcLimits;

bool rt_gc_set_config(const RtGcConfig* cfg);
bool rt_gc_set_limits(const RtGcLimits* limits);
bool rt_gc_get_config(RtGcConfig* out_cfg);
bool rt_gc_get_limits(RtGcLimits* out_limits);

// -----------------------------------------------------------------------------
// GC entrypoints (stop-the-world)
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
// Trigger a stop-the-world GC cycle.
//
// This may relocate nursery (young generation) objects. Callers must treat GC pointers as
// relocatable across this call:
// - compiled code relies on LLVM statepoints (`gc.relocate`), and
// - runtime/FFI code must root pointers via the handle stack (`rt_root_push` / `rt_root_pop`) or a
//   stable handle (`HandleId`).
void rt_gc_collect(void);
// Trigger a stop-the-world **minor** GC (nursery evacuation only).
void rt_gc_collect_minor(void);
// Trigger a stop-the-world **major** GC (full heap). This is an alias for `rt_gc_collect`.
void rt_gc_collect_major(void);
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
// module's in-memory stackmaps byte range (the payload may be appended into a
// broader RELRO output section like `.data.rel.ro`).
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
// Like `rt_gc_pin`, but takes the pointer as a `GcHandle` (pointer-to-slot).
uint32_t rt_gc_pin_h(GcHandle ptr);
void rt_gc_unpin(uint32_t handle);
// Read/write the current pointer value stored in a root handle.
//
// This allows host/async code to store only the `uint32_t` handle across async boundaries
// (instead of a raw `GcPtr`, which may be relocated by the GC).
GcPtr rt_gc_root_get(uint32_t handle);
bool rt_gc_root_set(uint32_t handle, GcPtr ptr);
// Like `rt_gc_root_set`, but takes the new pointer value as a `GcHandle` (pointer-to-slot).
bool rt_gc_root_set_h(uint32_t handle, GcHandle ptr);

// -----------------------------------------------------------------------------
// Persistent handles (stable u64 ids)
// -----------------------------------------------------------------------------
// Persistent handles provide stable `HandleId` values for crossing async/OS/thread boundaries
// (host-owned queues, OS event loop userdata, cross-thread wakeups, ...).
//
// - If the stored pointer refers to a GC-managed object, it must be the GC *object base pointer*
//   (start of ObjHeader). The GC treats all live handles as roots and may update the stored pointer
//   during relocation/compaction.
// - If the stored pointer does not point into the GC heap, it is ignored by GC tracing: it will not
//   keep any GC object alive and will not be relocated.
//
// Note: HandleId values are non-zero; `0` is reserved as an "invalid"/"none" sentinel.
HandleId rt_handle_alloc(GcPtr ptr);
// Like `rt_handle_alloc`, but takes the pointer as a `GcHandle` (pointer-to-slot).
HandleId rt_handle_alloc_h(GcHandle ptr);
void rt_handle_free(HandleId handle);
GcPtr rt_handle_load(HandleId handle);
void rt_handle_store(HandleId handle, GcPtr ptr);
// Like `rt_handle_store`, but takes the new pointer value as a `GcHandle` (pointer-to-slot).
void rt_handle_store_h(HandleId handle, GcHandle ptr);

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
  uint64_t alloc_bytes;
  uint64_t alloc_array_calls;
  uint64_t alloc_array_bytes;
  uint64_t gc_collect_calls;
  uint64_t safepoint_calls;
  uint64_t write_barrier_calls_total;
  uint64_t write_barrier_range_calls;
  uint64_t write_barrier_old_young_hits;
  uint64_t set_young_range_calls;
  uint64_t thread_init_calls;
  uint64_t thread_deinit_calls;
  uint64_t remembered_objects_added;
  uint64_t remembered_objects_scanned_minor;
  uint64_t card_marks_total;
  uint64_t cards_scanned_minor;
  uint64_t cards_kept_after_rebuild;
} RtGcStatsSnapshot;

void rt_gc_stats_snapshot(RtGcStatsSnapshot* out);
void rt_gc_stats_reset(void);
#endif

// Expensive GC verification helpers (heap integrity checks, debug shape-table queries).
//
// These entrypoints are only available when `runtime-native` was built with the
// Cargo feature `gc_debug`. Define `RUNTIME_NATIVE_GC_DEBUG` to expose them in C.
#ifdef RUNTIME_NATIVE_GC_DEBUG
// Return the number of shapes registered via `rt_register_shape_table*`.
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
uint64_t rt_weak_add_h(GcHandle value);
GcPtr rt_weak_get(uint64_t handle);
void rt_weak_remove(uint64_t handle);

// -----------------------------------------------------------------------------
// Threading / safepoints
// -----------------------------------------------------------------------------

// Legacy numeric thread kind values (kept for compatibility).
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

// Mark/unmark the current thread as parked (idle) inside the runtime.
//
// IMPORTANT: When `parked == false` (unparking), this function performs a safepoint poll
// before returning (fast path if no stop-the-world is requested).
void rt_thread_set_parked(bool parked);

// -----------------------------------------------------------------------------
// Strings
// -----------------------------------------------------------------------------
// Concatenate two UTF-8 byte strings into a new allocation.
//
// Ownership: the returned `StringRef` is allocated by the runtime and must be freed exactly once via
// `rt_string_free` (or the compatibility alias `rt_stringref_free`) when no longer needed.
StringRef rt_string_concat(const uint8_t* a, size_t a_len, const uint8_t* b, size_t b_len);
// Free an owned `StringRef` allocated by `rt_string_concat` or `rt_string_to_owned_utf8`.
//
// IMPORTANT:
// - Passing a borrowed `StringRef` (e.g. from `rt_string_as_utf8`, `rt_string_lookup`,
//   `rt_string_lookup_pinned`) is invalid and the runtime will abort.
//
// This is a no-op for empty string references (`len == 0`), including `{ptr=NULL, len=0}`.
void rt_string_free(StringRef s);
// Free an owned `StringRef` allocated by `rt_string_concat` or `rt_string_to_owned_utf8`.
//
// This is a no-op for empty string references (`len == 0`).
//
// Compatibility alias for older codegen/tests. Prefer `rt_string_free`.
void rt_stringref_free(StringRef s);
// Allocate a GC-managed UTF-8 string and copy `bytes`.
GcPtr rt_string_new_utf8(const uint8_t* bytes, size_t len);
// Concatenate two GC-managed UTF-8 strings into a new GC-managed string.
GcPtr rt_string_concat_gc(GcPtr a, GcPtr b);
// Return the length (in UTF-8 bytes) of a GC-managed string.
size_t rt_string_len(GcPtr s);
// Borrow the UTF-8 bytes of a GC-managed string.
//
// Ownership: the returned `StringRef` is borrowed and must NOT be freed (do not call `rt_string_free` /
// `rt_stringref_free` on it).
//
// The returned view points into the GC heap and is only valid until the next GC safepoint/collection
// (the string may be relocated).
StringRef rt_string_as_utf8(GcPtr s);
// Allocate and return an owned copy of the UTF-8 bytes of a GC-managed string.
//
// The returned `StringRef` must be freed via `rt_string_free` (or `rt_stringref_free`).
StringRef rt_string_to_owned_utf8(GcPtr s);
InternedId rt_string_intern(const uint8_t* s, size_t len);
// Lookup an interned string by stable ID.
//
// Ownership: the returned `StringRef` is borrowed and must NOT be freed (do not call `rt_string_free` /
// `rt_stringref_free` on it).
//
// GC-safety / lifetime contract:
// - The interner may store unpinned entries as weak references to GC-managed objects.
// - For **unpinned** entries, the returned `ptr..ptr+len` may point into a movable GC allocation and
//   is only valid until the next GC safepoint/collection (the bytes may be relocated or reclaimed).
// - For **pinned** entries, the returned `ptr` points to interner-owned non-GC memory and is stable
//   for the lifetime of the process.
// - If you need a GC-stable pointer and want to enforce "pinned-only", use `rt_string_lookup_pinned`.
//
// Return value contract:
// - On success: returns `{ptr, len}` for the UTF-8 bytes.
// - On invalid/reclaimed ID: returns `{ptr = NULL, len = 0}` (distinct from the valid empty string,
//   which returns `{ptr != NULL, len = 0}`).
StringRef rt_string_lookup(InternedId id);
void rt_string_pin_interned(InternedId id);
// Look up the UTF-8 bytes for a **pinned** interned string ID.
//
// GC-safety / lifetime contract:
// - The runtime may use a moving GC for GC-backed interned strings. Returning a raw pointer into a
//   movable GC allocation would be unsafe unless the object is pinned or the bytes are copied out.
// - `rt_string_lookup_pinned` only succeeds for **pinned** interned strings. If the entry is not pinned,
//   this returns false.
// - On success, `out->ptr..out->ptr+out->len` points to non-GC memory owned by the interner and is
//   stable for the lifetime of the process.
// - The returned `StringRef` is borrowed and must NOT be freed (do not call `rt_string_free` /
//   `rt_stringref_free` on it).
// - Returns false if `id` is invalid, was reclaimed, or is not pinned.
//
// Contract:
// - `out` must be non-null.
bool rt_string_lookup_pinned(InternedId id, StringRef* out);

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
// Like `rt_parallel_spawn_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
TaskId rt_parallel_spawn_rooted_h(void (*task)(uint8_t*), GcHandle data);
// Spawn `task(data, promise)` on the runtime's parallel worker pool and return a legacy promise
// settled by the task.
//
// The task is responsible for resolving/rejecting the promise via `rt_promise_resolve_legacy` /
// `rt_promise_reject_legacy`.
LegacyPromiseRef rt_parallel_spawn_promise_legacy(void (*task)(uint8_t* data, LegacyPromiseRef promise), uint8_t* data);
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
// Like `rt_parallel_for`, but `data` is a GC-managed object that the runtime will keep alive (and
// relocatable) until the call returns.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until `rt_parallel_for_rooted` returns.
// - The callback receives the current relocated pointer.
void rt_parallel_for_rooted(size_t start, size_t end, void (*body)(size_t, uint8_t*), uint8_t* data);
// Like `rt_parallel_for_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
void rt_parallel_for_rooted_h(size_t start, size_t end, void (*body)(size_t, uint8_t*), GcHandle data);

// Spawn CPU-bound work on the parallel worker pool and return a promise that can
// be awaited by the async runtime.
//
// While the worker task is outstanding, the runtime will treat the promise as "external pending"
// work: `rt_async_poll` / `rt_async_poll_legacy` will continue to report pending work (and may
// block) until the promise settles.
PromiseRef rt_parallel_spawn_promise(void (*task)(uint8_t*, PromiseRef), uint8_t* data, PromiseLayout layout);
// Like `rt_parallel_spawn_promise`, but `data` is a GC-managed object that the runtime will keep
// alive until the worker task finishes executing.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until the task completes.
// - The worker callback receives the (possibly relocated) pointer after any GC relocation.
PromiseRef rt_parallel_spawn_promise_rooted(void (*task)(uint8_t*, PromiseRef), uint8_t* data, PromiseLayout layout);
// Like `rt_parallel_spawn_promise_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
PromiseRef rt_parallel_spawn_promise_rooted_h(void (*task)(uint8_t*, PromiseRef), GcHandle data, PromiseLayout layout);

// Like `rt_parallel_spawn_promise`, but allocates the promise as a **GC-managed object** with the
// provided `promise_shape`.
//
// This is required when the promise payload contains GC pointers: the runtime uses `promise_shape`
// to precisely trace and update those pointers during moving GC.
//
// The promise payload begins immediately after the `PromiseHeader` prefix (same as the native async
// ABI). Use `rt_promise_payload_ptr` to obtain the payload pointer.
PromiseRef rt_parallel_spawn_promise_with_shape(
  void (*task)(uint8_t*, PromiseRef),
  uint8_t* data,
  size_t promise_size,
  size_t promise_align,
  RtShapeId promise_shape
);
// Like `rt_parallel_spawn_promise_with_shape`, but `data` is a GC-managed object that the runtime
// will keep alive until the worker task finishes executing.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until the task completes.
// - The worker callback receives the (possibly relocated) pointer after any GC relocation.
PromiseRef rt_parallel_spawn_promise_with_shape_rooted(
  void (*task)(uint8_t*, PromiseRef),
  uint8_t* data,
  size_t promise_size,
  size_t promise_align,
  RtShapeId promise_shape
);
// Like `rt_parallel_spawn_promise_with_shape_rooted`, but takes the GC pointer as a `GcHandle`
// (pointer-to-slot).
PromiseRef rt_parallel_spawn_promise_with_shape_rooted_h(
  void (*task)(uint8_t*, PromiseRef),
  GcHandle data,
  size_t promise_size,
  size_t promise_align,
  RtShapeId promise_shape
);

// -----------------------------------------------------------------------------
// Blocking thread pool
// -----------------------------------------------------------------------------
// Run `task(data, promise)` on the runtime's dedicated blocking thread pool (for I/O, crypto,
// etc.). The task must resolve/reject `promise` via `rt_promise_resolve_legacy` /
// `rt_promise_reject_legacy`.
//
// Contract:
// - `data` must remain valid until `task` runs.
// - `data` must point to non-GC-managed memory: blocking tasks run in a GC-safe ("NativeSafe")
//   region and must not dereference GC pointers.
//
// Blocking tasks execute in a GC-safe ("NativeSafe") region: they must not touch the GC heap (no
// GC allocations, no write barriers, and no dereferencing GC-managed pointers).
//
// Pool size:
// - default: min(available_parallelism, 4)
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
// Returns the promise payload pointer.
//
// - For promises created by `rt_parallel_spawn_promise` / `rt_parallel_spawn_promise_rooted`, this
//   returns the runtime-allocated **out-of-line** payload buffer.
// - For GC-managed native async ABI promises (`rt_alloc` + `rt_promise_init`), this returns the
//   **inline** payload pointer immediately after the `PromiseHeader` prefix. This includes promises
//   created by `rt_parallel_spawn_promise_with_shape`.
//
// For non-payload promises, this may return NULL.
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
//
// `CoroutineId` values are allocated via the persistent handle ABI:
//   // Under a moving GC, use the handle-based variant:
//   //   GcPtr coro_ptr = (GcPtr)coro;
//   //   CoroutineId id = rt_handle_alloc_h(&coro_ptr);
//   CoroutineId id = rt_handle_alloc((GcPtr)coro_ptr);
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
enum { RT_ASYNC_ABI_VERSION = 2 };
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
  RtGcPrefix gc;
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
// Tear down all pending async work without running it.
//
// This discards queued microtasks/macrotasks, timers, I/O watchers, and pending promise reactions,
// and invokes any registered drop hooks for discarded jobs (e.g. coroutine destroy, microtask
// payload drops). Call this when an embedding needs to abandon the event loop early (termination,
// timeout, shutdown) to avoid leaks.
void rt_async_cancel_all(void);

// Drive the runtime's async/event-loop queues for one turn.
//
// A single poll turn:
// - drains queued microtasks
// - promotes due timers into the macrotask queue
// - runs at most one macrotask (timer callbacks, I/O readiness callbacks, etc)
// - runs a microtask checkpoint
//
// This call may block in the platform reactor wait syscall (`epoll_wait`/`kevent`) when there is no
// ready work but there are pending timers, I/O watchers, or outstanding "external" work (e.g. a
// parallel task spawned via `rt_parallel_spawn_promise` that has not yet settled its promise).
//
// Return value:
// - true  iff there is still pending work after this poll turn (queued microtasks/macrotasks,
//          active timers, I/O watchers, or outstanding external work).
// - false when the runtime is fully idle.
//
// Note: `rt_async_poll_legacy` is a compatibility alias with identical behavior.
// To drain only microtasks (non-blocking) without running timers/I/O/macrotasks, use
// `rt_drain_microtasks`.
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
// Like `rt_promise_then`, but `data` is a GC-managed pointer that must remain alive across moving
// collections until the callback runs.
//
// IMPORTANT: `data` must be the GC *object base pointer* (the same kind of pointer returned by
// `rt_alloc` / stored in `ValueRef`), not an interior pointer into an object payload.
void rt_promise_then_rooted(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);
void rt_promise_then_rooted_h(LegacyPromiseRef p, void (*on_settle)(uint8_t*), GcHandle data);

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
// Like `rt_promise_then_legacy`, but `data` is a GC-managed object base pointer that must remain
// alive (and relocatable) until `on_settle` runs.
//
// Contract:
// - `data` must be the base pointer of a GC-managed object (start of ObjHeader). This is the same
//   kind of pointer returned by `rt_alloc` / stored in `ValueRef` (not an interior pointer into an
//   object payload).
// - The runtime registers a strong GC root for `data` until the callback runs.
// - When the callback runs, the runtime passes the *current* pointer (after any relocation).
void rt_promise_then_rooted_legacy(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);
void rt_promise_then_rooted_h_legacy(LegacyPromiseRef p, void (*on_settle)(uint8_t*), GcHandle data);
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
// Compatibility alias for `rt_async_poll` (identical behavior).
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
// or during `rt_async_poll`/`rt_async_poll_legacy` when the event loop is otherwise idle).
//
// Microtasks are executed FIFO in the same queue as promise reaction jobs (e.g. async/await
// coroutine wakeups).
void rt_queue_microtask(Microtask task);
// Like `rt_queue_microtask`, but provides a `drop_data` hook that runs only if the microtask is
// discarded without executing (e.g. `rt_async_cancel_all`).
void rt_queue_microtask_with_drop(void (*cb)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*));
// Like `rt_queue_microtask`, but `data` is a GC-managed object that the runtime
// will keep alive until `cb` runs.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until the microtask executes.
void rt_queue_microtask_rooted(void (*cb)(uint8_t*), uint8_t* data);
// Like `rt_queue_microtask_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
void rt_queue_microtask_rooted_h(void (*cb)(uint8_t*), GcHandle data);
// Drain only the microtask queue (does not run timers/reactor/macrotasks).
// Returns true if any microtasks were executed.
bool rt_drain_microtasks(void);

// Timers. Timer callbacks are macrotasks; after each timer callback, `rt_async_poll`/`rt_async_poll_legacy` runs a
// microtask checkpoint. This is a minimal API surface; HTML-specific clamping (e.g. nested 4ms
// clamp) is handled at higher layers.
TimerId rt_set_timeout(void (*cb)(uint8_t*), uint8_t* data, uint64_t delay_ms);
// Like `rt_set_timeout`, but `data` is a GC-managed object that the runtime will keep alive until
// the timeout fires (or is cleared).
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until the timeout executes or is cleared.
// - The callback receives the current relocated pointer.
TimerId rt_set_timeout_rooted(void (*cb)(uint8_t*), uint8_t* data, uint64_t delay_ms);
// Like `rt_set_timeout_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
TimerId rt_set_timeout_rooted_h(void (*cb)(uint8_t*), GcHandle data, uint64_t delay_ms);
TimerId rt_set_timeout_with_drop(void (*cb)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*), uint64_t delay_ms);
TimerId rt_set_interval(void (*cb)(uint8_t*), uint8_t* data, uint64_t interval_ms);
// Like `rt_set_interval`, but `data` is a GC-managed object that the runtime will keep alive until
// the interval is cleared with `rt_clear_timer`.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until `rt_clear_timer` is called.
// - Each callback receives the current relocated pointer.
TimerId rt_set_interval_rooted(void (*cb)(uint8_t*), uint8_t* data, uint64_t interval_ms);
// Like `rt_set_interval_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
TimerId rt_set_interval_rooted_h(void (*cb)(uint8_t*), GcHandle data, uint64_t interval_ms);
TimerId rt_set_interval_with_drop(void (*cb)(uint8_t*), uint8_t* data, void (*drop_data)(uint8_t*), uint64_t interval_ms);
void rt_clear_timer(TimerId id);

// -----------------------------------------------------------------------------
// GC-rooted scheduling APIs (HandleId-based)
// -----------------------------------------------------------------------------
//
// These variants are safe for GC-managed userdata under a moving GC.
//
// Ownership:
// - The runtime consumes `HandleId data` and treats it as a strong root while the
//   work item is queued/registered.
// - The runtime will free the handle exactly once when the work item is torn
//   down (after execution, or on cancellation/unregister).
//
// If `data` is stale (freed), the callback is treated as a no-op.
void rt_queue_microtask_handle(void (*cb)(GcPtr), HandleId data);
void rt_queue_microtask_handle_with_drop(void (*cb)(GcPtr), HandleId data, void (*drop_data)(GcPtr));
TimerId rt_set_timeout_handle(void (*cb)(GcPtr), HandleId data, uint64_t delay_ms);
TimerId rt_set_timeout_handle_with_drop(void (*cb)(GcPtr), HandleId data, void (*drop_data)(GcPtr), uint64_t delay_ms);
TimerId rt_set_interval_handle(void (*cb)(GcPtr), HandleId data, uint64_t interval_ms);
TimerId rt_set_interval_handle_with_drop(void (*cb)(GcPtr), HandleId data, void (*drop_data)(GcPtr), uint64_t interval_ms);
// Handle-based I/O watchers (`rt_io_register_handle*`) follow the same contract as the regular
// I/O watcher APIs below (nonblocking fd + edge-triggered drain requirement).
IoWatcherId rt_io_register_handle(
  RtFd fd,
  uint32_t interests,
  void (*cb)(uint32_t events, GcPtr data),
  HandleId data
);
IoWatcherId rt_io_register_handle_with_drop(
  RtFd fd,
  uint32_t interests,
  void (*cb)(uint32_t events, GcPtr data),
  HandleId data,
  void (*drop_data)(GcPtr)
);

// -----------------------------------------------------------------------------
// I/O watchers (reactor-backed readiness notifications)
// -----------------------------------------------------------------------------
//
// Contract:
// - `fd` must be set to `O_NONBLOCK` before registration/update and must remain `O_NONBLOCK` for the
//   lifetime of the registration.
// - `interests` must include `RT_IO_READABLE` and/or `RT_IO_WRITABLE` (it must not be 0).
// - Readiness notifications are edge-triggered; consumers must drain reads/writes
//   until they return `EAGAIN`/`WouldBlock`.
// - `rt_io_register*` returns 0 on failure.
IoWatcherId rt_io_register(RtFd fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), uint8_t* data);
IoWatcherId rt_io_register_with_drop(RtFd fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), uint8_t* data, void (*drop_data)(uint8_t* data));
// Like `rt_io_register`, but `data` is a GC-managed object that the runtime will keep alive until
// the watcher is unregistered with `rt_io_unregister`.
//
// Contract:
// - `data` must be a pointer to the base of a GC-managed object (start of ObjHeader).
// - The runtime registers a strong GC root for `data` until `rt_io_unregister` is called.
// - Each callback receives the current relocated pointer.
IoWatcherId rt_io_register_rooted(RtFd fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), uint8_t* data);
// Like `rt_io_register_rooted`, but takes the GC pointer as a `GcHandle` (pointer-to-slot).
IoWatcherId rt_io_register_rooted_h(RtFd fd, uint32_t interests, void (*cb)(uint32_t events, uint8_t* data), GcHandle data);
void rt_io_update(IoWatcherId id, uint32_t interests);
void rt_io_unregister(IoWatcherId id);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // ECMA_RS_RUNTIME_NATIVE_H
