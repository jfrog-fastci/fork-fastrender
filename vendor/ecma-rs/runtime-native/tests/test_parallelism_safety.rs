use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use runtime_native::test_util::TestRuntimeGuard;

extern "C" fn set_flag(data: *mut u8) {
  // Safety: the test passes a valid `*const AtomicBool` which stays alive for the test duration.
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

extern "C" fn enqueue_microtask_set_flag(data: *mut u8) {
  runtime_native::test_util::enqueue_microtask(set_flag, data);
}

#[test]
fn rt_async_poll_is_serialized_and_deadlock_free() {
  let _rt = TestRuntimeGuard::new();

  let done = Arc::new(AtomicBool::new(false));
  let done_ptr = Arc::as_ptr(&done) as *mut u8;

  // Schedule a timer that, when it fires, schedules a microtask to flip `done`.
  // This exercises:
  // - timer queue
  // - microtask queue
  // - cross-thread wakeups
  let _timer = runtime_native::test_util::schedule_timer(Duration::from_millis(5), enqueue_microtask_set_flag, done_ptr);

  let start = Instant::now();
  let deadline = start + Duration::from_secs(2);

  let t1_done = done.clone();
  let t2_done = done.clone();

  let t1 = thread::spawn(move || {
    while !t1_done.load(Ordering::SeqCst) {
      runtime_native::rt_async_poll();
      if Instant::now() > deadline {
        panic!("timeout waiting for rt_async_poll to make progress (thread 1)");
      }
    }
  });

  let t2 = thread::spawn(move || {
    while !t2_done.load(Ordering::SeqCst) {
      runtime_native::rt_async_poll();
      if Instant::now() > deadline {
        panic!("timeout waiting for rt_async_poll to make progress (thread 2)");
      }
    }
  });

  t1.join().expect("thread 1 panicked");
  t2.join().expect("thread 2 panicked");

  assert!(done.load(Ordering::SeqCst));
}
