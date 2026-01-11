#[cfg(not(target_os = "linux"))]
#[test]
fn io_uring_buffers_tests_skipped_non_linux() {
  // The io_uring integration tests are Linux-only; compilation is still expected to succeed
  // elsewhere.
}

#[cfg(target_os = "linux")]
mod linux {
  use std::io;
  use std::io::Write;
  use std::os::fd::AsRawFd;
  use std::os::fd::RawFd;
  use std::os::unix::net::UnixStream;
  use std::thread;
  use std::time::Duration;

  use runtime_io_uring::IoUringDriver;
  use runtime_native::buffer::{ArrayBuffer, BackingStoreAllocator, GlobalBackingStoreAllocator, Uint8Array};

  struct Fd(RawFd);

  impl Drop for Fd {
    fn drop(&mut self) {
      unsafe {
        libc::close(self.0);
      }
    }
  }

  fn pipe() -> io::Result<(Fd, Fd)> {
    let mut fds = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    Ok((Fd(fds[0]), Fd(fds[1])))
  }

  fn try_driver() -> io::Result<Option<IoUringDriver>> {
    match IoUringDriver::new(64) {
      Ok(d) => Ok(Some(d)),
      Err(e) => {
        eprintln!("skipping io_uring tests: {e}");
        Ok(None)
      }
    }
  }

  #[test]
  fn io_uring_read_writes_into_uint8array_backing_store() -> io::Result<()> {
    let Some(mut driver) = try_driver()? else {
      return Ok(());
    };

    let (mut write_end, read_end) = UnixStream::pair()?;

    let alloc = GlobalBackingStoreAllocator::default();
    let mut buf = ArrayBuffer::new_zeroed_in(&alloc, 5).expect("alloc ArrayBuffer backing store");
    let view = Uint8Array::view(&buf, 0, 5).expect("create Uint8Array view");
    let pinned = view.pin().expect("pin Uint8Array backing store");

    write_end.write_all(b"hello")?;

    let read_op = driver.submit_read(read_end.as_raw_fd(), pinned, 0)?;
    let read_c = read_op.wait(&mut driver)?;
    assert_eq!(read_c.result, 5);

    // Bytes were written into the backing store in-place: read through the original view.
    assert_eq!(view.get(0).unwrap(), Some(b'h'));
    assert_eq!(view.get(1).unwrap(), Some(b'e'));
    assert_eq!(view.get(2).unwrap(), Some(b'l'));
    assert_eq!(view.get(3).unwrap(), Some(b'l'));
    assert_eq!(view.get(4).unwrap(), Some(b'o'));

    // Clean up explicitly so other tests that look at allocator counters aren't affected by this
    // backing store.
    drop(read_c);
    drop(view);
    buf.finalize_in(&alloc);
    drop(buf);
    assert_eq!(alloc.external_bytes(), 0);

    Ok(())
  }

  #[test]
  fn io_uring_op_keeps_backing_store_alive_until_cqe() -> io::Result<()> {
    let Some(mut driver) = try_driver()? else {
      return Ok(());
    };

    let (read_fd, write_fd) = pipe()?;

    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 4).expect("alloc ArrayBuffer backing store");
    let view = Uint8Array::view(&buf, 0, 4).expect("create Uint8Array view");
    let pinned = view.pin().expect("pin Uint8Array backing store");

    assert_eq!(alloc.external_bytes(), 4);

    let read_op = driver.submit_read(read_fd.0, pinned, 0)?;

    // Drop the headers to simulate them becoming unreachable (e.g. GC finalization). The backing
    // store must remain alive while the io_uring op holds the pinned guard.
    drop(view);
    drop(buf);

    // Trigger a GC safepoint/collection cycle (milestone runtime doesn't yet run a full GC, but this
    // exercises the stop-the-world handshake).
    runtime_native::rt_gc_collect();

    assert_eq!(alloc.external_bytes(), 4);

    unsafe {
      libc::write(write_fd.0, b"rust".as_ptr() as *const _, 4);
    }

    let read_c = read_op.wait(&mut driver)?;
    assert_eq!(read_c.result, 4);
    assert_eq!(alloc.external_bytes(), 4);

    // The returned resource owns the pinned backing store guard; dropping it releases the last
    // handle, allowing the backing store to be freed.
    drop(read_c);
    assert_eq!(alloc.external_bytes(), 0);

    Ok(())
  }

  #[test]
  fn io_uring_cancel_does_not_drop_buffer_until_target_cqe() -> io::Result<()> {
    let Some(mut driver) = try_driver()? else {
      return Ok(());
    };

    let (read_fd, write_fd) = pipe()?;

    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 1).expect("alloc ArrayBuffer backing store");
    let view = Uint8Array::view(&buf, 0, 1).expect("create Uint8Array view");
    let pinned = view.pin().expect("pin Uint8Array backing store");

    drop(view);
    drop(buf);
    assert_eq!(alloc.external_bytes(), 1);

    // Blocked read.
    let read_op = driver.submit_read(read_fd.0, pinned, 0)?;

    // Best-effort cancel.
    let cancel_op = driver.cancel(read_op.id())?;

    // Safety net: if cancellation doesn't work, write a byte so the read unblocks.
    let write_fd_dup = unsafe { libc::dup(write_fd.0) };
    assert!(write_fd_dup >= 0);
    let writer = thread::spawn(move || {
      thread::sleep(Duration::from_millis(200));
      let _ = unsafe { libc::write(write_fd_dup, b"x".as_ptr() as *const _, 1) };
      unsafe {
        libc::close(write_fd_dup);
      }
    });

    let mut saw_cancel_before_read = false;
    while !(cancel_op.is_completed() && read_op.is_completed()) {
      driver.wait_for_cqe()?;
      if cancel_op.is_completed() && !read_op.is_completed() {
        saw_cancel_before_read = true;
        assert_eq!(
          alloc.external_bytes(),
          1,
          "backing store dropped before target CQE (cancel completed first)"
        );
      }
    }

    writer.join().expect("writer thread panicked");

    let cancel_res = cancel_op
      .try_take_completion()
      .expect("cancel completed")
      .result;
    let read_c = read_op.try_take_completion().expect("read completed");

    // The backing store is still held by the returned pinned buffer.
    assert_eq!(alloc.external_bytes(), 1);

    // Cancellation is best-effort and kernel-dependent; only assert that cancellation doesn't drop
    // the target buffer early.
    let cancel_unsupported = cancel_res == -(libc::EINVAL as i32)
      || cancel_res == -(libc::EOPNOTSUPP as i32)
      || cancel_res == -(libc::ENOSYS as i32);
    if cancel_unsupported {
      eprintln!("note: skipping cancellation result assertions (kernel returned {cancel_res})");
    }

    drop(read_c);
    assert_eq!(alloc.external_bytes(), 0);

    if !saw_cancel_before_read {
      eprintln!("note: cancel CQE arrived after read CQE on this kernel");
    }

    Ok(())
  }
}

