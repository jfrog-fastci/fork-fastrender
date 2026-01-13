#![cfg(windows)]

mod common;

use std::ffi::{c_void, OsString};
use std::fs;
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};

use win_sandbox::mitigations;
use win_sandbox::RendererSandbox;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::{
  CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
};
use windows_sys::Win32::Security::{GetTokenInformation, TokenIsAppContainer, TOKEN_QUERY};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

const CHILD_ENV: &str = "FASTR_WIN_SANDBOX_TEST_CHILD";
const PATH_ENV: &str = "FASTR_WIN_SANDBOX_TEST_PATH";
const DISABLE_MITIGATIONS_ENV: &str = "FASTR_DISABLE_WIN_MITIGATIONS";

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

struct EnvRestore {
  prev_child: Option<OsString>,
  prev_path: Option<OsString>,
  prev_threads: Option<OsString>,
  prev_disable_mitigations: Option<OsString>,
}

impl EnvRestore {
  fn install(file_path: &Path) -> Self {
    let prev_child = std::env::var_os(CHILD_ENV);
    let prev_path = std::env::var_os(PATH_ENV);
    let prev_threads = std::env::var_os("RUST_TEST_THREADS");
    let prev_disable_mitigations = std::env::var_os(DISABLE_MITIGATIONS_ENV);

    std::env::set_var(CHILD_ENV, "1");
    std::env::set_var(PATH_ENV, file_path);
    std::env::set_var("RUST_TEST_THREADS", "1");
    std::env::remove_var(DISABLE_MITIGATIONS_ENV);

    Self {
      prev_child,
      prev_path,
      prev_threads,
      prev_disable_mitigations,
    }
  }
}

impl Drop for EnvRestore {
  fn drop(&mut self) {
    restore_var(CHILD_ENV, self.prev_child.take());
    restore_var(PATH_ENV, self.prev_path.take());
    restore_var("RUST_TEST_THREADS", self.prev_threads.take());
    restore_var(
      DISABLE_MITIGATIONS_ENV,
      self.prev_disable_mitigations.take(),
    );
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

  if !common::require_appcontainer_profile("win-sandbox AppContainer filesystem denial test") {
    return;
  }

  let user_profile =
    PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE must be set on Windows"));
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

  let sandbox = RendererSandbox::appcontainer_no_capabilities();
  let child = sandbox
    .spawn(&exe, &args)
    .expect("spawn sandboxed child process");
  let handle = child.process.as_raw();
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
  if !current_process_is_appcontainer() {
    eprintln!("sandbox regression: child is not running inside an AppContainer token");
    std::process::exit(1);
  }

  mitigations::verify_renderer_mitigations_current_process()
    .expect("expected renderer mitigations to be active in sandboxed child");

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
        Some(code)
          if code == ERROR_ACCESS_DENIED as i32
            || code == ERROR_PATH_NOT_FOUND as i32
            || code == ERROR_FILE_NOT_FOUND as i32
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

fn current_process_is_appcontainer() -> bool {
  unsafe {
    let mut token: HANDLE = std::ptr::null_mut();
    let ok = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token);
    if ok == 0 || token.is_null() {
      return false;
    }

    let mut is_appcontainer: u32 = 0;
    let mut returned: u32 = 0;
    let ok = GetTokenInformation(
      token,
      TokenIsAppContainer,
      (&mut is_appcontainer as *mut u32).cast::<c_void>(),
      std::mem::size_of::<u32>() as u32,
      &mut returned,
    );
    CloseHandle(token);

    ok != 0 && is_appcontainer != 0
  }
}
