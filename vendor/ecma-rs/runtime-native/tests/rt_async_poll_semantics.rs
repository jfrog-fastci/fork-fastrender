use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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

fn drive_two_due_timers(poll: extern "C" fn() -> bool) -> (bool, usize, bool, usize) {
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
  runtime_native::test_util::schedule_timer(Duration::ZERO, inc, counter as *const AtomicUsize as *mut u8);
  runtime_native::test_util::schedule_timer(Duration::ZERO, inc, counter as *const AtomicUsize as *mut u8);

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
fn rt_async_poll_timer_pending_semantics() {
  let _rt = TestRuntimeGuard::new();

  let (first_pending, after_first, second_pending, after_second) =
    drive_two_due_timers(runtime_native::rt_async_poll);

  assert_eq!(after_first, 1, "first poll turn should execute exactly one due timer callback");
  assert!(
    first_pending,
    "first poll turn should report pending work (second due timer callback queued)"
  );

  assert_eq!(after_second, 2, "second poll turn should execute the second due timer callback");
  assert!(
    !second_pending,
    "second poll turn should report the runtime as fully idle after draining both due timers"
  );
}

#[test]
fn rt_async_poll_returns_true_when_timer_is_pending_after_turn() {
  let _rt = TestRuntimeGuard::new();

  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  // Queue a timer far in the future. If `rt_async_poll` were to block when work is pending but not
  // runnable, this would hang the test. Ensure there is immediately-runnable work by also enqueuing
  // a microtask.
  let timer = runtime_native::test_util::schedule_timer(
    Duration::from_secs(60),
    inc,
    counter as *const AtomicUsize as *mut u8,
  );
  runtime_native::test_util::enqueue_microtask(inc, counter as *const AtomicUsize as *mut u8);

  // The microtask should run, but the timer is still pending, so the runtime is not fully idle.
  assert!(runtime_native::rt_async_poll());
  assert_eq!(counter.load(Ordering::SeqCst), 1);

  // Cancel the timer and verify the runtime becomes idle.
  assert!(runtime_native::async_rt::global().cancel_timer(timer));
  assert!(!runtime_native::rt_async_poll());
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
