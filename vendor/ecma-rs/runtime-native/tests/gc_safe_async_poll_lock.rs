use runtime_native::async_rt::Task;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

extern "C" fn noop(_data: *mut u8) {}

#[test]
fn stop_the_world_completes_while_thread_waits_on_async_poll_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Keep the async runtime non-idle so a poller thread blocks in `epoll_wait` (while holding the
  // global poll lock).
  let timer = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(60),
    Task::new(noop, std::ptr::null_mut()),
  );

  let (tx_poller, rx_poller) = mpsc::channel();
  let poller = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Main);
    tx_poller.send(id.get()).unwrap();
    let _ = runtime_native::rt_async_poll_legacy();
    threading::unregister_current_thread();
  });
  let poller_id = rx_poller.recv().unwrap();

  // Wait until the poller thread is actually blocked in `epoll_wait` and has marked itself parked.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let parked = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == poller_id)
      .map(|t| t.is_parked())
      .unwrap_or(false);
    if parked {
      break;
    }
    assert!(Instant::now() < deadline, "poller thread did not park in time");
    std::thread::yield_now();
  }

  let (tx_contender, rx_contender) = mpsc::channel();
  let contender = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Main);
    tx_contender.send(id.get()).unwrap();
    let _ = runtime_native::rt_async_poll_legacy();
    threading::unregister_current_thread();
  });
  let contender_id = rx_contender.recv().unwrap();

  // The contender blocks on the global async poll mutex. That lock acquisition must enter a GC-safe
  // region so stop-the-world does not wait for it.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let native_safe = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == contender_id)
      .map(|t| t.is_native_safe())
      .unwrap_or(false);
    if native_safe {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "contender thread did not enter NativeSafe while waiting on async poll lock"
    );
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while a thread was blocked on the async poll lock"
  );

  // Wake the poller so it releases the lock and both threads can return.
  assert!(
    runtime_native::async_rt::global().cancel_timer(timer),
    "expected timer to exist"
  );

  poller.join().unwrap();
  contender.join().unwrap();

  threading::unregister_current_thread();
}

