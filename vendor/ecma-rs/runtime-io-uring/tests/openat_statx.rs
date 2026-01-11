#![cfg(target_os = "linux")]

use std::fs;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use runtime_io_uring::Completion;
use runtime_io_uring::Driver;
use runtime_io_uring::PreparedOp;

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
  fn drop(&mut self) {
    self.0.fetch_add(1, Ordering::SeqCst);
  }
}

fn new_driver() -> Option<Driver> {
  match Driver::new(8) {
    Ok(d) => Some(d),
    Err(err) => {
      eprintln!("skipping: failed to create io_uring instance: {err}");
      None
    }
  }
}

#[test]
fn openat_reads_file() {
  let Some(mut driver) = new_driver() else {
    return;
  };

  let dir = tempfile::tempdir().unwrap();
  let file_name = "hello.txt";
  let file_path = dir.path().join(file_name);
  let contents = b"hello world";
  fs::write(&file_path, contents).unwrap();

  let dir_file = fs::File::open(dir.path()).unwrap();
  let op_id = driver
    .submit_openat(
      dir_file.as_raw_fd(),
      std::path::Path::new(file_name),
      libc::O_RDONLY,
      0,
    )
    .unwrap();

  let (res, op) = match driver.wait().unwrap() {
    Completion::Op { id, res, op } => {
      assert_eq!(id, op_id);
      (res, op)
    }
    other => panic!("unexpected completion: {other:?}"),
  };

  assert!(res >= 0, "openat failed: {res}");
  match op {
    PreparedOp::OpenAt { .. } => {}
    other => panic!("expected OpenAt op, got: {other:?}"),
  }

  let mut file = unsafe { fs::File::from_raw_fd(res) };
  let mut got = Vec::new();
  file.read_to_end(&mut got).unwrap();
  assert_eq!(got, contents);
}

#[test]
fn statx_reports_size() {
  let Some(mut driver) = new_driver() else {
    return;
  };

  let dir = tempfile::tempdir().unwrap();
  let file_name = "size.txt";
  let file_path = dir.path().join(file_name);
  let contents = b"1234567890";
  fs::write(&file_path, contents).unwrap();

  let dir_file = fs::File::open(dir.path()).unwrap();
  let op_id = driver
    .submit_statx(
      dir_file.as_raw_fd(),
      std::path::Path::new(file_name),
      0,
      libc::STATX_SIZE as u32,
    )
    .unwrap();

  let (res, op) = match driver.wait().unwrap() {
    Completion::Op { id, res, op } => {
      assert_eq!(id, op_id);
      (res, op)
    }
    other => panic!("unexpected completion: {other:?}"),
  };

  if res == -libc::EINVAL || res == -libc::EOPNOTSUPP {
    eprintln!("skipping: IORING_OP_STATX not supported by kernel");
    return;
  }

  let st = op.into_statx_result(res).unwrap();
  assert_ne!(st.stx_mask & (libc::STATX_SIZE as u32), 0);
  assert_eq!(st.stx_size as usize, contents.len());
}

#[test]
fn openat_path_buffer_is_dropped_only_after_cqe_is_processed() {
  let Some(mut driver) = new_driver() else {
    return;
  };

  let dir = tempfile::tempdir().unwrap();
  let file_name = "drop.txt";
  let file_path = dir.path().join(file_name);
  fs::write(&file_path, b"x").unwrap();

  let dir_file = fs::File::open(dir.path()).unwrap();

  let drops = Arc::new(AtomicUsize::new(0));
  let op_id = driver
    .submit(
      PreparedOp::openat_with_keep_alive(
        dir_file.as_raw_fd(),
        std::path::Path::new(file_name),
        libc::O_RDONLY,
        0,
        DropCounter(Arc::clone(&drops)),
      )
      .unwrap(),
    )
    .unwrap();

  assert_eq!(drops.load(Ordering::SeqCst), 0);

  let completion = driver.wait().unwrap();
  let (res, op) = match completion {
    Completion::Op { id, res, op } => {
      assert_eq!(id, op_id);
      (res, op)
    }
    other => panic!("unexpected completion: {other:?}"),
  };
  assert!(res >= 0);
  drop(unsafe { fs::File::from_raw_fd(res) });

  assert_eq!(drops.load(Ordering::SeqCst), 0);
  drop(op);
  assert_eq!(drops.load(Ordering::SeqCst), 1);
}
