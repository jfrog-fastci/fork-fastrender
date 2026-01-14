use crate::error::{Error, ResourceError, Result};
use crate::resource::{FetchedResource, ResourceFetcher};
use percent_encoding::percent_decode_str;

const CHROME_CSS: &str =
  include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/chrome/chrome.css"));
const ABOUT_CSS: &str =
  include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/chrome/about.css"));
const CHROME_JS: &str =
  include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/chrome/chrome.js"));
const SVG_MIME: &str = "image/svg+xml";

fn chrome_icon_bytes(path: &str) -> Option<&'static [u8]> {
  Some(match path {
    "/appearance.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/appearance.svg"
    )),
    "/arrow_down.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/arrow_down.svg"
    )),
    "/arrow_up.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/arrow_up.svg"
    )),
    "/back.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/back.svg"
    )),
    "/bookmark_filled.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/bookmark_filled.svg"
    )),
    "/bookmark_outline.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/bookmark_outline.svg"
    )),
    "/check.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/check.svg"
    )),
    "/close_tab.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/close_tab.svg"
    )),
    "/copy.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/copy.svg"
    )),
    "/download.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/download.svg"
    )),
    "/edit.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/edit.svg"
    )),
    "/error.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/error.svg"
    )),
    "/folder.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/folder.svg"
    )),
    "/forward.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/forward.svg"
    )),
    "/fullscreen.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/fullscreen.svg"
    )),
    "/fullscreen_exit.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/fullscreen_exit.svg"
    )),
    "/history.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/history.svg"
    )),
    "/home.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/home.svg"
    )),
    "/info.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/info.svg"
    )),
    "/lock_secure.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/lock_secure.svg"
    )),
    "/menu.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/menu.svg"
    )),
    "/mute.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/mute.svg"
    )),
    "/new_tab.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/new_tab.svg"
    )),
    "/open_in_new_tab.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/open_in_new_tab.svg"
    )),
    "/pause.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/pause.svg"
    )),
    "/play.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/play.svg"
    )),
    "/plus.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/plus.svg"
    )),
    "/reload.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/reload.svg"
    )),
    "/search.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/search.svg"
    )),
    "/spinner.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/spinner.svg"
    )),
    "/stop_loading.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/stop_loading.svg"
    )),
    "/tab.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/tab.svg"
    )),
    "/trash.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/trash.svg"
    )),
    "/volume.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/volume.svg"
    )),
    "/warning_insecure.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/warning_insecure.svg"
    )),
    "/zoom_in.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/zoom_in.svg"
    )),
    "/zoom_out.svg" => include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/assets/browser_icons/zoom_out.svg"
    )),
    _ => return None,
  })
}

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

fn trim_ascii_whitespace(value: &str) -> &str {
  // Match HTML URL-ish attribute whitespace rules (TAB/LF/FF/CR/SPACE).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn parse_chrome_url(url: &str) -> Result<ParsedChromeUrl<'_>> {
  let url = trim_ascii_whitespace(url);
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
    let url = trim_ascii_whitespace(url);
    let parsed = parse_chrome_url(url)?;
    let host = parsed.host.to_ascii_lowercase();
    let canonical_url = format!("chrome://{host}{}", parsed.path);

    match (host.as_str(), parsed.path) {
      ("styles", "/chrome.css") => Ok(FetchedResource::with_final_url(
        CHROME_CSS.as_bytes().to_vec(),
        Some("text/css".to_string()),
        Some(canonical_url.clone()),
      )),
      ("styles", "/about.css") => Ok(FetchedResource::with_final_url(
        ABOUT_CSS.as_bytes().to_vec(),
        Some("text/css".to_string()),
        Some(canonical_url.clone()),
      )),
      ("icons", path) => {
        let Some(bytes) = chrome_icon_bytes(path) else {
          return Err(Error::Resource(ResourceError::new(
            url,
            format!("unknown chrome:// icon: chrome://icons{path}"),
          )));
        };
        Ok(FetchedResource::with_final_url(
          bytes.to_vec(),
          Some(SVG_MIME.to_string()),
          Some(canonical_url.clone()),
        ))
      }
      ("scripts", "/chrome.js") => Ok(FetchedResource::with_final_url(
        CHROME_JS.as_bytes().to_vec(),
        Some("text/javascript".to_string()),
        Some(canonical_url),
      )),
      _ => Err(Error::Resource(ResourceError::new(
        url,
        "unknown chrome:// asset (allowed: chrome://styles/chrome.css, chrome://styles/about.css, chrome://scripts/chrome.js, chrome://icons/<name>.svg)",
      ))),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{ChromeAssetsFetcher, SVG_MIME};
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
  fn fetch_chrome_css_trims_ascii_whitespace() {
    let fetcher = ChromeAssetsFetcher::new();
    let url = " \nchrome://styles/chrome.css\t";
    let res = fetcher.fetch(url).expect("fetch chrome.css with whitespace");
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
    assert_eq!(res.final_url.as_deref(), Some("chrome://styles/chrome.css"));
  }

  #[test]
  fn fetch_chrome_css_canonicalizes_scheme_and_host_case() {
    let fetcher = ChromeAssetsFetcher::new();
    let res = fetcher
      .fetch("CHROME://STYLES/chrome.css")
      .expect("fetch chrome.css with mixed-case URL");
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
    assert_eq!(res.final_url.as_deref(), Some("chrome://styles/chrome.css"));
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
  fn fetch_allowlisted_about_css() {
    let fetcher = ChromeAssetsFetcher::new();
    let url = "chrome://styles/about.css";
    let res = fetcher.fetch(url).expect("fetch about.css");
    assert!(
      !res.bytes.is_empty(),
      "about.css should not be empty (embedded test asset)"
    );
    let text = std::str::from_utf8(&res.bytes).expect("about.css should be UTF-8");
    assert!(
      text.contains("FASTR_ABOUT_SHARED_CSS"),
      "expected FASTR_ABOUT_SHARED_CSS marker in about.css"
    );
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
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
    assert!(
      msg.contains("chrome://icons/<name>.svg"),
      "expected supported chrome:// patterns in error message: {msg}"
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
  fn fetch_allowlisted_chrome_icon() {
    let fetcher = ChromeAssetsFetcher::new();
    let url = "chrome://icons/back.svg";
    let res = fetcher.fetch(url).expect("fetch chrome icon");
    assert!(
      !res.bytes.is_empty(),
      "chrome icon bytes should not be empty (embedded test asset)"
    );
    assert_eq!(res.content_type.as_deref(), Some(SVG_MIME));
    assert_eq!(res.final_url.as_deref(), Some(url));
  }

  #[test]
  fn reject_unknown_chrome_icon() {
    let fetcher = ChromeAssetsFetcher::new();
    let err = fetcher
      .fetch("chrome://icons/does_not_exist.svg")
      .expect_err("unknown chrome icon should error");
    let msg = err.to_string();
    assert!(
      msg.contains("unknown chrome:// icon"),
      "unexpected error message: {msg}"
    );
  }

  #[test]
  fn reject_icon_path_traversal_attempt() {
    let fetcher = ChromeAssetsFetcher::new();
    assert!(
      fetcher.fetch("chrome://icons/../styles/chrome.css").is_err(),
      "expected dot-segment traversal to error"
    );
    assert!(
      fetcher.fetch("chrome://icons/%2e%2e/styles/chrome.css").is_err(),
      "expected percent-encoded dot-segment traversal to error"
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
