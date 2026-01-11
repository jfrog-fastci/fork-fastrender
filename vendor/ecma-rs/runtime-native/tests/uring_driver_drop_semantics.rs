#[cfg(not(target_os = "linux"))]
#[test]
fn uring_driver_drop_semantics_skipped_non_linux() {
  // The UringDriver is Linux-only; compilation is still expected to succeed elsewhere.
}

#[cfg(target_os = "linux")]
mod linux {
  use std::future::Future;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::{Arc, Mutex};
  use std::task::{Context, Poll, Wake, Waker};
  use std::time::{Duration, Instant};

  use runtime_native::buffer::{ArrayBuffer, Uint8Array};
  use runtime_native::gc::{GcHeap, TypeDescriptor, OBJ_HEADER_SIZE};
  use runtime_native::io::{IoLimiter, IoLimits, UringDriver, UringIoError};
  use runtime_native::test_util::TestRuntimeGuard;

  fn is_uring_unavailable(err: &std::io::Error) -> bool {
    matches!(
      err.raw_os_error(),
      Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
  }

  static ARRAY_BUFFER_DESC: TypeDescriptor =
    TypeDescriptor::new(OBJ_HEADER_SIZE + core::mem::size_of::<ArrayBuffer>(), &[]);
  static UINT8_ARRAY_PTR_OFFSETS: [u32; 1] = [OBJ_HEADER_SIZE as u32];
  static UINT8_ARRAY_DESC: TypeDescriptor = TypeDescriptor::new(
    OBJ_HEADER_SIZE + core::mem::size_of::<Uint8Array>(),
    &UINT8_ARRAY_PTR_OFFSETS,
  );
  static DUMMY_DESC: TypeDescriptor = TypeDescriptor::new(OBJ_HEADER_SIZE, &[]);

  fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(std::io::Error::last_os_error());
    }
    // Safety: `pipe` returns new, owned file descriptors.
    let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((rfd, wfd))
  }

  fn alloc_array_buffer(heap: &mut GcHeap, byte_len: usize) -> *mut u8 {
    // Allocate the ArrayBuffer header in old gen so it doesn't move during nursery evacuation.
    let obj = heap.alloc_old(&ARRAY_BUFFER_DESC);
    let payload = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer };
    let header = ArrayBuffer::new_zeroed(byte_len).unwrap();
    unsafe {
      payload.write(header);
    }
    obj
  }

  fn alloc_uint8_array(
    heap: &mut GcHeap,
    buffer: *mut u8,
    byte_offset: usize,
    length: usize,
  ) -> *mut u8 {
    let obj = heap.alloc_young(&UINT8_ARRAY_DESC);
    let payload = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut Uint8Array };
    let buffer_payload = unsafe { &*(buffer.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    let view = Uint8Array::view(buffer_payload, byte_offset, length).unwrap();
    unsafe {
      payload.write(view);
    }
    obj
  }

  fn alloc_dummy(heap: &mut GcHeap) -> *mut u8 {
    heap.alloc_young(&DUMMY_DESC)
  }

  fn finalize_array_buffer(buffer_obj: *mut u8) {
    // Safety: payload layout matches `ArrayBuffer`.
    let buf = unsafe { &mut *(buffer_obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer) };
    buf.finalize();
  }

  fn pin_count(buffer_obj: *mut u8) -> u32 {
    let buf = unsafe { &*(buffer_obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    buf.pin_count()
  }

  fn is_io_borrowed(buffer_obj: *mut u8) -> bool {
    let buf = unsafe { &*(buffer_obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    buf.is_io_borrowed()
  }

  fn write_all(fd: RawFd, bytes: &[u8]) {
    let mut written = 0usize;
    while written < bytes.len() {
      let rc = unsafe {
        libc::write(
          fd,
          bytes[written..].as_ptr() as *const libc::c_void,
          bytes.len() - written,
        )
      };
      assert!(rc >= 0, "write failed: {}", std::io::Error::last_os_error());
      written += rc as usize;
    }
  }

  struct FlagWake {
    flag: Arc<AtomicBool>,
  }

  impl Wake for FlagWake {
    fn wake(self: Arc<Self>) {
      self.flag.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
      self.flag.store(true, Ordering::SeqCst);
    }
  }

  fn flag_waker(flag: Arc<AtomicBool>) -> Waker {
    Waker::from(Arc::new(FlagWake { flag }))
  }

  fn block_on<F: Future>(fut: F, timeout: Duration) -> F::Output {
    let woke = Arc::new(AtomicBool::new(false));
    let waker = flag_waker(woke.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    let deadline = Instant::now() + timeout;

    loop {
      match fut.as_mut().poll(&mut cx) {
        Poll::Ready(out) => return out,
        Poll::Pending => {
          while !woke.swap(false, Ordering::SeqCst) {
            assert!(Instant::now() <= deadline, "timed out waiting for future");
            std::thread::yield_now();
          }
        }
      }
    }
  }

  #[test]
  fn shutdown_and_drain_releases_pins_and_borrows() {
    let _rt = TestRuntimeGuard::new();

    let heap = Arc::new(Mutex::new(GcHeap::new()));
    let limiter = Arc::new(IoLimiter::new(IoLimits {
      max_pinned_bytes: 1024,
      max_inflight_ops: 8,
      max_pinned_bytes_per_op: None,
    }));
    let driver = match UringDriver::new_with_limiter(8, Arc::clone(&limiter)) {
      Ok(driver) => driver,
      Err(err) if is_uring_unavailable(&err) => return,
      Err(err) => panic!("failed to create io_uring driver: {err:?}"),
    };

    let (rfd, wfd) = pipe().unwrap();

    let (array_obj, buffer_obj, promise_obj, bytes_ptr) = {
      let mut heap = heap.lock().unwrap();
      let buffer_obj = alloc_array_buffer(&mut heap, 1);
      let array_obj = alloc_uint8_array(&mut heap, buffer_obj, 0, 1);
      let promise_obj = alloc_dummy(&mut heap);

      let bytes_ptr = unsafe {
        let view = &*(array_obj.add(OBJ_HEADER_SIZE) as *const Uint8Array);
        view.as_ptr_range().unwrap().0 as *const u8
      };

      (array_obj, buffer_obj, promise_obj, bytes_ptr)
    };

    let fut = driver
      .read_into_uint8_array(
        Arc::clone(&heap),
        rfd.as_raw_fd(),
        array_obj,
        promise_obj,
        None,
      )
      .unwrap();

    assert_eq!(pin_count(buffer_obj), 1);
    assert!(is_io_borrowed(buffer_obj));
    assert_eq!(limiter.counters().inflight_ops_current, 1);
    assert_eq!(limiter.counters().pinned_bytes_current, 1);

    // Unblock the read.
    write_all(wfd.as_raw_fd(), b"x");

    // Drain and stop the driver thread.
    driver.shutdown_and_drain().unwrap();

    match block_on(fut, Duration::from_secs(2)) {
      Ok(n) => {
        assert_eq!(n, 1);
        let got = unsafe { std::slice::from_raw_parts(bytes_ptr, n) };
        assert_eq!(got, &[b'x']);
      }
      Err(UringIoError::Cancelled) => {
        // Cancellation is best-effort; shutdown may cancel the read before it observes the byte.
      }
      Err(err) => panic!("unexpected read error: {err:?}"),
    }

    assert_eq!(pin_count(buffer_obj), 0);
    assert!(!is_io_borrowed(buffer_obj));
    assert_eq!(limiter.counters().inflight_ops_current, 0);
    assert_eq!(limiter.counters().pinned_bytes_current, 0);

    finalize_array_buffer(buffer_obj);
  }

  #[test]
  fn drop_with_inflight_op_panics_in_debug_and_leaks_in_release() {
    let _rt = TestRuntimeGuard::new();

    // Use a tiny nursery to keep the intentional leak small.
    let heap = Arc::new(Mutex::new(GcHeap::with_nursery_size(64 * 1024)));
    let limiter = Arc::new(IoLimiter::new(IoLimits {
      max_pinned_bytes: 1024,
      max_inflight_ops: 8,
      max_pinned_bytes_per_op: None,
    }));
    let driver = match UringDriver::new_with_limiter(8, Arc::clone(&limiter)) {
      Ok(driver) => driver,
      Err(err) if is_uring_unavailable(&err) => return,
      Err(err) => panic!("failed to create io_uring driver: {err:?}"),
    };

    let (rfd, _wfd) = pipe().unwrap();

    let (array_obj, buffer_obj, promise_obj) = {
      let mut heap = heap.lock().unwrap();
      let buffer_obj = alloc_array_buffer(&mut heap, 1);
      let array_obj = alloc_uint8_array(&mut heap, buffer_obj, 0, 1);
      let promise_obj = alloc_dummy(&mut heap);
      (array_obj, buffer_obj, promise_obj)
    };

    let fut = driver
      .read_into_uint8_array(
        Arc::clone(&heap),
        rfd.as_raw_fd(),
        array_obj,
        promise_obj,
        None,
      )
      .unwrap();

    assert_eq!(pin_count(buffer_obj), 1);
    assert!(is_io_borrowed(buffer_obj));
    assert_eq!(limiter.counters().inflight_ops_current, 1);
    assert_eq!(limiter.counters().pinned_bytes_current, 1);

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      // Drop the outer handle first; the future still holds a clone.
      drop(driver);
      // Dropping the last handle while an op is still in flight is not allowed (policy B).
      drop(fut);
    }));

    if cfg!(debug_assertions) {
      assert!(
        res.is_err(),
        "expected a debug panic on drop with in-flight ops"
      );
    } else {
      assert!(
        res.is_ok(),
        "release builds should be safe-by-leak (no panic)"
      );
    }

    // The backing store pin and IO borrow must still be held: SQE pointers must remain valid.
    assert_eq!(pin_count(buffer_obj), 1);
    assert!(is_io_borrowed(buffer_obj));
    assert_eq!(limiter.counters().inflight_ops_current, 1);
    assert_eq!(limiter.counters().pinned_bytes_current, 1);
  }
}
