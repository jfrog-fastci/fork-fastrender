//! Guard that ensures the macOS relaxed sandbox profile still blocks access to `$HOME`.
//!
//! This prevents accidentally widening filesystem allow rules (e.g. allowing `~/` reads) in the
//! relaxed profile.

#![cfg(target_os = "macos")]

use fastrender::sandbox::macos::{
  MacosSandboxMode, MacosSandboxStatus, ENV_DISABLE_RENDERER_SANDBOX, ENV_MACOS_RENDERER_SANDBOX,
};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENV_HOME_FILE: &str = "FASTR_TEST_SANDBOX_HOME_FILE";

fn apply_relaxed_sandbox_profile() -> MacosSandboxStatus {
  fastrender::sandbox::macos::apply_renderer_sandbox(MacosSandboxMode::RendererSystemFonts)
    .expect("apply macOS relaxed renderer sandbox")
}

fn assert_permission_denied(err: &io::Error, path: &Path) {
  let raw = err.raw_os_error();
  assert_ne!(
    err.kind(),
    io::ErrorKind::NotFound,
    "expected sandbox to block reading {}, but got NotFound instead (raw={raw:?}, err={err:?})",
    path.display()
  );
  assert!(
    err.kind() == io::ErrorKind::PermissionDenied
      || matches!(raw, Some(libc::EPERM) | Some(libc::EACCES)),
    "expected PermissionDenied/EPERM/EACCES when reading {} under sandbox; got kind={:?} raw={raw:?} err={err:?}",
    path.display(),
    err.kind()
  );
}

#[test]
fn relaxed_sandbox_profile_denies_home_file_read() {
  if let Some(path) = std::env::var_os(ENV_HOME_FILE) {
    // Child process: apply sandbox then attempt to read the home-owned file.
    let path = PathBuf::from(path);
    let status = apply_relaxed_sandbox_profile();
    if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
      eprintln!(
        "skipping relaxed-sandbox home guard: process was already sandboxed (status={status:?})"
      );
      return;
    }
    let err = std::fs::read_to_string(&path).unwrap_err();
    assert_permission_denied(&err, &path);
    return;
  }

  // Parent process: create a temp file under $HOME, write a sentinel, then spawn a child to attempt
  // the read after sandboxing.
  let home = std::env::var_os("HOME").expect("HOME env var must be set for sandbox test");
  let home = PathBuf::from(home);
  assert!(
    home.is_dir(),
    "HOME env var did not resolve to a directory: {}",
    home.display()
  );

  let mut file = tempfile::Builder::new()
    .prefix("fastr_test_sandbox_home_")
    .tempfile_in(&home)
    .expect("create temp file in $HOME");

  const SENTINEL: &str = "fastr_sandbox_home_sentinel";
  use std::io::Write;
  file.write_all(SENTINEL.as_bytes())
    .expect("write sentinel to home file");
  file.flush().expect("flush sentinel to disk");

  let path = file.path().to_path_buf();
  let roundtrip = std::fs::read_to_string(&path).expect("read back sentinel in parent");
  assert_eq!(roundtrip, SENTINEL, "home file should contain sentinel");

  let exe = std::env::current_exe().expect("current test exe path");
  // `module_path!()` includes the crate name (e.g. `integration::...`) but the test harness expects
  // `--exact` arguments without it (e.g. `guards::...`).
  let module_path = module_path!();
  let module_path = module_path
    .split_once("::")
    .map(|(_, rest)| rest)
    .unwrap_or(module_path);
  let test_name = format!("{module_path}::relaxed_sandbox_profile_denies_home_file_read");

  let output = Command::new(exe)
    // Force the sandbox configuration for this guard, independent of any developer/CI override.
    .env_remove(ENV_DISABLE_RENDERER_SANDBOX)
    .env_remove(ENV_MACOS_RENDERER_SANDBOX)
    .env(ENV_HOME_FILE, &path)
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg(&test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandboxed child process");

  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  // Parent cleanup: ensure the home file is removed after the child exits.
  file.close().expect("remove temp file in $HOME");
  assert!(
    !path.exists(),
    "expected temp file to be removed after child exits: {}",
    path.display()
  );
}
