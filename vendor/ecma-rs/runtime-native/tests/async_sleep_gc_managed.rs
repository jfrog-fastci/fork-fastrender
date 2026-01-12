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

  // Drive the runtime until the timer callback fulfills the promise.
  let start = Instant::now();
  loop {
    unsafe {
      runtime_native::rt_async_run_until_idle_abi();
    }

    let header = p.0.cast::<PromiseHeader>();
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

  // Create a weak handle to the promise object itself (object base pointer == PromiseHeader ptr).
  let weak = runtime_native::rt_weak_add(p.0.cast::<u8>());
  let _weak_guard = WeakHandleGuard(weak);

  // Drop the last strong reference and force a GC. A legacy `Box<RtPromise>` allocation would not
  // be collectible by the GC and would remain visible through the weak handle.
  p = PromiseRef::null();
  assert!(p.is_null());
  runtime_native::rt_gc_collect();

  assert!(
    runtime_native::rt_weak_get(weak).is_null(),
    "sleep promise should be collectible after settlement when unreferenced"
  );
}
