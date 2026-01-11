use runtime_native::async_rt::Interest;
use runtime_native::async_rt::Task;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::test_util::TestRuntimeGuard;
use std::os::fd::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

extern "C" fn set_atomic_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

#[test]
fn timer_fires() {
  let _rt = TestRuntimeGuard::new();

  let fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_millis(10),
    Task::new(set_atomic_bool, fired as *const AtomicBool as *mut u8),
  );

  let start = Instant::now();
  while !fired.load(Ordering::SeqCst) {
    assert!(start.elapsed() < Duration::from_secs(1), "timer did not fire");
    runtime_native::rt_async_poll_legacy();
  }

  // With no remaining timers or watchers, the runtime should become idle.
  assert!(!runtime_native::rt_async_poll_legacy());
}

struct ReadCtx {
  fired: AtomicBool,
  fd: RawFd,
}

extern "C" fn on_readable(data: *mut u8) {
  let ctx = unsafe { &*(data as *const ReadCtx) };

  // Drain one byte so the pipe stops being readable.
  let mut buf = [0u8; 1];
  unsafe {
    let _ = libc::read(ctx.fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len());
  }

  ctx.fired.store(true, Ordering::SeqCst);
}

#[test]
fn epoll_readiness() {
  let _rt = TestRuntimeGuard::new();

  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
  assert_eq!(rc, 0, "pipe2 failed: {}", std::io::Error::last_os_error());
  let read_fd = fds[0];
  let write_fd = fds[1];

  let ctx: &'static ReadCtx = Box::leak(Box::new(ReadCtx {
    fired: AtomicBool::new(false),
    fd: read_fd,
  }));

  let watcher_id = runtime_native::async_rt::global()
    .register_fd(read_fd, Interest::READABLE, Task::new(on_readable, ctx as *const ReadCtx as *mut u8))
    .unwrap();

  let timed_out: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let watchdog = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(1),
    Task::new(set_atomic_bool, timed_out as *const AtomicBool as *mut u8),
  );

  let writer = std::thread::spawn(move || unsafe {
    // Give the poller a moment to enter epoll_wait.
    std::thread::sleep(Duration::from_millis(10));
    let buf = [1u8; 1];
    let _ = libc::write(write_fd, buf.as_ptr().cast::<libc::c_void>(), buf.len());
    let _ = libc::close(write_fd);
  });

  while !ctx.fired.load(Ordering::SeqCst) && !timed_out.load(Ordering::SeqCst) {
    runtime_native::rt_async_poll_legacy();
  }

  let ok = ctx.fired.load(Ordering::SeqCst);

  runtime_native::async_rt::global().deregister_fd(watcher_id);
  let _ = runtime_native::async_rt::global().cancel_timer(watchdog);
  let _ = writer.join();
  unsafe {
    let _ = libc::close(read_fd);
  }

  assert!(ok, "timed out waiting for epoll readability");
  assert!(!runtime_native::rt_async_poll_legacy());
}

#[test]
fn wake_from_epoll_wait() {
  let _rt = TestRuntimeGuard::new();

  let ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let timer_fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  // Keep the runtime non-idle so `rt_async_poll` will block in epoll_wait.
  let dummy_timer = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(1),
    Task::new(set_atomic_bool, timer_fired as *const AtomicBool as *mut u8),
  );

  let producer = std::thread::spawn({
    let ran_ref: &'static AtomicBool = ran;
    move || {
      std::thread::sleep(Duration::from_millis(20));
      runtime_native::async_rt::global().enqueue_microtask(Task::new(set_atomic_bool, ran_ref as *const AtomicBool as *mut u8));
    }
  });

  let start = Instant::now();
  let _pending = runtime_native::rt_async_poll_legacy();
  let elapsed = start.elapsed();

  let _ = producer.join();
  let _ = runtime_native::async_rt::global().cancel_timer(dummy_timer);

  assert!(ran.load(Ordering::SeqCst), "microtask did not run");
  assert!(
    elapsed < Duration::from_millis(500),
    "rt_async_poll did not wake promptly (elapsed={elapsed:?})"
  );
  assert!(!timer_fired.load(Ordering::SeqCst), "poll returned only after timer fired");
  assert!(!runtime_native::rt_async_poll_legacy());
}

#[test]
fn idle_detection() {
  let _rt = TestRuntimeGuard::new();

  // Avoid including one-time thread registration overhead in the timing check.
  threading::register_current_thread(ThreadKind::Main);

  // Warm up once to ensure the global runtime is initialized.
  let _ = runtime_native::rt_async_poll_legacy();

  let start = Instant::now();
  let pending = runtime_native::rt_async_poll_legacy();
  let elapsed = start.elapsed();

  assert!(!pending, "expected quiescent runtime");
  // `rt_async_poll` should return immediately when there are no watchers/timers, but allow generous
  // scheduling slack for heavily loaded CI/agent environments.
  assert!(
    elapsed < Duration::from_secs(1),
    "rt_async_poll should return quickly when idle (elapsed={elapsed:?})"
  );

  threading::unregister_current_thread();
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn parked_event_loop_thread_counts_as_quiescent_for_stop_the_world() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Keep the runtime non-idle so `rt_async_poll` blocks in epoll_wait.
  let timer_fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let timer_id = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(5),
    Task::new(set_atomic_bool, timer_fired as *const AtomicBool as *mut u8),
  );

  let (tx_id, rx_id) = mpsc::channel();
  let poll_thread = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Main);
    tx_id.send(id).unwrap();

    // This should block in epoll_wait.
    runtime_native::rt_async_poll_legacy();

    threading::unregister_current_thread();
  });

  let poll_thread_id = rx_id.recv().unwrap();

  // Wait until the poll thread has parked itself.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let parked = threading::all_threads()
      .into_iter()
      .find(|t| t.id() == poll_thread_id)
      .map(|t| t.is_parked())
      .unwrap_or(false);
    if parked {
      break;
    }
    assert!(Instant::now() < deadline, "poll thread did not park in time");
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let _resume = ResumeWorldOnDrop;
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1)),
    "stop-the-world should treat parked event loop threads as quiescent"
  );

  runtime_native::rt_gc_resume_world();

  // Wake the poll thread and allow it to return.
  let _ = runtime_native::async_rt::global().cancel_timer(timer_id);
  poll_thread.join().unwrap();

  assert!(!timer_fired.load(Ordering::SeqCst));
  threading::unregister_current_thread();
}
