#![cfg(windows)]

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

#[test]
fn appcontainer_temp_dir_is_writable() {
  // Ensure the sandbox isn't disabled by a developer's env overrides.
  std::env::remove_var("FASTR_DISABLE_RENDERER_SANDBOX");
  std::env::remove_var("FASTR_WINDOWS_RENDERER_SANDBOX");

  let child_exe = Path::new(env!("CARGO_BIN_EXE_appcontainer_temp_smoke"));
  let child = spawn_sandboxed(child_exe, &[], &[]).expect("spawn appcontainer child");
  assert_eq!(
    child.level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandbox level, got {:?}",
    child.level
  );

  let handle = child.process.as_raw_handle() as HANDLE;
  const TIMEOUT_MS: u32 = 30_000;
  let wait = unsafe { WaitForSingleObject(handle, TIMEOUT_MS) };
  assert_ne!(wait, WAIT_TIMEOUT, "sandboxed child timed out");
  assert_eq!(wait, WAIT_OBJECT_0, "WaitForSingleObject failed: {wait}");

  let mut exit_code: u32 = 0;
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  assert_ne!(
    ok,
    0,
    "GetExitCodeProcess failed: {}",
    std::io::Error::last_os_error()
  );
  assert_eq!(
    exit_code, 0,
    "expected child to exit cleanly, got code {exit_code}"
  );
}
