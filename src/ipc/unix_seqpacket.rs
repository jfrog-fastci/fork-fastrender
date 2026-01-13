//! Unix-domain `SOCK_SEQPACKET` IPC with optional `SCM_RIGHTS` fd passing (Linux).
//!
//! We use `SOCK_SEQPACKET` so message boundaries are preserved (no ad-hoc framing needed) and fd
//! passing is naturally atomic with the payload.

#![cfg(target_os = "linux")]

use std::io;
use std::mem;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

#[derive(Debug, thiserror::Error)]
pub enum FdPassingError {
  #[error("invalid input: {reason}")]
  InvalidInput { reason: &'static str },
  #[error("sendmsg failed")]
  SendmsgFailed {
    #[source]
    source: io::Error,
  },
  #[error("sendmsg wrote {written} bytes, expected {expected}")]
  ShortSend { written: usize, expected: usize },
  #[error("recvmsg failed")]
  RecvmsgFailed {
    #[source]
    source: io::Error,
  },
  #[error("peer closed the socket (EOF)")]
  UnexpectedEof,
  #[error("failed to set FD_CLOEXEC on received fd {fd}")]
  SetCloexecFailed {
    fd: RawFd,
    #[source]
    source: io::Error,
  },
  #[error("message was truncated (msg_flags={msg_flags:#x})")]
  Truncated { msg_flags: i32 },
  #[error("unexpected ancillary data (level={level}, type={ty})")]
  UnexpectedCmsg { level: i32, ty: i32 },
  #[error("malformed SCM_RIGHTS cmsg_len={cmsg_len} (expected header + N*sizeof(int))")]
  MalformedRightsCmsg { cmsg_len: usize },
  #[error("too many file descriptors received: {received} > max {max}")]
  TooManyFds { received: usize, max: usize },
}

/// A Unix-domain `SOCK_SEQPACKET` socket.
#[derive(Debug)]
pub struct UnixSeqpacket {
  fd: OwnedFd,
}

impl UnixSeqpacket {
  /// Create a connected socket pair using `socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, ...)`.
  pub fn pair() -> io::Result<(UnixSeqpacket, UnixSeqpacket)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `socketpair` initializes `fds` on success.
    let rc = unsafe {
      libc::socketpair(
        libc::AF_UNIX,
        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
        0,
        fds.as_mut_ptr(),
      )
    };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `socketpair` returns new fds on success.
    let a = unsafe { OwnedFd::from_raw_fd(fds[0] as RawFd) };
    let b = unsafe { OwnedFd::from_raw_fd(fds[1] as RawFd) };
    Ok((UnixSeqpacket { fd: a }, UnixSeqpacket { fd: b }))
  }

  /// Send one atomic seqpacket message containing `bytes` and optional `SCM_RIGHTS` fds.
  ///
  /// - Uses a single `sendmsg` call (atomic message boundary).
  /// - Uses `MSG_NOSIGNAL` to avoid SIGPIPE when the peer is closed.
  /// - Retries on `EINTR`.
  pub fn send_msg(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> Result<(), FdPassingError> {
    if bytes.is_empty() {
      // Disallow empty payload messages. This ensures:
      // - `recvmsg` returning 0 bytes is unambiguous EOF for callers, and
      // - `SCM_RIGHTS` is never sent without a byte payload (see `unix(7)`).
      return Err(FdPassingError::InvalidInput {
        reason: "seqpacket messages must contain at least one byte of payload data",
      });
    }

    let iov = libc::iovec {
      iov_base: bytes.as_ptr() as *mut libc::c_void,
      iov_len: bytes.len(),
    };

    let mut control_storage;
    let (msg_control, msg_controllen) = if fds.is_empty() {
      (ptr::null_mut(), 0)
    } else {
      let data_len = fds
        .len()
        .checked_mul(mem::size_of::<libc::c_int>())
        .ok_or(FdPassingError::InvalidInput {
          reason: "fd list is too large",
        })?;
      let space = cmsg_space(data_len).ok_or(FdPassingError::InvalidInput {
        reason: "fd list is too large",
      })?;
      let cmsg_len = cmsg_len(data_len).ok_or(FdPassingError::InvalidInput {
        reason: "fd list is too large",
      })?;

      control_storage = AlignedBytes::zeroed(space);

      // SAFETY: `control_storage` is suitably aligned for `cmsghdr` (and we sized it using
      // `cmsg_space`).
      unsafe {
        let cmsg = control_storage.as_mut_ptr().cast::<libc::cmsghdr>();
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = cmsg_len;

        let data_ptr = (control_storage.as_mut_ptr().add(cmsg_header_len()))
          .cast::<libc::c_int>();
        for (idx, fd) in fds.iter().enumerate() {
          // SAFETY: `data_ptr` points into `control_storage` and we allocated enough space for all
          // fds.
          *data_ptr.add(idx) = fd.as_raw_fd();
        }
      }

      (
        control_storage.as_mut_ptr().cast::<libc::c_void>(),
        space,
      )
    };

    let hdr = libc::msghdr {
      msg_name: ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: (&iov as *const libc::iovec).cast_mut(),
      msg_iovlen: 1,
      msg_control,
      msg_controllen,
      msg_flags: 0,
    };

    loop {
      // SAFETY: `hdr` points to valid iov/control buffers for the duration of the call.
      let rc = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
      if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(FdPassingError::SendmsgFailed { source: err });
      }
      let written = rc as usize;
      if written != bytes.len() {
        return Err(FdPassingError::ShortSend {
          written,
          expected: bytes.len(),
        });
      }
      return Ok(());
    }
  }

  /// Receive one atomic seqpacket message containing up to `max_bytes` bytes and up to `max_fds`
  /// file descriptors.
  ///
  /// This uses `recvmsg(..., MSG_CMSG_CLOEXEC)` so any received fds are `FD_CLOEXEC`.
  ///
  /// If the kernel/libc rejects `MSG_CMSG_CLOEXEC` (observed as `EINVAL` on some older or
  /// sandbox-restricted environments), this retries without the flag and sets `FD_CLOEXEC` on the
  /// received fds via `fcntl`.
  pub fn recv_msg(
    &self,
    max_bytes: usize,
    max_fds: usize,
  ) -> Result<(Vec<u8>, Vec<OwnedFd>), FdPassingError> {
    if max_bytes == 0 {
      return Err(FdPassingError::InvalidInput {
        reason: "max_bytes must be non-zero",
      });
    }
    self.recv_msg_impl(max_bytes, max_fds, None)
  }

  /// Internal recvmsg implementation with a test-only flags override.
  ///
  /// When `flags_override` is `Some`, that value is used for the initial `recvmsg` call instead of
  /// `MSG_CMSG_CLOEXEC`. This allows unit tests to force the "manual CLOEXEC" path even on kernels
  /// that support `MSG_CMSG_CLOEXEC`.
  fn recv_msg_impl(
    &self,
    max_bytes: usize,
    max_fds: usize,
    flags_override: Option<libc::c_int>,
  ) -> Result<(Vec<u8>, Vec<OwnedFd>), FdPassingError> {
    if max_bytes == 0 {
      return Err(FdPassingError::InvalidInput {
        reason: "max_bytes must be non-zero",
      });
    }
    let mut bytes = vec![0u8; max_bytes];
    let mut iov = libc::iovec {
      iov_base: bytes.as_mut_ptr().cast::<libc::c_void>(),
      iov_len: bytes.len(),
    };

    // Allocate room for `max_fds + 1` so we can detect over-limit fds without relying on
    // `MSG_CTRUNC` (and close the extra fd immediately).
    let detect_fds = max_fds.saturating_add(1).max(1);
    let fd_bytes = detect_fds
      .checked_mul(mem::size_of::<libc::c_int>())
      .ok_or(FdPassingError::InvalidInput {
        reason: "max_fds is too large",
      })?;
    let control_len = cmsg_space(fd_bytes).ok_or(FdPassingError::InvalidInput {
      reason: "max_fds is too large",
    })?;
    let mut control_storage = AlignedBytes::zeroed(control_len);

    let mut hdr = libc::msghdr {
      msg_name: ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: (&mut iov as *mut libc::iovec),
      msg_iovlen: 1,
      msg_control: control_storage.as_mut_ptr().cast::<libc::c_void>(),
      msg_controllen: control_len,
      msg_flags: 0,
    };

    let read_len: usize;
    let mut need_manual_cloexec = false;
    loop {
      // `recvmsg` mutates `msg_controllen` on success. Ensure retries start with the full buffer.
      hdr.msg_controllen = control_len;
      hdr.msg_flags = 0;

      // SAFETY: `hdr` points to valid iov/control buffers for the duration of the call.
      let flags = flags_override.unwrap_or(libc::MSG_CMSG_CLOEXEC);
      let rc = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut hdr, flags) };
      if rc >= 0 {
        read_len = rc as usize;
        if (flags & libc::MSG_CMSG_CLOEXEC) == 0 {
          need_manual_cloexec = true;
        }
        break;
      }

      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }

      // Runtime fallback: some environments reject MSG_CMSG_CLOEXEC with EINVAL.
      if err.raw_os_error() == Some(libc::EINVAL) && (flags & libc::MSG_CMSG_CLOEXEC) != 0 {
        need_manual_cloexec = true;
        let rc = loop {
          hdr.msg_controllen = control_len;
          hdr.msg_flags = 0;
          let rc = unsafe {
            libc::recvmsg(
              self.fd.as_raw_fd(),
              &mut hdr,
              flags & !libc::MSG_CMSG_CLOEXEC,
            )
          };
          if rc >= 0 {
            break rc;
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(FdPassingError::RecvmsgFailed { source: err });
        };
        read_len = rc as usize;
        break;
      }

      return Err(FdPassingError::RecvmsgFailed { source: err });
    }

    bytes.truncate(read_len);

    let mut received_fds = Vec::<OwnedFd>::new();
    let mut extra_fds = 0usize;
    let mut protocol_error: Option<FdPassingError> = None;

    let control_used = std::cmp::min(hdr.msg_controllen, control_len);
    let control_slice = control_storage.as_slice(control_used);
    parse_control_messages(
      control_slice,
      max_fds,
      &mut received_fds,
      &mut extra_fds,
      &mut protocol_error,
    );

    if extra_fds > 0 {
      let received = max_fds.saturating_add(extra_fds);
      return Err(FdPassingError::TooManyFds { received, max: max_fds });
    }
    if let Some(err) = protocol_error {
      return Err(err);
    }

    if (hdr.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC)) != 0 {
      return Err(FdPassingError::Truncated {
        msg_flags: hdr.msg_flags,
      });
    }

    if read_len == 0 {
      return Err(FdPassingError::UnexpectedEof);
    }

    if need_manual_cloexec {
      for fd in &received_fds {
        let raw = fd.as_raw_fd();
        let current = loop {
          // SAFETY: `fcntl` is called with a valid file descriptor.
          let current = unsafe { libc::fcntl(raw, libc::F_GETFD) };
          if current >= 0 {
            break current;
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(FdPassingError::SetCloexecFailed { fd: raw, source: err });
        };
        loop {
          // SAFETY: `fcntl` is called with a valid file descriptor.
          let rc = unsafe { libc::fcntl(raw, libc::F_SETFD, current | libc::FD_CLOEXEC) };
          if rc >= 0 {
            break;
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(FdPassingError::SetCloexecFailed { fd: raw, source: err });
        }
      }
    }

    Ok((bytes, received_fds))
  }
}

impl AsFd for UnixSeqpacket {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.fd.as_fd()
  }
}

fn cmsg_align(len: usize) -> Option<usize> {
  let align = mem::size_of::<usize>();
  let added = len.checked_add(align - 1)?;
  Some(added & !(align - 1))
}

fn cmsg_header_len() -> usize {
  // `cmsghdr` is tiny; this cannot overflow.
  cmsg_align(mem::size_of::<libc::cmsghdr>()).expect("cmsghdr align overflow") // fastrender-allow-unwrap
}

fn cmsg_len(data_len: usize) -> Option<usize> {
  cmsg_header_len().checked_add(data_len)
}

fn cmsg_space(data_len: usize) -> Option<usize> {
  let aligned = cmsg_align(data_len)?;
  cmsg_header_len().checked_add(aligned)
}

/// Byte storage aligned for use as a control message buffer (`cmsghdr` alignment).
#[derive(Debug)]
struct AlignedBytes {
  words: Vec<usize>,
}

impl AlignedBytes {
  fn zeroed(len_bytes: usize) -> AlignedBytes {
    let word_bytes = mem::size_of::<usize>();
    let words = if len_bytes == 0 {
      0
    } else {
      // Round up without overflowing.
      (len_bytes - 1) / word_bytes + 1
    };
    AlignedBytes {
      words: vec![0usize; words],
    }
  }

  fn as_mut_ptr(&mut self) -> *mut u8 {
    self.words.as_mut_ptr().cast::<u8>()
  }

  fn as_slice(&self, len_bytes: usize) -> &[u8] {
    // SAFETY: the backing allocation is at least `len_bytes` bytes (rounded up to `usize` words).
    unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), len_bytes) }
  }
}

fn parse_control_messages(
  control: &[u8],
  max_fds: usize,
  out_fds: &mut Vec<OwnedFd>,
  extra_fds: &mut usize,
  protocol_error: &mut Option<FdPassingError>,
) {
  let hdr_len = cmsg_header_len();
  let mut offset = 0usize;
  while offset + hdr_len <= control.len() {
    // SAFETY: bounds checked above.
    let hdr_ptr = unsafe { control.as_ptr().add(offset).cast::<libc::cmsghdr>() };
    // SAFETY: `cmsghdr` is plain old data; we may receive unaligned buffers so use unaligned read.
    let hdr = unsafe { ptr::read_unaligned(hdr_ptr) };
    let cmsg_len = hdr.cmsg_len as usize;
    if cmsg_len < hdr_len {
      *protocol_error = Some(FdPassingError::MalformedRightsCmsg { cmsg_len });
      return;
    }

    let Some(aligned_len) = cmsg_align(cmsg_len) else {
      *protocol_error = Some(FdPassingError::MalformedRightsCmsg { cmsg_len });
      return;
    };
    if aligned_len == 0 || offset + aligned_len > control.len() {
      *protocol_error = Some(FdPassingError::MalformedRightsCmsg { cmsg_len });
      return;
    }

    if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
      let data_len = cmsg_len - hdr_len;
      if data_len % mem::size_of::<libc::c_int>() != 0 {
        *protocol_error = Some(FdPassingError::MalformedRightsCmsg { cmsg_len });
        return;
      }
      let fd_count = data_len / mem::size_of::<libc::c_int>();
      let data_ptr = unsafe { control.as_ptr().add(offset + hdr_len).cast::<libc::c_int>() };
      for idx in 0..fd_count {
        // SAFETY: `data_ptr` points inside `control` and we bounds-checked `data_len` above.
        let raw = unsafe { ptr::read_unaligned(data_ptr.add(idx)) };
        if raw < 0 {
          *protocol_error = Some(FdPassingError::MalformedRightsCmsg { cmsg_len });
          continue;
        }
        // SAFETY: the fd value came from the kernel via `recvmsg(SCM_RIGHTS)`, so it's owned by us.
        let owned = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
        if out_fds.len() < max_fds {
          out_fds.push(owned);
        } else {
          // Close immediately to avoid leaking fds when callers set a lower `max_fds`.
          drop(owned);
          *extra_fds += 1;
        }
      }
    } else {
      // Keep parsing so we can close any SCM_RIGHTS fds that appear later.
      if protocol_error.is_none() {
        *protocol_error = Some(FdPassingError::UnexpectedCmsg {
          level: hdr.cmsg_level,
          ty: hdr.cmsg_type,
        });
      }
    }

    offset += aligned_len;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::ffi::CString;
  use std::fs;
  use std::io::Write;
  use std::sync::Mutex;

  static TEST_LOCK: Mutex<()> = Mutex::new(());

  #[derive(Debug)]
  struct SharedMemory {
    fd: OwnedFd,
    len: usize,
  }

  impl SharedMemory {
    fn new(contents: &[u8]) -> io::Result<SharedMemory> {
      let name = CString::new("fastrender_unix_seqpacket_test").expect("CString");
      // SAFETY: `memfd_create` is a Linux syscall; name pointer is NUL-terminated.
      let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
      if raw < 0 {
        return Err(io::Error::last_os_error());
      }
      let len = contents.len();
      // SAFETY: `ftruncate` takes an owned fd; we close on error below.
      let rc = unsafe { libc::ftruncate(raw, len as libc::off_t) };
      if rc != 0 {
        let err = io::Error::last_os_error();
        // SAFETY: `raw` is valid when `memfd_create` succeeds.
        unsafe {
          libc::close(raw);
        }
        return Err(err);
      }

      // SAFETY: `raw` is owned and valid.
      let owned = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
      let mut file = fs::File::from(owned);
      file.write_all(contents)?;
      let fd: OwnedFd = file.into();

      Ok(SharedMemory { fd, len })
    }

    fn as_fd(&self) -> BorrowedFd<'_> {
      self.fd.as_fd()
    }

    fn len(&self) -> usize {
      self.len
    }
  }

  fn count_open_fds() -> usize {
    fs::read_dir("/proc/self/fd")
      .expect("read /proc/self/fd")
      .count()
  }

  #[test]
  fn unix_seqpacket_roundtrip_and_cloexec() {
    let _guard = TEST_LOCK.lock().unwrap();

    let (tx, rx) = UnixSeqpacket::pair().expect("socketpair");

    let shm_payload = b"hello from shm";
    let shm = SharedMemory::new(shm_payload).expect("memfd");

    let msg = b"control-bytes";
    tx
      .send_msg(msg, &[shm.as_fd()])
      .expect("send_msg succeeds");

    let (got_msg, mut fds) = rx.recv_msg(1024, 1).expect("recv_msg succeeds");
    assert_eq!(got_msg, msg);
    assert_eq!(fds.len(), 1);

    let recv_fd = fds.pop().unwrap();
    let flags = unsafe { libc::fcntl(recv_fd.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "fcntl(F_GETFD) failed: {:?}", io::Error::last_os_error());
    assert_ne!(
      flags & libc::FD_CLOEXEC,
      0,
      "expected received fd to have FD_CLOEXEC set"
    );

    // SAFETY: mmap args are validated; we unmap immediately.
    unsafe {
      let ptr = libc::mmap(
        ptr::null_mut(),
        shm.len(),
        libc::PROT_READ,
        libc::MAP_SHARED,
        recv_fd.as_raw_fd(),
        0,
      );
      assert_ne!(ptr, libc::MAP_FAILED, "mmap failed: {:?}", io::Error::last_os_error());
      let slice = std::slice::from_raw_parts(ptr.cast::<u8>(), shm.len());
      assert_eq!(slice, shm_payload);
      libc::munmap(ptr, shm.len());
    }
  }

  #[test]
  fn unix_seqpacket_send_rejects_fd_only_messages() {
    let (tx, _rx) = UnixSeqpacket::pair().expect("socketpair");

    let mut fds = [0i32; 2];
    // SAFETY: `pipe2` initializes `fds` on success.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(rc, 0, "pipe2 failed: {}", io::Error::last_os_error());

    // SAFETY: `pipe2` returns owned fds.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    let err = tx.send_msg(&[], &[read.as_fd()]).unwrap_err();
    assert!(
      matches!(err, FdPassingError::InvalidInput { .. }),
      "unexpected error: {err:?}"
    );

    drop(write);
  }

  #[test]
  fn unix_seqpacket_recv_too_many_fds_closes_and_errors() {
    let _guard = TEST_LOCK.lock().unwrap();

    let (tx, rx) = UnixSeqpacket::pair().expect("socketpair");
    let shm1 = SharedMemory::new(b"one").expect("memfd one");
    let shm2 = SharedMemory::new(b"two").expect("memfd two");

    tx
      .send_msg(b"x", &[shm1.as_fd(), shm2.as_fd()])
      .expect("send_msg succeeds");

    let before = count_open_fds();
    let res = rx.recv_msg(16, 1);
    assert!(
      matches!(res, Err(FdPassingError::TooManyFds { .. })),
      "expected TooManyFds error, got {res:?}"
    );
    let after = count_open_fds();
    assert_eq!(after, before, "expected recv_msg error to not leak fds");
  }

  #[test]
  fn recvmsg_cloexec_fallback() {
    let _guard = TEST_LOCK.lock().unwrap();

    let (tx, rx) = UnixSeqpacket::pair().expect("socketpair");

    let shm_payload = b"hello from shm";
    let shm = SharedMemory::new(shm_payload).expect("memfd");

    tx
      .send_msg(b"control", &[shm.as_fd()])
      .expect("send_msg succeeds");

    // Force the fallback path by skipping MSG_CMSG_CLOEXEC on the initial recvmsg call.
    let (_msg, mut fds) = rx
      .recv_msg_impl(1024, 1, Some(0))
      .expect("recv_msg fallback succeeds");
    assert_eq!(fds.len(), 1);

    let recv_fd = fds.pop().unwrap();
    let flags = unsafe { libc::fcntl(recv_fd.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "fcntl(F_GETFD) failed: {:?}", io::Error::last_os_error());
    assert_ne!(
      flags & libc::FD_CLOEXEC,
      0,
      "expected received fd to have FD_CLOEXEC set (fallback path)"
    );
  }

  #[test]
  fn send_empty_payload_is_rejected() {
    let _guard = TEST_LOCK.lock().unwrap();

    let (tx, _rx) = UnixSeqpacket::pair().expect("socketpair");
    let res = tx.send_msg(&[], &[]);
    assert!(
      matches!(res, Err(FdPassingError::InvalidInput { .. })),
      "expected InvalidInput for empty payload, got {res:?}"
    );
  }

  #[test]
  fn recv_eof_is_reported() {
    let _guard = TEST_LOCK.lock().unwrap();

    let (tx, rx) = UnixSeqpacket::pair().expect("socketpair");
    drop(tx);
    let res = rx.recv_msg(16, 0);
    assert!(
      matches!(res, Err(FdPassingError::UnexpectedEof)),
      "expected UnexpectedEof, got {res:?}"
    );
  }
}
