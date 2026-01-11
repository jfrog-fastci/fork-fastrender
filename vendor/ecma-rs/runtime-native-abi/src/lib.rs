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
pub const RT_ASYNC_ABI_VERSION: u32 = 1;

// Pointer/usize assumptions (the ABI is currently 64-bit only).
pub const RT_PTR_SIZE_BYTES: usize = 8;
pub const RT_PTR_ALIGN_BYTES: usize = 8;

// Thread kind constants (match `runtime-native/include/runtime_native.h`).
pub const RT_THREAD_KIND_MAIN: u32 = 0;
pub const RT_THREAD_KIND_WORKER: u32 = 1;
pub const RT_THREAD_KIND_IO: u32 = 2;
pub const RT_THREAD_KIND_EXTERNAL: u32 = 3;

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

// These ABI types are intended to be stored in global static tables (generated by codegen) and
// shared across threads for tracing. Raw pointers are used for FFI stability; the caller guarantees
// the pointed-to data is immutable and lives for the duration of the process.
unsafe impl Send for RtShapeDescriptor {}
unsafe impl Sync for RtShapeDescriptor {}

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

/// Opaque handle to a promise/coroutine managed by the runtime.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PromiseRef(pub *mut c_void);

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

/// Opaque coroutine state allocated/owned by generated code.
///
/// The full coroutine frame layout is owned by the compiler; the runtime treats
/// this as an opaque handle at the ABI boundary.
#[repr(C)]
pub struct Coroutine {
  _private: [u8; 0],
}

/// Legacy promise placeholder (used by older runtime-native tests/utilities).
#[repr(C)]
pub struct RtPromise {
  _private: [u8; 0],
}

pub type LegacyPromiseRef = *mut RtPromise;

/// Payload layout for promises returned from `rt_parallel_spawn_promise`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PromiseLayout {
  pub size: usize,
  pub align: usize,
}

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

extern "C" {
  // Thread registration / state
  pub fn rt_thread_init(kind: u32);
  pub fn rt_thread_deinit();
  pub fn rt_thread_register(kind: u32) -> u64;
  pub fn rt_thread_unregister();
  pub fn rt_thread_set_parked(parked: bool);
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
  pub fn rt_backing_store_external_bytes() -> usize;
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
  pub fn rt_gc_unpin(handle: u32);
  pub fn rt_gc_root_get(handle: u32) -> GcPtr;
  pub fn rt_gc_root_set(handle: u32, ptr: GcPtr) -> bool;

  // Persistent handles (stable u64 ids).
  pub fn rt_handle_alloc(ptr: GcPtr) -> HandleId;
  pub fn rt_handle_free(handle: HandleId);
  pub fn rt_handle_load(handle: HandleId) -> GcPtr;
  pub fn rt_handle_store(handle: HandleId, ptr: GcPtr);

  pub fn rt_gc_set_young_range(start: *mut u8, end: *mut u8);
  pub fn rt_gc_get_young_range(out_start: *mut GcPtr, out_end: *mut GcPtr);

  // Weak references (weak handles).
  pub fn rt_weak_add(value: GcPtr) -> u64;
  pub fn rt_weak_get(handle: u64) -> GcPtr;
  pub fn rt_weak_remove(handle: u64);

  // Strings
  pub fn rt_string_concat(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> StringRef;
  pub fn rt_string_intern(s: *const u8, len: usize) -> InternedId;
  pub fn rt_string_pin_interned(id: InternedId);

  // Parallel
  pub fn rt_parallel_spawn(task: RtTaskFn, data: *mut u8) -> TaskId;
  pub fn rt_parallel_join(tasks: *const TaskId, count: usize);
  pub fn rt_parallel_for(start: usize, end: usize, body: RtParallelForBodyFn, data: *mut u8);
  pub fn rt_parallel_spawn_promise(
    task: extern "C" fn(*mut u8, PromiseRef),
    data: *mut u8,
    layout: PromiseLayout,
  ) -> PromiseRef;
  pub fn rt_spawn_blocking(task: extern "C" fn(*mut u8, LegacyPromiseRef), data: *mut u8) -> LegacyPromiseRef;

  // Async
  pub fn rt_promise_init(p: PromiseRef);
  pub fn rt_promise_fulfill(p: PromiseRef);
  pub fn rt_promise_try_fulfill(p: PromiseRef) -> bool;
  pub fn rt_promise_reject(p: PromiseRef);
  pub fn rt_promise_try_reject(p: PromiseRef) -> bool;
  pub fn rt_promise_mark_handled(p: PromiseRef);
  pub fn rt_promise_payload_ptr(p: PromiseRef) -> *mut u8;
  pub fn rt_async_spawn(coro: *mut Coroutine) -> PromiseRef;
  pub fn rt_async_spawn_deferred(coro: *mut Coroutine) -> PromiseRef;
  pub fn rt_async_cancel_all();
  pub fn rt_async_poll() -> bool;
  pub fn rt_async_wait();
  pub fn rt_async_set_strict_await_yields(strict: bool);
  pub fn rt_async_run_until_idle() -> bool;
  pub fn rt_async_block_on(p: PromiseRef);

  // Microtasks + timers (queueMicrotask / setTimeout / setInterval).
  pub fn rt_async_sleep(delay_ms: u64) -> PromiseRef;
  pub fn rt_queue_microtask(cb: extern "C" fn(*mut u8), data: *mut u8);
  pub fn rt_queue_microtask_with_drop(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
  );
  pub fn rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId;
  pub fn rt_set_timeout_with_drop(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
    delay_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_set_interval_with_drop(
    cb: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
    interval_ms: u64,
  ) -> TimerId;
  pub fn rt_clear_timer(id: TimerId);

  // I/O watchers (epoll-backed readiness notifications).
  pub fn rt_io_register(
    fd: i32,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
  ) -> IoWatcherId;
  pub fn rt_io_register_with_drop(
    fd: i32,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
  ) -> IoWatcherId;
  pub fn rt_io_update(id: IoWatcherId, interests: u32);
  pub fn rt_io_unregister(id: IoWatcherId);

  // Async runtime diagnostics/limits.
  pub fn rt_async_set_limits(max_steps: usize, max_queue_len: usize);
  pub fn rt_async_take_last_error() -> *mut c_char;
  pub fn rt_async_free_c_string(s: *mut c_char);

  // Legacy promise/coroutine ABI (temporary; will be removed once codegen migrates).
  pub fn rt_promise_new_legacy() -> LegacyPromiseRef;
  pub fn rt_promise_resolve_legacy(p: LegacyPromiseRef, value: ValueRef);
  pub fn rt_promise_resolve_into_legacy(p: LegacyPromiseRef, value: PromiseResolveInput);
  pub fn rt_promise_resolve_promise_legacy(p: LegacyPromiseRef, other: LegacyPromiseRef);
  pub fn rt_promise_resolve_thenable_legacy(p: LegacyPromiseRef, thenable: ThenableRef);
  pub fn rt_promise_reject_legacy(p: LegacyPromiseRef, err: ValueRef);
  pub fn rt_promise_then_legacy(p: LegacyPromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8);
  pub fn rt_promise_then_with_drop_legacy(
    p: LegacyPromiseRef,
    on_settle: extern "C" fn(*mut u8),
    data: *mut u8,
    drop_data: extern "C" fn(*mut u8),
  );

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

    assert!(size_of::<PromiseRef>() == 8);
    assert!(align_of::<PromiseRef>() == 8);

    assert!(size_of::<GcPtr>() == 8);
    assert!(align_of::<GcPtr>() == 8);
    assert!(size_of::<GcHandle>() == 8);
    assert!(align_of::<GcHandle>() == 8);

    assert!(size_of::<StringRef>() == 16);
    assert!(align_of::<StringRef>() == 8);

    assert!(size_of::<PromiseLayout>() == 16);
    assert!(align_of::<PromiseLayout>() == 8);

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

    assert!(size_of::<RtShapeDescriptor>() == 24);
    assert!(align_of::<RtShapeDescriptor>() == 8);

    assert!(size_of::<AtomicU64>() == 8);
    assert!(align_of::<AtomicU64>() == 8);
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
      "RT_PROMISE_RESOLVE_VALUE",
      "RT_PROMISE_RESOLVE_PROMISE",
      "RT_PROMISE_RESOLVE_THENABLE",
      "RT_CORO_DONE",
      "RT_CORO_PENDING",
      "RT_CORO_YIELD",
      "RT_ASYNC_ABI_VERSION",
    ] {
      assert!(header.contains(c), "missing constant `{c}` in generated header");
    }

    // Types.
    assert!(
      header.contains("typedef struct StringRef") || header.contains("typedef struct StringRef {"),
      "missing StringRef typedef"
    );
    for ty in [
      "RtShapeId",
      "InternedId",
      "TaskId",
      "TimerId",
      "IoWatcherId",
      "RtFd",
      "HandleId",
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
      "GcPtr",
      "GcHandle",
      "RtArrayHeader",
    ] {
      assert!(header.contains(ty), "missing type `{ty}` in generated header");
    }
    assert!(
      header.contains("typedef struct Coroutine Coroutine;") || header.contains("struct Coroutine;"),
      "missing Coroutine forward declaration"
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
      header.contains("typedef struct RtPromise RtPromise;") || header.contains("struct RtPromise;"),
      "missing RtPromise forward declaration"
    );

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
      "RT_GC_EPOCH",
      "rt_gc_safepoint(",
      "rt_gc_safepoint_slow(",
      "rt_gc_poll(",
      "rt_gc_safepoint_relocate_h(",
      "rt_keep_alive_gc_ref(",
      "rt_write_barrier(",
      "rt_write_barrier_range(",
      "rt_gc_collect(",
      "rt_backing_store_external_bytes(",
      "rt_stackmaps_register(",
      "rt_stackmaps_unregister(",
      "rt_root_push(",
      "rt_root_pop(",
      "rt_global_root_register(",
      "rt_global_root_unregister(",
      "rt_gc_register_root_slot(",
      "rt_gc_unregister_root_slot(",
      "rt_gc_pin(",
      "rt_gc_unpin(",
      "rt_gc_root_get(",
      "rt_gc_root_set(",
      "rt_handle_alloc(",
      "rt_handle_free(",
      "rt_handle_load(",
      "rt_handle_store(",
      "rt_gc_set_young_range(",
      "rt_gc_get_young_range(",
      "rt_weak_add(",
      "rt_weak_get(",
      "rt_weak_remove(",
      "rt_string_concat(",
      "rt_string_intern(",
      "rt_string_pin_interned(",
      "rt_parallel_spawn(",
      "rt_parallel_join(",
      "rt_parallel_for(",
      "rt_parallel_spawn_promise(",
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
      "rt_queue_microtask_with_drop(",
      "rt_set_timeout(",
      "rt_set_timeout_with_drop(",
      "rt_set_interval(",
      "rt_set_interval_with_drop(",
      "rt_clear_timer(",
      "rt_io_register(",
      "rt_io_register_with_drop(",
      "rt_io_update(",
      "rt_io_unregister(",
      "rt_async_set_limits(",
      "rt_async_take_last_error(",
      "rt_async_free_c_string(",
      "rt_promise_new_legacy(",
      "rt_promise_resolve_legacy(",
      "rt_promise_resolve_into_legacy(",
      "rt_promise_resolve_promise_legacy(",
      "rt_promise_resolve_thenable_legacy(",
      "rt_promise_reject_legacy(",
      "rt_promise_then_legacy(",
      "rt_promise_then_with_drop_legacy(",
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
  }
}
