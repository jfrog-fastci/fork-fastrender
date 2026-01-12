//! Shared WHATWG URL resolution helper for JS bindings.
//!
//! Web APIs like `fetch()` and `new Request()` accept *relative* URL strings. In browser contexts
//! these are resolved against the document base URL.

use std::sync::OnceLock;

use thiserror::Error;

use super::url::{Url, UrlError, UrlLimits};

#[derive(Debug, Error)]
pub enum UrlResolveError {
  #[error("relative URL has no base URL")]
  RelativeUrlWithoutBase,
  #[error(transparent)]
  Url(#[from] UrlError),
}

/// Resolve `input` as a WHATWG URL, optionally against `base`.
///
/// When `base` is `None`, relative URL strings error (mirroring the WHATWG parser's "relative URL
/// without a base" failure).
pub fn resolve_url(input: &str, base_url: Option<&str>) -> Result<String, UrlResolveError> {
  static DEFAULT_LIMITS: OnceLock<UrlLimits> = OnceLock::new();
  let limits = DEFAULT_LIMITS.get_or_init(UrlLimits::default);

  // `WebUrl::parse_without_diagnostics` intentionally drops detailed diagnostics (including the
  // underlying `url::ParseError`), but our JS bindings need to distinguish the "relative URL
  // without a base" failure from other parse errors when `base` is missing.
  if base_url.is_none()
    && matches!(
      ::url::Url::parse(input),
      Err(::url::ParseError::RelativeUrlWithoutBase)
    )
  {
    return Err(UrlResolveError::RelativeUrlWithoutBase);
  }

  let url =
    Url::parse_without_diagnostics(input, base_url, limits).map_err(UrlResolveError::Url)?;

  url.href().map_err(UrlResolveError::Url)
}

#[cfg(test)]
mod tests {
  use super::{resolve_url, UrlResolveError};

  #[test]
  fn resolves_relative_against_document_url() {
    let resolved = resolve_url("foo", Some("https://example.com/dir/page")).unwrap();
    assert_eq!(resolved, "https://example.com/dir/foo");
  }

  #[test]
  fn resolves_absolute_path_against_document_url() {
    let resolved = resolve_url("/abs", Some("https://example.com/dir/page")).unwrap();
    assert_eq!(resolved, "https://example.com/abs");
  }

  #[test]
  fn relative_without_base_errors() {
    let err = resolve_url("foo", None).unwrap_err();
    assert!(matches!(err, UrlResolveError::RelativeUrlWithoutBase));
  }
}
