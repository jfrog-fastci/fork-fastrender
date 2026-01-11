use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

extern "C" fn noop_task(_data: *mut u8) {}

fn make_pipe() -> (OwnedFd, OwnedFd) {
  let mut fds = [0; 2];
  // Safety: libc call; `fds` is valid for writes.
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc != 0 {
    panic!("pipe() failed: {}", std::io::Error::last_os_error());
  }

  // Safety: `pipe` returns owned fds on success.
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  (read, write)
}

#[test]
fn safepoint_request_wakes_epoll_wait() {
  // Ensure the async runtime is initialized (and registers its wake callback
  // with the GC safepoint coordinator).
  let _ = runtime_native::async_rt::global();

  // Register a dummy I/O watcher that never becomes ready so `rt_async_poll`
  // blocks in `epoll_wait`.
  let (read_fd, write_fd) = make_pipe();
  let watcher = runtime_native::async_rt::register_fd(
    read_fd.as_raw_fd(),
    runtime_native::async_rt::Interest::READABLE,
    noop_task,
    std::ptr::null_mut(),
  )
  .expect("failed to register dummy fd watcher");

  // Spawn a thread that blocks inside `rt_async_poll()` (and therefore `epoll_wait`).
  let (started_tx, started_rx) = mpsc::channel();
  let poll_thread = std::thread::spawn(move || {
    runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Main);
    started_tx.send(()).unwrap();
    runtime_native::rt_async_poll();
  });
  started_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("poll thread did not start");

  // Wait until the poll thread is actually blocked in `epoll_wait` (not just briefly entering it).
  let deadline = Instant::now() + Duration::from_secs(1);
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
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(100));

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

