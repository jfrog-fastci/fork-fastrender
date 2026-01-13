#![cfg(target_os = "macos")]

//! macOS Seatbelt sandbox probe tool.
//!
//! This binary is intended for iterating on Seatbelt profile changes without needing to
//! run the full multi-process browser stack.

use clap::{Parser, ValueEnum};
use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "Probe FastRender's macOS renderer sandbox (Seatbelt) behavior")]
struct Args {
  /// Which sandbox profile to apply.
  #[arg(long, value_enum)]
  mode: SandboxMode,

  /// Port to attempt connecting to on 127.0.0.1 (should be denied by the sandbox).
  ///
  /// Tip: start a server (e.g. `python3 -m http.server 8000`) so a non-sandboxed run would
  /// succeed, making sandbox-denial obvious.
  #[arg(long, default_value_t = 8000)]
  port: u16,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SandboxMode {
  Strict,
  Relaxed,
}

fn main() {
  std::process::exit(run());
}

fn run() -> i32 {
  let args = Args::parse();
  println!("mode: {:?}", args.mode);

  let temp_dir = std::env::temp_dir();
  let profile = build_profile(args.mode, &temp_dir);
  if let Err(err) = apply_sandbox(&profile) {
    eprintln!("sandbox: failed to apply: {err}");
    // Distinct from probe failures so scripts can tell "sandbox didn't load".
    return 2;
  }
  println!("sandbox: applied");

  let mut unexpected_success = false;

  let read_result = probe_read_passwd();
  unexpected_success |= report_action(
    "read /etc/passwd",
    read_result,
    matches!(args.mode, SandboxMode::Strict | SandboxMode::Relaxed),
  );

  let write_result = probe_write_temp_file(&temp_dir);
  unexpected_success |= report_action(
    &format!("write temp file under {}", temp_dir.display()),
    write_result,
    matches!(args.mode, SandboxMode::Strict),
  );

  let connect_result = probe_connect_localhost(args.port);
  unexpected_success |= report_action(
    &format!("connect to 127.0.0.1:{}", args.port),
    connect_result,
    matches!(args.mode, SandboxMode::Strict | SandboxMode::Relaxed),
  );

  let exit_code = if unexpected_success { 1 } else { 0 };
  println!("exit_code: {exit_code}");
  exit_code
}

fn build_profile(mode: SandboxMode, temp_dir: &PathBuf) -> String {
  // NOTE: Seatbelt string escaping is C-like; we keep it minimal and only escape quotes and
  // backslashes in the dynamically injected temp-dir path.
  let temp_dir = escape_seatbelt_string(&temp_dir.to_string_lossy());
  match mode {
    SandboxMode::Strict => format!(
      r#"(version 1)
(allow default)
(deny network*)
(deny file-read* (literal "/etc/passwd"))
(deny file-write* (subpath "{temp_dir}"))
"#
    ),
    SandboxMode::Relaxed => r#"(version 1)
(allow default)
(deny network*)
(deny file-read* (literal "/etc/passwd"))
"#
    .to_string(),
  }
}

fn escape_seatbelt_string(raw: &str) -> String {
  raw.replace('\\', r"\\").replace('"', r#"\""#)
}

fn probe_read_passwd() -> ActionResult {
  match fs::read("/etc/passwd") {
    Ok(bytes) => ActionResult::success(format!("read {} bytes", bytes.len())),
    Err(err) => ActionResult::failure(err),
  }
}

fn probe_write_temp_file(temp_dir: &PathBuf) -> ActionResult {
  let filename = format!("fastrender_sandbox_probe_{}.tmp", std::process::id());
  let path = temp_dir.join(filename);
  let payload = b"fastrender sandbox probe\n";

  match fs::write(&path, payload) {
    Ok(()) => {
      let _ = fs::remove_file(&path);
      ActionResult::success(format!(
        "wrote {} bytes to {}",
        payload.len(),
        path.display()
      ))
    }
    Err(err) => ActionResult::failure(err),
  }
}

fn probe_connect_localhost(port: u16) -> ActionResult {
  let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
  match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
    Ok(_stream) => ActionResult::success("connected".to_string()),
    Err(err) => ActionResult::failure(err),
  }
}

#[derive(Debug)]
struct ActionResult {
  ok: bool,
  detail: String,
  kind: Option<io::ErrorKind>,
}

impl ActionResult {
  fn success(detail: String) -> Self {
    Self {
      ok: true,
      detail,
      kind: None,
    }
  }

  fn failure(err: io::Error) -> Self {
    Self {
      ok: false,
      detail: err.to_string(),
      kind: Some(err.kind()),
    }
  }
}

fn report_action(name: &str, result: ActionResult, expected_denied: bool) -> bool {
  let status = if result.ok {
    "OK"
  } else if matches!(result.kind, Some(io::ErrorKind::PermissionDenied)) {
    "DENIED"
  } else {
    "FAIL"
  };

  let expected = if expected_denied {
    "expected=DENIED"
  } else {
    "expected=ALLOWED/UNKNOWN"
  };

  if result.ok {
    println!("{name}: {status} ({expected}; {})", result.detail);
  } else {
    println!("{name}: {status} ({expected}; error={})", result.detail);
    if expected_denied && matches!(result.kind, Some(io::ErrorKind::ConnectionRefused)) {
      println!(
        "  note: connection was refused (no listener). Start a local server on that port to \
distinguish sandbox denial from ordinary connection failures."
      );
    }
  }

  result.ok && expected_denied
}

// ============================================================================
// Seatbelt sandbox API bindings
// ============================================================================

#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(
    profile: *const libc::c_char,
    flags: u64,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

fn apply_sandbox(profile: &str) -> Result<(), String> {
  let profile =
    CString::new(profile).map_err(|_| "sandbox profile contains NUL byte".to_string())?;
  let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();
  // SAFETY: Calls into macOS' libsandbox. `errorbuf` is either left null or set to a malloc'd
  // C-string that must be released with `sandbox_free_error`.
  let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut errorbuf) };
  if rc == 0 {
    return Ok(());
  }
  if errorbuf.is_null() {
    return Err("sandbox_init failed (no error buffer)".to_string());
  }
  // SAFETY: `errorbuf` is a valid NUL-terminated C string allocated by libsandbox.
  let message = unsafe { CStr::from_ptr(errorbuf) }
    .to_string_lossy()
    .into_owned();
  // SAFETY: `sandbox_free_error` is the documented destructor for the returned buffer.
  unsafe { sandbox_free_error(errorbuf) };
  Err(message)
}
