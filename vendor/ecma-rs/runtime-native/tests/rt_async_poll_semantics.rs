use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

extern "C" fn inc(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
}

fn drive_two_macrotasks(poll: extern "C" fn() -> bool) -> (bool, usize, bool, usize) {
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
  runtime_native::async_rt::enqueue_macrotask(inc, counter as *const AtomicUsize as *mut u8);
  runtime_native::async_rt::enqueue_macrotask(inc, counter as *const AtomicUsize as *mut u8);

  let first_pending = poll();
  let after_first = counter.load(Ordering::SeqCst);

  let second_pending = poll();
  let after_second = counter.load(Ordering::SeqCst);

  (first_pending, after_first, second_pending, after_second)
}

fn drive_two_due_timers(poll: extern "C" fn() -> bool) -> (bool, usize, bool, usize) {
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
  runtime_native::test_util::schedule_timer(Duration::ZERO, inc, counter as *const AtomicUsize as *mut u8);
  runtime_native::test_util::schedule_timer(Duration::ZERO, inc, counter as *const AtomicUsize as *mut u8);

  let first_pending = poll();
  let after_first = counter.load(Ordering::SeqCst);

  let second_pending = poll();
  let after_second = counter.load(Ordering::SeqCst);

  (first_pending, after_first, second_pending, after_second)
}

#[test]
fn rt_async_poll_returns_false_when_idle() {
  let _rt = TestRuntimeGuard::new();
  assert!(!runtime_native::rt_async_poll());
  // Optional parity check: the legacy entrypoint is an alias with identical behavior.
  assert!(!runtime_native::rt_async_poll_legacy());
}

#[test]
fn rt_async_poll_macrotask_pending_semantics() {
  let _rt = TestRuntimeGuard::new();

  let (first_pending, after_first, second_pending, after_second) =
    drive_two_macrotasks(runtime_native::rt_async_poll);

  assert_eq!(after_first, 1, "first poll turn should execute exactly one macrotask");
  assert!(first_pending, "first poll turn should report pending work (second macrotask queued)");

  assert_eq!(after_second, 2, "second poll turn should execute the second macrotask");
  assert!(
    !second_pending,
    "second poll turn should report the runtime as fully idle after draining both macrotasks"
  );
}

#[test]
fn rt_async_poll_timer_pending_semantics() {
  let _rt = TestRuntimeGuard::new();

  let (first_pending, after_first, second_pending, after_second) =
    drive_two_due_timers(runtime_native::rt_async_poll);

  assert_eq!(after_first, 1, "first poll turn should execute exactly one due timer callback");
  assert!(
    first_pending,
    "first poll turn should report pending work (second due timer callback queued)"
  );

  assert_eq!(after_second, 2, "second poll turn should execute the second due timer callback");
  assert!(
    !second_pending,
    "second poll turn should report the runtime as fully idle after draining both due timers"
  );
}

#[test]
fn rt_async_poll_returns_true_when_timer_is_pending_after_turn() {
  let _rt = TestRuntimeGuard::new();

  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  // Queue a timer far in the future. If `rt_async_poll` were to block when work is pending but not
  // runnable, this would hang the test. Ensure there is immediately-runnable work by also enqueuing
  // a microtask.
  let timer = runtime_native::test_util::schedule_timer(
    Duration::from_secs(60),
    inc,
    counter as *const AtomicUsize as *mut u8,
  );
  runtime_native::test_util::enqueue_microtask(inc, counter as *const AtomicUsize as *mut u8);

  // The microtask should run, but the timer is still pending, so the runtime is not fully idle.
  assert!(runtime_native::rt_async_poll());
  assert_eq!(counter.load(Ordering::SeqCst), 1);

  // Cancel the timer and verify the runtime becomes idle.
  assert!(runtime_native::async_rt::global().cancel_timer(timer));
  assert!(!runtime_native::rt_async_poll());
}

#[test]
fn rt_async_poll_matches_legacy() {
  let stable = {
    let _rt = TestRuntimeGuard::new();
    drive_two_macrotasks(runtime_native::rt_async_poll)
  };

  let legacy = {
    let _rt = TestRuntimeGuard::new();
    drive_two_macrotasks(runtime_native::rt_async_poll_legacy)
  };

  assert_eq!(stable, legacy);
}

#[cfg(target_os = "linux")]
mod linux {
  use super::*;
  use runtime_native::abi::RT_IO_READABLE;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

  fn pipe_nonblocking() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    assert_eq!(rc, 0, "pipe2 failed: {}", std::io::Error::last_os_error());
    // Safety: `pipe2` returns new, owned fds on success.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    (read, write)
  }

  struct IoCtx {
    fd: i32,
    reads: AtomicUsize,
  }

  extern "C" fn on_readable(_events: u32, data: *mut u8) {
    let ctx = unsafe { &*(data as *const IoCtx) };
    let mut buf = [0u8; 8];
    unsafe {
      // Best-effort: drain one read. Errors are ignored (this is a test callback).
      let _ = libc::read(
        ctx.fd,
        buf.as_mut_ptr() as *mut libc::c_void,
        buf.len(),
      );
    }
    ctx.reads.fetch_add(1, Ordering::SeqCst);
  }

  #[test]
  fn rt_async_poll_reports_pending_when_io_watcher_is_registered() {
    let _rt = TestRuntimeGuard::new();

    let (read_fd, write_fd) = pipe_nonblocking();

    let ctx = Box::new(IoCtx {
      fd: read_fd.as_raw_fd(),
      reads: AtomicUsize::new(0),
    });
    let ctx_ptr = Box::into_raw(ctx);

    let watcher = runtime_native::rt_io_register(
      read_fd.as_raw_fd() as i32,
      RT_IO_READABLE,
      on_readable,
      ctx_ptr.cast::<u8>(),
    );
    assert_ne!(watcher, 0, "rt_io_register returned 0");

    // Make the fd readable.
    let byte: u8 = 1;
    let rc = unsafe {
      libc::write(
        write_fd.as_raw_fd(),
        (&byte as *const u8).cast::<libc::c_void>(),
        1,
      )
    };
    assert_eq!(rc, 1, "write failed: {}", std::io::Error::last_os_error());

    // Poll once: should run the watcher callback and return `true` because an I/O watcher remains
    // registered (pending work) even after the callback runs.
    assert!(runtime_native::rt_async_poll());
    assert_eq!(
      unsafe { &*ctx_ptr }.reads.load(Ordering::SeqCst),
      1,
      "expected exactly one watcher callback in one poll turn"
    );

    runtime_native::rt_io_unregister(watcher);
    // Safety: the watcher is unregistered, and we won't call `rt_async_poll` again until after
    // dropping the callback context.
    unsafe { drop(Box::from_raw(ctx_ptr)) };

    assert!(!runtime_native::rt_async_poll());
  }
}
