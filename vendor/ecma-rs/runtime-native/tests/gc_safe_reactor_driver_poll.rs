#![cfg(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::ReactorDriver;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

#[test]
fn stop_the_world_completes_while_threads_blocked_in_reactor_driver_poll() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let driver = ReactorDriver::new().expect("reactor driver must be supported on this platform");
  let driver_a = driver.clone();
  let driver_b = driver.clone();

  let (a_id_tx, a_id_rx) = mpsc::channel::<u64>();
  let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();

  // Thread A blocks inside `ReactorDriver::poll` holding the driver's internal poll mutex.
  let poller_a = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    a_id_tx.send(id.get()).unwrap();
    let _ = driver_a
      .poll(Some(Duration::from_secs(2)))
      .expect("poll must succeed");
    threading::unregister_current_thread();
  });

  let poller_a_id = a_id_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("poll thread A should register");

  // Wait until thread A marks itself parked while blocked in the OS reactor wait syscall.
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut a_is_parked = false;
  while Instant::now() < deadline {
    a_is_parked = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == poller_a_id)
      .map(|t| t.is_parked())
      .unwrap_or(false);
    if a_is_parked {
      break;
    }
    std::thread::yield_now();
  }

  // Thread B contends on the driver's poll mutex while A is blocked.
  let poller_b = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    b_id_tx.send(id.get()).unwrap();
    // This will block until thread A returns and releases the poll mutex.
    let _ = driver_b
      .poll(Some(Duration::ZERO))
      .expect("poll must succeed");
    threading::unregister_current_thread();
  });

  let poller_b_id = b_id_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("poll thread B should register");

  // Wait until thread B is blocked in the contended lock acquisition path (NativeSafe).
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut b_is_native_safe = false;
  while Instant::now() < deadline {
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

  // Stop-the-world should not wait for:
  // - thread A blocked in `poll()` (parked), or
  // - thread B blocked on the driver's internal mutex (NativeSafe while waiting).
  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1));
  runtime_native::rt_gc_resume_world();

  // Always wake thread A so it can return promptly (even if the STW assertion fails).
  driver.notify().expect("notify must succeed");

  poller_a.join().unwrap();
  poller_b.join().unwrap();

  threading::unregister_current_thread();

  assert!(
    a_is_parked,
    "poll thread A did not enter the parked state while blocked in ReactorDriver::poll"
  );
  assert!(
    b_is_native_safe,
    "poll thread B did not enter a GC-safe region while blocked on ReactorDriver::poll mutex"
  );
  assert!(
    stopped,
    "world did not stop while threads were blocked in ReactorDriver::poll"
  );
}
