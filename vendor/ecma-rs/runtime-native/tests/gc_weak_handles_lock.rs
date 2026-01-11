use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;

#[test]
fn stop_the_world_completes_while_thread_waits_on_global_weak_handles_lock() {
  let _rt = TestRuntimeGuard::new();

  // Register the test harness thread as the runtime "main" thread.
  threading::register_current_thread(ThreadKind::Main);

  let (tx_id, rx_id) = mpsc::channel();
  let ready = Arc::new(Barrier::new(2));
  let start = Arc::new(Barrier::new(2));
  let done = Arc::new(AtomicBool::new(false));

  let ready_worker = ready.clone();
  let start_worker = start.clone();
  let done_worker = done.clone();
  let worker = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    // Signal that we're registered and ready, then wait for the lock to be held.
    ready_worker.wait();
    start_worker.wait();

    // Any weak-handle operation will contend on the global lock.
    let _ = runtime_native::rt_weak_get(0);
    done_worker.store(true, Ordering::Release);

    threading::unregister_current_thread();
  });

  let worker_id = rx_id.recv().unwrap();
  ready.wait();

  // Hold the global weak-handle lock so the worker thread blocks in the contended path.
  let weak_lock = runtime_native::gc::weak::debug_hold_global_weak_handles_lock();

  // Start contended acquisition.
  start.wait();

  // Wait until the worker is blocked in the contended path (NativeSafe).
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let worker_state = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == worker_id)
      .expect("worker thread state");

    if worker_state.is_native_safe() {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "worker did not enter NativeSafe while waiting for global weak-handle lock"
    );
    std::thread::yield_now();
  }

  // Stop-the-world should not wait for the worker blocked on the weak-handle lock.
  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1));
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not reach safepoint in time while worker was blocked on global weak-handle lock"
  );

  // Allow the worker to proceed.
  drop(weak_lock);

  // Ensure the worker completes once the lock is released.
  let deadline = std::time::Instant::now() + Duration::from_secs(4);
  while !done.load(Ordering::Acquire) {
    assert!(
      std::time::Instant::now() < deadline,
      "worker did not complete after global weak-handle lock was released"
    );
    std::thread::yield_now();
  }

  worker.join().unwrap();

  threading::unregister_current_thread();
}
