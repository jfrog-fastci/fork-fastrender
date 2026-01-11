use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_async_poll_legacy as rt_async_poll, rt_async_sleep, rt_promise_then_legacy as rt_promise_then};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

#[test]
fn async_sleep_resolves_promise() {
  let _rt = TestRuntimeGuard::new();

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = Box::into_raw(settled);

  let promise = rt_async_sleep(20);
  rt_promise_then(promise, set_bool, settled_ptr.cast::<u8>());

  let start = Instant::now();
  while !unsafe { &*settled_ptr }.load(Ordering::SeqCst) {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_sleep promise to settle"
    );
  }

  unsafe {
    drop(Box::from_raw(settled_ptr));
  }
}

