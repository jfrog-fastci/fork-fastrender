#[cfg(unix)]
mod unix {
  use runtime_native::buffer::ArrayBuffer;
  use runtime_native::io::{IoVecRange, PinnedIoVec, PinnedMsgHdr};
  use std::os::unix::io::RawFd;
  use std::io;

  #[derive(Debug)]
  struct Fd(RawFd);

  impl Drop for Fd {
    fn drop(&mut self) {
      unsafe {
        libc::close(self.0);
      }
    }
  }

  fn pipe() -> io::Result<(Fd, Fd)> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    Ok((Fd(fds[0]), Fd(fds[1])))
  }

  fn socketpair() -> io::Result<(Fd, Fd)> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    Ok((Fd(fds[0]), Fd(fds[1])))
  }

  #[test]
  fn writev_readv_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let (read_fd, write_fd) = pipe()?;

    let a = ArrayBuffer::from_bytes(b"hello ".to_vec()).unwrap();
    let b = ArrayBuffer::from_bytes(b"world".to_vec()).unwrap();
    let total_len = a.byte_len() + b.byte_len();

    let write_ranges = vec![
      IoVecRange::whole_array_buffer(&a),
      IoVecRange::whole_array_buffer(&b),
    ];
    let write_iov = PinnedIoVec::try_from_ranges(&write_ranges).unwrap();
    let write_iovcnt: libc::c_int = write_iov.len().try_into().unwrap();

    let nw = unsafe { libc::writev(write_fd.0, write_iov.as_iovec_ptr(), write_iovcnt) };
    if nw < 0 {
      return Err(io::Error::last_os_error().into());
    }
    assert_eq!(nw as usize, total_len);

    let out_a = ArrayBuffer::new_zeroed(a.byte_len()).unwrap();
    let out_b = ArrayBuffer::new_zeroed(b.byte_len()).unwrap();
    let read_ranges = vec![
      IoVecRange::whole_array_buffer(&out_a),
      IoVecRange::whole_array_buffer(&out_b),
    ];
    let read_iov = PinnedIoVec::try_from_ranges(&read_ranges).unwrap();
    let read_iovcnt: libc::c_int = read_iov.len().try_into().unwrap();

    let nr = unsafe { libc::readv(read_fd.0, read_iov.as_iovec_ptr(), read_iovcnt) };
    if nr < 0 {
      return Err(io::Error::last_os_error().into());
    }
    assert_eq!(nr as usize, total_len);

    let out_a_bytes = unsafe { out_a.pin().unwrap().as_slice().to_vec() };
    let out_b_bytes = unsafe { out_b.pin().unwrap().as_slice().to_vec() };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&out_a_bytes);
    out.extend_from_slice(&out_b_bytes);
    assert_eq!(&out, b"hello world");

    Ok(())
  }

  #[test]
  fn drop_releases_pins() {
    let buf = ArrayBuffer::new_zeroed(16).unwrap();
    assert_eq!(buf.pin_count(), 0);

    let ranges = vec![IoVecRange::whole_array_buffer(&buf)];
    let pinned = PinnedIoVec::try_from_ranges(&ranges).unwrap();
    assert_eq!(buf.pin_count(), 1);

    drop(pinned);
    assert_eq!(buf.pin_count(), 0);
  }

  #[test]
  fn sendmsg_recvmsg_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let (sock_a, sock_b) = socketpair()?;

    let a = ArrayBuffer::from_bytes(b"hello ".to_vec()).unwrap();
    let b = ArrayBuffer::from_bytes(b"world".to_vec()).unwrap();
    let total_len = a.byte_len() + b.byte_len();

    let write_ranges = vec![
      IoVecRange::whole_array_buffer(&a),
      IoVecRange::whole_array_buffer(&b),
    ];
    let write_iov = PinnedIoVec::try_from_ranges(&write_ranges).unwrap();
    let send = PinnedMsgHdr::new(write_iov);

    let nw = unsafe { libc::sendmsg(sock_a.0, send.as_msghdr_ptr(), 0) };
    if nw < 0 {
      return Err(io::Error::last_os_error().into());
    }
    assert_eq!(nw as usize, total_len);

    let out_a = ArrayBuffer::new_zeroed(a.byte_len()).unwrap();
    let out_b = ArrayBuffer::new_zeroed(b.byte_len()).unwrap();
    let read_ranges = vec![
      IoVecRange::whole_array_buffer(&out_a),
      IoVecRange::whole_array_buffer(&out_b),
    ];
    let read_iov = PinnedIoVec::try_from_ranges(&read_ranges).unwrap();
    let mut recv = PinnedMsgHdr::new(read_iov);

    let nr = unsafe { libc::recvmsg(sock_b.0, recv.as_msghdr_mut_ptr(), 0) };
    if nr < 0 {
      return Err(io::Error::last_os_error().into());
    }
    assert_eq!(nr as usize, total_len);

    let out_a_bytes = unsafe { out_a.pin().unwrap().as_slice().to_vec() };
    let out_b_bytes = unsafe { out_b.pin().unwrap().as_slice().to_vec() };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&out_a_bytes);
    out.extend_from_slice(&out_b_bytes);
    assert_eq!(&out, b"hello world");

    Ok(())
  }
}
