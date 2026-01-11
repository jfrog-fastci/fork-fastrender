use runtime_native::current_thread;
use runtime_native::threading;
use runtime_native::Runtime;
use runtime_native::threading::safepoint::StopReason;
use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn thread_attach_detach_registry_and_tls() {
  let _rt = runtime_native::test_util::TestRuntimeGuard::new();
  let n = 8usize;
  let runtime = Arc::new(Runtime::new());

  assert_eq!(runtime.thread_count(), 0);
  let baseline = threading::thread_counts();

  let attached = Arc::new(Barrier::new(n + 1));
  let detach = Arc::new(Barrier::new(n + 1));

  let mut handles = Vec::with_capacity(n);
  for _ in 0..n {
    let runtime = runtime.clone();
    let attached = attached.clone();
    let detach = detach.clone();
    handles.push(thread::spawn(move || {
      let guard = runtime.attach_current_thread().expect("attach failed");

      let thread = current_thread().expect("TLS should be set after attach");
      assert_eq!(thread.id, guard.thread().id);

      assert!(
        threading::registry::current_thread_id().is_some(),
        "attached thread must be registered in the global thread registry"
      );

      // Double attach is rejected.
      assert!(runtime.attach_current_thread().is_err());

      attached.wait();
      detach.wait();

      drop(guard);
      assert!(current_thread().is_none());

      thread.id
    }));
  }

  // Wait until all threads have attached.
  attached.wait();
  assert_eq!(runtime.thread_count(), n);

  let counts = threading::thread_counts();
  assert_eq!(counts.total, baseline.total + n);
  assert_eq!(counts.external, baseline.external + n);

  // Allow threads to detach.
  detach.wait();

  let mut ids = Vec::with_capacity(n);
  for handle in handles {
    ids.push(handle.join().expect("thread panicked"));
  }

  assert_eq!(runtime.thread_count(), 0);

  let uniq: HashSet<u32> = ids.into_iter().collect();
  assert_eq!(uniq.len(), n);

  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let counts = threading::thread_counts();
    if counts.total == baseline.total {
      assert_eq!(counts.main, baseline.main);
      assert_eq!(counts.worker, baseline.worker);
      assert_eq!(counts.io, baseline.io);
      assert_eq!(counts.external, baseline.external);
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "global thread registry did not return to baseline after detach"
    );
    std::thread::yield_now();
  }
}

#[test]
fn stop_the_world_blocks_attach() {
  let _rt = runtime_native::test_util::TestRuntimeGuard::new();
  let runtime = Arc::new(Runtime::new());

  let guard = runtime.attach_current_thread().expect("attach main");
  let main_id = guard.thread().id;

  // Hold an STW guard while another thread tries to attach.
  let stw = runtime.stop_the_world_guard();

  let (tx_started, rx_started) = mpsc::channel();
  let (tx_attached, rx_attached) = mpsc::channel();

  let runtime2 = runtime.clone();
  let handle = thread::spawn(move || {
    tx_started.send(()).unwrap();
    let g = runtime2.attach_current_thread().expect("attach worker");
    let id = current_thread().expect("TLS should be set").id;
    tx_attached.send(id).unwrap();
    drop(g);
  });

  // Ensure the worker thread is running and attempting to attach.
  rx_started.recv().unwrap();

  // Attach should not complete while STW is held.
  assert!(rx_attached.recv_timeout(Duration::from_millis(100)).is_err());
  assert_eq!(runtime.thread_count(), 1);

  drop(stw);

  let worker_id = rx_attached
    .recv_timeout(Duration::from_secs(2))
    .expect("worker should attach after STW released");
  assert_ne!(worker_id, main_id);

  handle.join().unwrap();

  drop(guard);
  assert_eq!(runtime.thread_count(), 0);
}

#[test]
fn global_stop_the_world_does_not_deadlock_thread_detach() {
  let _rt = runtime_native::test_util::TestRuntimeGuard::new();
  let runtime = Arc::new(Runtime::new());

  // Register the main thread as a GC mutator so the global stop-the-world
  // coordinator will wait for it.
  runtime_native::rt_thread_init(0);

  let guard = runtime.attach_current_thread().expect("attach main");

  let (tx_epoch, rx_epoch) = mpsc::channel();
  let (tx_go, rx_go) = mpsc::channel();
  let (tx_stopped, rx_stopped) = mpsc::channel();

  let handle = thread::spawn(move || {
    // Register the coordinator thread so `rt_gc_wait_for_world_stopped_timeout`
    // will skip it when checking for quiescence.
    runtime_native::rt_thread_init(3);

    let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
    tx_epoch.send(stop_epoch).unwrap();

    // Wait until the main thread is about to detach (and therefore will enter a
    // safepoint if needed).
    rx_go.recv().unwrap();

    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2));
    runtime_native::rt_gc_resume_world();
    runtime_native::rt_thread_deinit();
    tx_stopped.send(stopped).unwrap();
  });

  // Wait for the stop-the-world request to be active.
  let _stop_epoch = rx_epoch.recv().unwrap();
  tx_go.send(()).unwrap();

  // Dropping the guard must not deadlock even if a global STW is active. The
  // detach path should cooperate by entering the safepoint instead of blocking
  // blindly.
  drop(guard);

  runtime_native::rt_thread_deinit();

  let stopped = rx_stopped
    .recv_timeout(Duration::from_secs(3))
    .expect("coordinator thread should report STW result");
  assert!(
    stopped,
    "world did not reach safepoint in time; thread detach likely blocked without acknowledging STW"
  );

  handle.join().unwrap();
  assert_eq!(runtime.thread_count(), 0);
}

#[test]
fn runtime_attach_registers_global_thread_registry() {
  let _rt = runtime_native::test_util::TestRuntimeGuard::new();
  let runtime = Runtime::new();

  assert!(
    threading::registry::current_thread_state().is_none(),
    "test thread should start out unregistered"
  );

  let guard = runtime.attach_current_thread().expect("attach");
  let state = threading::registry::current_thread_state().expect("attach should register global thread");
  assert_eq!(state.kind(), threading::ThreadKind::External);

  drop(guard);
  assert!(
    threading::registry::current_thread_state().is_none(),
    "detach should unregister global thread"
  );
}

#[test]
fn runtime_stop_the_world_stops_attached_workers() {
  let _rt = runtime_native::test_util::TestRuntimeGuard::new();
  let runtime = Arc::new(Runtime::new());

  let main_guard = runtime.attach_current_thread().expect("attach main");

  let start = Arc::new(Barrier::new(2));
  let stop = Arc::new(AtomicBool::new(false));
  let (tx_id, rx_id) = mpsc::channel::<u64>();

  let runtime2 = runtime.clone();
  let start2 = start.clone();
  let stop2 = stop.clone();
  let handle = thread::spawn(move || {
    let guard = runtime2.attach_current_thread().expect("attach worker");
    let id = threading::registry::current_thread_id()
      .expect("worker should be registered")
      .get();
    tx_id.send(id).unwrap();

    start2.wait();

    while !stop2.load(Ordering::Acquire) {
      runtime_native::rt_gc_safepoint();
      std::hint::spin_loop();
    }

    drop(guard);
  });

  let worker_id = rx_id.recv_timeout(Duration::from_secs(2)).expect("worker id");
  start.wait();

  runtime.stop_the_world(StopReason::Test, || {
    let stop_epoch = runtime_native::threading::safepoint::RT_GC_EPOCH.load(Ordering::Acquire);
    assert_eq!(stop_epoch & 1, 1, "expected odd epoch during stop_the_world");

    let mut observed = None;
    threading::registry::for_each_thread(|thread| {
      if thread.id().get() == worker_id {
        observed = Some(thread.safepoint_epoch_observed());
      }
    });
    assert_eq!(
      observed,
      Some(stop_epoch),
      "worker did not publish stop epoch while world was stopped"
    );
  });

  stop.store(true, Ordering::Release);
  handle.join().unwrap();
  drop(main_guard);
}
