use crate::VmError;
use core::alloc::Layout;
use core::mem;
use core::ptr;
use std::alloc::{alloc, dealloc};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

/// Fallible `Box<T>` allocation that reports `VmError::OutOfMemory` instead of aborting.
///
/// Rust's standard `Box::new` will abort the process on allocator OOM. For hostile JavaScript run
/// under extreme memory pressure (e.g. a small RLIMIT_AS), we need box allocations to be
/// recoverable.
pub(crate) fn box_try_new_vm<T>(value: T) -> Result<Box<T>, VmError> {
  // `alloc` with a zero-sized layout is allowed to return null. Avoid misclassifying that as OOM by
  // delegating to `Box::new`, which does not allocate for ZSTs.
  if mem::size_of::<T>() == 0 {
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

// SAFETY: This mirrors the allocation layout used by `std::sync::Arc`: refcounts followed by the
// payload. We use this only so we can allocate an `Arc<T>` fallibly (returning
// `VmError::OutOfMemory` instead of aborting the process via the global allocator's OOM handler).
#[repr(C)]
struct ArcInner<T> {
  strong: AtomicUsize,
  weak: AtomicUsize,
  data: T,
}

/// Fallible `Arc<T>` allocation that reports `VmError::OutOfMemory` instead of aborting.
///
/// Rust's standard `Arc::new` will abort the process on allocator OOM. `vm-js` frequently stores
/// attacker-controlled structures (source text, parsed ASTs, compiled scripts) inside `Arc` in hot
/// runtime paths; those allocations must be recoverable under memory pressure.
///
/// ## Safety
///
/// This function allocates a `std::sync::Arc` payload using the same layout as the standard
/// library:
/// - `strong` refcount
/// - `weak` refcount (the implicit weak count held by all `Arc`s)
/// - payload (`T`)
///
/// It then constructs the `Arc<T>` using `Arc::from_raw` with a pointer to the `data` field.
pub(crate) fn arc_try_new_vm<T>(value: T) -> Result<Arc<T>, VmError> {
  let layout = Layout::new::<ArcInner<T>>();
  // SAFETY: We allocate enough space for `ArcInner<T>` and initialise all fields before converting
  // it into an `Arc<T>` via `Arc::from_raw`.
  unsafe {
    let raw = alloc(layout) as *mut ArcInner<T>;
    if raw.is_null() {
      return Err(VmError::OutOfMemory);
    }

    // Ensure we deallocate the raw allocation if something panics before we hand ownership to the
    // returned `Arc<T>`. This is only for robustness (the initialisation code below should not
    // panic in practice).
    struct AllocationGuard<T> {
      ptr: *mut ArcInner<T>,
      layout: Layout,
      data_init: bool,
    }

    impl<T> Drop for AllocationGuard<T> {
      fn drop(&mut self) {
        if self.ptr.is_null() {
          return;
        }
        // SAFETY: `ptr` was allocated with `layout`, and `data` was only written when `data_init`
        // is true.
        unsafe {
          if self.data_init {
            ptr::drop_in_place(ptr::addr_of_mut!((*self.ptr).data));
          }
          dealloc(self.ptr as *mut u8, self.layout);
        }
      }
    }

    let mut guard = AllocationGuard {
      ptr: raw,
      layout,
      data_init: false,
    };

    // `Arc::new` initialises both counts to 1 (the implicit weak count).
    ptr::addr_of_mut!((*raw).strong).write(AtomicUsize::new(1));
    ptr::addr_of_mut!((*raw).weak).write(AtomicUsize::new(1));
    ptr::addr_of_mut!((*raw).data).write(value);
    guard.data_init = true;

    // Ownership transferred to the returned `Arc<T>`.
    guard.ptr = ptr::null_mut();

    Ok(Arc::from_raw(ptr::addr_of!((*raw).data)))
  }
}

pub(crate) fn arc_str_try_from_vm(value: &str) -> Result<Arc<str>, VmError> {
  // Allocate an `ArcInner<str>` by hand so allocator OOM becomes recoverable.
  //
  // Layout is `ArcInnerHeader { strong, weak }` followed by the UTF-8 bytes.
  #[repr(C)]
  struct ArcInnerHeader {
    strong: AtomicUsize,
    weak: AtomicUsize,
  }

  let header_layout = Layout::new::<ArcInnerHeader>();
  let data_layout = Layout::array::<u8>(value.len()).map_err(|_| VmError::OutOfMemory)?;
  let (layout, offset) = header_layout
    .extend(data_layout)
    .map_err(|_| VmError::OutOfMemory)?;
  let layout = layout.pad_to_align();

  // SAFETY: We allocate enough space for the header + bytes, initialise both refcounts and the
  // byte slice, then build an `Arc<str>` from the raw pointer to the str payload.
  unsafe {
    let raw = alloc(layout);
    if raw.is_null() {
      return Err(VmError::OutOfMemory);
    }

    let header = raw as *mut ArcInnerHeader;
    ptr::addr_of_mut!((*header).strong).write(AtomicUsize::new(1));
    ptr::addr_of_mut!((*header).weak).write(AtomicUsize::new(1));

    let data_ptr = raw.add(offset) as *mut u8;
    ptr::copy_nonoverlapping(value.as_ptr(), data_ptr, value.len());

    let bytes = core::slice::from_raw_parts(data_ptr as *const u8, value.len());
    let s = core::str::from_utf8_unchecked(bytes);
    Ok(Arc::from_raw(s as *const str))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_alloc::FailNextMatchingAllocGuard;

  #[test]
  fn arc_try_new_vm_returns_out_of_memory_on_alloc_failure() {
    const ARC_INNER_SIZE: usize = std::mem::size_of::<ArcInner<u64>>();
    const ARC_INNER_ALIGN: usize = std::mem::align_of::<ArcInner<u64>>();
    let _guard = FailNextMatchingAllocGuard::new(ARC_INNER_SIZE, ARC_INNER_ALIGN);

    let err = arc_try_new_vm(123u64).expect_err("expected OOM error");
    assert!(matches!(err, VmError::OutOfMemory));
  }

  #[test]
  fn arc_str_try_from_vm_returns_out_of_memory_on_alloc_failure() {
    #[repr(C)]
    struct ArcInnerHeader {
      strong: AtomicUsize,
      weak: AtomicUsize,
    }

    let value = "hello world";
    let header_layout = Layout::new::<ArcInnerHeader>();
    let data_layout = Layout::array::<u8>(value.len()).expect("layout for bytes");
    let (layout, _offset) = header_layout.extend(data_layout).expect("combined layout");
    let layout = layout.pad_to_align();

    let _guard = FailNextMatchingAllocGuard::new(layout.size(), layout.align());

    let err = arc_str_try_from_vm(value).expect_err("expected OOM error");
    assert!(matches!(err, VmError::OutOfMemory));
  }
}
