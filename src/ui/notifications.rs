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

/// Icon choice for a warning toast.
///
/// This is intentionally separate from the egui-only [`crate::ui::BrowserIcon`] type so warning
/// classification can be unit-tested without compiling the optional GUI stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningToastIcon {
  Info,
  WarningInsecure,
}

/// User-facing presentation for a warning string shown in a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarningToastPresentation {
  /// Short title shown in the toast header (e.g. "Viewport clamped").
  pub title: String,
  /// Optional one-line summary shown below the header.
  pub summary: Option<String>,
  pub icon: WarningToastIcon,
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
  if max_chars == 0 {
    return String::new();
  }
  let mut chars = value.chars();
  let mut buf = String::new();
  for _ in 0..max_chars {
    let Some(ch) = chars.next() else {
      return value.to_string();
    };
    buf.push(ch);
  }

  if chars.next().is_none() {
    value.to_string()
  } else {
    buf.push('…');
    buf
  }
}

/// Classify a warning string into a reusable presentation model for the warning toast UI.
///
/// Deterministic and UI-framework-agnostic (no egui types).
pub fn classify_warning_toast(warning: Option<&str>) -> Option<WarningToastPresentation> {
  let warning = warning?.trim();
  if warning.is_empty() {
    return None;
  }

  // Known warning prefixes: special-case stable titles/icons so the toast header stays consistent
  // even when the warning details include dynamic values.
  if warning.starts_with("Viewport clamped:") || warning == "Viewport clamped" {
    return Some(WarningToastPresentation {
      title: "Viewport clamped".to_string(),
      summary: Some("Viewport was reduced to stay within safety limits.".to_string()),
      icon: WarningToastIcon::WarningInsecure,
    });
  }

  let first_line = warning.lines().next().unwrap_or(warning).trim();
  let summary = (!first_line.is_empty()).then(|| truncate_chars(first_line, 160));

  Some(WarningToastPresentation {
    title: "Warning".to_string(),
    summary,
    icon: WarningToastIcon::Info,
  })
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

  #[test]
  fn classify_warning_toast_none_or_empty_is_none() {
    assert_eq!(classify_warning_toast(None), None);
    assert_eq!(classify_warning_toast(Some("")), None);
    assert_eq!(classify_warning_toast(Some("   ")), None);
  }

  #[test]
  fn classify_warning_toast_viewport_clamped() {
    let presentation =
      classify_warning_toast(Some("Viewport clamped: requested viewport_css=(1,1) dpr=2")).unwrap();
    assert_eq!(presentation.title, "Viewport clamped");
    assert_eq!(
      presentation.summary.as_deref(),
      Some("Viewport was reduced to stay within safety limits.")
    );
    assert_eq!(presentation.icon, WarningToastIcon::WarningInsecure);
  }

  #[test]
  fn classify_warning_toast_generic() {
    let presentation = classify_warning_toast(Some("Something went wrong")).unwrap();
    assert_eq!(presentation.title, "Warning");
    assert_eq!(presentation.summary.as_deref(), Some("Something went wrong"));
    assert_eq!(presentation.icon, WarningToastIcon::Info);
  }
}
