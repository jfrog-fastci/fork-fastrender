#![cfg(target_os = "linux")]

use std::process::Command;

fn get_dumpable() -> i32 {
  // SAFETY: `prctl` is a process-global syscall. `PR_GET_DUMPABLE` ignores the remaining args, but
  // we pass zeros to avoid libc varargs UB on implementations that unconditionally read them.
  let rc = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
  assert!(rc >= 0, "prctl(PR_GET_DUMPABLE) should succeed");
  rc
}

#[test]
fn sandbox_sets_prctl_dumpable_0() {
  const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_LINUX_PRCTL_DUMPABLE_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();

  if is_child {
    let before = get_dumpable();
    assert_eq!(
      before, 1,
      "expected PR_GET_DUMPABLE to be 1 in a fresh process before sandboxing"
    );

    // This installs the Linux renderer sandbox (seccomp). Even if seccomp installation is rejected
    // by the host kernel, the dumpable hardening step should still be applied first.
    if let Err(err) = fastrender::sandbox::apply_renderer_seccomp_denylist() {
      if matches!(err, fastrender::sandbox::SandboxError::SetDumpableFailed { .. }) {
        panic!("failed to apply PR_SET_DUMPABLE=0: {err}");
      }
    }

    let after = get_dumpable();
    assert_eq!(
      after, 0,
      "expected PR_GET_DUMPABLE to be 0 after applying sandbox hardening"
    );
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Avoid a large libtest threadpool: the sandbox uses TSYNC and applies to all threads.
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg("sandbox_sets_prctl_dumpable_0")
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
