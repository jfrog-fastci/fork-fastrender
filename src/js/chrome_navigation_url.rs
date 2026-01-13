//! Host-side URL validation for privileged `chrome.navigation.navigate(url)`.
//!
//! The privileged JS bindings are implemented in `src/js/vmjs/chrome_api.rs` and are re-exported
//! under `crate::js::chrome_api`. This module contains shared URL validation helpers that embedders
//! can use before honoring a navigation request.

use std::fmt;
use url::Url;

use super::vmjs_chrome_api::MAX_CHROME_API_URL_CODE_UNITS;

/// Maximum length allowed for `chrome.navigation.navigate(url)` URLs, measured in UTF-16 code units.
///
/// This is enforced in the native `vm-js` binding *before* converting the JS string to a Rust
/// `String`, preventing hostile pages from forcing large allocations outside the VM heap.
pub const MAX_CHROME_NAVIGATION_URL_CODE_UNITS: usize = MAX_CHROME_API_URL_CODE_UNITS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeApiError {
  /// The URL is empty (`""`).
  EmptyUrl,
  /// The URL exceeds [`MAX_CHROME_NAVIGATION_URL_CODE_UNITS`] UTF-16 code units.
  UrlTooLong,
  /// The URL failed to parse.
  InvalidUrl(String),
  /// The URL parsed, but its scheme is not allowed for chrome-driven navigation.
  RejectedScheme(String),
}

impl fmt::Display for ChromeApiError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ChromeApiError::EmptyUrl => write!(f, "Navigation URL cannot be empty"),
      ChromeApiError::UrlTooLong => write!(
        f,
        "Navigation URL is too long (max {} UTF-16 code units)",
        MAX_CHROME_NAVIGATION_URL_CODE_UNITS
      ),
      ChromeApiError::InvalidUrl(reason) => write!(f, "Invalid navigation URL: {reason}"),
      ChromeApiError::RejectedScheme(scheme) => write!(
        f,
        "Navigation to {scheme}: URLs is not allowed"
      ),
    }
  }
}

impl std::error::Error for ChromeApiError {}

/// Validate a URL string intended for `chrome.navigation.navigate(url)`.
///
/// Callers should trim HTML ASCII whitespace (U+0009, U+000A, U+000C, U+000D, U+0020) prior to
/// calling this function.
pub fn validate_chrome_navigation_url(url: &str) -> Result<(), ChromeApiError> {
  if url.is_empty() {
    return Err(ChromeApiError::EmptyUrl);
  }

  // Bound the UTF-16 length. This iterator stops after `MAX + 1` code units, so it is safe even for
  // extremely large inputs.
  if url
    .encode_utf16()
    .take(MAX_CHROME_NAVIGATION_URL_CODE_UNITS + 1)
    .count()
    > MAX_CHROME_NAVIGATION_URL_CODE_UNITS
  {
    return Err(ChromeApiError::UrlTooLong);
  }

  let parsed = Url::parse(url).map_err(|err| ChromeApiError::InvalidUrl(err.to_string()))?;
  match parsed.scheme() {
    "http" | "https" | "file" | "about" => Ok(()),
    other => Err(ChromeApiError::RejectedScheme(other.to_string())),
  }
}
