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

#[test]
fn weak_add_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();

  // Register the test harness thread as the runtime "main" thread.
  threading::register_current_thread(ThreadKind::Main);

  // Pointers are treated as opaque addresses; they do not need to be dereferenceable in this test.
  let mut slot_value: *mut u8 = 0x1111usize as *mut u8;
  let new_value: *mut u8 = 0x2222usize as *mut u8;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = (&mut slot_value as *mut *mut u8) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  let handle = std::thread::scope(|scope| {
    // Thread A holds the global weak-handle lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to add a weak handle while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<u64>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<u64>();

    scope.spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      let weak_lock = runtime_native::gc::weak::debug_hold_global_weak_handles_lock();
      a_locked_tx.send(()).unwrap();
      a_release_rx.recv().unwrap();
      drop(weak_lock);
      threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the weak-handle lock");

    scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      c_registered_tx.send(id.get()).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` is a valid slot pointer.
      let handle = unsafe { runtime_native::rt_weak_add_h(slot_ptr) };
      c_done_tx.send(handle).unwrap();

      threading::unregister_current_thread();
    });

    let worker_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start contended acquisition.
    c_start_tx.send(()).unwrap();

    // Wait until the worker is blocked in the contended path (NativeSafe).
    let deadline = std::time::Instant::now() + TIMEOUT;
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
        "worker did not enter NativeSafe while waiting for weak-handle lock"
      );
      std::thread::yield_now();
    }

    // Update the slot while thread C is blocked. If `rt_weak_add_h` incorrectly read the slot
    // before acquiring the lock, it would still observe the old value.
    slot_value = new_value;

    // Release the lock so `global_weak_add_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("weak handle add should complete after lock is released")
  });

  // Unlike persistent `HandleId`, weak handles do not reserve 0 as a sentinel: the first allocated
  // weak handle may be `0` (index 0, generation 0).
  assert_eq!(runtime_native::rt_weak_get(handle), new_value);

  runtime_native::rt_weak_remove(handle);
  assert!(runtime_native::rt_weak_get(handle).is_null());

  threading::unregister_current_thread();
}
