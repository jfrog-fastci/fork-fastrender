//! Windows renderer sandbox configuration helpers.
//!
//! This module currently focuses on *configuration/selection* and debug escape hatches.
//! The concrete sandbox implementation is expected to live alongside these helpers.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// Debug escape hatch: disable the Windows renderer sandbox.
///
/// This is intentionally Windows-only (the variable is ignored on other platforms).
const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

/// Legacy/alternative spelling for disabling the Windows renderer sandbox.
///
/// Accepted values:
/// - `off`, `0`, `false`, `no` (case-insensitive) => disable sandboxing
/// - any other non-empty value => leave sandboxing enabled (default)
const ENV_WINDOWS_RENDERER_SANDBOX: &str = "FASTR_WINDOWS_RENDERER_SANDBOX";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsRendererSandboxLevel {
  /// Primary mode: AppContainer with zero capabilities.
  AppContainer,
  /// Fallback mode: restricted token + low integrity.
  RestrictedToken,
}

/// Returns the sandbox level the Windows renderer should attempt to use.
///
/// - `None` means "spawn unsandboxed" (debug escape hatch).
/// - `Some(..)` indicates the preferred sandbox mode; callers are expected to apply
///   fallbacks if a stronger sandbox is unavailable.
pub(crate) fn requested_renderer_sandbox_level() -> Option<WindowsRendererSandboxLevel> {
  if renderer_sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return None;
  }

  // Preferred sandbox. Callers are expected to fall back to restricted-token mode if
  // AppContainer is unavailable (e.g. older Windows versions or policy restrictions).
  Some(WindowsRendererSandboxLevel::AppContainer)
}

fn renderer_sandbox_disabled_via_env() -> bool {
  if env_var_truthy(std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX).as_deref()) {
    return true;
  }

  let Some(raw) = std::env::var_os(ENV_WINDOWS_RENDERER_SANDBOX) else {
    return false;
  };
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

fn env_var_truthy(raw: Option<&OsStr>) -> bool {
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

fn log_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: Windows renderer sandbox is DISABLED (debug escape hatch). \
Set {ENV_DISABLE_RENDERER_SANDBOX}=0/1 or {ENV_WINDOWS_RENDERER_SANDBOX}=off to control this."
    );
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Mutex;

  static ENV_LOCK: Mutex<()> = Mutex::new(());

  #[test]
  fn sandbox_disabled_env_forces_none() {
    let _guard = ENV_LOCK.lock().unwrap();

    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_windows = std::env::var_os(ENV_WINDOWS_RENDERER_SANDBOX);

    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");
    std::env::remove_var(ENV_WINDOWS_RENDERER_SANDBOX);
    assert_eq!(requested_renderer_sandbox_level(), None);

    std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::set_var(ENV_WINDOWS_RENDERER_SANDBOX, "off");
    assert_eq!(requested_renderer_sandbox_level(), None);

    match prev_disable {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
    match prev_windows {
      Some(value) => std::env::set_var(ENV_WINDOWS_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_WINDOWS_RENDERER_SANDBOX),
    }
  }
}

