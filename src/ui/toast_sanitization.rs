use std::collections::VecDeque;
use std::path::Path;

use super::profile_autosave::ProfileAutosaveError;

const PROFILE_AUTOSAVE_ERROR_SUMMARY_MAX_BYTES: usize = 160;
const PROFILE_AUTOSAVE_SAVE_ERROR_MAX_BYTES: usize = 200;
const PROFILE_AUTOSAVE_SAVE_PATH_MAX_BYTES: usize = 240;

fn truncate_utf8_string_in_place(value: &mut String, max_bytes: usize) {
  if max_bytes == 0 {
    value.clear();
    value.shrink_to_fit();
    return;
  }
  if value.len() > max_bytes {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
      end -= 1;
    }
    value.truncate(end);
  }

  // Drop attacker-controlled excess capacity eagerly so the browser UI doesn't retain a large
  // allocation even when the payload was already within our byte limit (hostile senders can reserve
  // huge buffers with small logical lengths).
  if value.capacity() > max_bytes {
    let mut out = String::with_capacity(value.len());
    out.push_str(value);
    *value = out;
  }
}

/// Sanitize/truncate an internal error message for display in a chrome toast.
///
/// This is intentionally small + single-line:
/// - Collapses whitespace/control chars into single spaces.
/// - Clamps to `max_bytes` bytes without splitting UTF-8 codepoints.
/// - Adds an ellipsis when truncation occurs (and there is room).
pub fn sanitize_toast_detail_single_line(raw: &str, max_bytes: usize) -> Option<String> {
  if max_bytes == 0 {
    return None;
  }
  // Avoid O(n) scans over attacker-controlled strings (e.g. corrupted session files, verbose nested
  // errors). We only need enough input to populate our small output buffer, so cap the amount of
  // text we inspect.
  const ABSURD_INPUT_BYTES_MULTIPLIER: usize = 64;
  let absurd_limit = max_bytes.saturating_mul(ABSURD_INPUT_BYTES_MULTIPLIER);
  let mut raw = raw;
  let mut truncated_from_input = false;
  if raw.len() > absurd_limit {
    let mut end = absurd_limit.min(raw.len());
    while end > 0 && !raw.is_char_boundary(end) {
      end -= 1;
    }
    raw = raw.get(..end).unwrap_or("");
    truncated_from_input = true;
  }

  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }

  // Avoid allocating based on `raw.len()` (could be very large). Pre-allocate up to the limit.
  let mut out = String::with_capacity(max_bytes.min(128));
  let mut pending_space = false;
  let mut truncated = truncated_from_input;

  for ch in raw.chars() {
    if ch.is_whitespace() || ch.is_control() {
      pending_space = true;
      continue;
    }

    if pending_space && !out.is_empty() {
      if out.len() + 1 > max_bytes {
        truncated = true;
        break;
      }
      out.push(' ');
    }
    pending_space = false;

    let ch_len = ch.len_utf8();
    if out.len() + ch_len > max_bytes {
      truncated = true;
      break;
    }
    out.push(ch);
  }

  if truncated && !out.is_empty() {
    const ELLIPSIS: char = '…';
    let ellipsis_len = ELLIPSIS.len_utf8();
    if out.len() + ellipsis_len > max_bytes {
      truncate_utf8_string_in_place(&mut out, max_bytes.saturating_sub(ellipsis_len));
    }
    if !out.is_empty() {
      out.push(ELLIPSIS);
    }
  }

  if out.is_empty() {
    None
  } else {
    Some(out)
  }
}

pub fn sanitize_path_for_toast(path: &Path, max_bytes: usize) -> String {
  if max_bytes == 0 {
    return String::new();
  }

  // When truncating, we keep the *tail* of the string so filenames remain visible.
  // Reserve space for a single leading ellipsis.
  const ELLIPSIS: char = '…';
  let ellipsis_bytes = ELLIPSIS.len_utf8();
  if max_bytes <= ellipsis_bytes {
    return ELLIPSIS.to_string();
  }
  let keep_bytes = max_bytes - ellipsis_bytes;

  let raw = path.to_string_lossy();
  let mut raw = raw.as_ref();

  // Avoid scanning arbitrarily huge strings in full: keep only the tail portion for sanitization.
  // This keeps worst-case work bounded when the persisted path is corrupt or attacker-controlled
  // (e.g. from a malformed session file).
  const ABSURD_PATH_BYTES_MULTIPLIER: usize = 64;
  let absurd_limit = max_bytes.saturating_mul(ABSURD_PATH_BYTES_MULTIPLIER);

  // We only need the full string when it fits within the byte limit. Otherwise, keep just the tail
  // (bounded) to avoid allocating based on attacker-controlled path lengths.
  let mut full = String::with_capacity(max_bytes.min(128));
  let mut overflowed = false;
  if raw.len() > absurd_limit {
    let start = raw.len().saturating_sub(absurd_limit);
    let mut start = start.min(raw.len());
    while start < raw.len() && !raw.is_char_boundary(start) {
      start += 1;
    }
    raw = raw.get(start..).unwrap_or(raw);
    // The displayed output will necessarily be truncated vs the original string.
    overflowed = true;
  }

  let mut tail: VecDeque<char> = VecDeque::new();
  let mut tail_bytes: usize = 0;

  let mut pending_space = false;
  let mut emitted_any = false;

  let mut push_out = |ch: char| {
    let ch_len = ch.len_utf8();

    // Track whether the output exceeded our display limit (before we add the ellipsis).
    if !overflowed {
      if full.len() + ch_len > max_bytes {
        overflowed = true;
      } else {
        full.push(ch);
      }
    }

    tail.push_back(ch);
    tail_bytes += ch_len;
    while tail_bytes > keep_bytes {
      if let Some(front) = tail.pop_front() {
        tail_bytes = tail_bytes.saturating_sub(front.len_utf8());
      } else {
        tail_bytes = 0;
        break;
      }
    }
  };

  for ch in raw.chars() {
    if ch.is_whitespace() || ch.is_control() {
      pending_space = true;
      continue;
    }

    if pending_space && emitted_any {
      push_out(' ');
    }
    pending_space = false;
    emitted_any = true;
    push_out(ch);
  }

  if !overflowed {
    return full;
  }

  let mut out = String::with_capacity(max_bytes.min(128));
  out.push(ELLIPSIS);
  for ch in tail {
    out.push(ch);
  }
  out
}

/// User-facing toast for autosave *startup* failures.
pub fn format_profile_autosave_spawn_failure_toast(err: &str) -> String {
  let base = "Failed to start profile autosave\nBookmarks/history changes may not be saved.";
  let Some(summary) = sanitize_toast_detail_single_line(err, PROFILE_AUTOSAVE_ERROR_SUMMARY_MAX_BYTES)
  else {
    return base.to_string();
  };
  format!("Failed to start profile autosave: {summary}\nBookmarks/history changes may not be saved.")
}

/// User-facing toast for autosave *save* failures (bookmarks/history write errors).
pub fn format_profile_autosave_save_error_toast(err: &ProfileAutosaveError) -> String {
  let (store_label, path, message) = match err {
    ProfileAutosaveError::Bookmarks { path, message } => ("bookmarks", path, message),
    ProfileAutosaveError::History { path, message } => ("history", path, message),
  };

  let mut text = format!("Failed to save {store_label}");

  let safe_path = sanitize_path_for_toast(Path::new(path), PROFILE_AUTOSAVE_SAVE_PATH_MAX_BYTES);
  if !safe_path.trim().is_empty() {
    text.push_str("\nPath: ");
    text.push_str(&safe_path);
  }

  let safe_error = sanitize_toast_detail_single_line(message, PROFILE_AUTOSAVE_SAVE_ERROR_MAX_BYTES);
  if let Some(safe_error) = safe_error {
    if !safe_error.trim().is_empty() {
      text.push_str("\nError: ");
      text.push_str(&safe_error);
    }
  }

  text
}

#[cfg(test)]
mod tests {
  use super::{
    format_profile_autosave_save_error_toast, format_profile_autosave_spawn_failure_toast,
    sanitize_toast_detail_single_line, PROFILE_AUTOSAVE_ERROR_SUMMARY_MAX_BYTES,
    PROFILE_AUTOSAVE_SAVE_ERROR_MAX_BYTES, PROFILE_AUTOSAVE_SAVE_PATH_MAX_BYTES,
  };
  use crate::ui::profile_autosave::ProfileAutosaveError;

  #[test]
  fn toast_detail_single_line_sanitizes_whitespace_and_controls() {
    assert_eq!(
      sanitize_toast_detail_single_line("  failed:\n\tbad\u{0000} thing  ", 200).as_deref(),
      Some("failed: bad thing")
    );
    assert_eq!(
      sanitize_toast_detail_single_line(" \t\r\n ", 200).as_deref(),
      None
    );
  }

  #[test]
  fn toast_detail_single_line_truncates_and_appends_ellipsis() {
    let long = "a".repeat(2000);
    let detail = sanitize_toast_detail_single_line(&long, 64).expect("expected non-empty detail");
    assert!(
      detail.len() <= 64,
      "expected detail to be clamped to max bytes"
    );
    assert!(
      detail.ends_with('…'),
      "expected clamped detail to end with ellipsis, got {detail:?}"
    );
  }

  #[test]
  fn profile_autosave_toast_truncates_extremely_long_errors() {
    let long_error = "x".repeat(PROFILE_AUTOSAVE_ERROR_SUMMARY_MAX_BYTES * 10);
    let toast = format_profile_autosave_spawn_failure_toast(&long_error);
    let (first, second) = toast
      .split_once('\n')
      .expect("toast should contain a second line");
    assert_eq!(second, "Bookmarks/history changes may not be saved.");
    let summary = first
      .split_once(':')
      .map(|(_, tail)| tail.trim())
      .unwrap_or("");
    assert!(summary.ends_with('…'));
    assert!(
      summary.len() <= PROFILE_AUTOSAVE_ERROR_SUMMARY_MAX_BYTES,
      "summary was not truncated: {} bytes",
      summary.len()
    );
  }

  #[test]
  fn profile_autosave_toast_empty_error_is_generic() {
    let toast = format_profile_autosave_spawn_failure_toast("");
    assert_eq!(
      toast,
      "Failed to start profile autosave\nBookmarks/history changes may not be saved."
    );
  }

  #[test]
  fn profile_autosave_toast_whitespace_error_is_generic() {
    let toast = format_profile_autosave_spawn_failure_toast("  \n\t  ");
    assert_eq!(
      toast,
      "Failed to start profile autosave\nBookmarks/history changes may not be saved."
    );
  }

  #[test]
  fn profile_autosave_toast_control_only_error_is_generic() {
    let toast = format_profile_autosave_spawn_failure_toast("\u{0000}\u{0007}\n");
    assert_eq!(
      toast,
      "Failed to start profile autosave\nBookmarks/history changes may not be saved."
    );
  }

  #[test]
  fn profile_autosave_save_error_toast_sanitizes_path_and_error() {
    let err = ProfileAutosaveError::Bookmarks {
      path: "line1\nline2".to_string(),
      message: "oops\r\nmore".to_string(),
    };
    let toast = format_profile_autosave_save_error_toast(&err);
    let lines: Vec<&str> = toast.lines().collect();
    assert_eq!(lines[0], "Failed to save bookmarks");
    assert_eq!(lines[1], "Path: line1 line2");
    assert_eq!(lines[2], "Error: oops more");
  }

  #[test]
  fn profile_autosave_save_error_toast_truncates_long_fields() {
    let err = ProfileAutosaveError::History {
      path: "a".repeat(PROFILE_AUTOSAVE_SAVE_PATH_MAX_BYTES * 10),
      message: "b".repeat(PROFILE_AUTOSAVE_SAVE_ERROR_MAX_BYTES * 10),
    };
    let toast = format_profile_autosave_save_error_toast(&err);
    let lines: Vec<&str> = toast.lines().collect();
    assert_eq!(lines[0], "Failed to save history");

    let path_line = lines
      .iter()
      .find(|line| line.starts_with("Path:"))
      .expect("expected a Path: line");
    let safe_path = path_line
      .split_once("Path:")
      .map(|(_, rest)| rest.trim())
      .unwrap_or("");
    assert!(
      safe_path.len() <= PROFILE_AUTOSAVE_SAVE_PATH_MAX_BYTES,
      "expected sanitized path <= {} bytes, got {}",
      PROFILE_AUTOSAVE_SAVE_PATH_MAX_BYTES,
      safe_path.len()
    );
    assert!(
      safe_path.starts_with('…'),
      "expected truncated path to start with ellipsis"
    );

    let err_line = lines
      .iter()
      .find(|line| line.starts_with("Error:"))
      .expect("expected an Error: line");
    let safe_err = err_line
      .split_once("Error:")
      .map(|(_, rest)| rest.trim())
      .unwrap_or("");
    assert!(
      safe_err.len() <= PROFILE_AUTOSAVE_SAVE_ERROR_MAX_BYTES,
      "expected sanitized error <= {} bytes, got {}",
      PROFILE_AUTOSAVE_SAVE_ERROR_MAX_BYTES,
      safe_err.len()
    );
    assert!(
      safe_err.ends_with('…'),
      "expected truncated error to end with ellipsis"
    );
  }
}

