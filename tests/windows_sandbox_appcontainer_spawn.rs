#![cfg(windows)]

use std::ffi::OsString;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::ExitStatusExt;

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};

fn wait_process(handle: HANDLE) -> io::Result<std::process::ExitStatus> {
  // SAFETY: caller owns the process handle and we wait indefinitely for it to signal.
  let wait_rc = unsafe { WaitForSingleObject(handle, INFINITE) };
  if wait_rc != 0 {
    return Err(io::Error::last_os_error());
  }

  let mut exit_code: u32 = 0;
  // SAFETY: `exit_code` is writable.
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  if ok == 0 {
    return Err(io::Error::last_os_error());
  }

  Ok(std::process::ExitStatus::from_raw(exit_code))
}

/// Child-side noop used by `appcontainer_spawn_can_execute_from_temp_dir`.
///
/// This runs in the sandboxed process (selected via `--exact`) and should exit successfully.
#[test]
fn appcontainer_child_smoke() {
  // Intentionally empty.
}

/// Regression test for developer builds on Windows where an AppContainer token cannot execute the
/// original test binary due to missing ACL entries.
///
/// This is environment-dependent (AppContainer support, filesystem ACL policy), so keep it ignored
/// by default.
#[test]
#[ignore]
fn appcontainer_spawn_can_execute_from_temp_dir() {
  let exe = std::env::current_exe().expect("current test exe path");
  let tmp = tempfile::tempdir().expect("temp dir");
  let copied = tmp
    .path()
    .join(exe.file_name().expect("test exe should have a file name"));
  std::fs::copy(&exe, &copied).expect("copy test exe to temp dir");

  let args = vec![
    OsString::from("--exact"),
    OsString::from("appcontainer_child_smoke"),
    OsString::from("--nocapture"),
  ];

  let child = spawn_sandboxed(&copied, &args, &[]).expect("spawn sandboxed child");
  assert_eq!(
    child.level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandboxing to succeed (not fall back)"
  );

  let handle = child.process.as_raw_handle() as HANDLE;
  let status = wait_process(handle).expect("wait for child");
  assert!(status.success(), "child should exit successfully");
}
