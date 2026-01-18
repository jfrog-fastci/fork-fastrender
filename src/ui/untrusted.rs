//! Helpers for handling untrusted values coming from the renderer/worker process.
//!
//! The UI treats all [`crate::ui::messages::WorkerToUi`] payloads as untrusted. Do not store or
//! display raw worker strings without first applying the helpers in this module.

use crate::interaction::FormSubmission;
use crate::tree::box_tree::{SelectControl, SelectItem};
use crate::ui::protocol_limits::{
  MAX_FAVICON_BYTES,
  MAX_FAVICON_EDGE_PX,
  MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES,
  MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_COUNT,
  MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_NAME_BYTES,
  MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_VALUE_BYTES,
  MAX_OPEN_IN_NEW_TAB_REQUEST_TOTAL_HEADER_BYTES,
  MAX_SELECT_ITEMS,
  MAX_SELECT_LABEL_BYTES,
  MAX_URL_BYTES,
};
use std::sync::Arc;

/// Validation failure for an untrusted [`FormSubmission`] payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrustedFormSubmissionError {
  InvalidUrl,
  UrlTooLong,
  InvalidHeaderName,
  InvalidHeaderValue,
  TooManyHeaders,
  HeaderNameTooLong,
  HeaderValueTooLong,
  HeadersTooLarge,
  BodyTooLarge,
}

impl UntrustedFormSubmissionError {
  /// User-facing toast message for a blocked form submission.
  pub fn toast_message(self) -> &'static str {
    match self {
      Self::InvalidUrl | Self::UrlTooLong => "Blocked attempt to open an invalid URL",
      Self::InvalidHeaderName | Self::InvalidHeaderValue => {
        "Blocked attempt to open a new tab: invalid request headers"
      }
      Self::TooManyHeaders
      | Self::HeaderNameTooLong
      | Self::HeaderValueTooLong
      | Self::HeadersTooLarge => "Blocked attempt to open a new tab: request headers too large",
      Self::BodyTooLarge => "Blocked attempt to open a new tab: request body too large",
    }
  }
}

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

    let ch_len = ch.len_utf8();
    let needs_space = pending_space && !out.is_empty();
    let extra_len = if needs_space { 1 } else { 0 };
    if out.len() + extra_len + ch_len > max_bytes {
      break;
    }
    if needs_space {
      out.push(' ');
    }
    pending_space = false;
    out.push(ch);
  }

  out
}

/// Validate + sanitize a navigation URL originating from the worker (untrusted renderer process).
///
/// This is intended for *display* and chrome state updates (address bar, tab title fallback, open
/// in new tab requests). It enforces the same scheme allowlist as user-typed URLs.
pub fn validate_untrusted_navigation_url(url: &str) -> Result<String, String> {
  // Reject absurdly large inputs early. `sanitize_untrusted_text` stops scanning once it reaches
  // `MAX_URL_BYTES`, but whitespace/control-only payloads could otherwise force O(n) work where
  // n is attacker-controlled.
  const ABSURD_URL_BYTES_MULTIPLIER: usize = 64;
  let absurd_url_limit = MAX_URL_BYTES.saturating_mul(ABSURD_URL_BYTES_MULTIPLIER);
  if url.len() > absurd_url_limit {
    return Err("URL too long".to_string());
  }

  // Apply the generic sanitization pass first so we never parse or store huge/hostile strings.
  let sanitized = sanitize_untrusted_text(url, MAX_URL_BYTES);
  if sanitized.trim().is_empty() {
    return Err("empty URL".to_string());
  }

  // Reuse the existing allowlist logic (http/https/file/about; reject javascript/unknown).
  crate::ui::url::validate_user_navigation_url_scheme(&sanitized)?;
  Ok(sanitized)
}

/// Validate + sanitize an untrusted [`FormSubmission`] originating from the worker process.
///
/// This is intended for `WorkerToUi::RequestOpenInNewTabRequest`: the windowed UI must treat the
/// payload as untrusted IPC and enforce size limits before cloning/storing it or forwarding it back
/// to the worker.
///
/// On success, this returns a sanitized `FormSubmission` with a normalized, scheme-validated URL.
pub fn validate_untrusted_form_submission_for_open_in_new_tab_request(
  mut request: FormSubmission,
) -> Result<FormSubmission, UntrustedFormSubmissionError> {
  // Avoid spending time sanitizing/parsing absurdly large payloads. This protects against inputs
  // that are mostly control/whitespace (which would otherwise take O(n) time to scan while the
  // output stays empty).
  const ABSURD_URL_BYTES_MULTIPLIER: usize = 64;
  let absurd_url_limit = MAX_URL_BYTES.saturating_mul(ABSURD_URL_BYTES_MULTIPLIER);
  if request.url.len() > absurd_url_limit {
    return Err(UntrustedFormSubmissionError::UrlTooLong);
  }

  request.url = validate_untrusted_navigation_url(&request.url)
    .map_err(|_| UntrustedFormSubmissionError::InvalidUrl)?;

  // Protect against hostile allocations where the attacker reserves a huge buffer but keeps the
  // logical length small. Length-based limits alone would accept such a payload and retain the
  // oversized allocation when forwarding the request back to the worker.
  const ABSURD_CAPACITY_MULTIPLIER: usize = 4;

  if request.headers.capacity()
    > MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_COUNT.saturating_mul(ABSURD_CAPACITY_MULTIPLIER)
  {
    return Err(UntrustedFormSubmissionError::TooManyHeaders);
  }

  if request.headers.len() > MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_COUNT {
    return Err(UntrustedFormSubmissionError::TooManyHeaders);
  }

  let mut total_header_bytes: usize = 0;
  for (name, value) in request.headers.iter() {
    if name.capacity()
      > MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_NAME_BYTES.saturating_mul(ABSURD_CAPACITY_MULTIPLIER)
    {
      return Err(UntrustedFormSubmissionError::HeaderNameTooLong);
    }
    if name.len() > MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_NAME_BYTES {
      return Err(UntrustedFormSubmissionError::HeaderNameTooLong);
    }
    if value.capacity()
      > MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_VALUE_BYTES.saturating_mul(ABSURD_CAPACITY_MULTIPLIER)
    {
      return Err(UntrustedFormSubmissionError::HeaderValueTooLong);
    }
    if value.len() > MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_VALUE_BYTES {
      return Err(UntrustedFormSubmissionError::HeaderValueTooLong);
    }

    // Validate that names/values satisfy HTTP token/value constraints so the worker cannot smuggle
    // control characters or invalid separators into the network stack.
    if http::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
      return Err(UntrustedFormSubmissionError::InvalidHeaderName);
    }
    if http::header::HeaderValue::from_str(value).is_err() {
      return Err(UntrustedFormSubmissionError::InvalidHeaderValue);
    }

    total_header_bytes = total_header_bytes
      .checked_add(name.len())
      .and_then(|total| total.checked_add(value.len()))
      .ok_or(UntrustedFormSubmissionError::HeadersTooLarge)?;
    if total_header_bytes > MAX_OPEN_IN_NEW_TAB_REQUEST_TOTAL_HEADER_BYTES {
      return Err(UntrustedFormSubmissionError::HeadersTooLarge);
    }
  }

  if let Some(body) = request.body.as_ref() {
    if body.capacity()
      > MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES.saturating_mul(ABSURD_CAPACITY_MULTIPLIER)
    {
      return Err(UntrustedFormSubmissionError::BodyTooLarge);
    }
    if body.len() > MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES {
      return Err(UntrustedFormSubmissionError::BodyTooLarge);
    }
  }

  Ok(request)
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
  if width > MAX_FAVICON_EDGE_PX || height > MAX_FAVICON_EDGE_PX {
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

/// Sanitize a `<select>` control snapshot received from the worker.
///
/// `<select>` option labels/values are derived from page content, so they must be treated as
/// untrusted text for UI display.
pub fn sanitize_untrusted_select_control(mut control: SelectControl) -> SelectControl {
  let items = match Arc::try_unwrap(control.items) {
    Ok(items) => items,
    Err(shared) => (*shared).clone(),
  };
  let mut items = items;

  if items.len() > MAX_SELECT_ITEMS {
    items.truncate(MAX_SELECT_ITEMS);
  }

  for item in &mut items {
    match item {
      SelectItem::OptGroupLabel { label, .. } => {
        *label = sanitize_untrusted_text(label, MAX_SELECT_LABEL_BYTES);
      }
      SelectItem::Option { label, value, .. } => {
        *label = sanitize_untrusted_text(label, MAX_SELECT_LABEL_BYTES);
        *value = sanitize_untrusted_text(value, MAX_SELECT_LABEL_BYTES);
      }
    }
  }

  // Ensure selected indices remain in-bounds after truncation.
  control.selected.retain(|idx| *idx < items.len());
  control.items = Arc::new(items);
  control
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::interaction::FormSubmissionMethod;
  use std::sync::Arc;

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
  fn validate_untrusted_favicon_rgba_rejects_oversized_dimensions() {
    let width = crate::ui::protocol_limits::MAX_FAVICON_EDGE_PX + 1;
    let height = 1;
    let len = (width as usize) * (height as usize) * 4;
    assert!(!validate_untrusted_favicon_rgba(len, width, height));
  }

  #[test]
  fn clamp_untrusted_utf8_does_not_split_codepoints() {
    let s = "é".repeat(10);
    let clamped = clamp_untrusted_utf8(&s, 5);
    assert!(clamped.len() <= 5);
    assert!(clamped.is_char_boundary(clamped.len()));
    assert!(clamped.chars().all(|c| c == 'é'));
  }

  #[test]
  fn sanitize_untrusted_select_control_strips_control_chars() {
    let control = SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(vec![
        SelectItem::OptGroupLabel {
          label: "Group\u{0000}".to_string(),
          disabled: false,
        },
        SelectItem::Option {
          node_id: 1,
          label: "A\u{007f}".to_string(),
          value: "v\u{001f}".to_string(),
          selected: true,
          disabled: false,
          in_optgroup: true,
        },
      ]),
      selected: vec![1],
    };

    let sanitized = sanitize_untrusted_select_control(control);
    assert_eq!(sanitized.items[0].label(), "Group");
    match &sanitized.items[1] {
      SelectItem::Option { label, value, .. } => {
        assert_eq!(label, "A");
        assert_eq!(value, "v");
      }
      _ => panic!("expected Option"),
    }
  }

  #[test]
  fn validate_untrusted_form_submission_rejects_excessive_body() {
    let request = FormSubmission {
      url: "https://example.com/".to_string(),
      method: FormSubmissionMethod::Post,
      headers: vec![(
        "content-type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
      )],
      body: Some(vec![0u8; MAX_OPEN_IN_NEW_TAB_REQUEST_BODY_BYTES + 1]),
    };

    let err = validate_untrusted_form_submission_for_open_in_new_tab_request(request)
      .expect_err("expected body size limit to reject request");
    assert_eq!(err, UntrustedFormSubmissionError::BodyTooLarge);
  }

  #[test]
  fn validate_untrusted_form_submission_rejects_too_many_headers() {
    let mut headers = Vec::new();
    for idx in 0..(MAX_OPEN_IN_NEW_TAB_REQUEST_HEADER_COUNT + 1) {
      headers.push((format!("x-{idx}"), "v".to_string()));
    }
    let request = FormSubmission {
      url: "https://example.com/".to_string(),
      method: FormSubmissionMethod::Post,
      headers,
      body: None,
    };

    let err = validate_untrusted_form_submission_for_open_in_new_tab_request(request)
      .expect_err("expected header count limit to reject request");
    assert_eq!(err, UntrustedFormSubmissionError::TooManyHeaders);
  }

  #[test]
  fn validate_untrusted_form_submission_accepts_small_payload() {
    let request = FormSubmission {
      url: " https://example.com/\n".to_string(),
      method: FormSubmissionMethod::Post,
      headers: vec![(
        "content-type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
      )],
      body: Some(b"q=a+b".to_vec()),
    };

    let sanitized = validate_untrusted_form_submission_for_open_in_new_tab_request(request)
      .expect("expected request to be valid");
    assert_eq!(sanitized.url, "https://example.com/");
    assert_eq!(sanitized.body, Some(b"q=a+b".to_vec()));
  }

  #[test]
  fn validate_untrusted_form_submission_rejects_invalid_header_name() {
    let request = FormSubmission {
      url: "https://example.com/".to_string(),
      method: FormSubmissionMethod::Post,
      headers: vec![("bad header".to_string(), "ok".to_string())],
      body: None,
    };

    let err = validate_untrusted_form_submission_for_open_in_new_tab_request(request)
      .expect_err("expected invalid header name to be rejected");
    assert_eq!(err, UntrustedFormSubmissionError::InvalidHeaderName);
  }

  #[test]
  fn validate_untrusted_form_submission_rejects_invalid_header_value() {
    let request = FormSubmission {
      url: "https://example.com/".to_string(),
      method: FormSubmissionMethod::Post,
      headers: vec![("x-test".to_string(), "hello\nworld".to_string())],
      body: None,
    };

    let err = validate_untrusted_form_submission_for_open_in_new_tab_request(request)
      .expect_err("expected invalid header value to be rejected");
    assert_eq!(err, UntrustedFormSubmissionError::InvalidHeaderValue);
  }
}
