//! Browser-side validation for renderer-initiated same-document URL updates.
//!
//! In a multi-process browser architecture the renderer process is not trusted to update the
//! authoritative history/address-bar URL directly. APIs like `history.pushState`,
//! `history.replaceState`, and `location.hash = ...` can mutate the visible URL without performing a
//! navigation; if the browser accepted the renderer-provided URL blindly a compromised renderer
//! could spoof the address bar (e.g. by switching origins).
//!
//! This module provides a reusable validator that the browser process can apply to any
//! renderer-provided "same document" URL mutation before committing it to history state.
//!
//! Security invariants intentionally mirror the checks performed in
//! `src/js/vmjs/window_realm.rs` for `history.pushState`/`replaceState`, but **must** be rechecked in
//! the browser because the renderer may be hostile.

use crate::resource::web_url::{WebUrl, WebUrlError, WebUrlLimitKind, WebUrlLimits};
use thiserror::Error;

/// Maximum URL length (in UTF-8 bytes) accepted from the renderer for same-document mutations.
///
/// Rationale:
/// - Real-world same-document URL updates (SPA routing, hash changes) are typically well under a few
///   kilobytes.
/// - Keeping a relatively small hard cap prevents pathological renderer input from causing
///   disproportionate work during parsing/canonicalization and from bloating browser history state.
///
/// The browser is free to apply additional, stricter UI/UX limits when displaying the URL.
pub const MAX_RENDERER_SAME_DOCUMENT_URL_BYTES: usize = 32 * 1024; // 32 KiB

#[derive(Debug, Error)]
pub enum ValidationError {
  #[error(
    "renderer-provided URL exceeded maximum length ({attempted_bytes} > {max_bytes} bytes)"
  )]
  UrlTooLong {
    max_bytes: usize,
    attempted_bytes: usize,
  },

  #[error("current URL is not a valid absolute URL")]
  InvalidCurrentUrl,

  #[error("renderer-provided URL is not a valid URL relative to the current URL")]
  InvalidNewUrl,

  #[error("unsupported URL scheme: {scheme}")]
  UnsupportedScheme { scheme: String },

  #[error(
    "same-document URL update may not change origin (current origin {current_origin}, new origin {new_origin})"
  )]
  OriginChanged {
    current_origin: String,
    new_origin: String,
  },

  #[error(
    "same-document URL update may not change scheme for opaque origins (current scheme {current_scheme}, new scheme {new_scheme})"
  )]
  OpaqueOriginSchemeChanged {
    current_scheme: String,
    new_scheme: String,
  },
}

fn map_web_url_error(err: WebUrlError) -> Option<ValidationError> {
  match err {
    WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit,
      attempted,
    } => Some(ValidationError::UrlTooLong {
      max_bytes: limit,
      attempted_bytes: attempted,
    }),
    _ => None,
  }
}

fn scheme_and_origin_for_history_validation(url: &WebUrl) -> (String, String) {
  let inner = url.inner.lock();
  let scheme = inner.url.scheme().to_string();
  let origin = match scheme.as_str() {
    "http" | "https" => inner.url.origin().ascii_serialization(),
    _ => "null".to_string(),
  };
  (scheme, origin)
}

/// Validate and canonicalize a renderer-provided same-document URL mutation.
///
/// This function must be applied by the *browser process* to any URL update received from a
/// renderer process for APIs that mutate the visible URL without navigating
/// (`history.pushState/replaceState`, `location.hash`, history traversal).
///
/// Security checks:
/// - Enforces a hard maximum URL length on the renderer-provided string (`new_url`).
/// - Resolves `new_url` against `current_url` using the bounded [`WebUrl`] parser.
/// - Enforces a supported scheme allowlist: `http`, `https`, `file`, `data`, `about`.
/// - Enforces same-origin URL updates:
///   - For `http`/`https` URLs, compares `Url::origin().ascii_serialization()`.
///   - For other (opaque-origin) schemes, treats origin as `"null"` *and* requires the scheme to
///     remain unchanged (since multiple schemes share the `"null"` serialized origin).
///
/// On success, returns the resolved URL in canonical serialized form (`href()`), which must be used
/// for browser history/address-bar state rather than the raw `new_url` string.
pub fn validate_same_document_url_update(
  current_url: &str,
  new_url: &str,
) -> Result<String, ValidationError> {
  // Fast-path length check for hostile renderer input. `new_url` arrives from IPC; rejecting before
  // parsing avoids unnecessary work when the renderer sends an obviously invalid payload.
  if new_url.len() > MAX_RENDERER_SAME_DOCUMENT_URL_BYTES {
    return Err(ValidationError::UrlTooLong {
      max_bytes: MAX_RENDERER_SAME_DOCUMENT_URL_BYTES,
      attempted_bytes: new_url.len(),
    });
  }

  let limits = WebUrlLimits {
    max_input_bytes: MAX_RENDERER_SAME_DOCUMENT_URL_BYTES,
    ..WebUrlLimits::default()
  };

  let current = WebUrl::parse_without_diagnostics(current_url, None, &limits).map_err(|err| {
    map_web_url_error(err).unwrap_or(ValidationError::InvalidCurrentUrl)
  })?;

  let parsed = WebUrl::parse_without_diagnostics(new_url, Some(current_url), &limits)
    .map_err(|err| map_web_url_error(err).unwrap_or(ValidationError::InvalidNewUrl))?;

  let (new_scheme, new_origin) = scheme_and_origin_for_history_validation(&parsed);

  match new_scheme.as_str() {
    "http" | "https" | "file" | "data" | "about" => {}
    _ => return Err(ValidationError::UnsupportedScheme { scheme: new_scheme }),
  }

  let (current_scheme, current_origin) = scheme_and_origin_for_history_validation(&current);

  if current_origin != new_origin {
    return Err(ValidationError::OriginChanged {
      current_origin,
      new_origin,
    });
  }

  if current_origin == "null" && current_scheme != new_scheme {
    return Err(ValidationError::OpaqueOriginSchemeChanged {
      current_scheme,
      new_scheme,
    });
  }

  parsed
    .href()
    .map_err(|err| map_web_url_error(err).unwrap_or(ValidationError::InvalidNewUrl))
}

#[cfg(test)]
mod tests {
  use super::{
    validate_same_document_url_update, ValidationError, MAX_RENDERER_SAME_DOCUMENT_URL_BYTES,
  };

  #[test]
  fn accepts_same_origin_path_query_fragment_changes() {
    let current = "https://example.com/a/b?x=1#old";
    let new = "/c/d?y=2#new";
    let validated = validate_same_document_url_update(current, new).expect("should validate");
    assert_eq!(validated, "https://example.com/c/d?y=2#new");
  }

  #[test]
  fn rejects_origin_change() {
    let current = "https://example.com/";
    let new = "https://evil.com/";
    let err = validate_same_document_url_update(current, new).expect_err("expected rejection");
    assert!(matches!(err, ValidationError::OriginChanged { .. }));
  }

  #[test]
  fn rejects_opaque_origin_scheme_change() {
    let current = "about:blank";
    let new = "data:text/plain,hi";
    let err = validate_same_document_url_update(current, new).expect_err("expected rejection");
    assert!(matches!(err, ValidationError::OpaqueOriginSchemeChanged { .. }));
  }

  #[test]
  fn accepts_about_to_about_and_data_to_data() {
    let current = "about:blank";
    let new = "about:blank#hash";
    let validated = validate_same_document_url_update(current, new).expect("about update");
    assert_eq!(validated, "about:blank#hash");

    let current = "data:text/plain,hi";
    let new = "data:text/plain,bye";
    let validated = validate_same_document_url_update(current, new).expect("data update");
    assert_eq!(validated, "data:text/plain,bye");
  }

  #[test]
  fn rejects_unsupported_scheme() {
    let current = "https://example.com/";
    let new = "javascript:alert(1)";
    let err = validate_same_document_url_update(current, new).expect_err("expected rejection");
    assert!(matches!(err, ValidationError::UnsupportedScheme { .. }));
  }

  #[test]
  fn rejects_overly_long_urls() {
    let current = "https://example.com/";
    let new = format!(
      "https://example.com/{}",
      "a".repeat(MAX_RENDERER_SAME_DOCUMENT_URL_BYTES)
    );
    assert!(
      new.len() > MAX_RENDERER_SAME_DOCUMENT_URL_BYTES,
      "test should exceed limit"
    );
    let err = validate_same_document_url_update(current, &new).expect_err("expected rejection");
    assert!(matches!(err, ValidationError::UrlTooLong { .. }));
  }
}

