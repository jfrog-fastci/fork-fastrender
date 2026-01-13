#![cfg(windows)]

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use std::path::Path;

#[test]
fn appcontainer_temp_dir_is_writable() {
  // Ensure the sandbox isn't disabled by a developer's env overrides.
  std::env::remove_var("FASTR_DISABLE_RENDERER_SANDBOX");
  std::env::remove_var("FASTR_WINDOWS_RENDERER_SANDBOX");
  std::env::remove_var("FASTR_WINDOWS_SANDBOX_INHERIT_ENV");

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
}
