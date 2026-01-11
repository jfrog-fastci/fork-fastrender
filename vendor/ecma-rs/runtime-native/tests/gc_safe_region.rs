use runtime_native::sync::GcAwareMutex;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;

#[test]
fn stop_the_world_completes_while_thread_waits_on_gc_aware_mutex() {
  const ITERS: usize = 50;

  // Register the test harness thread as the runtime "main" thread.
  threading::register_current_thread(ThreadKind::Main);

  for _ in 0..ITERS {
    let m = Arc::new(GcAwareMutex::new(()));

    // Hold the mutex so the worker must contend.
    let main_guard = m.lock();

    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));
    let acquired = Arc::new(AtomicBool::new(false));

    let m_worker = m.clone();
    let started_worker = started.clone();
    let acquired_worker = acquired.clone();

    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      // Start contended acquisition.
      started_worker.wait();

      let _g = m_worker.lock();
      acquired_worker.store(true, Ordering::Release);
      drop(_g);

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    // Wait until the worker is blocked in the contended path (NativeSafe).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
      let worker = threading::all_threads()
        .into_iter()
        .find(|t| t.id().get() == worker_id)
        .expect("worker thread state");

      if worker.is_native_safe() {
        break;
      }
      assert!(
        std::time::Instant::now() < deadline,
        "worker did not enter NativeSafe while waiting for mutex"
      );
      std::thread::yield_now();
    }

    // Stop-the-world should *not* wait for the contended worker.
    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not reach safepoint in time while worker was blocked on mutex"
    );

    // Allow the worker to acquire the mutex and continue.
    drop(main_guard);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !acquired.load(Ordering::Acquire) {
      assert!(
        std::time::Instant::now() < deadline,
        "worker did not acquire mutex after it was released"
      );
      std::thread::yield_now();
    }

    handle.join().unwrap();
  }

  threading::unregister_current_thread();
}
