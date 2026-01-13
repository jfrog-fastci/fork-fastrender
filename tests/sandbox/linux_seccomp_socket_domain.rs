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
fn socket_domain_filter_allows_unix_denies_inet() {
  const CHILD_ENV: &str = "FASTR_TEST_SECCOMP_SOCKET_DOMAIN_CHILD";
  const TEST_NAME: &str = concat!(
    module_path!(),
    "::socket_domain_filter_allows_unix_denies_inet"
  );
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    match fastrender::sandbox::apply_renderer_sandbox(fastrender::sandbox::RendererSandboxConfig {
      // This test asserts the "allow AF_UNIX, deny AF_INET" policy.
      network_policy: fastrender::sandbox::NetworkPolicy::AllowUnixSocketsOnly,
      // Avoid closing unrelated fds in the libtest child process; this test focuses on seccomp.
      close_extra_fds: false,
      ..fastrender::sandbox::RendererSandboxConfig::default()
    }) {
      Ok(
        fastrender::sandbox::SandboxStatus::Applied
        | fastrender::sandbox::SandboxStatus::AppliedWithoutTsync,
      ) => {}
      Ok(
        fastrender::sandbox::SandboxStatus::DisabledByEnv
        | fastrender::sandbox::SandboxStatus::DisabledByConfig
        | fastrender::sandbox::SandboxStatus::ReportOnly
        | fastrender::sandbox::SandboxStatus::Unsupported,
      ) => return,
      Err(err) => {
        if is_seccomp_unsupported_error(&err) {
          return;
        }
        panic!("apply Linux sandbox: {err}");
      }
    }

    let mut fds = [-1, -1];
    // SAFETY: `fds` is a valid pointer to two integers.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(
      rc,
      0,
      "socketpair(AF_UNIX, SOCK_STREAM) should succeed: {}",
      std::io::Error::last_os_error()
    );
    assert!(
      fds[0] >= 0 && fds[1] >= 0,
      "expected socketpair to return valid fds, got {fds:?}"
    );
    // SAFETY: fds are valid on success.
    unsafe {
      libc::close(fds[0]);
      libc::close(fds[1]);
    }

    // SAFETY: `socket` is a raw libc call.
    let unix_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    assert!(
      unix_fd >= 0,
      "socket(AF_UNIX, SOCK_STREAM) should succeed: {}",
      std::io::Error::last_os_error()
    );
    // SAFETY: fd is valid on success.
    unsafe {
      libc::close(unix_fd);
    }

    // SAFETY: direct libc syscall wrapper.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert_eq!(fd, -1, "socket(AF_INET) should be blocked by seccomp");
    let err = std::io::Error::last_os_error();
    assert_eq!(
      err.raw_os_error(),
      Some(libc::EPERM),
      "expected socket(AF_INET) to fail with EPERM, got {err:?}"
    );
    return;
  }

  let exe = std::env::current_exe().expect("current test executable path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Avoid a large libtest threadpool: the sandbox applies to all threads when TSYNC is
    // supported, and when TSYNC is unavailable we must avoid spawning additional threads.
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg(TEST_NAME)
    .arg("--nocapture")
    .output()
    .expect("spawn seccomp test child process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
