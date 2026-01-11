use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;

#[test]
fn safepoint_barrier_stops_and_resumes_workers() {
  let _rt = TestRuntimeGuard::new();
  const WORKERS: usize = 4;

  // Register the test harness thread as the runtime "main" thread.
  threading::register_current_thread(ThreadKind::Main);

  let start_barrier = Arc::new(Barrier::new(WORKERS + 1));
  let stop = Arc::new(AtomicBool::new(false));

  // Half active, half idle (parked) at the time of the GC request.
  let active_flags: Vec<_> = (0..WORKERS).map(|i| Arc::new(AtomicBool::new(i < WORKERS / 2))).collect();
  let work_counters: Vec<_> = (0..WORKERS).map(|_| Arc::new(AtomicU64::new(0))).collect();

  let park_lock = Arc::new(Mutex::new(()));
  let park_cv = Arc::new(Condvar::new());

  let mut handles = Vec::new();
  for i in 0..WORKERS {
    let start_barrier = start_barrier.clone();
    let stop = stop.clone();
    let active = active_flags[i].clone();
    let counter = work_counters[i].clone();
    let park_lock = park_lock.clone();
    let park_cv = park_cv.clone();

    handles.push(std::thread::spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);

      start_barrier.wait();

      while !stop.load(Ordering::Acquire) {
        threading::safepoint_poll();

        if active.load(Ordering::Acquire) {
          // Busy loop.
          counter.fetch_add(1, Ordering::Relaxed);
          std::hint::spin_loop();
          continue;
        }

        // Park until signalled.
        threading::set_parked(true);
        let mut guard = park_lock.lock().unwrap();
        while !active.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
          guard = park_cv.wait(guard).unwrap();
        }
        drop(guard);
        threading::set_parked(false);
      }

      threading::unregister_current_thread();
    }));
  }

  // Let workers start.
  start_barrier.wait();

  // Wait until the active workers have made progress so we can later assert they stop.
  let active_progress_target = 10_000;
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let total: u64 = work_counters.iter().take(WORKERS / 2).map(|c| c.load(Ordering::Relaxed)).sum();
    if total >= active_progress_target {
      break;
    }
    assert!(std::time::Instant::now() < deadline, "workers failed to start making progress");
    std::thread::yield_now();
  }

  // Stop-the-world.
  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2)),
    "world did not reach safepoint in time"
  );

  // Once stopped, the active workers must not keep running.
  let before: Vec<u64> = work_counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
  std::thread::sleep(Duration::from_millis(50));
  let after: Vec<u64> = work_counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
  assert_eq!(before, after, "workers kept running while world was stopped");

  // Resume.
  runtime_native::rt_gc_resume_world();

  // Unpark the idle workers so they can do work too.
  for active in &active_flags {
    active.store(true, Ordering::Release);
  }
  park_cv.notify_all();

  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let total: u64 = work_counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    if total >= active_progress_target * 2 {
      break;
    }
    assert!(std::time::Instant::now() < deadline, "workers failed to resume making progress");
    std::thread::yield_now();
  }

  stop.store(true, Ordering::Release);
  park_cv.notify_all();
  for h in handles {
    h.join().unwrap();
  }

  threading::unregister_current_thread();
}
