#![cfg(windows)]

use std::ffi::OsString;
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::ExitStatusExt;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, TerminateProcess, WaitForSingleObject};

const FILE_NETWORK_TEST_NAME: &str = concat!(module_path!(), "::appcontainer_denies_filesystem_and_network");
const JOB_KILL_TEST_NAME: &str =
  concat!(module_path!(), "::job_object_kill_on_close_terminates_child");

fn process_handle(process: &std::os::windows::io::OwnedHandle) -> HANDLE {
  process.as_raw_handle() as HANDLE
}

fn exit_code(process: &std::os::windows::io::OwnedHandle) -> std::io::Result<u32> {
  let mut code: u32 = 0;
  // SAFETY: process handle is valid for lifetime of `OwnedHandle`.
  let ok = unsafe { GetExitCodeProcess(process_handle(process), &mut code) };
  if ok == 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(code)
}

fn wait_for_exit_status(
  process: &std::os::windows::io::OwnedHandle,
  timeout: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
  let ms: u32 = timeout
    .as_millis()
    .min(u128::from(u32::MAX))
    .try_into()
    .unwrap_or(u32::MAX);
  // SAFETY: process handle is valid.
  let rc = unsafe { WaitForSingleObject(process_handle(process), ms) };
  match rc {
    WAIT_OBJECT_0 => {
      let code = exit_code(process)?;
      Ok(Some(std::process::ExitStatus::from_raw(code)))
    }
    WAIT_TIMEOUT => Ok(None),
    WAIT_FAILED => Err(std::io::Error::last_os_error()),
    other => Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("WaitForSingleObject returned unexpected value {other}"),
    )),
  }
}

fn wait_for_exit_or_kill(
  process: &std::os::windows::io::OwnedHandle,
  timeout: Duration,
  context: &str,
) -> std::process::ExitStatus {
  match wait_for_exit_status(process, timeout).expect("wait for sandboxed child process") {
    Some(status) => status,
    None => {
      // SAFETY: process handle is valid.
      let _ = unsafe { TerminateProcess(process_handle(process), 1) };
      panic!("timeout waiting for sandboxed child to exit ({context})");
    }
  }
}

#[test]
fn appcontainer_denies_filesystem_and_network() {
  const CHILD_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_SANDBOX_CHILD";
  const FILE_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_SANDBOX_FILE";
  const PORT_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_SANDBOX_PORT";

  if std::env::var_os(CHILD_ENV).is_some() {
    let file_path = std::env::var_os(FILE_ENV).expect("child missing file path env");
    let port_raw = std::env::var(PORT_ENV).expect("child missing port env");
    let port: u16 = port_raw.parse().expect("port should parse as u16");

    let path = std::path::PathBuf::from(file_path);

    match std::fs::read_to_string(&path) {
      Ok(contents) => panic!(
        "expected AppContainer sandbox to deny reading {path:?}, but read {len} bytes: {contents:?}",
        len = contents.len()
      ),
      Err(err) => {
        assert!(
          err.kind() == std::io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(5),
          "expected read_to_string({path:?}) to fail with PermissionDenied/ERROR_ACCESS_DENIED(5), got {err:?}"
        );
      }
    }

    match TcpStream::connect(("127.0.0.1", port)) {
      Ok(_) => panic!(
        "expected AppContainer sandbox to deny TcpStream::connect to 127.0.0.1:{port}"
      ),
      Err(err) => {
        assert!(
          err.kind() == std::io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(10013),
          "expected connect to fail with PermissionDenied/WSAEACCES(10013), got {err:?}"
        );
      }
    }

    return;
  }

  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let file_path = temp_dir.path().join("fastrender_windows_sandbox_probe.txt");
  std::fs::write(&file_path, "fastrender sandbox probe").expect("write probe file");
  let parent_contents =
    std::fs::read_to_string(&file_path).expect("parent should be able to read probe file");
  assert_eq!(parent_contents, "fastrender sandbox probe");

  let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind localhost listener");
  let port = listener.local_addr().expect("listener addr").port();

  let exe = std::env::current_exe().expect("current test executable path");
  let args = vec![
    OsString::from("--exact"),
    OsString::from(FILE_NETWORK_TEST_NAME),
    OsString::from("--nocapture"),
  ];
  let child = {
    let _child_env = crate::common::EnvVarGuard::set(CHILD_ENV, "1");
    let _file_env = crate::common::EnvVarGuard::set(FILE_ENV, file_path.as_os_str());
    let _port_env = crate::common::EnvVarGuard::set(PORT_ENV, port.to_string());
    fastrender::sandbox::windows::spawn_sandboxed(&exe, &args, &[])
      .expect("spawn sandboxed child process")
  };

  // Keep the listener alive for the duration of the child probe so `ECONNREFUSED` isn't a false
  // positive.
  let _listener_guard = listener;

  let status = wait_for_exit_or_kill(
    &child.process,
    Duration::from_secs(10),
    "appcontainer probe",
  );
  assert!(
    status.success(),
    "sandboxed probe child should exit successfully (status={status:?})"
  );
}

#[test]
fn job_object_kill_on_close_terminates_child() {
  const CHILD_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_JOB_CHILD";

  if std::env::var_os(CHILD_ENV).is_some() {
    loop {
      std::thread::sleep(Duration::from_secs(1));
    }
  }

  let exe = std::env::current_exe().expect("current test executable path");
  let args = vec![
    OsString::from("--exact"),
    OsString::from(JOB_KILL_TEST_NAME),
    OsString::from("--nocapture"),
  ];
  let fastrender::sandbox::windows::SandboxedChild { process, job, .. } = {
    let _child_env = crate::common::EnvVarGuard::set(CHILD_ENV, "1");
    fastrender::sandbox::windows::spawn_sandboxed(&exe, &args, &[])
      .expect("spawn sandboxed child process")
  };

  // Ensure the child is actually running (otherwise a crash could make this test pass trivially).
  std::thread::sleep(Duration::from_millis(200));
  assert!(
    wait_for_exit_status(&process, Duration::from_millis(0))
      .expect("poll sandboxed child")
      .is_none(),
    "expected child to still be running before job is closed"
  );

  drop(job);

  let status = match wait_for_exit_status(&process, Duration::from_secs(3))
    .expect("wait for child to terminate after closing job")
  {
    Some(status) => status,
    None => {
      let _ = unsafe { TerminateProcess(process_handle(&process), 1) };
      panic!("expected JobObject kill-on-close to terminate child within timeout");
    }
  };

  assert!(
    !status.success(),
    "child should not exit cleanly when terminated by JobObject (status={status:?})"
  );
}
