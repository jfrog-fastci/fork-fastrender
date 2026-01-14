use crate::ui::notifications::{Toast, ToastKind};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// Maximum number of characters from a download file name to include in a toast.
///
/// Toasts are short-lived UI affordances; keep the file name reasonably small so extremely long
/// names do not create giant popups.
const MAX_DOWNLOAD_TOAST_FILE_NAME_CHARS: usize = 120;

/// Maximum number of characters from a download error string to include in the toast.
const MAX_DOWNLOAD_TOAST_ERROR_SUMMARY_CHARS: usize = 200;

const DOWNLOAD_TOAST_MORE_SUFFIX_PREFIX: &str = " (+";
const DOWNLOAD_TOAST_MORE_SUFFIX_SUFFIX: &str = " more)";

/// Stateful coalescer for download lifecycle toast notifications.
///
/// The windowed browser UI shows a single chrome toast at a time. Download events can arrive in
/// bursts (multiple files starting/finishing quickly), so we coalesce them into one toast and keep
/// track of which filenames we've already counted.
#[derive(Debug, Default)]
pub struct DownloadToastCoalescer {
  seen_file_names: HashSet<u64>,
}

impl DownloadToastCoalescer {
  pub fn reset(&mut self) {
    self.seen_file_names.clear();
  }

  /// Clears internal state when the current chrome toast is no longer a download toast.
  ///
  /// This prevents the coalescer from retaining large state after the toast expires or is replaced
  /// by unrelated notifications.
  pub fn sync_current_toast(&mut self, toast: Option<&Toast>) {
    if !toast.is_some_and(|toast| is_download_toast_text(&toast.text)) {
      self.reset();
    }
  }
}

/// Minimal download lifecycle event used for generating user-facing toast notifications.
///
/// This is intentionally independent of any UI framework (egui/winit) so it can be unit tested in
/// the core crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEvent {
  Started {
    file_name: String,
  },
  Finished {
    file_name: String,
    outcome: DownloadOutcome,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
  Completed,
  Cancelled,
  Failed { error: String },
}

impl From<crate::ui::messages::DownloadOutcome> for DownloadOutcome {
  fn from(outcome: crate::ui::messages::DownloadOutcome) -> Self {
    match outcome {
      crate::ui::messages::DownloadOutcome::Completed => Self::Completed,
      crate::ui::messages::DownloadOutcome::Cancelled => Self::Cancelled,
      crate::ui::messages::DownloadOutcome::Failed { error } => Self::Failed { error },
    }
  }
}

fn strip_more_suffix(text: &str) -> &str {
  let trimmed = text.trim_end();
  if trimmed.ends_with(DOWNLOAD_TOAST_MORE_SUFFIX_SUFFIX) {
    if let Some(prefix_idx) = trimmed.rfind(DOWNLOAD_TOAST_MORE_SUFFIX_PREFIX) {
      let number_start = prefix_idx + DOWNLOAD_TOAST_MORE_SUFFIX_PREFIX.len();
      let number_end = trimmed.len() - DOWNLOAD_TOAST_MORE_SUFFIX_SUFFIX.len();
      if let Some(digits) = trimmed.get(number_start..number_end) {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
          return trimmed.get(..prefix_idx).unwrap_or("").trim_end();
        }
      }
    }
  }

  trimmed
}

fn file_name_fingerprint(file_name: &str) -> u64 {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  file_name.hash(&mut hasher);
  hasher.finish()
}

fn truncate_chars_with_ellipsis(value: &str, max_chars: usize) -> String {
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

fn normalize_file_name_for_toast(file_name: &str) -> String {
  let name = file_name.trim();
  if name.is_empty() {
    return "download".to_string();
  }
  truncate_chars_with_ellipsis(name, MAX_DOWNLOAD_TOAST_FILE_NAME_CHARS)
}

fn summarize_download_error_for_toast(error: &str) -> Option<String> {
  let trimmed = error.trim();
  if trimmed.is_empty() {
    return None;
  }

  // Prefer the first non-empty line; many errors include verbose context on subsequent lines.
  let first_line = trimmed
    .lines()
    .map(|l| l.trim())
    .find(|l| !l.is_empty())
    .unwrap_or(trimmed);
  let compact = truncate_chars_with_ellipsis(first_line, MAX_DOWNLOAD_TOAST_ERROR_SUMMARY_CHARS);
  if compact.trim().is_empty() {
    None
  } else {
    Some(compact)
  }
}

fn download_toast_base(event: &DownloadEvent) -> (ToastKind, String, Option<String>) {
  match event {
    DownloadEvent::Started { file_name } => {
      let file_name = normalize_file_name_for_toast(file_name);
      (
        ToastKind::Info,
        format!("Downloading {file_name}…"),
        Some(file_name),
      )
    }
    DownloadEvent::Finished { file_name, outcome } => {
      let file_name_norm = normalize_file_name_for_toast(file_name);
      match outcome {
        DownloadOutcome::Completed => (
          ToastKind::Info,
          format!("Downloaded {file_name_norm}"),
          Some(file_name_norm),
        ),
        DownloadOutcome::Cancelled => (
          ToastKind::Warning,
          format!("Download cancelled: {file_name_norm}"),
          Some(file_name_norm),
        ),
        DownloadOutcome::Failed { error } => {
          let summary = summarize_download_error_for_toast(error);
          let text = if let Some(summary) = summary {
            format!("Download failed: {file_name_norm}\n{summary}")
          } else {
            format!("Download failed: {file_name_norm}")
          };
          (ToastKind::Error, text, Some(file_name_norm))
        }
      }
    }
  }
}

fn is_download_toast_text(text: &str) -> bool {
  let base = strip_more_suffix(text);
  base.starts_with("Downloading ")
    || base.starts_with("Downloaded ")
    || base.starts_with("Download failed:")
    || base.starts_with("Download cancelled")
}

fn extract_download_file_name_from_toast_text(text: &str) -> Option<String> {
  let base = strip_more_suffix(text).trim();
  if base.starts_with("Downloading ") {
    let rest = base.strip_prefix("Downloading ")?;
    let rest = rest.strip_suffix('…').unwrap_or(rest);
    let file_name = rest.trim();
    if file_name.is_empty() {
      None
    } else {
      Some(file_name.to_string())
    }
  } else if base.starts_with("Downloaded ") {
    let rest = base.strip_prefix("Downloaded ")?;
    let file_name = rest.trim();
    if file_name.is_empty() {
      None
    } else {
      Some(file_name.to_string())
    }
  } else if base.starts_with("Download failed:") {
    let rest = base.strip_prefix("Download failed:")?.trim_start();
    let file_name = rest
      .split_once('\n')
      .map(|(first, _)| first)
      .unwrap_or(rest)
      .split_once(" — ")
      .map(|(first, _)| first)
      .unwrap_or(rest)
      .trim();
    if file_name.is_empty() {
      None
    } else {
      Some(file_name.to_string())
    }
  } else if base.starts_with("Download cancelled:") {
    let rest = base.strip_prefix("Download cancelled:")?.trim_start();
    let file_name = rest.trim();
    if file_name.is_empty() {
      None
    } else {
      Some(file_name.to_string())
    }
  } else {
    None
  }
}

/// Create a toast presentation for a download lifecycle event, coalescing against an existing
/// visible download toast.
///
/// Coalescing rules:
/// - Only applies when the existing toast is itself a download toast (determined by matching known
///   download toast prefixes).
/// - Higher severity (`Error > Warning > Info`) supersedes lower.
/// - When coalescing, append `(+N more)` where `N` is the number of additional *distinct* download
///   file names observed while the toast was visible.
/// - Start→finish transitions for a single download do not increment the counter.
pub fn coalesce_download_toast(
  coalescer: &mut DownloadToastCoalescer,
  existing_toast: Option<&Toast>,
  event: DownloadEvent,
) -> (ToastKind, String) {
  let (new_kind, new_base_text, new_file_name) = download_toast_base(&event);

  let existing = existing_toast.filter(|t| is_download_toast_text(&t.text));
  if existing.is_none() {
    coalescer.reset();
  }

  if let Some(file_name) = new_file_name
    .as_deref()
    .map(str::trim)
    .filter(|n| !n.is_empty())
  {
    coalescer
      .seen_file_names
      .insert(file_name_fingerprint(file_name));
  }

  let Some(existing) = existing else {
    return (new_kind, new_base_text);
  };

  let existing_base = strip_more_suffix(&existing.text).to_string();
  let existing_file_name = extract_download_file_name_from_toast_text(&existing_base);

  fn kind_rank(kind: ToastKind) -> u8 {
    match kind {
      ToastKind::Info => 0,
      ToastKind::Warning => 1,
      ToastKind::Error => 2,
    }
  }
  let replace_message = kind_rank(new_kind) >= kind_rank(existing.kind);

  let out_kind = if replace_message {
    new_kind
  } else {
    existing.kind
  };
  let out_base = if replace_message {
    new_base_text
  } else {
    existing_base
  };

  let base_file_name = if replace_message {
    new_file_name
  } else {
    existing_file_name
  };

  let base_fp = base_file_name
    .as_deref()
    .map(str::trim)
    .filter(|n| !n.is_empty())
    .map(file_name_fingerprint);
  if let Some(fp) = base_fp {
    coalescer.seen_file_names.insert(fp);
  }

  let mut more = coalescer.seen_file_names.len();
  if base_fp.is_some() {
    more = more.saturating_sub(1);
  }

  let out_text = if more == 0 {
    out_base
  } else {
    format!("{out_base} (+{more} more)")
  };

  (out_kind, out_text)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Instant;

  fn toast(kind: ToastKind, text: &str) -> Toast {
    Toast {
      kind,
      text: text.to_string(),
      expires_at: Instant::now(),
    }
  }

  #[test]
  fn mapping_started_is_info() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Started {
        file_name: "file.txt".to_string(),
      },
    );
    assert_eq!(kind, ToastKind::Info);
    assert_eq!(text, "Downloading file.txt…");
  }

  #[test]
  fn mapping_finished_completed_is_info() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Finished {
        file_name: "file.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    assert_eq!(kind, ToastKind::Info);
    assert_eq!(text, "Downloaded file.txt");
  }

  #[test]
  fn mapping_finished_cancelled_is_warning_and_includes_file_name() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Finished {
        file_name: "file.txt".to_string(),
        outcome: DownloadOutcome::Cancelled,
      },
    );
    assert_eq!(kind, ToastKind::Warning);
    assert_eq!(text, "Download cancelled: file.txt");
  }

  #[test]
  fn mapping_finished_failed_is_error_and_includes_summary() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Finished {
        file_name: "file.txt".to_string(),
        outcome: DownloadOutcome::Failed {
          error: "Network error: connection reset by peer\nverbose details".to_string(),
        },
      },
    );
    assert_eq!(kind, ToastKind::Error);
    assert!(
      text.starts_with("Download failed: file.txt\nNetwork error: connection reset by peer"),
      "unexpected toast text: {text:?}"
    );
  }

  #[test]
  fn coalescing_different_files_increments_more() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Started {
        file_name: "a.txt".to_string(),
      },
    );
    let toast1 = toast(kind1, &text1);
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Started {
        file_name: "b.txt".to_string(),
      },
    );
    assert_eq!(kind, ToastKind::Info);
    assert_eq!(text, "Downloading b.txt… (+1 more)");
  }

  #[test]
  fn coalescing_multiple_started_events_accumulates_more_count() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Started {
        file_name: "a.txt".to_string(),
      },
    );
    let toast1 = toast(kind1, &text1);

    let (kind2, text2) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Started {
        file_name: "b.txt".to_string(),
      },
    );
    let toast2 = toast(kind2, &text2);

    let (kind3, text3) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast2),
      DownloadEvent::Started {
        file_name: "c.txt".to_string(),
      },
    );

    assert_eq!(kind3, ToastKind::Info);
    assert_eq!(text3, "Downloading c.txt… (+2 more)");
  }

  #[test]
  fn finishing_already_counted_file_does_not_inflate_more_count() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Started {
        file_name: "a.txt".to_string(),
      },
    );
    let toast1 = toast(kind1, &text1);

    let (kind2, text2) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Started {
        file_name: "b.txt".to_string(),
      },
    );
    let toast2 = toast(kind2, &text2);

    let (kind3, text3) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast2),
      DownloadEvent::Started {
        file_name: "c.txt".to_string(),
      },
    );
    let toast3 = toast(kind3, &text3);
    assert_eq!(kind3, ToastKind::Info);
    assert_eq!(text3, "Downloading c.txt… (+2 more)");

    let (kind4, text4) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast3),
      DownloadEvent::Finished {
        file_name: "a.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    assert_eq!(kind4, ToastKind::Info);
    assert_eq!(text4, "Downloaded a.txt (+2 more)");
  }

  #[test]
  fn coalescing_same_file_does_not_increment_more() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Started {
        file_name: "a.txt".to_string(),
      },
    );
    let toast1 = toast(kind1, &text1);
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Finished {
        file_name: "a.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    assert_eq!(kind, ToastKind::Info);
    assert_eq!(text, "Downloaded a.txt");
  }

  #[test]
  fn coalescing_multiple_completed_events_accumulates_more_count() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Finished {
        file_name: "a.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    let toast1 = toast(kind1, &text1);

    let (kind2, text2) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Finished {
        file_name: "b.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    let toast2 = toast(kind2, &text2);

    let (kind3, text3) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast2),
      DownloadEvent::Finished {
        file_name: "c.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );

    assert_eq!(kind3, ToastKind::Info);
    assert_eq!(text3, "Downloaded c.txt (+2 more)");
  }

  #[test]
  fn error_supersedes_info_and_carries_more() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Finished {
        file_name: "a.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    let toast1 = toast(kind1, &text1);
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Finished {
        file_name: "b.txt".to_string(),
        outcome: DownloadOutcome::Failed {
          error: "nope".to_string(),
        },
      },
    );
    assert_eq!(kind, ToastKind::Error);
    assert!(
      text.starts_with("Download failed: b.txt\nnope"),
      "unexpected toast text: {text:?}"
    );
    assert!(
      text.ends_with("(+1 more)"),
      "expected coalesced suffix, got: {text:?}"
    );
  }

  #[test]
  fn info_does_not_override_error_but_increments_more() {
    let mut coalescer = DownloadToastCoalescer::default();
    let (kind1, text1) = coalesce_download_toast(
      &mut coalescer,
      None,
      DownloadEvent::Finished {
        file_name: "a.txt".to_string(),
        outcome: DownloadOutcome::Failed {
          error: "nope".to_string(),
        },
      },
    );
    let toast1 = toast(kind1, &text1);
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      Some(&toast1),
      DownloadEvent::Finished {
        file_name: "b.txt".to_string(),
        outcome: DownloadOutcome::Completed,
      },
    );
    assert_eq!(kind, ToastKind::Error);
    assert_eq!(text, "Download failed: a.txt\nnope (+1 more)");
  }

  #[test]
  fn non_download_toast_is_not_coalesced() {
    let mut coalescer = DownloadToastCoalescer::default();
    let existing = toast(ToastKind::Info, "Save not implemented yet");
    let (kind, text) = coalesce_download_toast(
      &mut coalescer,
      Some(&existing),
      DownloadEvent::Started {
        file_name: "a.txt".to_string(),
      },
    );
    assert_eq!(kind, ToastKind::Info);
    assert_eq!(text, "Downloading a.txt…");
  }

  #[test]
  fn long_file_names_are_truncated_with_ellipsis() {
    let long_name = "a".repeat(MAX_DOWNLOAD_TOAST_FILE_NAME_CHARS + 10);
    let normalized = normalize_file_name_for_toast(&long_name);
    assert!(
      normalized.ends_with('…'),
      "expected ellipsis truncation, got {normalized:?}"
    );
    assert_eq!(
      normalized.chars().count(),
      MAX_DOWNLOAD_TOAST_FILE_NAME_CHARS + 1
    );
  }
}
