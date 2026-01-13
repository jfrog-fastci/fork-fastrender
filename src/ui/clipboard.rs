//! Clipboard protocol helpers and safety limits.
//!
//! Clipboard text crosses the browser ↔ renderer boundary in two directions:
//! - Renderer → browser: [`crate::ui::WorkerToUi::SetClipboardText`]
//! - Browser → renderer: [`crate::ui::UiToWorker::Paste`]
//!
//! In a multiprocess architecture we must treat both payloads as potentially attacker-controlled:
//! - A compromised renderer could try to send an enormous `SetClipboardText` to OOM the browser
//!   process (or the OS clipboard implementation).
//! - The OS clipboard can contain arbitrarily large data; without a cap, Ctrl/Cmd+V could flood the
//!   renderer with gigabytes of text.
//!
//! Policy:
//! - Clipboard text is deterministically truncated to [`MAX_CLIPBOARD_TEXT_BYTES`] bytes.
//! - Truncation always happens on a UTF-8 character boundary (so the result is valid UTF-8).
//! - Call sites should apply the clamp at the UI↔worker boundary in *both* directions.

/// Maximum bytes allowed for a clipboard text payload crossing the UI↔worker boundary.
///
/// This is a conservative hard cap intended to prevent untrusted senders from forcing large
/// allocations in the receiver.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 1 * 1024 * 1024; // 1 MiB

fn utf8_truncate_boundary(text: &str, max_bytes: usize) -> usize {
  if text.len() <= max_bytes {
    return text.len();
  }
  let mut idx = max_bytes;
  // Move back to the nearest UTF-8 codepoint boundary. This is bounded to at most 3 iterations
  // because a UTF-8 codepoint is at most 4 bytes.
  while idx > 0 && !text.is_char_boundary(idx) {
    idx -= 1;
  }
  idx
}

/// Truncate `text` to at most `max_bytes`, returning a valid UTF-8 subslice.
///
/// The returned string is guaranteed to be on a UTF-8 character boundary and is never longer than
/// `max_bytes` bytes.
pub fn truncate_utf8_to_max_bytes(text: &str, max_bytes: usize) -> &str {
  let idx = utf8_truncate_boundary(text, max_bytes);
  &text[..idx]
}

/// Clamp clipboard text that will cross the UI↔worker boundary.
///
/// Oversized clipboard text is deterministically truncated on a UTF-8 boundary to
/// [`MAX_CLIPBOARD_TEXT_BYTES`] bytes.
pub fn clamp_clipboard_text(text: &str) -> &str {
  truncate_utf8_to_max_bytes(text, MAX_CLIPBOARD_TEXT_BYTES)
}

/// Clamp clipboard text in place, truncating to [`MAX_CLIPBOARD_TEXT_BYTES`] bytes.
///
/// Returns `true` when truncation occurred.
pub fn clamp_clipboard_text_in_place(text: &mut String) -> bool {
  let idx = utf8_truncate_boundary(text, MAX_CLIPBOARD_TEXT_BYTES);
  if idx < text.len() {
    text.truncate(idx);
    true
  } else {
    false
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn clamp_clipboard_text_does_not_truncate_small_strings() {
    let s = "hello world";
    assert_eq!(clamp_clipboard_text(s), s);
  }

  #[test]
  fn clamp_clipboard_text_truncates_ascii_on_exact_boundary() {
    let oversize = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES + 16);
    let clamped = clamp_clipboard_text(&oversize);
    assert_eq!(clamped.len(), MAX_CLIPBOARD_TEXT_BYTES);
    // `clamped` is a `&str`, so it must be valid UTF-8.
    assert!(oversize.is_char_boundary(clamped.len()));
  }

  #[test]
  fn clamp_clipboard_text_truncates_multibyte_on_char_boundary() {
    // "€" is 3 bytes in UTF-8; MAX_CLIPBOARD_TEXT_BYTES is not divisible by 3, so truncation must
    // back up to the previous boundary.
    let unit = "€";
    assert_eq!(unit.len(), 3);
    let reps = (MAX_CLIPBOARD_TEXT_BYTES / unit.len()) + 8;
    let oversize = unit.repeat(reps);
    assert!(oversize.len() > MAX_CLIPBOARD_TEXT_BYTES);

    let clamped = clamp_clipboard_text(&oversize);
    assert!(clamped.len() <= MAX_CLIPBOARD_TEXT_BYTES);
    assert!(oversize.is_char_boundary(clamped.len()));
  }

  #[test]
  fn clamp_clipboard_text_in_place_truncates() {
    let mut oversize = "x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
    assert!(clamp_clipboard_text_in_place(&mut oversize));
    assert_eq!(oversize.len(), MAX_CLIPBOARD_TEXT_BYTES);
  }
}
