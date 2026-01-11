use runtime_native::test_util::{reset_runtime_state, TestRuntimeGuard};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[repr(C)]
struct DropCounter {
  drops: Arc<AtomicUsize>,
}

impl Drop for DropCounter {
  fn drop(&mut self) {
    self.drops.fetch_add(1, Ordering::AcqRel);
  }
}

extern "C" fn noop(_data: *mut u8) {}

extern "C" fn drop_counter(data: *mut u8) {
  // Safety: allocated as `Box<DropCounter>` in the test setup.
  unsafe {
    drop(Box::from_raw(data as *mut DropCounter));
  }
}

#[test]
fn promise_then_with_drop_runs_drop_on_discard() {
  let _rt = TestRuntimeGuard::new();

  let drops = Arc::new(AtomicUsize::new(0));
  let data = Box::new(DropCounter { drops: drops.clone() });
  let data_ptr = Box::into_raw(data) as *mut u8;

  let promise = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_then_with_drop_legacy(promise, noop, data_ptr, drop_counter);
  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());

  // Discard the queued microtask (simulates teardown) and ensure the callback state is freed.
  reset_runtime_state();

  assert_eq!(drops.load(Ordering::Acquire), 1);
}

