#![cfg(target_os = "linux")]

mod util;

use std::io;
use std::os::fd::RawFd;
use std::thread;
use std::time::Duration;

use runtime_io_uring::mock_gc::MockGc;
use runtime_io_uring::GcIoBuf;

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

#[test]
fn cancel_readv_holds_buffers_until_cqe() {
  let Some(mut driver) = util::try_new_driver(8) else {
    return;
  };

  let (read_fd, write_fd) = pipe().unwrap();

  let gc = MockGc::new();
  let h1 = gc.alloc_zeroed(8);
  let h2 = gc.alloc_zeroed(8);

  let b1: GcIoBuf<_> = GcIoBuf::from_gc(&gc, h1);
  let b2: GcIoBuf<_> = GcIoBuf::from_gc(&gc, h2);

  // Submit a readv that will block (writer stays open but sends nothing).
  let read_op = driver
    .submit_readv(read_fd.0, vec![b1, b2], None)
    .unwrap();

  // Best-effort cancel.
  let cancel_op = match driver.cancel(read_op.id()) {
    Ok(op) => op,
    Err(e) => {
      let raw = e.raw_os_error();
      if matches!(
        raw,
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
      ) {
        eprintln!("skipping: IORING_OP_ASYNC_CANCEL not supported by kernel ({e})");

        // Unblock and drain the read op before returning, to avoid leaving in-flight state.
        unsafe {
          libc::write(write_fd.0, b"x".as_ptr() as *const _, 1);
        }
        while !read_op.is_completed() {
          driver.wait_for_cqe().unwrap();
        }
        drop(read_op.try_take_completion().expect("read completed"));
        return;
      }
      panic!("cancel submit failed: {e}");
    }
  };

  // Safety net: if cancellation doesn't work, write some data so the read unblocks.
  let write_fd_dup = unsafe { libc::dup(write_fd.0) };
  assert!(write_fd_dup >= 0);
  let writer = thread::spawn(move || {
    thread::sleep(Duration::from_millis(200));
    let _ = unsafe { libc::write(write_fd_dup, b"x".as_ptr() as *const _, 1) };
    unsafe {
      libc::close(write_fd_dup);
    }
  });

  assert_eq!(gc.root_drops(h1), 0);
  assert_eq!(gc.pin_drops(h1), 0);
  assert_eq!(gc.root_drops(h2), 0);
  assert_eq!(gc.pin_drops(h2), 0);

  let mut saw_cancel_before_read = false;
  while !(cancel_op.is_completed() && read_op.is_completed()) {
    driver.wait_for_cqe().unwrap();

    if cancel_op.is_completed() && !read_op.is_completed() {
      saw_cancel_before_read = true;
      assert_eq!(gc.root_drops(h1), 0);
      assert_eq!(gc.pin_drops(h1), 0);
      assert_eq!(gc.root_count(h1), 1);
      assert_eq!(gc.pin_count(h1), 1);
      assert_eq!(gc.root_drops(h2), 0);
      assert_eq!(gc.pin_drops(h2), 0);
      assert_eq!(gc.root_count(h2), 1);
      assert_eq!(gc.pin_count(h2), 1);
    }
  }

  // Join the safety-net writer (no-op if cancellation worked).
  writer.join().expect("writer thread panicked");

  let cancel_res = cancel_op
    .try_take_completion()
    .expect("cancel completed")
    .result;
  let read_c = read_op.try_take_completion().expect("read completed");

  // The pinned/rooted guards are stored in the returned buffers, so they must not have dropped yet.
  assert_eq!(gc.root_drops(h1), 0);
  assert_eq!(gc.pin_drops(h1), 0);
  assert_eq!(gc.root_drops(h2), 0);
  assert_eq!(gc.pin_drops(h2), 0);

  let read_canceled = read_c.result == -(libc::ECANCELED as i32) || read_c.result == -(libc::EINTR as i32);
  if !read_canceled {
    let cancel_unsupported = cancel_res == -(libc::EINVAL as i32)
      || cancel_res == -(libc::EOPNOTSUPP as i32)
      || cancel_res == -(libc::ENOSYS as i32);

    if cancel_unsupported {
      eprintln!(
        "skipping cancellation semantics (kernel returned {cancel_res}); read result was {}",
        read_c.result
      );
      drop(read_c);
      return;
    }

    if cancel_res == -(libc::ENOENT as i32) {
      eprintln!(
        "skipping cancellation semantics due to race (cancel -ENOENT); read result was {}",
        read_c.result
      );
      drop(read_c);
      return;
    }

    panic!(
      "expected readv to be canceled (-ECANCELED/-EINTR), got {}; cancel CQE result={}",
      read_c.result, cancel_res
    );
  }

  drop(read_c);
  assert_eq!(gc.root_drops(h1), 1);
  assert_eq!(gc.pin_drops(h1), 1);
  assert_eq!(gc.root_count(h1), 0);
  assert_eq!(gc.pin_count(h1), 0);
  assert_eq!(gc.root_drops(h2), 1);
  assert_eq!(gc.pin_drops(h2), 1);
  assert_eq!(gc.root_count(h2), 0);
  assert_eq!(gc.pin_count(h2), 0);

  if !saw_cancel_before_read {
    eprintln!("note: cancel CQE arrived after readv CQE on this kernel");
  }
}
