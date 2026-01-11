#[cfg(unix)]
mod unix {
  use runtime_native::buffer::{ArrayBuffer, Uint8Array};
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
  fn writev_readv_uint8array_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let (read_fd, write_fd) = pipe()?;

    // Test both:
    // - a non-zero view byteOffset (via `Uint8Array::view`), and
    // - a non-zero range offset (via `IoVecRange::uint8_array_range`).
    let buf_a = ArrayBuffer::from_bytes(b"_hello ".to_vec()).unwrap();
    let view_a = Uint8Array::view(&buf_a, 1, 6).unwrap(); // "hello "

    let buf_b = ArrayBuffer::from_bytes(b"world_".to_vec()).unwrap();
    let view_b = Uint8Array::view(&buf_b, 0, 6).unwrap(); // "world_"

    let total_len = 6 + 5;

    let write_ranges = vec![
      IoVecRange::uint8_array(&view_a),
      IoVecRange::uint8_array_range(&view_b, 0, 5).unwrap(), // "world"
    ];
    let write_iov = PinnedIoVec::try_from_ranges(&write_ranges).unwrap();
    let write_iovcnt: libc::c_int = write_iov.len().try_into().unwrap();

    let nw = unsafe { libc::writev(write_fd.0, write_iov.as_iovec_ptr(), write_iovcnt) };
    if nw < 0 {
      return Err(io::Error::last_os_error().into());
    }
    assert_eq!(nw as usize, total_len);

    let out_a = ArrayBuffer::new_zeroed(6).unwrap();
    let out_view_a = Uint8Array::view(&out_a, 0, 6).unwrap();
    let out_b = ArrayBuffer::new_zeroed(5).unwrap();
    let out_view_b = Uint8Array::view(&out_b, 0, 5).unwrap();

    let read_ranges = vec![
      IoVecRange::uint8_array(&out_view_a),
      IoVecRange::uint8_array(&out_view_b),
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
  fn drop_releases_pins_via_msghdr() {
    let buf = ArrayBuffer::new_zeroed(16).unwrap();
    assert_eq!(buf.pin_count(), 0);

    let ranges = vec![IoVecRange::whole_array_buffer(&buf)];
    let pinned = PinnedIoVec::try_from_ranges(&ranges).unwrap();
    assert_eq!(buf.pin_count(), 1);

    let hdr = PinnedMsgHdr::new(pinned);
    assert_eq!(buf.pin_count(), 1);

    drop(hdr);
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

  #[cfg(target_os = "linux")]
  #[test]
  fn uring_writev_readv_smoke() -> Result<(), Box<dyn std::error::Error>> {
    use io_uring::{opcode, types, IoUring};
    use std::os::fd::AsRawFd;

    let ring = IoUring::new(8);
    let mut ring = match ring {
      Ok(r) => r,
      Err(err) => {
        eprintln!("skipping: failed to create io_uring instance: {err}");
        return Ok(());
      }
    };

    let file = tempfile::tempfile()?;
    let fd = file.as_raw_fd();

    let a = ArrayBuffer::from_bytes(b"hello ".to_vec()).unwrap();
    let b = ArrayBuffer::from_bytes(b"world".to_vec()).unwrap();
    let total_len = a.byte_len() + b.byte_len();

    let write_ranges = vec![
      IoVecRange::whole_array_buffer(&a),
      IoVecRange::whole_array_buffer(&b),
    ];
    let write_iov = PinnedIoVec::try_from_ranges(&write_ranges).unwrap();
    let write_iovcnt: u32 = write_iov.len().try_into().unwrap();

    let sqe = opcode::Writev::new(types::Fd(fd), write_iov.as_iovec_ptr(), write_iovcnt)
      .offset(0)
      .build()
      .user_data(1);
    unsafe {
      ring
        .submission()
        .push(&sqe)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring submission queue is full"))?;
    }
    ring.submit_and_wait(1)?;

    let mut wrote = None;
    for cqe in ring.completion() {
      if cqe.user_data() == 1 {
        wrote = Some(cqe.result());
      }
    }
    let wrote = wrote.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing writev cqe"))?;
    if wrote < 0 {
      // Some kernels/filesystems may not support all io_uring ops. Match runtime-io-uring's
      // behavior and treat EINVAL/EOPNOTSUPP as a skip.
      if wrote == -libc::EINVAL || wrote == -libc::EOPNOTSUPP {
        eprintln!("skipping: IORING_OP_WRITEV not supported by kernel/filesystem");
        return Ok(());
      }
      return Err(io::Error::from_raw_os_error(-wrote).into());
    }
    assert_eq!(wrote as usize, total_len);

    let out_a = ArrayBuffer::new_zeroed(a.byte_len()).unwrap();
    let out_b = ArrayBuffer::new_zeroed(b.byte_len()).unwrap();
    let read_ranges = vec![
      IoVecRange::whole_array_buffer(&out_a),
      IoVecRange::whole_array_buffer(&out_b),
    ];
    let read_iov = PinnedIoVec::try_from_ranges(&read_ranges).unwrap();
    let read_iovcnt: u32 = read_iov.len().try_into().unwrap();

    let sqe = opcode::Readv::new(types::Fd(fd), read_iov.as_iovec_ptr(), read_iovcnt)
      .offset(0)
      .build()
      .user_data(2);
    unsafe {
      ring
        .submission()
        .push(&sqe)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring submission queue is full"))?;
    }
    ring.submit_and_wait(1)?;

    let mut read = None;
    for cqe in ring.completion() {
      if cqe.user_data() == 2 {
        read = Some(cqe.result());
      }
    }
    let read = read.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing readv cqe"))?;
    if read < 0 {
      if read == -libc::EINVAL || read == -libc::EOPNOTSUPP {
        eprintln!("skipping: IORING_OP_READV not supported by kernel/filesystem");
        return Ok(());
      }
      return Err(io::Error::from_raw_os_error(-read).into());
    }
    assert_eq!(read as usize, total_len);

    let out_a_bytes = unsafe { out_a.pin().unwrap().as_slice().to_vec() };
    let out_b_bytes = unsafe { out_b.pin().unwrap().as_slice().to_vec() };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&out_a_bytes);
    out.extend_from_slice(&out_b_bytes);
    assert_eq!(&out, b"hello world");

    Ok(())
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn uring_sendmsg_recvmsg_smoke() -> Result<(), Box<dyn std::error::Error>> {
    use io_uring::{opcode, types, IoUring};

    let ring = IoUring::new(8);
    let mut ring = match ring {
      Ok(r) => r,
      Err(err) => {
        eprintln!("skipping: failed to create io_uring instance: {err}");
        return Ok(());
      }
    };

    let (sock_a, sock_b) = socketpair()?;

    let a = ArrayBuffer::from_bytes(b"hello ".to_vec()).unwrap();
    let b = ArrayBuffer::from_bytes(b"world".to_vec()).unwrap();
    let total_len = a.byte_len() + b.byte_len();

    let send_ranges = vec![
      IoVecRange::whole_array_buffer(&a),
      IoVecRange::whole_array_buffer(&b),
    ];
    let send_iov = PinnedIoVec::try_from_ranges(&send_ranges).unwrap();
    let send_hdr = PinnedMsgHdr::new(send_iov);

    let sqe = opcode::SendMsg::new(types::Fd(sock_a.0), send_hdr.as_msghdr_ptr())
      .flags(0)
      .build()
      .user_data(1);
    unsafe {
      ring
        .submission()
        .push(&sqe)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring submission queue is full"))?;
    }
    ring.submit_and_wait(1)?;

    let mut send_res = None;
    for cqe in ring.completion() {
      if cqe.user_data() == 1 {
        send_res = Some(cqe.result());
      }
    }
    let send_res =
      send_res.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing sendmsg cqe"))?;
    if send_res < 0 {
      if send_res == -libc::EINVAL || send_res == -libc::EOPNOTSUPP {
        eprintln!("skipping: IORING_OP_SENDMSG not supported by kernel");
        return Ok(());
      }
      return Err(io::Error::from_raw_os_error(-send_res).into());
    }
    assert_eq!(send_res as usize, total_len);

    let out_a = ArrayBuffer::new_zeroed(a.byte_len()).unwrap();
    let out_b = ArrayBuffer::new_zeroed(b.byte_len()).unwrap();
    let recv_ranges = vec![
      IoVecRange::whole_array_buffer(&out_a),
      IoVecRange::whole_array_buffer(&out_b),
    ];
    let recv_iov = PinnedIoVec::try_from_ranges(&recv_ranges).unwrap();
    let mut recv_hdr = PinnedMsgHdr::new(recv_iov);

    let sqe = opcode::RecvMsg::new(types::Fd(sock_b.0), recv_hdr.as_msghdr_mut_ptr())
      .flags(0)
      .build()
      .user_data(2);
    unsafe {
      ring
        .submission()
        .push(&sqe)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring submission queue is full"))?;
    }
    ring.submit_and_wait(1)?;

    let mut recv_res = None;
    for cqe in ring.completion() {
      if cqe.user_data() == 2 {
        recv_res = Some(cqe.result());
      }
    }
    let recv_res =
      recv_res.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing recvmsg cqe"))?;
    if recv_res < 0 {
      if recv_res == -libc::EINVAL || recv_res == -libc::EOPNOTSUPP {
        eprintln!("skipping: IORING_OP_RECVMSG not supported by kernel");
        return Ok(());
      }
      return Err(io::Error::from_raw_os_error(-recv_res).into());
    }
    assert_eq!(recv_res as usize, total_len);

    let out_a_bytes = unsafe { out_a.pin().unwrap().as_slice().to_vec() };
    let out_b_bytes = unsafe { out_b.pin().unwrap().as_slice().to_vec() };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&out_a_bytes);
    out.extend_from_slice(&out_b_bytes);
    assert_eq!(&out, b"hello world");

    // Ensure we still have access to the output metadata after completion.
    let _ = recv_hdr.msg_flags();

    Ok(())
  }

  #[test]
  fn recvmsg_reports_sender_name() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixDatagram;

    let dir = tempfile::Builder::new()
      .prefix("rn_iovec")
      .tempdir_in("/tmp")
      .or_else(|_| tempfile::tempdir())?;
    let path_a = dir.path().join("a.sock");
    let path_b = dir.path().join("b.sock");

    // Keep the filesystem socket paths safely under the platform's `sockaddr_un.sun_path` limit.
    let max_path_len = unsafe { std::mem::zeroed::<libc::sockaddr_un>().sun_path.len() };
    let path_a_bytes = path_a.as_os_str().as_bytes();
    let path_b_bytes = path_b.as_os_str().as_bytes();
    if path_a_bytes.len() >= max_path_len || path_b_bytes.len() >= max_path_len {
      eprintln!("skipping: unix socket path too long");
      return Ok(());
    }

    let sock_a = match UnixDatagram::bind(&path_a) {
      Ok(s) => s,
      Err(err) => {
        if err.raw_os_error() == Some(libc::ENAMETOOLONG) {
          eprintln!("skipping: unix socket path too long");
          return Ok(());
        }
        return Err(err.into());
      }
    };
    let sock_b = match UnixDatagram::bind(&path_b) {
      Ok(s) => s,
      Err(err) => {
        if err.raw_os_error() == Some(libc::ENAMETOOLONG) {
          eprintln!("skipping: unix socket path too long");
          return Ok(());
        }
        return Err(err.into());
      }
    };

    // Send a datagram using the stdlib API (sendto). We only need the recvmsg side to validate
    // `PinnedMsgHdr` name buffers.
    sock_a.send_to(b"hi", &path_b)?;

    let out = ArrayBuffer::new_zeroed(2).unwrap();
    let read_ranges = vec![IoVecRange::whole_array_buffer(&out)];
    let read_iov = PinnedIoVec::try_from_ranges(&read_ranges).unwrap();
    let name_buf = vec![0u8; std::mem::size_of::<libc::sockaddr_storage>()];
    let mut recv_hdr = PinnedMsgHdr::with_name(read_iov, name_buf);

    let nr = unsafe { libc::recvmsg(sock_b.as_raw_fd(), recv_hdr.as_msghdr_mut_ptr(), 0) };
    if nr < 0 {
      return Err(io::Error::last_os_error().into());
    }
    assert_eq!(nr as usize, 2);

    let out_bytes = unsafe { out.pin().unwrap().as_slice().to_vec() };
    assert_eq!(&out_bytes, b"hi");

    let name = recv_hdr.name().unwrap();
    assert!(recv_hdr.name_len() > 0);

    // The returned sockaddr bytes should contain the bound path somewhere in the payload.
    assert!(
      name
        .windows(path_a_bytes.len())
        .any(|w| w == path_a_bytes),
      "recvmsg name did not contain sender path; name={name:?} sender_path={path_a:?}"
    );

    Ok(())
  }
}
