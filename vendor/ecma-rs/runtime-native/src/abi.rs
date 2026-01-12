use core::ffi::c_void;

pub use runtime_native_abi::{
  Coroutine, CoroutineId, InternedId, LegacyPromiseRef, Microtask, PromiseRef, RtGcConfig, RtGcLimits,
  RtParallelForBodyFn, RtShapeDescriptor, RtShapeId, RtThreadKind, RtTaskFn, StringRef, TaskId,
};

/// Identifier for a timer returned by `rt_set_timeout` / `rt_set_interval`.
pub type TimerId = u64;

/// Opaque value reference.
///
/// The full JS value/GC story is not implemented yet; compiled code can treat this as a pointer
/// payload.
pub type ValueRef = *mut c_void;

// -----------------------------------------------------------------------------
// Promise resolution ABI (PromiseResolve / thenable assimilation)
// -----------------------------------------------------------------------------

/// Tag for [`PromiseResolveInput`].
///
/// This is the native runtime equivalent of ECMAScript's "promise resolution
/// procedure" input, allowing codegen to explicitly represent:
/// - an immediate value,
/// - another runtime promise (adoption),
/// - or a typed thenable (`PromiseLike`) that must be assimilated.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromiseResolveKind {
  /// A non-thenable value.
  Value = 0,
  /// Another runtime-native promise that should be adopted.
  Promise = 1,
  /// A typed thenable that must be assimilated by calling its `then`.
  Thenable = 2,
}

/// Callback passed to a typed thenable's `then` implementation.
///
/// This corresponds to the `resolve` function in the spec's
/// `PromiseResolveThenableJob`.
pub type ThenableResolveCallback = extern "C" fn(*mut u8, PromiseResolveInput);

/// Callback passed to a typed thenable's `then` implementation.
///
/// This corresponds to the `reject` function in the spec's
/// `PromiseResolveThenableJob`.
pub type ThenableRejectCallback = extern "C" fn(*mut u8, ValueRef);

/// VTable describing a typed thenable (`PromiseLike<T>`).
///
/// Generated code can represent any `T: PromiseLike<U>` as `(ptr, vtable)` and
/// invoke the `then` method via [`ThenableVTable::call_then`], without dynamic
/// property lookup.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ThenableVTable {
  /// Call `thenable.then(on_fulfilled, on_rejected)`.
  ///
  /// Returns a non-null `ValueRef` if calling `then` synchronously "throws"
  /// (represented in this milestone runtime as an error payload pointer).
  pub call_then: unsafe extern "C" fn(
    thenable: *mut u8,
    on_fulfilled: ThenableResolveCallback,
    on_rejected: ThenableRejectCallback,
    data: *mut u8,
  ) -> ValueRef,
}

/// ABI representation of a typed thenable value.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
      kind: PromiseResolveKind::Value,
      payload: PromiseResolvePayload { value },
    }
  }

  #[inline]
  pub const fn promise(promise: LegacyPromiseRef) -> Self {
    Self {
      kind: PromiseResolveKind::Promise,
      payload: PromiseResolvePayload { promise },
    }
  }

  #[inline]
  pub const fn thenable(thenable: ThenableRef) -> Self {
    Self {
      kind: PromiseResolveKind::Thenable,
      payload: PromiseResolvePayload { thenable },
    }
  }
}
/// Optional GC/runtime statistics snapshot exposed for debugging/benching.
///
/// Enabled by the `gc_stats` Cargo feature.
#[cfg(feature = "gc_stats")]
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

// -----------------------------------------------------------------------------
// Coroutine ABI (LLVM-generated async/await state machines)
// -----------------------------------------------------------------------------

/// Status code returned by a coroutine `resume` function.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtCoroStatus {
  /// The coroutine is finished (it should have resolved/rejected its promise).
  Done = 0,
  /// The coroutine is suspended on an awaited promise.
  Pending = 1,
  /// The coroutine requested a cooperative yield (runtime will reschedule it).
  Yield = 2,
}

/// Header that prefixes all LLVM-generated coroutine frame payload structs.
///
/// Generated code must ensure this is the first field of the coroutine payload struct
/// (`#[repr(C)]`).
///
/// When coroutine frames are allocated in the GC heap, the full allocation begins with the runtime
/// [`crate::gc::ObjHeader`] prefix. In that case, the pointer passed to the legacy coroutine ABI
/// (`*mut RtCoroutineHeader`) is a *derived pointer* to the payload immediately after the
/// `ObjHeader`.
#[repr(C)]
pub struct RtCoroutineHeader {
  /// Entry point for resuming the coroutine.
  pub resume: extern "C" fn(*mut RtCoroutineHeader) -> RtCoroStatus,
  /// Promise returned to the caller from `rt_async_spawn`.
  pub promise: LegacyPromiseRef,
  /// Program counter/state used by the generated state machine.
  pub state: u32,
  /// Whether the awaited promise rejected (0 = fulfilled, 1 = rejected).
  pub await_is_error: u32,
  /// Value produced by the last `await` (valid when `await_is_error == 0`).
  pub await_value: ValueRef,
  /// Error produced by the last `await` (valid when `await_is_error == 1`).
  pub await_error: ValueRef,
}

// -----------------------------------------------------------------------------
// I/O watchers (reactor-backed readiness notifications)
// -----------------------------------------------------------------------------

pub type IoWatcherId = u64;
pub type RtFd = i32;

pub const RT_IO_READABLE: u32 = 0x1;
pub const RT_IO_WRITABLE: u32 = 0x2;
pub const RT_IO_ERROR: u32 = 0x4;

const fn max_usize(a: usize, b: usize) -> usize {
  if a > b { a } else { b }
}

const fn round_up(value: usize, align: usize) -> usize {
  (value + align - 1) / align * align
}

const _: () = {
  use core::mem::{align_of, size_of};

  const PTR_SIZE: usize = size_of::<*const u8>();
  const PTR_ALIGN: usize = align_of::<*const u8>();

  // `StringRef` is `{ const uint8_t* ptr; size_t len; }` in the C header.
  const USIZE_SIZE: usize = size_of::<usize>();
  const USIZE_ALIGN: usize = align_of::<usize>();
  const STRINGREF_ALIGN: usize = max_usize(PTR_ALIGN, USIZE_ALIGN);
  const STRINGREF_OFF_LEN: usize = round_up(PTR_SIZE, USIZE_ALIGN);
  const STRINGREF_SIZE: usize = round_up(STRINGREF_OFF_LEN + USIZE_SIZE, STRINGREF_ALIGN);

  if size_of::<StringRef>() != STRINGREF_SIZE {
    panic!("StringRef ABI size mismatch");
  }
  if align_of::<StringRef>() != STRINGREF_ALIGN {
    panic!("StringRef ABI alignment mismatch");
  }

  // `RtCoroutineHeader` layout is part of the compiler/runtime ABI contract.
  const RESUME_SIZE: usize = size_of::<extern "C" fn(*mut RtCoroutineHeader) -> RtCoroStatus>();
  const RESUME_ALIGN: usize = align_of::<extern "C" fn(*mut RtCoroutineHeader) -> RtCoroStatus>();
  const PROMISE_SIZE: usize = size_of::<LegacyPromiseRef>();
  const PROMISE_ALIGN: usize = align_of::<LegacyPromiseRef>();
  const U32_SIZE: usize = size_of::<u32>();
  const U32_ALIGN: usize = align_of::<u32>();
  const VALUE_SIZE: usize = size_of::<ValueRef>();
  const VALUE_ALIGN: usize = align_of::<ValueRef>();

  const HEADER_ALIGN: usize = max_usize(
    max_usize(RESUME_ALIGN, PROMISE_ALIGN),
    max_usize(U32_ALIGN, VALUE_ALIGN),
  );

  const HEADER_OFF_PROMISE: usize = round_up(RESUME_SIZE, PROMISE_ALIGN);
  const HEADER_OFF_STATE: usize = round_up(HEADER_OFF_PROMISE + PROMISE_SIZE, U32_ALIGN);
  const HEADER_OFF_AWAIT_IS_ERROR: usize = round_up(HEADER_OFF_STATE + U32_SIZE, U32_ALIGN);
  const HEADER_OFF_AWAIT_VALUE: usize =
    round_up(HEADER_OFF_AWAIT_IS_ERROR + U32_SIZE, VALUE_ALIGN);
  const HEADER_OFF_AWAIT_ERROR: usize = round_up(HEADER_OFF_AWAIT_VALUE + VALUE_SIZE, VALUE_ALIGN);
  const HEADER_SIZE: usize = round_up(HEADER_OFF_AWAIT_ERROR + VALUE_SIZE, HEADER_ALIGN);

  if size_of::<RtCoroutineHeader>() != HEADER_SIZE {
    panic!("RtCoroutineHeader ABI size mismatch");
  }
  if align_of::<RtCoroutineHeader>() != HEADER_ALIGN {
    panic!("RtCoroutineHeader ABI alignment mismatch");
  }
};
