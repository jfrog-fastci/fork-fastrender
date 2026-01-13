//! Guard that ensures the macOS relaxed sandbox profile still blocks access to `$HOME`.
//!
//! This prevents accidentally widening filesystem allow rules (e.g. allowing `~/` reads) in the
//! relaxed profile.

#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString};
use std::io;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENV_HOME_FILE: &str = "FASTR_TEST_SANDBOX_HOME_FILE";

#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
  fn sandbox_free_error(errorbuf: *mut c_char);
}

fn escape_sandbox_string(value: &str) -> String {
  value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn apply_relaxed_sandbox_profile() {
  // The parent created the file under `$HOME`, so deny reads under that directory.
  let home = std::env::var("HOME").expect("HOME env var must be set for sandbox test");
  let home = home.trim_end_matches('/');
  assert!(
    !home.is_empty(),
    "HOME env var must not be empty for sandbox test"
  );

  // A deliberately minimal "relaxed" profile: allow everything by default, but deny reads of the
  // user's home directory. This keeps the test harness functional while providing a guardrail
  // against accidentally allowing `~/` access in the relaxed profile.
  let profile = format!(
    "(version 1)\n(allow default)\n(deny file-read* (subpath \"{}\"))\n",
    escape_sandbox_string(home)
  );
  let profile =
    CString::new(profile).expect("sandbox profile should not contain interior NUL bytes");

  let mut errorbuf: *mut c_char = std::ptr::null_mut();
  // SAFETY: Calls into the macOS libsandbox API with stable pointers.
  let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut errorbuf) };
  if rc != 0 {
    let message = if errorbuf.is_null() {
      format!("sandbox_init failed with rc={rc}")
    } else {
      // SAFETY: errorbuf points to a NUL-terminated string on failure.
      let msg = unsafe { CStr::from_ptr(errorbuf) }
        .to_string_lossy()
        .into_owned();
      // SAFETY: errorbuf was allocated by libsandbox.
      unsafe { sandbox_free_error(errorbuf) };
      msg
    };
    panic!("sandbox_init failed: {message}");
  }
}

fn assert_permission_denied(err: &io::Error, path: &Path) {
  let raw = err.raw_os_error();
  assert_ne!(
    err.kind(),
    io::ErrorKind::NotFound,
    "expected sandbox to block reading {}, but got NotFound instead (raw={raw:?}, err={err:?})",
    path.display()
  );
  assert!(
    err.kind() == io::ErrorKind::PermissionDenied
      || matches!(raw, Some(libc::EPERM) | Some(libc::EACCES)),
    "expected PermissionDenied/EPERM/EACCES when reading {} under sandbox; got kind={:?} raw={raw:?} err={err:?}",
    path.display(),
    err.kind()
  );
}

#[test]
fn relaxed_sandbox_profile_denies_home_file_read() {
  if let Some(path) = std::env::var_os(ENV_HOME_FILE) {
    // Child process: apply sandbox then attempt to read the home-owned file.
    let path = PathBuf::from(path);
    apply_relaxed_sandbox_profile();
    let err = std::fs::read_to_string(&path).unwrap_err();
    assert_permission_denied(&err, &path);
    return;
  }

  // Parent process: create a temp file under $HOME, write a sentinel, then spawn a child to attempt
  // the read after sandboxing.
  let home = std::env::var_os("HOME").expect("HOME env var must be set for sandbox test");
  let home = PathBuf::from(home);
  assert!(
    home.is_dir(),
    "HOME env var did not resolve to a directory: {}",
    home.display()
  );

  let mut file = tempfile::Builder::new()
    .prefix("fastr_test_sandbox_home_")
    .tempfile_in(&home)
    .expect("create temp file in $HOME");

  const SENTINEL: &str = "fastr_sandbox_home_sentinel";
  use std::io::Write;
  file.write_all(SENTINEL.as_bytes())
    .expect("write sentinel to home file");
  file.flush().expect("flush sentinel to disk");

  let path = file.path().to_path_buf();
  let roundtrip = std::fs::read_to_string(&path).expect("read back sentinel in parent");
  assert_eq!(roundtrip, SENTINEL, "home file should contain sentinel");

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = concat!(
    module_path!(),
    "::relaxed_sandbox_profile_denies_home_file_read"
  );

  let output = Command::new(exe)
    .env(ENV_HOME_FILE, &path)
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandboxed child process");

  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  // Parent cleanup: ensure the home file is removed after the child exits.
  file.close().expect("remove temp file in $HOME");
  assert!(
    !path.exists(),
    "expected temp file to be removed after child exits: {}",
    path.display()
  );
}
