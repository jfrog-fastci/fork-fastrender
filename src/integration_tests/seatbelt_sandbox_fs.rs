//! macOS Seatbelt sandbox regression tests.
//!
//! These tests intentionally run sandboxing in a *child process* because Seatbelt sandboxing is
//! irreversible: once a profile is installed, it cannot be removed for the lifetime of the
//! process.
//!
//! The goal is to ensure that our "no filesystem access" guarantees cover filesystem *metadata*
//! and directory listing (`stat`, `readdir`, `realpath`), not just file reads.

use std::io;
use std::process::Command;

mod seatbelt {
  use std::ffi::{CStr, CString};
  use std::io;

  // `sandbox_init`/`sandbox_free_error` live in `libsandbox` on macOS.
  #[link(name = "sandbox")]
  extern "C" {
    fn sandbox_init(
      profile: *const libc::c_char,
      flags: u64,
      errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(errorbuf: *mut libc::c_char);
  }

  // From `<sandbox.h>`:
  //   #define SANDBOX_NAMED 0x0001
  const SANDBOX_NAMED: u64 = 0x0001;

  pub fn apply_named_profile(profile_name: &str) -> io::Result<()> {
    let profile = CString::new(profile_name)
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "profile name contained NUL"))?;
    let mut error_buf: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `sandbox_init` is a C API. We provide a NUL-terminated profile string and a valid
    // pointer to `error_buf`.
    let rc = unsafe { sandbox_init(profile.as_ptr(), SANDBOX_NAMED, &mut error_buf) };
    if rc == 0 {
      return Ok(());
    }

    let msg = unsafe {
      // SAFETY: When sandbox_init fails, it may return an owned error string; free it with
      // `sandbox_free_error` after copying.
      let message = if error_buf.is_null() {
        "sandbox_init failed with no error message".to_string()
      } else {
        CStr::from_ptr(error_buf).to_string_lossy().into_owned()
      };
      if !error_buf.is_null() {
        sandbox_free_error(error_buf);
      }
      message
    };
    Err(io::Error::new(io::ErrorKind::Other, msg))
  }

  pub fn apply_profile(profile: &str) -> io::Result<()> {
    let profile = CString::new(profile)
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "profile contained NUL"))?;
    let mut error_buf: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `sandbox_init` is a C API. We provide a NUL-terminated profile string and a valid
    // pointer to `error_buf`.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error_buf) };
    if rc == 0 {
      return Ok(());
    }

    let msg = unsafe {
      // SAFETY: When sandbox_init fails, it may return an owned error string; free it with
      // `sandbox_free_error` after copying.
      let message = if error_buf.is_null() {
        "sandbox_init failed with no error message".to_string()
      } else {
        CStr::from_ptr(error_buf).to_string_lossy().into_owned()
      };
      if !error_buf.is_null() {
        sandbox_free_error(error_buf);
      }
      message
    };
    Err(io::Error::new(io::ErrorKind::Other, msg))
  }
}

fn assert_permission_denied<T>(result: io::Result<T>, context: &str) {
  match result {
    Ok(_) => panic!("expected sandbox to deny {context}, but it succeeded"),
    Err(err) => {
      assert_eq!(
        err.kind(),
        io::ErrorKind::PermissionDenied,
        "expected sandbox to deny {context} with PermissionDenied, got {err:?}"
      );
    }
  }
}

#[test]
fn seatbelt_strict_sandbox_denies_filesystem_metadata_and_listing() {
  const CHILD_ENV: &str = "FASTR_SEATBELT_STRICT_CHILD";
  if std::env::var_os(CHILD_ENV).is_some() {
    seatbelt::apply_named_profile("pure-computation").expect("apply pure-computation sandbox");

    assert_permission_denied(
      std::fs::metadata("/etc/passwd"),
      "std::fs::metadata(\"/etc/passwd\")",
    );
    assert_permission_denied(std::fs::read_dir("/etc"), "std::fs::read_dir(\"/etc\")");
    assert_permission_denied(
      std::fs::canonicalize("/etc/passwd"),
      "std::fs::canonicalize(\"/etc/passwd\")",
    );
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Filter down to this test so we don't sandbox the rest of the suite.
    .arg("seatbelt_strict_sandbox_denies_filesystem_metadata_and_listing")
    .arg("--nocapture")
    .output()
    .expect("spawn child test process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

#[test]
fn seatbelt_relaxed_sandbox_denies_home_metadata_but_allows_system_fonts_listing() {
  const CHILD_ENV: &str = "FASTR_SEATBELT_RELAXED_CHILD";
  const HOME_FILE_ENV: &str = "FASTR_SEATBELT_HOME_FILE";
  if std::env::var_os(CHILD_ENV).is_some() {
    let home_file = std::env::var_os(HOME_FILE_ENV).expect("home file env missing in child");
    let home_file = std::path::PathBuf::from(home_file);

    // "Relaxed" profile used by the multiprocess sandbox when font discovery is enabled. It should
    // still deny access to user-controlled paths (including metadata), but allow listing system font
    // directories so font discovery can run.
    //
    // This mirrors the Task 42 intent: allow font discovery without granting general filesystem
    // access.
    const PROFILE: &str = r#"(version 1)
(allow default)
(deny file-read* (require-not (subpath "/System/Library/Fonts")))
(deny file-write*)
"#;
    seatbelt::apply_profile(PROFILE).expect("apply relaxed seatbelt profile");

    assert_permission_denied(
      std::fs::metadata(&home_file),
      "std::fs::metadata(<home-created file>)",
    );

    let font_dir = std::path::Path::new("/System/Library/Fonts");
    let mut entries = std::fs::read_dir(font_dir).expect("system font directory listing should be allowed in relaxed mode");
    if let Some(entry) = entries.next() {
      entry.expect("expected to read at least one entry from system font dir");
    }
    return;
  }

  // Create a file *inside the user's home directory* (Task 165) and ensure the sandbox blocks even
  // metadata access to it.
  let home = std::env::var_os("HOME").expect("HOME env var should be set for sandbox tests");
  let home_dir = std::path::PathBuf::from(home);
  let tmp = tempfile::Builder::new()
    .prefix("fastr-seatbelt-home-")
    .tempdir_in(&home_dir)
    .expect("create tempdir in home");
  let home_file_path = tmp.path().join("secret.txt");
  std::fs::write(&home_file_path, b"top secret").expect("write home test file");

  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .env(HOME_FILE_ENV, &home_file_path)
    // Keep the tempdir alive in the parent while the child runs so the path exists.
    .arg("seatbelt_relaxed_sandbox_denies_home_metadata_but_allows_system_fonts_listing")
    .arg("--nocapture")
    .output()
    .expect("spawn child test process");
  drop(tmp);

  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
