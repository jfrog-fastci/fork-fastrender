//! Unix IPC bootstrap helpers (socketpair + inherited FD).
//!
//! # Design
//!
//! This module provides a small, reusable building block for spawning child
//! processes with an already-connected IPC channel *without touching the
//! filesystem* (no temp files, no named sockets).
//!
//! It is intended for a browser-style multiprocess architecture where the parent
//! process creates an `AF_UNIX` `socketpair(2)` and passes one end to the child.
//!
//! # Safety / correctness rules
//!
//! ## FD ownership
//! - The parent creates a socketpair and owns both FDs initially.
//! - The "child end" is **transferred** to the child. After calling
//!   [`spawn_child_with_ipc`], the parent should treat that FD as no longer
//!   usable (the helper consumes it).
//! - The child must take ownership of the inherited FD by constructing an
//!   `OwnedFd`/`UnixStream` from the numeric FD (see below).
//!
//! ## `CLOEXEC`
//! To avoid leaking the IPC socket into *unrelated* child processes, the
//! socketpair is created with `FD_CLOEXEC` set on **both** ends.
//!
//! [`spawn_child_with_ipc`] then arranges for the child process to inherit the
//! socket by duplicating the FD to a well-known number (`3`) **without**
//! `FD_CLOEXEC` immediately before `exec(2)`.
//!
//! This avoids a common race in multithreaded parents: clearing `CLOEXEC` in the
//! parent process can leak the FD into concurrently spawned children.
//!
//! ## How the child reads the FD
//! The parent sets an environment variable (`env_key`) to the numeric FD the
//! child should use (currently always `"3"`).
//!
//! In the child process:
//!
//! ```no_run
//! # use std::{env, io};
//! # use std::os::unix::io::{FromRawFd, RawFd};
//! # use std::os::unix::net::UnixStream;
//! let fd: RawFd = env::var("FASTERENDER_IPC_FD")
//!     .unwrap()
//!     .parse::<i32>()
//!     .unwrap();
//! let sock = unsafe { UnixStream::from_raw_fd(fd) };
//! # let _ = sock;
//! ```

use std::io;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

#[cfg(unix)]
const INHERITED_IPC_FD: RawFd = 3;

/// Create a connected Unix socketpair suitable for IPC.
///
/// On platforms that support it, this uses `SOCK_SEQPACKET` for message
/// boundaries. If `SOCK_SEQPACKET` is not supported at runtime, it falls back
/// to `SOCK_STREAM`.
///
/// Both returned FDs have `FD_CLOEXEC` set.
#[cfg(unix)]
pub fn socket_pair_seqpacket() -> io::Result<(OwnedFd, OwnedFd)> {
  socket_pair_with_type(libc::SOCK_SEQPACKET).or_else(|e| {
    // Some Unix platforms don't support SOCK_SEQPACKET for AF_UNIX socketpairs.
    // Fall back to SOCK_STREAM so the caller can still get an in-memory IPC
    // channel without hitting the filesystem.
    match e.raw_os_error() {
      Some(code)
        if code == libc::EPROTONOSUPPORT || code == libc::EOPNOTSUPP || code == libc::EINVAL =>
      {
        socket_pair_with_type(libc::SOCK_STREAM)
      }
      _ => Err(e),
    }
  })
}

#[cfg(unix)]
fn socket_pair_with_type(sock_type: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
  // Prefer an atomic CLOEXEC socketpair where available, then enforce CLOEXEC via fcntl
  // as a backstop (and for non-Linux platforms).
  #[cfg(any(target_os = "linux", target_os = "android"))]
  let sock_cloexec = libc::SOCK_CLOEXEC;
  #[cfg(not(any(target_os = "linux", target_os = "android")))]
  let sock_cloexec = 0;

  let mut fds: [libc::c_int; 2] = [-1, -1];
  let rc =
    unsafe { libc::socketpair(libc::AF_UNIX, sock_type | sock_cloexec, 0, fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }

  // Safety: socketpair() returns valid file descriptors on success.
  let fd0 = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let fd1 = unsafe { OwnedFd::from_raw_fd(fds[1]) };

  // Ensure CLOEXEC is set even if SOCK_CLOEXEC wasn't available/supported.
  set_cloexec(fd0.as_raw_fd(), true)?;
  set_cloexec(fd1.as_raw_fd(), true)?;

  Ok((fd0, fd1))
}

/// Configure `cmd` so the spawned child inherits `child_end` as an IPC socket.
///
/// - The IPC socket will be available in the child at FD `3` (`INHERITED_IPC_FD`).
/// - `env_key` will be set to the numeric FD value (currently `"3"`).
/// - The socket is passed without touching the filesystem.
/// - All other inherited file descriptors are marked `FD_CLOEXEC` (defense-in-depth).
///
/// The function consumes `child_end` to make ownership transfer explicit.
///
/// ## Implementation notes
/// This uses `CommandExt::pre_exec` and only performs async-signal-safe syscalls
/// in the child.
#[cfg(unix)]
pub fn spawn_child_with_ipc(
  cmd: &mut std::process::Command,
  child_end: OwnedFd,
  env_key: &str,
) -> io::Result<()> {
  cmd.env(env_key, INHERITED_IPC_FD.to_string());

  // Safety: the pre_exec closure is restricted to async-signal-safe operations.
  unsafe {
    cmd.pre_exec(move || {
      // Defense-in-depth: ensure the child is killed if the parent disappears.
      let _ = crate::sandbox::linux_set_parent_death_signal();

      let src_fd = child_end.as_raw_fd();

      // Duplicate to a well-known FD (3) to avoid collisions and keep the child-side
      // contract stable. dup2 is async-signal-safe.
      if libc::dup2(src_fd, INHERITED_IPC_FD) == -1 {
        return Err(io::Error::last_os_error());
      }

      // `dup2()` clears `FD_CLOEXEC` on the new descriptor, except when `src_fd == dst_fd` (in
      // which case no duplication occurs). Only do the extra `fcntl` in that rare case to keep the
      // `pre_exec` closure minimal.
      if src_fd == INHERITED_IPC_FD {
        set_cloexec(INHERITED_IPC_FD, false)?;
      }

      // Defense-in-depth: ensure unrelated inherited file descriptors do not leak into the exec'd
      // child process.
      //
      // This only sets `FD_CLOEXEC` and does not close fds, so it does not interfere with
      // `std::process::Command`'s internal CLOEXEC exec-error pipes.
      crate::sandbox::set_cloexec_on_fds_except(&[0, 1, 2, INHERITED_IPC_FD])?;

      Ok(())
    });
  }

  Ok(())
}

#[cfg(unix)]
fn set_cloexec(fd: RawFd, cloexec: bool) -> io::Result<()> {
  let flags = loop {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags != -1 {
      break flags;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  };

  let mut new_flags = flags;
  if cloexec {
    new_flags |= libc::FD_CLOEXEC;
  } else {
    new_flags &= !libc::FD_CLOEXEC;
  }

  if new_flags != flags {
    loop {
      let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
      if rc != -1 {
        break;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{Read as _, Write as _};
  use std::os::unix::io::IntoRawFd as _;
  use std::os::unix::net::UnixStream;
  use std::process::Command;
  use std::time::Duration;

  const CHILD_MODE_ENV: &str = "FASTERENDER_IPC_BOOTSTRAP_CHILD";
  const IPC_FD_ENV: &str = "FASTERENDER_IPC_FD";

  // This single test acts as both the parent and the re-exec'd child, depending
  // on `CHILD_MODE_ENV`. This keeps the helper binary self-contained and avoids
  // hitting the filesystem.
  #[test]
  fn ipc_bootstrap_spawn_child_with_ipc_roundtrip_smoke() -> io::Result<()> {
    if std::env::var_os(CHILD_MODE_ENV).is_some() {
      return child_mode();
    }

    parent_mode()
  }

  fn parent_mode() -> io::Result<()> {
    let (parent_end, child_end) = socket_pair_seqpacket()?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);

    // Run only this test in the child process.
    cmd.arg("ipc_bootstrap_spawn_child_with_ipc_roundtrip_smoke");
    cmd.env(CHILD_MODE_ENV, "1");

    spawn_child_with_ipc(&mut cmd, child_end, IPC_FD_ENV)?;

    let mut child = cmd.spawn()?;

    let mut sock = unsafe { UnixStream::from_raw_fd(parent_end.into_raw_fd()) };
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;

    let mut ping = [0u8; 4];
    sock.read_exact(&mut ping)?;
    assert_eq!(&ping, b"ping");

    sock.write_all(b"pong")?;
    sock.flush()?;

    let status = child.wait()?;
    assert!(status.success(), "child exited with {status:?}");

    Ok(())
  }

  fn child_mode() -> io::Result<()> {
    let fd: RawFd = std::env::var(IPC_FD_ENV)
      .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "missing IPC_FD_ENV"))?
      .parse::<i32>()
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid IPC fd"))?;

    let mut sock = unsafe { UnixStream::from_raw_fd(fd) };
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;

    sock.write_all(b"ping")?;
    sock.flush()?;

    let mut pong = [0u8; 4];
    sock.read_exact(&mut pong)?;
    assert_eq!(&pong, b"pong");

    Ok(())
  }
}
