use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::Runtime;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

extern "C" {
  fn rt_gc_safepoint_slow(epoch: u64);
}

#[inline(never)]
fn enter_safepoint_slow(epoch: u64) {
  // Safety: `rt_gc_safepoint_slow` is a runtime-native entrypoint that follows the C ABI.
  unsafe {
    rt_gc_safepoint_slow(epoch);
  }
}

#[test]
fn safepoint_stop_the_world_waits_for_rt_thread_attach() {
  let baseline = threading::thread_counts();
  threading::register_current_thread(ThreadKind::Main);

  let runtime = Arc::new(Runtime::new());
  let runtime_for_thread = runtime.clone();

  let (tx_ready, rx_ready) = mpsc::channel::<()>();
  let (tx_epoch, rx_epoch) = mpsc::channel::<u64>();
  let (tx_resumed, rx_resumed) = mpsc::channel::<()>();

  let handle = std::thread::spawn(move || unsafe {
    let thread = runtime_native::rt_thread_attach(Arc::as_ptr(&runtime_for_thread).cast_mut());
    assert!(!thread.is_null(), "rt_thread_attach returned null");
    tx_ready.send(()).unwrap();

    let epoch = rx_epoch.recv().unwrap();
    enter_safepoint_slow(epoch);
    tx_resumed.send(()).unwrap();

    runtime_native::rt_thread_detach(thread);
  });

  rx_ready
    .recv_timeout(Duration::from_secs(2))
    .expect("worker did not attach in time");

  // Request stop-the-world. The attached thread is registered in the global thread registry, so
  // the coordinator must wait for it to hit a safepoint.
  let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
  struct ResumeOnDrop;
  impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
      runtime_native::rt_gc_resume_world();
    }
  }
  let _resume = ResumeOnDrop;

  assert!(
    !runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(50)),
    "world stopped before the attached thread entered a safepoint"
  );

  tx_epoch.send(stop_epoch).unwrap();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2)),
    "world did not stop in time"
  );

  runtime_native::rt_gc_resume_world();
  rx_resumed
    .recv_timeout(Duration::from_secs(2))
    .expect("worker did not resume after rt_gc_resume_world");

  handle.join().unwrap();
  threading::unregister_current_thread();

  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let counts = threading::thread_counts();
    if counts.total == baseline.total {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "thread registry did not return to baseline after detach"
    );
    std::thread::yield_now();
  }
}
