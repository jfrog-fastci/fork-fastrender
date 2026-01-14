use crate::ui::ToastKind;

/// A chrome toast queued during startup before any browser window exists.
///
/// The windowed browser (`src/bin/browser.rs`) can record these events during initialization and
/// then display them in the first window that opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupChromeToast {
  pub kind: ToastKind,
  pub text: String,
}

impl StartupChromeToast {
  pub fn new(kind: ToastKind, text: impl Into<String>) -> Self {
    Self {
      kind,
      text: text.into(),
    }
  }
}

/// Stores a single startup chrome toast that should be shown at most once per process.
///
/// This is intentionally UI-framework agnostic so it can be unit tested without compiling the
/// windowing stack.
#[derive(Debug, Default)]
pub struct StartupChromeToastOnce {
  pending: Option<StartupChromeToast>,
}

impl StartupChromeToastOnce {
  pub fn is_pending(&self) -> bool {
    self.pending.is_some()
  }

  /// Records a toast if no other startup toast is currently pending.
  pub fn push_if_empty(&mut self, toast: StartupChromeToast) {
    if self.pending.is_none() {
      self.pending = Some(toast);
    }
  }

  /// Takes the pending toast if this is the first window.
  pub fn take_for_window(
    &mut self,
    window_index: usize,
    first_window_index: usize,
  ) -> Option<StartupChromeToast> {
    if window_index == first_window_index {
      self.pending.take()
    } else {
      None
    }
  }
}

/// User-facing toast shown when profile autosave (bookmarks/history) could not be started.
pub fn profile_autosave_start_failed_toast(err: &str) -> StartupChromeToast {
  // Keep the first segment before ':' as a short title (see `toast_title_from_text` in
  // `src/bin/browser.rs`), but still include the error details for debugging.
  let err = err.trim();
  let details = if err.is_empty() {
    "Bookmarks and history changes may not be saved.".to_string()
  } else {
    format!("Bookmarks and history changes may not be saved. ({err})")
  };
  StartupChromeToast::new(
    ToastKind::Warning,
    format!("Profile autosave disabled: {details}"),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn profile_autosave_start_failed_toast_has_warning_kind() {
    let toast = profile_autosave_start_failed_toast("thread spawn failed");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(toast.text.contains("Profile autosave disabled"));
  }

  #[test]
  fn startup_toast_shows_only_once_in_first_window() {
    let mut once = StartupChromeToastOnce::default();
    once.push_if_empty(StartupChromeToast::new(ToastKind::Warning, "A"));
    // Not the first window.
    assert_eq!(once.take_for_window(1, 0), None);
    assert!(once.is_pending());
    // First window consumes it.
    let toast = once.take_for_window(0, 0).unwrap();
    assert_eq!(toast.text, "A");
    assert!(!once.is_pending());
    // Further windows do not see it.
    assert_eq!(once.take_for_window(0, 0), None);
  }
}

