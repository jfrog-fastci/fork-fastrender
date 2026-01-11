#![cfg(all(target_os = "linux", feature = "debug_stability"))]

use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;

use runtime_io_uring::mock_gc::{MockGc, MockGcHandle};
use runtime_io_uring::GcIoBuf;
use runtime_io_uring::IoBuf;
use runtime_io_uring::IoBufMut;
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
fn pinned_gc_buf_pointer_is_stable_until_cqe() -> io::Result<()> {
  let Some(mut driver) = try_driver()? else {
    return Ok(());
  };

  let (reader, mut writer) = UnixStream::pair()?;

  let gc = MockGc::new();
  let handle = gc.alloc_zeroed(16);
  let before = gc.ptr(handle).unwrap();

  let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
  let read_op = driver.submit_read(reader.as_raw_fd(), buf, 0)?;

  // Force a collection while the op is in-flight; the pinned buffer must not relocate.
  gc.collect();
  let after = gc.ptr(handle).unwrap();
  assert_eq!(before, after);

  writer.write_all(b"hello")?;
  let c = read_op.wait(&mut driver)?;
  assert_eq!(c.result, 5);

  Ok(())
}

/// Intentionally unsound `IoBufMut` implementation that points into movable GC memory without
/// pinning. With `debug_stability`, the driver should detect the relocation and panic on CQE.
#[derive(Debug)]
struct UnpinnedMockGcBuf {
  gc: MockGc,
  handle: MockGcHandle,
  // Keep the object alive across collections.
  _root: runtime_io_uring::mock_gc::MockGcRoot,
}

unsafe impl IoBuf for UnpinnedMockGcBuf {
  fn stable_ptr(&self) -> NonNull<u8> {
    self.gc.ptr(self.handle).expect("invalid handle")
  }

  fn len(&self) -> usize {
    self.gc.len(self.handle).expect("invalid handle")
  }
}

unsafe impl IoBufMut for UnpinnedMockGcBuf {
  fn stable_mut_ptr(&mut self) -> NonNull<u8> {
    self.gc.ptr(self.handle).expect("invalid handle")
  }
}

#[test]
fn unpinned_gc_buf_panics_on_cqe_pointer_change() -> io::Result<()> {
  let Some(mut driver) = try_driver()? else {
    return Ok(());
  };

  let (reader, mut writer) = UnixStream::pair()?;

  let gc = MockGc::new();
  let handle = gc.alloc_zeroed(16);
  let before = gc.ptr(handle).unwrap();

  // Root the object but do not pin it.
  let root = <MockGc as runtime_io_uring::GcHooks>::root(&gc, handle);
  let bad_buf = UnpinnedMockGcBuf {
    gc: gc.clone(),
    handle,
    _root: root,
  };

  let read_op = driver.submit_read(reader.as_raw_fd(), bad_buf, 0)?;

  // This relocates the rooted but unpinned object, changing the buffer pointer.
  gc.collect();
  let after = gc.ptr(handle).unwrap();
  assert_ne!(before, after);

  writer.write_all(b"hello")?;

  // Panics during CQE processing.
  let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _ = read_op.wait(&mut driver).unwrap();
  }));
  let Err(panic_payload) = res else {
    panic!("expected debug_stability panic, but op completed successfully");
  };

  let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
    *s
  } else if let Some(s) = panic_payload.downcast_ref::<String>() {
    s.as_str()
  } else {
    "<non-string panic payload>"
  };
  assert!(
    msg.contains("pointer moved"),
    "unexpected panic message: {msg}"
  );
  Ok(())
}
