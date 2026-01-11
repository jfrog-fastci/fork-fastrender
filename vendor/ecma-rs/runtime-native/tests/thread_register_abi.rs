use runtime_native::abi::RtThreadKind::*;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    // If the test panics mid-stop-the-world, ensure we don't leave the process
    // with a permanently stopped world and blocked threads.
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn safepoint_barrier_stops_threads_registered_via_c_abi() {
  let _rt = TestRuntimeGuard::new();

  // Register the test harness thread as the runtime "main" thread via the stable C ABI.
  let stop = Arc::new(AtomicBool::new(false));
  let work_counter = Arc::new(AtomicU64::new(0));

  let main_was_registered = runtime_native::threading::registry::current_thread_id().is_some();
  let main_id = runtime_native::rt_thread_register(RT_THREAD_MAIN);
  assert_ne!(main_id, 0);

  let worker_stop = stop.clone();
  let worker_counter = work_counter.clone();

  let handle = std::thread::spawn(move || {
    let worker_id = runtime_native::rt_thread_register(RT_THREAD_WORKER);
    assert_ne!(worker_id, 0);

    while !worker_stop.load(Ordering::Acquire) {
      runtime_native::rt_gc_safepoint();
      worker_counter.fetch_add(1, Ordering::Relaxed);
      std::hint::spin_loop();
    }

    runtime_native::rt_thread_unregister();
  });

  struct WorkerJoinOnDrop {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
  }

  impl Drop for WorkerJoinOnDrop {
    fn drop(&mut self) {
      self.stop.store(true, Ordering::Release);
      if let Some(handle) = self.handle.take() {
        let _ = handle.join();
      }
    }
  }

  // Ensure we always resume the world before joining the worker on panic.
  let mut worker = WorkerJoinOnDrop {
    stop: stop.clone(),
    handle: Some(handle),
  };
  let _resume = ResumeWorldOnDrop;

  // Wait until the worker has made progress so we can later assert it stops.
  let progress_target = 10_000;
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    if work_counter.load(Ordering::Relaxed) >= progress_target {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "worker failed to start making progress"
    );
    std::thread::yield_now();
  }

  // Stop-the-world.
  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2)),
    "world did not reach safepoint in time"
  );

  // Once stopped, the worker must not keep running.
  let before = work_counter.load(Ordering::Relaxed);
  std::thread::sleep(Duration::from_millis(50));
  let after = work_counter.load(Ordering::Relaxed);
  assert_eq!(before, after, "worker kept running while world was stopped");

  // Resume.
  runtime_native::rt_gc_resume_world();

  // Ensure the worker continues to make progress after resuming.
  let resume_deadline = Instant::now() + Duration::from_secs(2);
  loop {
    if work_counter.load(Ordering::Relaxed) > after {
      break;
    }
    assert!(
      Instant::now() < resume_deadline,
      "worker failed to resume making progress"
    );
    std::thread::yield_now();
  }

  stop.store(true, Ordering::Release);
  worker.handle.take().unwrap().join().unwrap();

  if !main_was_registered {
    runtime_native::rt_thread_unregister();
  }
}
