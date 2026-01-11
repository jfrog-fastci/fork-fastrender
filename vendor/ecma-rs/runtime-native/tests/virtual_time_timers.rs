use runtime_native::clock::VirtualClock;
use runtime_native::abi::Microtask;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

extern "C" fn notify_tx(data: *mut u8) {
  // Safety: `data` is a stable pointer to a leaked `mpsc::Sender<()>` owned by the test.
  let tx = unsafe { &*(data as *const mpsc::Sender<()>) };
  let _ = tx.send(());
}

extern "C" fn noop(_data: *mut u8) {}

#[test]
fn virtual_time_allows_long_timeouts_without_wall_clock_waiting() {
  let _rt = TestRuntimeGuard::new();

  let clock = Arc::new(VirtualClock::new());
  runtime_native::async_rt::set_clock_for_tests(clock.clone());

  let (tx, rx) = mpsc::channel::<()>();
  let tx = Box::leak(Box::new(tx));

  // Schedule a long timeout (30s), then advance virtual time so it becomes immediately due.
  let id = runtime_native::rt_set_timeout(notify_tx, tx as *const _ as *mut u8, 30_000);
  clock.advance(Duration::from_secs(30));

  // Drive the runtime on another thread and bound the wall-clock time the test can block for.
  let stop = Arc::new(AtomicBool::new(false));
  let stop_poll = stop.clone();
  let poll_thread = std::thread::spawn(move || {
    while !stop_poll.load(Ordering::Acquire) {
      runtime_native::rt_async_poll_legacy();
    }
  });

  let fired = rx.recv_timeout(Duration::from_millis(250)).is_ok();

  stop.store(true, Ordering::Release);
  // Ensure any poll thread blocked in `epoll_wait` wakes so it can observe `stop`.
  unsafe {
    runtime_native::rt_queue_microtask(Microtask {
      func: noop,
      data: core::ptr::null_mut(),
      drop: None,
    });
  }
  poll_thread.join().unwrap();

  // Always clean up (even on failure) so the process-global runtime doesn't keep stray timers.
  runtime_native::rt_clear_timer(id);

  assert!(fired, "virtual timeout did not fire quickly; runtime likely waited on wall-clock time");
}
