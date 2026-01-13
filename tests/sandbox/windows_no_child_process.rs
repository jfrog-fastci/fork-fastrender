#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::Command;

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::{
  SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

const CHILD_ENV: &str = "FASTR_TEST_WIN_SANDBOX_NO_CHILD_PROCESS_CHILD";

// Windows error codes we explicitly treat as "cmd.exe could not be found" rather than "sandbox
// blocked process creation".
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;

fn cmd_exe_path() -> Result<PathBuf, String> {
  if let Some(spec) = std::env::var_os("ComSpec") {
    let path = PathBuf::from(spec);
    if path.is_file() {
      return Ok(path);
    }
  }

  if let Some(root) = std::env::var_os("SystemRoot") {
    let path = PathBuf::from(root).join("System32").join("cmd.exe");
    if path.is_file() {
      return Ok(path);
    }
  }

  // Fall back to relying on PATH resolution. This should still work on normal Windows installs.
  Ok(PathBuf::from("cmd.exe"))
}

fn assert_cmd_spawn_works(cmd_exe: &PathBuf) {
  let status = Command::new(cmd_exe)
    .arg("/C")
    .arg("exit 0")
    .status()
    .unwrap_or_else(|err| {
      panic!(
        "expected cmd.exe to be spawnable in the unsandboxed parent process; cmd={}, err={} (raw_os_error={:?})",
        cmd_exe.display(),
        err,
        err.raw_os_error()
      )
    });
  assert!(
    status.success(),
    "expected cmd.exe to exit successfully in the unsandboxed parent process; cmd={}, status={:?}",
    cmd_exe.display(),
    status
  );
}

fn std_handle(kind: u32) -> Option<HANDLE> {
  // SAFETY: Win32 call; returns INVALID_HANDLE_VALUE / null on error.
  let h = unsafe { GetStdHandle(kind) };
  if h == 0 || h == INVALID_HANDLE_VALUE {
    None
  } else {
    Some(h)
  }
}

fn collect_inheritable_std_handles() -> Vec<std::os::windows::io::RawHandle> {
  let mut handles: Vec<HANDLE> = Vec::new();
  for h in [
    std_handle(STD_INPUT_HANDLE),
    std_handle(STD_OUTPUT_HANDLE),
    std_handle(STD_ERROR_HANDLE),
  ]
  .into_iter()
  .flatten()
  {
    if !handles.contains(&h) {
      // Ensure the handle is inheritable so it can be included in the restricted handle list.
      //
      // Best-effort: some environments may not allow changing inherit flags on std handles.
      // SAFETY: Win32 call; valid handle value.
      let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
      handles.push(h);
    }
  }

  handles.into_iter().map(|h| h as _).collect()
}

#[test]
fn sandboxed_renderer_cannot_spawn_child_process() {
  let cmd_exe = cmd_exe_path().expect("determine cmd.exe path");

  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    match Command::new(&cmd_exe).arg("/C").arg("exit 0").status() {
      Ok(status) => panic!(
        "sandbox allowed spawning a child process (cmd.exe). cmd={}, status={:?}",
        cmd_exe.display(),
        status
      ),
      Err(err) => {
        let raw = err.raw_os_error();
        eprintln!(
          "CreateProcess blocked as expected. cmd={}, err={} (raw_os_error={raw:?})",
          cmd_exe.display(),
          err
        );
        if matches!(raw, Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)) {
          panic!(
            "cmd.exe could not be resolved (raw_os_error={raw:?}); this is not a sandbox failure. cmd={}",
            cmd_exe.display()
          );
        }
      }
    }
    return;
  }

  // Sanity check: in the normal (unsandboxed) test process, cmd.exe should spawn successfully. If
  // this fails, the regression test could pass for the wrong reason.
  assert_cmd_spawn_works(&cmd_exe);

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandbox::windows_no_child_process::sandboxed_renderer_cannot_spawn_child_process";

  let inherit_handles = collect_inheritable_std_handles();
  let args = vec![
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let child = crate::common::with_env_vars(&[(CHILD_ENV, "1")], || {
    spawn_sandboxed(&exe, &args, &inherit_handles).expect("spawn sandboxed child test process")
  });
  assert_ne!(
    child.level,
    WindowsSandboxLevel::None,
    "spawn_sandboxed returned WindowsSandboxLevel::None; sandbox may be disabled or unavailable"
  );

  let timeout_ms: u32 = 10_000;
  // SAFETY: waiting on a valid process handle.
  let wait_rc = unsafe { WaitForSingleObject(child.process.as_raw_handle() as HANDLE, timeout_ms) };
  if wait_rc != 0 {
    panic!(
      "sandboxed child did not exit cleanly within {timeout_ms}ms (WaitForSingleObject rc={wait_rc})"
    );
  }

  let mut exit_code: u32 = 0;
  // SAFETY: querying exit code for a valid process handle.
  let ok = unsafe { GetExitCodeProcess(child.process.as_raw_handle() as HANDLE, &mut exit_code) };
  assert!(ok != 0, "GetExitCodeProcess failed");
  assert_eq!(
    exit_code, 0,
    "sandboxed child process exited non-zero (exit_code={exit_code}, pid={}, level={:?})",
    child.pid, child.level
  );
}
