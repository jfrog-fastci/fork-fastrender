//! Helpers for formatting startup notifications shown in the windowed browser chrome.
//!
//! These are kept UI-framework agnostic (no egui types) so they can be unit tested without the
//! optional GUI feature flags.

use crate::ui::untrusted::sanitize_untrusted_text;
use std::path::Path;

/// Maximum UTF-8 bytes kept for the profile file path shown in a startup toast.
pub const STARTUP_PROFILE_TOAST_PATH_MAX_BYTES: usize = 256;

/// Maximum UTF-8 bytes kept for the error string shown in a startup toast.
///
/// Keep this relatively small because error strings can contain long backtraces or nested causes.
pub const STARTUP_PROFILE_TOAST_ERROR_MAX_BYTES: usize = 512;

/// Format a chrome toast shown when a persisted profile store (bookmarks/history) failed to load.
///
/// Returns `None` when the sanitized output would be empty (to avoid showing blank notifications).
pub fn format_profile_store_load_failure_toast(
  store_label: &str,
  path: &Path,
  error: &str,
) -> Option<String> {
  let store_label = store_label.trim();
  if store_label.is_empty() {
    return None;
  }

  let safe_error = sanitize_error_for_toast(error);
  if safe_error.trim().is_empty() {
    return None;
  }

  let raw_path = path.to_string_lossy();
  let safe_path =
    sanitize_untrusted_text_with_ellipsis(&raw_path, STARTUP_PROFILE_TOAST_PATH_MAX_BYTES);
  if safe_path.trim().is_empty() {
    return None;
  }

  Some(format!(
    "Using empty {store_label}: failed to load on startup.\nPath: {safe_path}\nError: {safe_error}"
  ))
}

fn sanitize_error_for_toast(error: &str) -> String {
  let trimmed = error.trim();
  if trimmed.is_empty() {
    return String::new();
  }

  // Prefer the first non-empty line; error chains can include verbose context on later lines.
  let first_line = trimmed
    .lines()
    .map(|line| line.trim())
    .find(|line| !line.is_empty())
    .unwrap_or(trimmed);

  sanitize_untrusted_text_with_ellipsis(first_line, STARTUP_PROFILE_TOAST_ERROR_MAX_BYTES)
}

fn sanitize_untrusted_text_with_ellipsis(text: &str, max_bytes: usize) -> String {
  if max_bytes == 0 {
    return String::new();
  }

  const ELLIPSIS: char = '…';
  let ellipsis_bytes = ELLIPSIS.len_utf8();

  // Probe with +1 so we can detect truncation without allocating unboundedly.
  let probe_limit = max_bytes.saturating_add(1);
  let probe = sanitize_untrusted_text(text, probe_limit);
  if probe.len() <= max_bytes {
    return probe;
  }

  if max_bytes <= ellipsis_bytes {
    return ELLIPSIS.to_string();
  }

  let mut out = sanitize_untrusted_text(text, max_bytes - ellipsis_bytes);
  out.push(ELLIPSIS);
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn long_errors_are_truncated() {
    let err = "x".repeat(STARTUP_PROFILE_TOAST_ERROR_MAX_BYTES.saturating_mul(4));
    let path = Path::new("/tmp/fastrender_bookmarks.json");
    let msg = format_profile_store_load_failure_toast("bookmarks", path, &err)
      .expect("expected toast text");
    let error_line = msg
      .lines()
      .find(|line| line.trim_start().starts_with("Error:"))
      .expect("expected an Error: line");
    let error_value = error_line
      .split_once("Error:")
      .map(|(_, rest)| rest.trim_start())
      .unwrap_or("");
    assert_eq!(error_value.len(), STARTUP_PROFILE_TOAST_ERROR_MAX_BYTES);
    assert!(
      error_value.ends_with('…'),
      "expected truncated error to end with ellipsis"
    );
  }

  #[test]
  fn empty_errors_do_not_produce_a_toast() {
    let path = Path::new("/tmp/fastrender_history.json");
    assert!(format_profile_store_load_failure_toast("history", path, "").is_none());
    assert!(format_profile_store_load_failure_toast("history", path, "   \n\t").is_none());
    assert!(format_profile_store_load_failure_toast("history", path, "\u{0000}\u{007f}").is_none());
  }
}
