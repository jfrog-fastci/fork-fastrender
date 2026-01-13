use std::ffi::CString;
use std::io;

#[cfg(target_os = "macos")]
mod seatbelt {
  use std::ffi::CStr;
  use std::io;

  use libc::{c_char, c_int, pid_t};

  /// `sandbox_check` filter type for "no additional arguments".
  ///
  /// This corresponds to `SANDBOX_FILTER_NONE` in `<sandbox.h>`.
  pub const SANDBOX_FILTER_NONE: c_int = 0;

  // `sandbox_check` lives in `libsandbox`.
  #[link(name = "sandbox")]
  extern "C" {
    pub fn sandbox_check(pid: pid_t, operation: *const c_char, ty: c_int, ...) -> c_int;
  }

  /// Safe wrapper around `sandbox_check(..., SANDBOX_FILTER_NONE)`.
  pub fn check_operation_allowed_for_pid(pid: pid_t, operation: &CStr) -> io::Result<bool> {
    // SAFETY: `operation` is NUL-terminated, and we pass no varargs for `SANDBOX_FILTER_NONE`.
    let rc = unsafe { sandbox_check(pid, operation.as_ptr(), SANDBOX_FILTER_NONE) };
    if rc == 0 {
      return Ok(true);
    }

    // `sandbox_check` does not have a stable "denied vs errored" contract across all OS releases.
    // In practice:
    // - denied: rc != 0 (often -1) and errno is EPERM/EACCES
    // - invalid operation/type: rc == -1 and errno is EINVAL
    if rc == -1 {
      let err = io::Error::last_os_error();
      if matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
        return Ok(false);
      }
      return Err(err);
    }

    Ok(false)
  }

  pub fn check_operation_allowed(operation: &CStr) -> io::Result<bool> {
    // SAFETY: libc FFI.
    let pid = unsafe { libc::getpid() };
    check_operation_allowed_for_pid(pid, operation)
  }
}

/// Debug helper to query the current macOS Seatbelt sandbox for an operation decision.
///
/// This is intended for diagnostics/tests (e.g. "why did this network syscall succeed?") and
/// currently uses `SANDBOX_FILTER_NONE` to avoid requiring operation-specific filter arguments.
///
/// On non-macOS platforms this returns `ErrorKind::Unsupported`.
pub fn check_operation_allowed(operation: &str) -> io::Result<bool> {
  let operation = CString::new(operation).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "sandbox operation contains an interior NUL byte",
    )
  })?;

  #[cfg(target_os = "macos")]
  {
    seatbelt::check_operation_allowed(&operation)
  }

  #[cfg(not(target_os = "macos"))]
  {
    let _ = operation;
    Err(io::Error::new(
      io::ErrorKind::Unsupported,
      "sandbox_check diagnostics are only available on macOS",
    ))
  }
}

