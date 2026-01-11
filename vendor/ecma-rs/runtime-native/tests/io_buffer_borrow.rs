use runtime_native::io::{IoOpDebugHooks, IoRuntime, IoVecRange, PinnedIoVec};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::buffer::{ArrayBuffer, ArrayBufferError, BorrowError, Uint8Array};
use runtime_native::{rt_async_poll_legacy as rt_async_poll, TypedArrayError};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

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
fn safe_typed_array_access_is_rejected_while_read_is_in_flight() {
  let _rt = TestRuntimeGuard::new();
  let io_rt = IoRuntime::new();

  let (rfd, wfd) = pipe();

  // A 1-byte buffer is enough to test the borrow protocol deterministically.
  let backing_buf = ArrayBuffer::new_zeroed(1).unwrap();
  let view = Uint8Array::view(&backing_buf, 0, backing_buf.byte_len()).unwrap();

  // Start an in-flight `read(2)` op. `read` borrows the backing store exclusively (kernel writes
  // into the buffer), so safe JS-visible reads must be rejected.
  let debug = IoOpDebugHooks::pause_before_finish();
  let _promise = io_rt
    .read_with_debug_hooks(rfd, &view, 0..view.length(), &[], Some(debug.clone()))
    .unwrap();

  assert!(matches!(
    view.get(0),
    Err(TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed)))
  ));
  assert!(matches!(
    view.as_ptr_range(),
    Err(TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed)))
  ));

  // Write one byte so the read can complete.
  let byte = [0xABu8; 1];
  let rc = unsafe { libc::write(wfd.as_raw_fd(), byte.as_ptr().cast::<libc::c_void>(), 1) };
  assert_eq!(rc, 1);

  // Wait for the I/O worker to finish the syscall and pause before dropping its last reference.
  let start = Instant::now();
  while !debug.reached_finish() {
    assert!(
      Instant::now() < start + Duration::from_secs(2),
      "timed out waiting for worker finish"
    );
    std::thread::yield_now();
  }

  // Borrow is released on op drop, so safe access must still be rejected until the op is dropped.
  assert!(matches!(
    view.get(0),
    Err(TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed)))
  ));

  // Allow the worker to drop its reference, then drive the event loop to drop the op record from
  // the registry.
  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || {
    io_rt.debug_counters().inflight_ops_current == 0 && !backing_buf.is_io_borrowed()
  });

  // Safe access works again once the borrow is released.
  assert_eq!(view.get(0).unwrap(), Some(0xAB));
}

#[test]
fn safe_typed_array_access_is_rejected_while_read_iovecs_is_in_flight() {
  let _rt = TestRuntimeGuard::new();
  let io_rt = IoRuntime::new();

  let (rfd, wfd) = pipe();

  let backing_buf = ArrayBuffer::new_zeroed(1).unwrap();
  let view = Uint8Array::view(&backing_buf, 0, backing_buf.byte_len()).unwrap();

  let ranges = [IoVecRange::uint8_array(&view)];
  let iovecs = PinnedIoVec::try_from_ranges(&ranges).unwrap();

  let debug = IoOpDebugHooks::pause_before_finish();
  let _promise = io_rt
    .read_iovecs_with_debug_hooks(rfd, iovecs, &[], Some(debug.clone()))
    .unwrap();

  assert!(matches!(
    view.get(0),
    Err(TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed)))
  ));

  let byte = [0xCDu8; 1];
  let rc = unsafe { libc::write(wfd.as_raw_fd(), byte.as_ptr().cast::<libc::c_void>(), 1) };
  assert_eq!(rc, 1);

  let start = Instant::now();
  while !debug.reached_finish() {
    assert!(
      Instant::now() < start + Duration::from_secs(2),
      "timed out waiting for worker finish"
    );
    std::thread::yield_now();
  }

  assert!(matches!(
    view.get(0),
    Err(TypedArrayError::Buffer(ArrayBufferError::Borrow(BorrowError::Borrowed)))
  ));

  debug.release_finish();
  wait_until(start + Duration::from_secs(2), || {
    io_rt.debug_counters().inflight_ops_current == 0 && !backing_buf.is_io_borrowed()
  });

  assert_eq!(view.get(0).unwrap(), Some(0xCD));
}
