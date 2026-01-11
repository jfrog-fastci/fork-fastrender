use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use std::time::Instant;

#[test]
fn unregister_waits_for_gc_resume() {
  threading::register_current_thread(ThreadKind::Main);

  let (tx_id, rx_id) = mpsc::channel::<u64>();
  let (tx_go, rx_go) = mpsc::channel::<()>();
  let (tx_started, rx_started) = mpsc::channel::<()>();
  let (tx_done, rx_done) = mpsc::channel::<()>();

  let handle = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    rx_go.recv().unwrap();
    tx_started.send(()).unwrap();

    // `unregister_current_thread` must not complete while a stop-the-world request is pending.
    threading::unregister_current_thread();
    tx_done.send(()).unwrap();
  });

  let worker_id = rx_id
    .recv_timeout(Duration::from_secs(2))
    .expect("worker did not register in time");

  // Request a stop-the-world, but intentionally do not resume yet.
  runtime_native::rt_gc_request_stop_the_world();

  tx_go.send(()).unwrap();
  rx_started
    .recv_timeout(Duration::from_secs(2))
    .expect("worker did not begin unregister in time");

  // Wait until either:
  // - the worker completed unregister early (bug), or
  // - the worker is blocked at the GC safepoint (expected).
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut completed_early = false;
  let mut reached_safepoint = false;
  while Instant::now() < deadline {
    match rx_done.try_recv() {
      Ok(()) => {
        completed_early = true;
        break;
      }
      Err(TryRecvError::Empty) => {}
      Err(TryRecvError::Disconnected) => break,
    }

    if runtime_native::threading::safepoint::threads_waiting_at_safepoint() >= 1 {
      reached_safepoint = true;
      break;
    }

    std::thread::yield_now();
  }

  let still_registered = threading::all_threads()
    .into_iter()
    .any(|t| t.id().get() == worker_id);

  // Resume the world so the worker can finish teardown and unregister.
  runtime_native::rt_gc_resume_world();

  let completed_after_resume = if completed_early {
    true
  } else {
    rx_done.recv_timeout(Duration::from_secs(2)).is_ok()
  };

  if completed_after_resume {
    let _ = handle.join();
  }
  threading::unregister_current_thread();

  assert!(
    reached_safepoint || completed_early,
    "worker neither reached safepoint nor completed unregister (test timeout)"
  );
  assert!(
    completed_after_resume,
    "worker did not finish unregister after the world was resumed"
  );
  assert!(
    !completed_early,
    "worker completed unregister while stop-the-world was active"
  );
  assert!(
    still_registered,
    "worker disappeared from the thread registry while stop-the-world was active"
  );
}

