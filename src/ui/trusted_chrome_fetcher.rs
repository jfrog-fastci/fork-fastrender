use crate::error::{Error, ResourceError, Result};
use crate::resource::{data_url::decode_data_url, FetchedResource, ResourceFetcher};
use super::chrome_assets::ChromeAssetsFetcher;
use super::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
use std::sync::Arc;
use url::Url;

const CHROME_URL_PREFIX: &str = "chrome://";

fn is_html_ascii_whitespace_char(value: char) -> bool {
  matches!(
    value,
    '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '
  )
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_html_ascii_whitespace_char)
}

fn has_case_insensitive_prefix(value: &str, prefix: &str) -> bool {
  value
    .as_bytes()
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn is_chrome_url(url: &str) -> bool {
  has_case_insensitive_prefix(url, CHROME_URL_PREFIX)
}

fn blocked(url: &str, reason: impl Into<String>) -> Error {
  Error::Resource(ResourceError::new(
    url.to_string(),
    format!("blocked by trusted chrome fetcher: {}", reason.into()),
  ))
}

/// A strict policy wrapper for renderer-chrome HTML/JS that must never implicitly fetch arbitrary
/// network or filesystem resources.
///
/// Allowed URL schemes:
/// - `chrome://...` (delegated to the wrapped chrome asset fetcher)
/// - `data:` (decoded inline via the shared data URL helper)
///
/// Everything else (including `http`, `https`, `file`, and relative URLs) is rejected with a clear,
/// actionable error.
#[derive(Clone)]
pub struct TrustedChromeFetcher {
  inner: Arc<dyn ResourceFetcher>,
}

impl TrustedChromeFetcher {
  pub fn new(inner: Arc<dyn ResourceFetcher>) -> Self {
    Self { inner }
  }
}

/// Construct the default trusted-chrome fetcher stack used by renderer-chrome documents.
///
/// This pairs the strict [`TrustedChromeFetcher`] policy wrapper with the built-in
/// [`ChromeAssetsFetcher`] (and the favicon-capable [`ChromeDynamicAssetFetcher`]) so chrome HTML
/// can load `chrome://...` assets and inline `data:` URLs, but cannot accidentally fetch arbitrary
/// network or filesystem resources.
pub fn trusted_chrome_fetcher() -> Arc<dyn ResourceFetcher> {
  let assets: Arc<dyn ResourceFetcher> = Arc::new(ChromeAssetsFetcher::new());
  let dynamic: Arc<dyn ResourceFetcher> = Arc::new(ChromeDynamicAssetFetcher::new(assets));
  Arc::new(TrustedChromeFetcher::new(dynamic))
}

impl ResourceFetcher for TrustedChromeFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let url = trim_ascii_whitespace(url);
    if url.is_empty() {
      return Err(blocked(url, "empty URL"));
    }

    if is_chrome_url(url) {
      return self.inner.fetch(url);
    }

    if crate::resource::is_data_url(url) {
      return decode_data_url(url);
    }

    // Provide actionable diagnostics for common cases (http/https/file/relative).
    if has_case_insensitive_prefix(url, "http://") {
      return Err(blocked(
        url,
        "http:// URL is not allowed (network access is disabled)",
      ));
    }
    if has_case_insensitive_prefix(url, "https://") {
      return Err(blocked(
        url,
        "https:// URL is not allowed (network access is disabled)",
      ));
    }
    if has_case_insensitive_prefix(url, "file://") {
      return Err(blocked(
        url,
        "file:// URL is not allowed (filesystem access is disabled)",
      ));
    }

    match Url::parse(url) {
      Ok(parsed) => {
        if parsed.scheme().eq_ignore_ascii_case("chrome") {
          return Err(blocked(url, "chrome URL must use the chrome:// scheme"));
        }
        Err(blocked(
          url,
          format!(
            "URL scheme {:?} is not allowed (only chrome:// and data: are permitted)",
            parsed.scheme()
          ),
        ))
      }
      Err(_) => Err(blocked(
        url,
        "relative URL is not allowed (only chrome:// and data: are permitted)",
      )),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::FetchedResource;

  #[derive(Default)]
  struct DummyChromeAssetFetcher;

  impl ResourceFetcher for DummyChromeAssetFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      if url == "chrome://styles/chrome.css" {
        return Ok(FetchedResource::new(
          b"body { background: red; }".to_vec(),
          Some("text/css".to_string()),
        ));
      }
      Err(Error::Resource(ResourceError::new(
        url.to_string(),
        "asset not found".to_string(),
      )))
    }
  }

  #[test]
  fn chrome_url_succeeds() {
    let fetcher = TrustedChromeFetcher::new(Arc::new(DummyChromeAssetFetcher::default()));
    let res = fetcher
      .fetch("chrome://styles/chrome.css")
      .expect("chrome url should be allowed");
    assert!(!res.bytes.is_empty());
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
  }

  #[test]
  fn data_url_succeeds() {
    #[derive(Default)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        panic!("inner fetcher should not be called for data: URLs (got {url})");
      }
    }

    let fetcher = TrustedChromeFetcher::new(Arc::new(PanicFetcher::default()));
    let res = fetcher
      .fetch("data:text/plain;base64,aGk=")
      .expect("data url should be allowed");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn https_url_is_blocked() {
    let fetcher = TrustedChromeFetcher::new(Arc::new(DummyChromeAssetFetcher::default()));
    let err = fetcher
      .fetch("https://example.com/")
      .expect_err("https URL should be blocked");
    let msg = err.to_string();
    assert!(
      msg.contains("blocked by trusted chrome fetcher"),
      "unexpected error message: {msg}"
    );
    assert!(
      msg.contains("https://example.com/"),
      "expected URL in error message: {msg}"
    );
  }
}
