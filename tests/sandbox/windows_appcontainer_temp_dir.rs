#![cfg(windows)]

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use std::path::Path;
use tempfile::TempDir;

#[test]
fn appcontainer_temp_dir_is_writable() {
  // Simulate a "normal" browser process environment where TEMP/TMP point at a user-profile
  // directory. AppContainers frequently cannot access that location, so the sandbox spawn path
  // must override TEMP/TMP to an AppContainer-writable directory.
  let forced_parent_temp = TempDir::new().expect("create parent temp dir");
  let child_exe = Path::new(env!("CARGO_BIN_EXE_appcontainer_temp_smoke"));
  let child = {
    // Ensure the sandbox isn't disabled by a developer's env overrides.
    let _guard = crate::common::EnvVarsGuard::remove(&[
      "FASTR_DISABLE_RENDERER_SANDBOX",
      "FASTR_WINDOWS_RENDERER_SANDBOX",
      "FASTR_WINDOWS_SANDBOX_INHERIT_ENV",
    ]);
    let _temp_guard = crate::common::EnvVarGuard::set("TEMP", forced_parent_temp.path());
    let _tmp_guard = crate::common::EnvVarGuard::set("TMP", forced_parent_temp.path());
    spawn_sandboxed(child_exe, &[], &[]).expect("spawn appcontainer child")
  };
  assert_eq!(
    child.level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandbox level, got {:?}",
    child.level
  );

  let exit_code = child.wait().expect("wait for appcontainer child");
  assert_eq!(
    exit_code, 0,
    "expected child to exit cleanly, got code {exit_code}"
  );
}
