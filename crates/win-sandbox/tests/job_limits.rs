#![cfg(windows)]

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};

use win_sandbox::{is_nested_job_supported, Job, WinSandboxError};

use windows_sys::Win32::System::Threading::WaitForSingleObject;

fn should_skip_job_assignment_error(err: &WinSandboxError) -> bool {
  match err {
    WinSandboxError::Win32 { code, .. } => {
      // When the parent process is already inside a Job that disallows nested jobs/breakaway, Windows
      // typically reports this as ERROR_ACCESS_DENIED. In that case, this is an environment
      // limitation (not a regression in Job support), so skip the test.
      *code == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED
    }
    _ => false,
  }
}

#[test]
fn kill_on_close_terminates_processes() {
  if !is_nested_job_supported() {
    eprintln!("skipping kill_on_close_terminates_processes: nested job support unavailable");
    return;
  }

  let job = Job::new(None).expect("create job");
  job.set_kill_on_close().expect("set kill-on-close");

  let helper = env!("CARGO_BIN_EXE_job_test_helper");
  let mut child = Command::new(helper)
    .arg("sleep")
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn child");

  match job.assign_process(&child) {
    Ok(()) => {}
    Err(err) if should_skip_job_assignment_error(&err) => {
      eprintln!("skipping kill_on_close_terminates_processes: cannot assign child to Job ({err})");
      let _ = child.kill();
      let _ = child.wait();
      return;
    }
    Err(err) => panic!("assign process to job: {err}"),
  }

  // Let the child proceed into its sleep loop.
  child
    .stdin
    .as_mut()
    .unwrap()
    .write_all(b"go\n")
    .expect("signal child");

  drop(job); // Should terminate the child process.

  let wait = unsafe {
    WaitForSingleObject(
      child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
      10_000,
    )
  };
  const WAIT_OBJECT_0: u32 = 0x0000_0000;
  const WAIT_TIMEOUT: u32 = 0x0000_0102;
  match wait {
    WAIT_OBJECT_0 => {
      let _ = child.wait();
    }
    WAIT_TIMEOUT => {
      let _ = child.kill();
      let _ = child.wait();
      panic!("child was not terminated by job close");
    }
    other => {
      let _ = child.kill();
      let _ = child.wait();
      panic!("unexpected wait result: {other}");
    }
  }
}

#[test]
fn active_process_limit_blocks_grandchildren() {
  if !is_nested_job_supported() {
    eprintln!("skipping active_process_limit_blocks_grandchildren: nested job support unavailable");
    return;
  }

  let job = Job::new(None).expect("create job");
  job.set_kill_on_close().expect("set kill-on-close");
  job
    .set_active_process_limit(1)
    .expect("set active process limit");

  let helper = env!("CARGO_BIN_EXE_job_test_helper");
  let mut child = Command::new(helper)
    .arg("spawn-grandchild")
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn child");

  match job.assign_process(&child) {
    Ok(()) => {}
    Err(err) if should_skip_job_assignment_error(&err) => {
      eprintln!(
        "skipping active_process_limit_blocks_grandchildren: cannot assign child to Job ({err})"
      );
      let _ = child.kill();
      let _ = child.wait();
      return;
    }
    Err(err) => panic!("assign process to job: {err}"),
  }

  child
    .stdin
    .as_mut()
    .unwrap()
    .write_all(b"go\n")
    .expect("signal child");

  let status = child.wait().expect("wait for child");
  assert!(
    status.success(),
    "child unexpectedly spawned a grandchild (exit={status})"
  );

  drop(job);
}
