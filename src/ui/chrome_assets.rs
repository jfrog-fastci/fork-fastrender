use crate::error::{Error, ResourceError, Result};
use crate::resource::{FetchedResource, ResourceFetcher};
use percent_encoding::percent_decode_str;

const CHROME_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/chrome/chrome.css"));
const CHROME_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/chrome/chrome.js"));
const ABOUT_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/chrome/about.css"));

#[derive(Debug, Clone, Copy, Default)]
pub struct ChromeAssetsFetcher;

impl ChromeAssetsFetcher {
  pub fn new() -> Self {
    Self
  }
}

#[derive(Debug, Clone, Copy)]
struct ParsedChromeUrl<'a> {
  host: &'a str,
  path: &'a str,
}

fn decoded_equals_dot_segment(segment: &str) -> bool {
  let decoded = percent_decode_str(segment).decode_utf8_lossy();
  decoded == "." || decoded == ".."
}

fn parse_chrome_url(url: &str) -> Result<ParsedChromeUrl<'_>> {
  let Some(scheme) = url.get(..7) else {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URL too short",
    )));
  };
  if !scheme.eq_ignore_ascii_case("chrome:") {
    return Err(Error::Resource(ResourceError::new(
      url,
      "ChromeAssetsFetcher only supports chrome:// URLs",
    )));
  }

  let after_scheme = &url[7..];
  if !after_scheme.starts_with("//") {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URLs must be absolute (chrome://...)",
    )));
  }

  let after_slashes = &after_scheme[2..];
  if after_slashes.is_empty() {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URL missing host segment",
    )));
  }

  // Split the authority and path.
  let authority_end = after_slashes
    .find(['/', '?', '#'])
    .unwrap_or(after_slashes.len());
  let host = &after_slashes[..authority_end];
  let remainder = &after_slashes[authority_end..];

  if host.is_empty() {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URL missing host segment",
    )));
  }
  if decoded_equals_dot_segment(host) {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URL rejected: host must not be '.' or '..'",
    )));
  }
  // Disallow credentials, ports, etc. We only support a small allowlist and want to keep the URL
  // format unambiguous.
  if host.contains('@') || host.contains(':') {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URLs must not include credentials or ports",
    )));
  }

  if remainder.contains('?') || remainder.contains('#') {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URLs must not include query strings or fragments",
    )));
  }

  let path = if remainder.is_empty() { "/" } else { remainder };
  if !path.starts_with('/') {
    return Err(Error::Resource(ResourceError::new(
      url,
      "chrome asset URLs must use an absolute path",
    )));
  }

  // Reject dot-segment traversal attempts, even if they wouldn't match the allowlist.
  for segment in path.split('/') {
    if segment.is_empty() {
      continue;
    }
    if decoded_equals_dot_segment(segment) {
      return Err(Error::Resource(ResourceError::new(
        url,
        "chrome asset URL rejected: path traversal is not allowed",
      )));
    }
  }

  Ok(ParsedChromeUrl { host, path })
}

impl ResourceFetcher for ChromeAssetsFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let parsed = parse_chrome_url(url)?;
    let host = parsed.host.to_ascii_lowercase();

    match (host.as_str(), parsed.path) {
      ("styles", "/about.css") => Ok(FetchedResource::with_final_url(
        ABOUT_CSS.as_bytes().to_vec(),
        Some("text/css".to_string()),
        Some(url.to_string()),
      )),
      ("styles", "/chrome.css") => Ok(FetchedResource::with_final_url(
        CHROME_CSS.as_bytes().to_vec(),
        Some("text/css".to_string()),
        Some(url.to_string()),
      )),
      ("scripts", "/chrome.js") => Ok(FetchedResource::with_final_url(
        CHROME_JS.as_bytes().to_vec(),
        Some("text/javascript".to_string()),
        Some(url.to_string()),
      )),
      _ => Err(Error::Resource(ResourceError::new(
        url,
        "unknown chrome:// asset (allowed: chrome://styles/about.css, chrome://styles/chrome.css, chrome://scripts/chrome.js)",
      ))),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::ChromeAssetsFetcher;
  use crate::resource::ResourceFetcher;

  #[test]
  fn fetch_allowlisted_chrome_css() {
    let fetcher = ChromeAssetsFetcher::new();
    let url = "chrome://styles/chrome.css";
    let res = fetcher.fetch(url).expect("fetch chrome.css");
    assert!(
      !res.bytes.is_empty(),
      "chrome.css should not be empty (embedded test asset)"
    );
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
    assert_eq!(res.final_url.as_deref(), Some(url));
  }

  #[test]
  fn fetch_allowlisted_chrome_js() {
    let fetcher = ChromeAssetsFetcher::new();
    let url = "chrome://scripts/chrome.js";
    let res = fetcher.fetch(url).expect("fetch chrome.js");
    assert!(
      !res.bytes.is_empty(),
      "chrome.js should not be empty (embedded test asset)"
    );
    assert_eq!(res.content_type.as_deref(), Some("text/javascript"));
    assert_eq!(res.final_url.as_deref(), Some(url));
  }

  #[test]
  fn reject_unknown_chrome_url() {
    let fetcher = ChromeAssetsFetcher::new();
    let err = fetcher
      .fetch("chrome://styles/unknown.css")
      .expect_err("unknown chrome asset should error");
    let msg = err.to_string();
    assert!(
      msg.contains("unknown chrome:// asset"),
      "unexpected error message: {msg}"
    );
  }

  #[test]
  fn reject_path_traversal_attempt() {
    let fetcher = ChromeAssetsFetcher::new();
    let err = fetcher
      .fetch("chrome://styles/../scripts/chrome.js")
      .expect_err("path traversal should error");
    let msg = err.to_string();
    assert!(
      msg.contains("path traversal"),
      "unexpected error message: {msg}"
    );
  }

  #[test]
  fn reject_non_absolute_chrome_url() {
    let fetcher = ChromeAssetsFetcher::new();
    let err = fetcher
      .fetch("chrome:scripts/chrome.js")
      .expect_err("non-absolute chrome URL should error");
    let msg = err.to_string();
    assert!(
      msg.contains("absolute"),
      "unexpected error message: {msg}"
    );
  }
}
