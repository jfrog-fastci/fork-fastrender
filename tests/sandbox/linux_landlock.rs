use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};

use fastrender::sandbox::linux_landlock;

#[test]
fn landlock_deny_all_blocks_etc_passwd() {
  const CHILD_ENV: &str = "FASTR_TEST_LANDLOCK_CHILD";
  const TEST_NAME: &str = concat!(module_path!(), "::landlock_deny_all_blocks_etc_passwd");
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
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .arg("--exact")
    .arg(TEST_NAME)
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

#[test]
fn landlock_deny_all_allows_memfd_and_inherited_pipe() {
  const CHILD_ENV: &str = "FASTR_TEST_LANDLOCK_IPC_CHILD";
  const PIPE_READ_ENV: &str = "FASTR_TEST_LANDLOCK_PIPE_READ_FD";
  const PIPE_WRITE_ENV: &str = "FASTR_TEST_LANDLOCK_PIPE_WRITE_FD";
  const TEST_NAME: &str = concat!(
    module_path!(),
    "::landlock_deny_all_allows_memfd_and_inherited_pipe"
  );

  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    if !std::path::Path::new("/etc/passwd").exists() {
      eprintln!("skipping Landlock test: /etc/passwd does not exist on this system");
      return;
    }

    let read_fd: i32 = std::env::var(PIPE_READ_ENV)
      .expect("pipe read fd env")
      .parse()
      .expect("pipe read fd int");
    let write_fd: i32 = std::env::var(PIPE_WRITE_ENV)
      .expect("pipe write fd env")
      .parse()
      .expect("pipe write fd int");

    // Apply deny-all ruleset. If unsupported, skip.
    let status =
      linux_landlock::apply(&linux_landlock::LandlockConfig::deny_all()).expect("apply landlock");
    match status {
      linux_landlock::LandlockStatus::Unsupported { reason } => {
        eprintln!("skipping Landlock IPC test: Landlock unsupported ({reason:?})");
        return;
      }
      linux_landlock::LandlockStatus::Applied { abi } => {
        eprintln!("Landlock applied (abi={abi})");
      }
    }

    // Keep the existing guarantee: opening a path should fail under deny-all.
    let path = std::ffi::CString::new("/etc/passwd").unwrap();
    // SAFETY: `path` is a NUL-terminated string.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    assert_eq!(
      fd, -1,
      "expected open(/etc/passwd) to be denied by Landlock"
    );

    // (1) `memfd_create` should still work (anonymous shared memory).
    let memfd = unsafe {
      let name = b"fr_landlock\0";
      libc::memfd_create(name.as_ptr().cast(), libc::MFD_CLOEXEC)
    };
    assert_ne!(
      memfd, -1,
      "expected memfd_create to succeed under Landlock (errno={:?})",
      std::io::Error::last_os_error()
    );

    // SAFETY: `memfd` is a valid file descriptor on success.
    let mut memfd_file = unsafe { std::fs::File::from_raw_fd(memfd) };
    // Ensure the file is big enough to mmap by writing to it (avoid ftruncate differences across
    // kernels).
    let mut buf = [0u8; 4096];
    buf[..5].copy_from_slice(b"hello");
    use std::io::{Read, Seek, Write};
    memfd_file.write_all(&buf).expect("write memfd");

    let map_len = buf.len();
    let ptr = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        map_len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        memfd_file.as_raw_fd(),
        0,
      )
    };
    assert_ne!(
      ptr,
      libc::MAP_FAILED,
      "mmap(memfd) failed under Landlock (errno={:?})",
      std::io::Error::last_os_error()
    );

    unsafe {
      let slice = std::slice::from_raw_parts_mut(ptr.cast::<u8>(), map_len);
      assert_eq!(&slice[..5], b"hello");
      slice[0] = b'X';
    }
    memfd_file.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut first = [0u8; 1];
    memfd_file.read_exact(&mut first).unwrap();
    assert_eq!(first[0], b'X');
    unsafe {
      libc::munmap(ptr, map_len);
    }

    // (2) A pre-created pipe inherited from the parent should remain usable for read/write.
    // Write and then read back through the inherited pipe FDs.
    let msg = b"ping";
    write_all_fd(write_fd, msg).expect("write inherited pipe");
    let mut got = [0u8; 4];
    read_exact_fd(read_fd, &mut got).expect("read inherited pipe");
    assert_eq!(&got, msg);

    return;
  }

  // Parent: create a pipe before spawning the sandboxed child, and pass the FDs through env vars.
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe: {:?}", std::io::Error::last_os_error());
  let read_fd = fds[0];
  let write_fd = fds[1];

  // Ensure the FDs are inheritable across exec.
  for fd in [read_fd, write_fd] {
    // SAFETY: fcntl is called with a valid fd.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    assert_ne!(rc, -1, "fcntl(F_SETFD): {:?}", std::io::Error::last_os_error());
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let mut child = Command::new(exe)
    .env(CHILD_ENV, "1")
    .env(PIPE_READ_ENV, read_fd.to_string())
    .env(PIPE_WRITE_ENV, write_fd.to_string())
    .env("RUST_TEST_THREADS", "1")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .arg("--exact")
    .arg(TEST_NAME)
    .arg("--nocapture")
    .spawn()
    .expect("spawn child test process");

  // Close our copies; the child inherited its own.
  unsafe {
    libc::close(read_fd);
    libc::close(write_fd);
  }

  let output = child.wait_with_output().expect("wait child output");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

fn read_exact_fd(fd: i32, mut buf: &mut [u8]) -> io::Result<()> {
  while !buf.is_empty() {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
      return Err(io::Error::last_os_error());
    }
    if n == 0 {
      return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF"));
    }
    let n = n as usize;
    buf = &mut buf[n..];
  }
  Ok(())
}

fn write_all_fd(fd: i32, mut buf: &[u8]) -> io::Result<()> {
  while !buf.is_empty() {
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
      return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    buf = &buf[n..];
  }
  Ok(())
}
