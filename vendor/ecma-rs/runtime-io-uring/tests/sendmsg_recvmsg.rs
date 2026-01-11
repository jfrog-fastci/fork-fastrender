#![cfg(target_os = "linux")]

mod util;

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use runtime_io_uring::{OwnedIoBuf, RecvMsg, SendMsg};

#[test]
fn sendmsg_recvmsg_socketpair_payload() {
  let Some(mut driver) = util::try_new_driver(8) else {
    return;
  };

  let (a, b) = UnixStream::pair().unwrap();

  let send_op = match driver.submit_sendmsg(
    a.as_raw_fd(),
    SendMsg::new(vec![
      OwnedIoBuf::from_vec(b"hi".to_vec()),
      OwnedIoBuf::from_vec(b" there".to_vec()),
    ]),
  ) {
    Ok(op) => op,
    Err(e) => {
      let raw = e.raw_os_error();
      if matches!(
        raw,
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
      ) {
        eprintln!("skipping: IORING_OP_SENDMSG not supported by kernel ({e})");
        return;
      }
      panic!("submit_sendmsg failed: {e}");
    }
  };

  let send_c = send_op.wait(&mut driver).unwrap();
  if send_c.result == -(libc::EINVAL as i32) || send_c.result == -(libc::EOPNOTSUPP as i32) {
    eprintln!("skipping: IORING_OP_SENDMSG not supported by kernel");
    return;
  }
  assert_eq!(send_c.result, 8);

  let recv_op = match driver.submit_recvmsg(
    b.as_raw_fd(),
    RecvMsg::new(vec![OwnedIoBuf::new_zeroed(2), OwnedIoBuf::new_zeroed(6)]),
  ) {
    Ok(op) => op,
    Err(e) => {
      let raw = e.raw_os_error();
      if matches!(
        raw,
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
      ) {
        eprintln!("skipping: IORING_OP_RECVMSG not supported by kernel ({e})");
        return;
      }
      panic!("submit_recvmsg failed: {e}");
    }
  };

  let recv_c = recv_op.wait(&mut driver).unwrap();
  if recv_c.result == -(libc::EINVAL as i32) || recv_c.result == -(libc::EOPNOTSUPP as i32) {
    eprintln!("skipping: IORING_OP_RECVMSG not supported by kernel");
    return;
  }
  assert_eq!(recv_c.result, 8);

  assert_eq!(recv_c.resource.bufs.len(), 2);
  assert_eq!(recv_c.resource.bufs[0].as_slice(), b"hi");
  assert_eq!(recv_c.resource.bufs[1].as_slice(), b" there");
  assert!(recv_c.resource.name().is_none());
  assert!(recv_c.resource.control().is_none());
}
