use std::path::PathBuf;

/// Environment variable that forces the windowed browser UI to use in-app dialogs instead of
/// native OS dialogs (useful for CI/headless environments).
pub const ENV_BROWSER_FORCE_IN_APP_DIALOGS: &str = "FASTR_BROWSER_FORCE_IN_APP_DIALOGS";

fn parse_env_bool(raw: Option<&str>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  match trimmed.to_ascii_lowercase().as_str() {
    "0" | "false" | "no" | "off" => false,
    _ => true,
  }
}

/// Parse `FASTR_BROWSER_FORCE_IN_APP_DIALOGS` (default: false).
pub fn force_in_app_dialogs_from_env(env_value: Option<&str>) -> bool {
  parse_env_bool(env_value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSaveDialogOutcome {
  /// The user chose a destination path.
  Chosen(PathBuf),
  /// The user cancelled the dialog.
  Cancelled,
  /// The native dialog could not be opened (or native dialogs are disabled) and the caller should
  /// fall back to an in-app dialog.
  FallbackToInApp,
}

/// Safely run a native save dialog callback.
///
/// Some native dialog backends can panic (e.g. missing portal backend on Linux). Treat those
/// failures as a request to fall back to an in-app UI (instead of crashing).
pub fn native_save_dialog_outcome(
  force_in_app_dialogs: bool,
  open_dialog: impl FnOnce() -> Option<PathBuf>,
) -> NativeSaveDialogOutcome {
  if force_in_app_dialogs {
    return NativeSaveDialogOutcome::FallbackToInApp;
  }

  match std::panic::catch_unwind(std::panic::AssertUnwindSafe(open_dialog)) {
    Ok(Some(path)) => NativeSaveDialogOutcome::Chosen(path),
    Ok(None) => NativeSaveDialogOutcome::Cancelled,
    Err(_) => NativeSaveDialogOutcome::FallbackToInApp,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn force_in_app_dialogs_env_override_parses_truthy_values() {
    assert!(!force_in_app_dialogs_from_env(None));
    assert!(!force_in_app_dialogs_from_env(Some("0")));
    assert!(!force_in_app_dialogs_from_env(Some("false")));
    assert!(force_in_app_dialogs_from_env(Some("1")));
    assert!(force_in_app_dialogs_from_env(Some("true")));
    assert!(force_in_app_dialogs_from_env(Some("yes")));
  }

  #[test]
  fn native_save_dialog_outcome_skips_dialog_when_forced() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let called = Arc::new(AtomicBool::new(false));
    let called2 = Arc::clone(&called);
    let outcome = native_save_dialog_outcome(true, || {
      called2.store(true, Ordering::SeqCst);
      Some(std::path::PathBuf::from("ignored"))
    });
    assert_eq!(outcome, NativeSaveDialogOutcome::FallbackToInApp);
    assert!(
      !called.load(Ordering::SeqCst),
      "expected dialog closure to be skipped"
    );
  }

  #[test]
  fn native_save_dialog_outcome_catches_panics() {
    let outcome = native_save_dialog_outcome(false, || panic!("boom"));
    assert_eq!(outcome, NativeSaveDialogOutcome::FallbackToInApp);
  }

  #[test]
  fn native_save_dialog_outcome_distinguishes_cancel() {
    let outcome = native_save_dialog_outcome(false, || None);
    assert_eq!(outcome, NativeSaveDialogOutcome::Cancelled);
  }
}
