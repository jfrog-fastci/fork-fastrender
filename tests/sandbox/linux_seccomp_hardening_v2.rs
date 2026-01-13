use std::process::Command;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_IO_URING_SETUP: Option<libc::c_long> = Some(libc::SYS_io_uring_setup as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_IO_URING_SETUP: Option<libc::c_long> = None;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_OPEN_BY_HANDLE_AT: Option<libc::c_long> =
  Some(libc::SYS_open_by_handle_at as libc::c_long);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const SYS_OPEN_BY_HANDLE_AT: Option<libc::c_long> = None;

fn assert_syscall_fails_with_errno(name: &str, rc: libc::c_long, expected_errno: i32) {
  assert_eq!(rc, -1, "expected `{name}` syscall to fail, got rc={rc}");
  let err = std::io::Error::last_os_error();
  // Older kernels may not implement newer syscalls (e.g. io_uring). In that case, ENOSYS is an
  // acceptable outcome: the attack surface is not present.
  if err.raw_os_error() == Some(libc::ENOSYS) {
    return;
  }
  assert_eq!(
    err.raw_os_error(),
    Some(expected_errno),
    "expected `{name}` to fail with errno={expected_errno}, got {err:?}"
  );
}

fn is_seccomp_unsupported_error(err: &fastrender::sandbox::SandboxError) -> bool {
  let errno = match err {
    fastrender::sandbox::SandboxError::SetParentDeathSignalFailed { source }
    | fastrender::sandbox::SandboxError::SetDumpableFailed { source }
    | fastrender::sandbox::SandboxError::DisableCoreDumpsFailed { source }
    | fastrender::sandbox::SandboxError::EnableNoNewPrivsFailed { source } => source.raw_os_error(),
    fastrender::sandbox::SandboxError::SeccompInstallRejected { errno, .. } => Some(*errno),
    fastrender::sandbox::SandboxError::SeccompInstallFailed { errno, .. } => Some(*errno),
    _ => None,
  };
  matches!(errno, Some(code) if code == libc::ENOSYS || code == libc::EINVAL)
}

#[test]
fn blocks_high_risk_syscalls() {
  const CHILD_ENV: &str = "FASTR_TEST_SECCOMP_HARDENING_V2_CHILD";
  const TEST_NAME: &str = concat!(module_path!(), "::blocks_high_risk_syscalls");
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    match fastrender::sandbox::apply_renderer_seccomp_denylist() {
      Ok(fastrender::sandbox::SandboxStatus::Applied)
      | Ok(fastrender::sandbox::SandboxStatus::AppliedWithoutTsync) => {}
      Ok(
        fastrender::sandbox::SandboxStatus::DisabledByEnv
        | fastrender::sandbox::SandboxStatus::DisabledByConfig
        | fastrender::sandbox::SandboxStatus::ReportOnly
        | fastrender::sandbox::SandboxStatus::Unsupported,
      ) => {
        return;
      }
      Err(err) => {
        if is_seccomp_unsupported_error(&err) {
          return;
        }
        panic!("failed to apply seccomp sandbox in child: {err}");
      }
    }

    // `io_uring_setup` should be blocked (returning EPERM) even though the arguments are invalid.
    // Without the seccomp filter, this would return something like `EFAULT`.
    if let Some(nr) = SYS_IO_URING_SETUP {
      let rc =
        unsafe { libc::syscall(nr, 2 as libc::c_long, std::ptr::null_mut::<libc::c_void>()) };
      assert_syscall_fails_with_errno("io_uring_setup", rc, libc::EPERM);
    }

    // File-handle based path traversal primitives should be blocked as well.
    if let Some(nr) = SYS_OPEN_BY_HANDLE_AT {
      let rc = unsafe {
        libc::syscall(
          nr,
          -1 as libc::c_long,
          std::ptr::null_mut::<libc::c_void>(),
          0 as libc::c_long,
        )
      };
      assert_syscall_fails_with_errno("open_by_handle_at", rc, libc::EPERM);
    }
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Avoid a large libtest threadpool: the sandbox applies to all threads when TSYNC is
    // supported, and when TSYNC is unavailable we must avoid spawning additional threads.
    .env("RUST_TEST_THREADS", "1")
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
