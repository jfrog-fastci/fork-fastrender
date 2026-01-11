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

use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicUsize;

use crate::abi::RtShapeId;

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

  /// Promise reaction list head.
  ///
  /// This is an intrusive singly-linked list of [`crate::promise_reactions::PromiseReactionNode`]
  /// objects, stored as a raw pointer cast to `usize`.
  ///
  /// The list is pushed in LIFO order and drained by the runtime in FIFO order by reversing the
  /// list before scheduling reaction jobs.
  ///
  /// Stored values:
  /// - `0`: no reactions yet.
  /// - `ptr`: a `*mut PromiseReactionNode` cast to `usize` (head of list).
  pub reactions: AtomicUsize,

  /// Reserved for runtime flags (e.g. unhandled rejection tracking).
  pub flags: AtomicU8,
}

impl PromiseHeader {
  pub const PENDING: PromiseState = 0;
  pub const FULFILLED: PromiseState = 1;
  pub const REJECTED: PromiseState = 2;
}

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

  /// Allocation size in bytes of the coroutine's result promise (`Promise<T>`).
  pub promise_size: u32,
  /// Allocation alignment of the coroutine's result promise (`Promise<T>`).
  pub promise_align: u32,
  /// Runtime "shape id" for the promise object allocation.
  ///
  /// This must be a runtime-local [`RtShapeId`] index into the shape table registered via
  /// `rt_register_shape_table` (not the semantic `types_ts_interned::ShapeId`).
  pub promise_shape_id: RtShapeId,

  /// ABI version for forward compatibility (generated code should set to `0` for now).
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

  /// Intrusive list pointer used by the runtime while the coroutine is suspended.
  pub next_waiter: *mut Coroutine,

  /// Reserved for runtime flags (e.g. scheduled/running bits).
  pub flags: u32,
}

// Safety: `Coroutine` is a plain ABI header embedded at the start of a coroutine frame
// allocation. The scheduler may move coroutine frames (and/or handles to them) across
// threads; dereferencing the raw pointers remains `unsafe` and is governed by the runtime's
// own synchronization rules.
unsafe impl Send for Coroutine {}

/// Opaque pointer to a coroutine frame (and therefore the start of a generated coroutine).
pub type CoroutineRef = *mut Coroutine;

// ---- Compile-time checks ----

const _: () = {
  assert!(core::mem::align_of::<PromiseHeader>() >= 8);

  let ptr_size = core::mem::size_of::<usize>();
  let ptr_align = core::mem::align_of::<usize>();

  // C header layout (`include/runtime_native.h`):
  //   vtable ptr, promise ptr, next_waiter ptr, flags u32
  assert!(core::mem::align_of::<Coroutine>() == ptr_align);
  let raw_size = (3 * ptr_size) + core::mem::size_of::<u32>();
  let expected_size = (raw_size + (ptr_align - 1)) & !(ptr_align - 1);
  assert!(core::mem::size_of::<Coroutine>() == expected_size);
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
