#![cfg(target_os = "linux")]

use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::panic::AssertUnwindSafe;

use runtime_io_uring::buf::GcIoBuf;
use runtime_io_uring::mock_gc::MockGc;
use runtime_io_uring::IoUringDriver;

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
fn drop_handle_does_not_free_pins_until_cqe() -> io::Result<()> {
  let Some(mut driver) = try_driver()? else {
    return Ok(());
  };

  let (rx, mut tx) = UnixStream::pair()?;

  let gc = MockGc::new();
  let handle = gc.alloc_zeroed(8);

  // Submit a blocked read backed by a GC-pinned buffer.
  let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
  let read_op = driver.submit_read(rx.as_raw_fd(), buf, 0)?;

  // Drop the per-op handle immediately (detached/canceled task). The GC root+pin guard must remain
  // alive until the CQE is processed.
  drop(read_op);
  assert_eq!(gc.root_drops(handle), 0);
  assert_eq!(gc.pin_drops(handle), 0);
  assert_eq!(gc.root_count(handle), 1);
  assert_eq!(gc.pin_count(handle), 1);

  // Complete the read and drive CQE processing. Cleanup is CQE-driven and happens exactly once.
  tx.write_all(b"x")?;
  while gc.root_count(handle) != 0 {
    driver.wait_for_cqe()?;
  }

  assert_eq!(gc.root_drops(handle), 1);
  assert_eq!(gc.pin_drops(handle), 1);
  assert_eq!(gc.root_count(handle), 0);
  assert_eq!(gc.pin_count(handle), 0);

  Ok(())
}

#[test]
fn drop_driver_with_inflight_ops_panics_in_debug_and_does_not_free_early() -> io::Result<()> {
  let Some(mut driver) = try_driver()? else {
    return Ok(());
  };

  let (rx, _tx) = UnixStream::pair()?;

  let gc = MockGc::new();
  let handle = gc.alloc_zeroed(8);

  // Blocked read, then immediately drop the driver without draining completions.
  let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
  let _read_op = driver.submit_read(rx.as_raw_fd(), buf, 0)?;

  let res = std::panic::catch_unwind(AssertUnwindSafe(|| drop(driver)));
  if cfg!(debug_assertions) {
    assert!(res.is_err(), "expected drop to panic in debug builds");
  } else {
    assert!(res.is_ok(), "drop should not panic in release builds");
  }

  // Regardless of policy behavior, dropping the driver must not free/prematurely unpin SQE
  // pointers while the kernel might still dereference them.
  assert_eq!(gc.root_drops(handle), 0);
  assert_eq!(gc.pin_drops(handle), 0);
  assert_eq!(gc.root_count(handle), 1);
  assert_eq!(gc.pin_count(handle), 1);

  Ok(())
}
