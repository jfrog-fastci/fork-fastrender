use runtime_native::buffer::{ArrayBufferError, BorrowError};
use runtime_native::io::{IoOpDebugHooks, IoRuntime, IoVecRange, PinnedIoVec};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::buffer::{ArrayBuffer, Uint8Array};
use runtime_native::{rt_async_poll_legacy as rt_async_poll, rt_promise_then_legacy as rt_promise_then};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
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

fn root_registry_len() -> usize {
  let mut count = 0usize;
  runtime_native::roots::global_root_registry().for_each_root_slot(|_slot| count += 1);
  count
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
  let buf = ArrayBuffer::new_zeroed(1024 * 1024).unwrap();
  let view = Uint8Array::view(&buf, 0, buf.byte_len()).unwrap();
  let root_obj = Box::into_raw(Box::new(0u8)) as *mut u8;

  let debug = IoOpDebugHooks::pause_before_finish();
  let promise = io_rt
    .write_with_debug_hooks(wfd, &view, 0..view.length(), &[root_obj], Some(debug.clone()))
    .unwrap();

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = Box::into_raw(settled);
  rt_promise_then(promise, set_bool, settled_ptr.cast::<u8>());

  assert_eq!(io_rt.debug_registry_len(), 1);
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, buf.byte_len());
  assert_eq!(root_registry_len(), 1);

  // Trigger teardown while the write is blocked.
  io_rt.teardown();
  assert_eq!(io_rt.debug_registry_len(), 0, "teardown must clear the registry immediately");

  // Wait for the worker thread to observe cancellation and pause right before dropping the op.
  let start = Instant::now();
  wait_until(start + Duration::from_secs(2), || debug.reached_finish());

  // The op is now canceled but not yet dropped: pins/permit must still be held.
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, buf.byte_len());
  assert_eq!(root_registry_len(), 1);
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  // Let the worker thread finish and drop the op record, releasing pins/permit.
  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || {
    let c = io_rt.debug_counters();
    c.inflight_ops_current == 0 && c.pinned_bytes_current == 0 && root_registry_len() == 0
  });

  assert_eq!(io_rt.debug_counters().pinned_bytes_current, 0);
  assert_eq!(root_registry_len(), 0);
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  drop(rfd);
  unsafe {
    drop(Box::from_raw(root_obj));
    drop(Box::from_raw(settled_ptr));
  }
}

#[test]
fn teardown_detaches_queued_completion_tasks() {
  let _rt = TestRuntimeGuard::new();
  let io_rt = IoRuntime::new();

  let (rfd, wfd) = pipe();

  let buf = ArrayBuffer::from_bytes(vec![1u8]).unwrap();
  let view = Uint8Array::view(&buf, 0, buf.byte_len()).unwrap();
  let root_obj = Box::into_raw(Box::new(0u8)) as *mut u8;
  let debug = IoOpDebugHooks::pause_before_finish();
  let promise = io_rt
    .write_with_debug_hooks(wfd, &view, 0..view.length(), &[root_obj], Some(debug.clone()))
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
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, buf.byte_len());
  assert_eq!(root_registry_len(), 1);

  // Simulate hard termination / realm teardown: detach completions + clear registry.
  io_rt.teardown();
  assert_eq!(io_rt.debug_registry_len(), 0);

  // Allow the worker thread to drop the op record.
  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || {
    let c = io_rt.debug_counters();
    c.inflight_ops_current == 0 && c.pinned_bytes_current == 0 && root_registry_len() == 0
  });

  assert_eq!(io_rt.debug_counters().pinned_bytes_current, 0);
  assert_eq!(root_registry_len(), 0);

  // Drive the event loop. The queued completion task must *not* settle the promise after teardown.
  for _ in 0..16 {
    let _ = rt_async_poll();
  }
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  drop(rfd);
  unsafe {
    drop(Box::from_raw(root_obj));
    drop(Box::from_raw(settled_ptr));
  }
}

#[test]
fn teardown_keeps_backing_store_pins_until_cancel_ack() {
  let _rt = TestRuntimeGuard::new();
  let io_rt = IoRuntime::new();

  // Keep the read end open but never read from it, so the write eventually blocks.
  let (rfd, wfd) = pipe();

  // Allocate a JS-style backing store that enforces pin-count rules for detach/transfer/resize.
  let mut buffer = Box::new(runtime_native::ArrayBuffer::new_zeroed(1024 * 1024).unwrap());
  let view = runtime_native::Uint8Array::view(&*buffer, 0, buffer.byte_len()).unwrap();

  let iovecs = PinnedIoVec::try_from_ranges(&[IoVecRange::uint8_array(&view)]).unwrap();
  let root_obj = Box::into_raw(Box::new(0u8)) as *mut u8;

  let debug = IoOpDebugHooks::pause_before_finish();
  let promise = io_rt
    .write_iovecs_with_debug_hooks(wfd, iovecs, &[root_obj], Some(debug.clone()))
    .unwrap();

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = Box::into_raw(settled);
  rt_promise_then(promise, set_bool, settled_ptr.cast::<u8>());

  assert_eq!(buffer.pin_count(), 1);
  assert!(buffer.is_io_borrowed());
  assert_eq!(
    buffer.try_with_slice(|_| ()).unwrap_err(),
    ArrayBufferError::Borrow(BorrowError::Borrowed)
  );
  assert_eq!(io_rt.debug_counters().inflight_ops_current, 1);
  assert_eq!(io_rt.debug_counters().pinned_bytes_current, buffer.byte_len());
  assert_eq!(root_registry_len(), 1);

  // Detach/transfer must be rejected while the backing store is pinned.
  assert_eq!(buffer.detach(), Err(ArrayBufferError::Pinned));
  assert_eq!(buffer.transfer().unwrap_err(), ArrayBufferError::Pinned);
  assert_eq!(buffer.resize(buffer.byte_len() * 2), Err(ArrayBufferError::Pinned));

  io_rt.teardown();
  assert_eq!(io_rt.debug_registry_len(), 0);

  let start = Instant::now();
  wait_until(start + Duration::from_secs(2), || debug.reached_finish());

  // Still pinned until the op record is dropped.
  assert_eq!(buffer.pin_count(), 1);
  assert!(buffer.is_io_borrowed());
  assert_eq!(buffer.detach(), Err(ArrayBufferError::Pinned));
  assert_eq!(buffer.transfer().unwrap_err(), ArrayBufferError::Pinned);
  assert_eq!(buffer.resize(buffer.byte_len() * 2), Err(ArrayBufferError::Pinned));

  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || {
    let c = io_rt.debug_counters();
    c.inflight_ops_current == 0
      && root_registry_len() == 0
      && buffer.pin_count() == 0
      && !buffer.is_io_borrowed()
  });

  assert_eq!(buffer.pin_count(), 0);
  assert_eq!(root_registry_len(), 0);
  buffer.try_with_slice(|_| ()).unwrap();
  assert!(!unsafe { &*settled_ptr }.load(Ordering::SeqCst));

  // Once unpinned, detach succeeds and frees the backing store.
  buffer.detach().unwrap();

  drop(rfd);
  unsafe {
    drop(Box::from_raw(root_obj));
    drop(Box::from_raw(settled_ptr));
  }
}
