#![cfg(windows)]

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};

use win_sandbox::Job;

use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

fn assign_child(job: &Job, child: &std::process::Child) {
  let ok = unsafe {
    AssignProcessToJobObject(
      job.handle(),
      child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
    )
  };
  assert_ne!(ok, 0, "AssignProcessToJobObject failed");
}

#[test]
fn kill_on_close_terminates_processes() {
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

  assign_child(&job, &child);

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
  assert_ne!(wait, WAIT_TIMEOUT, "child was not terminated by job close");
  assert_eq!(wait, WAIT_OBJECT_0, "unexpected wait result: {wait}");

  let _ = child.wait();
}

#[test]
fn active_process_limit_blocks_grandchildren() {
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

  assign_child(&job, &child);

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
