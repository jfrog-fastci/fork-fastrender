use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::abi::RtThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

extern "C" {
  fn rt_thread_register(kind: RtThreadKind) -> u64;
  fn rt_thread_unregister();
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    // If the test panics mid-stop-the-world, ensure we don't leave the process
    // with a permanently stopped world and blocked worker threads.
    runtime_native::rt_gc_resume_world();
  }
}

fn unregister_cycle(baseline_threads: usize) {
  let worker_ready = Arc::new(AtomicBool::new(false));
  let unregister_started = Arc::new(AtomicBool::new(false));
  let unregister_returned = Arc::new(AtomicBool::new(false));

  let lock = Arc::new(Mutex::new(false));
  let cv = Arc::new(Condvar::new());

  let worker_ready_worker = worker_ready.clone();
  let unregister_started_worker = unregister_started.clone();
  let unregister_returned_worker = unregister_returned.clone();
  let lock_worker = lock.clone();
  let cv_worker = cv.clone();

  let handle = std::thread::spawn(move || {
    unsafe { rt_thread_register(RtThreadKind::RT_THREAD_WORKER) };

    // Enter a GC-safe region so the stop-the-world coordinator can treat this
    // thread as already quiescent while it blocks in the external condvar wait.
    let _gc_safe = threading::enter_gc_safe_region();

    worker_ready_worker.store(true, Ordering::Release);

    // Wait for the coordinator to signal an unregister attempt.
    let mut guard = lock_worker.lock().unwrap();
    while !*guard {
      guard = cv_worker.wait(guard).unwrap();
    }
    drop(guard);

    unregister_started_worker.store(true, Ordering::Release);
    unsafe { rt_thread_unregister() };
    unregister_returned_worker.store(true, Ordering::Release);
  });

  // Wait for the worker to register and enter the GC-safe region.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !worker_ready.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "worker did not enter GC-safe region in time"
    );
    std::thread::yield_now();
  }

  assert_eq!(
    threading::thread_counts().total,
    baseline_threads + 1,
    "expected worker to be registered"
  );

  // Stop-the-world.
  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2)),
    "world did not reach safepoint in time"
  );

  // While the world is stopped, signal the worker to unregister.
  {
    let mut guard = lock.lock().unwrap();
    *guard = true;
    cv.notify_all();
  }

  // Wait for the worker to begin calling `rt_thread_unregister()`.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !unregister_started.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "worker did not start unregister in time"
    );
    std::thread::yield_now();
  }

  // The worker must not successfully unregister/return while the world is
  // stopped.
  std::thread::sleep(Duration::from_millis(10));
  assert!(
    !unregister_returned.load(Ordering::Acquire),
    "worker returned from rt_thread_unregister while world was stopped"
  );
  assert_eq!(
    threading::thread_counts().total,
    baseline_threads + 1,
    "worker disappeared from registry while world was stopped"
  );

  // Resume the world; the worker should now be able to complete unregistration.
  runtime_native::rt_gc_resume_world();

  let deadline = Instant::now() + Duration::from_secs(2);
  while !unregister_returned.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "worker did not finish unregister after world resumed"
    );
    std::thread::yield_now();
  }

  handle.join().unwrap();

  assert_eq!(
    threading::thread_counts().total,
    baseline_threads,
    "registry thread count did not return to baseline"
  );
}

#[test]
fn thread_unregister_during_stw_blocks_until_resumed() {
  let _rt = TestRuntimeGuard::new();
  let _resume_guard = ResumeWorldOnDrop;

  // Register the test harness thread as the runtime "main" thread via the ABI.
  unsafe { rt_thread_register(RtThreadKind::RT_THREAD_MAIN) };
  let baseline_threads = threading::thread_counts().total;

  unregister_cycle(baseline_threads);

  unsafe { rt_thread_unregister() };
}

#[test]
fn thread_unregister_during_stw_stress() {
  let _rt = TestRuntimeGuard::new();
  let _resume_guard = ResumeWorldOnDrop;

  unsafe { rt_thread_register(RtThreadKind::RT_THREAD_MAIN) };
  let baseline_threads = threading::thread_counts().total;

  for _ in 0..100 {
    unregister_cycle(baseline_threads);
  }

  unsafe { rt_thread_unregister() };
}
