//! `chrome://` scheme support for the trusted browser-chrome runtime.
//!
//! # Design decision: **Option B (dedicated fetcher)**
//!
//! We intentionally keep `chrome://` out of the generic [`crate::resource::ResourcePolicy`] /
//! [`crate::resource::AllowedSchemes`] surface. Instead, `chrome://` is only served by a dedicated
//! wrapper fetcher: [`TrustedChromeFetcher`].
//!
//! ## Threat model
//!
//! In the renderer-chrome architecture, there are two kinds of documents:
//!
//! - **Trusted chrome documents** (browser process): built-in UI pages controlled by us.
//! - **Untrusted web content** (renderer/sandboxed process): arbitrary HTML/CSS/JS from the network.
//!
//! `chrome://` resources are *privileged*:
//!
//! - They may include UI CSS, icons, scripts, etc.
//! - Future work may allow chrome documents to call privileged Rust APIs.
//!
//! Therefore, **untrusted content must never be able to navigate to or subresource-fetch
//! `chrome://...`**, including via indirect pathways like "open in new tab" IPC.
//!
//! ## Why a dedicated fetcher has the fewest footguns
//!
//! `ResourcePolicy` is used broadly across the codebase, and callers frequently start from
//! `AllowedSchemes::all()` for "normal web" behavior. If `chrome` became "just another scheme" in
//! `AllowedSchemes`, it would be easy to accidentally enable it for untrusted renderers by copying a
//! broad allowlist.
//!
//! With Option B:
//!
//! - The **default** fetcher stack does not understand `chrome://` at all, so it stays blocked even
//!   if someone relaxes `AllowedSchemes` for content.
//! - Only the trusted chrome runtime opts into `chrome://` by explicitly wrapping its fetcher with
//!   [`TrustedChromeFetcher`].
//! - The wrapper is small and auditable: it only serves a fixed, compile-time allowlist of assets
//!   (no filesystem access, no path traversal).
//!
//! This approach is intentionally boring: it minimises accidental privilege expansion.

use crate::error::{Error, ResourceError, Result};
use crate::resource::{FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use url::Url;

/// Returns true when `url` is a `chrome://...` URL (case-insensitive scheme).
pub fn is_chrome_url(url: &str) -> bool {
  let trimmed = url.trim_start();
  trimmed
    .as_bytes()
    .get(.."chrome:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"chrome:"))
}

/// A wrapper [`ResourceFetcher`] that serves built-in `chrome://` assets.
///
/// All non-`chrome://` requests are delegated to the wrapped `inner` fetcher.
///
/// # Security
///
/// Only use this fetcher in the trusted browser-chrome runtime. Do **not** use it for untrusted web
/// content.
#[derive(Clone)]
pub struct TrustedChromeFetcher {
  inner: std::sync::Arc<dyn ResourceFetcher>,
}

impl std::fmt::Debug for TrustedChromeFetcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TrustedChromeFetcher")
      .finish_non_exhaustive()
  }
}

impl TrustedChromeFetcher {
  pub fn new(inner: std::sync::Arc<dyn ResourceFetcher>) -> Self {
    Self { inner }
  }

  fn fetch_chrome_asset(&self, url: &str) -> Result<FetchedResource> {
    let parsed = Url::parse(url).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!("invalid chrome:// URL: {err}"),
      ))
    })?;
    if !parsed.scheme().eq_ignore_ascii_case("chrome") {
      return Err(Error::Resource(ResourceError::new(
        url,
        "not a chrome:// URL".to_string(),
      )));
    }

    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    // `Url::path()` always includes the leading `/`.
    let path = parsed.path();

    // `chrome://styles/chrome.css`
    if host == "styles" && path == "/chrome.css" {
      return Ok(FetchedResource::with_final_url(
        include_bytes!("../../assets/chrome/styles/chrome.css").to_vec(),
        Some("text/css; charset=utf-8".to_string()),
        Some(parsed.to_string()),
      ));
    }

    // `chrome://icons/<name>.svg`
    if host == "icons" {
      let name = path.strip_prefix('/').unwrap_or(path);
      if let Some(bytes) = chrome_icon_bytes(name) {
        return Ok(FetchedResource::with_final_url(
          bytes.to_vec(),
          Some("image/svg+xml".to_string()),
          Some(parsed.to_string()),
        ));
      }
    }

    Err(Error::Resource(ResourceError::new(
      url,
      format!("unknown chrome:// asset: chrome://{host}{path}"),
    )))
  }
}

impl ResourceFetcher for TrustedChromeFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if is_chrome_url(url) {
      return self.fetch_chrome_asset(url);
    }
    self.inner.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    if is_chrome_url(req.url) {
      return self.fetch_chrome_asset(req.url);
    }
    self.inner.fetch_with_request(req)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    if is_chrome_url(req.url) {
      // No outbound request headers for built-in assets.
      return Some(String::new());
    }
    self.inner.request_header_value(req, header_name)
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    if is_chrome_url(url) {
      return Some(String::new());
    }
    self.inner.cookie_header_value(url)
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
    if is_chrome_url(url) {
      return;
    }
    self.inner.store_cookie_from_document(url, cookie_string)
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    if is_chrome_url(req.fetch.url) {
      // We only support built-in asset GET semantics. Treat everything else as a fetch error.
      if req.method.eq_ignore_ascii_case("GET") {
        return self.fetch_chrome_asset(req.fetch.url);
      }
      return Err(Error::Resource(ResourceError::new(
        req.fetch.url,
        "chrome:// assets do not support non-GET HTTP methods".to_string(),
      )));
    }
    self.inner.fetch_http_request(req)
  }
}

fn chrome_icon_bytes(name: &str) -> Option<&'static [u8]> {
  match name {
    "appearance.svg" => Some(include_bytes!("../../assets/browser_icons/appearance.svg")),
    "arrow_down.svg" => Some(include_bytes!("../../assets/browser_icons/arrow_down.svg")),
    "arrow_up.svg" => Some(include_bytes!("../../assets/browser_icons/arrow_up.svg")),
    "back.svg" => Some(include_bytes!("../../assets/browser_icons/back.svg")),
    "bookmark_filled.svg" => Some(include_bytes!(
      "../../assets/browser_icons/bookmark_filled.svg"
    )),
    "bookmark_outline.svg" => Some(include_bytes!(
      "../../assets/browser_icons/bookmark_outline.svg"
    )),
    "check.svg" => Some(include_bytes!("../../assets/browser_icons/check.svg")),
    "close_tab.svg" => Some(include_bytes!("../../assets/browser_icons/close_tab.svg")),
    "copy.svg" => Some(include_bytes!("../../assets/browser_icons/copy.svg")),
    "download.svg" => Some(include_bytes!("../../assets/browser_icons/download.svg")),
    "edit.svg" => Some(include_bytes!("../../assets/browser_icons/edit.svg")),
    "error.svg" => Some(include_bytes!("../../assets/browser_icons/error.svg")),
    "folder.svg" => Some(include_bytes!("../../assets/browser_icons/folder.svg")),
    "forward.svg" => Some(include_bytes!("../../assets/browser_icons/forward.svg")),
    "history.svg" => Some(include_bytes!("../../assets/browser_icons/history.svg")),
    "home.svg" => Some(include_bytes!("../../assets/browser_icons/home.svg")),
    "info.svg" => Some(include_bytes!("../../assets/browser_icons/info.svg")),
    "lock_secure.svg" => Some(include_bytes!("../../assets/browser_icons/lock_secure.svg")),
    "menu.svg" => Some(include_bytes!("../../assets/browser_icons/menu.svg")),
    "new_tab.svg" => Some(include_bytes!("../../assets/browser_icons/new_tab.svg")),
    "open_in_new_tab.svg" => Some(include_bytes!(
      "../../assets/browser_icons/open_in_new_tab.svg"
    )),
    "plus.svg" => Some(include_bytes!("../../assets/browser_icons/plus.svg")),
    "reload.svg" => Some(include_bytes!("../../assets/browser_icons/reload.svg")),
    "search.svg" => Some(include_bytes!("../../assets/browser_icons/search.svg")),
    "spinner.svg" => Some(include_bytes!("../../assets/browser_icons/spinner.svg")),
    "stop_loading.svg" => Some(include_bytes!(
      "../../assets/browser_icons/stop_loading.svg"
    )),
    "tab.svg" => Some(include_bytes!("../../assets/browser_icons/tab.svg")),
    "trash.svg" => Some(include_bytes!("../../assets/browser_icons/trash.svg")),
    "warning_insecure.svg" => Some(include_bytes!(
      "../../assets/browser_icons/warning_insecure.svg"
    )),
    "zoom_in.svg" => Some(include_bytes!("../../assets/browser_icons/zoom_in.svg")),
    "zoom_out.svg" => Some(include_bytes!("../../assets/browser_icons/zoom_out.svg")),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::FastRender;
  use crate::resource::HttpFetcher;
  use std::sync::Arc;

  #[test]
  fn trusted_fetcher_serves_chrome_css() {
    let inner = Arc::new(HttpFetcher::new()) as Arc<dyn ResourceFetcher>;
    let fetcher = TrustedChromeFetcher::new(inner);
    let res = fetcher
      .fetch("chrome://styles/chrome.css")
      .expect("expected css asset");
    assert!(std::str::from_utf8(&res.bytes)
      .unwrap()
      .contains("FastRender internal browser chrome"));
    assert!(
      res
        .content_type
        .as_deref()
        .unwrap_or("")
        .starts_with("text/css"),
      "expected text/css content type, got {:?}",
      res.content_type
    );
  }

  #[test]
  fn chrome_css_applies_when_rendering_with_trusted_fetcher() {
    let inner = Arc::new(HttpFetcher::new()) as Arc<dyn ResourceFetcher>;
    let fetcher = Arc::new(TrustedChromeFetcher::new(inner)) as Arc<dyn ResourceFetcher>;
    let mut renderer = FastRender::builder()
      .base_url("chrome://test/")
      .fetcher(fetcher)
      .build()
      .expect("build renderer");
    // Regression test: ensure the stylesheet is actually fetched + applied.
    //
    // The UA stylesheet sets `body { margin: 8px; }`. The chrome stylesheet zeros that out, so the
    // top-left pixel should be inside the first child div (red) rather than the html background
    // (green).
    let html = r#"<!doctype html>
      <html>
        <head>
          <link rel="stylesheet" href="chrome://styles/chrome.css">
          <style>
            html { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body><div style="width: 8px; height: 8px; background: rgb(255, 0, 0);"></div></body>
      </html>"#;
    let pixmap = renderer.render_html(html, 8, 8).expect("render");
    // Premultiplied RGBA.
    let px = &pixmap.data()[0..4];
    assert_eq!(px, &[255, 0, 0, 255]);
  }

  #[test]
  fn is_chrome_url_recognizes_case_insensitive_scheme() {
    assert!(is_chrome_url("chrome://styles/chrome.css"));
    assert!(is_chrome_url("Chrome://styles/chrome.css"));
    assert!(is_chrome_url("   cHrOmE://styles/chrome.css"));
    assert!(!is_chrome_url("https://example.com/"));
  }

  #[test]
  fn untrusted_http_document_with_chrome_img_does_not_crash() {
    // Regression test: untrusted content must not be able to load `chrome://` subresources, but
    // encountering such a URL should be non-fatal (missing image, no crash).
    let mut renderer = FastRender::builder()
      .base_url("https://example.com/")
      .build()
      .expect("build renderer");
    let html = r#"<!doctype html>
      <html>
        <body>
          <img src="chrome://icons/back.svg">
        </body>
      </html>"#;
    renderer
      .render_html(html, 16, 16)
      .expect("render should succeed even if chrome:// subresource is blocked");
  }
}
