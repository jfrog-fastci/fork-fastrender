//! Unix file-descriptor passing helpers (`SCM_RIGHTS`).
//!
//! This module is used for hardening IPC boundaries between trusted and untrusted processes.
//! The receiver implementation is intentionally defensive:
//! - It treats `MSG_CTRUNC` as a hard error.
//! - It validates ancillary data layout before reading it.
//! - It closes any received file descriptors on error to avoid leaks.
//! - On Linux/Android, it uses `recvmsg(MSG_CMSG_CLOEXEC)` so received descriptors are atomically
//!   marked `FD_CLOEXEC` (no exec-race FD leaks).

use std::io;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::io::FromRawFd;

#[cfg(unix)]
use thiserror::Error;

#[cfg(unix)]
const DEFAULT_DATA_BUF_LEN: usize = 4096;

/// Extra SCM_RIGHTS capacity beyond `max_fds` used by [`recv_msg`].
///
/// This lets the receiver deterministically detect protocol violations (too many passed fds)
/// without relying on `MSG_CTRUNC`.
#[cfg(unix)]
const EXTRA_FD_SLOP: usize = 8;

/// Absolute cap on SCM_RIGHTS fds we are willing to receive in a single `recvmsg` call.
///
/// The receiver closes any excess fds and returns [`RecvMsgError::TooManyFds`].
#[cfg(unix)]
const ABS_MAX_FDS_PER_MESSAGE: usize = 64;

#[cfg(unix)]
#[derive(Debug, Error)]
pub enum RecvMsgError {
  #[error("recvmsg failed")]
  Io(#[source] io::Error),

  #[error("recvmsg returned truncated ancillary data (MSG_CTRUNC)")]
  ControlTruncated,

  #[error("recvmsg returned truncated payload (MSG_TRUNC)")]
  PayloadTruncated,

  #[error("received {received} file descriptors but max allowed is {max}")]
  TooManyFds { received: usize, max: usize },

  #[error("malformed ancillary data (invalid cmsghdr layout)")]
  MalformedCmsg,

  #[error("malformed SCM_RIGHTS data length: {data_len} bytes")]
  MalformedRightsDataLen { data_len: usize },

  #[error("invalid SCM_RIGHTS fd value: {fd}")]
  InvalidFd { fd: RawFd },

  #[error("received SCM_RIGHTS file descriptors without any payload bytes")]
  FdWithoutPayload,
}

#[cfg(unix)]
#[derive(Debug)]
pub struct RecvMsg {
  pub bytes: Vec<u8>,
  pub fds: Vec<OwnedFd>,
}

#[cfg(unix)]
fn cmsg_align(len: usize) -> usize {
  let align = std::mem::size_of::<usize>();
  len.saturating_add(align - 1) & !(align - 1)
}

#[cfg(unix)]
fn cmsg_hdr_len() -> usize {
  cmsg_align(std::mem::size_of::<libc::cmsghdr>())
}

#[cfg(unix)]
fn cmsg_space(data_len: usize) -> usize {
  cmsg_hdr_len().saturating_add(cmsg_align(data_len))
}

#[cfg(unix)]
fn alloc_control_storage(control_len: usize) -> Vec<usize> {
  let word = std::mem::size_of::<usize>();
  let words = control_len.saturating_add(word - 1) / word;
  vec![0usize; words.max(1)]
}

#[cfg(unix)]
fn control_storage_ptr(storage: &mut [usize]) -> *mut libc::c_void {
  storage.as_mut_ptr().cast::<libc::c_void>()
}

#[cfg(unix)]
struct ParsedControl {
  fds: Vec<RawFd>,
  error: Option<RecvMsgError>,
}

#[cfg(unix)]
fn parse_control_for_fds(control: &[u8]) -> ParsedControl {
  let mut out = ParsedControl {
    fds: Vec::new(),
    error: None,
  };
  let hdr_len = cmsg_hdr_len();
  let mut offset = 0usize;
  while offset
    .checked_add(std::mem::size_of::<libc::cmsghdr>())
    .is_some_and(|end| end <= control.len())
  {
    let hdr_ptr = unsafe { control.as_ptr().add(offset) as *const libc::cmsghdr };
    // `cmsghdr` fields are written by the kernel; use unaligned reads defensively.
    let hdr = unsafe { std::ptr::read_unaligned(hdr_ptr) };
    let cmsg_len = hdr.cmsg_len as usize;
    if cmsg_len < hdr_len {
      out.error = Some(RecvMsgError::MalformedCmsg);
      break;
    }
    let end = match offset.checked_add(cmsg_len) {
      Some(end) => end,
      None => {
        out.error = Some(RecvMsgError::MalformedCmsg);
        break;
      }
    };
    if end > control.len() {
      out.error = Some(RecvMsgError::MalformedCmsg);
      break;
    }

    if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
      let data_len = cmsg_len.saturating_sub(hdr_len);
      if data_len % std::mem::size_of::<RawFd>() != 0 {
        out.error = Some(RecvMsgError::MalformedRightsDataLen { data_len });
        break;
      }
      let count = data_len / std::mem::size_of::<RawFd>();
      let data_offset = match offset.checked_add(hdr_len) {
        Some(offset) => offset,
        None => {
          out.error = Some(RecvMsgError::MalformedCmsg);
          break;
        }
      };
      if data_offset
        .checked_add(data_len)
        .map_or(true, |end| end > control.len())
      {
        out.error = Some(RecvMsgError::MalformedCmsg);
        break;
      }
      for i in 0..count {
        let fd_ptr = unsafe {
          control
            .as_ptr()
            .add(data_offset)
            .add(i * std::mem::size_of::<RawFd>())
            as *const RawFd
        };
        let fd = unsafe { std::ptr::read_unaligned(fd_ptr) };
        out.fds.push(fd);
      }
    }

    let next = match offset.checked_add(cmsg_align(cmsg_len)) {
      Some(next) => next,
      None => {
        out.error = Some(RecvMsgError::MalformedCmsg);
        break;
      }
    };
    if next <= offset {
      out.error = Some(RecvMsgError::MalformedCmsg);
      break;
    }
    offset = next;
  }
  out
}

#[cfg(unix)]
fn recv_msg_inner(
  sock_fd: RawFd,
  max_fds: usize,
  data_buf_len: usize,
  control_buf_len: usize,
) -> Result<RecvMsg, RecvMsgError> {
  let mut data_buf = vec![0u8; data_buf_len.max(1)];
  let mut iov = libc::iovec {
    iov_base: data_buf.as_mut_ptr().cast::<libc::c_void>(),
    iov_len: data_buf.len(),
  };

  let mut control_storage = alloc_control_storage(control_buf_len);
  let control_ptr = control_storage_ptr(&mut control_storage);

  let mut hdr = libc::msghdr {
    msg_name: std::ptr::null_mut(),
    msg_namelen: 0,
    msg_iov: &mut iov,
    msg_iovlen: 1,
    msg_control: control_ptr,
    msg_controllen: control_buf_len,
    msg_flags: 0,
  };

  // On Linux/Android we prefer MSG_CMSG_CLOEXEC so received fds are atomically FD_CLOEXEC. However,
  // some older kernels/sandboxed environments reject MSG_CMSG_CLOEXEC with EINVAL. In that case we
  // retry without the flag and then set FD_CLOEXEC manually.
  let mut need_manual_cloexec = false;
  #[cfg(any(target_os = "linux", target_os = "android"))]
  let read_len = loop {
    // `recvmsg` mutates `msg_controllen` on success. Ensure retries start with the full buffer.
    hdr.msg_controllen = control_buf_len;
    hdr.msg_flags = 0;

    // SAFETY: `recvmsg` writes into the provided iovec + control buffers. All pointers remain valid
    // for the duration of the call.
    let rc = unsafe { libc::recvmsg(sock_fd, &mut hdr, libc::MSG_CMSG_CLOEXEC) };
    if rc >= 0 {
      break rc;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    if err.raw_os_error() == Some(libc::EINVAL) {
      need_manual_cloexec = true;
      // Retry without MSG_CMSG_CLOEXEC.
      let rc2 = loop {
        hdr.msg_controllen = control_buf_len;
        hdr.msg_flags = 0;
        let rc2 = unsafe { libc::recvmsg(sock_fd, &mut hdr, 0) };
        if rc2 >= 0 {
          break rc2;
        }
        let err2 = io::Error::last_os_error();
        if err2.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(RecvMsgError::Io(err2));
      };
      break rc2;
    }
    return Err(RecvMsgError::Io(err));
  };
  #[cfg(not(any(target_os = "linux", target_os = "android")))]
  let read_len = loop {
    need_manual_cloexec = true;
    hdr.msg_controllen = control_buf_len;
    hdr.msg_flags = 0;
    // SAFETY: `recvmsg` writes into the provided iovec + control buffers. All pointers remain valid
    // for the duration of the call.
    let rc = unsafe { libc::recvmsg(sock_fd, &mut hdr, 0) };
    if rc >= 0 {
      break rc;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(RecvMsgError::Io(err));
  };

  let read_len = read_len as usize;
  data_buf.truncate(read_len);

  let flags = hdr.msg_flags;
  let controllen = (hdr.msg_controllen as usize).min(control_buf_len);

  let control_bytes = unsafe {
    std::slice::from_raw_parts(control_ptr.cast::<u8>(), controllen)
  };

  let parsed = parse_control_for_fds(control_bytes);
  let raw_fds = parsed.fds;
  let mut owned_fds = Vec::<OwnedFd>::with_capacity(raw_fds.len());
  for fd in raw_fds {
    if fd < 0 {
      return Err(RecvMsgError::InvalidFd { fd });
    }
    // SAFETY: fds returned in SCM_RIGHTS are newly-created, owned by this process.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    owned_fds.push(owned);
  }

  if need_manual_cloexec {
    for fd in &owned_fds {
      let flags = loop {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if flags >= 0 {
          break flags;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(RecvMsgError::Io(err));
      };
      if (flags & libc::FD_CLOEXEC) == 0 {
        loop {
          let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
          if rc >= 0 {
            break;
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(RecvMsgError::Io(err));
        }
      }
    }
  }

  // On any error after this point, dropping `owned_fds` closes all received descriptors.
  if (flags & libc::MSG_CTRUNC) != 0 {
    return Err(RecvMsgError::ControlTruncated);
  }
  if (flags & libc::MSG_TRUNC) != 0 {
    return Err(RecvMsgError::PayloadTruncated);
  }
  if let Some(err) = parsed.error {
    return Err(err);
  }
  if !owned_fds.is_empty() && data_buf.is_empty() {
    return Err(RecvMsgError::FdWithoutPayload);
  }
  if owned_fds.len() > max_fds {
    return Err(RecvMsgError::TooManyFds {
      received: owned_fds.len(),
      max: max_fds,
    });
  }

  Ok(RecvMsg {
    bytes: data_buf,
    fds: owned_fds,
  })
}

#[cfg(unix)]
pub fn recv_msg(sock_fd: RawFd, max_fds: usize) -> Result<RecvMsg, RecvMsgError> {
  // Allocate enough space to detect protocol violations without relying on MSG_CTRUNC.
  let cap_fds = max_fds
    .saturating_add(EXTRA_FD_SLOP)
    .min(ABS_MAX_FDS_PER_MESSAGE)
    .max(1);
  let mut control_len = cmsg_space(std::mem::size_of::<RawFd>() * cap_fds);

  // Linux `SO_PASSCRED` delivers an `SCM_CREDENTIALS` cmsg. Include space so that enabling it on a
  // test socket doesn't force a truncation error.
  #[cfg(target_os = "linux")]
  {
    control_len = control_len.saturating_add(cmsg_space(std::mem::size_of::<libc::ucred>()));
  }

  recv_msg_inner(sock_fd, max_fds, DEFAULT_DATA_BUF_LEN, control_len)
}

#[cfg(all(test, unix))]
pub(crate) fn recv_msg_with_control_buf(
  sock_fd: RawFd,
  max_fds: usize,
  control_buf_len: usize,
) -> Result<RecvMsg, RecvMsgError> {
  recv_msg_inner(sock_fd, max_fds, DEFAULT_DATA_BUF_LEN, control_buf_len)
}

#[cfg(all(test, target_os = "linux"))]
mod fd_passing_edge_cases {
  use super::*;
  use std::os::unix::io::{AsRawFd, IntoRawFd};
  use std::os::unix::net::UnixDatagram;

  fn make_pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: `pipe2` writes two fds on success.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(rc, 0, "pipe2 failed: {}", io::Error::last_os_error());
    // SAFETY: pipe2 returns owned fds.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
  }

  fn send_fds(sock: &UnixDatagram, payload: &[u8], fds: &[RawFd]) {
    let mut iov = libc::iovec {
      iov_base: payload.as_ptr().cast::<libc::c_void>().cast_mut(),
      iov_len: payload.len(),
    };

    let mut control_storage: Vec<usize> = Vec::new();
    let (control_ptr, control_len) = if fds.is_empty() {
      (std::ptr::null_mut(), 0usize)
    } else {
      let data_len = std::mem::size_of::<RawFd>() * fds.len();
      let control_len = cmsg_space(data_len);
      control_storage = alloc_control_storage(control_len);
      let control_ptr = control_storage_ptr(&mut control_storage);

      // Build a single SCM_RIGHTS cmsg.
      let hdr_len = cmsg_hdr_len();
      let cmsg_ptr = control_ptr.cast::<u8>().cast::<libc::cmsghdr>();
      // SAFETY: control buffer is at least `sizeof(cmsghdr)` bytes.
      unsafe {
        std::ptr::write(
          cmsg_ptr,
          libc::cmsghdr {
            cmsg_len: (hdr_len + data_len) as _,
            cmsg_level: libc::SOL_SOCKET,
            cmsg_type: libc::SCM_RIGHTS,
          },
        );
        let data_ptr = (control_ptr.cast::<u8>()).add(hdr_len).cast::<u8>();
        std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), data_ptr, data_len);
      }

      (control_ptr, control_len)
    };

    let mut hdr = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: &mut iov,
      msg_iovlen: 1,
      msg_control: control_ptr,
      msg_controllen: control_len,
      msg_flags: 0,
    };

    // SAFETY: sendmsg reads from iovec + control message buffers.
    let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &mut hdr, 0) };
    assert!(
      sent >= 0,
      "sendmsg failed: {}",
      io::Error::last_os_error()
    );
  }

  fn write_expect_epipe(fd: RawFd) {
    let buf = [0x55u8; 1];
    // SAFETY: `fd` is expected to be a valid pipe write end for the duration of this call.
    let rc = unsafe { libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
    assert_eq!(rc, -1, "expected write to fail");
    let err = io::Error::last_os_error();
    assert_eq!(
      err.raw_os_error(),
      Some(libc::EPIPE),
      "expected EPIPE, got {err:?}"
    );
  }

  #[test]
  fn received_fds_are_cloexec() {
    let (sender, receiver) = UnixDatagram::pair().expect("socketpair");

    let mut fds = [0; 2];
    // SAFETY: `pipe2` writes two fds on success.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), 0) };
    assert_eq!(rc, 0, "pipe2 failed: {}", io::Error::last_os_error());
    // SAFETY: pipe2 returns owned fds.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

    send_fds(&sender, b"z", &[read.as_raw_fd()]);
    drop(read);

    let msg = recv_msg(receiver.as_raw_fd(), 1).expect("recv_msg");
    assert_eq!(msg.fds.len(), 1);
    let got = msg.fds[0].as_raw_fd();

    let flags = unsafe { libc::fcntl(got, libc::F_GETFD) };
    assert!(flags >= 0, "fcntl(F_GETFD) failed: {}", io::Error::last_os_error());
    assert_ne!(
      flags & libc::FD_CLOEXEC,
      0,
      "expected received fd to have FD_CLOEXEC set"
    );

    drop(msg);
    drop(write);
  }

  #[test]
  fn control_truncation_sets_msg_ctrunc_and_errors() {
    let (sender, receiver) = UnixDatagram::pair().expect("socketpair");

    let mut pipes = Vec::new();
    let mut send_fds_vec = Vec::<RawFd>::new();
    for _ in 0..4 {
      let (read, write) = make_pipe();
      send_fds_vec.push(read.as_raw_fd());
      pipes.push((read, write));
    }

    send_fds(&sender, b"x", &send_fds_vec);

    // Provide a control buffer too small to hold 4 fds, but large enough for at least one.
    let small_control_len = cmsg_space(std::mem::size_of::<RawFd>() * 2);
    let err = recv_msg_with_control_buf(receiver.as_raw_fd(), 4, small_control_len).unwrap_err();
    assert!(
      matches!(err, RecvMsgError::ControlTruncated),
      "unexpected error: {err:?}"
    );

    drop(pipes);
  }

  #[test]
  fn too_many_fds_closes_extras_and_errors() {
    let (sender, receiver) = UnixDatagram::pair().expect("socketpair");

    let mut send_fds_vec = Vec::<RawFd>::new();
    let mut write_ends = Vec::<OwnedFd>::new();
    for _ in 0..5 {
      let (read, write) = make_pipe();
      send_fds_vec.push(read.into_raw_fd());
      write_ends.push(write);
    }

    send_fds(&sender, b"y", &send_fds_vec);
    // Close the sender copies of the read ends so only the received fds keep the pipes alive.
    for fd in send_fds_vec {
      // SAFETY: `fd` is owned by this scope after `into_raw_fd` above.
      unsafe {
        libc::close(fd);
      }
    }

    let err = recv_msg(receiver.as_raw_fd(), 4).unwrap_err();
    assert!(
      matches!(err, RecvMsgError::TooManyFds { received: 5, max: 4 }),
      "unexpected error: {err:?}"
    );

    // The extra fd (5th) must be closed by the receiver even though we errored.
    write_expect_epipe(write_ends[4].as_raw_fd());

    drop(write_ends);
  }

  #[test]
  fn ignores_non_scm_rights_cmsgs_from_passcred() {
    let (sender, receiver) = UnixDatagram::pair().expect("socketpair");

    let opt: libc::c_int = 1;
    // SAFETY: `setsockopt` args are valid for enabling SO_PASSCRED.
    let rc = unsafe {
      libc::setsockopt(
        receiver.as_raw_fd(),
        libc::SOL_SOCKET,
        libc::SO_PASSCRED,
        std::ptr::addr_of!(opt).cast::<libc::c_void>(),
        std::mem::size_of_val(&opt) as _,
      )
    };
    assert_eq!(rc, 0, "setsockopt(SO_PASSCRED) failed");

    sender.send(b"hello").expect("send payload");
    let msg = recv_msg(receiver.as_raw_fd(), 0).expect("recv_msg should ignore SCM_CREDENTIALS");
    assert_eq!(msg.bytes, b"hello");
    assert!(msg.fds.is_empty(), "unexpected fds: {:?}", msg.fds);
  }

  #[test]
  fn malformed_cmsg_is_rejected() {
    // Construct a control buffer with a cmsghdr that claims a length smaller than its header.
    let hdr_len = cmsg_hdr_len();
    let mut buf = vec![0u8; hdr_len.max(32)];
    let hdr = libc::cmsghdr {
      cmsg_len: (hdr_len.saturating_sub(1)) as _,
      cmsg_level: libc::SOL_SOCKET,
      cmsg_type: libc::SCM_RIGHTS,
    };
    unsafe {
      std::ptr::write_unaligned(buf.as_mut_ptr().cast::<libc::cmsghdr>(), hdr);
    }

    let parsed = parse_control_for_fds(&buf);
    let err = parsed.error.expect("expected parse error");
    assert!(
      matches!(err, RecvMsgError::MalformedCmsg),
      "unexpected error: {err:?}"
    );
  }
}
