#![cfg(target_os = "linux")]

use std::io;
use std::process::Command;

use fastrender::sandbox::linux_landlock;

#[test]
fn landlock_deny_all_blocks_etc_passwd() {
  const CHILD_ENV: &str = "FASTR_TEST_LANDLOCK_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    if !std::path::Path::new("/etc/passwd").exists() {
      eprintln!("skipping Landlock test: /etc/passwd does not exist on this system");
      return;
    }

    // Create some FDs before applying Landlock; deny-all Landlock should not break use of
    // already-open pipes/sockets/memfd (used by the multiprocess renderer IPC/shmem design).
    let mut pipe_fds = [-1i32; 2];
    // SAFETY: `pipe_fds` points to 2 writable integers.
    let rc = unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(rc, 0, "pipe2 should succeed before applying Landlock");

    let mut sock_fds = [-1i32; 2];
    // SAFETY: `sock_fds` points to 2 writable integers.
    let rc = unsafe {
      libc::socketpair(
        libc::AF_UNIX,
        libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
        0,
        sock_fds.as_mut_ptr(),
      )
    };
    assert_eq!(rc, 0, "socketpair should succeed before applying Landlock");

    // memfd_create may not be available on very old kernels; treat ENOSYS as a skip for this part.
    let memfd_name = std::ffi::CString::new("fastrender-landlock-test").unwrap();
    // SAFETY: syscall ABI matches `memfd_create(const char *name, unsigned int flags)`.
    let memfd_rc = unsafe { libc::syscall(libc::SYS_memfd_create, memfd_name.as_ptr(), libc::MFD_CLOEXEC) };
    let memfd: i32 = memfd_rc as i32;
    let memfd_supported = memfd >= 0;
    if !memfd_supported {
      let err = io::Error::last_os_error();
      if err.raw_os_error() != Some(libc::ENOSYS) {
        panic!("memfd_create failed unexpectedly before applying Landlock: {err:?}");
      }
      eprintln!("memfd_create unavailable (ENOSYS); skipping memfd portion of Landlock test");
    }

    let status =
      linux_landlock::apply(&linux_landlock::LandlockConfig::deny_all()).expect("apply landlock");
    match status {
      linux_landlock::LandlockStatus::Unsupported { reason } => {
        eprintln!("skipping Landlock test: Landlock unsupported ({reason:?})");
        return;
      }
      linux_landlock::LandlockStatus::Applied { abi } => {
        eprintln!("Landlock applied (abi={abi})");
      }
    }

    // Verify we can still use the inherited pipe FDs.
    let msg = b"hello";
    // SAFETY: `pipe_fds[1]` is a valid FD, `msg` is valid memory.
    let wrote = unsafe { libc::write(pipe_fds[1], msg.as_ptr().cast(), msg.len()) };
    assert_eq!(wrote, msg.len() as isize, "write to pipe should succeed");
    let mut buf = [0u8; 16];
    // SAFETY: `pipe_fds[0]` is a valid FD, `buf` is valid writable memory.
    let read = unsafe { libc::read(pipe_fds[0], buf.as_mut_ptr().cast(), msg.len()) };
    assert_eq!(read, msg.len() as isize, "read from pipe should succeed");
    assert_eq!(&buf[..msg.len()], msg, "pipe message should round-trip");

    // Verify we can still use the inherited Unix socketpair FDs.
    let msg = b"ping";
    // SAFETY: `sock_fds[0]` is a valid FD, `msg` is valid memory.
    let wrote = unsafe { libc::send(sock_fds[0], msg.as_ptr().cast(), msg.len(), 0) };
    assert_eq!(wrote, msg.len() as isize, "send on socketpair should succeed");
    let mut buf = [0u8; 16];
    // SAFETY: `sock_fds[1]` is a valid FD, `buf` is valid writable memory.
    let read = unsafe { libc::recv(sock_fds[1], buf.as_mut_ptr().cast(), msg.len(), 0) };
    assert_eq!(read, msg.len() as isize, "recv on socketpair should succeed");
    assert_eq!(&buf[..msg.len()], msg, "socketpair message should round-trip");

    if memfd_supported {
      // Verify we can still read/write to an already-open memfd.
      let msg = b"memfd";
      // SAFETY: `memfd` is a valid FD, `msg` is valid memory.
      let wrote = unsafe { libc::write(memfd, msg.as_ptr().cast(), msg.len()) };
      assert_eq!(wrote, msg.len() as isize, "write to memfd should succeed");
      // SAFETY: `memfd` is a valid FD.
      let off = unsafe { libc::lseek(memfd, 0, libc::SEEK_SET) };
      assert_eq!(off, 0, "lseek(memfd, 0) should succeed");
      let mut buf = [0u8; 16];
      // SAFETY: `memfd` is a valid FD, `buf` is valid writable memory.
      let read = unsafe { libc::read(memfd, buf.as_mut_ptr().cast(), msg.len()) };
      assert_eq!(read, msg.len() as isize, "read from memfd should succeed");
      assert_eq!(&buf[..msg.len()], msg, "memfd contents should round-trip");
    }

    let path = std::ffi::CString::new("/etc/passwd").unwrap();
    // SAFETY: `path` is a NUL-terminated string.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd >= 0 {
      // SAFETY: `fd` is a valid file descriptor.
      unsafe { libc::close(fd) };
      panic!("expected open(/etc/passwd) to be denied by Landlock");
    }
    let errno = io::Error::last_os_error()
      .raw_os_error()
      .expect("errno should be set");
    assert!(
      errno == libc::EPERM || errno == libc::EACCES,
      "expected permission error (EPERM/EACCES) when opening /etc/passwd under deny-all landlock, got {errno}"
    );

    // Close FDs (best-effort).
    // SAFETY: close ignores invalid FDs; these are expected to be valid.
    unsafe {
      libc::close(pipe_fds[0]);
      libc::close(pipe_fds[1]);
      libc::close(sock_fds[0]);
      libc::close(sock_fds[1]);
      if memfd_supported {
        libc::close(memfd);
      }
    }
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "landlock_deny_all_blocks_etc_passwd";
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
