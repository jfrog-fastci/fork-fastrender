use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
  /// Fail **all** allocation requests on the current thread.
  static FAIL_ALL: Cell<bool> = Cell::new(false);

  /// Fail the next allocation request matching `(size, align)` on the current thread.
  ///
  /// This is reset to `None` after the first matching allocation attempt.
  static FAIL_NEXT_LAYOUT: Cell<Option<(usize, usize)>> = Cell::new(None);
}

pub(crate) struct TestAllocator;

#[global_allocator]
static GLOBAL_ALLOCATOR: TestAllocator = TestAllocator;

fn should_fail_alloc(size: usize, align: usize) -> bool {
  if FAIL_ALL.with(|f| f.get()) {
    return true;
  }

  let mut fail = false;
  FAIL_NEXT_LAYOUT.with(|slot| {
    if slot.get() == Some((size, align)) {
      slot.set(None);
      fail = true;
    }
  });
  fail
}

unsafe impl GlobalAlloc for TestAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if should_fail_alloc(layout.size(), layout.align()) {
      return std::ptr::null_mut();
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    if should_fail_alloc(layout.size(), layout.align()) {
      return std::ptr::null_mut();
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    if should_fail_alloc(new_size, layout.align()) {
      return std::ptr::null_mut();
    }
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

pub(crate) struct FailAllocsGuard;

impl FailAllocsGuard {
  pub(crate) fn new() -> Self {
    FAIL_ALL.with(|f| f.set(true));
    Self
  }
}

impl Drop for FailAllocsGuard {
  fn drop(&mut self) {
    FAIL_ALL.with(|f| f.set(false));
  }
}

pub(crate) struct FailNextMatchingAllocGuard {
  size: usize,
  align: usize,
}

impl FailNextMatchingAllocGuard {
  pub(crate) fn new(size: usize, align: usize) -> Self {
    FAIL_NEXT_LAYOUT.with(|slot| slot.set(Some((size, align))));
    Self { size, align }
  }
}

impl Drop for FailNextMatchingAllocGuard {
  fn drop(&mut self) {
    // If no matching allocation happened, clear the request so later tests on the same thread don't
    // inherit it.
    FAIL_NEXT_LAYOUT.with(|slot| {
      if slot.get() == Some((self.size, self.align)) {
        slot.set(None);
      }
    });
  }
}

