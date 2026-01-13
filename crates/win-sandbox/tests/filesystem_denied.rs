#![cfg(windows)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::ExitStatusExt;

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

const CHILD_ENV: &str = "FASTR_WIN_SANDBOX_TEST_CHILD";
const PATH_ENV: &str = "FASTR_WIN_SANDBOX_TEST_PATH";

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

struct EnvRestore {
  prev_disable: Option<OsString>,
  prev_legacy: Option<OsString>,
  prev_child: Option<OsString>,
  prev_path: Option<OsString>,
  prev_threads: Option<OsString>,
}

impl EnvRestore {
  fn install(file_path: &Path) -> Self {
    let prev_disable = std::env::var_os("FASTR_DISABLE_RENDERER_SANDBOX");
    let prev_legacy = std::env::var_os("FASTR_WINDOWS_RENDERER_SANDBOX");
    let prev_child = std::env::var_os(CHILD_ENV);
    let prev_path = std::env::var_os(PATH_ENV);
    let prev_threads = std::env::var_os("RUST_TEST_THREADS");

    std::env::remove_var("FASTR_DISABLE_RENDERER_SANDBOX");
    std::env::remove_var("FASTR_WINDOWS_RENDERER_SANDBOX");
    std::env::set_var(CHILD_ENV, "1");
    std::env::set_var(PATH_ENV, file_path);
    std::env::set_var("RUST_TEST_THREADS", "1");

    Self {
      prev_disable,
      prev_legacy,
      prev_child,
      prev_path,
      prev_threads,
    }
  }
}

impl Drop for EnvRestore {
  fn drop(&mut self) {
    restore_var("FASTR_DISABLE_RENDERER_SANDBOX", self.prev_disable.take());
    restore_var("FASTR_WINDOWS_RENDERER_SANDBOX", self.prev_legacy.take());
    restore_var(CHILD_ENV, self.prev_child.take());
    restore_var(PATH_ENV, self.prev_path.take());
    restore_var("RUST_TEST_THREADS", self.prev_threads.take());
  }
}

fn restore_var(key: &str, prev: Option<OsString>) {
  match prev {
    Some(value) => std::env::set_var(key, value),
    None => std::env::remove_var(key),
  }
}

#[test]
fn appcontainer_blocks_userprofile_filesystem_access() {
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    run_child();
  }

  static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  let _env_guard = ENV_LOCK.lock().unwrap();

  let user_profile = PathBuf::from(
    std::env::var_os("USERPROFILE").expect("USERPROFILE must be set on Windows"),
  );
  let file_path = user_profile.join(format!(
    "fastrender_sandbox_test_{}_{}.txt",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos()
  ));
  let marker = format!(
    "fastrender-sandbox-marker:{}",
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos()
  );

  fs::write(&file_path, marker.as_bytes()).expect("write marker under USERPROFILE");

  struct Cleanup(PathBuf);
  impl Drop for Cleanup {
    fn drop(&mut self) {
      let _ = fs::remove_file(&self.0);
    }
  }
  let _cleanup = Cleanup(file_path.clone());

  let _env_restore = EnvRestore::install(&file_path);

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "appcontainer_blocks_userprofile_filesystem_access";
  let args = vec![
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let child = spawn_sandboxed(&exe, &args, &[]).expect("spawn sandboxed child process");
  assert_eq!(
    child.level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandboxing to be used (not fallback)"
  );

  let handle = child.process.as_raw_handle() as HANDLE;
  let status = wait_process(handle, 20_000).expect("wait for sandboxed child");

  assert!(
    status.success(),
    "sandboxed child should exit 0 when filesystem access is denied (status={status:?})"
  );
}

fn wait_process(handle: HANDLE, timeout_ms: u32) -> std::io::Result<std::process::ExitStatus> {
  let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
  if wait == WAIT_TIMEOUT {
    return Err(std::io::Error::new(
      std::io::ErrorKind::TimedOut,
      "sandboxed child timed out",
    ));
  }
  if wait != WAIT_OBJECT_0 {
    return Err(std::io::Error::last_os_error());
  }

  let mut exit_code: u32 = 0;
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  if ok == 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(std::process::ExitStatus::from_raw(exit_code))
}

fn run_child() -> ! {
  let path = std::env::var_os(PATH_ENV).expect("child must receive file path in env");
  let path = Path::new(&path);

  match fs::read_to_string(path) {
    Ok(contents) => {
      eprintln!(
        "sandbox regression: AppContainer child read USERPROFILE file successfully: {} bytes",
        contents.len()
      );
      std::process::exit(1);
    }
    Err(err) => {
      let raw = err.raw_os_error();
      let allowed = matches!(
        raw,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 || code == ERROR_FILE_NOT_FOUND as i32
      );
      if allowed {
        std::process::exit(0);
      }
      eprintln!(
        "unexpected error when reading USERPROFILE file from sandboxed child: {err} (raw={raw:?})"
      );
      std::process::exit(1);
    }
  }
}
