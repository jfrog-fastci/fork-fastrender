use std::time::{Duration, Instant};

/// Default duration for transient warning toasts.
pub const WARNING_TOAST_DEFAULT_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
pub struct WarningToast {
  pub text: String,
  pub expires_at: Instant,
}

/// Lifecycle state for a per-tab warning toast.
///
/// This is intentionally UI-framework agnostic (no egui types) so it can be unit tested without
/// compiling optional GUI dependencies.
#[derive(Debug, Default)]
pub struct WarningToastState {
  last_warning_was_some: bool,
  toast: Option<WarningToast>,
}

impl WarningToastState {
  /// Updates the toast state from the current `warning` string.
  ///
  /// Returns `true` when a new toast is shown for this update.
  pub fn update(&mut self, warning: Option<&str>, now: Instant, ttl: Duration) -> bool {
    let warning_is_some = warning.is_some_and(|text| !text.trim().is_empty());
    let mut shown = false;

    // Display rule: only show when warning transitions from None → Some.
    if !self.last_warning_was_some && warning_is_some {
      if let Some(text) = warning {
        self.toast = Some(WarningToast {
          text: text.to_string(),
          expires_at: now + ttl,
        });
        shown = true;
      }
    }

    self.last_warning_was_some = warning_is_some;
    self.expire(now);
    shown
  }

  pub fn toast(&self) -> Option<&WarningToast> {
    self.toast.as_ref()
  }

  pub fn dismiss(&mut self) {
    self.toast = None;
  }

  pub fn expire(&mut self, now: Instant) {
    if self
      .toast
      .as_ref()
      .is_some_and(|toast| now >= toast.expires_at)
    {
      self.toast = None;
    }
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.toast.as_ref().map(|toast| toast.expires_at)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn warning_toast_none_to_some_shows_then_expires() {
    let mut state = WarningToastState::default();
    let ttl = Duration::from_secs(3);
    let t0 = Instant::now();

    assert!(!state.update(None, t0, ttl));
    assert!(state.toast().is_none());

    assert!(state.update(Some("Viewport clamped"), t0, ttl));
    let toast = state.toast().expect("toast should exist");
    assert_eq!(toast.text, "Viewport clamped");
    assert_eq!(state.next_deadline(), Some(t0 + ttl));

    // While the warning remains present (even if the string changes), a new toast should not be
    // shown.
    assert!(!state.update(Some("Viewport clamped (updated)"), t0 + Duration::from_secs(1), ttl));
    assert!(
      state.toast().is_some(),
      "toast should still be visible before expiry"
    );

    // At/after expiry, the toast should be cleared.
    assert!(!state.update(Some("Viewport clamped (updated)"), t0 + ttl, ttl));
    assert!(state.toast().is_none());

    // While warning stays Some, it should not reappear.
    assert!(!state.update(Some("Viewport clamped (updated)"), t0 + ttl + Duration::from_secs(1), ttl));
    assert!(state.toast().is_none());

    // When warning clears then becomes Some again, show again.
    assert!(!state.update(None, t0 + ttl + Duration::from_secs(2), ttl));
    assert!(state.update(Some("Viewport clamped again"), t0 + ttl + Duration::from_secs(3), ttl));
    assert!(state.toast().is_some());
  }

  #[test]
  fn warning_toast_dismiss_blocks_until_warning_clears() {
    let mut state = WarningToastState::default();
    let ttl = Duration::from_secs(3);
    let t0 = Instant::now();

    assert!(state.update(Some("Viewport clamped"), t0, ttl));
    assert!(state.toast().is_some());

    state.dismiss();
    assert!(state.toast().is_none());

    // Warning still present => no new toast.
    assert!(!state.update(Some("Viewport clamped"), t0 + Duration::from_secs(1), ttl));
    assert!(state.toast().is_none());

    // Warning clears => next present shows again.
    assert!(!state.update(None, t0 + Duration::from_secs(2), ttl));
    assert!(state.update(Some("Viewport clamped"), t0 + Duration::from_secs(3), ttl));
    assert!(state.toast().is_some());
  }
}

