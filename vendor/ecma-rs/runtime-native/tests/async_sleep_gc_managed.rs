use runtime_native::abi::PromiseRef;
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

extern "C" {
  fn rt_async_sleep(delay_ms: u64) -> PromiseRef;
}

struct ThreadInitGuard;

impl ThreadInitGuard {
  fn new() -> Self {
    runtime_native::rt_thread_init(0);
    Self
  }
}

impl Drop for ThreadInitGuard {
  fn drop(&mut self) {
    runtime_native::rt_thread_deinit();
  }
}

struct WeakHandleGuard(u64);

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      runtime_native::rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
}

#[test]
fn async_sleep_returns_gc_managed_collectible_promise() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadInitGuard::new();

  let mut p = unsafe { rt_async_sleep(0) };
  assert!(!p.is_null(), "rt_async_sleep returned a null promise");

  // Create a weak handle to the promise object itself (object base pointer == PromiseHeader ptr).
  //
  // Use the `*_h` variant so the weak-handle table lock can be acquired safely even under a moving
  // GC (it reloads the pointer from an addressable slot after lock acquisition).
  let mut promise_ptr = p.0.cast::<u8>();
  let weak = unsafe { runtime_native::rt_weak_add_h(&mut promise_ptr as *mut *mut u8) };
  let _weak_guard = WeakHandleGuard(weak);

  // Drop the last strong reference and force a GC while the timer is still pending.
  //
  // This ensures `rt_async_sleep` is not storing a raw GC pointer inside the timer queue: the
  // runtime must keep the promise alive (and relocatable) via a persistent root until the timer
  // callback runs.
  p = PromiseRef::null();
  assert!(p.is_null());
  runtime_native::rt_gc_collect();

  let pending_ptr = runtime_native::rt_weak_get(weak);
  assert!(
    !pending_ptr.is_null(),
    "sleep promise should stay alive while timer is pending"
  );

  // Drive the runtime until the timer callback fulfills the promise. Use the weak handle to locate
  // the current promise pointer each iteration (it may relocate across GC).
  let start = Instant::now();
  loop {
    unsafe {
      runtime_native::rt_async_run_until_idle_abi();
    }

    let ptr = runtime_native::rt_weak_get(weak);
    assert!(
      !ptr.is_null(),
      "sleep promise should not be collected before settlement"
    );
    let header = ptr.cast::<PromiseHeader>();
    let state = unsafe { &(*header).state }.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED {
      break;
    }
    assert_ne!(
      state,
      PromiseHeader::REJECTED,
      "rt_async_sleep promise unexpectedly rejected"
    );
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_sleep promise to fulfill"
    );
    std::thread::yield_now();
  }

  // After settlement, the timer task should have dropped its persistent root. A legacy
  // `Box<RtPromise>` allocation would not be collectible by the GC and would remain visible through
  // the weak handle.
  runtime_native::rt_gc_collect();
  assert!(
    runtime_native::rt_weak_get(weak).is_null(),
    "sleep promise should be collectible after settlement when unreferenced"
  );
}
