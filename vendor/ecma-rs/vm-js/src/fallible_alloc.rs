use crate::VmError;
use core::alloc::Layout;
use core::mem;
use core::ptr;
use std::alloc::alloc;

/// Fallible `Box<T>` allocation that reports `VmError::OutOfMemory` instead of aborting.
///
/// Rust's standard `Box::new` will abort the process on allocator OOM. For hostile JavaScript run
/// under extreme memory pressure (e.g. a small RLIMIT_AS), we need box allocations to be
/// recoverable.
pub(crate) fn box_try_new_vm<T>(value: T) -> Result<Box<T>, VmError> {
  let size = mem::size_of::<T>();
  if size == 0 {
    // ZST boxes don't allocate; they can't fail.
    return Ok(Box::new(value));
  }

  let layout = Layout::new::<T>();
  // SAFETY: We allocate enough space for `T` and immediately initialise it before converting it
  // into a `Box<T>`.
  unsafe {
    let raw = alloc(layout) as *mut T;
    if raw.is_null() {
      return Err(VmError::OutOfMemory);
    }
    ptr::write(raw, value);
    Ok(Box::from_raw(raw))
  }
}

