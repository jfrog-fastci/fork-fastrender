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

  // Parse and wrap any SCM_RIGHTS fds *before* checking MSG_CTRUNC/MSG_TRUNC so we reliably close
  // them on protocol errors (fd leak defense).
  let control_len = (msg.msg_controllen as usize).min(CONTROL_LEN);
  let start = msg.msg_control as usize;
  let end = start.checked_add(control_len).ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      "control buffer length overflow while receiving fd",
    )
  })?;

  let header_aligned = cmsg_align(std::mem::size_of::<libc::cmsghdr>());
  let mut fds_out: Vec<OwnedFd> = Vec::new();
  let mut protocol_error: Option<io::Error> = None;

  if control_len > 0 {
    let mut cmsg_ptr = msg.msg_control as *const libc::cmsghdr;
    while (cmsg_ptr as usize)
      .checked_add(std::mem::size_of::<libc::cmsghdr>())
      .is_some_and(|next| next <= end)
    {
      // SAFETY: bounds checked above.
      let cmsg = unsafe { &*cmsg_ptr };
      let cmsg_len_raw = cmsg.cmsg_len as usize;
      if cmsg_len_raw < header_aligned {
        protocol_error = Some(io::Error::new(
          io::ErrorKind::InvalidData,
          "malformed control message while receiving fd",
        ));
        break;
      }

      let Some(cmsg_end) = (cmsg_ptr as usize).checked_add(cmsg_len_raw) else {
        protocol_error = Some(io::Error::new(
          io::ErrorKind::InvalidData,
          "control message length overflow while receiving fd",
        ));
        break;
      };
      if cmsg_end > end {
        protocol_error = Some(io::Error::new(
          io::ErrorKind::InvalidData,
          "control message length out of bounds while receiving fd",
        ));
        break;
      }

      if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
        let data_len = cmsg_len_raw - header_aligned;
        if data_len % std::mem::size_of::<RawFd>() != 0 {
          protocol_error = Some(io::Error::new(
            io::ErrorKind::InvalidData,
            "misaligned SCM_RIGHTS payload while receiving fd",
          ));
          break;
        }
        let fd_count = data_len / std::mem::size_of::<RawFd>();
        let data_ptr = (cmsg_ptr as *const u8).wrapping_add(header_aligned) as *const RawFd;
        // SAFETY: data_ptr points into the received control buffer; fd_count is bounds checked.
        let fd_slice = unsafe { std::slice::from_raw_parts(data_ptr, fd_count) };
        for &fd in fd_slice {
          if fd < 0 {
            protocol_error = Some(io::Error::new(
              io::ErrorKind::InvalidData,
              "received negative fd via SCM_RIGHTS",
            ));
            break;
          }
          // SAFETY: fds returned via SCM_RIGHTS are now owned by the receiver.
          fds_out.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
      } else {
        // Keep parsing so any SCM_RIGHTS fds appearing later are closed.
        if protocol_error.is_none() {
          protocol_error = Some(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected control message while receiving fd",
          ));
        }
      }

      let next = (cmsg_ptr as usize).checked_add(cmsg_align(cmsg_len_raw));
      let Some(next) = next else { break };
      if next <= cmsg_ptr as usize {
        break;
      }
      cmsg_ptr = next as *const libc::cmsghdr;
    }
  }

  if let Some(err) = protocol_error {
    drop(fds_out);
    return Err(err);
  }

  if read_len == 0 {
    // EOF is a valid outcome when the peer closed the socket without sending a message.
    // However, be defensive: if we somehow received fds with a zero-length payload, treat it as a
    // protocol violation and ensure all fds are closed.
    if fds_out.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "peer closed socket while receiving fd",
      ));
    }
    drop(fds_out);
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "received SCM_RIGHTS file descriptor without payload bytes",
    ));
  }

  if fds_out.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "missing SCM_RIGHTS control message",
    ));
  }

  if fds_out.len() != 1 {
    drop(fds_out);
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "expected exactly one fd via SCM_RIGHTS",
    ));
  }

  let owned = fds_out.pop().expect("checked len == 1");
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
    let flags = loop {
      let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) };
      if flags >= 0 {
        break flags;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    };
    if (flags & libc::FD_CLOEXEC) == 0 {
      loop {
        let rc = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if rc >= 0 {
          break;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      }
    }
  }
  Ok(owned)
}
