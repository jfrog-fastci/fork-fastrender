use std::path::PathBuf;
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, BOOL};
use windows_sys::Win32::System::JobObjects::{
  AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, JobObjectExtendedLimitInformation,
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
};
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, ProcessChildProcessPolicy, SetProcessMitigationPolicy,
  PROCESS_MITIGATION_CHILD_PROCESS_POLICY,
};

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

fn apply_child_process_policy() -> Result<(), String> {
  // `PROCESS_MITIGATION_CHILD_PROCESS_POLICY` bit 0 = NoChildProcessCreation.
  let policy = PROCESS_MITIGATION_CHILD_PROCESS_POLICY { Flags: 1 };
  // SAFETY: Windows API call; we pass a valid pointer/length to a POD struct.
  let ok: BOOL = unsafe {
    SetProcessMitigationPolicy(
      ProcessChildProcessPolicy,
      &policy as *const _ as *const std::ffi::c_void,
      std::mem::size_of::<PROCESS_MITIGATION_CHILD_PROCESS_POLICY>(),
    )
  };
  if ok == 0 {
    return Err(format!(
      "SetProcessMitigationPolicy(ProcessChildProcessPolicy) failed: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok(())
}

struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for JobHandle {
  fn drop(&mut self) {
    // SAFETY: closing an owned HANDLE.
    unsafe {
      let _ = CloseHandle(self.0);
    }
  }
}

fn apply_job_active_process_limit_one() -> Result<JobHandle, String> {
  // SAFETY: Windows API call; passing null security attributes + name creates an unnamed job object.
  let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
  if job == 0 {
    return Err(format!(
      "CreateJobObjectW failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
  info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
  info.BasicLimitInformation.ActiveProcessLimit = 1;

  // SAFETY: Windows API call; struct is initialized and buffer length matches.
  let ok: BOOL = unsafe {
    SetInformationJobObject(
      job,
      JobObjectExtendedLimitInformation,
      &mut info as *mut _ as *mut std::ffi::c_void,
      std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    )
  };
  if ok == 0 {
    return Err(format!(
      "SetInformationJobObject(JobObjectExtendedLimitInformation) failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  // SAFETY: `GetCurrentProcess` returns a pseudo-handle for the current process.
  let ok: BOOL = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
  if ok == 0 {
    return Err(format!(
      "AssignProcessToJobObject failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  Ok(JobHandle(job))
}

fn apply_no_child_process_sandbox() -> Result<Option<JobHandle>, String> {
  let mut applied_any = false;
  let mut job_guard = None;

  match apply_child_process_policy() {
    Ok(()) => applied_any = true,
    Err(err) => {
      eprintln!("warning: failed to apply child-process mitigation policy: {err}");
    }
  }

  match apply_job_active_process_limit_one() {
    Ok(handle) => {
      job_guard = Some(handle);
      applied_any = true;
    }
    Err(err) => {
      eprintln!("warning: failed to apply job object active-process limit: {err}");
    }
  }

  if applied_any {
    Ok(job_guard)
  } else {
    Err("failed to apply any Windows no-child-process sandbox policy".to_string())
  }
}

#[test]
fn sandboxed_renderer_cannot_spawn_child_process() {
  let cmd_exe = cmd_exe_path().expect("determine cmd.exe path");

  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let _job_guard =
      apply_no_child_process_sandbox().expect("apply Windows no-child-process sandbox policy");

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
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandboxed child test process");

  assert!(
    output.status.success(),
    "sandboxed child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
