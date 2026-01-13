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

pub use super::seatbelt::escape_seatbelt_string_literal;

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use crate::sandbox::config;

/// Debug escape hatch: disable the macOS Seatbelt renderer sandbox entirely.
///
/// This matches the Windows escape-hatch name so cross-platform harnesses can share a single knob.
pub const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

/// Developer override for selecting a macOS Seatbelt sandbox profile.
///
/// Accepted values (case-insensitive):
/// - `pure-computation`, `pure`, `strict` => strict sandbox (default)
/// - `system-fonts`, `fonts`, `relaxed` => renderer-friendly sandbox that allows reading system font paths
/// - `off`, `0`, `false`, `no` => disable sandbox (equivalent to `FASTR_DISABLE_RENDERER_SANDBOX=1`)
///
/// Note: multiprocess renderer entrypoints should prefer `FASTR_RENDERER_SANDBOX=strict|relaxed|off`;
/// this env var remains as a macOS-only legacy alias and is only consulted when
/// `FASTR_RENDERER_SANDBOX` is unset.
pub const ENV_MACOS_RENDERER_SANDBOX: &str = "FASTR_MACOS_RENDERER_SANDBOX";

fn env_var_truthy(raw: Option<&std::ffi::OsStr>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  if raw.is_empty() {
    return false;
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  !matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

fn sandbox_disabled_via_env() -> bool {
  if env_var_truthy(std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX).as_deref()) {
    return true;
  }

  // New multiprocess renderer sandbox knob (`FASTR_RENDERER_SANDBOX=strict|relaxed|off`).
  // Treat explicit `off`/`0` as a sandbox disable escape hatch.
  if let Some(raw) = std::env::var_os(config::ENV_RENDERER_SANDBOX) {
    if !raw.is_empty() {
      let raw = raw.to_string_lossy();
      let trimmed = raw.trim();
      if !trimmed.is_empty() && matches!(trimmed.to_ascii_lowercase().as_str(), "0" | "off") {
        return true;
      }
    }
  }

  let Some(raw) = std::env::var_os(ENV_MACOS_RENDERER_SANDBOX) else {
    return false;
  };
  if raw.is_empty() {
    return false;
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

fn log_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: macOS Seatbelt renderer sandbox is DISABLED (debug escape hatch; INSECURE). \
Set {ENV_DISABLE_RENDERER_SANDBOX}=0/1, {config_var}={strict}|{relaxed}|{off}, or {ENV_MACOS_RENDERER_SANDBOX}=pure-computation|system-fonts|off to control this.",
      config_var = config::ENV_RENDERER_SANDBOX,
      strict = "strict",
      relaxed = "relaxed",
      off = "off",
    );
  });
}

fn sandbox_mode_override_from_env() -> io::Result<Option<MacosSandboxMode>> {
  if let Some(raw) = std::env::var_os(config::ENV_RENDERER_SANDBOX) {
    if !raw.is_empty() {
      let raw = raw.to_string_lossy();
      let trimmed = raw.trim();
      if !trimmed.is_empty() {
        return match trimmed.to_ascii_lowercase().as_str() {
          "0" | "off" => Ok(None),
          "1" | "strict" => Ok(Some(MacosSandboxMode::PureComputation)),
          "relaxed" => Ok(Some(MacosSandboxMode::RendererSystemFonts)),
          other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
              "invalid {}={other:?} (expected strict|relaxed|off)",
              config::ENV_RENDERER_SANDBOX
            ),
          )),
        };
      }
    }
  }

  let Some(raw) = std::env::var_os(ENV_MACOS_RENDERER_SANDBOX) else {
    return Ok(None);
  };
  if raw.is_empty() {
    return Ok(None);
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Ok(None);
  }
  let normalized = trimmed.to_ascii_lowercase().replace('_', "-");
  match normalized.as_str() {
    "pure-computation" | "pure" | "strict" => Ok(Some(MacosSandboxMode::PureComputation)),
    "system-fonts" | "fonts" | "relaxed" | "renderer-system-fonts" => {
      Ok(Some(MacosSandboxMode::RendererSystemFonts))
    }
    "0" | "false" | "no" | "off" => Ok(None),
    other => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "invalid {ENV_MACOS_RENDERER_SANDBOX}={other:?} (expected pure-computation|system-fonts|off)"
      ),
    )),
  }
}

fn apply_renderer_sandbox_inner(mode: MacosSandboxMode) -> io::Result<MacosSandboxStatus> {
  match mode {
    MacosSandboxMode::PureComputation => apply_strict_sandbox_hardened_profile(),
    MacosSandboxMode::RendererSystemFonts => {
      apply_profile_source_with_home_param(RENDERER_SYSTEM_FONTS_PROFILE)
    }
  }
}

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
  fn sandbox_check(
    pid: libc::pid_t,
    operation: *const libc::c_char,
    r#type: libc::c_int,
    ...
  ) -> libc::c_int;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSandboxStatus {
  /// The sandbox profile was installed successfully by this call.
  Applied,
  /// The process was already running under a sandbox (e.g. launched from an App Sandbox or wrapped
  /// in `sandbox-exec`), so `sandbox_init(3)` reported that it could not install a new profile.
  ///
  /// Callers should treat this as "sandbox is active" and continue running under the inherited
  /// sandbox policy.
  AlreadySandboxed,
  /// Sandbox installation was not requested (e.g. env var unset in `apply_macos_sandbox_from_env`).
  Disabled,
}

const RENDERER_SYSTEM_FONTS_PROFILE: &str = r#"(version 1)
(deny default)

;; Allow basic runtime operations (threads, sysctl reads, mach services, etc.).
;; This is part of macOS itself: /System/Library/Sandbox/Profiles/system.sb
(import "system.sb")

;; Block all networking.
(deny network*)

;; Defense in depth: prevent "no network" bypasses by talking to system daemons over XPC/mach.
;; `com.apple.nsurlsessiond` is a common NSURLSession helper that can perform network on behalf of
;; the client.
(deny mach-lookup (global-name "com.apple.nsurlsessiond"))

;; Block writes everywhere.
(deny file-write*)

;; Explicitly deny reads from user-controlled / sensitive locations.
(deny file-read* (subpath (param "HOME")))
(deny file-read* (subpath (param "TMPDIR")))
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

fn sbpl_quote(value: &str) -> String {
  let escaped = escape_seatbelt_string_literal(value);
  let mut out = String::with_capacity(escaped.len() + 2);
  out.push('"');
  out.push_str(&escaped);
  out.push('"');
  out
}

/// Builds the renderer sandbox SBPL profile source, allowing only the supplied POSIX shared memory
/// names (`shm_open`) in addition to the baseline renderer policy.
///
/// Seatbelt's POSIX shared memory names can be represented with or without a leading `/` depending
/// on the API layer, so this helper allowlists both forms for each name.
pub fn build_renderer_sbpl(allowed_posix_shm_names: &[&str]) -> String {
  use std::collections::BTreeSet;

  let mut allowed = BTreeSet::<String>::new();
  for &name in allowed_posix_shm_names {
    if name.is_empty() {
      continue;
    }
    allowed.insert(name.to_string());
    if let Some(without_slash) = name.strip_prefix('/') {
      if !without_slash.is_empty() {
        allowed.insert(without_slash.to_string());
      }
    } else {
      allowed.insert(format!("/{name}"));
    }
  }

  let mut sbpl = String::from(RENDERER_SYSTEM_FONTS_PROFILE);
  if !sbpl.ends_with('\n') {
    sbpl.push('\n');
  }

  if allowed.is_empty() {
    return sbpl;
  }

  sbpl.push_str("\n;; Allow POSIX shared memory IPC for pixel buffers (restricted by name).\n");
  for name in allowed {
    sbpl.push_str("(allow ipc-posix-shm (ipc-posix-name ");
    sbpl.push_str(&sbpl_quote(&name));
    sbpl.push_str("))\n");
  }
  sbpl
}

// Minimal embedded fallback for the strict `pure-computation` sandbox.
//
// Requirements:
// - `(version 1)`
// - `(deny default)`
// - deny file-read*, file-write*, and network*
// - allow enough for basic runtime (threads, memory, stdio)
const STRICT_FALLBACK_PROFILE: &str = r#"(version 1)
(deny default)
(import "system.sb")
(deny file-read*)
(deny file-write*)
(deny network*)
(deny process*)
(allow file-read-data (vnode-type PIPE))
(allow file-write-data (vnode-type PIPE))
(allow file-read-metadata (vnode-type PIPE))
(allow file-read-data (vnode-type CHAR-DEVICE))
(allow file-write-data (vnode-type CHAR-DEVICE))
(allow file-read-metadata (vnode-type CHAR-DEVICE))
(allow file-ioctl (vnode-type PIPE))
(allow file-ioctl (vnode-type CHAR-DEVICE))
(allow sysctl-read)
(allow mach-lookup)
(deny mach-lookup (global-name "com.apple.nsurlsessiond"))
(allow ipc-posix-shm)
(allow ipc-posix-sem)
"#;

// Strict sandbox profile used when the system ships `pure-computation` as an importable SBPL file.
//
// We prefer this over `SANDBOX_NAMED` so we can layer additional defense-in-depth denies while still
// inheriting Apple's upstream `pure-computation` profile semantics.
const PURE_COMPUTATION_HARDENED_PROFILE: &str = r#"(version 1)
(import "pure-computation.sb")

;; Defense in depth: prevent "no network" bypasses via system XPC daemons.
(deny mach-lookup (global-name "com.apple.nsurlsessiond"))
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrictSandboxBackend {
  NamedProfile,
  EmbeddedFallback,
}

fn error_indicates_already_sandboxed(errno: Option<i32>, message: &str) -> bool {
  if matches!(errno, Some(code) if code == libc::EALREADY) {
    return true;
  }
  let lower = message.to_ascii_lowercase();
  lower.contains("already") && lower.contains("sandbox")
}

// `sandbox_check` filters are not exposed in `libc` either. These values match `<sandbox.h>`.
const SANDBOX_FILTER_NONE: libc::c_int = 0;
const SANDBOX_FILTER_PATH: libc::c_int = 1;
const SANDBOX_FILTER_GLOBAL_NAME: libc::c_int = 2;

const OP_FILE_READ_DATA: &[u8] = b"file-read-data\0";
const OP_FILE_READ_METADATA: &[u8] = b"file-read-metadata\0";
const OP_MACH_LOOKUP: &[u8] = b"mach-lookup\0";
const OP_NETWORK_OUTBOUND: &[u8] = b"network-outbound\0";

fn cstr_from_bytes(bytes: &'static [u8]) -> &'static CStr {
  // SAFETY: all call sites pass string literals with a trailing NUL byte and no interior NULs.
  unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

fn sandbox_check_inner(
  operation: &CStr,
  filter: libc::c_int,
  filter_arg: Option<&CStr>,
) -> io::Result<bool> {
  // SAFETY: `sandbox_check` is an FFI call. When the filter is PATH we pass a valid NUL-terminated
  // C string as the corresponding vararg.
  let rc = unsafe {
    match filter_arg {
      Some(arg) => sandbox_check(0, operation.as_ptr(), filter, arg.as_ptr()),
      None => sandbox_check(0, operation.as_ptr(), filter),
    }
  };
  if rc == 0 {
    return Ok(true);
  }
  if rc > 0 {
    // Seatbelt returns a positive errno-style value for denied operations (e.g. EPERM).
    return Ok(false);
  }
  Err(io::Error::last_os_error())
}

fn format_sandbox_check(result: io::Result<bool>) -> String {
  match result {
    Ok(true) => "allowed".to_string(),
    Ok(false) => "denied".to_string(),
    Err(err) => format!("error({:?}): {err}", err.kind()),
  }
}

/// Query whether the current process' Seatbelt sandbox would allow reading the file at `path`.
///
/// This uses `sandbox_check(3)` with the `SANDBOX_FILTER_PATH` filter against both `file-read-data`
/// and `file-read-metadata`.
pub fn sandbox_check_file_read(path: &Path) -> io::Result<bool> {
  let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "sandbox_check path contains NUL byte")
  })?;
  let data_allowed =
    sandbox_check_inner(cstr_from_bytes(OP_FILE_READ_DATA), SANDBOX_FILTER_PATH, Some(&path))?;
  let meta_allowed = sandbox_check_inner(
    cstr_from_bytes(OP_FILE_READ_METADATA),
    SANDBOX_FILTER_PATH,
    Some(&path),
  )?;
  Ok(data_allowed && meta_allowed)
}

/// Render a human-friendly `sandbox_check` verdict for [`sandbox_check_file_read`].
pub fn sandbox_check_file_read_diagnostic(path: &Path) -> String {
  format_sandbox_check(sandbox_check_file_read(path))
}

/// Query whether the current process' Seatbelt sandbox would allow outbound network connections.
///
/// This uses `sandbox_check(3)` for the `network-outbound` operation with no additional filters.
pub fn sandbox_check_network_outbound() -> io::Result<bool> {
  sandbox_check_inner(
    cstr_from_bytes(OP_NETWORK_OUTBOUND),
    SANDBOX_FILTER_NONE,
    None,
  )
}

/// Query whether the current process' Seatbelt sandbox would allow `mach-lookup` for a given
/// Mach/XPC service name.
///
/// This is defense-in-depth against system daemons that can proxy privileged work (e.g. networking)
/// over XPC. Callers can use this to assert that a "no network" sandbox also blocks access to
/// `com.apple.nsurlsessiond` and similar services.
pub fn sandbox_check_mach_lookup(service: &str) -> io::Result<bool> {
  let service = CString::new(service).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "sandbox_check mach service contains NUL byte",
    )
  })?;
  sandbox_check_inner(
    cstr_from_bytes(OP_MACH_LOOKUP),
    SANDBOX_FILTER_GLOBAL_NAME,
    Some(service.as_c_str()),
  )
}

/// Render a human-friendly `sandbox_check` verdict for [`sandbox_check_network_outbound`].
pub fn sandbox_check_network_outbound_diagnostic() -> String {
  format_sandbox_check(sandbox_check_network_outbound())
}

/// A "relaxed" Seatbelt profile that still denies access to most of the filesystem, but allows
/// read access to a conservative set of system paths needed by typical dynamically-linked Rust
/// binaries.
///
/// This is intentionally an allowlist: anything outside these paths (including the current working
/// directory and `/tmp`) should be denied with a permission error.
pub(crate) const RELAXED_SYSTEM_ALLOWLIST_PROFILE: &str = r#"(version 1)
(deny default)
(allow process*)
(allow file-read*
  (subpath "/System")
  (subpath "/usr/lib")
  (subpath "/usr/share")
  (subpath "/Library")
  (subpath "/dev")
  (subpath "/private/var/db")
)
"#;

fn sandbox_init_profile(profile: &CStr, flags: u64) -> io::Result<MacosSandboxStatus> {
  let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();

  // `sandbox_init(3)` is documented to set `errno` on failure, but callers should not rely on
  // `errno` being meaningful if the implementation forgets to do so. Reset it to avoid treating a
  // stale thread-local value as authoritative (which could mask real sandbox-init failures).
  //
  // SAFETY: `__error()` returns a pointer to the current thread's `errno`.
  unsafe {
    *libc::__error() = 0;
  }

  // SAFETY: `sandbox_init` installs an irreversible process-wide sandbox. The FFI contract requires
  // a NUL-terminated profile string and a valid out-pointer for the error buffer.
  let rc = unsafe { sandbox_init(profile.as_ptr(), flags, &mut errorbuf) };
  if rc == 0 {
    return Ok(MacosSandboxStatus::Applied);
  }

  // SAFETY: `__error()` returns a pointer to the current thread's `errno`.
  let raw_errno = unsafe { *libc::__error() };
  let raw_errno = if raw_errno == 0 { None } else { Some(raw_errno) };
  let message = sandbox_message(errorbuf);
  if error_indicates_already_sandboxed(raw_errno, &message) {
    return Ok(MacosSandboxStatus::AlreadySandboxed);
  }

  Err(io::Error::new(
    io::ErrorKind::Other,
    format!("sandbox_init failed (errno={raw_errno:?}): {message}"),
  ))
}

fn sandbox_init_profile_with_parameters(
  profile: &CStr,
  flags: u64,
  parameters: &[*const libc::c_char],
) -> io::Result<MacosSandboxStatus> {
  let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();

  // SAFETY: `__error()` returns a pointer to the current thread's `errno`.
  unsafe {
    *libc::__error() = 0;
  }

  // SAFETY: `sandbox_init_with_parameters` installs an irreversible process-wide sandbox. The FFI
  // contract requires a NUL-terminated profile string, a NULL-terminated `parameters` list, and a
  // valid out-pointer for the error buffer.
  let rc = unsafe {
    sandbox_init_with_parameters(profile.as_ptr(), flags, parameters.as_ptr(), &mut errorbuf)
  };
  if rc == 0 {
    return Ok(MacosSandboxStatus::Applied);
  }

  // SAFETY: `__error()` returns a pointer to the current thread's `errno`.
  let raw_errno = unsafe { *libc::__error() };
  let raw_errno = if raw_errno == 0 { None } else { Some(raw_errno) };
  let message = sandbox_message(errorbuf);
  if error_indicates_already_sandboxed(raw_errno, &message) {
    return Ok(MacosSandboxStatus::AlreadySandboxed);
  }

  Err(io::Error::new(
    io::ErrorKind::Other,
    format!("sandbox_init_with_parameters failed (errno={raw_errno:?}): {message}"),
  ))
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

pub(crate) fn apply_named_profile(profile_name: &str) -> io::Result<MacosSandboxStatus> {
  let profile_name =
    CString::new(profile_name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL"))?;
  sandbox_init_profile(&profile_name, SANDBOX_NAMED)
}

fn apply_profile_source(profile_source: &str) -> io::Result<MacosSandboxStatus> {
  let profile_source = CString::new(profile_source)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sandbox profile contains NUL"))?;
  sandbox_init_profile(&profile_source, SANDBOX_PROFILE)
}

pub(crate) fn apply_profile_source_with_home_param(
  profile_source: &str,
) -> io::Result<MacosSandboxStatus> {
  let profile_source = CString::new(profile_source)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sandbox profile contains NUL"))?;

  let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
  let home =
    CString::new(home).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "HOME contains NUL"))?;
  let key =
    CString::new("HOME").map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "HOME contains NUL"))?;

  let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
  let tmpdir = CString::new(tmpdir)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "TMPDIR contains NUL"))?;
  let tmpdir_key = CString::new("TMPDIR").expect("static cstr should not contain NUL"); // fastrender-allow-unwrap

  // `sandbox_init_with_parameters` expects a NULL-terminated list: [key, value, key, value, NULL].
  let params: [*const libc::c_char; 5] = [
    key.as_ptr(),
    home.as_ptr(),
    tmpdir_key.as_ptr(),
    tmpdir.as_ptr(),
    std::ptr::null(),
  ];
  sandbox_init_profile_with_parameters(&profile_source, SANDBOX_PROFILE, &params)
}

fn error_indicates_unknown_profile(message: &str) -> bool {
  let lower = message.to_ascii_lowercase();
  lower.contains("unknown profile")
    || lower.contains("no such profile")
    || lower.contains("profile not found")
    || lower.contains("invalid profile")
    // `sandbox_init` can also fail while resolving `import` directives (e.g. if the target SBPL
    // file is missing). Treat these as "missing profile" signals so we can fall back to the
    // embedded strict profile.
    || lower.contains("could not open")
    || lower.contains("cannot open")
    || lower.contains("failed to open")
}

fn apply_strict_sandbox_named_first(
  profile_name: &str,
) -> io::Result<(MacosSandboxStatus, StrictSandboxBackend)> {
  match apply_named_profile(profile_name) {
    Ok(status) => Ok((status, StrictSandboxBackend::NamedProfile)),
    Err(err) => {
      if !error_indicates_unknown_profile(&err.to_string()) {
        return Err(err);
      }

      match apply_profile_source(STRICT_FALLBACK_PROFILE) {
        Ok(status) => Ok((status, StrictSandboxBackend::EmbeddedFallback)),
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

fn apply_strict_sandbox_hardened_profile() -> io::Result<MacosSandboxStatus> {
  match apply_profile_source(PURE_COMPUTATION_HARDENED_PROFILE) {
    Ok(status) => Ok(status),
    Err(err) => {
      if error_indicates_unknown_profile(&err.to_string()) {
        apply_profile_source(STRICT_FALLBACK_PROFILE)
      } else {
        Err(err)
      }
    }
  }
}

/// Apply a strict Seatbelt sandbox profile to the current process.
///
/// This first attempts to apply a hardened SBPL profile that imports macOS's built-in
/// `pure-computation` profile (`pure-computation.sb`) and layers additional defense-in-depth denies
/// (for example, blocking `mach-lookup` to `com.apple.nsurlsessiond`). If the system profile is
/// unavailable (or rejected as invalid), it falls back to an embedded strict SBPL profile string.
///
/// ⚠️ This is irreversible for the lifetime of the process; tests must apply it in a dedicated
/// child process.
pub fn apply_strict_sandbox() -> io::Result<MacosSandboxStatus> {
  if sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return Ok(MacosSandboxStatus::Disabled);
  }

  if let Some(mode) = sandbox_mode_override_from_env()? {
    return apply_renderer_sandbox_inner(mode);
  }

  apply_strict_sandbox_hardened_profile()
}

/// Apply the macOS Seatbelt "pure-computation" sandbox profile to the current process.
///
/// This is an alias for [`apply_strict_sandbox`].
pub fn apply_pure_computation_sandbox() -> io::Result<MacosSandboxStatus> {
  apply_strict_sandbox()
}

/// Apply a renderer-focused sandbox to the current process.
///
/// This call is irreversible: once applied, the process cannot regain privileges.
pub fn apply_renderer_sandbox(mode: MacosSandboxMode) -> io::Result<MacosSandboxStatus> {
  if sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return Ok(MacosSandboxStatus::Disabled);
  }

  let mode = sandbox_mode_override_from_env()?.unwrap_or(mode);
  apply_renderer_sandbox_inner(mode)
}

/// Apply a macOS Seatbelt sandbox profile based on an environment variable.
///
/// When sandboxing is controlled by `FASTR_MACOS_RENDERER_SANDBOX`, callers can use this helper to
/// opt into sandboxing from a parent process (or when running under an App Sandbox wrapper).
///
/// Returns [`MacosSandboxStatus::Disabled`] when no env var requests sandboxing.
pub fn apply_macos_sandbox_from_env() -> io::Result<MacosSandboxStatus> {
  if sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return Ok(MacosSandboxStatus::Disabled);
  }

  let Some(mode) = sandbox_mode_override_from_env()? else {
    return Ok(MacosSandboxStatus::Disabled);
  };

  apply_renderer_sandbox_inner(mode)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ipc::shmem::{generate_shmem_id, MAX_SHMEM_NAME_LEN};
  use std::ffi::CString;
  use std::io::{self, Write};
  use std::net::{TcpListener, TcpStream, UdpSocket};
  use std::process::Command;
  use std::time::{Instant, SystemTime};

  const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";
  const PORT_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_PORT";
  const SHM_ALLOWED_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_SHM_ALLOWED";
  const SHM_DENIED_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_SHM_DENIED";
  const RELAXED_CHILD_ENV: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_CHILD";
  const RELAXED_CWD_FILE_ENV: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_CWD_FILE";
  const RELAXED_TMP_FILE_ENV: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_TMP_FILE";

  fn shm_name(label: &str) -> String {
    // macOS commonly limits POSIX shm names to `PSHMNAMLEN=31` bytes including the leading '/'.
    // Keep these test names within that limit so failures are attributable to the sandbox policy,
    // not name-length errors like `ENAMETOOLONG`.
    let tag = match label {
      "allowed" => 'a',
      "denied" => 'd',
      _ => 'x',
    };
    let name = format!("/{}{}", tag, generate_shmem_id());
    assert!(
      name.len() <= MAX_SHMEM_NAME_LEN,
      "generated shm name too long: {} bytes (max {MAX_SHMEM_NAME_LEN}): {name:?}",
      name.len()
    );
    name
  }

  fn shm_unlink_best_effort(name: &str) {
    let Ok(c_name) = CString::new(name) else {
      return;
    };
    // SAFETY: `c_name` is a valid C string.
    let rc = unsafe { libc::shm_unlink(c_name.as_ptr()) };
    if rc == 0 {
      return;
    }
    let _ = std::io::Error::last_os_error();
  }

  fn shm_open_create(name: &str) -> Result<libc::c_int, std::io::Error> {
    let c_name = CString::new(name).expect("shm name contains NUL byte");
    // SAFETY: `c_name` is a valid C string. `shm_open` returns an owned fd on success.
    let fd = unsafe {
      libc::shm_open(
        c_name.as_ptr(),
        libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
        0o600,
      )
    };
    if fd == -1 {
      return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
  }

  fn close_fd_best_effort(fd: libc::c_int) {
    if fd < 0 {
      return;
    }
    // SAFETY: `fd` came from `shm_open`.
    unsafe {
      libc::close(fd);
    }
  }

  fn is_permission_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
      return true;
    }
    matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES))
  }

  fn assert_spawn_denied(mut command: Command) {
    match command.status() {
      Ok(status) => panic!(
        "expected Seatbelt sandbox to deny spawning {:?}, but it exited with status {status}",
        command
      ),
      Err(err) => assert!(
        is_permission_error(&err),
        "expected sandbox to deny spawning {:?}, got {err:?}",
        command
      ),
    }
  }

  #[test]
  fn seatbelt_pure_computation_blocks_filesystem_and_network() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let port: u16 = std::env::var(PORT_ENV)
        .expect("child process missing sandbox port env var")
        .parse()
        .expect("parse sandbox port env var");

      let status = apply_pure_computation_sandbox().expect("apply pure-computation sandbox");
      assert!(
        matches!(
          status,
          MacosSandboxStatus::Applied | MacosSandboxStatus::AlreadySandboxed
        ),
        "expected sandbox apply to succeed, got {status:?}"
      );
      if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
        eprintln!(
          "skipping Seatbelt policy assertions: process was already sandboxed (status={status:?})"
        );
        return;
      }

      // 1) File read should fail.
      let passwd_private = Path::new("/private/etc/passwd");
      let passwd_etc = Path::new("/etc/passwd");
      let (passwd_path, read_result) = match std::fs::read_to_string(passwd_private) {
        Ok(contents) => (passwd_private, Ok(contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
          (passwd_etc, std::fs::read_to_string(passwd_etc))
        }
        Err(err) => (passwd_private, Err(err)),
      };
      match read_result {
        Ok(contents) => panic!(
          "expected filesystem read to be denied by sandbox (read {} bytes from {}); sandbox_check: {}={}; {}={}",
          contents.len(),
          passwd_path.display(),
          passwd_private.display(),
          sandbox_check_file_read_diagnostic(passwd_private),
          passwd_etc.display(),
          sandbox_check_file_read_diagnostic(passwd_etc)
        ),
        Err(read_err) => assert!(
          is_permission_error(&read_err),
          "expected file read to be denied by sandbox, got {read_err:?}; sandbox_check: {}={}; {}={}",
          passwd_private.display(),
          sandbox_check_file_read_diagnostic(passwd_private),
          passwd_etc.display(),
          sandbox_check_file_read_diagnostic(passwd_etc)
        ),
      }

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
      let bind_err =
        TcpListener::bind("127.0.0.1:0").expect_err("expected network bind to be denied by sandbox");
      assert!(
        is_permission_error(&bind_err),
        "expected network bind to be denied by sandbox, got {bind_err:?}; sandbox_check network-outbound: {}",
        sandbox_check_network_outbound_diagnostic()
      );

      let udp_bind_err =
        UdpSocket::bind("127.0.0.1:0").expect_err("expected UDP bind to be denied by sandbox");
      assert!(
        is_permission_error(&udp_bind_err),
        "expected UDP bind to be denied by sandbox, got {udp_bind_err:?}; sandbox_check network-outbound: {}",
        sandbox_check_network_outbound_diagnostic()
      );

      match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_stream) => panic!(
          "expected network connect to be denied by sandbox; sandbox_check network-outbound: {}",
          sandbox_check_network_outbound_diagnostic()
        ),
        Err(connect_err) => assert!(
          is_permission_error(&connect_err),
          "expected network connect to be denied by sandbox, got {connect_err:?}; sandbox_check network-outbound: {}",
          sandbox_check_network_outbound_diagnostic()
        ),
      };
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
  fn seatbelt_renderer_system_fonts_denies_filesystem_writes() {
    const ENV_TEMP_CREATE_TARGET: &str = "FASTR_TEST_MACOS_SANDBOX_WRITE_DENIED_TEMP_CREATE_TARGET";
    const ENV_HOME_CREATE_TARGET: &str = "FASTR_TEST_MACOS_SANDBOX_WRITE_DENIED_HOME_CREATE_TARGET";
    const ENV_TEMP_EXISTING_TARGET: &str =
      "FASTR_TEST_MACOS_SANDBOX_WRITE_DENIED_TEMP_EXISTING_TARGET";
    const ENV_HOME_EXISTING_TARGET: &str =
      "FASTR_TEST_MACOS_SANDBOX_WRITE_DENIED_HOME_EXISTING_TARGET";

    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let temp_create_target = std::env::var_os(ENV_TEMP_CREATE_TARGET)
        .map(std::path::PathBuf::from)
        .expect("child missing temp create target env var");
      let home_create_target = std::env::var_os(ENV_HOME_CREATE_TARGET)
        .map(std::path::PathBuf::from)
        .expect("child missing home create target env var");
      let temp_existing_target = std::env::var_os(ENV_TEMP_EXISTING_TARGET)
        .map(std::path::PathBuf::from)
        .expect("child missing temp existing target env var");
      let home_existing_target = std::env::var_os(ENV_HOME_EXISTING_TARGET)
        .map(std::path::PathBuf::from)
        .expect("child missing home existing target env var");

      let status = apply_renderer_sandbox(MacosSandboxMode::RendererSystemFonts)
        .expect("apply renderer-system-fonts sandbox profile");
      if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
        eprintln!(
          "skipping renderer-system-fonts assertions: process was already sandboxed (status={status:?})"
        );
        return;
      }

      let temp_err = std::fs::write(&temp_create_target, b"fastrender sandbox write test")
        .expect_err("expected sandbox to deny writes under temp_dir");
      assert!(
        is_permission_error(&temp_err),
        "expected sandbox to deny writes under temp_dir (path={}, err={temp_err:?})",
        temp_create_target.display()
      );

      let home_err = std::fs::write(&home_create_target, b"fastrender sandbox write test")
        .expect_err("expected sandbox to deny writes under $HOME");
      assert!(
        is_permission_error(&home_err),
        "expected sandbox to deny writes under $HOME (path={}, err={home_err:?})",
        home_create_target.display()
      );

      // Also ensure we can't open existing files for writing (defense-in-depth: deny modifying).
      let temp_modify_err = match std::fs::OpenOptions::new()
        .append(true)
        .open(&temp_existing_target)
      {
        Ok(mut file) => file
          .write_all(b"append")
          .expect_err("expected sandbox to deny appending to an existing temp file"),
        Err(err) => err,
      };
      assert!(
        is_permission_error(&temp_modify_err),
        "expected sandbox to deny modifying existing temp file (path={}, err={temp_modify_err:?})",
        temp_existing_target.display()
      );

      let home_modify_err = match std::fs::OpenOptions::new()
        .append(true)
        .open(&home_existing_target)
      {
        Ok(mut file) => file
          .write_all(b"append")
          .expect_err("expected sandbox to deny appending to an existing home file"),
        Err(err) => err,
      };
      assert!(
        is_permission_error(&home_modify_err),
        "expected sandbox to deny modifying existing home file (path={}, err={home_modify_err:?})",
        home_existing_target.display()
      );

      return;
    }

    let temp_dir = std::env::temp_dir();
    let pid = std::process::id();
    let temp_create_target =
      temp_dir.join(format!("fastrender_sandbox_write_test_{pid}_temp_create.txt"));
    let temp_existing_target =
      temp_dir.join(format!("fastrender_sandbox_write_test_{pid}_temp_existing.txt"));

    let home_dir = std::env::var_os("HOME")
      .map(std::path::PathBuf::from)
      .expect("HOME should be set for sandbox write test");
    let caches_dir = home_dir.join("Library").join("Caches");
    let home_parent = if caches_dir.is_dir() { caches_dir } else { home_dir };
    let home_create_target =
      home_parent.join(format!("fastrender_sandbox_write_test_{pid}_home_create.txt"));
    let home_existing_target =
      home_parent.join(format!("fastrender_sandbox_write_test_{pid}_home_existing.txt"));

    // Create seed files that the sandboxed child will attempt to modify. These should be blocked by
    // the sandbox.
    let _ = std::fs::remove_file(&temp_existing_target);
    let _ = std::fs::remove_file(&home_existing_target);
    std::fs::write(&temp_existing_target, b"seed").expect("create temp existing seed file");
    std::fs::write(&home_existing_target, b"seed").expect("create home existing seed file");

    // Best-effort cleanup in case the host environment already has a stale file from a previous run
    // (writes should fail once the sandbox is active, so cleanup must happen before/after via the
    // unsandboxed parent).
    let _ = std::fs::remove_file(&temp_create_target);
    let _ = std::fs::remove_file(&home_create_target);

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::macos::tests::seatbelt_renderer_system_fonts_denies_filesystem_writes";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .env(ENV_TEMP_CREATE_TARGET, &temp_create_target)
      .env(ENV_HOME_CREATE_TARGET, &home_create_target)
      .env(ENV_TEMP_EXISTING_TARGET, &temp_existing_target)
      .env(ENV_HOME_EXISTING_TARGET, &home_existing_target)
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");

    let _ = std::fs::remove_file(&temp_create_target);
    let _ = std::fs::remove_file(&home_create_target);
    let _ = std::fs::remove_file(&temp_existing_target);
    let _ = std::fs::remove_file(&home_existing_target);

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
      let status = apply_pure_computation_sandbox().expect("apply pure-computation sandbox");
      if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
        eprintln!(
          "skipping process-spawn denial assertions: process was already sandboxed (status={status:?})"
        );
        return;
      }

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
      let status = apply_pure_computation_sandbox().expect("apply pure-computation sandbox");
      assert!(
        matches!(
          status,
          MacosSandboxStatus::Applied | MacosSandboxStatus::AlreadySandboxed
        ),
        "expected sandbox apply to succeed, got {status:?}"
      );
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

      let (status, backend) =
        apply_strict_sandbox_named_first("fastrender-nonexistent-seatbelt-profile").expect(
          "apply strict sandbox with embedded fallback when the named profile is missing",
        );
      if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
        eprintln!(
          "skipping strict-sandbox fallback assertions: process was already sandboxed (status={status:?})"
        );
        return;
      }
      assert_eq!(
        backend,
        StrictSandboxBackend::EmbeddedFallback,
        "expected strict sandbox helper to use the embedded fallback profile"
      );

      std::io::stdout()
        .write_all(b"fastrender-seatbelt-fallback-ok")
        .and_then(|_| std::io::stdout().flush())
        .expect("expected stdout to remain usable under fallback sandbox");

      std::thread::spawn(|| 42_u32)
        .join()
        .expect("thread should spawn + join successfully under fallback sandbox");

      let parallelism = std::thread::available_parallelism()
        .expect("available_parallelism should work under fallback sandbox");
      assert!(
        parallelism.get() >= 1,
        "available_parallelism should return >= 1 (got {})",
        parallelism.get()
      );

      let system_now = SystemTime::now();
      let _ = system_now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time should be after UNIX_EPOCH");
      let _instant_now = Instant::now();

      let mut bytes = [0u8; 32];
      getrandom::getrandom(&mut bytes).expect("getrandom should succeed under fallback sandbox");
      assert!(
        bytes.iter().any(|&b| b != 0),
        "getrandom returned an all-zero buffer, which is unexpectedly unlikely"
      );

      // File reads should be denied.
      let passwd_private = Path::new("/private/etc/passwd");
      let passwd_etc = Path::new("/etc/passwd");
      let (passwd_path, read_result) = match std::fs::read_to_string(passwd_private) {
        Ok(contents) => (passwd_private, Ok(contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
          (passwd_etc, std::fs::read_to_string(passwd_etc))
        }
        Err(err) => (passwd_private, Err(err)),
      };
      match read_result {
        Ok(contents) => panic!(
          "expected filesystem read to be denied by sandbox (read {} bytes from {}); sandbox_check: {}={}; {}={}",
          contents.len(),
          passwd_path.display(),
          passwd_private.display(),
          sandbox_check_file_read_diagnostic(passwd_private),
          passwd_etc.display(),
          sandbox_check_file_read_diagnostic(passwd_etc)
        ),
        Err(read_err) => assert!(
          is_permission_error(&read_err),
          "expected file read to be denied by sandbox, got {read_err:?}; sandbox_check: {}={}; {}={}",
          passwd_private.display(),
          sandbox_check_file_read_diagnostic(passwd_private),
          passwd_etc.display(),
          sandbox_check_file_read_diagnostic(passwd_etc)
        ),
      }

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

      let connect_err =
        TcpStream::connect(("127.0.0.1", port)).expect_err("expected network connect to be denied");
      assert!(
        is_permission_error(&connect_err),
        "expected network connect to be denied by sandbox, got {connect_err:?}; sandbox_check network-outbound: {}",
        sandbox_check_network_outbound_diagnostic()
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
      let status = apply_pure_computation_sandbox().expect("apply pure-computation sandbox");
      assert!(
        matches!(
          status,
          MacosSandboxStatus::Applied | MacosSandboxStatus::AlreadySandboxed
        ),
        "expected sandbox apply to succeed, got {status:?}"
      );

      eprintln!("spawning a thread under sandbox");
      std::thread::spawn(|| 42_u32)
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
    let test_name =
      "sandbox::macos::tests::seatbelt_pure_computation_allows_basic_rust_runtime_features";
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
  fn seatbelt_reapply_returns_already_sandboxed_status() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let first = apply_pure_computation_sandbox().expect("apply sandbox first time");
      assert!(
        matches!(
          first,
          MacosSandboxStatus::Applied | MacosSandboxStatus::AlreadySandboxed
        ),
        "expected first sandbox init to succeed, got {first:?}"
      );

      let second = apply_pure_computation_sandbox().expect("apply sandbox second time");
      assert_eq!(
        second,
        MacosSandboxStatus::AlreadySandboxed,
        "expected second sandbox init to report AlreadySandboxed"
      );
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::macos::tests::seatbelt_reapply_returns_already_sandboxed_status";
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
  fn renderer_sbpl_ipc_posix_shm_allowlist() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let allowed = std::env::var(SHM_ALLOWED_ENV).expect("child missing allowed shm name");
      let denied = std::env::var(SHM_DENIED_ENV).expect("child missing denied shm name");

      let sbpl = build_renderer_sbpl(&[allowed.as_str()]);
      let status = apply_profile_source_with_home_param(&sbpl).expect("apply renderer SBPL profile");
      if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
        eprintln!(
          "skipping shm allowlist assertions: process was already sandboxed (status={status:?})"
        );
        return;
      }

      let fd = shm_open_create(&allowed).unwrap_or_else(|err| {
        panic!("shm_open({allowed}) should succeed in sandbox: {err} (sbpl={sbpl:?})");
      });

      let denied_err = shm_open_create(&denied).expect_err("expected sandbox to deny shm_open");
      assert!(
        is_permission_error(&denied_err),
        "expected denied shm_open to fail with permission error, got {denied_err:?}"
      );

      // Best-effort cleanup. The parent process also unlinks after the child returns to avoid
      // leaving segments behind if the child crashes mid-test.
      shm_unlink_best_effort(&allowed);
      close_fd_best_effort(fd);
      return;
    }

    let allowed = shm_name("allowed");
    let denied = shm_name("denied");
    shm_unlink_best_effort(&allowed);
    shm_unlink_best_effort(&denied);

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos::tests::renderer_sbpl_ipc_posix_shm_allowlist";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .env(SHM_ALLOWED_ENV, &allowed)
      .env(SHM_DENIED_ENV, &denied)
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");

    // Clean up even if the child failed before unlinking.
    shm_unlink_best_effort(&allowed);
    shm_unlink_best_effort(&denied);
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  #[test]
  fn seatbelt_relaxed_profile_denies_reads_outside_allowlist() {
    // Seatbelt sandboxing is process-wide and irreversible, so run the assertions in a child
    // process to keep the parent test runner unrestricted.
    if std::env::var_os(RELAXED_CHILD_ENV).is_some() {
      let cwd_path = std::env::var_os(RELAXED_CWD_FILE_ENV)
        .map(std::path::PathBuf::from)
        .expect("child missing RELAXED_CWD_FILE_ENV");
      let tmp_path = std::env::var_os(RELAXED_TMP_FILE_ENV)
        .map(std::path::PathBuf::from)
        .expect("child missing RELAXED_TMP_FILE_ENV");

      let status = apply_renderer_sandbox(MacosSandboxMode::RendererSystemFonts)
        .expect("apply relaxed (renderer system fonts) sandbox profile");
      if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
        eprintln!(
          "skipping relaxed sandbox assertions: process was already sandboxed (status={status:?})"
        );
        return;
      }

      let cwd_err = std::fs::read(&cwd_path)
        .expect_err("expected sandbox to deny reading canary file in current working directory");
      assert!(
        is_permission_error(&cwd_err),
        "expected permission error when reading {}, got {cwd_err:?}",
        cwd_path.display()
      );

      let tmp_err = std::fs::read(&tmp_path)
        .expect_err("expected sandbox to deny reading canary file under /tmp");
      assert!(
        is_permission_error(&tmp_err),
        "expected permission error when reading {}, got {tmp_err:?}",
        tmp_path.display()
      );
      return;
    }

    // Create canary files in locations that should be denied by the relaxed renderer sandbox.
    let cwd = std::env::current_dir().expect("current working directory");
    let repo_tmp = tempfile::Builder::new()
      .prefix("fastr_sandbox_relaxed_cwd")
      .tempdir_in(&cwd)
      .expect("create temp dir in current working directory");
    let cwd_file_path = repo_tmp.path().join("canary.txt");
    std::fs::write(&cwd_file_path, b"fastrender sandbox canary")
      .expect("write canary file in cwd temp dir");

    let mut tmp_file = tempfile::Builder::new()
      .prefix("fastr_sandbox_relaxed_tmp")
      .tempfile_in("/tmp")
      .expect("create canary file under /tmp");
    tmp_file
      .write_all(b"fastrender sandbox canary")
      .expect("write canary file under /tmp");
    let _ = tmp_file.flush();

    // Ensure the files are readable before sandboxing so failures are attributed to the sandbox
    // policy rather than missing files.
    std::fs::read(&cwd_file_path).expect("parent should be able to read cwd canary before sandbox");
    std::fs::read(tmp_file.path()).expect("parent should be able to read /tmp canary before sandbox");

    let exe = std::env::current_exe().expect("current test executable path");
    let test_name = "sandbox::macos::tests::seatbelt_relaxed_profile_denies_reads_outside_allowlist";
    let output = Command::new(exe)
      .env(RELAXED_CHILD_ENV, "1")
      .env_os(RELAXED_CWD_FILE_ENV, &cwd_file_path)
      .env_os(RELAXED_TMP_FILE_ENV, tmp_file.path())
      // Keep the libtest harness single-threaded in the child process (best-effort).
      .env("RUST_TEST_THREADS", "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn sandboxed child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );

    // Keep the tempdir and tempfile alive until after the child exits.
    drop(tmp_file);
    drop(repo_tmp);
  }
}
