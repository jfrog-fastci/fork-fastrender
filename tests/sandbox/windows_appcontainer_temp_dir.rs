#![cfg(windows)]

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use std::os::windows::io::RawHandle;
use std::path::Path;
use tempfile::TempDir;
use windows_sys::Win32::Foundation::{
  GetHandleInformation, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

struct HandleInheritGuard {
  saved: Vec<(HANDLE, u32)>,
}

impl HandleInheritGuard {
  fn new(handles: &[HANDLE]) -> Self {
    let mut saved = Vec::with_capacity(handles.len());
    for handle in handles {
      if *handle == 0 || *handle == INVALID_HANDLE_VALUE {
        continue;
      }
      let mut flags: u32 = 0;
      let ok = unsafe { GetHandleInformation(*handle, &mut flags) };
      if ok == 0 {
        continue;
      }
      saved.push((*handle, flags));
      let _ = unsafe { SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    Self { saved }
  }
}

impl Drop for HandleInheritGuard {
  fn drop(&mut self) {
    for (handle, flags) in self.saved.drain(..) {
      let inherit = if (flags & HANDLE_FLAG_INHERIT) != 0 {
        HANDLE_FLAG_INHERIT
      } else {
        0
      };
      let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, inherit) };
    }
  }
}

fn collect_stdio_handles_for_inheritance() -> (Vec<RawHandle>, HandleInheritGuard) {
  let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
  let std_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
  let std_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

  let mut handles: Vec<HANDLE> = Vec::new();
  for h in [std_in, std_out, std_err] {
    if h == 0 || h == INVALID_HANDLE_VALUE {
      continue;
    }
    if !handles.contains(&h) {
      handles.push(h);
    }
  }

  let guard = HandleInheritGuard::new(&handles);
  let inherit = handles.iter().copied().map(|h| h as RawHandle).collect();
  (inherit, guard)
}

#[test]
fn appcontainer_temp_dir_is_writable() {
  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "appcontainer_temp_dir_is_writable",
  ) {
    return;
  }

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
      "FASTR_ALLOW_UNSANDBOXED_RENDERER",
      "FASTR_DISABLE_WIN_MITIGATIONS",
      "FASTR_WINDOWS_SANDBOX_INHERIT_ENV",
    ]);
    let _temp_guard = crate::common::EnvVarGuard::set("TEMP", forced_parent_temp.path());
    let _tmp_guard = crate::common::EnvVarGuard::set("TMP", forced_parent_temp.path());
    let (inherit, _inherit_guard) = collect_stdio_handles_for_inheritance();
    spawn_sandboxed(child_exe, &[], &inherit).expect("spawn appcontainer child")
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
