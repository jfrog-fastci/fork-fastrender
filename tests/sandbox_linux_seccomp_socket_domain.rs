#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn socket_domain_filter_allows_unix_denies_inet() {
  const CHILD_ENV: &str = "FASTR_TEST_SECCOMP_SOCKET_DOMAIN_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let status = fastrender::sandbox::apply_renderer_sandbox(
      fastrender::sandbox::RendererSandboxConfig {
        network_policy: fastrender::sandbox::NetworkPolicy::AllowUnixSocketsOnly,
      },
    )
    .expect("apply Linux sandbox");
    assert_eq!(status, fastrender::sandbox::SandboxStatus::Applied);

    let mut fds = [-1, -1];
    // SAFETY: `fds` is a valid pointer to two integers.
    let rc =
      unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
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
    // Avoid a large libtest threadpool: the sandbox uses TSYNC and applies to all threads.
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg("socket_domain_filter_allows_unix_denies_inet")
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
