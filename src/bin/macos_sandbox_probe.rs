//! macOS Seatbelt sandbox probe tool.
//!
//! This binary is intended for iterating on Seatbelt profile changes without needing to
//! run the full multi-process browser stack.

#[cfg(not(target_os = "macos"))]
fn main() {
  eprintln!("macos_sandbox_probe is only supported on macOS.");
  std::process::exit(2);
}

#[cfg(target_os = "macos")]
mod enabled {
  use clap::{Parser, ValueEnum};
  use std::ffi::{CStr, CString};
  use std::fs;
  use std::io;
  use std::io::{Read, Write};
  use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
  use std::os::unix::io::FromRawFd;
  use std::os::unix::net::{UnixListener, UnixStream};
  use std::path::PathBuf;
  use std::time::Duration;

  #[derive(Parser)]
  #[command(about = "Probe FastRender's macOS renderer sandbox (Seatbelt) behavior")]
  struct Args {
    /// Which sandbox profile to apply.
    #[arg(long, value_enum)]
    mode: SandboxMode,

    /// Port to attempt connecting to on 127.0.0.1.
    ///
    /// - `0` (default): bind an ephemeral port before applying the sandbox so a non-sandboxed
    ///   process would succeed, making sandbox denial obvious.
    /// - Non-zero: connect to that port directly.
    #[arg(long, default_value_t = 0)]
    port: u16,
  }

  #[derive(Clone, Copy, Debug, ValueEnum)]
  enum SandboxMode {
    Strict,
    Relaxed,
  }

  pub(crate) fn run() -> i32 {
    let args = Args::parse();
    println!("mode: {:?}", args.mode);

    let temp_dir = std::env::temp_dir();
    let (listener, connect_port) = prepare_listener(args.port);

    let profile = build_profile(args.mode, &temp_dir);
    if let Err(err) = apply_sandbox(&profile) {
      eprintln!("sandbox: failed to apply: {err}");
      // Distinct from probe failures so scripts can tell "sandbox didn't load".
      return 2;
    }
    println!("sandbox: applied");

    // Keep any pre-bound listener alive until after the connect probe.
    let _listener = listener;

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

    let connect_result = probe_connect_localhost(connect_port);
    unexpected_success |= report_action(
      &format!("connect to 127.0.0.1:{}", connect_port),
      connect_result,
      matches!(args.mode, SandboxMode::Strict | SandboxMode::Relaxed),
    );

    println!();
    println!("== IPC capability matrix (after sandbox) ==");

    let socketpair_result = probe_unix_stream_pair();
    unexpected_success |= report_action(
      "ipc: unix socketpair (UnixStream::pair)",
      socketpair_result,
      false,
    );

    let pipe_result = probe_pipe();
    unexpected_success |= report_action("ipc: pipe() (anonymous)", pipe_result, false);

    let listener_result = probe_unix_listener_bind_temp(&temp_dir);
    unexpected_success |= report_action(
      &format!("ipc: unix listener bind under {}", temp_dir.display()),
      listener_result,
      matches!(args.mode, SandboxMode::Strict),
    );

    let exit_code = if unexpected_success { 1 } else { 0 };
    println!("exit_code: {exit_code}");
    exit_code
  }

  fn prepare_listener(port: u16) -> (Option<TcpListener>, u16) {
    if port == 0 {
      match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => {
          let actual_port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
          println!("listener: bound to 127.0.0.1:{actual_port}");
          return (Some(listener), actual_port);
        }
        Err(err) => {
          eprintln!("listener: failed to bind ephemeral port: {err}");
          return (None, 0);
        }
      }
    }

    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
      Ok(listener) => {
        println!("listener: bound to 127.0.0.1:{port}");
        (Some(listener), port)
      }
      Err(err) => {
        eprintln!(
          "listener: could not bind to 127.0.0.1:{port}: {err} (connect probe may return connection refused)"
        );
        (None, port)
      }
    }
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

  fn probe_unix_stream_pair() -> ActionResult {
    let (mut a, mut b) = match UnixStream::pair() {
      Ok(pair) => pair,
      Err(err) => return ActionResult::failure(err),
    };

    let _ = a.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = b.set_read_timeout(Some(Duration::from_secs(2)));

    let payload = b"fastrender-ipc";
    if let Err(err) = a.write_all(payload).and_then(|_| a.flush()) {
      return ActionResult::failure(err);
    }

    let mut buf = vec![0u8; payload.len()];
    if let Err(err) = b.read_exact(&mut buf) {
      return ActionResult::failure(err);
    }

    if buf == payload {
      ActionResult::success(format!("sent {} bytes", payload.len()))
    } else {
      ActionResult::failure(io::Error::new(
        io::ErrorKind::Other,
        "socketpair message mismatch",
      ))
    }
  }

  fn probe_unix_listener_bind_temp(temp_dir: &PathBuf) -> ActionResult {
    let filename = format!("fastrender_sandbox_probe_{}.sock", std::process::id());
    let path = temp_dir.join(filename);

    // Best-effort cleanup if a previous run left the socket file behind.
    let _ = fs::remove_file(&path);

    match UnixListener::bind(&path) {
      Ok(listener) => {
        drop(listener);
        let _ = fs::remove_file(&path);
        ActionResult::success(format!("bound {}", path.display()))
      }
      Err(err) => ActionResult::failure(err),
    }
  }

  fn probe_pipe() -> ActionResult {
    let mut fds = [0i32; 2];
    // SAFETY: `pipe` writes two file descriptors into the provided array on success.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return ActionResult::failure(io::Error::last_os_error());
    }

    // SAFETY: `pipe` returns owned file descriptors. We wrap them so they are closed on drop.
    let mut read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };

    let payload = b"fastrender-pipe";
    if let Err(err) = write_end.write_all(payload).and_then(|_| write_end.flush()) {
      return ActionResult::failure(err);
    }
    drop(write_end);

    let mut buf = vec![0u8; payload.len()];
    if let Err(err) = read_end.read_exact(&mut buf) {
      return ActionResult::failure(err);
    }

    if buf == payload {
      ActionResult::success(format!("sent {} bytes", payload.len()))
    } else {
      ActionResult::failure(io::Error::new(
        io::ErrorKind::Other,
        "pipe message mismatch",
      ))
    }
  }

  #[derive(Debug)]
  struct ActionResult {
    ok: bool,
    detail: String,
    kind: Option<io::ErrorKind>,
    raw_os_error: Option<i32>,
  }

  impl ActionResult {
    fn success(detail: String) -> Self {
      Self {
        ok: true,
        detail,
        kind: None,
        raw_os_error: None,
      }
    }

    fn failure(err: io::Error) -> Self {
      Self {
        ok: false,
        kind: Some(err.kind()),
        raw_os_error: err.raw_os_error(),
        detail: err.to_string(),
      }
    }
  }

  fn report_action(name: &str, result: ActionResult, expected_denied: bool) -> bool {
    let denied = matches!(result.kind, Some(io::ErrorKind::PermissionDenied))
      || matches!(result.raw_os_error, Some(libc::EACCES | libc::EPERM));
    let status = if result.ok {
      "OK"
    } else if denied {
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
      if expected_denied
        && matches!(
          result.kind,
          Some(io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut)
        )
      {
        println!(
          "  note: connection failed without a sandbox permission error. Try --port 0 (default) \
to probe against a self-bound listener."
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
}

#[cfg(target_os = "macos")]
fn main() {
  std::process::exit(enabled::run());
}
