#![cfg(windows)]

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};

use win_sandbox::Job;

use windows_sys::Win32::System::Threading::WaitForSingleObject;

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

  job.assign_process(&child).expect("assign process to job");

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

  job.assign_process(&child).expect("assign process to job");

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
