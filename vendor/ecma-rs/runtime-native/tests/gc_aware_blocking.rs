use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[test]
fn stw_completes_while_thread_blocked_on_gc_aware_mutex() {
  threading::register_current_thread(ThreadKind::Main);

  let lock = Arc::new(threading::GcAwareMutex::new(()));
  let held = lock.lock();

  let (tx_id, rx_id) = mpsc::channel();
  let acquired = Arc::new(AtomicBool::new(false));

  let handle = std::thread::spawn({
    let lock = lock.clone();
    let acquired = acquired.clone();
    move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      // This blocks until the main thread releases `held`. The GC-aware mutex
      // wrapper must mark this thread as GC-quiescent while waiting so stop-the-world
      // GC coordination doesn't hang.
      let _guard = lock.lock();
      acquired.store(true, Ordering::Release);

      threading::unregister_current_thread();
    }
  });

  let worker_id = rx_id.recv().unwrap();

  // Wait until the worker is definitely blocked on the lock (and therefore
  // treated as quiescent for stop-the-world coordination).
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let quiescent = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == worker_id)
      .is_some_and(|t| t.is_parked() || t.is_native_safe());
    if quiescent {
      break;
    }
    assert!(Instant::now() < deadline, "worker did not enter a GC-quiescent state");
    std::thread::yield_now();
  }

  // Stop-the-world should complete even though the worker isn't polling
  // safepoints (because it's parked inside the lock acquisition).
  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2));
  runtime_native::rt_gc_resume_world();
  assert!(stopped, "world did not stop while worker was blocked on a runtime lock");

  drop(held);

  let deadline = Instant::now() + Duration::from_secs(2);
  while !acquired.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "worker did not acquire the lock after resuming the world");
    std::thread::yield_now();
  }

  handle.join().unwrap();
  threading::unregister_current_thread();
}
