//! macOS Seatbelt sandbox regression tests.
//!
//! These tests assert that applying a restrictive Seatbelt sandbox policy prevents filesystem and
//! network access. They use a child-process strategy so the main test runner is not permanently
//! sandboxed.

use std::ffi::{CStr, CString};
use std::io;
use std::net::TcpListener;
use std::process::Command;

const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";

#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(
    profile: *const libc::c_char,
    flags: libc::uint64_t,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

// `sandbox_init(3)` flags. We only need `SANDBOX_NAMED` to use one of macOS's built-in profiles.
const SANDBOX_NAMED: libc::uint64_t = 0x0001;

fn apply_pure_computation_sandbox() -> Result<(), String> {
  // `pure-computation` is a built-in Seatbelt profile that denies filesystem and network access.
  let profile = CString::new("pure-computation").map_err(|err| err.to_string())?;

  let mut error: *mut libc::c_char = std::ptr::null_mut();
  // SAFETY: FFI call. `profile` is a valid NUL-terminated string and `error` is a valid out param.
  let rc = unsafe { sandbox_init(profile.as_ptr(), SANDBOX_NAMED, &mut error) };

  if rc != 0 {
    let message = if error.is_null() {
      format!("sandbox_init returned {rc} with no error buffer")
    } else {
      // SAFETY: `sandbox_init` populates `error` with a NUL-terminated C string.
      let msg = unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned();
      // SAFETY: `sandbox_free_error` expects the pointer returned by `sandbox_init`.
      unsafe { sandbox_free_error(error) };
      msg
    };
    return Err(message);
  }

  if !error.is_null() {
    // Some versions of the API might still allocate an empty error buffer on success.
    // SAFETY: Same as above.
    unsafe { sandbox_free_error(error) };
  }

  Ok(())
}

fn assert_sandbox_denied(action: &str, err: &io::Error) {
  let kind = err.kind();
  let raw = err.raw_os_error();

  let is_permission = matches!(kind, io::ErrorKind::PermissionDenied)
    || matches!(raw, Some(code) if code == libc::EPERM || code == libc::EACCES);

  assert!(
    is_permission,
    "{action}: expected PermissionDenied/EPERM/EACCES, got kind={kind:?} raw_os_error={raw:?} err={err}"
  );
}

#[test]
fn seatbelt_sandbox_blocks_filesystem_and_network() {
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if !is_child {
    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos_seatbelt::seatbelt_sandbox_blocks_filesystem_and_network";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child sandbox test should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
    return;
  }

  apply_pure_computation_sandbox().expect("apply Seatbelt sandbox profile");

  match std::fs::read_to_string("/etc/passwd") {
    Ok(_) => panic!("sandboxed process should not be able to read /etc/passwd"),
    Err(err) => assert_sandbox_denied("read /etc/passwd", &err),
  }

  match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => panic!(
      "sandboxed process should not be able to bind TCP listener; got local_addr={:?}",
      listener.local_addr()
    ),
    Err(err) => assert_sandbox_denied("TcpListener::bind(127.0.0.1:0)", &err),
  }

  match tempfile::NamedTempFile::new() {
    Ok(_) => panic!("sandboxed process should not be able to create a tempfile"),
    Err(err) => assert_sandbox_denied("tempfile::NamedTempFile::new()", &err),
  }
}

