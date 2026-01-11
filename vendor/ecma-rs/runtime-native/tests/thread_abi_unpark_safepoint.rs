use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use runtime_native::abi::RtThreadKind;
use runtime_native::test_util::TestRuntimeGuard;

extern "C" {
  fn rt_thread_register(kind: RtThreadKind) -> u64;
  fn rt_thread_unregister();
  fn rt_thread_set_parked(parked: bool);
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    // If the test panics mid-stop-the-world, ensure we don't leave the process
    // with a permanently stopped world and blocked worker threads.
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn unparking_during_stw_blocks_at_safepoint_until_resumed() {
  let _rt = TestRuntimeGuard::new();
  let _resume_guard = ResumeWorldOnDrop;

  // Register the test harness thread as the runtime "main" thread via the stable ABI.
  unsafe { rt_thread_register(RtThreadKind::RT_THREAD_MAIN) };

  let parked = Arc::new(AtomicBool::new(false));
  let unpark_started = Arc::new(AtomicBool::new(false));
  let unpark_returned = Arc::new(AtomicBool::new(false));
  let mutator_work = Arc::new(AtomicUsize::new(0));

  let lock = Arc::new(Mutex::new(false));
  let cv = Arc::new(Condvar::new());

  let parked_worker = parked.clone();
  let unpark_started_worker = unpark_started.clone();
  let unpark_returned_worker = unpark_returned.clone();
  let mutator_work_worker = mutator_work.clone();
  let lock_worker = lock.clone();
  let cv_worker = cv.clone();

  let handle = std::thread::spawn(move || {
    unsafe { rt_thread_register(RtThreadKind::RT_THREAD_WORKER) };

    // Park and block on the condvar.
    unsafe { rt_thread_set_parked(true) };
    parked_worker.store(true, Ordering::Release);

    let mut guard = lock_worker.lock().unwrap();
    while !*guard {
      guard = cv_worker.wait(guard).unwrap();
    }
    drop(guard);

    // Attempt to unpark while a stop-the-world is in progress. The ABI contract
    // requires `rt_thread_set_parked(false)` to poll a safepoint before returning.
    unpark_started_worker.store(true, Ordering::Release);
    unsafe { rt_thread_set_parked(false) };
    unpark_returned_worker.store(true, Ordering::Release);

    // This must not run until after `rt_gc_resume_world()`.
    mutator_work_worker.fetch_add(1, Ordering::SeqCst);

    unsafe { rt_thread_unregister() };
  });

  // Wait until the worker has parked itself.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !parked.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "worker did not enter parked state in time"
    );
    std::thread::yield_now();
  }

  // Stop-the-world while the worker is parked. Because it's parked, the
  // coordinator should treat it as already quiescent.
  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2)),
    "world did not reach safepoint in time"
  );

  // Signal the worker to unpark while the world is still stopped.
  {
    let mut guard = lock.lock().unwrap();
    *guard = true;
    cv.notify_all();
  }

  // Wait for the worker to begin leaving the parked state.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !unpark_started.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "worker did not start unparking in time"
    );
    std::thread::yield_now();
  }

  // The worker must now block at a safepoint (slow path) because the world is
  // stopped. Wait until we observe it waiting.
  let deadline = Instant::now() + Duration::from_secs(2);
  while runtime_native::threading::safepoint::threads_waiting_at_safepoint() == 0 {
    assert!(
      Instant::now() < deadline,
      "worker did not block at safepoint after unparking"
    );
    std::thread::yield_now();
  }

  // While stopped, the worker must not run mutator work and must not return
  // from `rt_thread_set_parked(false)`.
  std::thread::sleep(Duration::from_millis(25));
  assert_eq!(
    mutator_work.load(Ordering::Acquire),
    0,
    "worker ran mutator work while world was stopped"
  );
  assert!(
    !unpark_returned.load(Ordering::Acquire),
    "worker returned from rt_thread_set_parked(false) while world was stopped"
  );

  // Resume the world; the worker should now return and do its work.
  runtime_native::rt_gc_resume_world();

  let deadline = Instant::now() + Duration::from_secs(2);
  while mutator_work.load(Ordering::Acquire) == 0 {
    assert!(
      Instant::now() < deadline,
      "worker did not resume mutator work after world resumed"
    );
    std::thread::yield_now();
  }

  handle.join().unwrap();
  unsafe { rt_thread_unregister() };
}
