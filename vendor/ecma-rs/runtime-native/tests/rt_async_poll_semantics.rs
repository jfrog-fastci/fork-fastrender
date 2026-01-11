use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};

extern "C" fn inc(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
}

fn drive_two_macrotasks(poll: extern "C" fn() -> bool) -> (bool, usize, bool, usize) {
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
  runtime_native::async_rt::enqueue_macrotask(inc, counter as *const AtomicUsize as *mut u8);
  runtime_native::async_rt::enqueue_macrotask(inc, counter as *const AtomicUsize as *mut u8);

  let first_pending = poll();
  let after_first = counter.load(Ordering::SeqCst);

  let second_pending = poll();
  let after_second = counter.load(Ordering::SeqCst);

  (first_pending, after_first, second_pending, after_second)
}

#[test]
fn rt_async_poll_returns_false_when_idle() {
  let _rt = TestRuntimeGuard::new();
  assert!(!runtime_native::rt_async_poll());
  // Optional parity check: the legacy entrypoint is an alias with identical behavior.
  assert!(!runtime_native::rt_async_poll_legacy());
}

#[test]
fn rt_async_poll_macrotask_pending_semantics() {
  let _rt = TestRuntimeGuard::new();

  let (first_pending, after_first, second_pending, after_second) =
    drive_two_macrotasks(runtime_native::rt_async_poll);

  assert_eq!(after_first, 1, "first poll turn should execute exactly one macrotask");
  assert!(first_pending, "first poll turn should report pending work (second macrotask queued)");

  assert_eq!(after_second, 2, "second poll turn should execute the second macrotask");
  assert!(
    !second_pending,
    "second poll turn should report the runtime as fully idle after draining both macrotasks"
  );
}

#[test]
fn rt_async_poll_matches_legacy() {
  let stable = {
    let _rt = TestRuntimeGuard::new();
    drive_two_macrotasks(runtime_native::rt_async_poll)
  };

  let legacy = {
    let _rt = TestRuntimeGuard::new();
    drive_two_macrotasks(runtime_native::rt_async_poll_legacy)
  };

  assert_eq!(stable, legacy);
}

