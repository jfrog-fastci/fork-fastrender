//! macOS Seatbelt sandbox (libsandbox) helpers.
//!
//! FastRender uses Seatbelt to sandbox untrusted renderer processes on macOS. The sandbox is
//! process-wide and irreversible; apply it as early as possible during renderer startup (and run
//! sandbox tests in a dedicated child process).
//!
//! # Extending the relaxed profile
//! The relaxed renderer profile intentionally starts small: it blocks network and user filesystem
//! reads while allowing read-only access to a limited set of system font/framework locations. When
//! you observe sandbox denials impacting rendering (commonly font discovery), inspect Seatbelt logs
//! and extend the allowlist with the smallest additional system subpath:
//!
//! ```text
//! log stream --predicate 'process == "<renderer-binary>" && eventMessage CONTAINS "deny"' --style syslog
//! ```
//!
//! The log message usually includes the denied operation and path.

use std::ffi::{CStr, CString};
use std::io;

// Seatbelt sandboxing is macOS-specific.
//
// We link to the system `libsandbox` to call `sandbox_init`, which installs a process-wide sandbox
// profile that cannot be reverted. Callers must apply it only once and must do so in a dedicated
// child process when running tests.
#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(
    profile: *const libc::c_char,
    flags: u64,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_init_with_parameters(
    profile: *const libc::c_char,
    flags: u64,
    parameters: *const *const libc::c_char,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

// `sandbox_init` flags are not exposed in `libc`.
//
// Apple documents `SANDBOX_NAMED` as the flag to treat the `profile` string as a profile name
// rather than raw profile source code. We keep the constant local so the crate can compile on
// non-macOS targets without pulling in additional bindgen tooling.
const SANDBOX_NAMED: u64 = 0x0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSandboxMode {
  /// macOS built-in `pure-computation` profile.
  ///
  /// This is very strict and typically breaks system font discovery/loading.
  PureComputation,
  /// A renderer-friendly profile that blocks network + user filesystem reads, while allowing
  /// read-only access to system font/framework locations.
  RendererSystemFonts,
}

const RENDERER_SYSTEM_FONTS_PROFILE: &str = r#"(version 1)
(deny default)

;; Allow basic runtime operations (threads, sysctl reads, mach services, etc.).
;; This is part of macOS itself: /System/Library/Sandbox/Profiles/system.sb
(import "system.sb")

;; Block all networking.
(deny network*)

;; Block writes everywhere.
(deny file-write*)

;; Explicitly deny reads from user-controlled / sensitive locations.
(deny file-read* (subpath (param "HOME")))
(deny file-read* (subpath "/Users"))
(deny file-read* (subpath "/Volumes"))
(deny file-read* (subpath "/private/etc"))
(deny file-read* (subpath "/etc"))
(deny file-read* (subpath "/private/var/folders"))
(deny file-read* (subpath "/private/var/tmp"))
(deny file-read* (subpath "/private/tmp"))

;; Allow read-only access to system resources required for font discovery/loading.
(allow file-read* (subpath "/System/Library/Fonts"))
(allow file-read* (subpath "/Library/Fonts"))
(allow file-read* (subpath "/usr/share/fonts"))
(allow file-read* (subpath "/System/Library/Frameworks"))
(allow file-read* (subpath "/System/Library/PrivateFrameworks"))
(allow file-read* (subpath "/usr/lib"))
"#;

fn sandbox_init_profile(profile: &CStr, flags: u64) -> io::Result<()> {
  let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();

  // SAFETY: `sandbox_init` installs an irreversible process-wide sandbox. The FFI contract requires
  // a NUL-terminated profile string and a valid out-pointer for the error buffer.
  let rc = unsafe { sandbox_init(profile.as_ptr(), flags, &mut errorbuf) };
  if rc == 0 {
    return Ok(());
  }

  Err(io::Error::new(io::ErrorKind::Other, sandbox_message(errorbuf)))
}

fn sandbox_init_profile_with_parameters(
  profile: &CStr,
  flags: u64,
  parameters: &[*const libc::c_char],
) -> io::Result<()> {
  let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();

  // SAFETY: `sandbox_init_with_parameters` installs an irreversible process-wide sandbox. The FFI
  // contract requires a NUL-terminated profile string, a NULL-terminated `parameters` list, and a
  // valid out-pointer for the error buffer.
  let rc = unsafe {
    sandbox_init_with_parameters(profile.as_ptr(), flags, parameters.as_ptr(), &mut errorbuf)
  };
  if rc == 0 {
    return Ok(());
  }

  Err(io::Error::new(io::ErrorKind::Other, sandbox_message(errorbuf)))
}

fn sandbox_message(errorbuf: *mut libc::c_char) -> String {
  if errorbuf.is_null() {
    return "sandbox_init failed with unknown error".to_string();
  }

  // SAFETY: `errorbuf` is allocated by `libsandbox` and is NUL-terminated.
  let message = unsafe { CStr::from_ptr(errorbuf) }
    .to_string_lossy()
    .into_owned();
  // SAFETY: `sandbox_free_error` frees the buffer allocated by `sandbox_init`.
  unsafe { sandbox_free_error(errorbuf) };
  message
}

fn apply_named_profile(profile_name: &str) -> io::Result<()> {
  let profile_name =
    CString::new(profile_name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL"))?;
  sandbox_init_profile(&profile_name, SANDBOX_NAMED)
}

fn apply_profile_source(profile_source: &str) -> io::Result<()> {
  let profile_source = CString::new(profile_source)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sandbox profile contains NUL"))?;
  // Flags == 0 means the profile argument is raw profile source (not a named profile).
  sandbox_init_profile(&profile_source, 0)
}

fn apply_profile_source_with_home_param(profile_source: &str) -> io::Result<()> {
  let profile_source = CString::new(profile_source)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sandbox profile contains NUL"))?;

  let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
  let home = CString::new(home)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "HOME contains NUL"))?;
  let key = CString::new("HOME").expect("static cstr should not contain NUL");

  // `sandbox_init_with_parameters` expects a NULL-terminated list: [key, value, key, value, NULL].
  let params: [*const libc::c_char; 3] = [key.as_ptr(), home.as_ptr(), std::ptr::null()];
  sandbox_init_profile_with_parameters(&profile_source, 0, &params)
}

/// Apply the macOS Seatbelt "pure-computation" sandbox profile to the current process.
///
/// This profile is expected to prevent direct filesystem and network access.
///
/// ⚠️ This is irreversible for the lifetime of the process; tests must apply it in a dedicated
/// child process (see `FASTR_TEST_MACOS_SANDBOX_CHILD` in the unit tests below).
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  let named_err = match apply_named_profile("pure-computation") {
    Ok(()) => return Ok(()),
    Err(err) => err,
  };

  // Some environments may not support named profiles via `sandbox_init` (for example, if the flag
  // values differ across SDKs). Fall back to loading the profile source from known system paths.
  let candidate_paths = [
    "/usr/share/sandbox/pure-computation.sb",
    "/System/Library/Sandbox/Profiles/pure-computation.sb",
  ];
  for path in candidate_paths {
    if let Ok(source) = std::fs::read_to_string(path) {
      if let Ok(()) = apply_profile_source(&source) {
        return Ok(());
      }
    }
  }

  Err(named_err)
}

/// Apply a renderer-focused sandbox to the current process.
///
/// This call is irreversible: once applied, the process cannot regain privileges.
pub fn apply_renderer_sandbox(mode: MacosSandboxMode) -> io::Result<()> {
  match mode {
    MacosSandboxMode::PureComputation => apply_pure_computation_sandbox(),
    MacosSandboxMode::RendererSystemFonts => {
      apply_profile_source_with_home_param(RENDERER_SYSTEM_FONTS_PROFILE)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io;
  use std::net::{TcpListener, TcpStream};
  use std::process::Command;
  use std::time::{Instant, SystemTime};

  const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";
  const PORT_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_PORT";

  fn is_permission_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
      return true;
    }
    matches!(
      err.raw_os_error(),
      Some(libc::EPERM) | Some(libc::EACCES)
    )
  }

  #[test]
  fn seatbelt_pure_computation_blocks_filesystem_and_network() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let port: u16 = std::env::var(PORT_ENV)
        .expect("child process missing sandbox port env var")
        .parse()
        .expect("parse sandbox port env var");

      apply_pure_computation_sandbox().expect("apply pure-computation sandbox");

      // 1) File read should fail.
      let read_err = std::fs::read_to_string("/private/etc/passwd")
        .or_else(|err| {
          if err.kind() == io::ErrorKind::NotFound {
            std::fs::read_to_string("/etc/passwd")
          } else {
            Err(err)
          }
        })
        .expect_err("expected filesystem read to be denied by sandbox");
      assert!(
        is_permission_error(&read_err),
        "expected file read to be denied by sandbox, got {read_err:?}"
      );

      // 2) File write should fail.
      let temp_path = std::env::temp_dir().join(format!(
        "fastr_sandbox_test_{}_write.txt",
        std::process::id()
      ));
      let write_err = std::fs::write(&temp_path, b"fastrender sandbox test")
        .expect_err("expected filesystem write to be denied by sandbox");
      assert!(
        is_permission_error(&write_err),
        "expected file write to be denied by sandbox, got {write_err:?}"
      );

      // 3) Network access should fail, even to localhost.
      let connect_err = TcpStream::connect(("127.0.0.1", port))
        .expect_err("expected network connect to be denied by sandbox");
      assert!(
        is_permission_error(&connect_err),
        "expected network connect to be denied by sandbox, got {connect_err:?}"
      );
      return;
    }

    let _listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test TCP listener");
    let port = _listener
      .local_addr()
      .expect("listener local addr")
      .port()
      .to_string();

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::macos::tests::seatbelt_pure_computation_blocks_filesystem_and_network";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .env(PORT_ENV, port)
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
  }

  #[test]
  fn seatbelt_pure_computation_allows_basic_rust_runtime_features() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      eprintln!("applying Seatbelt pure-computation sandbox");
      apply_pure_computation_sandbox().expect("apply pure-computation sandbox");

      eprintln!("spawning a thread under sandbox");
      std::thread::spawn(|| {
        // Keep it simple: just return a value so the optimizer can't elide the thread.
        42_u32
      })
      .join()
      .expect("thread should spawn + join successfully under sandbox");

      eprintln!("checking std::thread::available_parallelism()");
      let parallelism = std::thread::available_parallelism()
        .expect("available_parallelism should work under the sandbox");
      assert!(
        parallelism.get() >= 1,
        "available_parallelism should return >= 1 (got {})",
        parallelism.get()
      );

      eprintln!("checking std::time clocks");
      let system_now = SystemTime::now();
      let unix = system_now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time should be after UNIX_EPOCH");
      eprintln!("SystemTime::now() OK (unix_ms={})", unix.as_millis());
      let _instant_now = Instant::now();

      eprintln!("checking getrandom under sandbox");
      let mut bytes = [0u8; 32];
      getrandom::getrandom(&mut bytes).expect("getrandom should succeed under sandbox");
      assert!(
        bytes.iter().any(|&b| b != 0),
        "getrandom returned an all-zero buffer, which is unexpectedly unlikely"
      );
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos::tests::seatbelt_pure_computation_allows_basic_rust_runtime_features";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
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
  }
}
