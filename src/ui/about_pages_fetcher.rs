use crate::error::{Error, ResourceError, Result};
use crate::resource::{FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use crate::ui::chrome_assets::ChromeAssetsFetcher;
use std::sync::Arc;
use url::Url;

#[derive(Clone)]
pub(crate) struct AboutPagesCompositeFetcher {
  default: Arc<dyn ResourceFetcher>,
  chrome: ChromeAssetsFetcher,
}

impl AboutPagesCompositeFetcher {
  pub(crate) fn new(default: Arc<dyn ResourceFetcher>) -> Self {
    Self {
      default,
      chrome: ChromeAssetsFetcher::new(),
    }
  }

  fn is_allowed_chrome_request(&self, req: &FetchRequest<'_>) -> bool {
    if req
      .client_origin
      .is_some_and(|origin| origin.scheme().eq_ignore_ascii_case("about"))
    {
      return true;
    }

    // Some call sites may not carry `client_origin` but still provide a referrer URL. Treat an
    // `about:` referrer as sufficient to allow internal chrome assets.
    if let Some(referrer) = req.referrer_url {
      // Avoid allocations by using a cheap prefix check first.
      if referrer
        .trim_start()
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("about:"))
      {
        return true;
      }
      if let Ok(parsed) = Url::parse(referrer) {
        if parsed.scheme().eq_ignore_ascii_case("about") {
          return true;
        }
      }
    }

    false
  }

  fn fetch_chrome(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    if !self.is_allowed_chrome_request(&req) {
      let origin = req
        .client_origin
        .map(|o| o.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
      return Err(Error::Resource(ResourceError::new(
        req.url,
        format!("blocked chrome:// subresource fetch from origin {origin}"),
      )));
    }

    self.chrome.fetch(req.url)
  }
}

impl ResourceFetcher for AboutPagesCompositeFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    // Without request metadata we cannot safely determine whether this `chrome://` request was
    // initiated by an `about:` document, so fail closed.
    if url.trim_start().get(..9).is_some_and(|p| p.eq_ignore_ascii_case("chrome://")) {
      return Err(Error::Resource(ResourceError::new(
        url,
        "blocked chrome:// fetch without an initiating about: origin".to_string(),
      )));
    }
    self.default.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    if req
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return self.fetch_chrome(req);
    }
    self.default.fetch_with_request(req)
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    if req
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      let _ = (etag, last_modified);
      return self.fetch_chrome(req);
    }
    self
      .default
      .fetch_with_request_and_validation(req, etag, last_modified)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    if req
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return None;
    }
    self.default.request_header_value(req, header_name)
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    if url.trim_start().get(..9).is_some_and(|p| p.eq_ignore_ascii_case("chrome://")) {
      return Some(String::new());
    }
    self.default.cookie_header_value(url)
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
    if url.trim_start().get(..9).is_some_and(|p| p.eq_ignore_ascii_case("chrome://")) {
      return;
    }
    self.default.store_cookie_from_document(url, cookie_string)
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    if req
      .fetch
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      // Only allow `GET`/`HEAD`-style chrome fetches, mirroring `ResourceFetcher::fetch_http_request`
      // default behavior.
      if !req.method.eq_ignore_ascii_case("GET") && !req.method.eq_ignore_ascii_case("HEAD") {
        return Err(Error::Resource(ResourceError::new(
          req.fetch.url,
          "blocked non-GET chrome:// request".to_string(),
        )));
      }
      let mut res = self.fetch_chrome(req.fetch)?;
      if req.method.eq_ignore_ascii_case("HEAD") {
        res.bytes.clear();
      }
      return Ok(res);
    }
    self.default.fetch_http_request(req)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::{origin_from_url, FetchDestination};
  use crate::ui::about_pages;

  #[test]
  fn chrome_stylesheet_allowed_from_about_origin() {
    let inner = Arc::new(crate::resource::HttpFetcher::new());
    let fetcher = AboutPagesCompositeFetcher::new(inner);
    let origin = origin_from_url("about:newtab").expect("origin");
    let req = FetchRequest::new(about_pages::ABOUT_SHARED_CSS_URL, FetchDestination::Style)
      .with_client_origin(&origin);
    let res = fetcher.fetch_with_request(req).expect("fetch");
    let text = std::str::from_utf8(&res.bytes).expect("utf-8");
    assert!(
      text.contains("FASTR_ABOUT_SHARED_CSS"),
      "expected shared CSS marker in chrome:// stylesheet"
    );
  }

  #[test]
  fn chrome_stylesheet_blocked_from_https_origin() {
    let inner = Arc::new(crate::resource::HttpFetcher::new());
    let fetcher = AboutPagesCompositeFetcher::new(inner);
    let origin = origin_from_url("https://example.com/").expect("origin");
    let req = FetchRequest::new(about_pages::ABOUT_SHARED_CSS_URL, FetchDestination::Style)
      .with_client_origin(&origin);
    let err = fetcher
      .fetch_with_request(req)
      .expect_err("expected chrome fetch to be blocked");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
      msg.contains("blocked") && msg.contains("chrome"),
      "expected error to mention blocked chrome fetch, got: {msg}"
    );
  }
}
