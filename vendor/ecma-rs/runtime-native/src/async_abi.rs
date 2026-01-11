#![doc = include_str!("../docs/async_abi.md")]

//! C-compatible async/await ABI for the native runtime.
//!
//! This module contains the shared layout contract between:
//! - The compiler/codegen (which emits `Promise<T>` allocations and coroutine frames), and
//! - The native runtime (which schedules coroutines and resolves/rejects promises).
//!
//! ## Layout invariants for generated code
//! - Every generated `Promise<T>` allocation begins with a [`PromiseHeader`] at offset 0.
//!   The payload `T` begins immediately after the header; the compiler chooses the full
//!   allocation layout, but the runtime only relies on the header prefix.
//! - Every generated coroutine frame begins with a [`Coroutine`] header at offset 0. The
//!   coroutine's locals/state machine fields follow immediately after.
//! - A coroutine must resolve/reject `coro.promise` itself before returning
//!   [`CoroutineStepTag::Complete`] from its `resume` function.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::abi::RtShapeId;

use static_assertions::{const_assert, const_assert_eq};

/// ABI version tag for the native coroutine ABI.
///
/// Generated code must set [`CoroutineVTable::abi_version`] to this value. The runtime validates the
/// version before calling into compiler-provided function pointers.
pub const RT_ASYNC_ABI_VERSION: u32 = runtime_native_abi::RT_ASYNC_ABI_VERSION;

/// Promise state stored in [`PromiseHeader::state`].
pub type PromiseState = u8;

/// Header prefix embedded at offset 0 of every generated `Promise<T>` object.
///
/// This type must remain:
/// - `#[repr(C)]` and ABI-stable,
/// - free of drop glue (only POD/atomics),
/// - and safe to embed as a prefix in a larger allocation.
#[repr(C, align(8))]
pub struct PromiseHeader {
  /// Current promise state.
  ///
  /// Values are [`PromiseHeader::PENDING`], [`PromiseHeader::FULFILLED`], or
  /// [`PromiseHeader::REJECTED`].
  pub state: AtomicU8,

  /// Promise reaction/waiter list head.
  ///
  /// Stored values:
  /// - `0`: no waiters/reactions registered yet.
  /// - [`PromiseHeader::WAITERS_CLOSED`]: reserved sentinel for a closed list (not currently used by
  ///   the runtime).
  /// - otherwise: a raw pointer to the head waiter node, cast to `usize`.
  ///
  /// The runtime currently uses this field to register await/then callbacks as an intrusive
  /// singly-linked list of promise reaction nodes (see `promise_reactions`).
  pub waiters: AtomicUsize,

  /// Reserved for runtime flags (e.g. unhandled rejection tracking).
  pub flags: AtomicU8,
}

impl PromiseHeader {
  pub const PENDING: PromiseState = 0;
  pub const FULFILLED: PromiseState = 1;
  pub const REJECTED: PromiseState = 2;

  /// Sentinel value stored in [`PromiseHeader::waiters`] once the promise has settled and will no
  /// longer accept waiter registrations.
  ///
  /// Note: the current runtime does not yet use this sentinel; it is reserved for a future lock-free
  /// protocol that closes the waiter list after settlement.
  ///
  /// This value must never alias a valid pointer. `1` is chosen because waiter nodes are at least
  /// pointer-aligned (and therefore cannot have an odd address).
  pub const WAITERS_CLOSED: usize = 1;

  #[inline]
  pub fn load_state(&self) -> PromiseState {
    self.state.load(Ordering::Acquire)
  }

  /// Attempt to transition this promise from [`PromiseHeader::PENDING`] to a final state.
  ///
  /// Returns `true` iff the caller won the transition race.
  #[inline]
  pub fn try_transition_state(&self, target: PromiseState) -> bool {
    self
      .state
      .compare_exchange(Self::PENDING, target, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
  }

  /// Promise has been marked "handled" for unhandled rejection tracking.
  pub const FLAG_HANDLED: u8 = 0x1;

  #[inline]
  pub fn is_handled(&self) -> bool {
    (self.flags.load(Ordering::Acquire) & Self::FLAG_HANDLED) != 0
  }

  /// Mark the promise as handled and return whether this call transitioned the flag.
  #[inline]
  pub fn mark_handled(&self) -> bool {
    let prev = self.flags.fetch_or(Self::FLAG_HANDLED, Ordering::AcqRel);
    (prev & Self::FLAG_HANDLED) == 0
  }
}

/// Runtime-internal flag bit in [`PromiseHeader::flags`] indicating this promise has an associated
/// out-of-line payload buffer (created by `rt_parallel_spawn_promise`).
///
/// The payload pointer can be retrieved via `rt_promise_payload_ptr`.
pub(crate) const PROMISE_FLAG_HAS_PAYLOAD: u8 = 1 << 1;

/// Runtime-internal flag bit in [`PromiseHeader::flags`] indicating the promise is tied to a piece
/// of pending "external" work (e.g. a task spawned by `rt_parallel_spawn_promise`).
///
/// While this flag is set, the JS-shaped event loop should not report itself as fully idle, even if
/// the task queues are empty.
pub(crate) const PROMISE_FLAG_EXTERNAL_PENDING: u8 = 1 << 2;

/// Opaque pointer to a promise header (and therefore the start of a generated `Promise<T>`).
pub type PromiseRef = *mut PromiseHeader;

/// Discriminant for [`CoroutineStep`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoroutineStepTag {
  /// The coroutine yielded and is awaiting another promise.
  Await = 0,
  /// The coroutine completed and has already resolved/rejected its own promise.
  Complete = 1,
}

/// Result of a single coroutine resume step.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CoroutineStep {
  pub tag: CoroutineStepTag,
  /// For [`CoroutineStepTag::Await`], the promise being awaited.
  ///
  /// For [`CoroutineStepTag::Complete`], this must be null.
  pub await_promise: PromiseRef,
}

impl CoroutineStep {
  #[inline]
  pub const fn await_(p: PromiseRef) -> Self {
    Self {
      tag: CoroutineStepTag::Await,
      await_promise: p,
    }
  }

  #[inline]
  pub const fn complete() -> Self {
    Self {
      tag: CoroutineStepTag::Complete,
      await_promise: core::ptr::null_mut(),
    }
  }
}

/// VTable describing a generated coroutine type.
#[repr(C)]
pub struct CoroutineVTable {
  /// Resume the coroutine state machine.
  ///
  /// The coroutine must return:
  /// - `Await(p)` to yield on another promise, or
  /// - `Complete` after resolving/rejecting `coro.promise`.
  pub resume: unsafe extern "C" fn(*mut Coroutine) -> CoroutineStep,

  /// Destroy (drop + deallocate) a coroutine frame.
  ///
  /// If [`CORO_FLAG_RUNTIME_OWNS_FRAME`] is set in `coro.flags`, the runtime will call this
  /// exactly once after the coroutine completes or is cancelled.
  ///
  /// If the flag is not set, the runtime must never call `destroy` (the caller owns the frame,
  /// e.g. stack-temporary frames for coroutines that cannot suspend).
  pub destroy: unsafe extern "C" fn(CoroutineRef),

  /// Allocation size in bytes of the coroutine's result promise (`Promise<T>`).
  pub promise_size: u32,
  /// Allocation alignment of the coroutine's result promise (`Promise<T>`).
  pub promise_align: u32,
  /// Runtime "shape id" for the promise object allocation.
  ///
  /// This must be a runtime-local [`RtShapeId`] index into the shape table registered via
  /// `rt_register_shape_table` (not the semantic `types_ts_interned::ShapeId`).
  pub promise_shape_id: RtShapeId,

  /// ABI version for forward compatibility.
  ///
  /// Generated code must set this to [`RT_ASYNC_ABI_VERSION`].
  pub abi_version: u32,
  /// Reserved for future ABI extensions; must be zeroed by generated code.
  pub reserved: [usize; 4],
}

/// Header embedded at offset 0 of every generated coroutine frame.
#[repr(C)]
pub struct Coroutine {
  pub vtable: *const CoroutineVTable,

  /// Result promise for this coroutine; written by `rt_async_spawn` before first resume.
  pub promise: PromiseRef,

  /// Intrusive list pointer used by the runtime while the coroutine is suspended (e.g. awaiting a
  /// promise).
  ///
  /// Note: the current runtime represents awaiters as separate promise reaction nodes stored in
  /// [`PromiseHeader::waiters`] (see `promise_reactions`), so this field is currently reserved for a
  /// future lock-free waiter protocol. Generated code should still initialize it to null.
  pub next_waiter: CoroutineRef,

  /// Reserved for runtime flags (e.g. scheduled/running bits).
  pub flags: u32,
}

/// Coroutine frame is owned by the runtime.
///
/// When set, the runtime will call `(*coro.vtable).destroy(coro)` exactly once after completion
/// or cancellation.
pub const CORO_FLAG_RUNTIME_OWNS_FRAME: u32 = 1 << 0;

/// Coroutine frame has been destroyed (runtime internal).
///
/// This bit is used to guard against double-destroy in cancellation/queueing edge cases.
pub const CORO_FLAG_DESTROYED: u32 = 1 << 1;

/// Debug aid: coroutine may suspend (yield `Await`) at runtime.
pub const CORO_FLAG_MAY_SUSPEND: u32 = 1 << 2;

// Safety: `Coroutine` is a plain ABI header embedded at the start of a coroutine frame
// allocation. The scheduler may move coroutine frames (and/or handles to them) across
// threads; dereferencing the raw pointers remains `unsafe` and is governed by the runtime's
// own synchronization rules.
unsafe impl Send for Coroutine {}

/// Opaque pointer to a coroutine frame (and therefore the start of a generated coroutine).
pub type CoroutineRef = *mut Coroutine;

// ---- Compile-time checks ----

const_assert_eq!(core::mem::align_of::<PromiseHeader>(), 8);
const_assert_eq!(core::mem::offset_of!(PromiseHeader, state), 0);
const_assert_eq!(
  core::mem::offset_of!(PromiseHeader, waiters),
  core::mem::size_of::<usize>()
);
const_assert_eq!(
  core::mem::offset_of!(PromiseHeader, flags),
  core::mem::size_of::<usize>() * 2
);
const_assert_eq!(
  core::mem::size_of::<PromiseHeader>(),
  core::mem::size_of::<usize>() * 2 + 8
);
const_assert!(
  core::mem::size_of::<PromiseHeader>() % core::mem::align_of::<PromiseHeader>() == 0
);

const _: () = {
  use core::mem::{align_of, offset_of, size_of};

  assert!(align_of::<PromiseHeader>() == 8);
  assert!(size_of::<PromiseHeader>() == 24);
  assert!(offset_of!(PromiseHeader, state) == 0);
  assert!(offset_of!(PromiseHeader, waiters) == 8);
  assert!(offset_of!(PromiseHeader, flags) == 16);

  // Keep the Rust ABI layout in sync with `include/runtime_native.h`.
  const PTR: usize = size_of::<*const u8>();
  const U32: usize = size_of::<u32>();

  let ptr_size = size_of::<usize>();
  let ptr_align = align_of::<usize>();

  // C header layout (`include/runtime_native.h`):
  //   vtable ptr, promise ptr, next_waiter ptr, flags u32
  assert!(align_of::<Coroutine>() == ptr_align);
  assert!(offset_of!(Coroutine, vtable) == 0);
  assert!(offset_of!(Coroutine, promise) == ptr_size);
  assert!(offset_of!(Coroutine, next_waiter) == 2 * ptr_size);
  assert!(offset_of!(Coroutine, flags) == 3 * ptr_size);
  let raw_size = (3 * ptr_size) + size_of::<u32>();
  let expected_size = (raw_size + (ptr_align - 1)) & !(ptr_align - 1);
  assert!(size_of::<Coroutine>() == expected_size);

  // `Coroutine` layout (vtable, promise, next_waiter, flags).
  assert!(offset_of!(Coroutine, vtable) == 0);
  assert!(offset_of!(Coroutine, promise) == PTR);
  assert!(offset_of!(Coroutine, next_waiter) == PTR * 2);
  assert!(offset_of!(Coroutine, flags) == PTR * 3);

  // `CoroutineVTable` layout (resume, destroy, promise_size/align/shape_id, abi_version, reserved).
  assert!(offset_of!(CoroutineVTable, resume) == 0);
  assert!(offset_of!(CoroutineVTable, destroy) == PTR);
  assert!(offset_of!(CoroutineVTable, promise_size) == PTR * 2);
  assert!(offset_of!(CoroutineVTable, promise_align) == PTR * 2 + U32);
  assert!(offset_of!(CoroutineVTable, promise_shape_id) == PTR * 2 + U32 * 2);
  assert!(offset_of!(CoroutineVTable, abi_version) == PTR * 2 + U32 * 3);
  assert!(offset_of!(CoroutineVTable, reserved) == PTR * 2 + U32 * 4);
};

#[allow(dead_code)]
fn _assert_abi_thread_safety() {
  fn assert_send_sync<T: Send + Sync>() {}
  fn assert_send<T: Send>() {}

  // `PromiseHeader` contains only atomics and POD fields.
  assert_send_sync::<PromiseHeader>();

  // Coroutine frames may be moved across threads by the scheduler.
  assert_send::<Coroutine>();
}
