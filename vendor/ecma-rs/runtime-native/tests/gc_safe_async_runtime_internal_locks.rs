#![cfg(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use runtime_native::async_rt::Task;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

const TIMEOUT: Duration = Duration::from_secs(2);

extern "C" fn noop(_: *mut u8) {}
extern "C" fn noop_io(_: u32, _: *mut u8) {}

fn wait_until_native_safe(thread_id: u64) {
  let deadline = Instant::now() + TIMEOUT;
  loop {
    let native_safe = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == thread_id)
      .map(|t| t.is_native_safe())
      .unwrap_or(false);
    if native_safe {
      return;
    }
    assert!(
      Instant::now() < deadline,
      "thread {thread_id} did not enter NativeSafe while blocked on async runtime lock"
    );
    std::thread::yield_now();
  }
}

fn pipe_nonblocking() -> (OwnedFd, OwnedFd) {
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());

  for &fd in &fds {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_ne!(flags, -1, "fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_ne!(rc, -1, "fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
  }

  // Safety: `pipe` returns new, owned file descriptors.
  unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_microtask_queue_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
  let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
  let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();
  let (b_start_tx, b_start_rx) = mpsc::channel::<()>();

  let thread_a = std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    runtime_native::async_rt::debug_with_microtasks_lock(|| {
      a_locked_tx.send(()).unwrap();
      a_release_rx.recv().unwrap();
    });

    runtime_native::rt_gc_safepoint();
    threading::unregister_current_thread();
  });

  a_locked_rx
    .recv_timeout(TIMEOUT)
    .expect("thread A should lock the microtask queue");

  let thread_b = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    b_id_tx.send(id.get()).unwrap();
    b_start_rx.recv().unwrap();

    // Contend on the microtask queue lock.
    runtime_native::async_rt::global().enqueue_microtask(Task::new(noop, std::ptr::null_mut()));

    threading::unregister_current_thread();
  });

  let b_id = b_id_rx.recv_timeout(TIMEOUT).expect("thread B should register");
  b_start_tx.send(()).unwrap();
  wait_until_native_safe(b_id);

  runtime_native::rt_gc_request_stop_the_world();
  a_release_tx.send(()).unwrap();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(TIMEOUT);
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while a thread was blocked on the microtask queue lock"
  );

  thread_a.join().unwrap();
  thread_b.join().unwrap();
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_timers_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let timer_id = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(60),
    Task::new(noop, std::ptr::null_mut()),
  );

  let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
  let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
  let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();
  let (b_start_tx, b_start_rx) = mpsc::channel::<()>();

  let thread_a = std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    runtime_native::async_rt::debug_with_timers_lock(|| {
      a_locked_tx.send(()).unwrap();
      a_release_rx.recv().unwrap();
    });

    runtime_native::rt_gc_safepoint();
    threading::unregister_current_thread();
  });

  a_locked_rx
    .recv_timeout(TIMEOUT)
    .expect("thread A should lock the timers map");

  let thread_b = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    b_id_tx.send(id.get()).unwrap();
    b_start_rx.recv().unwrap();

    // Contend on the timers lock.
    assert!(runtime_native::async_rt::global().cancel_timer(timer_id));

    threading::unregister_current_thread();
  });

  let b_id = b_id_rx.recv_timeout(TIMEOUT).expect("thread B should register");
  b_start_tx.send(()).unwrap();
  wait_until_native_safe(b_id);

  runtime_native::rt_gc_request_stop_the_world();
  a_release_tx.send(()).unwrap();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(TIMEOUT);
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while a thread was blocked on the timers lock"
  );

  thread_a.join().unwrap();
  thread_b.join().unwrap();
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_reactor_watchers_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let (rfd, _wfd) = pipe_nonblocking();
  let watcher = runtime_native::async_rt::global()
    .register_io(rfd.as_raw_fd(), runtime_native::abi::RT_IO_READABLE, noop_io, std::ptr::null_mut())
    .expect("register_io failed");

  let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
  let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
  let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();
  let (b_start_tx, b_start_rx) = mpsc::channel::<()>();

  let thread_a = std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    runtime_native::async_rt::debug_with_reactor_watchers_lock(|| {
      a_locked_tx.send(()).unwrap();
      a_release_rx.recv().unwrap();
    });

    runtime_native::rt_gc_safepoint();
    threading::unregister_current_thread();
  });

  a_locked_rx
    .recv_timeout(TIMEOUT)
    .expect("thread A should lock the reactor watcher map");

  let thread_b = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    b_id_tx.send(id.get()).unwrap();
    b_start_rx.recv().unwrap();

    assert!(runtime_native::async_rt::global().deregister_fd(watcher));

    threading::unregister_current_thread();
  });

  let b_id = b_id_rx.recv_timeout(TIMEOUT).expect("thread B should register");
  b_start_tx.send(()).unwrap();
  wait_until_native_safe(b_id);

  runtime_native::rt_gc_request_stop_the_world();
  a_release_tx.send(()).unwrap();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(TIMEOUT);
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while a thread was blocked on the reactor watcher map lock"
  );

  thread_a.join().unwrap();
  thread_b.join().unwrap();
  drop(rfd);
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_completes_while_thread_waits_on_web_timers_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let timer = runtime_native::rt_set_timeout(noop, std::ptr::null_mut(), 60_000);

  let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
  let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
  let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();
  let (b_start_tx, b_start_rx) = mpsc::channel::<()>();

  let thread_a = std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    let lock = runtime_native::debug_hold_web_timers_lock();
    a_locked_tx.send(()).unwrap();
    a_release_rx.recv().unwrap();
    drop(lock);

    runtime_native::rt_gc_safepoint();
    threading::unregister_current_thread();
  });

  a_locked_rx
    .recv_timeout(TIMEOUT)
    .expect("thread A should lock the web timer registry");

  let thread_b = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    b_id_tx.send(id.get()).unwrap();
    b_start_rx.recv().unwrap();

    runtime_native::rt_clear_timer(timer);

    threading::unregister_current_thread();
  });

  let b_id = b_id_rx.recv_timeout(TIMEOUT).expect("thread B should register");
  b_start_tx.send(()).unwrap();
  wait_until_native_safe(b_id);

  runtime_native::rt_gc_request_stop_the_world();
  a_release_tx.send(()).unwrap();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(TIMEOUT);
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while a thread was blocked on the web timer registry lock"
  );

  thread_a.join().unwrap();
  thread_b.join().unwrap();

  threading::unregister_current_thread();
}
