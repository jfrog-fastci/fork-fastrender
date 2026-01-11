use core::ffi::c_void;

/// A stable identifier for an interned UTF-8 string.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InternedId(pub u32);

/// Identifier for a parallel task.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

/// Opaque handle to a promise/coroutine managed by the runtime.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PromiseRef(pub *mut c_void);

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

