use core::ffi::c_void;

/// A stable identifier for an interned UTF-8 string.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InternedId(pub u32);

/// Identifier for a parallel task.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

/// Opaque value reference.
///
/// The full JS value/GC story is not implemented yet; compiled code can treat this as a pointer
/// payload.
pub type ValueRef = *mut c_void;

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

/// Shape identifier used by the AOT compiler to refer to statically-known object layouts.
///
/// For now this is just a 128-bit value (passed through, unused by the milestone runtime).
pub type ShapeId = u128;

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

/// Header that prefixes all LLVM-generated coroutine frame structs.
///
/// Generated code must ensure this is the first field of the frame struct (`#[repr(C)]`).
#[repr(C)]
pub struct RtCoroutineHeader {
  /// Entry point for resuming the coroutine.
  pub resume: extern "C" fn(*mut RtCoroutineHeader) -> RtCoroStatus,
  /// Promise returned to the caller from `rt_async_spawn`.
  pub promise: PromiseRef,
  /// Program counter/state used by the generated state machine.
  pub state: u32,
  /// Whether the awaited promise rejected (0 = fulfilled, 1 = rejected).
  pub await_is_error: u32,
  /// Value produced by the last `await` (valid when `await_is_error == 0`).
  pub await_value: ValueRef,
  /// Error produced by the last `await` (valid when `await_is_error == 1`).
  pub await_error: ValueRef,
}

