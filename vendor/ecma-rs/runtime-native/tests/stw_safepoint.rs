use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::threading::safepoint::{stop_the_world, StopReason, RT_GC_EPOCH};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

#[test]
fn stop_the_world_parks_active_workers_and_resumes() {
  let _rt = TestRuntimeGuard::new();
  const WORKERS: usize = 4;

  threading::register_current_thread(ThreadKind::Main);

  let start_barrier = Arc::new(Barrier::new(WORKERS + 1));
  let stop = Arc::new(AtomicBool::new(false));
  let counters: Vec<_> = (0..WORKERS).map(|_| Arc::new(AtomicU64::new(0))).collect();

  let mut handles = Vec::new();
  for i in 0..WORKERS {
    let start_barrier = start_barrier.clone();
    let stop = stop.clone();
    let counter = counters[i].clone();

    handles.push(std::thread::spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      start_barrier.wait();

      while !stop.load(Ordering::Acquire) {
        threading::safepoint_poll();
        counter.fetch_add(1, Ordering::Relaxed);
        std::hint::spin_loop();
      }

      threading::unregister_current_thread();
    }));
  }

  // Release workers.
  start_barrier.wait();

  // Ensure they are actually running before trying to stop them.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let total: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    if total >= 10_000 {
      break;
    }
    assert!(Instant::now() < deadline, "workers failed to start making progress");
    std::thread::yield_now();
  }

  stop_the_world(StopReason::Test, || {
    let stop_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    assert_eq!(stop_epoch & 1, 1, "expected odd epoch during stop-the-world");

    // All worker threads must have acknowledged the stop request (or be parked).
    for t in threading::all_threads() {
      if t.kind() != ThreadKind::Worker || t.is_parked() {
        continue;
      }
      assert_eq!(
        t.safepoint_epoch_observed(),
        stop_epoch,
        "worker did not publish stop epoch"
      );
    }

    // While stopped, mutators must not make progress.
    let before: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    std::thread::sleep(Duration::from_millis(25));
    let after: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    assert_eq!(before, after, "workers kept running while world was stopped");
  });

  // After resuming, workers should start making progress again.
  let before_total: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let total: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    if total >= before_total + 10_000 {
      break;
    }
    assert!(Instant::now() < deadline, "workers failed to resume making progress");
    std::thread::yield_now();
  }

  stop.store(true, Ordering::Release);
  for h in handles {
    h.join().unwrap();
  }

  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_does_not_wait_for_parked_threads() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let parked_ready = Arc::new(AtomicBool::new(false));
  let gate = Arc::new((Mutex::new(false), Condvar::new()));

  let parked_ready_worker = parked_ready.clone();
  let gate_worker = gate.clone();
  let handle = std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    threading::set_parked(true);
    parked_ready_worker.store(true, Ordering::Release);

    let (lock, cv) = &*gate_worker;
    let mut guard = lock.lock().unwrap();
    while !*guard {
      guard = cv.wait(guard).unwrap();
    }
    drop(guard);

    threading::set_parked(false);
    threading::unregister_current_thread();
  });

  let deadline = Instant::now() + Duration::from_secs(2);
  while !parked_ready.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "worker never reached parked state");
    std::thread::yield_now();
  }

  let start = Instant::now();
  stop_the_world(StopReason::Test, || {
    assert_eq!(
      RT_GC_EPOCH.load(Ordering::Acquire) & 1,
      1,
      "expected odd epoch during stop-the-world"
    );
  });
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "stop_the_world took too long; did it wait for parked threads?"
  );

  // Unblock worker and clean up.
  let (lock, cv) = &*gate;
  *lock.lock().unwrap() = true;
  cv.notify_one();
  handle.join().unwrap();

  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_is_not_reentrant() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let res = std::panic::catch_unwind(|| {
    stop_the_world(StopReason::Test, || {
      stop_the_world(StopReason::Test, || {});
    });
  });
  assert!(res.is_err(), "expected re-entrant stop_the_world to panic");

  // Even on panic, the coordinator must resume the world.
  assert_eq!(
    RT_GC_EPOCH.load(Ordering::Acquire) & 1,
    0,
    "expected even epoch after stop_the_world panic cleanup"
  );

  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_is_deterministic_under_stress() {
  let _rt = TestRuntimeGuard::new();
  const WORKERS: usize = 4;

  threading::register_current_thread(ThreadKind::Main);

  let start_barrier = Arc::new(Barrier::new(WORKERS + 1));
  let stop = Arc::new(AtomicBool::new(false));
  let mut handles = Vec::new();

  for _ in 0..WORKERS {
    let start_barrier = start_barrier.clone();
    let stop = stop.clone();
    handles.push(std::thread::spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      start_barrier.wait();
      while !stop.load(Ordering::Acquire) {
        threading::safepoint_poll();
        std::hint::spin_loop();
      }
      threading::unregister_current_thread();
    }));
  }

  start_barrier.wait();

  for _ in 0..1_000 {
    stop_the_world(StopReason::Test, || {
      let stop_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
      assert_eq!(stop_epoch & 1, 1, "expected odd epoch during stop-the-world");
      for t in threading::all_threads() {
        if t.kind() != ThreadKind::Worker || t.is_parked() {
          continue;
        }
        assert_eq!(t.safepoint_epoch_observed(), stop_epoch);
      }
    });
  }

  stop.store(true, Ordering::Release);
  for h in handles {
    h.join().unwrap();
  }

  threading::unregister_current_thread();
}
