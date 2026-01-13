use std::os::fd::AsRawFd;
use std::process::Command;

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
fn seccomp_denies_getdents64_on_inherited_dir_fd() {
  const CHILD_ENV: &str = "FASTR_TEST_SECCOMP_GETDENTS_CHILD";
  const TEST_NAME: &str = concat!(module_path!(), "::seccomp_denies_getdents64_on_inherited_dir_fd");

  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    // Open a directory FD before applying seccomp so we can verify `getdents64` is denied even for
    // inherited directory handles (defense in depth).
    //
    // Prefer `/etc` (common and typically non-empty), fall back to `/` if it doesn't exist.
    let dir_path = if std::path::Path::new("/etc").is_dir() {
      "/etc"
    } else {
      "/"
    };
    let dir = std::fs::File::open(dir_path).expect("open directory before seccomp");
    let dir_fd = dir.as_raw_fd();

    match fastrender::sandbox::apply_renderer_seccomp_denylist() {
      Ok(fastrender::sandbox::SandboxStatus::Applied)
      | Ok(fastrender::sandbox::SandboxStatus::AppliedWithoutTsync) => {}
      Ok(
        fastrender::sandbox::SandboxStatus::Disabled | fastrender::sandbox::SandboxStatus::Unsupported,
      ) => return,
      Err(err) => {
        if is_seccomp_unsupported_error(&err) {
          return;
        }
        panic!("failed to apply seccomp sandbox in child: {err}");
      }
    }

    let mut buf = [0u8; 8192];
    // SAFETY: We intentionally call a blocked syscall to verify seccomp filtering. Arguments are
    // valid (an open directory fd + writable buffer), so without the seccomp denylist this would
    // normally return a non-negative byte count.
    let rc = unsafe {
      libc::syscall(
        libc::SYS_getdents64,
        dir_fd as libc::c_long,
        buf.as_mut_ptr().cast::<libc::c_void>(),
        buf.len() as libc::c_long,
      )
    };
    assert_eq!(rc, -1, "expected getdents64 to be denied by seccomp");
    let err = std::io::Error::last_os_error();
    // If the kernel doesn't implement getdents64 (very old), ENOSYS is acceptable.
    if err.raw_os_error() == Some(libc::ENOSYS) {
      return;
    }
    assert_eq!(
      err.raw_os_error(),
      Some(libc::EPERM),
      "expected getdents64 to fail with EPERM, got {err:?}"
    );
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Avoid a large libtest threadpool: the sandbox is process-global. When TSYNC is supported it
    // applies to all threads; when TSYNC is unavailable the sandbox must be installed before any
    // additional threads spawn.
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
