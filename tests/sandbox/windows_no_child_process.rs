#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::Command;

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::{
  GetHandleInformation, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
  GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

const CHILD_ENV: &str = "FASTR_TEST_WIN_SANDBOX_NO_CHILD_PROCESS_CHILD";
const CMD_ENV: &str = "FASTR_TEST_WIN_SANDBOX_CMD_EXE";
const DISABLE_SANDBOX_ENV: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

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

fn cmd_exe_path_for_child() -> PathBuf {
  if let Some(cmd) = std::env::var_os(CMD_ENV) {
    if !cmd.is_empty() {
      return PathBuf::from(cmd);
    }
  }

  if let Some(spec) = std::env::var_os("ComSpec") {
    if !spec.is_empty() {
      return PathBuf::from(spec);
    }
  }

  if let Some(root) = std::env::var_os("SystemRoot") {
    if !root.is_empty() {
      return PathBuf::from(root).join("System32").join("cmd.exe");
    }
  }

  PathBuf::from("cmd.exe")
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

struct HandleInheritGuard {
  saved: Vec<(HANDLE, u32)>,
}

impl HandleInheritGuard {
  fn new(handles: &[HANDLE]) -> Self {
    let mut saved = Vec::with_capacity(handles.len());
    for handle in handles {
      let mut flags: u32 = 0;
      // SAFETY: Win32 call; `flags` points to a valid output location.
      let ok = unsafe { GetHandleInformation(*handle, &mut flags) };
      if ok == 0 {
        continue;
      }
      saved.push((*handle, flags));
      // SAFETY: Win32 call; valid handle value.
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
      // SAFETY: Win32 call; valid handle value.
      let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, inherit) };
    }
  }
}

fn collect_std_handles() -> Vec<HANDLE> {
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
      handles.push(h);
    }
  }

  handles
}

#[test]
fn sandboxed_renderer_cannot_spawn_child_process() {
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let cmd_exe = cmd_exe_path_for_child();
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

  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "sandboxed_renderer_cannot_spawn_child_process",
  ) {
    return;
  }

  // Ensure developer environment overrides don't silently change test semantics.
  let _env_guard = crate::common::EnvVarsGuard::remove(&[
    "FASTR_DISABLE_RENDERER_SANDBOX",
    "FASTR_WINDOWS_RENDERER_SANDBOX",
    "FASTR_ALLOW_UNSANDBOXED_RENDERER",
    "FASTR_DISABLE_WIN_MITIGATIONS",
    "FASTR_WINDOWS_SANDBOX_INHERIT_ENV",
  ]);

  let cmd_exe = cmd_exe_path().expect("determine cmd.exe path");
  // Sanity check: in the normal (unsandboxed) test process, cmd.exe should spawn successfully. If
  // this fails, the regression test could pass for the wrong reason.
  assert_cmd_spawn_works(&cmd_exe);

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name =
    "sandbox::windows_no_child_process::sandboxed_renderer_cannot_spawn_child_process";

  let std_handles = collect_std_handles();
  let inherit_handles: Vec<std::os::windows::io::RawHandle> =
    std_handles.iter().copied().map(|h| h as _).collect();
  let args = vec![
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let cmd_exe_env = cmd_exe.to_string_lossy();
  let child = crate::common::with_env_vars(&[(CHILD_ENV, "1"), (CMD_ENV, &cmd_exe_env)], || {
    let _inherit_guard = HandleInheritGuard::new(&std_handles);
    // The Windows sandbox may not have access to the repository working directory; use System32 as a
    // conservative working directory during process creation.
    let _cwd_guard = if let Some(root) = std::env::var_os("SystemRoot") {
      let system32 = PathBuf::from(root).join("System32");
      crate::common::CurrentDirGuard::set(&system32).ok()
    } else {
      None
    };
    let child = spawn_sandboxed(&exe, &args, &inherit_handles);
    drop(_cwd_guard);
    child.expect("spawn sandboxed child test process")
  });

  assert_eq!(
    child.level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandboxing (no silent fallback)"
  );

  // SAFETY: waiting on a valid process handle.
  let timeout_ms: u32 = 30_000;
  let wait_rc = unsafe { WaitForSingleObject(child.process.as_raw_handle() as HANDLE, timeout_ms) };
  match wait_rc {
    0 => {}
    // WAIT_TIMEOUT
    0x0000_0102 => {
      // Best-effort cleanup so we don't leak a hung sandbox process.
      unsafe {
        let _ = TerminateProcess(child.process.as_raw_handle() as HANDLE, 1);
      }
      panic!("sandboxed child did not exit within {timeout_ms}ms");
    }
    // WAIT_FAILED
    u32::MAX => {
      let err = std::io::Error::last_os_error();
      panic!("WaitForSingleObject failed: {err}");
    }
    other => panic!("WaitForSingleObject returned unexpected code {other}"),
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

#[test]
fn job_object_still_blocks_child_process_when_sandbox_disabled() {
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let cmd_exe = cmd_exe_path_for_child();
    match Command::new(&cmd_exe).arg("/C").arg("exit 0").status() {
      Ok(status) => panic!(
        "job object did not block spawning a child process (cmd.exe). cmd={}, status={:?}",
        cmd_exe.display(),
        status
      ),
      Err(err) => {
        let raw = err.raw_os_error();
        eprintln!(
          "CreateProcess blocked as expected (sandbox disabled). cmd={}, err={} (raw_os_error={raw:?})",
          cmd_exe.display(),
          err
        );
        if matches!(raw, Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)) {
          panic!(
            "cmd.exe could not be resolved (raw_os_error={raw:?}); this is not a Job Object failure. cmd={}",
            cmd_exe.display()
          );
        }
      }
    }
    return;
  }

  let cmd_exe = cmd_exe_path().expect("determine cmd.exe path");
  // Sanity check: cmd.exe should spawn successfully in the unsandboxed parent process.
  assert_cmd_spawn_works(&cmd_exe);

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name =
    "sandbox::windows_no_child_process::job_object_still_blocks_child_process_when_sandbox_disabled";

  let std_handles = collect_std_handles();
  let inherit_handles: Vec<std::os::windows::io::RawHandle> =
    std_handles.iter().copied().map(|h| h as _).collect();
  let args = vec![
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let cmd_exe_env = cmd_exe.to_string_lossy();
  let child = crate::common::with_env_vars(
    &[
      (CHILD_ENV, "1"),
      (CMD_ENV, &cmd_exe_env),
      // Disable token/AppContainer sandboxing but keep the job object guardrails.
      (DISABLE_SANDBOX_ENV, "1"),
    ],
    || {
      let _inherit_guard = HandleInheritGuard::new(&std_handles);
      // The Windows sandbox may not have access to the repository working directory; use System32 as a
      // conservative working directory during process creation.
      if let Some(root) = std::env::var_os("SystemRoot") {
        let system32 = PathBuf::from(root).join("System32");
        let _cwd_guard = crate::common::CurrentDirGuard::set(&system32).ok();
        let child = spawn_sandboxed(&exe, &args, &inherit_handles);
        drop(_cwd_guard);
        return child.expect("spawn sandbox-disabled child test process");
      }
      let child = spawn_sandboxed(&exe, &args, &inherit_handles);
      child.expect("spawn sandbox-disabled child test process")
    },
  );

  assert_eq!(
    child.level,
    WindowsSandboxLevel::None,
    "expected sandbox opt-out to spawn an unsandboxed child (job object still applied)"
  );

  let timeout_ms: u32 = 10_000;
  // SAFETY: waiting on a valid process handle.
  let wait_rc = unsafe { WaitForSingleObject(child.process.as_raw_handle() as HANDLE, timeout_ms) };
  if wait_rc != 0 {
    panic!(
      "sandbox-disabled child did not exit cleanly within {timeout_ms}ms (WaitForSingleObject rc={wait_rc})"
    );
  }

  let mut exit_code: u32 = 0;
  // SAFETY: querying exit code for a valid process handle.
  let ok = unsafe { GetExitCodeProcess(child.process.as_raw_handle() as HANDLE, &mut exit_code) };
  assert!(ok != 0, "GetExitCodeProcess failed");
  assert_eq!(
    exit_code, 0,
    "sandbox-disabled child process exited non-zero (exit_code={exit_code}, pid={}, level={:?})",
    child.pid, child.level
  );
}
