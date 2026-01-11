use runtime_native::buffer::{ArrayBuffer, Uint8Array};
use runtime_native::io::IoRuntime;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

fn pipe() -> (OwnedFd, OwnedFd) {
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
  // Safety: `pipe` returns new, owned file descriptors.
  unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn stop_the_world_completes_while_io_worker_blocked_in_poll() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let io_rt = IoRuntime::new();

  // Create a pipe and keep the read end open but never read from it, so writes eventually block.
  let (rfd, wfd) = pipe();

  // Large enough to overflow the pipe buffer so the worker thread blocks in poll().
  let buffer = ArrayBuffer::new_zeroed(1024 * 1024).expect("ArrayBuffer alloc failed");
  let view = Uint8Array::view(&buffer, 0, buffer.byte_len()).expect("Uint8Array view failed");

  let _promise = io_rt
    .write(wfd, &view, 0..view.length())
    .expect("IoRuntime::write failed");

  // Ensure the op is in-flight (i.e. the worker thread should exist and be executing syscalls).
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);

  // Wait until the I/O thread has entered a GC-safe region while blocked in poll/write syscalls.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let io_native_safe = threading::all_threads()
      .into_iter()
      .any(|t| t.kind() == ThreadKind::Io && t.is_native_safe());
    if io_native_safe {
      break;
    }
    assert!(Instant::now() < deadline, "io worker did not enter NativeSafe in time");
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1)),
    "world did not stop while IO worker was blocked in poll"
  );
  runtime_native::rt_gc_resume_world();

  // Cancel the I/O op and wait for the worker thread to observe cancellation and drop its permit.
  io_rt.teardown();
  let deadline = Instant::now() + Duration::from_secs(2);
  while io_rt.debug_counters().inflight_ops_current != 0 {
    assert!(Instant::now() < deadline, "io worker did not finish after teardown");
    // Drive the event loop in case any completion tasks were queued before teardown.
    let _ = runtime_native::rt_async_poll_legacy();
    std::thread::yield_now();
  }

  drop(rfd);
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_completes_while_io_worker_blocked_in_poll_read() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let io_rt = IoRuntime::new();

  // Create a pipe and keep the write end open but never write to it, so reads block in poll().
  let (rfd, wfd) = pipe();

  let buffer = ArrayBuffer::new_zeroed(1024).expect("ArrayBuffer alloc failed");
  let view = Uint8Array::view(&buffer, 0, buffer.byte_len()).expect("Uint8Array view failed");

  let _promise = io_rt
    .read(rfd, &view, 0..view.length())
    .expect("IoRuntime::read failed");

  // Ensure the op is in-flight (i.e. the worker thread should exist and be executing syscalls).
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);

  // Wait until the I/O thread has entered a GC-safe region while blocked in poll/read syscalls.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let io_native_safe = threading::all_threads()
      .into_iter()
      .any(|t| t.kind() == ThreadKind::Io && t.is_native_safe());
    if io_native_safe {
      break;
    }
    assert!(Instant::now() < deadline, "io worker did not enter NativeSafe in time");
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1)),
    "world did not stop while IO worker was blocked in poll"
  );
  runtime_native::rt_gc_resume_world();

  io_rt.teardown();
  let deadline = Instant::now() + Duration::from_secs(2);
  while io_rt.debug_counters().inflight_ops_current != 0 {
    assert!(Instant::now() < deadline, "io worker did not finish after teardown");
    let _ = runtime_native::rt_async_poll_legacy();
    std::thread::yield_now();
  }

  drop(wfd);
  threading::unregister_current_thread();
}
