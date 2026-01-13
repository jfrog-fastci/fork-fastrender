#![cfg(windows)]

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use std::path::Path;
use tempfile::TempDir;

#[test]
fn appcontainer_temp_dir_is_writable() {
  // Ensure the sandbox isn't disabled by a developer's env overrides.
  std::env::remove_var("FASTR_DISABLE_RENDERER_SANDBOX");
  std::env::remove_var("FASTR_WINDOWS_RENDERER_SANDBOX");
  std::env::remove_var("FASTR_WINDOWS_SANDBOX_INHERIT_ENV");

  // Simulate a "normal" browser process environment where TEMP/TMP point at a user-profile
  // directory. AppContainers frequently cannot access that location, so the sandbox spawn path
  // must override TEMP/TMP to an AppContainer-writable directory.
  let prev_temp = std::env::var_os("TEMP");
  let prev_tmp = std::env::var_os("TMP");
  let forced_parent_temp = TempDir::new().expect("create parent temp dir");
  std::env::set_var("TEMP", forced_parent_temp.path());
  std::env::set_var("TMP", forced_parent_temp.path());

  let child_exe = Path::new(env!("CARGO_BIN_EXE_appcontainer_temp_smoke"));
  let child = spawn_sandboxed(child_exe, &[], &[]).expect("spawn appcontainer child");
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

  // Best-effort restore of parent env vars.
  match prev_temp {
    Some(value) => std::env::set_var("TEMP", value),
    None => std::env::remove_var("TEMP"),
  }
  match prev_tmp {
    Some(value) => std::env::set_var("TMP", value),
    None => std::env::remove_var("TMP"),
  }
}
