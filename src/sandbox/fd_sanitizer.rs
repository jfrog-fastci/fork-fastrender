use std::io;

// `RawFd` is unix-only, but the crate aims to compile on all platforms.
// On non-unix targets we provide a stub `RawFd` type and return `Unsupported`.
#[cfg(any(unix, target_os = "wasi"))]
pub use std::os::fd::RawFd;
#[cfg(not(any(unix, target_os = "wasi")))]
pub type RawFd = i32;

/// Close all file descriptors except those listed in `keep`.
///
/// This is intended to be used as a *defense-in-depth* measure when spawning a
/// sandboxed renderer, typically from a `std::process::Command::pre_exec` hook.
///
/// ## `pre_exec` usage notes
/// `pre_exec` runs after `fork` and before `exec`, so it must avoid heap
/// allocation and other non-async-signal-safe operations. This helper:
/// - does **not** allocate,
/// - uses only a small, linear scan over `keep` to decide what to preserve,
/// - prefers Linux `close_range` when available, falling back to `close(2)` loops.
///
/// Callers should prepare the `keep` slice *before* `pre_exec` (e.g. a small
/// `[RawFd; N]` on the stack) and ensure it contains at least the stdio fds they
/// expect to keep (typically `0`, `1`, `2`).
pub fn close_fds_except(keep: &[RawFd]) -> io::Result<()> {
  close_fds_except_impl(keep)
}

/// Mark all file descriptors except those listed in `keep` as `FD_CLOEXEC`.
///
/// This is a spawn-oriented variant of [`close_fds_except`]. Instead of closing file descriptors
/// immediately, it ensures they will not survive an `execve(2)` boundary by setting the
/// `FD_CLOEXEC` flag.
///
/// ## Why `CLOEXEC` instead of close?
/// When used from `std::process::CommandExt::pre_exec`, Rust's process spawning machinery may hold
/// internal helper pipes open in the child until `exec`. Closing *all* FDs can interfere with error
/// reporting from `exec` failures. Setting `FD_CLOEXEC` avoids leaking unrelated FDs into the
/// sandboxed process *after* `exec` while keeping the spawn machinery intact.
///
/// Like [`close_fds_except`], this helper is allocation-free and intended to be safe to call from a
/// `pre_exec` hook.
pub fn set_cloexec_on_fds_except(keep: &[RawFd]) -> io::Result<()> {
  set_cloexec_on_fds_except_impl(keep)
}

#[inline]
fn fd_is_kept(fd: RawFd, keep: &[RawFd]) -> bool {
  // `keep` is expected to be tiny (typically a few pipe fds + stdio), so a linear
  // scan is both allocation-free and fast enough.
  for &k in keep {
    if k == fd {
      return true;
    }
  }
  false
}

#[cfg(target_os = "linux")]
fn close_fds_except_impl(keep: &[RawFd]) -> io::Result<()> {
  match close_fds_except_with_close_range(keep) {
    Ok(()) => Ok(()),
    Err(err) if err.raw_os_error() == Some(libc::ENOSYS) => close_fds_except_by_scanning(keep),
    Err(err) => Err(err),
  }
}

#[cfg(target_os = "linux")]
fn set_cloexec_on_fds_except_impl(keep: &[RawFd]) -> io::Result<()> {
  match set_cloexec_on_fds_except_with_close_range(keep) {
    Ok(()) => Ok(()),
    // Older kernels may not support `close_range` at all (ENOSYS) or may not support the
    // CLOEXEC-flagged operation (EINVAL).
    Err(err) if matches!(err.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EINVAL)) => {
      set_cloexec_on_fds_except_by_scanning(keep)
    }
    Err(err) => Err(err),
  }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn set_cloexec_on_fds_except_impl(keep: &[RawFd]) -> io::Result<()> {
  set_cloexec_on_fds_except_by_scanning(keep)
}

#[cfg(not(unix))]
fn set_cloexec_on_fds_except_impl(_keep: &[RawFd]) -> io::Result<()> {
  Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn close_fds_except_impl(keep: &[RawFd]) -> io::Result<()> {
  close_fds_except_by_scanning(keep)
}

#[cfg(not(unix))]
fn close_fds_except_impl(_keep: &[RawFd]) -> io::Result<()> {
  Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(target_os = "linux")]
fn close_fds_except_with_close_range(keep: &[RawFd]) -> io::Result<()> {
  // We do not sort or allocate: instead, repeatedly find the next kept fd above `start`.
  let mut start: u32 = 0;

  loop {
    let mut next_keep: Option<u32> = None;
    for &fd in keep {
      if fd < 0 {
        continue;
      }
      let fd_u32 = fd as u32;
      if fd_u32 < start {
        continue;
      }
      next_keep = match next_keep {
        None => Some(fd_u32),
        Some(cur) => Some(cur.min(fd_u32)),
      };
    }

    match next_keep {
      None => {
        close_range_syscall(start, u32::MAX, 0)?;
        return Ok(());
      }
      Some(kept) => {
        if kept > start {
          close_range_syscall(start, kept - 1, 0)?;
        }
        // Skip the kept fd itself.
        start = kept.saturating_add(1);
      }
    }
  }
}

#[cfg(target_os = "linux")]
fn close_range_syscall(first: u32, last: u32, flags: libc::c_uint) -> io::Result<()> {
  if first > last {
    return Ok(());
  }
  // SAFETY: `syscall` invokes the kernel directly. We pass the exact arguments
  // expected by `close_range(2)` with the requested flags.
  let rc = unsafe {
    libc::syscall(
      libc::SYS_close_range,
      first as libc::c_uint,
      last as libc::c_uint,
      flags,
    )
  };
  if rc == 0 {
    Ok(())
  } else {
    Err(io::Error::last_os_error())
  }
}

#[cfg(target_os = "linux")]
fn set_cloexec_on_fds_except_with_close_range(keep: &[RawFd]) -> io::Result<()> {
  // `CLOSE_RANGE_CLOEXEC` from `linux/close_range.h`.
  const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

  // We do not sort or allocate: instead, repeatedly find the next kept fd above `start`.
  let mut start: u32 = 0;

  loop {
    let mut next_keep: Option<u32> = None;
    for &fd in keep {
      if fd < 0 {
        continue;
      }
      let fd_u32 = fd as u32;
      if fd_u32 < start {
        continue;
      }
      next_keep = match next_keep {
        None => Some(fd_u32),
        Some(cur) => Some(cur.min(fd_u32)),
      };
    }

    match next_keep {
      None => {
        close_range_syscall(start, u32::MAX, CLOSE_RANGE_CLOEXEC)?;
        return Ok(());
      }
      Some(kept) => {
        if kept > start {
          close_range_syscall(start, kept - 1, CLOSE_RANGE_CLOEXEC)?;
        }
        // Skip the kept fd itself.
        start = kept.saturating_add(1);
      }
    }
  }
}

#[cfg(unix)]
fn close_fds_except_by_scanning(keep: &[RawFd]) -> io::Result<()> {
  let max_fd_exclusive = fd_scan_limit();

  // Iterate through the fd table. Ignore EBADF (already closed); treat other
  // errors as fatal so callers know sanitization might have been incomplete.
  for fd in 0..max_fd_exclusive {
    let fd_i32: RawFd = fd as RawFd;
    if fd_is_kept(fd_i32, keep) {
      continue;
    }

    // SAFETY: `close` is safe to call with any integer fd value.
    let rc = unsafe { libc::close(fd_i32) };
    if rc == 0 {
      continue;
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
      Some(libc::EBADF) => {
        // Not open; nothing to do.
      }
      Some(libc::EINTR) => {
        // POSIX leaves the state unspecified after EINTR, but in our intended
        // `pre_exec` use case there are no concurrent fd allocations, so a
        // best-effort retry is safe.
        //
        // Retry until we get something other than EINTR.
        loop {
          // SAFETY: same as above.
          let rc_retry = unsafe { libc::close(fd_i32) };
          if rc_retry == 0 {
            break;
          }
          let retry_err = io::Error::last_os_error();
          match retry_err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EBADF) => break,
            _ => return Err(retry_err),
          }
        }
      }
      _ => return Err(err),
    }
  }

  Ok(())
}

#[cfg(unix)]
fn set_cloexec_on_fds_except_by_scanning(keep: &[RawFd]) -> io::Result<()> {
  let max_fd_exclusive = fd_scan_limit();

  for fd in 0..max_fd_exclusive {
    let fd_i32: RawFd = fd as RawFd;
    if fd_is_kept(fd_i32, keep) {
      continue;
    }

    let flags = loop {
      // SAFETY: `fcntl` is safe to call with any integer fd value.
      let flags = unsafe { libc::fcntl(fd_i32, libc::F_GETFD) };
      if flags != -1 {
        break flags;
      }
      let err = io::Error::last_os_error();
      match err.raw_os_error() {
        Some(libc::EBADF) => break -1,
        Some(libc::EINTR) => continue,
        _ => return Err(err),
      }
    };

    if flags == -1 {
      continue;
    }

    if (flags & libc::FD_CLOEXEC) != 0 {
      continue;
    }

    loop {
      // SAFETY: `fcntl` is safe to call with any integer fd value.
      let rc = unsafe { libc::fcntl(fd_i32, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
      if rc != -1 {
        break;
      }
      let err = io::Error::last_os_error();
      match err.raw_os_error() {
        Some(libc::EBADF) => break,
        Some(libc::EINTR) => continue,
        _ => return Err(err),
      }
    }
  }

  Ok(())
}

#[cfg(unix)]
fn fd_scan_limit() -> usize {
  // Avoid pathological iteration if the soft limit was raised to something huge.
  // `1_048_576` is a compromise: high enough to cover common large ulimit values,
  // low enough to avoid spending minutes in a tight loop on misconfigured hosts.
  const MAX_SCAN: usize = 1_048_576;

  let mut limit: usize = 0;

  // Prefer RLIMIT_NOFILE when available.
  let mut rlim = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  // SAFETY: `getrlimit` writes to `rlim` when the pointer is valid.
  if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) } == 0 {
    // `rlim_cur` may be `RLIM_INFINITY` or exceed usize.
    if rlim.rlim_cur != libc::RLIM_INFINITY {
      // `rlim_t` is unsigned on Unix-ish platforms.
      limit = usize::try_from(rlim.rlim_cur as u64).unwrap_or(0);
    }
  }

  if limit == 0 {
    // Fallback to sysconf. Still might be huge, so it is capped below.
    // SAFETY: `sysconf` is a process-global query.
    let sys = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if sys > 0 {
      limit = usize::try_from(sys).unwrap_or(0);
    }
  }

  if limit == 0 {
    // Extremely conservative fallback. Most systems default to 1024.
    limit = 1024;
  }

  limit.min(MAX_SCAN)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::process::Command;

  #[cfg(target_os = "linux")]
  #[test]
  fn close_fds_except_closes_extra_fds() {
    const CHILD_ENV: &str = "FASTR_TEST_CLOSE_FDS_EXCEPT_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      run_child();
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::fd_sanitizer::tests::close_fds_except_closes_extra_fds";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  #[cfg(target_os = "linux")]
  fn run_child() {
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open("/etc/passwd").expect("open /etc/passwd");
    let file_fd = file.as_raw_fd();
    assert!(file_fd > 2, "expected file fd > 2, got {file_fd}");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind TCP listener");
    let addr = listener.local_addr().expect("listener addr");
    let client = TcpStream::connect(addr).expect("connect to listener");
    let client_fd = client.as_raw_fd();
    assert!(client_fd > 2, "expected socket fd > 2, got {client_fd}");

    // Complete the connection so the socket would be usable if it remained open.
    let (_server, _) = listener.accept().expect("accept connection");

    // Ensure the file is readable pre-sanitization (sanity check that the fd is valid).
    let mut buf = [0u8; 1];
    // SAFETY: `file_fd` is a valid open fd and `buf` is writable.
    let n = unsafe { libc::read(file_fd, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n, 1, "expected read from /etc/passwd to succeed");

    close_fds_except(&[0, 1, 2]).expect("close fds except stdio");

    // Stdout/stderr must remain usable.
    let msg_out = b"stdout still open\n";
    // SAFETY: fd 1 should remain open.
    let out_rc = unsafe { libc::write(1, msg_out.as_ptr().cast(), msg_out.len()) };
    assert_eq!(
      out_rc,
      msg_out.len() as isize,
      "expected stdout write to succeed"
    );

    let msg_err = b"stderr still open\n";
    // SAFETY: fd 2 should remain open.
    let err_rc = unsafe { libc::write(2, msg_err.as_ptr().cast(), msg_err.len()) };
    assert_eq!(
      err_rc,
      msg_err.len() as isize,
      "expected stderr write to succeed"
    );

    // The original file fd must be closed.
    let mut buf2 = [0u8; 1];
    // SAFETY: `file_fd` may be closed; this tests EBADF behavior.
    let n2 = unsafe { libc::read(file_fd, buf2.as_mut_ptr().cast(), buf2.len()) };
    assert_eq!(n2, -1, "expected read to fail on closed fd");
    let err = io::Error::last_os_error();
    assert_eq!(
      err.raw_os_error(),
      Some(libc::EBADF),
      "expected EBADF after closing file fd, got {err:?}"
    );

    // The original socket fd must be closed.
    let sock_msg = b"x";
    // SAFETY: `client_fd` may be closed; this tests EBADF behavior.
    let n3 = unsafe { libc::write(client_fd, sock_msg.as_ptr().cast(), sock_msg.len()) };
    assert_eq!(n3, -1, "expected write to fail on closed socket fd");
    let err2 = io::Error::last_os_error();
    assert_eq!(
      err2.raw_os_error(),
      Some(libc::EBADF),
      "expected EBADF after closing socket fd, got {err2:?}"
    );
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn set_cloexec_on_fds_except_prevents_fd_leaks_into_execed_child() {
    use std::os::fd::AsRawFd;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt as _;

    const CHILD_ENV: &str = "FASTR_TEST_SET_CLOEXEC_CHILD";
    const FD_ENV: &str = "FASTR_TEST_SET_CLOEXEC_FD";

    if std::env::var_os(CHILD_ENV).is_some() {
      let fd: RawFd = std::env::var(FD_ENV)
        .expect("missing FD_ENV")
        .parse::<i32>()
        .expect("parse fd env");
      let mut buf = [0u8; 1];
      let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
      assert_eq!(n, -1, "expected leaked fd to be closed on exec");
      let err = io::Error::last_os_error();
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EBADF),
        "expected EBADF for leaked fd after exec, got {err:?}"
      );
      return;
    }

    // Parent: create an inheritable FD by duping a CLOEXEC file descriptor to a high number.
    let file = std::fs::File::open("/etc/passwd").expect("open /etc/passwd");
    let file_fd = file.as_raw_fd();

    let mut target_fd: RawFd = -1;
    for cand in (64..512).rev() {
      let rc = unsafe { libc::fcntl(cand, libc::F_GETFD) };
      if rc == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EBADF) {
          target_fd = cand;
          break;
        }
      }
    }
    assert!(
      target_fd >= 0,
      "failed to find a free fd number for the leak test"
    );

    let dup_rc = unsafe { libc::dup2(file_fd, target_fd) };
    assert_eq!(
      dup_rc, target_fd,
      "dup2 should duplicate to requested target fd"
    );

    // `dup2` leaves FD_CLOEXEC cleared on the new descriptor (inheritable by default). Keep it open
    // in the parent until after spawn.
    let _leaked = unsafe { OwnedFd::from_raw_fd(target_fd) };

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::fd_sanitizer::tests::set_cloexec_on_fds_except_prevents_fd_leaks_into_execed_child";
    let mut cmd = Command::new(exe);
    cmd
      .env(CHILD_ENV, "1")
      .env(FD_ENV, target_fd.to_string())
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture");

    let keep = [0, 1, 2];
    unsafe {
      cmd.pre_exec(move || set_cloexec_on_fds_except(&keep));
    }

    let output = cmd.output().expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
