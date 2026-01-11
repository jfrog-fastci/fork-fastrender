use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use runtime_native::test_util::TestRuntimeGuard;

extern "C" fn noop_task(_data: *mut u8) {}

fn make_pipe() -> (OwnedFd, OwnedFd) {
  let mut fds = [0; 2];

  #[cfg(target_os = "linux")]
  {
    // Safety: libc call; `fds` is valid for writes.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
      panic!("pipe2() failed: {}", std::io::Error::last_os_error());
    }
  }

  #[cfg(not(target_os = "linux"))]
  {
    // Safety: libc call; `fds` is valid for writes.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      panic!("pipe() failed: {}", std::io::Error::last_os_error());
    }

    // Mimic `pipe2(O_CLOEXEC|O_NONBLOCK)` for platforms without `pipe2`.
    for &fd in &fds {
      let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
      if fd_flags == -1 {
        panic!("fcntl(F_GETFD) failed: {}", std::io::Error::last_os_error());
      }
      let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) };
      if rc == -1 {
        panic!("fcntl(F_SETFD) failed: {}", std::io::Error::last_os_error());
      }

      let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
      if flags == -1 {
        panic!("fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
      }
      let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
      if rc == -1 {
        panic!("fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
      }
    }
  }

  // Safety: `pipe` returns owned fds on success.
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  (read, write)
}

#[test]
fn safepoint_request_wakes_epoll_wait() {
  let _rt = TestRuntimeGuard::new();
  // Ensure the async runtime is initialized (and registers its wake callback
  // with the GC safepoint coordinator).
  let _ = runtime_native::async_rt::global();

  // Register a dummy I/O watcher that never becomes ready so `rt_async_poll_legacy`
  // blocks in `epoll_wait`.
  let (read_fd, write_fd) = make_pipe();
  let watcher = runtime_native::async_rt::register_fd(
    read_fd.as_raw_fd(),
    runtime_native::async_rt::Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect("failed to register dummy fd watcher");

  // Spawn a thread that blocks inside `rt_async_poll_legacy()` (and therefore `epoll_wait`).
  let (started_tx, started_rx) = mpsc::channel();
  let poll_thread = std::thread::spawn(move || {
    runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Main);
    started_tx.send(()).unwrap();
    runtime_native::rt_async_poll_legacy();
  });
  started_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("poll thread did not start");

  // Wait until the poll thread is actually blocked in `epoll_wait` (not just briefly entering it).
  //
  // This can be surprisingly sensitive to host load: the poll thread may be delayed by scheduling
  // or may briefly wake to drain the reactor wake eventfd before blocking. Keep the deadline
  // generous to avoid CI flakes.
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    if runtime_native::async_rt::debug_in_epoll_wait() {
      std::thread::sleep(Duration::from_millis(10));
      if runtime_native::async_rt::debug_in_epoll_wait() {
        break;
      }
    }
    if Instant::now() > deadline {
      panic!("poll thread did not enter epoll_wait");
    }
    std::thread::sleep(Duration::from_millis(1));
  }

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(500));

  // Always resume + clean up so the test can't hang even on failure.
  runtime_native::rt_gc_resume_world();

  runtime_native::async_rt::global().deregister_fd(watcher);

  // Keep the write end open until after deregistration so the read end doesn't
  // receive a hangup event.
  drop(write_fd);
  drop(read_fd);

  poll_thread.join().unwrap();

  assert!(
    stopped,
    "GC stop-the-world did not complete in time; epoll_wait likely was not woken"
  );
}
