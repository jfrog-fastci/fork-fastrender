//! Windows sandbox regression test: ensure `KILL_ON_JOB_CLOSE` is enforced for sandboxed children.
//!
//! FastRender's Windows sandbox spawns renderer processes inside a Job object with:
//! - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (lifecycle safety; renderer dies if browser dies)
//! - `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (defence in depth; prevent fork bombs)
//!
//! This test validates the first property by spawning a sandboxed child that would otherwise sleep
//! for a long time, dropping the job handle, and asserting the process is terminated promptly.

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use fastrender::sandbox::windows::spawn_sandboxed;
use win_sandbox::SandboxSupport;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{
  GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const STILL_ACTIVE: u32 = 259;

const CHILD_TIMEOUT_MS: u32 = 10_000;

#[test]
fn sandbox_job_kill_on_close_terminates_child_process() {
  let support = SandboxSupport::detect();
  match support {
    SandboxSupport::Full | SandboxSupport::NoAppContainer => {}
    other => {
      eprintln!("skipping JobObject kill-on-close test: nested jobs are unavailable ({other})");
      return;
    }
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandbox::windows_job_kill_on_close::sandbox_job_kill_on_close_child";

  // Run the child test only when explicitly requested via `--ignored`.
  let args = vec![
    OsString::from("--ignored"),
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let child = {
    // This regression test is about JobObject `KILL_ON_JOB_CLOSE`, which should be applied even if
    // token/AppContainer sandboxing is disabled (debug escape hatch). Disable AppContainer so the
    // test can still run on hosts where AppContainer is unavailable.
    let _env_guard = crate::common::EnvVarsGuard::new(&[
      ("FASTR_DISABLE_RENDERER_SANDBOX", Some("1")),
      ("FASTR_WINDOWS_RENDERER_SANDBOX", None),
      ("FASTR_ALLOW_UNSANDBOXED_RENDERER", None),
      ("FASTR_WINDOWS_SANDBOX_INHERIT_ENV", None),
    ]);
    spawn_sandboxed(&exe, &args, &[]).expect("spawn sandboxed child")
  };

  // Keep the process handle alive so we can observe termination after closing the job handle.
  let fastrender::sandbox::windows::SandboxedChild {
    process,
    job,
    pid,
    level,
  } = child;
  let Some(job) = job else {
    panic!(
      "sandboxed child was not assigned to a JobObject (pid={pid}, level={level:?}); cannot validate KILL_ON_JOB_CLOSE semantics"
    );
  };

  let handle = process.as_raw_handle() as HANDLE;

  // Ensure the child is still running before we drop the job handle; otherwise this test could
  // "pass" without validating kill-on-close semantics.
  let wait0 = unsafe { WaitForSingleObject(handle, 0) };
  if wait0 == WAIT_OBJECT_0 {
    let mut exit_code: u32 = 0;
    let _ = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    panic!(
      "sandbox child exited before kill-on-close assertion (pid={pid}, level={level:?}, exit_code={exit_code})"
    );
  }

  // Drop the job handle: `KILL_ON_JOB_CLOSE` should terminate the process tree.
  drop(job);

  let wait = unsafe { WaitForSingleObject(handle, CHILD_TIMEOUT_MS) };
  match wait {
    WAIT_OBJECT_0 => {
      // Process terminated. Exit code is not guaranteed (job termination is abrupt), but it should
      // not still be active.
      let mut exit_code: u32 = STILL_ACTIVE;
      let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
      if ok != 0 {
        assert_ne!(
          exit_code, STILL_ACTIVE,
          "expected child to be terminated after job close (pid={pid}, level={level:?})"
        );
      }
    }
    WAIT_TIMEOUT => {
      unsafe {
        // Best-effort cleanup to avoid leaving a sleeping child behind.
        let _ = TerminateProcess(handle, 1);
      }
      panic!(
        "child was not terminated by job close within {CHILD_TIMEOUT_MS}ms (pid={pid}, level={level:?})"
      );
    }
    other => {
      unsafe {
        let _ = TerminateProcess(handle, 1);
      }
      panic!("unexpected WaitForSingleObject result {other} (pid={pid}, level={level:?})");
    }
  }
}

#[test]
#[ignore]
fn sandbox_job_kill_on_close_child() {
  // The parent test drops the Job handle and expects us to be terminated. If kill-on-close regresses,
  // this child will run for a long time and the parent will time out + fail.
  std::thread::sleep(Duration::from_secs(120));
}
