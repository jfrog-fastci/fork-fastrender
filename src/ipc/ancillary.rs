//! File-descriptor passing over Unix domain sockets (SCM_RIGHTS).
//!
//! This is a very small wrapper around `sendmsg`/`recvmsg` used by the multiprocess IPC layer.
//! It is intentionally minimal: we only support sending/receiving a single file descriptor.
//!
//! # Safety
//!
//! The unsafe blocks in this module are limited to:
//! - building `msghdr`/`cmsghdr` structs for `sendmsg`/`recvmsg`
//! - interpreting the returned control message
//!
//! All public functions are panic-free and return `io::Result`.

use std::io;

#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
const fn cmsg_align(len: usize) -> usize {
  // cmsg alignment matches CMSG_ALIGN in glibc: align to sizeof(size_t).
  let align = std::mem::size_of::<usize>();
  (len + align - 1) & !(align - 1)
}

#[cfg(unix)]
const fn cmsg_space(data_len: usize) -> usize {
  cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
}

#[cfg(unix)]
const fn cmsg_len(data_len: usize) -> usize {
  cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + data_len
}

/// Send a single file descriptor over a connected Unix domain socket.
#[cfg(unix)]
pub fn send_fd(sock: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
  const PAYLOAD: [u8; 1] = [0u8];
  const FD_LEN: usize = std::mem::size_of::<RawFd>();
  const CONTROL_LEN: usize = cmsg_space(FD_LEN);

  #[repr(C)]
  union ControlBuf {
    _hdr: libc::cmsghdr,
    buf: [u8; CONTROL_LEN],
  }

  let mut iov = libc::iovec {
    iov_base: PAYLOAD.as_ptr() as *mut libc::c_void,
    iov_len: PAYLOAD.len(),
  };

  let mut control = ControlBuf { buf: [0u8; CONTROL_LEN] };

  let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
  msg.msg_iov = std::ptr::addr_of_mut!(iov);
  msg.msg_iovlen = 1;
  msg.msg_control = unsafe { control.buf.as_mut_ptr() as *mut libc::c_void };
  msg.msg_controllen = CONTROL_LEN;

  // SAFETY: `msg.msg_control` points at `control.buf` which is aligned for `cmsghdr` due to the
  // union. The buffer is large enough for one `SCM_RIGHTS` cmsg with a single FD.
  unsafe {
    let cmsg = msg.msg_control as *mut libc::cmsghdr;
    (*cmsg).cmsg_level = libc::SOL_SOCKET;
    (*cmsg).cmsg_type = libc::SCM_RIGHTS;
    (*cmsg).cmsg_len = cmsg_len(FD_LEN) as _;

    let data_ptr = (cmsg as *mut u8).add(cmsg_align(std::mem::size_of::<libc::cmsghdr>()))
      as *mut RawFd;
    std::ptr::write_unaligned(data_ptr, fd.as_raw_fd());
  }

  // SAFETY: `sendmsg` reads the iov and control buffers for the duration of the call. They are
  // stack-allocated and remain valid.
  #[cfg(any(target_os = "linux", target_os = "android"))]
  let flags = libc::MSG_NOSIGNAL;
  #[cfg(not(any(target_os = "linux", target_os = "android")))]
  let flags = 0;

  loop {
    let rc = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, flags) };
    if rc < 0 {
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
    if rc as usize != PAYLOAD.len() {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "short sendmsg while sending fd",
      ));
    }
    return Ok(());
  }
}

/// Receive a single file descriptor over a connected Unix domain socket.
///
/// On Linux this uses `MSG_CMSG_CLOEXEC` so the received fd is marked close-on-exec.
#[cfg(unix)]
pub fn recv_fd(sock: &UnixStream) -> io::Result<OwnedFd> {
  let mut payload = [0u8; 1];
  const FD_LEN: usize = std::mem::size_of::<RawFd>();
  const CONTROL_LEN: usize = cmsg_space(FD_LEN);

  #[repr(C)]
  union ControlBuf {
    _hdr: libc::cmsghdr,
    buf: [u8; CONTROL_LEN],
  }

  let mut iov = libc::iovec {
    iov_base: payload.as_mut_ptr() as *mut libc::c_void,
    iov_len: payload.len(),
  };

  let mut control = ControlBuf { buf: [0u8; CONTROL_LEN] };

  let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
  msg.msg_iov = std::ptr::addr_of_mut!(iov);
  msg.msg_iovlen = 1;
  msg.msg_control = unsafe { control.buf.as_mut_ptr() as *mut libc::c_void };
  msg.msg_controllen = CONTROL_LEN;

  #[cfg(any(target_os = "linux", target_os = "android"))]
  let flags = libc::MSG_CMSG_CLOEXEC;
  #[cfg(not(any(target_os = "linux", target_os = "android")))]
  let flags = 0;

  // SAFETY: `recvmsg` writes into the provided iov/control buffers which are valid for the call.
  let mut need_manual_cloexec = false;
  let read_len = loop {
    // `recvmsg` mutates `msg_controllen` on success. Ensure retries start with the full buffer.
    msg.msg_controllen = CONTROL_LEN;
    msg.msg_flags = 0;
    let rc = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags) };
    if rc >= 0 {
      break rc as usize;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    // Some older kernels/sandboxed environments reject MSG_CMSG_CLOEXEC with EINVAL. Retry without
    // the flag and set FD_CLOEXEC manually on the received fd.
    if err.raw_os_error() == Some(libc::EINVAL) && (flags & libc::MSG_CMSG_CLOEXEC) != 0 {
      need_manual_cloexec = true;
      loop {
        msg.msg_controllen = CONTROL_LEN;
        msg.msg_flags = 0;
        let rc2 = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags & !libc::MSG_CMSG_CLOEXEC) };
        if rc2 >= 0 {
          break rc2 as usize;
        }
        let err2 = io::Error::last_os_error();
        if err2.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err2);
      }
    } else {
      return Err(err);
    }
  };

  if read_len == 0 {
    return Err(io::Error::new(
      io::ErrorKind::UnexpectedEof,
      "peer closed socket while receiving fd",
    ));
  }

  if msg.msg_controllen < cmsg_len(FD_LEN) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "missing SCM_RIGHTS control message",
    ));
  }

  // SAFETY: `msg.msg_control` points at `control.buf`, which is aligned for `cmsghdr`.
  let received_fd = unsafe {
    let cmsg = msg.msg_control as *const libc::cmsghdr;
    if cmsg.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing SCM_RIGHTS control message",
      ));
    }
    if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "unexpected control message while receiving fd",
      ));
    }
    if (*cmsg).cmsg_len < cmsg_len(FD_LEN) as _ {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "short SCM_RIGHTS control message",
      ));
    }
    let data_ptr = (cmsg as *const u8).add(cmsg_align(std::mem::size_of::<libc::cmsghdr>()))
      as *const RawFd;
    std::ptr::read_unaligned(data_ptr)
  };

  if received_fd < 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "received negative fd via SCM_RIGHTS",
    ));
  }

  // SAFETY: `received_fd` came from the kernel via SCM_RIGHTS; we now own it.
  let owned = unsafe { OwnedFd::from_raw_fd(received_fd) };
  // Check truncation after parsing so any received fds are reliably closed on error.
  if (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
    drop(owned);
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "control message truncated while receiving fd",
    ));
  }
  if (msg.msg_flags & libc::MSG_TRUNC) != 0 {
    drop(owned);
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "payload truncated while receiving fd",
    ));
  }
  if need_manual_cloexec {
    let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
      return Err(io::Error::last_os_error());
    }
    if (flags & libc::FD_CLOEXEC) == 0 {
      let rc = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
      if rc < 0 {
        return Err(io::Error::last_os_error());
      }
    }
  }
  Ok(owned)
}
