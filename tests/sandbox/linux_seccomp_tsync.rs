use fastrender::sandbox::{
  apply_renderer_seccomp_denylist_with_report, RendererSandboxConfig, SandboxStatus,
};

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
fn force_disable_tsync_returns_applied_without_tsync() {
  const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_SECCOMP_CHILD";
  const TEST_NAME: &str = concat!(
    module_path!(),
    "::force_disable_tsync_returns_applied_without_tsync"
  );
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    match apply_renderer_seccomp_denylist_with_report(RendererSandboxConfig {
      force_disable_tsync: true,
      ..Default::default()
    }) {
      Ok((status, _report)) => assert_eq!(status, SandboxStatus::AppliedWithoutTsync),
      Err(err) => {
        if is_seccomp_unsupported_error(&err) {
          return;
        }
        panic!("apply renderer seccomp denylist: {err}");
      }
    }
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
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
