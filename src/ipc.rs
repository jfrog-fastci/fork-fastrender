//! Low-level Linux IPC primitives used for multiprocess architecture.
//!
//! This module is intentionally small and syscall-focused so we can keep tight control over:
//! - portability across older glibc/kernel combinations
//! - `FD_CLOEXEC` behavior (avoid leaking IPC fds into spawned helpers)
//! - tricky `SCM_RIGHTS` message patterns (e.g. fds with an empty payload)

use std::ffi::CString;
use std::io;
use std::mem;
use std::os::unix::io::RawFd;

/// A shared-memory file descriptor suitable for passing to other processes.
#[derive(Debug)]
pub struct SharedMemory {
  fd: RawFd,
  len: usize,
}

impl SharedMemory {
  /// Create an anonymous shared-memory region of `len` bytes.
  ///
  /// On Linux we prefer `memfd_create`, but we invoke it via `syscall(2)` so we don't link against
  /// the glibc `memfd_create` symbol (which is missing on older glibc builds).
  ///
  /// If the kernel doesn't support `memfd_create` (`ENOSYS`) or rejects our flags (`EINVAL`), we
  /// fall back to `shm_open`.
  pub fn create(name: &str, len: usize) -> io::Result<Self> {
    let fd = create_memfd_or_shm(name)?;
    let len_off: libc::off_t = len
      .try_into()
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shared memory too large"))?;
    // SAFETY: `ftruncate` is safe to call with a valid fd and a non-negative length.
    let rc = unsafe { libc::ftruncate(fd, len_off) };
    if rc != 0 {
      let err = io::Error::last_os_error();
      // SAFETY: best-effort close.
      unsafe {
        libc::close(fd);
      }
      return Err(err);
    }
    Ok(Self { fd, len })
  }

  pub fn len(&self) -> usize {
    self.len
  }

  pub fn as_raw_fd(&self) -> RawFd {
    self.fd
  }
}

impl Drop for SharedMemory {
  fn drop(&mut self) {
    // SAFETY: close(2) is safe with an owned fd.
    unsafe {
      libc::close(self.fd);
    }
  }
}

fn create_memfd_or_shm(name: &str) -> io::Result<RawFd> {
  let cname = CString::new(name)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "memfd name contains NUL"))?;

  let flags: libc::c_uint = libc::MFD_CLOEXEC as libc::c_uint;
  // 1) Preferred path: `memfd_create` via syscall for glibc compatibility.
  //
  // SAFETY: we pass a NUL-terminated pointer and valid flags; the syscall returns an fd or -1.
  let fd = unsafe { libc::syscall(libc::SYS_memfd_create, cname.as_ptr(), flags) };
  if fd >= 0 {
    return Ok(fd as RawFd);
  }
  let err = io::Error::last_os_error();
  match err.raw_os_error() {
    Some(libc::ENOSYS) | Some(libc::EINVAL) => {
      // 2) Fallback path: `shm_open` + immediate `shm_unlink` so the name doesn't persist.
      create_shm_open(name)
    }
    _ => Err(err),
  }
}

fn create_shm_open(name: &str) -> io::Result<RawFd> {
  // POSIX requires a leading '/'.
  let mut shm_name = String::with_capacity(name.len() + 1);
  shm_name.push('/');
  shm_name.push_str(name);

  let cname = CString::new(shm_name)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shm name contains NUL"))?;

  let flags = libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC;
  // SAFETY: `shm_open` expects a valid NUL-terminated name.
  let fd = unsafe { libc::shm_open(cname.as_ptr(), flags, 0o600) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }
  // Best-effort unlink so the object is removed when the last fd is closed.
  // SAFETY: name is valid.
  unsafe {
    libc::shm_unlink(cname.as_ptr());
  }
  Ok(fd)
}

/// A `SOCK_SEQPACKET` unix-domain socket used for control-plane IPC.
#[derive(Debug)]
pub struct UnixSeqpacket {
  fd: RawFd,
}

impl UnixSeqpacket {
  pub fn pair() -> io::Result<(Self, Self)> {
    let mut fds = [-1, -1];

    // Prefer creating CLOEXEC sockets atomically.
    #[cfg(test)]
    let force_fallback = TEST_FORCE_SOCKETPAIR_CLOEXEC_EINVAL.with(|cell| cell.get());
    #[cfg(not(test))]
    let force_fallback = false;

    if !force_fallback {
      // SAFETY: `fds` is valid for two ints.
      let rc = unsafe {
        libc::socketpair(
          libc::AF_UNIX,
          libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
          0,
          fds.as_mut_ptr(),
        )
      };
      if rc == 0 {
        return Ok((Self { fd: fds[0] }, Self { fd: fds[1] }));
      }

      let err = io::Error::last_os_error();
      if err.raw_os_error() != Some(libc::EINVAL) {
        return Err(err);
      }
    }

    // Older kernels reject SOCK_CLOEXEC on socketpair with `EINVAL`. Retry without it and then set
    // CLOEXEC manually.
    // SAFETY: `fds` is valid for two ints.
    let rc = unsafe {
      libc::socketpair(
        libc::AF_UNIX,
        libc::SOCK_SEQPACKET,
        0,
        fds.as_mut_ptr(),
      )
    };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }

    if let Err(err) = set_cloexec(fds[0]).and_then(|()| set_cloexec(fds[1])) {
      // SAFETY: close best-effort.
      unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
      }
      return Err(err);
    }

    Ok((Self { fd: fds[0] }, Self { fd: fds[1] }))
  }

  /// Send a message with an optional fd payload.
  ///
  /// Linux historically has edge-cases where sending only `SCM_RIGHTS` with an empty iovec can
  /// result in the control message not being delivered. When `bytes` is empty but `fds` is not, we
  /// therefore send a 1-byte dummy payload.
  pub fn send_msg(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let dummy;
    let payload: &[u8] = if bytes.is_empty() && !fds.is_empty() {
      dummy = [0u8; 1];
      &dummy
    } else {
      bytes
    };

    let mut iov = libc::iovec {
      iov_base: payload.as_ptr() as *mut libc::c_void,
      iov_len: payload.len(),
    };

    let mut cmsg_storage = Vec::new();
    let (msg_control, msg_controllen) = if fds.is_empty() {
      (std::ptr::null_mut(), 0)
    } else {
      let fd_bytes = fds.len() * mem::size_of::<RawFd>();
      let space = unsafe { libc::CMSG_SPACE(fd_bytes as u32) } as usize;
      cmsg_storage.resize(space, 0);
      (cmsg_storage.as_mut_ptr() as *mut libc::c_void, cmsg_storage.len())
    };

    let msg = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: &mut iov,
      msg_iovlen: 1,
      msg_control,
      msg_controllen,
      msg_flags: 0,
    };

    if !fds.is_empty() {
      // SAFETY: we allocated enough space via CMSG_SPACE; the returned pointer is within
      // `cmsg_storage`.
      unsafe {
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        debug_assert!(!hdr.is_null());
        (*hdr).cmsg_level = libc::SOL_SOCKET;
        (*hdr).cmsg_type = libc::SCM_RIGHTS;
        (*hdr).cmsg_len = libc::CMSG_LEN((fds.len() * mem::size_of::<RawFd>()) as u32) as usize;
        let data = libc::CMSG_DATA(hdr) as *mut RawFd;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), data, fds.len());
      }
    }

    // SAFETY: `msg` is fully initialized. `sendmsg` reads from `payload` and `fds`.
    let rc = unsafe { libc::sendmsg(self.fd, &msg, 0) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }
    if rc as usize != payload.len() {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "sendmsg wrote unexpected byte count",
      ));
    }
    Ok(())
  }

  /// Receive a message and any passed fds.
  pub fn recv_msg(&self, buf: &mut [u8], max_fds: usize) -> io::Result<(usize, Vec<RawFd>)> {
    let mut iov = libc::iovec {
      iov_base: buf.as_mut_ptr() as *mut libc::c_void,
      iov_len: buf.len(),
    };

    let mut cmsg_storage = Vec::new();
    let (msg_control, msg_controllen) = if max_fds == 0 {
      (std::ptr::null_mut(), 0)
    } else {
      let space =
        unsafe { libc::CMSG_SPACE((max_fds * mem::size_of::<RawFd>()) as u32) } as usize;
      cmsg_storage.resize(space, 0);
      (cmsg_storage.as_mut_ptr() as *mut libc::c_void, cmsg_storage.len())
    };

    let mut msg = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: &mut iov,
      msg_iovlen: 1,
      msg_control,
      msg_controllen,
      msg_flags: 0,
    };

    // SAFETY: `msg` is fully initialized.
    let rc = unsafe { libc::recvmsg(self.fd, &mut msg, 0) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }

    if (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "recvmsg control message truncated",
      ));
    }

    let mut received_fds = Vec::new();
    if max_fds != 0 {
      // SAFETY: iterate over the control buffer returned by the kernel.
      unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
          if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let data = libc::CMSG_DATA(cmsg) as *const RawFd;
            let data_len = ((*cmsg).cmsg_len as usize)
              .saturating_sub(mem::size_of::<libc::cmsghdr>());
            let fd_count = data_len / mem::size_of::<RawFd>();
            for i in 0..fd_count {
              received_fds.push(*data.add(i));
            }
          }
          cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
      }
    }

    Ok((rc as usize, received_fds))
  }

  pub fn as_raw_fd(&self) -> RawFd {
    self.fd
  }
}

impl Drop for UnixSeqpacket {
  fn drop(&mut self) {
    // SAFETY: close(2) is safe with an owned fd.
    unsafe {
      libc::close(self.fd);
    }
  }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
  // SAFETY: fcntl is safe for a valid fd.
  let current = unsafe { libc::fcntl(fd, libc::F_GETFD) };
  if current < 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: fcntl is safe for a valid fd.
  let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, current | libc::FD_CLOEXEC) };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(test)]
thread_local! {
  // Test-only knob to force the `socketpair(...|SOCK_CLOEXEC)` path to behave as if it returned
  // `EINVAL`, exercising the manual `fcntl(FD_CLOEXEC)` fallback.
  static TEST_FORCE_SOCKETPAIR_CLOEXEC_EINVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
struct ForceSocketpairCloexecFallback {
  prev: bool,
}

#[cfg(test)]
impl ForceSocketpairCloexecFallback {
  fn enable() -> Self {
    let prev = TEST_FORCE_SOCKETPAIR_CLOEXEC_EINVAL.with(|cell| {
      let prev = cell.get();
      cell.set(true);
      prev
    });
    Self { prev }
  }
}

#[cfg(test)]
impl Drop for ForceSocketpairCloexecFallback {
  fn drop(&mut self) {
    TEST_FORCE_SOCKETPAIR_CLOEXEC_EINVAL.with(|cell| cell.set(self.prev));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::File;
  use std::io::{Read, Seek, SeekFrom, Write};
  use std::os::unix::io::AsRawFd;
  use std::os::unix::io::FromRawFd;

  fn assert_cloexec(fd: RawFd) {
    // SAFETY: fcntl is safe for a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "fcntl(F_GETFD) failed: {}", io::Error::last_os_error());
    assert!(
      (flags & libc::FD_CLOEXEC) != 0,
      "expected FD_CLOEXEC to be set"
    );
  }

  #[test]
  fn ipc_portability_send_empty_payload_with_fd() {
    let (a, b) = UnixSeqpacket::pair().expect("socketpair");

    let mut tmp = tempfile::tempfile().expect("tempfile");
    tmp.write_all(b"hello").expect("write");
    tmp.flush().expect("flush");
    tmp.seek(SeekFrom::Start(0)).expect("seek");

    a.send_msg(&[], &[tmp.as_raw_fd()])
      .expect("send empty payload with fd");

    let mut buf = [0u8; 8];
    let (n, fds) = b.recv_msg(&mut buf, 8).expect("recv");
    assert_eq!(n, 1, "expected 1-byte payload");
    assert_eq!(buf[0], 0, "expected dummy payload byte to be 0");
    assert_eq!(fds.len(), 1, "expected 1 received fd");

    // Validate the received fd is readable and refers to the same file contents.
    let mut file = unsafe { File::from_raw_fd(fds[0]) };
    let mut s = String::new();
    file.read_to_string(&mut s).expect("read from received fd");
    assert_eq!(s, "hello");
    // Avoid double-close: `file` now owns the fd; don't let `fds[0]` be closed again elsewhere.
  }

  #[test]
  fn ipc_portability_unix_seqpacket_pair_sets_cloexec() {
    let (a, b) = UnixSeqpacket::pair().expect("socketpair");
    assert_cloexec(a.as_raw_fd());
    assert_cloexec(b.as_raw_fd());
  }

  #[test]
  fn ipc_portability_unix_seqpacket_pair_fallback_sets_cloexec() {
    let _guard = ForceSocketpairCloexecFallback::enable();
    let (a, b) = UnixSeqpacket::pair().expect("socketpair (forced fallback)");
    assert_cloexec(a.as_raw_fd());
    assert_cloexec(b.as_raw_fd());
  }
}
