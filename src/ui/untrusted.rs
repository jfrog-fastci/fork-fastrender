//! Helpers for handling untrusted values coming from the renderer/worker process.
//!
//! The UI treats all [`crate::ui::messages::WorkerToUi`] payloads as untrusted. Do not store or
//! display raw worker strings without first applying the helpers in this module.

use crate::ui::protocol_limits::{MAX_FAVICON_BYTES, MAX_URL_BYTES};

/// Clamp an untrusted string to `max_bytes` in UTF-8 without splitting code points.
///
/// Unlike [`sanitize_untrusted_text`], this does **not** remove control characters or normalize
/// whitespace; it is intended for payloads where preserving content matters (e.g. clipboard text),
/// but the UI still needs an upper bound to avoid OOM.
pub fn clamp_untrusted_utf8(s: &str, max_bytes: usize) -> String {
  if max_bytes == 0 {
    return String::new();
  }
  if s.len() <= max_bytes {
    return s.to_string();
  }
  let mut end = max_bytes.min(s.len());
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  s.get(..end).unwrap_or("").to_string()
}

/// Sanitize untrusted text for UI display/storage.
///
/// - Strips ASCII control characters (0x00–0x1F, 0x7F).
/// - Collapses runs of whitespace into a single ASCII space.
/// - Trims leading/trailing whitespace.
/// - Clamps the output to `max_bytes` in UTF-8 without splitting code points.
pub fn sanitize_untrusted_text(s: &str, max_bytes: usize) -> String {
  if max_bytes == 0 {
    return String::new();
  }

  // Avoid allocating based on `s.len()` (attacker-controlled). Pre-allocate up to the limit.
  let mut out = String::with_capacity(max_bytes.min(1024));

  // We build the string incrementally while enforcing `max_bytes` so extremely large inputs never
  // cause large intermediate allocations.
  let mut pending_space = false;
  for ch in s.chars() {
    if ch.is_ascii_control() {
      continue;
    }

    if ch.is_whitespace() {
      pending_space = true;
      continue;
    }

    if pending_space && !out.is_empty() {
      if out.len() + 1 > max_bytes {
        break;
      }
      out.push(' ');
    }
    pending_space = false;

    let ch_len = ch.len_utf8();
    if out.len() + ch_len > max_bytes {
      break;
    }
    out.push(ch);
  }

  out
}

/// Validate + sanitize a navigation URL originating from the worker (untrusted renderer process).
///
/// This is intended for *display* and chrome state updates (address bar, tab title fallback, open
/// in new tab requests). It enforces the same scheme allowlist as user-typed URLs.
pub fn validate_untrusted_navigation_url(url: &str) -> Result<String, String> {
  // Apply the generic sanitization pass first so we never parse or store huge/hostile strings.
  let sanitized = sanitize_untrusted_text(url, MAX_URL_BYTES);
  if sanitized.trim().is_empty() {
    return Err("empty URL".to_string());
  }

  // Reuse the existing allowlist logic (http/https/file/about; reject javascript/unknown).
  crate::ui::url::validate_user_navigation_url_scheme(&sanitized)?;
  Ok(sanitized)
}

/// Validate that an untrusted RGBA8 favicon buffer has a sane shape and byte length.
///
/// Returns `true` when:
/// - `width` and `height` are non-zero,
/// - `rgba_len == width * height * 4` (with checked arithmetic),
/// - and the payload fits within [`MAX_FAVICON_BYTES`].
pub fn validate_untrusted_favicon_rgba(rgba_len: usize, width: u32, height: u32) -> bool {
  if width == 0 || height == 0 {
    return false;
  }
  let expected = (width as usize)
    .checked_mul(height as usize)
    .and_then(|px| px.checked_mul(4));
  match expected {
    Some(expected) => expected == rgba_len && expected <= MAX_FAVICON_BYTES,
    None => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sanitize_strips_control_and_collapses_whitespace() {
    let s = " \tHello\u{0000}\nworld\u{007f}  ";
    let out = sanitize_untrusted_text(s, 1024);
    // Tabs/newlines are stripped as control characters; spaces are collapsed and trimmed.
    assert_eq!(out, "Helloworld");
    assert!(!out.chars().any(|c| c.is_ascii_control()));
  }

  #[test]
  fn sanitize_clamps_without_splitting_codepoints() {
    // "é" is 2 bytes in UTF-8.
    let s = "é".repeat(10);
    let out = sanitize_untrusted_text(&s, 5);
    assert!(out.len() <= 5);
    assert!(out.is_char_boundary(out.len()));
    assert!(out.chars().all(|c| c == 'é'));
  }

  #[test]
  fn validate_untrusted_navigation_url_rejects_javascript() {
    assert!(validate_untrusted_navigation_url("javascript:alert(1)").is_err());
  }

  #[test]
  fn validate_untrusted_favicon_rgba_rejects_mismatched_len() {
    assert!(!validate_untrusted_favicon_rgba(3, 2, 2));
    assert!(validate_untrusted_favicon_rgba(2 * 2 * 4, 2, 2));
  }

  #[test]
  fn clamp_untrusted_utf8_does_not_split_codepoints() {
    let s = "é".repeat(10);
    let clamped = clamp_untrusted_utf8(&s, 5);
    assert!(clamped.len() <= 5);
    assert!(clamped.is_char_boundary(clamped.len()));
    assert!(clamped.chars().all(|c| c == 'é'));
  }
}
