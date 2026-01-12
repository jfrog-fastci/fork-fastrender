use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use vm_js::{Heap, HeapLimits, VmError};

thread_local! {
  static FAIL_ALLOC: Cell<bool> = Cell::new(false);
}

struct FailingAlloc;

#[global_allocator]
static GLOBAL_ALLOCATOR: FailingAlloc = FailingAlloc;

unsafe impl GlobalAlloc for FailingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if FAIL_ALLOC.with(|f| f.get()) {
      return std::ptr::null_mut();
    }
    System.alloc(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout);
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    if FAIL_ALLOC.with(|f| f.get()) {
      return std::ptr::null_mut();
    }
    System.realloc(ptr, layout, new_size)
  }
}

fn set_fail_allocations(fail: bool) {
  FAIL_ALLOC.with(|f| f.set(fail));
}

#[test]
fn numeric_index_string_formatting_does_not_abort_on_allocator_oom() -> Result<(), VmError> {
  // This test guards against infallible intermediate `String` allocations in index formatting
  // (e.g. `i.to_string()`), which can abort the process under allocator OOM.
  //
  // We simulate allocator exhaustion after `[[OwnPropertyKeys]]` has performed its upfront
  // allocations, right before it begins per-index formatting.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();

  let len = 2048usize;
  let buf = scope.alloc_array_buffer(len)?;
  let view = scope.alloc_uint8_array(buf, 0, len)?;

  let mut first_tick = true;
  let result = scope.ordinary_own_property_keys_with_tick(view, || {
    if first_tick {
      first_tick = false;
      set_fail_allocations(true);
    }
    Ok(())
  });

  // Turn allocations back on so the test harness can continue normally.
  set_fail_allocations(false);

  match result {
    Err(VmError::OutOfMemory) => Ok(()),
    Err(e) => Err(e),
    Ok(_) => panic!("expected VmError::OutOfMemory"),
  }
}

