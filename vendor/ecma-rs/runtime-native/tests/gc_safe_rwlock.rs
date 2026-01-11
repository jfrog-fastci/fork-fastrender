use runtime_native::sync::GcAwareRwLock;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

fn wait_until_native_safe(thread_id: u64) {
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let thread = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == thread_id)
      .expect("worker thread state");
    if thread.is_native_safe() {
      return;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "thread did not enter NativeSafe while blocked on lock"
    );
    std::thread::yield_now();
  }
}

fn stw_must_complete_while_thread_is_blocked() {
  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1));
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not reach safepoint in time while thread was blocked on lock"
  );
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_gc_aware_rwlock_read() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const ITERS: usize = 20;
  for _ in 0..ITERS {
    let lock = Arc::new(GcAwareRwLock::new(()));
    // Hold an exclusive lock so the worker must block in `read()`.
    let write_guard = lock.write();

    let (tx_id, rx_id) = mpsc::channel();
    let acquired = Arc::new(AtomicBool::new(false));

    let lock_worker = lock.clone();
    let acquired_worker = acquired.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      let _g = lock_worker.read();
      acquired_worker.store(true, Ordering::Release);
      drop(_g);

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    wait_until_native_safe(worker_id);

    stw_must_complete_while_thread_is_blocked();

    drop(write_guard);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !acquired.load(Ordering::Acquire) {
      assert!(
        std::time::Instant::now() < deadline,
        "worker did not acquire read lock after it was released"
      );
      std::thread::yield_now();
    }

    handle.join().unwrap();
  }

  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_gc_aware_rwlock_write() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const ITERS: usize = 20;
  for _ in 0..ITERS {
    let lock = Arc::new(GcAwareRwLock::new(()));
    // Hold a shared lock so the worker must block in `write()`.
    let read_guard = lock.read();

    let (tx_id, rx_id) = mpsc::channel();
    let acquired = Arc::new(AtomicBool::new(false));

    let lock_worker = lock.clone();
    let acquired_worker = acquired.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      let _g = lock_worker.write();
      acquired_worker.store(true, Ordering::Release);
      drop(_g);

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    wait_until_native_safe(worker_id);

    stw_must_complete_while_thread_is_blocked();

    drop(read_guard);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !acquired.load(Ordering::Acquire) {
      assert!(
        std::time::Instant::now() < deadline,
        "worker did not acquire write lock after it was released"
      );
      std::thread::yield_now();
    }

    handle.join().unwrap();
  }

  threading::unregister_current_thread();
}

