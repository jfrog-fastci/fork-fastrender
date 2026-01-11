#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use runtime_io_uring::is_async_cancel_supported;
use runtime_io_uring::is_link_timeout_supported;
use runtime_io_uring::Completion;
use runtime_io_uring::Driver;
use runtime_io_uring::PreparedOp;

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
  fn drop(&mut self) {
    self.0.store(true, Ordering::SeqCst);
  }
}

fn new_supported_driver() -> Option<Driver> {
  let driver = match Driver::new(8) {
    Ok(driver) => driver,
    Err(err) => {
      eprintln!("skipping: failed to create io_uring instance: {err}");
      return None;
    }
  };

  let link_timeout_supported = match is_link_timeout_supported(&driver) {
    Ok(v) => v,
    Err(err) => {
      eprintln!("skipping: failed to probe io_uring ops: {err}");
      return None;
    }
  };

  if !link_timeout_supported {
    eprintln!("skipping: IORING_OP_LINK_TIMEOUT not supported by kernel");
    return None;
  }

  Some(driver)
}

#[test]
fn blocking_read_times_out_and_keeps_resources_until_target_cqe() {
  let mut driver = match new_supported_driver() {
    Some(driver) => driver,
    None => return,
  };

  let (reader, _writer) = UnixStream::pair().unwrap();

  let dropped = Arc::new(AtomicBool::new(false));
  let op = PreparedOp::read_with_keep_alive(
    reader.as_raw_fd(),
    vec![0u8; 16],
    DropFlag(Arc::clone(&dropped)),
  );

  let handle = driver.submit_with_timeout(op, Duration::from_millis(100)).unwrap();

  let mut seen_timeout = false;
  let mut seen_target = false;
  let mut target_res = 0;
  let mut timeout_res = 0;
  let mut target_op = None;

  while !(seen_timeout && seen_target) {
    assert!(
      !dropped.load(Ordering::SeqCst),
      "resources dropped before target CQE was processed"
    );

    match driver.wait().unwrap() {
      Completion::Timeout { id, target, res } => {
        if id == handle.timeout_id {
          assert!(!seen_timeout, "duplicate timeout completion");
          assert_eq!(target, handle.op_id);
          timeout_res = res;
          seen_timeout = true;
        }
      }
      Completion::Op { id, res, op } => {
        if id == handle.op_id {
          assert!(!seen_target, "duplicate target completion");
          target_res = res;
          target_op = Some(op);
          seen_target = true;
        }
      }
      _ => {}
    }
  }

  assert_eq!(timeout_res, -libc::ETIME);
  assert_eq!(target_res, -libc::ECANCELED);

  assert!(
    !dropped.load(Ordering::SeqCst),
    "resources dropped before target completion was observed"
  );
  drop(target_op);
  assert!(
    dropped.load(Ordering::SeqCst),
    "resources were not released after dropping target completion"
  );
}

#[test]
fn complete_just_before_timeout_race() {
  let mut driver = match new_supported_driver() {
    Some(driver) => driver,
    None => return,
  };

  let (reader, mut writer) = UnixStream::pair().unwrap();

  let handle = driver
    .submit_with_timeout(PreparedOp::read(reader.as_raw_fd(), vec![0u8; 5]), Duration::from_millis(500))
    .unwrap();

  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(400));
    writer.write_all(b"hello").unwrap();
  });

  let deadline = Instant::now() + Duration::from_secs(5);

  let mut seen_timeout = false;
  let mut seen_target = false;
  let mut target_res = 0;
  let mut timeout_res = 0;
  let mut target_op = None;

  while !(seen_timeout && seen_target) {
    assert!(Instant::now() < deadline, "test timed out waiting for CQEs");

    match driver.wait().unwrap() {
      Completion::Timeout { id, target, res } => {
        if id == handle.timeout_id {
          assert!(!seen_timeout, "duplicate timeout completion");
          assert_eq!(target, handle.op_id);
          timeout_res = res;
          seen_timeout = true;
        }
      }
      Completion::Op { id, res, op } => {
        if id == handle.op_id {
          assert!(!seen_target, "duplicate target completion");
          target_res = res;
          target_op = Some(op);
          seen_target = true;
        }
      }
      _ => {}
    }
  }

  assert_eq!(target_res, 5);
  match target_op.unwrap() {
    PreparedOp::Read { buf, .. } => assert_eq!(&buf[..5], b"hello"),
  }
  assert_eq!(timeout_res, -libc::ECANCELED);
}

#[test]
fn explicit_cancel_vs_timeout_race() {
  let mut driver = match new_supported_driver() {
    Some(driver) => driver,
    None => return,
  };

  let async_cancel_supported = match is_async_cancel_supported(&driver) {
    Ok(v) => v,
    Err(err) => {
      eprintln!("skipping: failed to probe io_uring ops: {err}");
      return;
    }
  };

  if !async_cancel_supported {
    eprintln!("skipping: IORING_OP_ASYNC_CANCEL not supported by kernel");
    return;
  }

  let (reader, _writer) = UnixStream::pair().unwrap();

  let dropped = Arc::new(AtomicBool::new(false));
  let handle = driver
    .submit_with_timeout(
      PreparedOp::read_with_keep_alive(
        reader.as_raw_fd(),
        vec![0u8; 16],
        DropFlag(Arc::clone(&dropped)),
      ),
      Duration::from_secs(5),
    )
    .unwrap();

  let cancel_id = driver.cancel(handle.op_id).unwrap();

  let deadline = Instant::now() + Duration::from_secs(5);
  let mut seen_timeout = false;
  let mut seen_target = false;
  let mut seen_cancel = false;
  let mut target_res = 0;
  let mut timeout_res = 0;
  let mut cancel_res = 0;
  let mut target_op = None;

  while !(seen_timeout && seen_target && seen_cancel) {
    assert!(
      !dropped.load(Ordering::SeqCst) || seen_target,
      "resources dropped before target CQE was processed"
    );
    assert!(Instant::now() < deadline, "test timed out waiting for CQEs");

    match driver.wait().unwrap() {
      Completion::Timeout { id, target, res } => {
        if id == handle.timeout_id {
          assert!(!seen_timeout, "duplicate timeout completion");
          assert_eq!(target, handle.op_id);
          timeout_res = res;
          seen_timeout = true;
        }
      }
      Completion::Op { id, res, op } => {
        if id == handle.op_id {
          assert!(!seen_target, "duplicate target completion");
          target_res = res;
          target_op = Some(op);
          seen_target = true;
        }
      }
      Completion::Cancel { id, target, res } => {
        if id == cancel_id {
          assert!(!seen_cancel, "duplicate cancel completion");
          assert_eq!(target, handle.op_id);
          cancel_res = res;
          seen_cancel = true;
        }
      }
    }
  }

  assert_eq!(target_res, -libc::ECANCELED);
  assert!(
    timeout_res == -libc::ECANCELED || timeout_res == -libc::ETIME,
    "unexpected timeout res: {timeout_res}"
  );
  assert!(
    cancel_res == 0 || cancel_res == -libc::ENOENT,
    "unexpected cancel res: {cancel_res}"
  );

  drop(target_op);
  assert!(
    dropped.load(Ordering::SeqCst),
    "resources were not released after dropping target completion"
  );
}
