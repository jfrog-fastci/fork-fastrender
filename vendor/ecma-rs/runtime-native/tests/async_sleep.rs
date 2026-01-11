use runtime_native::abi::PromiseRef;
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

extern "C" {
  fn rt_async_sleep(delay_ms: u64) -> PromiseRef;
}

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
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

#[test]
fn async_sleep_fulfills_promise() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadInitGuard::new();

  let p = unsafe { rt_async_sleep(10) };
  assert!(!p.is_null(), "rt_async_sleep returned a null promise");

  let settled = AtomicBool::new(false);
  runtime_native::rt_promise_then_legacy(p, set_bool, (&settled as *const AtomicBool).cast::<u8>().cast_mut());

  // Drive the runtime until the timer callback settles the promise.
  let start = Instant::now();
  loop {
    unsafe {
      runtime_native::rt_async_run_until_idle_abi();
    }

    let header = p.0.cast::<PromiseHeader>();
    let state = unsafe { &(*header).state }.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED && settled.load(Ordering::SeqCst) {
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
    std::thread::sleep(Duration::from_millis(1));
  }
}
