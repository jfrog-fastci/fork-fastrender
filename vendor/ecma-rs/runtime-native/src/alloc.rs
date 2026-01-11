use core::ptr::NonNull;

use crate::trap;

/// Milestone-1 allocator.
///
/// - Uses `libc::malloc` / `libc::calloc` (suitable alignment for any value).
/// - Aborts on allocation failure (rather than returning null).
/// - Returned allocations are never freed yet (GC not implemented).
pub(crate) fn malloc_bytes(size: usize, context: &str) -> *mut u8 {
  if size == 0 {
    return NonNull::<u8>::dangling().as_ptr();
  }

  // Safety: libc guarantees `malloc` returns a pointer suitably aligned for any value.
  let ptr = unsafe { libc::malloc(size) as *mut u8 };
  if ptr.is_null() {
    trap::rt_trap_oom(size, context);
  }
  ptr
}

pub(crate) fn calloc_array(len: usize, elem_size: usize, context: &str) -> *mut u8 {
  if len == 0 || elem_size == 0 {
    return NonNull::<u8>::dangling().as_ptr();
  }

  let bytes = len
    .checked_mul(elem_size)
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("allocation size overflow"));

  // Safety: libc guarantees `calloc` returns a pointer suitably aligned for any value.
  let ptr = unsafe { libc::calloc(len, elem_size) as *mut u8 };
  if ptr.is_null() {
    trap::rt_trap_oom(bytes, context);
  }
  ptr
}
