use runtime_native::async_rt::Task;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

extern "C" fn noop(_data: *mut u8) {}

extern "C" fn set_atomic_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

#[test]
fn stop_the_world_does_not_wait_for_async_poll_thread_blocked_in_epoll() {
  let _rt = TestRuntimeGuard::new();

  threading::register_current_thread(ThreadKind::Main);

  // Keep the async runtime non-idle so `rt_async_poll` blocks in `epoll_wait`.
  let _dummy_timer = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(60),
    Task::new(noop, std::ptr::null_mut()),
  );

  let (tx_id, rx_id) = mpsc::channel();
  let poller = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Main);
    tx_id.send(id.get()).unwrap();
    let _ = runtime_native::rt_async_poll_legacy();
    threading::unregister_current_thread();
  });

  let poller_id = rx_id.recv().unwrap();

  // Wait until the poller thread is actually blocked in epoll_wait and has
  // entered a GC-safe/parked state.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let quiescent = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == poller_id)
      .map(|t| t.is_parked() || t.is_native_safe())
      .unwrap_or(false);
    if quiescent {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "poll thread did not enter a GC-safe state in time"
    );
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200)),
    "world did not stop while async poll thread was GC-safe"
  );
  runtime_native::rt_gc_resume_world();

  // Wake the epoll_wait by enqueueing a microtask. The poller thread will drain it and return.
  let ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  runtime_native::async_rt::global()
    .enqueue_microtask(Task::new(set_atomic_bool, ran as *const AtomicBool as *mut u8));

  let deadline = Instant::now() + Duration::from_secs(2);
  while !ran.load(Ordering::SeqCst) {
    assert!(Instant::now() < deadline, "microtask did not run");
    std::thread::yield_now();
  }

  poller.join().unwrap();
  threading::unregister_current_thread();
}
