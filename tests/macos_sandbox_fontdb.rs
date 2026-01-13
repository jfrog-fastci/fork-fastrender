#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString};
use std::process::Command;
use std::ptr;

// macOS Seatbelt sandbox support (libsandbox).
//
// We keep this FFI local to the test so non-macOS builds don't need to worry about
// link flags, and so the test can exercise the real OS sandbox behavior (which is
// irreversible once enabled).
#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(
    profile: *const libc::c_char,
    flags: u64,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

// From macOS `<sandbox.h>`: interpret `profile` as a named system profile such as
// "no-network" rather than inline sandbox profile source.
const SANDBOX_NAMED: u64 = 1;

fn apply_relaxed_macos_sandbox() -> Result<(), String> {
  // "no-network" is a good approximation of a "relaxed" renderer sandbox: it blocks all network
  // sockets while leaving local filesystem reads + system service access available.
  //
  // The goal of this test is specifically to catch cases where a relaxed sandbox still breaks
  // `fontdb::Database::load_system_fonts()` due to CoreText/fontd mach service lookups, even when
  // raw file reads would otherwise be permitted.
  let profile = CString::new("no-network").expect("profile CString");
  let mut errorbuf: *mut libc::c_char = ptr::null_mut();
  // SAFETY: FFI call to system sandbox API. `errorbuf` is either left null (success) or points to
  // an allocated error string that must be freed with `sandbox_free_error`.
  let rc = unsafe { sandbox_init(profile.as_ptr(), SANDBOX_NAMED, &mut errorbuf) };
  if rc == 0 {
    return Ok(());
  }

  let msg = if errorbuf.is_null() {
    "sandbox_init failed (no error message)".to_string()
  } else {
    // SAFETY: `errorbuf` is a NUL-terminated C string from `sandbox_init`.
    let msg = unsafe { CStr::from_ptr(errorbuf) }
      .to_string_lossy()
      .into_owned();
    // SAFETY: Apple API requires freeing the error buffer when non-null.
    unsafe { sandbox_free_error(errorbuf) };
    msg
  };
  Err(msg)
}

#[test]
fn relaxed_sandbox_allows_fontdb_system_font_discovery() {
  const CHILD_ENV: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_FONTDB_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    apply_relaxed_macos_sandbox().expect("apply relaxed macOS sandbox (no-network profile)");

    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let face_count = db.faces().len();
    assert!(
      face_count > 0,
      "expected system font discovery to find at least one face under relaxed sandbox"
    );

    // Bonus sanity check: `fontdb` generic families (e.g. `sans-serif`) should still resolve.
    let query = fontdb::Query {
      families: &[fontdb::Family::SansSerif],
      weight: fontdb::Weight(400),
      stretch: fontdb::Stretch::Normal,
      style: fontdb::Style::Normal,
    };
    assert!(
      db.query(&query).is_some(),
      "expected fontdb generic sans-serif query to resolve under relaxed sandbox"
    );
    return;
  }

  // `sandbox_init` is irreversible. Run the actual sandboxed probe in a subprocess so it doesn't
  // affect the rest of the test suite.
  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .arg("--exact")
    .arg("relaxed_sandbox_allows_fontdb_system_font_discovery")
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

