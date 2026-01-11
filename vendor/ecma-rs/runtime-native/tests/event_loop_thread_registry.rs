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

#[cfg(target_os = "macos")]
extern "C" {
  fn pthread_threadid_np(thread: libc::pthread_t, thread_id: *mut u64) -> libc::c_int;
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn fallback_thread_id_hash() -> u64 {
  // `ThreadId` formatting is intentionally opaque, so we hash its Debug form.
  use std::hash::Hash;
  use std::hash::Hasher;
  let tid = std::thread::current().id();
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  tid.hash(&mut hasher);
  hasher.finish()
}

fn current_os_thread_id() -> u64 {
  #[cfg(any(target_os = "linux", target_os = "android"))]
  unsafe {
    libc::syscall(libc::SYS_gettid) as u64
  }

  #[cfg(target_os = "macos")]
  unsafe {
    let mut tid: u64 = 0;
    let rc = pthread_threadid_np(libc::pthread_self(), &mut tid as *mut u64);
    if rc == 0 {
      return tid;
    }
    fallback_thread_id_hash()
  }

  #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
  {
    // Fallback: stable but not OS-level.
    fallback_thread_id_hash()
  }
}

#[test]
fn event_loop_registers_main_thread_and_participates_in_stop_the_world() {
  let _rt = TestRuntimeGuard::new();

  let counts_before = threading::thread_counts();
  assert_eq!(
    counts_before.main, 0,
    "expected no main thread to be registered before rt_async_poll (counts={counts_before:?})"
  );

  // Keep the runtime non-idle so `rt_async_poll` will block in `epoll_wait`.
  let dummy_timer = runtime_native::async_rt::global().schedule_timer_in(
    Duration::from_secs(1),
    Task::new(noop, std::ptr::null_mut()),
  );

  let microtask_ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  let (poll_tid_tx, poll_tid_rx) = mpsc::channel::<u64>();
  let (poll_returned_tx, poll_returned_rx) = mpsc::channel::<()>();
  let (poll_exit_tx, poll_exit_rx) = mpsc::channel::<()>();

  let poller = std::thread::spawn(move || {
    poll_tid_tx.send(current_os_thread_id()).unwrap();
    // This call should register the event-loop thread as `ThreadKind::Main`.
    let _pending = runtime_native::rt_async_poll();
    poll_returned_tx.send(()).unwrap();
    poll_exit_rx.recv().unwrap();
    threading::unregister_current_thread();
  });

  let poller_os_tid = poll_tid_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  // Wait until the poll thread has registered itself as `Main`.
  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    let threads = threading::all_threads();
    let poller_kind = threads
      .iter()
      .find(|t| t.os_thread_id() == poller_os_tid)
      .map(|t| t.kind());

    if poller_kind == Some(ThreadKind::Main) {
      break;
    }

    assert!(
      Instant::now() < deadline,
      "timed out waiting for rt_async_poll to register the event-loop thread (threads={threads:?})"
    );
    std::thread::yield_now();
  }

  let counts_after = threading::thread_counts();
  assert_eq!(
    counts_after.main, 1,
    "rt_async_poll should register exactly one main thread (counts={counts_after:?})"
  );

  // Give the poller a moment to enter `epoll_wait` and mark itself parked.
  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    let threads = threading::all_threads();
    let poller_is_parked = threads
      .iter()
      .find(|t| t.os_thread_id() == poller_os_tid)
      .map(|t| t.is_parked())
      .unwrap_or(false);
    if poller_is_parked {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "timed out waiting for event-loop thread to enter epoll_wait (threads={threads:?})"
    );
    std::thread::yield_now();
  }

  // Request stop-the-world from another thread. The poller is blocked in
  // `epoll_wait` and marked as parked, so the coordinator should consider it
  // quiescent and allow the world to stop without waking it.
  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(10)),
    "world-stopped wait did not succeed; the event-loop thread may be unregistered or not marking itself parked"
  );

  // Wake the poller so it returns from `epoll_wait` and can observe the
  // safepoint request before running callbacks.
  runtime_native::async_rt::global().enqueue_microtask(Task::new(
    set_atomic_bool,
    microtask_ran as *const AtomicBool as *mut u8,
  ));

  // The microtask must not run while the world is stopped.
  std::thread::sleep(Duration::from_millis(20));
  assert!(
    !microtask_ran.load(Ordering::SeqCst),
    "microtask ran while stop-the-world was active"
  );

  runtime_native::rt_gc_resume_world();

  poll_returned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  assert!(microtask_ran.load(Ordering::SeqCst), "microtask did not run after GC resume");

  let _ = runtime_native::async_rt::global().cancel_timer(dummy_timer);

  // Calling `rt_async_poll` from another thread should be serialized and the
  // thread should be tracked as `External` (not a second `Main`).
  let (external_tid_tx, external_tid_rx) = mpsc::channel::<u64>();
  let (external_ready_tx, external_ready_rx) = mpsc::channel::<()>();
  let (external_exit_tx, external_exit_rx) = mpsc::channel::<()>();
  let external = std::thread::spawn(move || {
    external_tid_tx.send(current_os_thread_id()).unwrap();
    let _pending = runtime_native::rt_async_poll();
    external_ready_tx.send(()).unwrap();
    external_exit_rx.recv().unwrap();
    threading::unregister_current_thread();
  });

  let external_os_tid = external_tid_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  external_ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    let threads = threading::all_threads();
    let external_kind = threads
      .iter()
      .find(|t| t.os_thread_id() == external_os_tid)
      .map(|t| t.kind());

    if external_kind == Some(ThreadKind::External) {
      break;
    }

    assert!(
      Instant::now() < deadline,
      "timed out waiting for second rt_async_poll caller to register as External (threads={threads:?})"
    );
    std::thread::yield_now();
  }

  external_exit_tx.send(()).unwrap();
  external.join().unwrap();

  poll_exit_tx.send(()).unwrap();
  poller.join().unwrap();
}
