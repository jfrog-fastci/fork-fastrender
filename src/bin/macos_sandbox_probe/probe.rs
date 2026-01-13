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
fn main() {
  macos::main();
}

#[cfg(target_os = "macos")]
mod macos {
  use clap::{Parser, ValueEnum};
  use fastrender::ipc::shmem::generate_shmem_id;
  use std::collections::BTreeSet;
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
    PureComputation,
  }

  pub(super) fn main() {
    std::process::exit(run());
  }

  fn run() -> i32 {
    let args = Args::parse();
    println!("mode: {:?}", args.mode);

    let temp_dir = std::env::temp_dir();
    let (listener, connect_port) = prepare_listener(args.port);
    let preopened_socketpair = UnixStream::pair();
    let preopened_pipe = create_pipe_files();
    let (preopened_shm_fd, post_sandbox_shm_name) = setup_posix_shm_inputs();

    let (profile, profile_flags) = build_profile(args.mode, &temp_dir);
    if let Err(err) = apply_sandbox(&profile, profile_flags) {
      eprintln!("sandbox: failed to apply: {err}");
      // Distinct from probe failures so scripts can tell "sandbox didn't load".
      return 2;
    }
    println!("sandbox: applied");

    // Keep any pre-bound listener alive until after the connect probe.
    let _listener = listener;

    let mut unexpected_success = false;

    let expect_denied_read = matches!(
      args.mode,
      SandboxMode::Strict | SandboxMode::Relaxed | SandboxMode::PureComputation
    );
    let expect_denied_write = matches!(
      args.mode,
      SandboxMode::Strict | SandboxMode::PureComputation
    );
    let expect_denied_connect = matches!(
      args.mode,
      SandboxMode::Strict | SandboxMode::Relaxed | SandboxMode::PureComputation
    );
    let expect_denied_unix_listener = matches!(
      args.mode,
      SandboxMode::Strict | SandboxMode::PureComputation
    );

    let read_result = probe_read_passwd();
    unexpected_success |= report_action("read /etc/passwd", read_result, expect_denied_read);

    let write_result = probe_write_temp_file(&temp_dir);
    unexpected_success |= report_action(
      &format!("write temp file under {}", temp_dir.display()),
      write_result,
      expect_denied_write,
    );

    let connect_result = probe_connect_localhost(connect_port);
    unexpected_success |= report_action(
      &format!("connect to 127.0.0.1:{}", connect_port),
      connect_result,
      expect_denied_connect,
    );

    println!();
    println!("== IPC capability matrix (after sandbox) ==");

    report_capability(
      "ipc: unix socketpair (create after sandbox)",
      probe_unix_stream_pair_create_after_sandbox(),
    );

    let socketpair_inherited_result = match preopened_socketpair {
      Ok((a, b)) => probe_unix_stream_endpoints(a, b),
      Err(err) => ActionResult::failure_with_context("pre-sandbox UnixStream::pair", err),
    };
    report_capability(
      "ipc: unix socketpair (created before sandbox)",
      socketpair_inherited_result,
    );

    report_capability(
      "ipc: pipe() (create after sandbox)",
      probe_pipe_create_after_sandbox(),
    );

    let pipe_inherited_result = match preopened_pipe {
      Ok((read_end, write_end)) => probe_pipe_endpoints(read_end, write_end),
      Err(err) => ActionResult::failure_with_context("pre-sandbox pipe()", err),
    };
    report_capability(
      "ipc: pipe() (created before sandbox)",
      pipe_inherited_result,
    );

    let listener_result = probe_unix_listener_bind_temp(&temp_dir);
    let listener_unexpected_success = listener_result.ok && expect_denied_unix_listener;
    let listener_name = format!("ipc: unix listener bind under {}", temp_dir.display());
    report_capability(&listener_name, listener_result);
    unexpected_success |= listener_unexpected_success;

    println!();
    println!("== POSIX shared memory (after sandbox) ==");

    report_capability(
      "ipc: posix shmem create (shm_open+ftruncate+mmap)",
      probe_posix_shm_create_after_sandbox(&post_sandbox_shm_name),
    );

    let shm_mmap_result = match preopened_shm_fd {
      Ok(fd) => {
        let result = probe_posix_shm_mmap_inherited_fd(fd);
        // SAFETY: `fd` is a live file descriptor owned by this process.
        unsafe {
          libc::close(fd);
        }
        result
      }
      Err(err) => ActionResult::failure_with_context("pre-sandbox shm_open", err),
    };
    report_capability(
      "ipc: posix shmem mmap(inherited fd) (mmap)",
      shm_mmap_result,
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

  fn build_profile(mode: SandboxMode, temp_dir: &PathBuf) -> (String, u64) {
    // NOTE: Seatbelt string escaping is C-like; we keep it minimal and only escape quotes and
    // backslashes in the dynamically injected temp-dir path.
    let temp_dir_variants = seatbelt_path_variants(temp_dir);
    match mode {
      SandboxMode::Strict => (
        {
          let write_rules = temp_dir_variants
            .iter()
            .map(|dir| format!("(deny file-write* (subpath \"{dir}\"))"))
            .collect::<Vec<_>>()
            .join("\n");
          format!(
            r#"(version 1)
  (allow default)
  (deny network*)
  (deny file-read* (literal "/etc/passwd"))
  (deny file-read* (literal "/private/etc/passwd"))
  {write_rules}
  "#
          )
        },
        SANDBOX_PROFILE,
      ),
      SandboxMode::Relaxed => (
        r#"(version 1)
  (allow default)
  (deny network*)
  (deny file-read* (literal "/etc/passwd"))
  (deny file-read* (literal "/private/etc/passwd"))
  "#
        .to_string(),
        SANDBOX_PROFILE,
      ),
      SandboxMode::PureComputation => ("pure-computation".to_string(), SANDBOX_NAMED),
    }
  }

  fn seatbelt_path_variants(path: &PathBuf) -> Vec<String> {
    let mut variants = BTreeSet::new();
    variants.insert(escape_seatbelt_string(&path.to_string_lossy()));

    if let Ok(canonical) = path.canonicalize() {
      variants.insert(escape_seatbelt_string(&canonical.to_string_lossy()));
    }

    // macOS path aliases: `/etc`, `/tmp`, and `/var` typically resolve into `/private/*`.
    // Ensure we include both forms in case Seatbelt matches the resolved (canonical) path.
    let existing: Vec<String> = variants.iter().cloned().collect();
    for candidate in existing {
      if let Some(stripped) = candidate.strip_prefix("/private") {
        variants.insert(stripped.to_string());
      } else if candidate.starts_with("/etc/") || candidate == "/etc" {
        variants.insert(format!("/private{candidate}"));
      } else if candidate.starts_with("/var/") || candidate == "/var" {
        variants.insert(format!("/private{candidate}"));
      } else if candidate.starts_with("/tmp/") || candidate == "/tmp" {
        variants.insert(format!("/private{candidate}"));
      }
    }

    variants.into_iter().collect()
  }

  fn escape_seatbelt_string(raw: &str) -> String {
    raw.replace('\\', r"\\").replace('"', r#"\""#)
  }

  fn probe_read_passwd() -> ActionResult {
    match fs::read("/etc/passwd").or_else(|err| {
      if err.kind() == io::ErrorKind::NotFound {
        fs::read("/private/etc/passwd")
      } else {
        Err(err)
      }
    }) {
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

  fn probe_unix_stream_pair_create_after_sandbox() -> ActionResult {
    match UnixStream::pair() {
      Ok((a, b)) => probe_unix_stream_endpoints(a, b),
      Err(err) => ActionResult::failure(err),
    }
  }

  fn probe_unix_stream_endpoints(mut a: UnixStream, mut b: UnixStream) -> ActionResult {
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

  fn create_pipe_files() -> io::Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0i32; 2];
    // SAFETY: `pipe` writes two file descriptors into the provided array on success.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }

    // SAFETY: `pipe` returns owned file descriptors. We wrap them so they are closed on drop.
    let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((read_end, write_end))
  }

  fn probe_pipe_create_after_sandbox() -> ActionResult {
    match create_pipe_files() {
      Ok((read_end, write_end)) => probe_pipe_endpoints(read_end, write_end),
      Err(err) => ActionResult::failure(err),
    }
  }

  fn probe_pipe_endpoints(
    mut read_end: std::fs::File,
    mut write_end: std::fs::File,
  ) -> ActionResult {
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

    fn failure_with_context(context: &'static str, err: io::Error) -> Self {
      Self {
        ok: false,
        kind: Some(err.kind()),
        raw_os_error: err.raw_os_error(),
        detail: format!("{context}: {err}"),
      }
    }
  }

  fn report_action(name: &str, result: ActionResult, expected_denied: bool) -> bool {
    let denied = matches!(result.kind, Some(io::ErrorKind::PermissionDenied))
      || matches!(result.raw_os_error, Some(libc::EACCES | libc::EPERM));
    let status = if result.ok {
      "ALLOWED"
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
      if let Some(errno) = result.raw_os_error {
        println!(
          "{name}: {status} ({expected}; errno={errno}; error={})",
          result.detail
        );
      } else {
        println!("{name}: {status} ({expected}; error={})", result.detail);
      }
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

  fn report_capability(name: &str, result: ActionResult) {
    if result.ok {
      println!("{name}: ALLOWED ({})", result.detail);
      return;
    }
    if let Some(errno) = result.raw_os_error {
      println!("{name}: DENIED (errno={errno}; error={})", result.detail);
    } else {
      println!("{name}: DENIED (error={})", result.detail);
    }
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

  const SANDBOX_NAMED: u64 = 0x0001;
  const SANDBOX_PROFILE: u64 = 0x0002;
  const POSIX_SHM_LEN: usize = 4096;

  // Minimal embedded fallback for the strict `pure-computation` sandbox.
  //
  // This matches `src/sandbox/macos.rs`'s embedded fallback: deny filesystem + network while still
  // allowing enough of the runtime to keep the process alive.
  const PURE_COMPUTATION_FALLBACK_PROFILE: &str = r#"(version 1)
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

  fn error_indicates_unknown_profile(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unknown profile")
      || lower.contains("no such profile")
      || lower.contains("profile not found")
      || lower.contains("invalid profile")
  }

  fn sandbox_init_profile(profile: &CStr, flags: u64) -> Result<(), String> {
    let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: Calls into macOS' libsandbox. `errorbuf` is either left null or set to a malloc'd
    // C-string that must be released with `sandbox_free_error`.
    let rc = unsafe { sandbox_init(profile.as_ptr(), flags, &mut errorbuf) };
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

  fn apply_sandbox(profile: &str, flags: u64) -> Result<(), String> {
    let profile =
      CString::new(profile).map_err(|_| "sandbox profile contains NUL byte".to_string())?;

    match sandbox_init_profile(&profile, flags) {
      Ok(()) => Ok(()),
      Err(err) => {
        if flags == SANDBOX_NAMED && error_indicates_unknown_profile(&err) {
          // Fall back to applying a minimal embedded profile as raw SBPL source.
          let fallback = CString::new(PURE_COMPUTATION_FALLBACK_PROFILE)
            .expect("embedded fallback contains no NUL bytes"); // fastrender-allow-unwrap
          match sandbox_init_profile(&fallback, SANDBOX_PROFILE) {
            Ok(()) => Ok(()),
            Err(fallback_err) => Err(format!(
              "failed to apply Seatbelt named profile '{}' (error: {err}); fallback profile also failed (error: {fallback_err})",
              profile.to_string_lossy()
            )),
          }
        } else {
          Err(err)
        }
      }
    }
  }

  // ============================================================================
  // POSIX shared memory probes (shm_open + mmap)
  // ============================================================================

  fn setup_posix_shm_inputs() -> (Result<i32, io::Error>, CString) {
    // macOS's `shm_open` name length limit is commonly `PSHMNAMLEN=31` bytes, including the leading
    // `/`. Use the shared generator so probe runs match production behavior.
    let pre_name =
      CString::new(format!("/{}", generate_shmem_id())).expect("generated shm name contains no NUL"); // fastrender-allow-unwrap
    let post_name =
      CString::new(format!("/{}", generate_shmem_id())).expect("generated shm name contains no NUL"); // fastrender-allow-unwrap

    (create_posix_shm_object_pre_sandbox(&pre_name), post_name)
  }

  fn create_posix_shm_object_pre_sandbox(name: &CString) -> io::Result<i32> {
    // Best-effort cleanup in case a previous run left the name behind.
    unsafe {
      libc::shm_unlink(name.as_ptr());
    }

    // SAFETY: libc FFI. Returns an owned FD on success.
    let fd = unsafe {
      libc::shm_open(
        name.as_ptr(),
        libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
        0o600,
      )
    };
    if fd < 0 {
      return Err(io::Error::last_os_error());
    }

    // SAFETY: libc FFI.
    let rc = unsafe { libc::ftruncate(fd, POSIX_SHM_LEN as libc::off_t) };
    if rc != 0 {
      let err = io::Error::last_os_error();
      unsafe {
        libc::shm_unlink(name.as_ptr());
        libc::close(fd);
      }
      return Err(err);
    }

    // Unlink immediately so we don't leave named objects behind even if `shm_unlink` is denied after
    // sandbox activation. The FD remains usable.
    unsafe {
      libc::shm_unlink(name.as_ptr());
    }

    Ok(fd)
  }

  fn best_effort_shm_unlink(name: &CString) {
    unsafe {
      libc::shm_unlink(name.as_ptr());
    }
  }

  fn probe_posix_shm_create_after_sandbox(name: &CString) -> ActionResult {
    // Best-effort cleanup if a previous run left the name behind.
    best_effort_shm_unlink(name);

    // SAFETY: libc FFI. Returns an owned FD on success.
    let fd = unsafe {
      libc::shm_open(
        name.as_ptr(),
        libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
        0o600,
      )
    };
    if fd < 0 {
      return ActionResult::failure_with_context("shm_open", io::Error::last_os_error());
    }

    // SAFETY: libc FFI.
    let rc = unsafe { libc::ftruncate(fd, POSIX_SHM_LEN as libc::off_t) };
    if rc != 0 {
      let err = io::Error::last_os_error();
      unsafe {
        libc::close(fd);
      }
      best_effort_shm_unlink(name);
      return ActionResult::failure_with_context("ftruncate", err);
    }

    // SAFETY: libc FFI. `mmap` returns MAP_FAILED on error and sets errno.
    let mapped = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        POSIX_SHM_LEN,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
      )
    };
    if mapped == libc::MAP_FAILED {
      let err = io::Error::last_os_error();
      unsafe {
        libc::close(fd);
      }
      best_effort_shm_unlink(name);
      return ActionResult::failure_with_context("mmap", err);
    }

    unsafe {
      // Touch the mapping to confirm it's writable.
      (mapped as *mut u8).write_volatile(0xAB);
      libc::munmap(mapped, POSIX_SHM_LEN);
      libc::close(fd);
    }
    best_effort_shm_unlink(name);
    ActionResult::success("created+mapped".to_string())
  }

  fn probe_posix_shm_mmap_inherited_fd(fd: i32) -> ActionResult {
    // SAFETY: libc FFI. `mmap` returns MAP_FAILED on error and sets errno.
    let mapped = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        POSIX_SHM_LEN,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
      )
    };
    if mapped == libc::MAP_FAILED {
      return ActionResult::failure_with_context("mmap", io::Error::last_os_error());
    }
    unsafe {
      (mapped as *mut u8).write_volatile(0xCD);
      libc::munmap(mapped, POSIX_SHM_LEN);
    }
    ActionResult::success("mapped".to_string())
  }
}
