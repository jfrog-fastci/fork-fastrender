#![no_std]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("runtime-native ABI is currently only supported on 64-bit targets");

#[cfg(test)]
extern crate std;

use core::ffi::c_void;
use core::ffi::c_char;
use core::sync::atomic::AtomicU64;

/// ABI types shared between `runtime-native`, generated native code, and external tooling.
///
/// ## Shape IDs
/// `types_ts_interned::ShapeId` is a **semantic** identifier (`u128`) used by analysis and codegen.
/// It is *not* passed into the runtime directly. Codegen is responsible for producing a compact,
/// runtime-local shape table and mapping semantic shape IDs to [`RtShapeId`] indices.
///
/// The runtime uses [`RtShapeId`] (`u32`) to index into the registered shape-descriptor table.

/// Version of the stable runtime ABI.
///
/// Bump this only for backwards-incompatible changes (layout/signature changes).
pub const RT_NATIVE_ABI_VERSION: u32 = 0;

/// Version of the native async coroutine ABI (`CoroutineVTable` layout/semantics).
///
/// This is intentionally separate from [`RT_NATIVE_ABI_VERSION`]: the native async ABI evolves with
/// compiler/runtime codegen details (coroutine frame + promise layout metadata) and is validated at
/// runtime via `CoroutineVTable::abi_version`.
///
/// Must match:
/// - `runtime-native/include/runtime_native.h` (`RT_ASYNC_ABI_VERSION`)
/// - `runtime-native/src/async_abi.rs` (`RT_ASYNC_ABI_VERSION`)
pub const RT_ASYNC_ABI_VERSION: u32 = 2;

/// `Coroutine.flags` bitfield: when set, the runtime owns the coroutine frame and will call
/// `vtable->destroy(coro)` exactly once after completion or cancellation.
///
/// Must match `CORO_FLAG_RUNTIME_OWNS_FRAME` in `runtime-native/include/runtime_native.h`.
pub const CORO_FLAG_RUNTIME_OWNS_FRAME: u32 = 1 << 0;

// Pointer/usize assumptions (the ABI is currently 64-bit only).
pub const RT_PTR_SIZE_BYTES: usize = 8;
pub const RT_PTR_ALIGN_BYTES: usize = 8;

// Thread kind constants (match `runtime-native/include/runtime_native.h`).
pub const RT_THREAD_KIND_MAIN: u32 = 0;
pub const RT_THREAD_KIND_WORKER: u32 = 1;
pub const RT_THREAD_KIND_IO: u32 = 2;
pub const RT_THREAD_KIND_EXTERNAL: u32 = 3;

/// Thread kind enum used by [`rt_thread_register`].
///
/// Must match `RtThreadKind` in `runtime-native/include/runtime_native.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum RtThreadKind {
  RT_THREAD_MAIN = 0,
  RT_THREAD_WORKER = 1,
  RT_THREAD_IO = 2,
  RT_THREAD_EXTERNAL = 3,
}

// I/O watcher event flags (match `runtime-native/include/runtime_native.h`).
pub const RT_IO_READABLE: u32 = 0x1;
pub const RT_IO_WRITABLE: u32 = 0x2;
pub const RT_IO_ERROR: u32 = 0x4;

/// Raw pointer to a GC-managed object.
///
/// **Important:** `GcPtr` values are **object base pointers**: they point to the start of the
/// allocation's GC header (the `ObjHeader` prefix in the `runtime-native` crate), not to the start
/// of the payload after the header.
///
/// This matches the stable ABI contract for `rt_alloc` / `rt_alloc_pinned` in
/// `runtime-native/include/runtime_native.h` and is relied on by GC tracing (`RtShapeDescriptor`
/// pointer offsets are from the object base pointer).
pub type GcPtr = *mut u8;

/// GC handle (pointer-to-slot) used for passing GC-managed pointers across `may_gc` runtime calls.
///
/// This is the runtime-native "handle ABI": instead of passing a raw `GcPtr`, callers pass a
/// pointer to a root slot that contains the `GcPtr`. A moving GC updates that slot during
/// relocation, and runtime code can reload `*handle` after any safepoint.
pub type GcHandle = *mut GcPtr;

/// Opaque fixed-size prefix matching the runtime's `ObjHeader` layout.
///
/// The runtime's GC header is currently two machine words:
/// `{ type_desc: *const TypeDescriptor, meta: AtomicUsize }`.
///
/// This type exists so ABI structs that are also GC objects (e.g. coroutine frames) can include the
/// GC header prefix without exposing the header internals to generated code.
///
/// C codegen must treat this as opaque and must not read/write it directly; it is initialized by
/// `rt_alloc` / `rt_alloc_pinned`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RtGcPrefix {
  pub _opaque: [usize; 2],
}

// -----------------------------------------------------------------------------
// Array ABI (`rt_alloc_array`)
// -----------------------------------------------------------------------------

/// When set in the `elem_size` argument passed to `rt_alloc_array`, the array payload is treated as
/// a contiguous sequence of GC pointers.
///
/// The raw element size is `elem_size & !RT_ARRAY_ELEM_PTR_FLAG` and must equal
/// `size_of::<*mut u8>()`.
// Note: the ABI is currently 64-bit only, so this is always bit 63.
pub const RT_ARRAY_ELEM_PTR_FLAG: usize = 1usize << 63;

/// Array header flag: the array payload is a `len`-long sequence of `*mut u8` GC pointers.
pub const RT_ARRAY_FLAG_PTR_ELEMS: u32 = 1 << 0;

/// FFI-stable array header layout.
///
/// The object base pointer returned by `rt_alloc_array` points at the start of this header.
///
/// The first two words are the runtime's internal `ObjHeader` (opaque at the ABI boundary):
/// - `type_desc`: pointer to runtime type/shape metadata
/// - `meta`: per-object GC metadata bits / forwarding pointer
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RtArrayHeader {
  pub type_desc: *const c_void,
  pub meta: usize,
  pub len: usize,
  pub elem_size: u32,
  pub elem_flags: u32,
  /// Flexible payload; `RT_ARRAY_DATA_OFFSET` is the offset of this field.
  pub data: [u8; 0],
}

/// Byte offset from the array base pointer (header) to the start of the element payload.
// Note: the ABI is currently 64-bit only and `RtArrayHeader` is fixed-layout.
pub const RT_ARRAY_DATA_OFFSET: usize = 32;

/// Reserved invalid / sentinel runtime shape id value (raw).
///
/// Shape tables are 1-indexed: `RtShapeId(1)` refers to the first descriptor.
pub const RT_SHAPE_ID_INVALID_RAW: u32 = 0;

/// Reserved invalid / sentinel interned id value (raw).
pub const RT_INTERNED_ID_INVALID_RAW: u32 = u32::MAX;

/// Runtime-local identifier for an object shape (hidden class).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RtShapeId(pub u32);

impl RtShapeId {
  /// Reserved invalid / sentinel shape id.
  ///
  /// The shape table is 1-indexed: `RtShapeId(1)` refers to the first descriptor.
  pub const INVALID: Self = Self(RT_SHAPE_ID_INVALID_RAW);

  #[inline]
  pub const fn is_valid(self) -> bool {
    self.0 != RT_SHAPE_ID_INVALID_RAW
  }
}

/// FFI-stable descriptor for an allocated object shape.
///
/// This is intentionally minimal: the runtime only needs enough information to precisely trace GC
/// pointers within an object.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RtShapeDescriptor {
  /// Object size in bytes (including header).
  pub size: u32,
  /// Object alignment in bytes (power of two).
  pub align: u16,
  /// Shape flags (reserved for future use; must be `0` for now).
  pub flags: u16,
  /// Pointer-field byte offsets from the object base.
  ///
  /// Each entry is the offset of a `*mut u8` field containing a GC-traceable pointer.
  ///
  /// IMPORTANT: these offsets must list only **GC-managed** pointers (object references). Do not
  /// include pointers to external/non-GC memory (e.g. `ArrayBuffer` backing stores, OS buffers,
  /// iovec pointers). Misclassifying an external pointer as a GC pointer is a memory safety bug.
  pub ptr_offsets: *const u32,
  pub ptr_offsets_len: u32,
  /// Reserved for future expansion; must be `0`.
  pub reserved: u32,
}

// These ABI types are often emitted as global static tables by codegen and shared across threads
// for tracing.
//
// Note: for dlopen/JIT/multi-module embeddings, the runtime's shape registration entrypoints
// (`rt_register_shape_table*`) copy descriptor metadata (including `ptr_offsets`) into
// runtime-owned allocations. Callers therefore only need to keep the input descriptor table alive
// for the duration of the registration call.
unsafe impl Send for RtShapeDescriptor {}
unsafe impl Sync for RtShapeDescriptor {}

/// Optional GC/runtime statistics snapshot exposed for debugging/benching.
///
/// Must match `RtGcStatsSnapshot` in `runtime-native/include/runtime_native.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtGcStatsSnapshot {
  pub alloc_calls: u64,
  pub alloc_bytes: u64,
  pub alloc_array_calls: u64,
  pub alloc_array_bytes: u64,
  pub gc_collect_calls: u64,
  pub safepoint_calls: u64,
  pub write_barrier_calls_total: u64,
  pub write_barrier_range_calls: u64,
  pub write_barrier_old_young_hits: u64,
  pub set_young_range_calls: u64,
  pub thread_init_calls: u64,
  pub thread_deinit_calls: u64,
  pub remembered_objects_added: u64,
  pub remembered_objects_scanned_minor: u64,
  pub card_marks_total: u64,
  pub cards_scanned_minor: u64,
  pub cards_kept_after_rebuild: u64,
}

/// GC heap configuration for the process-global heap used by `rt_alloc*` and `rt_gc_collect`.
///
/// Must match `RtGcConfig` in `runtime-native/include/runtime_native.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtGcConfig {
  /// Size of the nursery (young generation), in bytes.
  pub nursery_size_bytes: usize,
  /// Allocation size threshold above which objects go to the large object space (LOS), in bytes.
  pub los_threshold_bytes: usize,
  /// Trigger a minor collection when nursery usage exceeds this percentage (`0..=100`).
  pub minor_gc_nursery_used_percent: u8,
  /// Trigger a major collection when old-generation live bytes exceed this threshold, in bytes.
  pub major_gc_old_bytes_threshold: usize,
  /// Trigger a major collection when the old generation owns more than this number of Immix blocks.
  pub major_gc_old_blocks_threshold: usize,
  /// Trigger a major collection when external (non-GC) bytes exceed this threshold, in bytes.
  pub major_gc_external_bytes_threshold: usize,
  /// Promotion policy: promote an object after it has survived this many minor collections (>= 1).
  pub promote_after_minor_survivals: u8,
}

/// Hard heap limits for the process-global heap used by `rt_alloc*` and `rt_gc_collect`.
///
/// Must match `RtGcLimits` in `runtime-native/include/runtime_native.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtGcLimits {
  /// Hard cap on GC heap usage, in bytes.
  pub max_heap_bytes: usize,
  /// Hard cap on total usage including external allocations, in bytes.
  pub max_total_bytes: usize,
}

/// A stable identifier for an interned UTF-8 string.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InternedId(pub u32);

impl InternedId {
  pub const INVALID: Self = Self(RT_INTERNED_ID_INVALID_RAW);
}

/// Identifier for a parallel task.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

/// Identifier for a timer returned by `rt_set_timeout` / `rt_set_interval`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(pub u64);

/// Identifier returned by `rt_io_register`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IoWatcherId(pub u64);

/// File descriptor type used by I/O watcher registration.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RtFd(pub i32);

/// Opaque value reference.
///
/// The full JS value representation is not implemented yet; treat as an opaque pointer payload.
pub type ValueRef = *mut c_void;

/// Stable persistent handle id (safe to store in OS event loop userdata like `epoll_event.data.u64`).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandleId(pub u64);

/// A single microtask callback scheduled onto the async runtime's microtask queue.
///
/// This is the low-level primitive used to implement Web-standard `queueMicrotask(cb)` without
/// allocating a promise/coroutine frame.
///
/// ## Safety / contracts
/// - `func` must be non-null.
/// - `data` must remain valid until `func(data)` runs (or until `drop(data)` runs).
/// - If the microtask is discarded without running (e.g. `rt_async_cancel_all`), the runtime calls
///   `drop(data)` if `drop` is non-null.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Microtask {
  pub func: extern "C" fn(*mut u8),
  pub data: *mut u8,
  pub drop: Option<extern "C" fn(*mut u8)>,
}

/// Stable handle to a coroutine frame managed by the runtime.
///
/// This is an ABI-stable `u64` identifier intended to remain valid across:
/// - moving/compacting GC (coroutine frames may relocate), and
/// - async/OS/thread boundaries (host work queues must not store raw pointers).
///
/// Note: coroutine IDs are currently backed by the same persistent handle table as `HandleId`.
///
/// - Allocate a new coroutine handle by calling `rt_handle_alloc(coro_ptr)` and treating the
///   returned ID as a `CoroutineId` (in Rust: `CoroutineId(handle.0)` where `handle: HandleId`).
/// - The runtime **consumes** the handle passed to `rt_async_spawn` / `rt_async_spawn_deferred` and
///   frees it (via `rt_handle_free`) when the coroutine completes or is cancelled.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CoroutineId(pub u64);

/// Opaque runtime record used by the `rt_thread_attach` / `rt_thread_detach` APIs.
#[repr(C)]
pub struct Runtime {
  _private: [u8; 0],
}

/// Opaque runtime thread record used by the `rt_thread_attach` / `rt_thread_detach` APIs.
#[repr(C)]
pub struct Thread {
  _private: [u8; 0],
}

/// Opaque promise header prefix.
///
/// In the native async ABI, every `Promise<T>` allocation begins with a `PromiseHeader` at offset 0.
/// The concrete layout of this header is owned by the runtime (`runtime-native/src/async_abi.rs`).
///
/// This definition exists only so `PromiseRef` can be represented as a pointer to an opaque C
/// struct, matching `runtime_native.h`.
#[repr(C)]
pub struct PromiseHeader {
  _private: [u8; 0],
}

/// Opaque handle to a runtime-managed promise.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PromiseRef(pub *mut PromiseHeader);

impl PromiseRef {
  #[inline]
  pub const fn null() -> Self {
    Self(core::ptr::null_mut())
  }

  #[inline]
  pub const fn is_null(self) -> bool {
    self.0.is_null()
  }
}

// `PromiseRef` is an opaque handle. The runtime API is responsible for ensuring thread-safety of
// operations performed through this handle.
unsafe impl Send for PromiseRef {}
unsafe impl Sync for PromiseRef {}

/// Payload layout for promises returned from [`rt_parallel_spawn_promise`] (or
/// [`rt_parallel_spawn_promise_rooted`]).
///
/// The runtime allocates a payload buffer described by this struct. The worker task can write its
/// result into `rt_promise_payload_ptr(promise)` and then call `rt_promise_fulfill` (or
/// `rt_promise_reject`).
///
/// Note: this payload buffer is treated as raw bytes and is **not traced by the GC**. If the payload
/// contains GC pointers, use `rt_parallel_spawn_promise_with_shape` instead (promise is GC-managed
/// and traced via a provided `RtShapeId`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PromiseLayout {
  pub size: usize,
  pub align: usize,
}

impl PromiseLayout {
  #[inline]
  pub const fn of<T>() -> Self {
    Self {
      size: core::mem::size_of::<T>(),
      align: core::mem::align_of::<T>(),
    }
  }
}

/// An FFI-friendly UTF-8 byte string reference.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringRef {
  pub ptr: *const u8,
  pub len: usize,
}

impl StringRef {
  pub const fn empty() -> Self {
    Self {
      ptr: b"".as_ptr(),
      len: 0,
    }
  }
}

// -----------------------------------------------------------------------------
// Native coroutine ABI (async/await lowering)
// -----------------------------------------------------------------------------

/// Discriminant for [`CoroutineStep`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum CoroutineStepTag {
  RT_CORO_STEP_AWAIT = 0,
  RT_CORO_STEP_COMPLETE = 1,
}

/// Result of a single coroutine resume step.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CoroutineStep {
  pub tag: CoroutineStepTag,
  /// For [`CoroutineStepTag::RT_CORO_STEP_AWAIT`], the promise being awaited.
  ///
  /// For [`CoroutineStepTag::RT_CORO_STEP_COMPLETE`], this must be null.
  pub await_promise: PromiseRef,
}

/// Resume function pointer for a coroutine frame.
pub type CoroutineResumeFn = unsafe extern "C" fn(*mut Coroutine) -> CoroutineStep;

/// Destroy (drop + deallocate) a coroutine frame.
pub type CoroutineDestroyFn = unsafe extern "C" fn(*mut Coroutine);

/// VTable describing a generated coroutine type.
#[repr(C)]
pub struct CoroutineVTable {
  pub resume: CoroutineResumeFn,
  pub destroy: CoroutineDestroyFn,
  pub promise_size: u32,
  pub promise_align: u32,
  pub promise_shape_id: RtShapeId,
  /// Must equal [`RT_ASYNC_ABI_VERSION`].
  pub abi_version: u32,
  /// Reserved for future ABI extensions; must be zeroed by generated code.
  pub reserved: [usize; 4],
}

/// Header embedded at offset 0 of every generated coroutine frame.
#[repr(C)]
pub struct Coroutine {
  /// GC object header prefix (object base pointer).
  pub gc: RtGcPrefix,
  pub vtable: *const CoroutineVTable,
  /// Result promise for this coroutine; written by `rt_async_spawn` before first resume.
  pub promise: PromiseRef,
  /// Intrusive list pointer used by the runtime while the coroutine is suspended.
  pub next_waiter: *mut Coroutine,
  /// `Coroutine.flags` bitfield.
  pub flags: u32,
}

/// Opaque pointer to a coroutine frame (and therefore the start of a generated coroutine).
pub type CoroutineRef = *mut Coroutine;

/// Legacy promise placeholder (used by older runtime-native tests/utilities).
#[repr(C)]
pub struct RtPromise {
  _private: [u8; 0],
}

/// Opaque handle to a legacy runtime-native promise.
///
/// ABI: `LegacyPromiseRef` is a raw pointer to an opaque `RtPromise` allocation.
///
/// In Rust we wrap the pointer in a `#[repr(transparent)]` newtype (instead of a bare `*mut
/// RtPromise`) so it can carry `Send`/`Sync` marker impls like [`PromiseRef`]. This matches how
/// generated code uses the handle: as an opaque, thread-safe runtime object reference.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LegacyPromiseRef(pub *mut RtPromise);

impl LegacyPromiseRef {
  #[inline]
  pub const fn null() -> Self {
    Self(core::ptr::null_mut())
  }

  #[inline]
  pub const fn is_null(self) -> bool {
    self.0.is_null()
  }
}

// `LegacyPromiseRef` is an opaque handle. The runtime API is responsible for ensuring thread-safety
// of operations performed through this handle.
unsafe impl Send for LegacyPromiseRef {}
unsafe impl Sync for LegacyPromiseRef {}

// -----------------------------------------------------------------------------
// Legacy promise resolution ABI (PromiseResolve / thenable assimilation)
// -----------------------------------------------------------------------------

/// Tag for [`PromiseResolveInput`].
pub type PromiseResolveKind = u8;
pub const RT_PROMISE_RESOLVE_VALUE: PromiseResolveKind = 0;
pub const RT_PROMISE_RESOLVE_PROMISE: PromiseResolveKind = 1;
pub const RT_PROMISE_RESOLVE_THENABLE: PromiseResolveKind = 2;

/// VTable describing a typed thenable (`PromiseLike<T>`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ThenableVTable {
  pub call_then: unsafe extern "C" fn(
    thenable: *mut u8,
    on_fulfilled: extern "C" fn(*mut u8, PromiseResolveInput),
    on_rejected: extern "C" fn(*mut u8, ValueRef),
    data: *mut u8,
  ) -> ValueRef,
}

/// ABI representation of a typed thenable value.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ThenableRef {
  pub vtable: *const ThenableVTable,
  pub ptr: *mut u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union PromiseResolvePayload {
  pub value: ValueRef,
  pub promise: LegacyPromiseRef,
  pub thenable: ThenableRef,
}

/// Input to the native runtime's promise resolution procedure.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PromiseResolveInput {
  pub kind: PromiseResolveKind,
  pub payload: PromiseResolvePayload,
}

impl PromiseResolveInput {
  #[inline]
  pub const fn value(value: ValueRef) -> Self {
    Self {
      kind: RT_PROMISE_RESOLVE_VALUE,
      payload: PromiseResolvePayload { value },
    }
  }

  #[inline]
  pub const fn promise(promise: LegacyPromiseRef) -> Self {
    Self {
      kind: RT_PROMISE_RESOLVE_PROMISE,
      payload: PromiseResolvePayload { promise },
    }
  }

  #[inline]
  pub const fn thenable(thenable: ThenableRef) -> Self {
    Self {
      kind: RT_PROMISE_RESOLVE_THENABLE,
      payload: PromiseResolvePayload { thenable },
    }
  }
}

// -----------------------------------------------------------------------------
// Legacy coroutine ABI (async/await lowering; will be removed once codegen migrates)
// -----------------------------------------------------------------------------

/// Status code returned by a legacy coroutine `resume` function.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum RtCoroStatus {
  RT_CORO_DONE = 0,
  RT_CORO_PENDING = 1,
  RT_CORO_YIELD = 2,
}

pub type RtCoroResumeFn = extern "C" fn(*mut RtCoroutineHeader) -> RtCoroStatus;

/// Header that prefixes legacy LLVM-generated coroutine frame structs.
#[repr(C)]
pub struct RtCoroutineHeader {
  pub resume: RtCoroResumeFn,
  pub promise: LegacyPromiseRef,
  pub state: u32,
  pub await_is_error: u32,
  pub await_value: ValueRef,
  pub await_error: ValueRef,
}

/// Function pointer type for parallel task entrypoints.
pub type RtTaskFn = extern "C" fn(*mut u8);

/// Function pointer type for `rt_parallel_for` loop bodies.
pub type RtParallelForBodyFn = extern "C" fn(usize, *mut u8);

/// Function pointer type for tasks spawned via [`rt_parallel_spawn_promise`].
pub type RtParallelPromiseTaskFn = extern "C" fn(*mut u8, PromiseRef);

/// Function pointer type for tasks spawned via [`rt_spawn_blocking_promise`].
///
/// The callback writes its result payload into `out_payload` (a temporary non-GC buffer owned by the
/// runtime) and returns a status tag:
/// - `0` => fulfill
/// - `1` => reject
/// - any other value is treated as reject
pub type RtBlockingPromiseTaskFn = extern "C" fn(data: *mut u8, out_payload: *mut u8) -> u8;

extern "C" {
  // Thread registration / state
  pub fn rt_thread_init(kind: u32);
  pub fn rt_thread_deinit();
  pub fn rt_thread_register(kind: RtThreadKind) -> u64;
  pub fn rt_thread_unregister();
  pub fn rt_thread_set_parked(parked: bool);
  pub fn rt_thread_current() -> *mut Thread;
  pub fn rt_thread_attach(runtime: *mut Runtime) -> *mut Thread;
  pub fn rt_thread_detach(thread: *mut Thread);
  pub fn rt_register_current_thread();
  pub fn rt_unregister_current_thread();
  pub fn rt_register_thread();
  pub fn rt_unregister_thread();

  // Memory
  pub fn rt_alloc(size: usize, shape: RtShapeId) -> GcPtr;
  pub fn rt_alloc_pinned(size: usize, shape: RtShapeId) -> GcPtr;
  pub fn rt_alloc_array(len: usize, elem_size: usize) -> GcPtr;
  pub fn rt_alloc_ptr_array(len: usize) -> *mut u8;
  pub fn rt_array_len(obj: *mut u8) -> usize;
  pub fn rt_array_data(obj: *mut u8) -> *mut u8;

  pub fn rt_register_shape_table(table: *const RtShapeDescriptor, len: usize);
  pub fn rt_register_shape_table_extend(table: *const RtShapeDescriptor, len: usize) -> RtShapeId;
  pub fn rt_register_shape_table_append(table: *const RtShapeDescriptor, len: usize) -> RtShapeId;
  pub fn rt_register_shape(desc: *const RtShapeDescriptor) -> RtShapeId;

  // GC
  ///
  /// This is an atomic `u64` in the runtime; treat it as `_Atomic uint64_t` from C.
  pub static RT_GC_EPOCH: AtomicU64;
  pub fn rt_gc_safepoint();
  pub fn rt_gc_safepoint_relocate_h(slot: GcHandle) -> GcPtr;
  pub fn rt_gc_safepoint_slow(requested_epoch: u64);
  pub fn rt_gc_poll() -> bool;
  pub fn rt_keep_alive_gc_ref(gc_ref: *mut u8);
  pub fn rt_write_barrier(obj: GcPtr, slot: *mut u8);
  pub fn rt_write_barrier_range(obj: GcPtr, start_slot: *mut u8, len: usize);
  pub fn rt_gc_collect();
  pub fn rt_gc_collect_minor();
  pub fn rt_gc_collect_major();
  pub fn rt_backing_store_external_bytes() -> usize;
  pub fn rt_gc_set_config(cfg: *const RtGcConfig) -> bool;
  pub fn rt_gc_set_limits(limits: *const RtGcLimits) -> bool;
  pub fn rt_gc_get_config(out_cfg: *mut RtGcConfig) -> bool;
  pub fn rt_gc_get_limits(out_limits: *mut RtGcLimits) -> bool;
  pub fn rt_stackmaps_register(start: *const u8, end: *const u8) -> bool;
  pub fn rt_stackmaps_unregister(start: *const u8) -> bool;

  // Global roots / handles
  pub fn rt_root_push(slot: GcHandle);
  pub fn rt_root_pop(slot: GcHandle);
  pub fn rt_global_root_register(slot: *mut usize);
  pub fn rt_global_root_unregister(slot: *mut usize);
  pub fn rt_gc_register_root_slot(slot: GcHandle) -> u32;
  pub fn rt_gc_unregister_root_slot(handle: u32);
  pub fn rt_gc_pin(ptr: GcPtr) -> u32;
  pub fn rt_gc_pin_h(ptr: GcHandle) -> u32;
  pub fn rt_gc_unpin(handle: u32);
  pub fn rt_gc_root_get(handle: u32) -> GcPtr;
  pub fn rt_gc_root_set(handle: u32, ptr: GcPtr) -> bool;
  pub fn rt_gc_root_set_h(handle: u32, ptr: GcHandle) -> bool;

  // Persistent handles (stable u64 ids).
  pub fn rt_handle_alloc(ptr: GcPtr) -> HandleId;
  pub fn rt_handle_alloc_h(ptr: GcHandle) -> HandleId;
  pub fn rt_handle_free(handle: HandleId);
  pub fn rt_handle_load(handle: HandleId) -> GcPtr;
  pub fn rt_handle_store(handle: HandleId, ptr: GcPtr);
  pub fn rt_handle_store_h(handle: HandleId, ptr: GcHandle);

  pub fn rt_gc_set_young_range(start: *mut u8, end: *mut u8);
  pub fn rt_gc_get_young_range(out_start: *mut GcPtr, out_end: *mut GcPtr);

  // Optional gc_stats APIs.
  pub fn rt_gc_stats_snapshot(out: *mut RtGcStatsSnapshot);
  pub fn rt_gc_stats_reset();

  // Optional gc_debug APIs.
  pub fn rt_debug_shape_count() -> usize;
  pub fn rt_debug_shape_descriptor(id: RtShapeId) -> *const RtShapeDescriptor;
  pub fn rt_debug_validate_heap();

  // Weak references (weak handles).
  pub fn rt_weak_add(value: GcPtr) -> u64;
  pub fn rt_weak_add_h(value: GcHandle) -> u64;
  pub fn rt_weak_get(handle: u64) -> GcPtr;
  pub fn rt_weak_remove(handle: u64);

  // Strings
  pub fn rt_string_concat(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> StringRef;
  pub fn rt_string_free(s: StringRef);
  pub fn rt_stringref_free(s: StringRef);
  pub fn rt_string_new_utf8(bytes: *const u8, len: usize) -> GcPtr;
  pub fn rt_string_concat_gc(a: GcPtr, b: GcPtr) -> GcPtr;
  pub fn rt_string_len(s: GcPtr) -> usize;
  pub fn rt_string_as_utf8(s: GcPtr) -> StringRef;
  pub fn rt_string_to_owned_utf8(s: GcPtr) -> StringRef;
  pub fn rt_string_intern(s: *const u8, len: usize) -> InternedId;
  pub fn rt_string_lookup(id: InternedId) -> StringRef;
  pub fn rt_string_pin_interned(id: InternedId);
  pub fn rt_string_lookup_pinned(id: InternedId, out: *mut StringRef) -> bool;

  // Parallel
  pub fn rt_parallel_spawn(task: RtTaskFn, data: *mut u8) -> TaskId;
  pub fn rt_parallel_spawn_rooted(task: RtTaskFn, data: GcPtr) -> TaskId;
  pub fn rt_parallel_spawn_rooted_h(task: RtTaskFn, data: GcHandle) -> TaskId;
  pub fn rt_parallel_spawn_promise_legacy(
    task: extern "C" fn(*mut u8, LegacyPromiseRef),
    data: *mut u8,
  ) -> LegacyPromiseRef;
  pub fn rt_parallel_join(tasks: *const TaskId, count: usize);
  pub fn rt_parallel_for(start: usize, end: usize, body: RtParallelForBodyFn, data: *mut u8);
  pub fn rt_parallel_for_rooted(start: usize, end: usize, body: RtParallelForBodyFn, data: GcPtr);
  pub fn rt_parallel_for_rooted_h(start: usize, end: usize, body: RtParallelForBodyFn, data: GcHandle);
  pub fn rt_parallel_spawn_promise(
    task: RtParallelPromiseTaskFn,
    data: *mut u8,
    layout: PromiseLayout,
  ) -> PromiseRef;
  pub fn rt_parallel_spawn_promise_rooted(
    task: RtParallelPromiseTaskFn,
    data: GcPtr,
    layout: PromiseLayout,
  ) -> PromiseRef;
  pub fn rt_parallel_spawn_promise_rooted_h(
    task: RtParallelPromiseTaskFn,
    data: GcHandle,
    layout: PromiseLayout,
  ) -> PromiseRef;
  pub fn rt_parallel_spawn_promise_with_shape(
    task: RtParallelPromiseTaskFn,
    data: *mut u8,
    promise_size: usize,
    promise_align: usize,
    promise_shape: RtShapeId,
  ) -> PromiseRef;
  pub fn rt_parallel_spawn_promise_with_shape_rooted(
    task: RtParallelPromiseTaskFn,
    data: GcPtr,
    promise_size: usize,
    promise_align: usize,
    promise_shape: RtShapeId,
  ) -> PromiseRef;
  pub fn rt_parallel_spawn_promise_with_shape_rooted_h(
    task: RtParallelPromiseTaskFn,
    data: GcHandle,
    promise_size: usize,
    promise_align: usize,
    promise_shape: RtShapeId,
  ) -> PromiseRef;
  pub fn rt_spawn_blocking(
    task: extern "C" fn(*mut u8, LegacyPromiseRef),
    data: *mut u8,
  ) -> LegacyPromiseRef;
  pub fn rt_spawn_blocking_promise(
    task: RtBlockingPromiseTaskFn,
    data: *mut u8,
    layout: PromiseLayout,
  ) -> PromiseRef;
  pub fn rt_spawn_blocking_promise_rooted(
    task: RtBlockingPromiseTaskFn,
    data: GcPtr,
    layout: PromiseLayout,
  ) -> PromiseRef;
  pub fn rt_spawn_blocking_promise_rooted_h(
    task: RtBlockingPromiseTaskFn,
    data: GcHandle,
    layout: PromiseLayout,
  ) -> PromiseRef;

  // Async
  pub fn rt_promise_init(p: PromiseRef);
  pub fn rt_promise_fulfill(p: PromiseRef);
  pub fn rt_promise_try_fulfill(p: PromiseRef) -> bool;
  pub fn rt_promise_reject(p: PromiseRef);
  pub fn rt_promise_try_reject(p: PromiseRef) -> bool;
  pub fn rt_promise_mark_handled(p: PromiseRef);
  pub fn rt_promise_payload_ptr(p: PromiseRef) -> *mut u8;
  pub fn rt_async_spawn(coro: CoroutineId) -> PromiseRef;
  pub fn rt_async_spawn_deferred(coro: CoroutineId) -> PromiseRef;
  pub fn rt_async_cancel_all();
  pub fn rt_async_poll() -> bool;
  pub fn rt_async_wait();
  pub fn rt_async_set_strict_await_yields(strict: bool);
  pub fn rt_async_run_until_idle() -> bool;
  pub fn rt_async_block_on(p: PromiseRef);

  // Microtasks + timers (queueMicrotask / setTimeout / setInterval).
  pub fn rt_async_sleep(delay_ms: u64) -> PromiseRef;
  pub fn rt_queue_microtask(task: Microtask);
  pub fn rt_queue_microtask_with_drop(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
  );
  pub fn rt_queue_microtask_rooted(cb: extern "C" fn(*mut u8), data: GcPtr);
  pub fn rt_queue_microtask_rooted_h(cb: extern "C" fn(*mut u8), data: GcHandle);
  pub fn rt_drain_microtasks() -> bool;
  pub fn rt_queue_microtask_handle(cb: extern "C" fn(GcPtr), data: HandleId);
  pub fn rt_queue_microtask_handle_with_drop(
    cb: extern "C" fn(GcPtr),
    data: HandleId,
    drop_data: extern "C" fn(GcPtr),
  );
  pub fn rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId;
  pub fn rt_set_timeout_rooted(cb: extern "C" fn(*mut u8), data: GcPtr, delay_ms: u64) -> TimerId;
  pub fn rt_set_timeout_rooted_h(cb: extern "C" fn(*mut u8), data: GcHandle, delay_ms: u64) -> TimerId;
  pub fn rt_set_timeout_with_drop(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
    delay_ms: u64,
  ) -> TimerId;
  pub fn rt_set_timeout_handle(cb: extern "C" fn(GcPtr), data: HandleId, delay_ms: u64) -> TimerId;
  pub fn rt_set_timeout_handle_with_drop(
    cb: extern "C" fn(GcPtr),
    data: HandleId,
    drop_data: extern "C" fn(GcPtr),
    delay_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval_rooted(
    cb: extern "C" fn(*mut u8),
    data: GcPtr,
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval_rooted_h(
    cb: extern "C" fn(*mut u8),
    data: GcHandle,
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval_with_drop(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval_handle(cb: extern "C" fn(GcPtr), data: HandleId, interval_ms: u64) -> TimerId;
  pub fn rt_set_interval_handle_with_drop(
    cb: extern "C" fn(GcPtr),
    data: HandleId,
    drop_data: extern "C" fn(GcPtr),
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_clear_timer(id: TimerId);

  // I/O watchers (reactor-backed readiness notifications).
  pub fn rt_io_register(
    fd: RtFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
  ) -> IoWatcherId;
  pub fn rt_io_register_with_drop(
    fd: RtFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
  ) -> IoWatcherId;
  pub fn rt_io_register_rooted(
    fd: RtFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: GcPtr,
  ) -> IoWatcherId;
  pub fn rt_io_register_rooted_h(
    fd: RtFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: GcHandle,
  ) -> IoWatcherId;
  pub fn rt_io_register_handle(
    fd: RtFd,
    interests: u32,
    cb: extern "C" fn(u32, GcPtr),
    data: HandleId,
  ) -> IoWatcherId;
  pub fn rt_io_register_handle_with_drop(
    fd: RtFd,
    interests: u32,
    cb: extern "C" fn(u32, GcPtr),
    data: HandleId,
    drop_data: extern "C" fn(GcPtr),
  ) -> IoWatcherId;
  pub fn rt_io_update(id: IoWatcherId, interests: u32);
  pub fn rt_io_unregister(id: IoWatcherId);

  // Async runtime diagnostics/limits.
  pub fn rt_async_set_limits(max_steps: usize, max_queue_len: usize);
  pub fn rt_async_take_last_error() -> *mut c_char;
  pub fn rt_async_free_c_string(s: *mut c_char);

  // Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates).
  pub fn rt_promise_new() -> LegacyPromiseRef;
  pub fn rt_promise_resolve(p: LegacyPromiseRef, value: ValueRef);
  pub fn rt_promise_then(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8);
  pub fn rt_promise_then_rooted(
    p: LegacyPromiseRef,
    on_settle: extern "C" fn(*mut u8),
    data: GcPtr,
  );
  pub fn rt_promise_then_rooted_h(
    p: LegacyPromiseRef,
    on_settle: extern "C" fn(*mut u8),
    data: GcHandle,
  );
  pub fn rt_coro_await(coro: *mut RtCoroutineHeader, awaited: LegacyPromiseRef, next_state: u32);

  pub fn rt_promise_new_legacy() -> LegacyPromiseRef;
  pub fn rt_promise_resolve_legacy(p: LegacyPromiseRef, value: ValueRef);
  pub fn rt_promise_resolve_into_legacy(p: LegacyPromiseRef, value: PromiseResolveInput);
  pub fn rt_promise_resolve_promise_legacy(p: LegacyPromiseRef, other: LegacyPromiseRef);
  pub fn rt_promise_resolve_thenable_legacy(p: LegacyPromiseRef, thenable: ThenableRef);
  pub fn rt_promise_reject_legacy(p: LegacyPromiseRef, err: ValueRef);
  pub fn rt_promise_then_legacy(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8);
  pub fn rt_promise_then_rooted_legacy(
    p: PromiseRef,
    on_settle: extern "C" fn(*mut u8),
    data: GcPtr,
  );
  pub fn rt_promise_then_rooted_h_legacy(
    p: PromiseRef,
    on_settle: extern "C" fn(*mut u8),
    data: GcHandle,
  );
  pub fn rt_promise_then_with_drop_legacy(
    p: PromiseRef,
    on_settle: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
  );
  pub fn rt_promise_drop_legacy(p: LegacyPromiseRef);

  pub fn rt_async_spawn_legacy(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef;
  pub fn rt_async_spawn_deferred_legacy(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef;
  pub fn rt_async_poll_legacy() -> bool;
  pub fn rt_async_sleep_legacy(delay_ms: u64) -> LegacyPromiseRef;
  pub fn rt_coro_await_legacy(coro: *mut RtCoroutineHeader, awaited: LegacyPromiseRef, next_state: u32);
  pub fn rt_coro_await_value_legacy(
    coro: *mut RtCoroutineHeader,
    awaited: PromiseResolveInput,
    next_state: u32,
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use core::mem::{align_of, size_of};
  use std::collections::BTreeSet;
  use std::string::String;
  use std::vec::Vec;

  // Layout invariants (compile-time).
  const _: () = {
    assert!(RT_PTR_SIZE_BYTES == 8);
    assert!(RT_PTR_ALIGN_BYTES == 8);

    assert!(RT_ARRAY_ELEM_PTR_FLAG == (1usize << 63));
    assert!(RT_ARRAY_DATA_OFFSET == 32);
    assert!(core::mem::offset_of!(RtArrayHeader, data) == RT_ARRAY_DATA_OFFSET);

    assert!(size_of::<RtArrayHeader>() == 32);
    assert!(align_of::<RtArrayHeader>() == 8);
    assert!(core::mem::offset_of!(RtArrayHeader, type_desc) == 0);
    assert!(core::mem::offset_of!(RtArrayHeader, meta) == 8);
    assert!(core::mem::offset_of!(RtArrayHeader, len) == 16);
    assert!(core::mem::offset_of!(RtArrayHeader, elem_size) == 24);
    assert!(core::mem::offset_of!(RtArrayHeader, elem_flags) == 28);
    assert!(core::mem::offset_of!(RtArrayHeader, data) == size_of::<RtArrayHeader>());

    assert!(size_of::<RtShapeId>() == 4);
    assert!(align_of::<RtShapeId>() == 4);

    assert!(size_of::<InternedId>() == 4);
    assert!(align_of::<InternedId>() == 4);

    assert!(size_of::<TaskId>() == 8);
    assert!(align_of::<TaskId>() == 8);

    assert!(size_of::<TimerId>() == 8);
    assert!(align_of::<TimerId>() == 8);

    assert!(size_of::<IoWatcherId>() == 8);
    assert!(align_of::<IoWatcherId>() == 8);

    assert!(size_of::<RtFd>() == 4);
    assert!(align_of::<RtFd>() == 4);

    assert!(size_of::<HandleId>() == 8);
    assert!(align_of::<HandleId>() == 8);
    assert!(size_of::<CoroutineId>() == 8);
    assert!(align_of::<CoroutineId>() == 8);

    assert!(size_of::<Microtask>() == 24);
    assert!(align_of::<Microtask>() == 8);
    assert!(core::mem::offset_of!(Microtask, func) == 0);
    assert!(core::mem::offset_of!(Microtask, data) == 8);
    assert!(core::mem::offset_of!(Microtask, drop) == 16);

    assert!(size_of::<PromiseRef>() == 8);
    assert!(align_of::<PromiseRef>() == 8);

    assert!(size_of::<PromiseLayout>() == 16);
    assert!(align_of::<PromiseLayout>() == 8);

    assert!(size_of::<GcPtr>() == 8);
    assert!(align_of::<GcPtr>() == 8);
    assert!(size_of::<GcHandle>() == 8);
    assert!(align_of::<GcHandle>() == 8);

    assert!(size_of::<StringRef>() == 16);
    assert!(align_of::<StringRef>() == 8);

    assert!(size_of::<ValueRef>() == 8);
    assert!(align_of::<ValueRef>() == 8);

    assert!(size_of::<PromiseResolveKind>() == 1);
    assert!(align_of::<PromiseResolveKind>() == 1);

    assert!(size_of::<ThenableVTable>() == 8);
    assert!(align_of::<ThenableVTable>() == 8);

    assert!(size_of::<ThenableRef>() == 16);
    assert!(align_of::<ThenableRef>() == 8);

    assert!(size_of::<PromiseResolvePayload>() == 16);
    assert!(align_of::<PromiseResolvePayload>() == 8);

    assert!(size_of::<PromiseResolveInput>() == 24);
    assert!(align_of::<PromiseResolveInput>() == 8);

    assert!(size_of::<RtCoroStatus>() == 4);
    assert!(align_of::<RtCoroStatus>() == 4);

    assert!(size_of::<RtCoroutineHeader>() == 40);
    assert!(align_of::<RtCoroutineHeader>() == 8);

    assert!(size_of::<CoroutineStepTag>() == 4);
    assert!(align_of::<CoroutineStepTag>() == 4);
    assert!(size_of::<CoroutineStep>() == 16);
    assert!(align_of::<CoroutineStep>() == 8);
    assert!(core::mem::offset_of!(CoroutineStep, tag) == 0);
    assert!(core::mem::offset_of!(CoroutineStep, await_promise) == 8);

    assert!(size_of::<CoroutineVTable>() == 64);
    assert!(align_of::<CoroutineVTable>() == 8);
    assert!(core::mem::offset_of!(CoroutineVTable, resume) == 0);
    assert!(core::mem::offset_of!(CoroutineVTable, destroy) == 8);
    assert!(core::mem::offset_of!(CoroutineVTable, promise_size) == 16);
    assert!(core::mem::offset_of!(CoroutineVTable, promise_align) == 20);
    assert!(core::mem::offset_of!(CoroutineVTable, promise_shape_id) == 24);
    assert!(core::mem::offset_of!(CoroutineVTable, abi_version) == 28);
    assert!(core::mem::offset_of!(CoroutineVTable, reserved) == 32);

    assert!(size_of::<RtGcPrefix>() == 16);
    assert!(align_of::<RtGcPrefix>() == 8);

    assert!(size_of::<Coroutine>() == 48);
    assert!(align_of::<Coroutine>() == 8);
    assert!(core::mem::offset_of!(Coroutine, gc) == 0);
    assert!(core::mem::offset_of!(Coroutine, vtable) == 16);
    assert!(core::mem::offset_of!(Coroutine, promise) == 24);
    assert!(core::mem::offset_of!(Coroutine, next_waiter) == 32);
    assert!(core::mem::offset_of!(Coroutine, flags) == 40);

    assert!(size_of::<RtShapeDescriptor>() == 24);
    assert!(align_of::<RtShapeDescriptor>() == 8);

    assert!(size_of::<RtGcStatsSnapshot>() == 136);
    assert!(align_of::<RtGcStatsSnapshot>() == 8);

    assert!(size_of::<RtGcConfig>() == 56);
    assert!(align_of::<RtGcConfig>() == 8);
    assert!(core::mem::offset_of!(RtGcConfig, nursery_size_bytes) == 0);
    assert!(core::mem::offset_of!(RtGcConfig, los_threshold_bytes) == 8);
    assert!(core::mem::offset_of!(RtGcConfig, minor_gc_nursery_used_percent) == 16);
    assert!(core::mem::offset_of!(RtGcConfig, major_gc_old_bytes_threshold) == 24);
    assert!(core::mem::offset_of!(RtGcConfig, major_gc_old_blocks_threshold) == 32);
    assert!(core::mem::offset_of!(RtGcConfig, major_gc_external_bytes_threshold) == 40);
    assert!(core::mem::offset_of!(RtGcConfig, promote_after_minor_survivals) == 48);

    assert!(size_of::<RtGcLimits>() == 16);
    assert!(align_of::<RtGcLimits>() == 8);
    assert!(core::mem::offset_of!(RtGcLimits, max_heap_bytes) == 0);
    assert!(core::mem::offset_of!(RtGcLimits, max_total_bytes) == 8);

    assert!(size_of::<AtomicU64>() == 8);
    assert!(align_of::<AtomicU64>() == 8);

    assert!(size_of::<RtThreadKind>() == 4);
    assert!(align_of::<RtThreadKind>() == 4);
  };

  #[test]
  fn generated_header_contains_expected_decls() {
    let header_path = std::path::Path::new(env!("OUT_DIR")).join("runtime_native_abi.h");
    let header = std::fs::read_to_string(&header_path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", header_path.display()));

    // Thread kind constants.
    for c in [
      "RT_THREAD_KIND_MAIN",
      "RT_THREAD_KIND_WORKER",
      "RT_THREAD_KIND_IO",
      "RT_THREAD_KIND_EXTERNAL",
      "RT_IO_READABLE",
      "RT_IO_WRITABLE",
      "RT_IO_ERROR",
      "RT_ARRAY_ELEM_PTR_FLAG",
      "RT_ARRAY_FLAG_PTR_ELEMS",
      "RT_ARRAY_DATA_OFFSET",
      "RT_ARRAY_ENCODE_PTR_ELEM_SIZE",
      "RT_ARRAY_DATA_PTR",
      "RT_PROMISE_RESOLVE_VALUE",
      "RT_PROMISE_RESOLVE_PROMISE",
      "RT_PROMISE_RESOLVE_THENABLE",
      "RT_CORO_DONE",
      "RT_CORO_PENDING",
      "RT_CORO_YIELD",
      "RT_CORO_STEP_AWAIT",
      "RT_CORO_STEP_COMPLETE",
      "RT_ASYNC_ABI_VERSION",
      "CORO_FLAG_RUNTIME_OWNS_FRAME",
    ] {
      assert!(header.contains(c), "missing constant `{c}` in generated header");
    }

    // Types.
    assert!(
      header.contains("typedef struct StringRef") || header.contains("typedef struct StringRef {"),
      "missing StringRef typedef"
    );
    assert!(
      header.contains("typedef struct Microtask"),
      "missing Microtask typedef"
    );
    for ty in [
      "RtThreadKind",
      "RtShapeId",
      "InternedId",
      "TaskId",
      "TimerId",
      "Microtask",
      "IoWatcherId",
      "RtFd",
      "HandleId",
      "CoroutineId",
      "PromiseRef",
      "LegacyPromiseRef",
      "PromiseLayout",
      "ValueRef",
      "PromiseResolveKind",
      "ThenableVTable",
      "ThenableRef",
      "PromiseResolveInput",
      "RtCoroStatus",
      "RtCoroutineHeader",
      "RtGcStatsSnapshot",
      "RtGcConfig",
      "RtGcLimits",
      "CoroutineRef",
      "CoroutineStepTag",
      "CoroutineStep",
      "CoroutineResumeFn",
      "CoroutineDestroyFn",
      "CoroutineVTable",
      "GcPtr",
      "GcHandle",
      "RtArrayHeader",
    ] {
      assert!(header.contains(ty), "missing type `{ty}` in generated header");
    }
    assert!(
      header.contains("typedef struct PromiseLayout")
        || header.contains("typedef struct PromiseLayout {"),
      "missing PromiseLayout typedef"
    );
    assert!(
      header.contains("typedef struct Coroutine {") || header.contains("struct Coroutine {"),
      "missing Coroutine definition"
    );
    assert!(
      header.contains("typedef struct Runtime Runtime;") || header.contains("struct Runtime;"),
      "missing Runtime forward declaration"
    );
    assert!(
      header.contains("typedef struct Thread Thread;") || header.contains("struct Thread;"),
      "missing Thread forward declaration"
    );
    assert!(
      header.contains("RT_THREAD;"),
      "missing RT_THREAD TLS symbol declaration"
    );
    assert!(
      header.contains("typedef struct RtPromise RtPromise;") || header.contains("struct RtPromise;"),
      "missing RtPromise forward declaration"
    );
    assert!(
      header.contains("typedef struct PromiseHeader PromiseHeader;") || header.contains("struct PromiseHeader;"),
      "missing PromiseHeader forward declaration"
    );
    assert!(
      header.contains("RT_THREAD;"),
      "missing RT_THREAD TLS symbol declaration"
    );
    assert!(
      header.contains("typedef struct RtCoroutineHeader RtCoroutineHeader;"),
      "missing RtCoroutineHeader forward declaration"
    );
    assert!(
      header.contains("typedef struct PromiseResolveInput PromiseResolveInput;"),
      "missing PromiseResolveInput forward declaration"
    );
    assert!(
      header.contains("typedef struct Coroutine Coroutine;"),
      "missing Coroutine forward declaration"
    );
    assert!(
      header.contains("RT_GC_EPOCH;"),
      "missing RT_GC_EPOCH extern declaration"
    );

    // Optional ABI surfaces are guarded in the handwritten C header; keep the generated header
    // consistent so consumers don't accidentally call missing symbols in non-feature builds.
    for guard in ["RUNTIME_NATIVE_GC_STATS", "RUNTIME_NATIVE_GC_DEBUG"] {
      let directive = std::format!("#ifdef {guard}");
      assert!(
        header.contains(&directive),
        "missing `{directive}` in generated header"
      );
    }

    // Functions (key entrypoints).
    for func in [
      "rt_thread_init(",
      "rt_thread_deinit(",
      "rt_thread_register(",
      "rt_thread_unregister(",
      "rt_thread_set_parked(",
      "rt_thread_attach(",
      "rt_thread_detach(",
      "rt_register_current_thread(",
      "rt_unregister_current_thread(",
      "rt_register_thread(",
      "rt_unregister_thread(",
      "rt_alloc(",
      "rt_alloc_pinned(",
      "rt_alloc_array(",
      "rt_alloc_ptr_array(",
      "rt_array_len(",
      "rt_array_data(",
      "rt_register_shape_table(",
      "rt_register_shape_table_extend(",
      "rt_register_shape_table_append(",
      "rt_register_shape(",
      "RT_GC_EPOCH",
      "rt_gc_safepoint(",
      "rt_gc_safepoint_slow(",
      "rt_gc_poll(",
      "rt_gc_safepoint_relocate_h(",
      "rt_keep_alive_gc_ref(",
      "rt_write_barrier(",
      "rt_write_barrier_range(",
      "rt_gc_collect(",
      "rt_gc_collect_minor(",
      "rt_gc_collect_major(",
      "rt_backing_store_external_bytes(",
      "rt_gc_set_config(",
      "rt_gc_set_limits(",
      "rt_gc_get_config(",
      "rt_gc_get_limits(",
      "rt_stackmaps_register(",
      "rt_stackmaps_unregister(",
      "rt_root_push(",
      "rt_root_pop(",
      "rt_global_root_register(",
      "rt_global_root_unregister(",
      "rt_gc_register_root_slot(",
      "rt_gc_unregister_root_slot(",
      "rt_gc_pin(",
      "rt_gc_pin_h(",
      "rt_gc_unpin(",
      "rt_gc_root_get(",
      "rt_gc_root_set(",
      "rt_gc_root_set_h(",
      "rt_handle_alloc(",
      "rt_handle_free(",
      "rt_handle_load(",
      "rt_handle_store(",
      "rt_gc_set_young_range(",
      "rt_gc_get_young_range(",
      "rt_gc_stats_snapshot(",
      "rt_gc_stats_reset(",
      "rt_debug_shape_count(",
      "rt_debug_shape_descriptor(",
      "rt_debug_validate_heap(",
      "RT_STACKMAPS_AUTO_REGISTER",
      "rt_weak_add(",
      "rt_weak_get(",
      "rt_weak_remove(",
      "rt_string_concat(",
      "rt_string_free(",
      "rt_string_intern(",
      "rt_string_pin_interned(",
      "rt_parallel_spawn(",
      "rt_parallel_spawn_rooted(",
      "rt_parallel_spawn_promise_legacy(",
      "rt_parallel_join(",
      "rt_parallel_for(",
      "rt_parallel_for_rooted(",
      "rt_parallel_for_rooted_h(",
      "rt_parallel_spawn_promise(",
      "rt_parallel_spawn_promise_rooted(",
      "rt_parallel_spawn_promise_rooted_h(",
      "rt_parallel_spawn_promise_with_shape(",
      "rt_parallel_spawn_promise_with_shape_rooted(",
      "rt_parallel_spawn_promise_with_shape_rooted_h(",
      "rt_spawn_blocking(",
      "rt_promise_init(",
      "rt_promise_fulfill(",
      "rt_promise_try_fulfill(",
      "rt_promise_reject(",
      "rt_promise_try_reject(",
      "rt_promise_mark_handled(",
      "rt_promise_payload_ptr(",
      "rt_async_spawn(",
      "rt_async_spawn_deferred(",
      "rt_async_cancel_all(",
      "rt_async_poll(",
      "rt_async_wait(",
      "rt_async_set_strict_await_yields(",
      "rt_async_run_until_idle(",
      "rt_async_block_on(",
      "rt_async_sleep(",
      "rt_queue_microtask(",
      "rt_queue_microtask_rooted(",
      "rt_queue_microtask_with_drop(",
      "rt_drain_microtasks(",
      "rt_queue_microtask_handle(",
      "rt_queue_microtask_handle_with_drop(",
      "rt_set_timeout(",
      "rt_set_timeout_rooted(",
      "rt_set_timeout_with_drop(",
      "rt_set_timeout_handle(",
      "rt_set_timeout_handle_with_drop(",
      "rt_set_interval(",
      "rt_set_interval_rooted(",
      "rt_set_interval_with_drop(",
      "rt_set_interval_handle(",
      "rt_set_interval_handle_with_drop(",
      "rt_clear_timer(",
      "rt_io_register(",
      "rt_io_register_with_drop(",
      "rt_io_register_rooted(",
      "rt_io_register_handle(",
      "rt_io_register_handle_with_drop(",
      "rt_io_update(",
      "rt_io_unregister(",
      "rt_async_set_limits(",
      "rt_async_take_last_error(",
      "rt_async_free_c_string(",
      "rt_promise_new(",
      "rt_promise_resolve(",
      "rt_promise_then(",
      "rt_promise_then_rooted(",
      "rt_coro_await(",
      "rt_promise_new_legacy(",
      "rt_promise_resolve_legacy(",
      "rt_promise_resolve_into_legacy(",
      "rt_promise_resolve_promise_legacy(",
      "rt_promise_resolve_thenable_legacy(",
      "rt_promise_reject_legacy(",
      "rt_promise_then_legacy(",
      "rt_promise_then_rooted_legacy(",
      "rt_promise_then_with_drop_legacy(",
      "rt_promise_drop_legacy(",
      "rt_async_spawn_legacy(",
      "rt_async_spawn_deferred_legacy(",
      "rt_async_poll_legacy(",
      "rt_async_sleep_legacy(",
      "rt_coro_await_legacy(",
      "rt_coro_await_value_legacy(",
    ] {
      assert!(
        header.contains(func),
        "generated header missing expected function `{func}`"
      );
    }

    assert!(
      header.contains("void rt_queue_microtask(Microtask"),
      "generated header missing expected signature for `rt_queue_microtask(Microtask ...)`"
    );
    assert!(
      header.contains("rt_thread_register(RtThreadKind"),
      "generated header missing expected signature for `rt_thread_register(RtThreadKind ...)`"
    );
  }

  fn extract_rt_function_names(source: &str) -> BTreeSet<String> {
    fn is_ident_byte(b: u8) -> bool {
      b.is_ascii_alphanumeric() || b == b'_'
    }

    let bytes = source.as_bytes();
    let mut names = BTreeSet::new();

    let mut i = 0;
    while i + 3 <= bytes.len() {
      if bytes[i] != b'r' || bytes[i + 1] != b't' || bytes[i + 2] != b'_' {
        i += 1;
        continue;
      }

      if i > 0 && is_ident_byte(bytes[i - 1]) {
        // Avoid matching substrings like `__rt_foo`.
        i += 1;
        continue;
      }

      let start = i;
      i += 3;
      while i < bytes.len() && is_ident_byte(bytes[i]) {
        i += 1;
      }

      let name = &source[start..i];

      // Skip whitespace between the identifier and the call/parens.
      let mut j = i;
      while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
      }

      if j < bytes.len() && bytes[j] == b'(' {
        names.insert(String::from(name));
      }
    }

    names
  }

  #[test]
  fn rt_function_surface_matches_runtime_native_h() {
    let runtime_native_h_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../runtime-native/include/runtime_native.h");
    let runtime_native_h = std::fs::read_to_string(&runtime_native_h_path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", runtime_native_h_path.display()));

    let generated_header_path = std::path::Path::new(env!("OUT_DIR")).join("runtime_native_abi.h");
    let generated_header = std::fs::read_to_string(&generated_header_path)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", generated_header_path.display()));

    let required = extract_rt_function_names(&runtime_native_h);
    assert!(
      required.contains("rt_thread_init"),
      "failed to extract any rt_* functions from {}",
      runtime_native_h_path.display()
    );

    let provided = extract_rt_function_names(&generated_header);
    assert!(
      provided.contains("rt_thread_init"),
      "failed to extract any rt_* functions from generated header {}",
      generated_header_path.display()
    );

    let missing: Vec<_> = required.difference(&provided).cloned().collect();
    assert!(
      missing.is_empty(),
      "runtime_native_abi.h is missing rt_* functions declared in runtime_native.h:\n{}",
      missing.join("\n")
    );

    let extra: Vec<_> = provided.difference(&required).cloned().collect();
    assert!(
      extra.is_empty(),
      "runtime_native_abi.h declares rt_* functions that are not in runtime_native.h:\n{}",
      extra.join("\n")
    );
  }

  #[test]
  fn generated_header_compiles_as_c() {
    use std::process::Command;

    let out_dir = std::path::PathBuf::from(env!("OUT_DIR"));

    let tmp_dir = std::env::temp_dir().join(std::format!(
      "runtime_native_abi_header_smoke_{}",
      std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    let c_path = tmp_dir.join("header_smoke.c");
    let cpp_path = tmp_dir.join("header_smoke.cpp");

    std::fs::write(
      &c_path,
      br#"
#include "runtime_native_abi.h"

int main(void) { return 0; }
"#,
    )
    .unwrap();
    std::fs::write(
      &cpp_path,
      br#"
#include "runtime_native_abi.h"

int main() { return 0; }
"#,
    )
    .unwrap();

    // Support common CC values that include arguments (e.g. "ccache clang-18").
    let cc = std::env::var("CC").unwrap_or_else(|_| std::string::String::from("cc"));
    let mut cc_parts = cc.split_whitespace();
    let program = cc_parts.next().unwrap_or("cc");
    let cc_args: Vec<&str> = cc_parts.collect();

    let variants: &[(&str, &[&str])] = &[
      ("", &[]),
      ("_stats", &["RUNTIME_NATIVE_GC_STATS"]),
      ("_debug", &["RUNTIME_NATIVE_GC_DEBUG"]),
      ("_stats_debug", &["RUNTIME_NATIVE_GC_STATS", "RUNTIME_NATIVE_GC_DEBUG"]),
    ];

    for (suffix, defines) in variants {
      let obj_path = tmp_dir.join(std::format!("header_smoke{suffix}.o"));
      let mut cmd = Command::new(program);
      cmd.args(&cc_args);
      cmd.arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror");
      for define in *defines {
        cmd.arg(std::format!("-D{define}"));
      }
      cmd.arg("-c")
        .arg(&c_path)
        .arg(std::format!("-I{}", out_dir.display()))
        .arg("-o")
        .arg(&obj_path);

      let output = cmd.output().unwrap_or_else(|e| panic!("failed to spawn C compiler: {e}"));
      if !output.status.success() {
        panic!(
          "runtime_native_abi.h smoke compile failed ({suffix}):\nstdout:\n{}\nstderr:\n{}",
          String::from_utf8_lossy(&output.stdout),
          String::from_utf8_lossy(&output.stderr),
        );
      }
    }

    for (suffix, defines) in variants {
      let obj_path = tmp_dir.join(std::format!("header_smoke{suffix}.cpp.o"));
      let mut cmd = Command::new(program);
      cmd.args(&cc_args);
      cmd.arg("-std=c++17")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror");
      for define in *defines {
        cmd.arg(std::format!("-D{define}"));
      }
      cmd.arg("-c")
        .arg(&cpp_path)
        .arg(std::format!("-I{}", out_dir.display()))
        .arg("-o")
        .arg(&obj_path);

      let output = cmd.output().unwrap_or_else(|e| panic!("failed to spawn C++ compiler: {e}"));
      if !output.status.success() {
        panic!(
          "runtime_native_abi.h smoke compile failed as C++ ({suffix}):\nstdout:\n{}\nstderr:\n{}",
          String::from_utf8_lossy(&output.stdout),
          String::from_utf8_lossy(&output.stderr),
        );
      }
    }
  }
}
