use std::ffi::{CStr, CString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;

#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(
    profile: *const libc::c_char,
    flags: u64,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

fn apply_relaxed_macos_sandbox_profile() {
  // "Relaxed" means we allow broad read-only access (including system font directories) while still
  // denying filesystem writes. This is defense-in-depth: even if the renderer needs to read fonts
  // from system locations, it must not be able to create/modify files.
  //
  // NOTE: The allowlist here is intentionally permissive (read-only). The test focuses on write
  // denial, not read denial.
  const PROFILE: &str = r#"
(version 1)
(deny default)

; Keep the runtime/test harness functional.
(allow process*)
(allow mach-lookup)
(allow sysctl-read)

; Allow read-only filesystem access (e.g. system fonts) in relaxed mode.
(allow file-read*)

; Allow writing to already-open stdio fds so the test harness can report results.
(allow file-write* (subpath "/dev/fd"))
(allow file-write* (literal "/dev/null"))
(allow file-write* (literal "/dev/stdout"))
(allow file-write* (literal "/dev/stderr"))

; Intentionally do NOT allow file-write* anywhere else.
"#;

  let profile = CString::new(PROFILE).expect("sandbox profile contains no null bytes");
  let mut err: *mut libc::c_char = ptr::null_mut();
  // SAFETY: FFI call; `err` is a valid out-pointer, and `profile` is NUL-terminated.
  let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err) };
  if rc == 0 {
    return;
  }

  let msg = if err.is_null() {
    "<null error>".to_string()
  } else {
    // SAFETY: `err` is a C string returned by `sandbox_init`.
    let msg = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
    // SAFETY: `err` was allocated by `sandbox_init`.
    unsafe { sandbox_free_error(err) };
    msg
  };
  panic!("sandbox_init failed with rc={rc}: {msg}");
}

fn assert_write_is_denied(path: &Path) {
  let result = std::fs::write(path, b"fastrender sandbox write test");
  if result.is_ok() {
    // Best-effort cleanup in case the sandbox unexpectedly allows writes.
    let _ = std::fs::remove_file(path);
    panic!("unexpectedly succeeded writing to {}", path.display());
  }
  let err = result.expect_err("write should fail");
  let raw = err.raw_os_error();
  assert!(
    err.kind() == io::ErrorKind::PermissionDenied
      || raw == Some(libc::EPERM)
      || raw == Some(libc::EACCES),
    "expected filesystem write to be denied for {} (kind={:?} raw={raw:?} err={err})",
    path.display(),
    err.kind()
  );
}

#[test]
fn macos_relaxed_sandbox_denies_filesystem_writes() {
  const CHILD_ENV: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_WRITE_DENIED_CHILD";
  const ENV_TEMP_TARGET: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_WRITE_DENIED_TEMP_TARGET";
  const ENV_HOME_TARGET: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_WRITE_DENIED_HOME_TARGET";

  if std::env::var_os(CHILD_ENV).is_some() {
    let temp_target = PathBuf::from(
      std::env::var_os(ENV_TEMP_TARGET).expect("missing temp target env var in child"),
    );
    let home_target = PathBuf::from(
      std::env::var_os(ENV_HOME_TARGET).expect("missing home target env var in child"),
    );

    apply_relaxed_macos_sandbox_profile();
    assert_write_is_denied(&temp_target);
    assert_write_is_denied(&home_target);
    return;
  }

  let temp_target = std::env::temp_dir().join(format!(
    "fastrender_sandbox_write_test_{}_temp.txt",
    std::process::id()
  ));

  let home_dir =
    PathBuf::from(std::env::var_os("HOME").expect("HOME should be set for sandbox write test"));
  let caches_dir = home_dir.join("Library").join("Caches");
  let home_target = if caches_dir.is_dir() {
    caches_dir.join(format!(
      "fastrender_sandbox_write_test_{}_home.txt",
      std::process::id()
    ))
  } else {
    home_dir.join(format!(
      "fastrender_sandbox_write_test_{}_home.txt",
      std::process::id()
    ))
  };

  // Best-effort cleanup in case the host environment already has a stale file from a previous run.
  let _ = std::fs::remove_file(&temp_target);
  let _ = std::fs::remove_file(&home_target);

  let exe = std::env::current_exe().expect("resolve current test exe");
  let test_name =
    "misc::macos_relaxed_sandbox_write_denied::macos_relaxed_sandbox_denies_filesystem_writes";
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .env(ENV_TEMP_TARGET, &temp_target)
    .env(ENV_HOME_TARGET, &home_target)
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn child sandbox test process");

  // Best-effort cleanup in case sandboxing regressed and allowed writes.
  let _ = std::fs::remove_file(&temp_target);
  let _ = std::fs::remove_file(&home_target);

  assert!(
    output.status.success(),
    "child sandbox test should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
