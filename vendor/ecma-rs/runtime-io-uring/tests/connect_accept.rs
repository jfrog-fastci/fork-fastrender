#![cfg(target_os = "linux")]

use runtime_io_uring::is_accept_supported;
use runtime_io_uring::is_async_cancel_supported;
use runtime_io_uring::Completion;
use runtime_io_uring::Driver;
use runtime_io_uring::PreparedOp;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

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

  let accept_supported = match is_accept_supported(&driver) {
    Ok(v) => v,
    Err(err) => {
      eprintln!("skipping: failed to probe io_uring ops: {err}");
      return None;
    }
  };

  if !accept_supported {
    eprintln!("skipping: IORING_OP_ACCEPT not supported by kernel");
    return None;
  }

  Some(driver)
}

#[test]
fn accept_connect_happy_path() {
  let mut driver = match new_supported_driver() {
    Some(d) => d,
    None => return,
  };

  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let addr = listener.local_addr().unwrap();

  let accept_id = driver.submit_accept(listener.as_raw_fd(), 0).unwrap();

  let mut client = TcpStream::connect(addr).unwrap();
  client.write_all(b"x").unwrap();

  let mut accepted_fd = None;
  let mut peer_addr = None;
  while accepted_fd.is_none() {
    match driver.wait().unwrap() {
      Completion::Op { id, res, op } if id == accept_id => {
        assert!(res >= 0, "accept failed: {res}");
        peer_addr = op.accept_peer_addr();
        accepted_fd = Some(res);
      }
      _ => {}
    }
  }

  let accepted_fd = accepted_fd.unwrap();
  let mut server = unsafe { TcpStream::from_raw_fd(accepted_fd) };
  server
    .set_read_timeout(Some(Duration::from_secs(1)))
    .unwrap();

  let mut buf = [0u8; 1];
  server.read_exact(&mut buf).unwrap();
  assert_eq!(&buf, b"x");

  // Peer address decoding is best-effort; just ensure it doesn't panic and is plausible.
  if let Some(peer) = peer_addr {
    assert_eq!(peer.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
  }
}

#[test]
fn accept_cancel_keeps_metadata_alive_until_cqe() {
  let mut driver = match new_supported_driver() {
    Some(d) => d,
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

  let listener = TcpListener::bind("127.0.0.1:0").unwrap();

  let dropped = Arc::new(AtomicBool::new(false));
  let accept_op = PreparedOp::accept_with_keep_alive(
    listener.as_raw_fd(),
    0,
    DropFlag(Arc::clone(&dropped)),
  );
  let accept_id = driver.submit(accept_op).unwrap();
  let _cancel_id = driver.cancel(accept_id).unwrap();

  assert!(
    !dropped.load(Ordering::SeqCst),
    "accept op dropped before CQE"
  );

  let mut seen_accept = false;
  let mut accept_res = 0;
  let mut accept_op = None;

  while !seen_accept {
    assert!(
      !dropped.load(Ordering::SeqCst),
      "resources dropped before accept CQE was processed"
    );

    match driver.wait().unwrap() {
      Completion::Op { id, res, op } if id == accept_id => {
        accept_res = res;
        accept_op = Some(op);
        seen_accept = true;
      }
      _ => {}
    }
  }

  assert!(
    accept_res == -libc::ECANCELED || accept_res == -libc::EINTR,
    "unexpected accept cancel result: {accept_res}"
  );

  assert!(
    !dropped.load(Ordering::SeqCst),
    "resources dropped before accept completion was observed"
  );
  drop(accept_op);
  assert!(
    dropped.load(Ordering::SeqCst),
    "resources were not released after dropping accept completion"
  );
}
