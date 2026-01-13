#![cfg(target_os = "linux")]

use std::process::Command;

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
