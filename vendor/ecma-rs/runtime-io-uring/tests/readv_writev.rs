#![cfg(target_os = "linux")]

mod util;

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use runtime_io_uring::OwnedIoBuf;

#[test]
fn readv_writev_socketpair() {
  let Some(mut driver) = util::try_new_driver(8) else {
    return;
  };

  let (a, b) = UnixStream::pair().unwrap();

  let write_op = driver
    .submit_writev(
      a.as_raw_fd(),
      vec![
        OwnedIoBuf::from_vec(b"hello".to_vec()),
        OwnedIoBuf::from_vec(b" world".to_vec()),
      ],
      None,
    )
    .unwrap();

  let write_c = write_op.wait(&mut driver).unwrap();
  if write_c.result == -(libc::EINVAL as i32) || write_c.result == -(libc::EOPNOTSUPP as i32) {
    eprintln!("skipping: IORING_OP_WRITEV not supported by kernel");
    return;
  }
  assert_eq!(write_c.result, 11);

  let read_op = driver
    .submit_readv(
      b.as_raw_fd(),
      vec![OwnedIoBuf::new_zeroed(5), OwnedIoBuf::new_zeroed(6)],
      None,
    )
    .unwrap();

  let read_c = read_op.wait(&mut driver).unwrap();
  if read_c.result == -(libc::EINVAL as i32) || read_c.result == -(libc::EOPNOTSUPP as i32) {
    eprintln!("skipping: IORING_OP_READV not supported by kernel");
    return;
  }
  assert_eq!(read_c.result, 11);

  assert_eq!(read_c.resource.len(), 2);
  assert_eq!(read_c.resource[0].as_slice(), b"hello");
  assert_eq!(read_c.resource[1].as_slice(), b" world");
}
