use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct HoldPollLockGuard;

impl Drop for HoldPollLockGuard {
  fn drop(&mut self) {
    runtime_native::async_rt::debug_set_hold_poll_lock(false);
  }
}

struct ResumeWorldGuard;

impl Drop for ResumeWorldGuard {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn stop_the_world_does_not_wait_for_async_poll_thread_blocked_on_poll_lock() {
  let _rt = TestRuntimeGuard::new();
  let _resume_world = ResumeWorldGuard;

  runtime_native::async_rt::debug_set_hold_poll_lock(true);
  let _hold_poll_lock = HoldPollLockGuard;

  let thread_a = std::thread::spawn(|| {
    threading::register_current_thread(ThreadKind::Main);
    let _ = runtime_native::rt_async_poll_legacy();
    threading::unregister_current_thread();
  });

  // Wait until thread A is holding the global serialization lock.
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut poll_lock_held = false;
  while Instant::now() < deadline {
    if runtime_native::async_rt::debug_poll_lock_is_held() {
      poll_lock_held = true;
      break;
    }
    std::thread::yield_now();
  }

  let (tx_id, rx_id) = mpsc::channel();
  let thread_b = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Main);
    let _ = tx_id.send(id.get());
    let _ = runtime_native::rt_async_poll_legacy();
    threading::unregister_current_thread();
  });

  let poller_b_id = rx_id.recv_timeout(Duration::from_secs(1)).ok();

  // Wait until thread B enters a GC-safe region while contending on the poll lock.
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut b_is_native_safe = false;
  while Instant::now() < deadline {
    let Some(poller_b_id) = poller_b_id else {
      break;
    };

    b_is_native_safe = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == poller_b_id)
      .map(|t| t.is_native_safe())
      .unwrap_or(false);
    if b_is_native_safe {
      break;
    }
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));

  // Always resume + release the held lock so the test can't hang even on failure.
  runtime_native::rt_gc_resume_world();
  runtime_native::async_rt::debug_set_hold_poll_lock(false);

  thread_a.join().unwrap();
  thread_b.join().unwrap();

  assert!(poll_lock_held, "thread A did not acquire the async poll lock in time");
  assert!(
    b_is_native_safe,
    "thread B did not enter a GC-safe region while blocked on the async poll lock"
  );
  assert!(
    stopped,
    "world did not stop while a thread was blocked on the async poll lock"
  );
}
