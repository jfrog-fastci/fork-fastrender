//! Hardened validation helpers for untrusted shared-memory file descriptors (Linux).
//!
//! When a file descriptor crosses an IPC boundary it must be treated as untrusted input.
//! In particular, `mmap`-ing an attacker-controlled fd without validation can lead to:
//! - SIGBUS crashes (e.g. if the sender truncates the file after the receiver maps it),
//! - excessive virtual memory mappings / resource exhaustion,
//! - type confusion (sockets/pipes/etc passed where a file was expected).
//!
//! This module centralizes validation so every IPC boundary can apply the same checks before
//! `mmap`.

use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd};

/// Successfully validated shared-memory metadata.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedShm {
  /// Size (in bytes) of the shared-memory object at validation time.
  pub size: u64,
  /// Memfd seals (Linux only).
  #[cfg(target_os = "linux")]
  pub seals: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum ShmValidateError {
  #[error("fstat failed")]
  FstatFailed {
    #[source]
    source: io::Error,
  },
  #[error("fd is not a regular file (mode=0o{mode:o})")]
  NotRegularFile { mode: u32 },
  #[error("fd reported a negative size ({size})")]
  NegativeSize { size: i64 },
  #[error("shared memory size {size} exceeds max allowed size {max}")]
  SizeTooLarge { size: u64, max: u64 },
  #[error("shared memory size {size} does not match expected {expected}")]
  SizeMismatch { size: u64, expected: u64 },
  #[cfg(target_os = "linux")]
  #[error("fd does not support seals (fcntl(F_GET_SEALS) returned EINVAL)")]
  NotSealable,
  #[cfg(target_os = "linux")]
  #[error("failed to query fd seals (fcntl(F_GET_SEALS) failed)")]
  GetSealsFailed {
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("memfd missing required seals (got=0x{got:x}, required=0x{required:x})")]
  MissingSeals { got: i32, required: i32 },
  #[error("rgba buffer size overflow for {width}x{height}")]
  RgbaSizeOverflow { width: u32, height: u32 },
  #[error("rgba buffer size {size} exceeds max allowed size {max}")]
  RgbaSizeTooLarge { size: u64, max: u64 },
}

fn is_regular_file(mode: libc::mode_t) -> bool {
  // `S_ISREG` is a macro in libc; implement the check directly to avoid relying on macro exports.
  (mode & libc::S_IFMT) == libc::S_IFREG
}

fn fstat_fd(fd: BorrowedFd<'_>) -> Result<libc::stat, ShmValidateError> {
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  // SAFETY: `fstat` writes to `st` when the pointer is valid.
  let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
  if rc != 0 {
    return Err(ShmValidateError::FstatFailed {
      source: io::Error::last_os_error(),
    });
  }
  Ok(st)
}

/// Validate a shared-memory file descriptor received across an IPC boundary.
///
/// - The fd must refer to a regular file (`S_IFREG`).
/// - The size must be non-negative and <= `max_size`.
/// - When `expected_size` is `Some`, the size must match exactly.
///
/// # Linux memfd sealing policy
///
/// On Linux we additionally require the fd to be *sealable* and to have `F_SEAL_SHRINK` and
/// `F_SEAL_GROW` applied. This ensures the sender cannot change the size after validation, which
/// could otherwise lead to SIGBUS crashes when the receiver accesses an `mmap`-ed range.
///
/// If `fcntl(F_GET_SEALS)` fails with `EINVAL` (meaning "not sealable"), we currently reject the fd
/// (`ShmValidateError::NotSealable`) to fail closed at the security boundary.
pub fn validate_shm_fd(
  fd: BorrowedFd<'_>,
  expected_size: Option<u64>,
  max_size: u64,
) -> Result<ValidatedShm, ShmValidateError> {
  let st = fstat_fd(fd)?;

  if !is_regular_file(st.st_mode) {
    return Err(ShmValidateError::NotRegularFile {
      mode: st.st_mode as u32,
    });
  }

  let size_i64 = st.st_size;
  if size_i64 < 0 {
    return Err(ShmValidateError::NegativeSize { size: size_i64 });
  }
  let size: u64 = size_i64
    .try_into()
    .map_err(|_| ShmValidateError::NegativeSize { size: size_i64 })?;

  if size > max_size {
    return Err(ShmValidateError::SizeTooLarge { size, max: max_size });
  }

  if let Some(expected) = expected_size {
    if size != expected {
      return Err(ShmValidateError::SizeMismatch { size, expected });
    }
  }

  #[cfg(target_os = "linux")]
  {
    let seals = get_and_validate_seals_linux(fd)?;
    return Ok(ValidatedShm { size, seals });
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = fd;
    return Ok(ValidatedShm { size });
  }
}

#[cfg(target_os = "linux")]
fn get_and_validate_seals_linux(fd: BorrowedFd<'_>) -> Result<i32, ShmValidateError> {
  let seals = loop {
    // SAFETY: `fcntl` is called with a valid fd and command.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals != -1 {
      break seals;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    if err.raw_os_error() == Some(libc::EINVAL) {
      return Err(ShmValidateError::NotSealable);
    }
    return Err(ShmValidateError::GetSealsFailed { source: err });
  };

  // Harden against post-validation truncation/extension (SIGBUS / logic bugs).
  let required = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
  if (seals & required) != required {
    return Err(ShmValidateError::MissingSeals { got: seals, required });
  }
  Ok(seals)
}

/// Compute the byte length of an RGBA8 buffer for the given dimensions.
///
/// This uses checked arithmetic and enforces a hard upper bound so attackers cannot request
/// absurd-sized mappings/allocations.
pub fn rgba_len(width: u32, height: u32) -> Result<u64, ShmValidateError> {
  let pixels = u64::from(width)
    .checked_mul(u64::from(height))
    .ok_or(ShmValidateError::RgbaSizeOverflow { width, height })?;
  let bytes = pixels
    .checked_mul(4)
    .ok_or(ShmValidateError::RgbaSizeOverflow { width, height })?;

  // Reuse the renderer's in-process pixmap guardrail.
  let max = crate::paint::pixmap::MAX_PIXMAP_BYTES;
  if bytes > max {
    return Err(ShmValidateError::RgbaSizeTooLarge { size: bytes, max });
  }
  Ok(bytes)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::io::{AsFd, AsRawFd, FromRawFd, OwnedFd};

  #[cfg(target_os = "linux")]
  fn socketpair_seqpacket() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `socketpair` initializes `fds` on success.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: fds returned by `socketpair` are valid and owned by us.
    let a = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let b = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((a, b))
  }

  #[cfg(target_os = "linux")]
  fn pipe_fds() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `pipe` initializes `fds` on success.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: fds returned by `pipe` are valid and owned by us.
    let r = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let w = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((r, w))
  }

  #[cfg(target_os = "linux")]
  fn create_memfd(name: &str, size: u64, seal: bool) -> io::Result<OwnedFd> {
    use std::ffi::CString;

    let cname = CString::new(name).expect("memfd name must be CString-safe");
    // SAFETY: `memfd_create` is called with a valid C string pointer.
    let raw = unsafe {
      libc::memfd_create(
        cname.as_ptr(),
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
      )
    };
    if raw < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is freshly created and owned.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let size_off: libc::off_t = size
      .try_into()
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "size does not fit in off_t"))?;
    // SAFETY: `ftruncate` is called with a valid fd and size.
    let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), size_off) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }

    if seal {
      let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
      // SAFETY: `fcntl` is called with a valid fd.
      let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) };
      if rc != 0 {
        return Err(io::Error::last_os_error());
      }
    }

    Ok(fd)
  }

  #[cfg(target_os = "linux")]
  fn cmsg_align(len: usize) -> usize {
    let align = std::mem::align_of::<usize>();
    (len + align - 1) & !(align - 1)
  }

  #[cfg(target_os = "linux")]
  fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
  }

  #[cfg(target_os = "linux")]
  fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + data_len
  }

  #[cfg(target_os = "linux")]
  unsafe fn cmsg_data(cmsg: *mut libc::cmsghdr) -> *mut u8 {
    (cmsg as *mut u8).add(cmsg_align(std::mem::size_of::<libc::cmsghdr>()))
  }

  #[cfg(target_os = "linux")]
  fn send_one_fd(sock: &OwnedFd, fd_to_send: &OwnedFd) -> io::Result<()> {
    let byte: [u8; 1] = [0x42];
    let mut iov = libc::iovec {
      iov_base: byte.as_ptr() as *mut libc::c_void,
      iov_len: byte.len(),
    };

    let data_len = std::mem::size_of::<libc::c_int>();
    let space = cmsg_space(data_len);
    let words = (space + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut control = vec![0usize; words];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = (control.len() * std::mem::size_of::<usize>()) as _;

    unsafe {
      let cmsg = msg.msg_control as *mut libc::cmsghdr;
      (*cmsg).cmsg_level = libc::SOL_SOCKET;
      (*cmsg).cmsg_type = libc::SCM_RIGHTS;
      (*cmsg).cmsg_len = cmsg_len(data_len) as _;
      let data = cmsg_data(cmsg) as *mut libc::c_int;
      std::ptr::write(data, fd_to_send.as_raw_fd());
    }

    // SAFETY: msghdr is correctly initialized with valid pointers.
    let rc = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  }

  #[cfg(target_os = "linux")]
  fn recv_one_fd(sock: &OwnedFd) -> io::Result<OwnedFd> {
    let mut byte: [u8; 1] = [0];
    let mut iov = libc::iovec {
      iov_base: byte.as_mut_ptr() as *mut libc::c_void,
      iov_len: byte.len(),
    };

    let data_len = std::mem::size_of::<libc::c_int>();
    let space = cmsg_space(data_len);
    let words = (space + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut control = vec![0usize; words];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = (control.len() * std::mem::size_of::<usize>()) as _;

    // SAFETY: msghdr is correctly initialized with valid pointers.
    let rc = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }

    if msg.msg_controllen < cmsg_len(data_len) as _ {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing SCM_RIGHTS control message",
      ));
    }

    // SAFETY: control buffer is aligned (usize) and large enough; msg_controllen checked above.
    let cmsg = msg.msg_control as *mut libc::cmsghdr;
    unsafe {
      if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "unexpected control message",
        ));
      }
      let data = cmsg_data(cmsg) as *const libc::c_int;
      let fd = std::ptr::read(data);
      if fd < 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "received invalid fd"));
      }
      // SAFETY: fd is now owned by the receiver.
      Ok(OwnedFd::from_raw_fd(fd))
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn shm_validate_rejects_pipe_fd_via_seqpacket_channel() {
    let (sock_a, sock_b) = socketpair_seqpacket().expect("socketpair");
    let (pipe_r, _pipe_w) = pipe_fds().expect("pipe");

    send_one_fd(&sock_a, &pipe_r).expect("send fd");
    let received = recv_one_fd(&sock_b).expect("recv fd");

    let err = validate_shm_fd(received.as_fd(), None, 4096).expect_err("pipe fd should reject");
    assert!(matches!(err, ShmValidateError::NotRegularFile { .. }));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn shm_validate_rejects_unsealed_memfd_when_seals_required() {
    let fd = create_memfd("shm_validate_unsealed", 4096, false).expect("memfd");
    let err = validate_shm_fd(fd.as_fd(), Some(4096), 4096).expect_err("unsealed memfd rejects");
    assert!(matches!(err, ShmValidateError::MissingSeals { .. }));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn shm_validate_rejects_size_mismatch_expected_smaller_and_larger() {
    let fd = create_memfd("shm_validate_mismatch", 4096, true).expect("memfd");

    let err =
      validate_shm_fd(fd.as_fd(), Some(4095), 4096).expect_err("expected smaller rejects");
    assert!(matches!(err, ShmValidateError::SizeMismatch { .. }));

    let err = validate_shm_fd(fd.as_fd(), Some(4097), 8192).expect_err("expected larger rejects");
    assert!(matches!(err, ShmValidateError::SizeMismatch { .. }));
  }
}
