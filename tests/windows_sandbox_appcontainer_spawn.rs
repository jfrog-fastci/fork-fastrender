#![cfg(windows)]

use std::ffi::OsString;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::ExitStatusExt;
use std::process::Command;

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

#[test]
fn disable_renderer_sandbox_logs_warning() {
  const CHILD_ENV: &str = "FASTR_TEST_DISABLE_RENDERER_SANDBOX_LOG_CHILD";
  const DISABLE_ENV: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

  if std::env::var_os(CHILD_ENV).is_some() {
    // This runs in the child process; the parent captures stderr to ensure the warning is printed.
    std::env::set_var(DISABLE_ENV, "1");

    let exe = std::env::current_exe().expect("current test exe path");
    let args = vec![
      OsString::from("--exact"),
      OsString::from("appcontainer_child_smoke"),
      OsString::from("--nocapture"),
    ];
    let child = spawn_sandboxed(&exe, &args, &[]).expect("spawn sandboxed child");
    assert_eq!(child.level, WindowsSandboxLevel::None);
    let handle = child.process.as_raw_handle() as HANDLE;
    let status = wait_process(handle).expect("wait for child");
    assert!(status.success(), "child should exit successfully");
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "disable_renderer_sandbox_logs_warning";
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .env(DISABLE_ENV, "1")
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn child test process");

  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("Windows renderer sandbox is DISABLED"),
    "expected sandbox disable warning in stderr (stderr={stderr})"
  );
}

#[test]
fn disable_renderer_sandbox_env_forces_unsandboxed_spawn() {
  let _guard = ENV_LOCK.lock().unwrap();

  const DISABLE_ENV: &str = "FASTR_DISABLE_RENDERER_SANDBOX";
  const LEGACY_ENV: &str = "FASTR_WINDOWS_RENDERER_SANDBOX";

  let prev_disable = std::env::var_os(DISABLE_ENV);
  let prev_legacy = std::env::var_os(LEGACY_ENV);

  let exe = std::env::current_exe().expect("current test exe path");
  let args = vec![
    OsString::from("--exact"),
    OsString::from("appcontainer_child_smoke"),
    OsString::from("--nocapture"),
  ];

  let spawn_and_wait = || {
    let child = spawn_sandboxed(&exe, &args, &[]).expect("spawn sandboxed child");
    assert_eq!(
      child.level,
      WindowsSandboxLevel::None,
      "expected sandbox opt-out to force unsandboxed spawn"
    );

    let handle = child.process.as_raw_handle() as HANDLE;
    let status = wait_process(handle).expect("wait for child");
    assert!(status.success(), "child should exit successfully");
  };

  // Primary opt-out env var.
  std::env::set_var(DISABLE_ENV, "1");
  std::env::remove_var(LEGACY_ENV);
  spawn_and_wait();

  // Legacy spelling.
  std::env::remove_var(DISABLE_ENV);
  std::env::set_var(LEGACY_ENV, "off");
  spawn_and_wait();

  match prev_disable {
    Some(value) => std::env::set_var(DISABLE_ENV, value),
    None => std::env::remove_var(DISABLE_ENV),
  }
  match prev_legacy {
    Some(value) => std::env::set_var(LEGACY_ENV, value),
    None => std::env::remove_var(LEGACY_ENV),
  }
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
