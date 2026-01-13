use std::time::{Duration, Instant};

/// Default duration for transient warning toasts.
pub const WARNING_TOAST_DEFAULT_TTL: Duration = Duration::from_secs(4);

/// Maximum number of characters to show in the collapsed toast title before truncating.
///
/// Keep this relatively small so warning toasts don't grow excessively wide, while still allowing
/// enough context to disambiguate warnings.
pub const WARNING_TOAST_TITLE_MAX_CHARS: usize = 100;

const WARNING_TOAST_FALLBACK_TITLE: &str = "Unspecified warning";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarningToastText {
  /// Short, single-line title derived from the warning text.
  pub title: String,
  /// Full warning details intended for hover/expanded display.
  pub details: String,
}

/// Split raw warning text into a short title + full details.
///
/// - Title: first non-empty line (trimmed), truncated with ellipsis.
/// - Details: full warning text (trimmed).
pub fn split_warning_toast_text(text: &str) -> WarningToastText {
  let details = text.trim().to_string();
  let first_non_empty_line = details
    .lines()
    .map(|line| line.trim())
    .find(|line| !line.is_empty());

  let title_raw = first_non_empty_line.unwrap_or(WARNING_TOAST_FALLBACK_TITLE);
  let title = truncate_chars(title_raw, WARNING_TOAST_TITLE_MAX_CHARS);

  WarningToastText { title, details }
}

pub fn derive_warning_toast_title(text: &str) -> String {
  split_warning_toast_text(text).title
}

/// Default duration for transient chrome notifications.
pub const TOAST_DEFAULT_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
  Info,
  Warning,
  Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
  pub kind: ToastKind,
  pub text: String,
  pub expires_at: Instant,
}

/// Lifecycle state for a transient toast.
///
/// This is intentionally UI-framework agnostic (no egui types) so it can be unit tested without
/// compiling optional GUI dependencies.
#[derive(Debug, Default)]
pub struct ToastState {
  toast: Option<Toast>,
}

impl ToastState {
  pub fn show(&mut self, kind: ToastKind, text: impl Into<String>, now: Instant, ttl: Duration) {
    let text = text.into();
    if text.trim().is_empty() {
      return;
    }
    self.toast = Some(Toast {
      kind,
      text,
      expires_at: now + ttl,
    });
  }

  pub fn toast(&self) -> Option<&Toast> {
    self.toast.as_ref()
  }

  pub fn dismiss(&mut self) {
    self.toast = None;
  }

  pub fn expire(&mut self, now: Instant) {
    if self.toast.as_ref().is_some_and(|toast| now >= toast.expires_at) {
      self.toast = None;
    }
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.toast.as_ref().map(|toast| toast.expires_at)
  }
}

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
  ViewportClamp,
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

fn parse_tuple_u32_prefix(input: &str) -> Option<((u32, u32), &str)> {
  let input = input.strip_prefix('(')?;
  let close = input.find(')')?;
  let inside = input.get(..close)?;
  let rest = input.get(close + 1..)?;
  let mut parts = inside.split(',');
  let a = parts.next()?.trim().parse::<u32>().ok()?;
  let b = parts.next()?.trim().parse::<u32>().ok()?;
  // Ensure there are exactly two elements.
  if parts.next().is_some() {
    return None;
  }
  Some(((a, b), rest))
}

fn parse_f32_prefix(input: &str) -> Option<(f32, &str)> {
  let input = input.trim_start();
  if input.is_empty() {
    return None;
  }
  let mut end = 0usize;
  for (idx, ch) in input.char_indices() {
    let ok = ch.is_ascii_digit() || matches!(ch, '.' | '+' | '-');
    if !ok {
      break;
    }
    end = idx + ch.len_utf8();
  }
  if end == 0 {
    return None;
  }
  let num = input.get(..end)?.parse::<f32>().ok()?;
  if !num.is_finite() {
    return None;
  }
  Some((num, input.get(end..)?))
}

fn format_float_compact(value: f32, decimals: usize) -> String {
  let s = format!("{:.*}", decimals, value);
  s.trim_end_matches('0')
    .trim_end_matches('.')
    .to_string()
}

fn viewport_clamped_summary(warning: &str) -> Option<String> {
  // Expected format from `BrowserLimits::warning_text`:
  // "Viewport clamped: requested viewport_css=(W, H) dpr=... → viewport_css=(W2, H2) dpr=... (..."
  let mut rest = warning.strip_prefix("Viewport clamped:")?.trim_start();
  rest = rest.strip_prefix("requested viewport_css=")?;
  let (requested_viewport, after_requested_viewport) = parse_tuple_u32_prefix(rest)?;

  let mut rest = after_requested_viewport.trim_start();
  rest = rest.strip_prefix("dpr=")?;
  let (requested_dpr, after_requested_dpr) = parse_f32_prefix(rest)?;

  let mut rest = after_requested_dpr.trim_start();
  rest = rest.strip_prefix("→")?.trim_start();
  rest = rest.strip_prefix("viewport_css=")?;
  let (clamped_viewport, after_clamped_viewport) = parse_tuple_u32_prefix(rest)?;

  let mut rest = after_clamped_viewport.trim_start();
  rest = rest.strip_prefix("dpr=")?;
  let (clamped_dpr, _) = parse_f32_prefix(rest)?;

  Some(format!(
    "{}×{} @ {} → {}×{} @ {}",
    requested_viewport.0,
    requested_viewport.1,
    format_float_compact(requested_dpr, 2),
    clamped_viewport.0,
    clamped_viewport.1,
    format_float_compact(clamped_dpr, 2),
  ))
}

/// Classify a warning string into a reusable presentation model for the warning toast UI.
///
/// Deterministic and UI-framework-agnostic (no egui types).
pub fn classify_warning_toast(warning: Option<&str>) -> Option<WarningToastPresentation> {
  let warning = warning?.trim();
  if warning.is_empty() {
    return None;
  }

  // Known warning patterns: special-case icons/summaries when possible.
  if warning.starts_with("Viewport clamped:") || warning == "Viewport clamped" {
    return Some(WarningToastPresentation {
      title: "Viewport clamped".to_string(),
      summary: viewport_clamped_summary(warning)
        .or_else(|| Some("Viewport was reduced to stay within safety limits.".to_string())),
      icon: WarningToastIcon::ViewportClamp,
    });
  }

  let summary = derive_warning_toast_title(warning);
  Some(WarningToastPresentation {
    title: "Warning".to_string(),
    summary: Some(summary),
    icon: WarningToastIcon::Info,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn toast_state_shows_and_expires() {
    let mut state = ToastState::default();
    let ttl = Duration::from_secs(2);
    let t0 = Instant::now();

    state.show(ToastKind::Error, "Hello", t0, ttl);
    let toast = state.toast().expect("toast should exist");
    assert_eq!(toast.kind, ToastKind::Error);
    assert_eq!(toast.text, "Hello");
    assert_eq!(state.next_deadline(), Some(t0 + ttl));

    state.expire(t0 + Duration::from_secs(1));
    assert!(state.toast().is_some());

    state.expire(t0 + ttl);
    assert!(state.toast().is_none());
  }

  #[test]
  fn toast_state_dismiss_clears() {
    let mut state = ToastState::default();
    let t0 = Instant::now();
    state.show(ToastKind::Info, "Hello", t0, Duration::from_secs(1));
    assert!(state.toast().is_some());
    state.dismiss();
    assert!(state.toast().is_none());
  }

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
    let input = "Viewport clamped: requested viewport_css=(800, 600) dpr=2.000 → viewport_css=(800, 600) dpr=1.000 (pixmap_px=800x600; limits: max_dim_px=8192 max_pixels=50000000)";
    let presentation = classify_warning_toast(Some(input)).unwrap();
    assert_eq!(presentation.title, "Viewport clamped");
    assert_eq!(
      presentation.summary.as_deref(),
      Some("800×600 @ 2 → 800×600 @ 1")
    );
    assert_eq!(presentation.icon, WarningToastIcon::ViewportClamp);
  }

  #[test]
  fn classify_warning_toast_generic() {
    let presentation = classify_warning_toast(Some("Something went wrong")).unwrap();
    assert_eq!(presentation.title, "Warning");
    assert_eq!(presentation.summary.as_deref(), Some("Something went wrong"));
    assert_eq!(presentation.icon, WarningToastIcon::Info);
  }

  #[test]
  fn split_warning_toast_title_uses_first_non_empty_line() {
    let out = split_warning_toast_text("\n\n  First line  \nSecond line\n");
    assert_eq!(out.title, "First line");
    assert_eq!(out.details, "First line  \nSecond line");
  }

  #[test]
  fn split_warning_toast_title_truncates_long_lines() {
    let long = "a".repeat(WARNING_TOAST_TITLE_MAX_CHARS + 10);
    let out = split_warning_toast_text(&long);
    assert!(
      out.title.ends_with('…'),
      "expected title to end with ellipsis, got {:?}",
      out.title
    );
    assert_eq!(out.title.chars().count(), WARNING_TOAST_TITLE_MAX_CHARS + 1);
    assert_eq!(out.details, long);
  }

  #[test]
  fn split_warning_toast_empty_string_is_robust() {
    let out = split_warning_toast_text("   \n\t  ");
    assert_eq!(out.title, WARNING_TOAST_FALLBACK_TITLE);
    assert_eq!(out.details, "");
  }
}
