use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;
use std::time::Instant;

extern "C" fn noop(_data: *mut u8) {}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_web_timers_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Hold the global `WEB_TIMERS` lock so the worker must contend when it calls
  // `rt_set_timeout`.
  let web_timers_lock = runtime_native::debug_hold_web_timers_lock();

  let (tx_id, rx_id) = mpsc::channel();
  let started = Arc::new(Barrier::new(2));
  let finished = Arc::new(AtomicBool::new(false));

  let started_worker = started.clone();
  let finished_worker = finished.clone();

  let handle = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    started_worker.wait();

    // This call should block on the contended `WEB_TIMERS` lock (and therefore
    // transition into a GC-safe region via `GcAwareMutex`).
    let timer_id = runtime_native::rt_set_timeout(noop, std::ptr::null_mut(), 0);
    runtime_native::rt_clear_timer(timer_id);

    finished_worker.store(true, Ordering::Release);
    threading::unregister_current_thread();
  });

  let worker_id = rx_id.recv().unwrap();
  started.wait();

  // Wait until the worker is blocked in the contended path (NativeSafe).
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut worker_is_native_safe = false;
  while Instant::now() < deadline {
    let worker = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == worker_id)
      .expect("worker thread state");
    if worker.is_native_safe() {
      worker_is_native_safe = true;
      break;
    }
    std::thread::yield_now();
  }

  // Stop-the-world must not wait for a thread blocked on the exported timer
  // registry lock.
  let mut world_stopped = false;
  if worker_is_native_safe {
    runtime_native::rt_gc_request_stop_the_world();
    world_stopped = {
      let _resume = ResumeWorldOnDrop;
      runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(100))
    };
  }

  // Resume mutator execution before releasing the lock so the worker can
  // continue once it acquires it.
  drop(web_timers_lock);

  // Ensure the blocked worker completes once the lock is released.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !finished.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "worker did not complete after releasing WEB_TIMERS lock"
    );
    std::thread::yield_now();
  }

  handle.join().unwrap();
  threading::unregister_current_thread();

  assert!(
    worker_is_native_safe,
    "worker did not enter NativeSafe while waiting for WEB_TIMERS lock"
  );
  assert!(
    world_stopped,
    "world did not reach safepoint in time while worker was blocked on WEB_TIMERS lock"
  );
}

