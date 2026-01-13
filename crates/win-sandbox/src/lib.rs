//! Windows sandboxing primitives (Win32).
//!
//! This crate intentionally contains only small, Windows-focused utilities so
//! higher-level crates can reuse Windows sandbox building blocks (job objects,
//! AppContainer identity helpers, process mitigation policies, etc.) without
//! pulling in the full `fastrender` dependency graph.
//!
//! For the intended Windows renderer sandbox boundary (Job objects + AppContainer
//! + handle allowlisting + mitigations + fallback mode), see
//! `docs/windows_sandbox.md` and the main spawner implementation in
//! `src/sandbox/windows.rs` (in the root `fastrender` crate).
//!
//! For a lightweight manual debugging / repro tool that spawns itself under the sandbox and prints
//! observed token/job/mitigation state, see `examples/probe.rs`:
//!
//! ```text
//! cargo run -p win-sandbox --example probe -- --connect-localhost
//! ```
//!
//! The public API is intentionally safe; internal Win32 calls are wrapped so
//! callers never need to use `unsafe` directly.
//!
//! ## Job objects
//!
//! [`Job`] is a small RAII wrapper around a Windows Job object handle. It can
//! enforce common renderer guardrails:
//!
//! - kill-on-close (`KILL_ON_JOB_CLOSE`)
//! - no child processes (`ActiveProcessLimit = 1`)
//! - optional job-wide committed memory limits
//! - headless UI restrictions
//!
//! When updating extended job limits, the wrapper clears breakaway flags so
//! sandboxed processes cannot escape via `CREATE_BREAKAWAY_FROM_JOB`.
//!
//! ## AppContainer
//!
//! [`AppContainerProfile`] provides a small helper around `CreateAppContainerProfile` /
//! `DeriveAppContainerSidFromAppContainerName` and returns an owned [`OwnedSid`]. The
//! implementation loads `userenv.dll` dynamically so binaries remain loadable on Windows builds
//! that do not ship the AppContainer exports.
//!
//! Defense in depth:
//! - When spawning AppContainer children, the spawners can optionally remove the broad
//!   `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the created token via
//!   `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (best-effort; older Windows builds may
//!   reject this attribute).
//!
//! ## Restricted tokens (fallback)
//!
//! [`RestrictedToken`] builds the "fallback" sandbox token used when AppContainer is unavailable:
//! `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` + a low integrity label (`S-1-16-4096`) applied
//! via `SetTokenInformation(TokenIntegrityLevel)`.
//!
//! ## Process mitigations
//!
//! The [`mitigations`] module provides a default mitigation policy bitmask suitable for headless
//! renderer processes, and [`spawn_sandboxed`] can apply that policy at process creation time via
//! `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`.
//!
//! ## Environment inheritance
//!
//! The process creation helpers in this crate (`spawn_sandboxed`, `restricted_token::spawn_with_token`,
//! and the higher-level sandbox spawners) inherit the current process environment by default.
//! `SpawnConfig.env` is an *override list* applied on top of `std::env::vars_os()`; this crate does
//! **not** implement environment sanitization.
//!
//! Callers that treat the child process as untrusted (for example a renderer process in a
//! multiprocess browser) should ensure secrets from the broker process environment are not leaked
//! into the child. The root `fastrender` crate’s Windows sandbox spawner (`src/sandbox/windows.rs`)
//! includes a sanitized environment allowlist for this reason.

#[cfg(windows)]
mod job;
#[cfg(windows)]
pub use job::Job;

#[cfg(windows)]
pub mod restricted_token;
#[cfg(windows)]
pub use restricted_token::RestrictedToken;

#[cfg(windows)]
mod renderer_sandbox;
#[cfg(windows)]
pub use renderer_sandbox::{RendererSandbox, SandboxedChild};

use thiserror::Error;

#[cfg(windows)]
pub use std::os::windows::io::RawHandle;

#[cfg(not(windows))]
pub type RawHandle = *mut core::ffi::c_void;

/// Result type alias for win-sandbox operations.
pub type Result<T> = std::result::Result<T, WinSandboxError>;

/// A Win32 error captured from `GetLastError()` along with a formatted message.
///
/// This is primarily a building block for [`WinSandboxError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastError {
  code: u32,
}

impl LastError {
  /// Captures the current thread's `GetLastError()` value.
  #[cfg(windows)]
  pub fn last() -> Self {
    // SAFETY: FFI call; does not have preconditions.
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    Self { code }
  }

  /// Creates a `LastError` from an explicit Win32 error code.
  pub fn from_code(code: u32) -> Self {
    Self { code }
  }

  /// Returns the underlying Win32 error code.
  pub fn code(self) -> u32 {
    self.code
  }

  /// Formats the error code using `FormatMessageW`.
  #[cfg(windows)]
  pub fn message(self) -> String {
    format_win32_error_message(self.code)
  }

  /// Formats the error code on non-Windows targets.
  #[cfg(not(windows))]
  pub fn message(self) -> String {
    format!("Win32 error {}", self.code)
  }
}

#[cfg(windows)]
fn format_win32_error_message(code: u32) -> String {
  use windows_sys::Win32::System::Diagnostics::Debug::{
    FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
  };

  // `FormatMessageW` returns a localized string and may append a trailing CRLF.
  // Use a fixed buffer to avoid heap allocation + `LocalFree` plumbing.
  let mut buf = [0u16; 512];

  // SAFETY: Win32 FFI call. We pass a valid writable buffer; `lpSource` and
  // `Arguments` are null because we use `FORMAT_MESSAGE_FROM_SYSTEM`.
  let len = unsafe {
    FormatMessageW(
      FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
      std::ptr::null(),
      code,
      0,
      buf.as_mut_ptr(),
      buf.len() as u32,
      std::ptr::null_mut(),
    )
  };

  if len == 0 {
    return format!("Win32 error {code}");
  }

  let msg = String::from_utf16_lossy(&buf[..len as usize]);
  msg.trim().to_string()
}

#[cfg(windows)]
fn format_hresult_message(hresult: i32) -> String {
  // For most Win32-facing APIs we use, errors are `HRESULT_FROM_WIN32(...)`,
  // which stores the original Win32 error code in the low 16 bits.
  const FACILITY_WIN32: u32 = 7;
  let hr = hresult as u32;
  let facility = (hr >> 16) & 0x1FFF;
  if facility == FACILITY_WIN32 {
    return format_win32_error_message(hr & 0xFFFF);
  }
  format!("HRESULT 0x{hr:08X}")
}

/// Errors produced by the Windows sandbox layer.
#[derive(Debug, Error, Clone)]
pub enum WinSandboxError {
  /// A Win32 API failed. Contains the function name and `GetLastError()` code.
  #[error("{func} failed with Win32 error {code}: {message}")]
  Win32 {
    func: &'static str,
    code: u32,
    message: String,
  },

  /// A Win32 API returned a failing HRESULT.
  #[error("{func} failed with HRESULT 0x{hresult:08X}: {message}")]
  HResult {
    func: &'static str,
    hresult: u32,
    message: String,
  },

  #[error("{func} returned a null pointer")]
  NullPointer { func: &'static str },

  /// A runtime check failed while asserting process mitigations are active.
  ///
  /// This is intended for tests and defense-in-depth verification.
  #[error("mitigation verification failed: {message}")]
  MitigationVerificationFailed { message: String },

  #[error("{arg} contains an interior NUL character")]
  InteriorNul { arg: &'static str },

  #[error("invalid environment variable `{name}`: `{value}`")]
  InvalidEnvVar { name: &'static str, value: String },

  #[error(transparent)]
  RendererSandboxMode(#[from] RendererSandboxModeError),
}

impl WinSandboxError {
  /// Builds a [`WinSandboxError::Win32`] by calling `GetLastError()` immediately.
  #[cfg(windows)]
  pub fn last(func: &'static str) -> Self {
    let err = LastError::last();
    Self::from_last_error(func, err)
  }

  /// Builds a [`WinSandboxError::Win32`] from an explicit error code.
  pub fn from_code(func: &'static str, code: u32) -> Self {
    Self::from_last_error(func, LastError::from_code(code))
  }

  fn from_last_error(func: &'static str, err: LastError) -> Self {
    Self::Win32 {
      func,
      code: err.code(),
      message: err.message(),
    }
  }

  /// Builds a [`WinSandboxError::HResult`] from an explicit HRESULT.
  #[cfg(windows)]
  pub fn from_hresult(func: &'static str, hresult: i32) -> Self {
    Self::HResult {
      func,
      hresult: hresult as u32,
      message: format_hresult_message(hresult),
    }
  }
}

/// RAII wrapper for an owned Win32 `HANDLE`.
///
/// This type closes the handle with `CloseHandle` on drop.
#[cfg(windows)]
#[derive(Debug)]
pub struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl OwnedHandle {
  /// Borrows the underlying `HANDLE`.
  pub fn as_raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
    self.0
  }

  /// Consumes the wrapper without closing the handle.
  pub fn into_raw(self) -> windows_sys::Win32::Foundation::HANDLE {
    let handle = self.0;
    std::mem::forget(self);
    handle
  }

  #[allow(dead_code)]
  pub(crate) fn from_raw(handle: windows_sys::Win32::Foundation::HANDLE) -> Self {
    Self(handle)
  }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawHandle for OwnedHandle {
  fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
    self.as_raw() as _
  }
}

#[cfg(windows)]
impl std::os::windows::io::IntoRawHandle for OwnedHandle {
  fn into_raw_handle(self) -> std::os::windows::io::RawHandle {
    self.into_raw() as _
  }
}

#[cfg(windows)]
impl Drop for OwnedHandle {
  fn drop(&mut self) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    let handle = self.0;
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
      return;
    }

    // SAFETY: `CloseHandle` is valid to call on kernel object handles returned
    // from Win32 APIs (job objects, tokens, ...). We defensively avoid closing
    // common sentinel values.
    unsafe {
      CloseHandle(handle);
    }
  }
}

/// RAII wrapper for an owned Win32 `PSID`.
///
/// # Allocation / free contract
///
/// Win32 isn't consistent about which deallocator is used for returned SIDs.
/// Some APIs require `FreeSid` (for example `AllocateAndInitializeSid` and
/// AppContainer SIDs returned from `CreateAppContainerProfile` /
/// `DeriveAppContainerSidFromAppContainerName`), while others require
/// `LocalFree` (for example `ConvertStringSidToSidW`).
#[cfg(windows)]
#[derive(Debug)]
pub struct OwnedSid {
  sid: windows_sys::Win32::Security::PSID,
  free: SidFreeMethod,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidFreeMethod {
  FreeSid,
  #[allow(dead_code)]
  LocalFree,
}

#[cfg(windows)]
impl OwnedSid {
  /// Borrows the underlying `PSID`.
  pub fn as_ptr(&self) -> windows_sys::Win32::Security::PSID {
    self.sid
  }

  /// Converts the SID to its string form (e.g. `"S-1-15-2-1"`).
  pub fn to_string_sid(&self) -> Result<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    if self.sid.is_null() {
      return Err(WinSandboxError::NullPointer {
        func: "OwnedSid::to_string_sid",
      });
    }

    let mut wide: *mut u16 = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(self.sid, &mut wide) };
    if ok == 0 {
      return Err(WinSandboxError::last("ConvertSidToStringSidW"));
    }
    if wide.is_null() {
      return Err(WinSandboxError::NullPointer {
        func: "ConvertSidToStringSidW",
      });
    }

    // SAFETY: Win32 contract: `ConvertSidToStringSidW` returns a NUL-terminated wide string
    // allocated with `LocalAlloc`; free it with `LocalFree`.
    unsafe {
      let mut len = 0usize;
      while *wide.add(len) != 0 {
        len += 1;
      }
      let slice = std::slice::from_raw_parts(wide, len);
      let s = String::from_utf16_lossy(slice);
      LocalFree(wide as _);
      Ok(s)
    }
  }

  /// Consumes the wrapper without freeing the SID.
  pub fn into_ptr(self) -> windows_sys::Win32::Security::PSID {
    let sid = self.sid;
    std::mem::forget(self);
    sid
  }

  pub(crate) fn from_free_sid(sid: windows_sys::Win32::Security::PSID) -> Self {
    Self {
      sid,
      free: SidFreeMethod::FreeSid,
    }
  }

  #[allow(dead_code)]
  pub(crate) fn from_local_free(sid: windows_sys::Win32::Security::PSID) -> Self {
    Self {
      sid,
      free: SidFreeMethod::LocalFree,
    }
  }
}

#[cfg(windows)]
impl Drop for OwnedSid {
  fn drop(&mut self) {
    if self.sid.is_null() {
      return;
    }

    // SAFETY: The deallocator is selected based on the Win32 API contract.
    unsafe {
      match self.free {
        SidFreeMethod::FreeSid => {
          windows_sys::Win32::Security::FreeSid(self.sid);
        }
        SidFreeMethod::LocalFree => {
          windows_sys::Win32::Foundation::LocalFree(self.sid as _);
        }
      }
    }
  }
}

#[cfg(windows)]
mod appcontainer;

#[cfg(windows)]
pub use appcontainer::{derive_appcontainer_sid, AppContainerProfile};

pub mod mitigations;

#[cfg(windows)]
mod spawn;

#[cfg(windows)]
pub use spawn::{spawn_sandboxed, ChildProcess, SpawnConfig};

pub mod support;
pub use support::{is_appcontainer_supported, is_nested_job_supported, SandboxSupport};

/// Runtime sandbox mode selection for the Windows renderer sandbox.
///
/// This type is intended for callers that want a "no silent downgrade" policy:
/// - If the host supports the required primitives, sandboxing is enabled.
/// - Otherwise, `new_default()` returns an error unless the caller has explicitly opted in to
///   running unsandboxed via `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`.
///
/// Note: This is distinct from the [`renderer_sandbox::RendererSandbox`] spawner helper, which is a
/// small convenience wrapper used by tests to spawn a no-capabilities AppContainer child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererSandboxMode {
  /// Full sandboxing is enabled (AppContainer + nested job objects).
  Enabled,
  /// The caller explicitly opted in to running without a sandbox.
  ///
  /// This is intended for developer convenience on unsupported Windows versions.
  Disabled,
}

#[derive(Debug, thiserror::Error, Clone, Copy)]
pub enum RendererSandboxModeError {
  #[error(
    "windows renderer sandbox is unavailable ({support}); set FASTR_ALLOW_UNSANDBOXED_RENDERER=1 to allow running without a sandbox"
  )]
  Unsupported { support: SandboxSupport },
}

impl RendererSandboxMode {
  pub fn new_default() -> std::result::Result<Self, RendererSandboxModeError> {
    let support = SandboxSupport::detect();
    if support == SandboxSupport::Full {
      return Ok(RendererSandboxMode::Enabled);
    }

    if allow_unsandboxed_renderer() {
      return Ok(RendererSandboxMode::Disabled);
    }

    Err(RendererSandboxModeError::Unsupported { support })
  }
}

fn allow_unsandboxed_renderer() -> bool {
  matches!(
    std::env::var_os("FASTR_ALLOW_UNSANDBOXED_RENDERER").as_deref(),
    Some(v) if v == std::ffi::OsStr::new("1")
  )
}

#[cfg(test)]
mod tests {
  #[cfg(windows)]
  use std::{env, ffi::OsString, time::Duration};

  #[cfg(windows)]
  use crate::{mitigations, spawn_sandboxed, SpawnConfig};

  // This is a helper entrypoint that runs inside the sandboxed child process.
  //
  // It's `#[ignore]` so the normal test run doesn't execute it in-process; the parent test spawns a
  // new copy of the test binary and runs this ignored test explicitly.
  #[cfg(windows)]
  #[test]
  #[ignore]
  fn verify_child_renderer_mitigations() {
    mitigations::verify_renderer_mitigations_current_process()
      .expect("child mitigation verification");
  }

  #[cfg(windows)]
  #[test]
  fn sandboxed_child_has_renderer_mitigations_enabled() {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _env_guard = ENV_LOCK.lock().unwrap();
    const DISABLE_MITIGATIONS_ENV: &str = "FASTR_DISABLE_WIN_MITIGATIONS";
    let prev_mitigations = env::var_os(DISABLE_MITIGATIONS_ENV);
    env::remove_var(DISABLE_MITIGATIONS_ENV);
    struct EnvRestore(Option<OsString>);
    impl Drop for EnvRestore {
      fn drop(&mut self) {
        match self.0.take() {
          Some(value) => env::set_var(DISABLE_MITIGATIONS_ENV, value),
          None => env::remove_var(DISABLE_MITIGATIONS_ENV),
        }
      }
    }
    let _restore = EnvRestore(prev_mitigations);

    let exe = env::current_exe().unwrap();

    let test_name = "tests::verify_child_renderer_mitigations";
    let args = vec![
      OsString::from("--ignored"),
      OsString::from("--exact"),
      OsString::from(test_name),
    ];

    let cfg = SpawnConfig {
      exe,
      args,
      env: Vec::new(),
      current_dir: None,
      inherit_handles: Vec::new(),
      appcontainer: None,
      job: None,
      mitigation_policy: Some(mitigations::renderer_mitigation_policy()),
      all_application_packages_hardened: true,
    };

    let child = spawn_sandboxed(&cfg).unwrap();

    let exit_code = child
      .wait_timeout(Duration::from_secs(30))
      .expect("wait for sandboxed child")
      .expect("sandboxed child exit code");

    assert_eq!(exit_code, 0, "sandboxed child exited with non-zero status");
  }
}

#[cfg(windows)]
mod child_process;
#[cfg(windows)]
pub mod renderer;
