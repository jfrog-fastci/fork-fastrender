#![cfg(target_os = "linux")]

use std::process::Command;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_PIDFD_OPEN: Option<libc::c_long> = Some(libc::SYS_pidfd_open as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_PIDFD_OPEN: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_PIDFD_GETFD: Option<libc::c_long> = Some(libc::SYS_pidfd_getfd as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_PIDFD_GETFD: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_PIDFD_SEND_SIGNAL: Option<libc::c_long> =
  Some(libc::SYS_pidfd_send_signal as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_PIDFD_SEND_SIGNAL: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_IO_URING_SETUP: Option<libc::c_long> = Some(libc::SYS_io_uring_setup as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_IO_URING_SETUP: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_IO_URING_ENTER: Option<libc::c_long> = Some(libc::SYS_io_uring_enter as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_IO_URING_ENTER: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_IO_URING_REGISTER: Option<libc::c_long> =
  Some(libc::SYS_io_uring_register as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_IO_URING_REGISTER: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_FANOTIFY_INIT: Option<libc::c_long> = Some(libc::SYS_fanotify_init as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_FANOTIFY_INIT: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_FANOTIFY_MARK: Option<libc::c_long> = Some(libc::SYS_fanotify_mark as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_FANOTIFY_MARK: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_NAME_TO_HANDLE_AT: Option<libc::c_long> =
  Some(libc::SYS_name_to_handle_at as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_NAME_TO_HANDLE_AT: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_OPEN_BY_HANDLE_AT: Option<libc::c_long> =
  Some(libc::SYS_open_by_handle_at as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_OPEN_BY_HANDLE_AT: Option<libc::c_long> = None;

fn assert_syscall_fails_with_eperm(name: &str, nr: libc::c_long, args: [libc::c_long; 6]) {
  // SAFETY: We intentionally call a blocked syscall to verify seccomp filtering.
  let rc = unsafe { libc::syscall(nr, args[0], args[1], args[2], args[3], args[4], args[5]) };
  assert_eq!(rc, -1, "{name} should be denied by seccomp");
  let err = std::io::Error::last_os_error();
  assert_eq!(
    err.raw_os_error(),
    Some(libc::EPERM),
    "{name} should fail with EPERM (got {err:?})"
  );
}

fn maybe_assert_syscall_fails_with_eperm(
  name: &str,
  nr: Option<libc::c_long>,
  args: [libc::c_long; 6],
) {
  let Some(nr) = nr else {
    return;
  };
  assert_syscall_fails_with_eperm(name, nr, args);
}

#[test]
fn linux_seccomp_blocks_ptrace_and_unshare() {
  const CHILD_ENV: &str = "FASTR_TEST_LINUX_SECCOMP_HARDENING_CHILD";

  if std::env::var_os(CHILD_ENV).is_some() {
    let status = fastrender::sandbox::apply_renderer_sandbox(
      fastrender::sandbox::RendererSandboxConfig::default(),
    )
    .expect("apply renderer sandbox policy");
    assert_eq!(
      status,
      fastrender::sandbox::SandboxStatus::Applied,
      "expected sandbox to be applied"
    );

    // SAFETY: We intentionally call a blocked syscall to verify seccomp filtering.
    let rc = unsafe {
      libc::ptrace(
        libc::PTRACE_TRACEME,
        0,
        std::ptr::null_mut::<libc::c_void>(),
        std::ptr::null_mut::<libc::c_void>(),
      )
    };
    assert_eq!(rc, -1, "ptrace should be denied by seccomp");
    let err = std::io::Error::last_os_error();
    assert_eq!(
      err.raw_os_error(),
      Some(libc::EPERM),
      "ptrace should fail with EPERM"
    );

    // `unshare(CLONE_NEWUSER)` is a well-known privilege boundary; block it even though it would
    // typically fail without additional privileges.
    // SAFETY: This syscall is blocked by the seccomp filter and should return EPERM.
    let rc = unsafe { libc::syscall(libc::SYS_unshare as libc::c_long, libc::CLONE_NEWUSER) };
    assert_eq!(rc, -1, "unshare should be denied by seccomp");
    let err = std::io::Error::last_os_error();
    assert_eq!(
      err.raw_os_error(),
      Some(libc::EPERM),
      "unshare should fail with EPERM"
    );

    let pid = unsafe { libc::getpid() } as libc::c_long;

    assert_syscall_fails_with_eperm(
      "setns",
      libc::SYS_setns as libc::c_long,
      [-1, 0, 0, 0, 0, 0],
    );

    assert_syscall_fails_with_eperm(
      "process_vm_readv",
      libc::SYS_process_vm_readv as libc::c_long,
      [pid, 0, 0, 0, 0, 0],
    );
    assert_syscall_fails_with_eperm(
      "process_vm_writev",
      libc::SYS_process_vm_writev as libc::c_long,
      [pid, 0, 0, 0, 0, 0],
    );
    assert_syscall_fails_with_eperm(
      "kcmp",
      libc::SYS_kcmp as libc::c_long,
      [pid, pid, 0, 0, 0, 0],
    );

    assert_syscall_fails_with_eperm(
      "keyctl",
      libc::SYS_keyctl as libc::c_long,
      [0, 0, 0, 0, 0, 0],
    );
    assert_syscall_fails_with_eperm(
      "add_key",
      libc::SYS_add_key as libc::c_long,
      [0, 0, 0, 0, 0, 0],
    );
    assert_syscall_fails_with_eperm(
      "request_key",
      libc::SYS_request_key as libc::c_long,
      [0, 0, 0, 0, 0, 0],
    );

    maybe_assert_syscall_fails_with_eperm("pidfd_open", SYS_PIDFD_OPEN, [pid, 0, 0, 0, 0, 0]);
    maybe_assert_syscall_fails_with_eperm("pidfd_getfd", SYS_PIDFD_GETFD, [-1, 0, 0, 0, 0, 0]);
    maybe_assert_syscall_fails_with_eperm(
      "pidfd_send_signal",
      SYS_PIDFD_SEND_SIGNAL,
      [-1, 0, 0, 0, 0, 0],
    );

    maybe_assert_syscall_fails_with_eperm("io_uring_setup", SYS_IO_URING_SETUP, [0, 0, 0, 0, 0, 0]);
    maybe_assert_syscall_fails_with_eperm(
      "io_uring_enter",
      SYS_IO_URING_ENTER,
      [-1, 0, 0, 0, 0, 0],
    );
    maybe_assert_syscall_fails_with_eperm(
      "io_uring_register",
      SYS_IO_URING_REGISTER,
      [-1, 0, 0, 0, 0, 0],
    );

    maybe_assert_syscall_fails_with_eperm("fanotify_init", SYS_FANOTIFY_INIT, [0, 0, 0, 0, 0, 0]);
    maybe_assert_syscall_fails_with_eperm("fanotify_mark", SYS_FANOTIFY_MARK, [-1, 0, 0, -1, 0, 0]);

    maybe_assert_syscall_fails_with_eperm(
      "name_to_handle_at",
      SYS_NAME_TO_HANDLE_AT,
      [-1, 0, 0, 0, 0, 0],
    );
    maybe_assert_syscall_fails_with_eperm(
      "open_by_handle_at",
      SYS_OPEN_BY_HANDLE_AT,
      [-1, 0, 0, 0, 0, 0],
    );

    return;
  }

  let exe = std::env::current_exe().expect("current test executable path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Avoid a large libtest threadpool: the sandbox uses TSYNC and applies to all threads.
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg("linux_seccomp_blocks_ptrace_and_unshare")
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
