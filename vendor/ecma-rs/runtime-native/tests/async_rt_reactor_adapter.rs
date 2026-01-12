#![cfg(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use runtime_native::async_rt::{Interest, Task};
use runtime_native::test_util::TestRuntimeGuard;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

extern "C" fn set_atomic_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

extern "C" fn noop_task(_data: *mut u8) {}

struct ReadCtx {
  fired: AtomicBool,
  fd: i32,
}

extern "C" fn on_readable(data: *mut u8) {
  let ctx = unsafe { &*(data as *const ReadCtx) };

  // Drain until WouldBlock to satisfy the edge-triggered contract.
  let mut buf = [0u8; 256];
  loop {
    let rc = unsafe { libc::read(ctx.fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    if rc > 0 {
      continue;
    }
    if rc == 0 {
      break;
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::Interrupted {
      continue;
    }
    if err.kind() == std::io::ErrorKind::WouldBlock {
      break;
    }
    break;
  }

  ctx.fired.store(true, Ordering::SeqCst);
}

fn set_nonblocking(fd: RawFd) {
  unsafe {
    let flags = libc::fcntl(fd, libc::F_GETFL);
    assert!(flags >= 0, "fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
    let rc = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    assert!(rc >= 0, "fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
  }
}

fn make_pipe() -> (OwnedFd, OwnedFd) {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
  set_nonblocking(fds[0]);
  set_nonblocking(fds[1]);
  // SAFETY: `pipe` returns owned fds on success.
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  (read, write)
}

fn make_pipe_blocking() -> (OwnedFd, OwnedFd) {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
  // SAFETY: `pipe` returns owned fds on success.
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  (read, write)
}

#[test]
fn async_rt_register_fd_requires_nonblocking() {
  let _rt = TestRuntimeGuard::new();

  let (read, write) = make_pipe_blocking();

  let err = runtime_native::async_rt::register_fd(
    read.as_raw_fd(),
    Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect_err("expected registering a blocking fd to fail");
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "got {err:?}");
  assert!(
    err.to_string().contains("O_NONBLOCK"),
    "expected error to mention O_NONBLOCK, got {err}"
  );

  // Ensure the failure did not leave a stale registration behind by setting O_NONBLOCK and
  // re-registering.
  set_nonblocking(read.as_raw_fd());
  set_nonblocking(write.as_raw_fd());

  let id = runtime_native::async_rt::register_fd(
    read.as_raw_fd(),
    Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect("expected registration to succeed after setting O_NONBLOCK");
  assert!(runtime_native::async_rt::global().deregister_fd(id));
}

#[test]
fn async_rt_register_fd_rejects_empty_interest() {
  let _rt = TestRuntimeGuard::new();

  let (read, _write) = make_pipe();

  let err = runtime_native::async_rt::register_fd(
    read.as_raw_fd(),
    Interest::empty(),
    noop_task,
    std::ptr::null_mut(),
  )
  .expect_err("expected empty interest to be rejected");
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "got {err:?}");

  // Ensure the failed registration does not leave a stale watcher behind by registering again.
  let id = runtime_native::async_rt::register_fd(
    read.as_raw_fd(),
    Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect("expected registration after empty-interest failure to succeed");
  assert!(runtime_native::async_rt::global().deregister_fd(id));
}

#[test]
fn async_rt_register_fd_rejects_duplicate_fd() {
  let _rt = TestRuntimeGuard::new();

  let (read, _write) = make_pipe();

  let id1 = runtime_native::async_rt::register_fd(
    read.as_raw_fd(),
    Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect("expected initial register_fd to succeed");

  let err = runtime_native::async_rt::register_fd(
    read.as_raw_fd(),
    Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect_err("expected duplicate register_fd to fail");
  assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "got {err:?}");

  assert!(
    runtime_native::async_rt::global().deregister_fd(id1),
    "deregister_fd must still succeed for the original watcher id"
  );
}

#[test]
fn readiness_and_wake_coalescing_via_async_rt_adapter() {
  let _rt = TestRuntimeGuard::new();

  let (ready_read, ready_write) = make_pipe();
  let ctx: &'static ReadCtx = Box::leak(Box::new(ReadCtx {
    fired: AtomicBool::new(false),
    fd: ready_read.as_raw_fd(),
  }));

  let ready_id = runtime_native::async_rt::register_fd(
    ready_read.as_raw_fd(),
    Interest::READABLE,
    on_readable,
    ctx as *const ReadCtx as *mut u8,
  )
  .expect("register_fd failed");

  // Fail-safe timer: if the ready event never arrives, `rt_async_poll_legacy` should still return.
  let timed_out: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let timer = runtime_native::async_rt::global().schedule_timer_in(
    Duration::from_secs(5),
    Task::new(set_atomic_bool, timed_out as *const AtomicBool as *mut u8),
  );

  let (tx, rx) = mpsc::channel::<()>();
  let poll_thread = std::thread::spawn(move || {
    let _pending = runtime_native::rt_async_poll_legacy();
    tx.send(()).unwrap();
  });

  // Wait until the polling thread is actually blocked inside the reactor syscall.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    if runtime_native::async_rt::debug_in_epoll_wait() {
      std::thread::sleep(Duration::from_millis(10));
      if runtime_native::async_rt::debug_in_epoll_wait() {
        break;
      }
    }
    assert!(Instant::now() < deadline, "poll thread did not enter reactor wait in time");
    std::thread::yield_now();
  }

  // While the poll thread is blocked, repeatedly register/deregister a dummy watcher to produce a
  // burst of wake signals. This exercises wake coalescing (eventfd/pipe/EVFILT_USER).
  let (stress_read, stress_write) = make_pipe();
  for _ in 0..10_000 {
    let id = runtime_native::async_rt::register_fd(
      stress_read.as_raw_fd(),
      Interest::READABLE,
      noop_task,
      std::ptr::null_mut(),
    )
    .expect("stress register_fd failed");
    assert!(runtime_native::async_rt::global().deregister_fd(id), "stress deregister_fd failed");
  }
  drop(stress_write);
  drop(stress_read);

  let start = Instant::now();
  // Trigger readiness.
  let b = [0x1u8; 1];
  let rc = unsafe { libc::write(ready_write.as_raw_fd(), b.as_ptr().cast::<libc::c_void>(), 1) };
  assert_eq!(rc, 1);

  rx
    // Give the fail-safe timer (`5s`) a chance to fire even under heavy CI load. This prevents the
    // poll thread from being left detached (and still interacting with the global runtime) after a
    // timeout panic.
    .recv_timeout(Duration::from_secs(6))
    .expect("rt_async_poll_legacy did not return");
  let elapsed = start.elapsed();

  poll_thread.join().unwrap();

  // Clean up runtime state (also ensures future tests start idle even if this one fails).
  assert!(runtime_native::async_rt::global().deregister_fd(ready_id));
  assert!(runtime_native::async_rt::global().cancel_timer(timer));

  assert!(ctx.fired.load(Ordering::SeqCst), "ready callback did not run");
  assert!(
    !timed_out.load(Ordering::SeqCst),
    "poll returned due to the fail-safe timer rather than readiness"
  );
  assert!(
    elapsed < Duration::from_secs(1),
    "poll did not return promptly (elapsed={elapsed:?})"
  );
}
