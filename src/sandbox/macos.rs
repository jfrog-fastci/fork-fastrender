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
// rather than raw profile source code.
const SANDBOX_NAMED: u64 = 0x0001;
// Treat the `profile` argument as raw SBPL profile source.
const SANDBOX_PROFILE: u64 = 0x0002;

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

// Minimal embedded fallback for the strict `pure-computation` sandbox.
//
// Requirements:
// - `(version 1)`
// - `(deny default)`
// - deny file-read*, file-write*, and network*
// - allow enough for basic runtime (threads, memory, stdio)
const STRICT_FALLBACK_PROFILE: &str = r#"(version 1)
(deny default)
(deny file-read*)
(deny file-write*)
(deny network*)
(allow process*)
(allow sysctl-read)
(allow mach-lookup)
(allow ipc-posix-shm)
(allow ipc-posix-sem)
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrictSandboxBackend {
  NamedProfile,
  EmbeddedFallback,
}

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
  sandbox_init_profile(&profile_source, SANDBOX_PROFILE)
}

fn apply_profile_source_with_home_param(profile_source: &str) -> io::Result<()> {
  let profile_source = CString::new(profile_source)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sandbox profile contains NUL"))?;

  let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
  let home =
    CString::new(home).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "HOME contains NUL"))?;
  let key = CString::new("HOME").expect("static cstr should not contain NUL");

  // `sandbox_init_with_parameters` expects a NULL-terminated list: [key, value, key, value, NULL].
  let params: [*const libc::c_char; 3] = [key.as_ptr(), home.as_ptr(), std::ptr::null()];
  sandbox_init_profile_with_parameters(&profile_source, SANDBOX_PROFILE, &params)
}

fn error_indicates_unknown_profile(message: &str) -> bool {
  let lower = message.to_ascii_lowercase();
  lower.contains("unknown profile")
    || lower.contains("no such profile")
    || lower.contains("profile not found")
    || lower.contains("invalid profile")
}

fn apply_strict_sandbox_named_first(profile_name: &str) -> io::Result<StrictSandboxBackend> {
  match apply_named_profile(profile_name) {
    Ok(()) => Ok(StrictSandboxBackend::NamedProfile),
    Err(err) => {
      if !error_indicates_unknown_profile(&err.to_string()) {
        return Err(err);
      }

      match apply_profile_source(STRICT_FALLBACK_PROFILE) {
        Ok(()) => Ok(StrictSandboxBackend::EmbeddedFallback),
        Err(fallback_err) => Err(io::Error::new(
          io::ErrorKind::Other,
          format!(
            "failed to apply Seatbelt sandbox named profile '{profile_name}' (error: {err}); fallback profile also failed (error: {fallback_err})",
          ),
        )),
      }
    }
  }
}

/// Apply a strict Seatbelt sandbox profile to the current process.
///
/// This first attempts macOS's built-in `pure-computation` profile via
/// `sandbox_init("pure-computation", SANDBOX_NAMED, ...)`. If that profile is unavailable or
/// rejected as invalid, it retries using an embedded SBPL profile string via `SANDBOX_PROFILE`.
///
/// ⚠️ This is irreversible for the lifetime of the process; tests must apply it in a dedicated
/// child process.
pub fn apply_strict_sandbox() -> io::Result<()> {
  apply_strict_sandbox_named_first("pure-computation").map(|_| ())
}

/// Apply the macOS Seatbelt "pure-computation" sandbox profile to the current process.
///
/// This is an alias for [`apply_strict_sandbox`].
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  apply_strict_sandbox()
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
  use std::io::{self, Write};
  use std::net::{TcpListener, TcpStream, UdpSocket};
  use std::process::Command;
  use std::time::{Instant, SystemTime};

  const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";
  const PORT_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_PORT";

  fn is_permission_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
      return true;
    }
    matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES))
  }

  fn assert_spawn_denied(mut command: Command) {
    match command.status() {
      Ok(status) => {
        panic!(
          "expected Seatbelt sandbox to deny spawning {:?}, but it exited with status {status}",
          command
        );
      }
      Err(err) => {
        assert!(
          is_permission_error(&err),
          "expected sandbox to deny spawning {:?}, got {err:?}",
          command
        );
      }
    }
  }

  #[test]
  fn pure_computation_sandbox_allows_inherited_stdout_pipe() {
    const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_STDOUT_CHILD";
    const SENTINEL: &[u8] = b"fastrender-seatbelt-stdout-ok";

    if std::env::var_os(CHILD_ENV).is_some() {
      // Call through the sandbox module's public wrapper to ensure it remains usable.
      super::super::apply_pure_computation_sandbox().expect("apply Seatbelt pure-computation sandbox");
      std::io::stdout()
        .write_all(SENTINEL)
        .and_then(|_| std::io::stdout().flush())
        .expect("write sentinel to stdout after sandbox");
      std::process::exit(0);
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos::tests::pure_computation_sandbox_allows_inherited_stdout_pipe";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn sandbox child process");

    assert!(
      output.status.success(),
      "sandbox child should exit 0 (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );

    assert!(
      output
        .stdout
        .windows(SENTINEL.len())
        .any(|window| window == SENTINEL),
      "expected sandbox child to write sentinel to stdout; got stdout={}, stderr={} ",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
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
      let bind_err = TcpListener::bind("127.0.0.1:0")
        .expect_err("expected network bind to be denied by sandbox");
      assert!(
        is_permission_error(&bind_err),
        "expected network bind to be denied by sandbox, got {bind_err:?}"
      );

      let udp_bind_err = UdpSocket::bind("127.0.0.1:0")
        .expect_err("expected UDP bind to be denied by sandbox");
      assert!(
        is_permission_error(&udp_bind_err),
        "expected UDP bind to be denied by sandbox, got {udp_bind_err:?}"
      );

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
  fn seatbelt_pure_computation_blocks_process_spawn() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      apply_pure_computation_sandbox().expect("apply pure-computation sandbox");

      assert_spawn_denied(Command::new("/usr/bin/true"));

      // Defense in depth: ensure a common shell entrypoint cannot be executed either.
      let mut sh = Command::new("/bin/sh");
      sh.arg("-c").arg(":");
      assert_spawn_denied(sh);
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos::tests::seatbelt_pure_computation_blocks_process_spawn";
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

  #[test]
  fn seatbelt_pure_computation_allows_inherited_stdout_pipe() {
    const SENTINEL: &[u8] = b"fastrender-seatbelt-stdout-ok";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      apply_pure_computation_sandbox().expect("apply pure-computation sandbox");
      std::io::stdout()
        .write_all(SENTINEL)
        .and_then(|_| std::io::stdout().flush())
        .expect("write sentinel to stdout after sandbox");
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::macos::tests::seatbelt_pure_computation_allows_inherited_stdout_pipe";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn sandbox child process");

    assert!(
      output.status.success(),
      "sandbox child should exit 0 (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );

    assert!(
      output
        .stdout
        .windows(SENTINEL.len())
        .any(|window| window == SENTINEL),
      "expected sandbox child to write sentinel to stdout; got stdout={}, stderr={}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  #[test]
  fn seatbelt_strict_sandbox_falls_back_when_named_profile_missing() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let port: u16 = std::env::var(PORT_ENV)
        .expect("child process missing sandbox port env var")
        .parse()
        .expect("parse sandbox port env var");

      let backend =
        apply_strict_sandbox_named_first("fastrender-nonexistent-seatbelt-profile").expect(
          "apply strict sandbox with embedded fallback when the named profile is missing",
        );
      assert_eq!(
        backend,
        StrictSandboxBackend::EmbeddedFallback,
        "expected strict sandbox helper to use the embedded fallback profile"
      );

      let read_err = std::fs::read_to_string("/etc/passwd")
        .expect_err("expected filesystem read to be denied by sandbox");
      assert!(
        is_permission_error(&read_err),
        "expected file read to be denied by sandbox, got {read_err:?}"
      );

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
      "sandbox::macos::tests::seatbelt_strict_sandbox_falls_back_when_named_profile_missing";
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
