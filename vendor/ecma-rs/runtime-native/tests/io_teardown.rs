use runtime_native::io::{IoOpDebugHooks, IoRuntime};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_async_poll_legacy as rt_async_poll, rt_promise_then_legacy as rt_promise_then};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

fn pipe() -> (OwnedFd, OwnedFd) {
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
  // Safety: `pipe` returns new, owned file descriptors.
  unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn wait_until(deadline: Instant, mut pred: impl FnMut() -> bool) {
  while !pred() {
    assert!(Instant::now() < deadline, "timed out waiting for condition");
    let _ = rt_async_poll();
    std::thread::yield_now();
  }
}

#[test]
fn teardown_clears_registry_but_keeps_pins_until_cancel_ack() {
  let _rt = TestRuntimeGuard::new();
  let io_rt = IoRuntime::new();

  // Create a pipe and keep the read end open but never read from it, so writes eventually block.
  let (rfd, wfd) = pipe();

  // Large enough to overflow the pipe buffer so the worker thread blocks in poll().
  let backing: Arc<[u8]> = vec![0u8; 1024 * 1024].into();

  let debug = IoOpDebugHooks::pause_before_finish();
  let promise = io_rt
    .write_with_debug_hooks(wfd, backing.clone(), 0..backing.len(), &[], Some(debug.clone()))
    .unwrap();

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = Box::into_raw(settled);
  rt_promise_then(promise, set_bool, settled_ptr.cast::<u8>());

  assert_eq!(io_rt.debug_registry_len(), 1);
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, backing.len());

  // Trigger teardown while the write is blocked.
  io_rt.teardown();
  assert_eq!(io_rt.debug_registry_len(), 0, "teardown must clear the registry immediately");

  // Wait for the worker thread to observe cancellation and pause right before dropping the op.
  let start = Instant::now();
  wait_until(start + Duration::from_secs(2), || debug.reached_finish());

  // The op is now canceled but not yet dropped: pins/permit must still be held.
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, backing.len());
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  // Let the worker thread finish and drop the op record, releasing pins/permit.
  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || io_rt.debug_counters().inflight_ops_current == 0);

  assert_eq!(io_rt.debug_counters().pinned_bytes_current, 0);
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  drop(rfd);
  unsafe {
    drop(Box::from_raw(settled_ptr));
  }
}

#[test]
fn teardown_detaches_queued_completion_tasks() {
  let _rt = TestRuntimeGuard::new();
  let io_rt = IoRuntime::new();

  let (rfd, wfd) = pipe();

  let backing: Arc<[u8]> = vec![1u8].into();
  let debug = IoOpDebugHooks::pause_before_finish();
  let promise = io_rt
    .write_with_debug_hooks(wfd, backing.clone(), 0..backing.len(), &[], Some(debug.clone()))
    .unwrap();

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = Box::into_raw(settled);
  rt_promise_then(promise, set_bool, settled_ptr.cast::<u8>());

  // Wait until the I/O thread has finished the syscall and enqueued the completion task but is
  // paused before dropping its last reference.
  let start = Instant::now();
  while !debug.reached_finish() {
    assert!(Instant::now() < start + Duration::from_secs(2), "timed out waiting for worker finish");
    std::thread::yield_now();
  }

  // The op should still be in the registry at this point (completion task hasn't run yet).
  assert_eq!(io_rt.debug_registry_len(), 1);
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, backing.len());

  // Simulate hard termination / realm teardown: detach completions + clear registry.
  io_rt.teardown();
  assert_eq!(io_rt.debug_registry_len(), 0);

  // Allow the worker thread to drop the op record.
  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || io_rt.debug_counters().inflight_ops_current == 0);

  assert_eq!(io_rt.debug_counters().pinned_bytes_current, 0);

  // Drive the event loop. The queued completion task must *not* settle the promise after teardown.
  for _ in 0..16 {
    let _ = rt_async_poll();
  }
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  drop(rfd);
  unsafe {
    drop(Box::from_raw(settled_ptr));
  }
}
