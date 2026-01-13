use crate::error::{Error, ResourceError, Result};
use crate::resource::{FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use crate::ui::TabId;
use crate::ui::protocol_limits::MAX_FAVICON_EDGE_PX;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

const DEFAULT_MAX_FAVICON_PNG_BYTES: usize = 32 * 1024;
const FAVICON_CONTENT_TYPE: &str = "image/png";

#[derive(Debug, Clone, Copy)]
pub struct ChromeDynamicAssetLimits {
  /// Maximum favicon width/height in pixels.
  pub max_favicon_edge_px: u32,
  /// Maximum encoded favicon PNG byte length.
  pub max_favicon_png_bytes: usize,
}

impl Default for ChromeDynamicAssetLimits {
  fn default() -> Self {
    Self {
      // Reuse the same edge limit enforced on untrusted favicon payloads coming from the UI↔worker
      // protocol so all favicon allocations stay in a small, predictable budget.
      max_favicon_edge_px: MAX_FAVICON_EDGE_PX,
      max_favicon_png_bytes: DEFAULT_MAX_FAVICON_PNG_BYTES,
    }
  }
}

#[derive(Clone)]
pub struct ChromeDynamicAssetFetcher {
  inner: Arc<dyn ResourceFetcher>,
  favicons: Arc<RwLock<HashMap<TabId, Vec<u8>>>>,
  limits: ChromeDynamicAssetLimits,
}

impl std::fmt::Debug for ChromeDynamicAssetFetcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChromeDynamicAssetFetcher")
      .field("limits", &self.limits)
      .field("favicon_count", &self.favicons.read().len())
      .finish_non_exhaustive()
  }
}

impl ChromeDynamicAssetFetcher {
  pub fn new(inner: Arc<dyn ResourceFetcher>) -> Self {
    Self::with_limits(inner, ChromeDynamicAssetLimits::default())
  }

  pub fn with_limits(inner: Arc<dyn ResourceFetcher>, limits: ChromeDynamicAssetLimits) -> Self {
    Self {
      inner,
      favicons: Arc::new(RwLock::new(HashMap::new())),
      limits,
    }
  }

  /// Returns the stable chrome URL used for a tab favicon.
  ///
  /// Canonical form: `chrome://favicon/<tab_id>`
  ///
  /// The fetcher also accepts the legacy `chrome://favicons/<tab_id>.png` form.
  pub fn favicon_url(tab_id: TabId) -> String {
    format!("chrome://favicon/{}", tab_id.0)
  }

  /// Insert/replace an encoded favicon PNG for a tab.
  ///
  /// This is intended to be called by the browser UI thread when it receives a favicon update from
  /// the render worker. Callers should pass the decoded dimensions for size enforcement (we avoid
  /// parsing PNG headers here).
  pub fn set_tab_favicon_png(
    &self,
    tab_id: TabId,
    png_bytes: Vec<u8>,
    width: u32,
    height: u32,
  ) -> Result<()> {
    self.ensure_favicon_dimensions(tab_id, width, height)?;
    self.ensure_favicon_png_size(tab_id, png_bytes.len())?;
    self.favicons.write().insert(tab_id, png_bytes);
    Ok(())
  }

  /// Convenience helper that encodes a premultiplied RGBA favicon buffer into PNG and stores it.
  pub fn set_tab_favicon_rgba(
    &self,
    tab_id: TabId,
    rgba_premultiplied: Vec<u8>,
    width: u32,
    height: u32,
  ) -> Result<()> {
    self.ensure_favicon_dimensions(tab_id, width, height)?;

    let expected_len = (width as usize)
      .checked_mul(height as usize)
      .and_then(|px| px.checked_mul(4))
      .ok_or_else(|| {
        Error::Resource(ResourceError::new(
          Self::favicon_url(tab_id),
          format!("favicon dimensions overflow ({width}x{height})"),
        ))
      })?;
    if rgba_premultiplied.len() != expected_len {
      return Err(Error::Resource(ResourceError::new(
        Self::favicon_url(tab_id),
        format!(
          "favicon RGBA length mismatch (expected {expected_len} bytes, got {})",
          rgba_premultiplied.len()
        ),
      )));
    }

    let Some(size) = tiny_skia::IntSize::from_wh(width, height) else {
      return Err(Error::Resource(ResourceError::new(
        Self::favicon_url(tab_id),
        format!("invalid favicon dimensions ({width}x{height})"),
      )));
    };
    let Some(pixmap) = tiny_skia::Pixmap::from_vec(rgba_premultiplied, size) else {
      return Err(Error::Resource(ResourceError::new(
        Self::favicon_url(tab_id),
        "invalid favicon pixel buffer".to_string(),
      )));
    };

    let png_bytes = crate::image_output::encode_image(&pixmap, crate::OutputFormat::Png)?;
    self.ensure_favicon_png_size(tab_id, png_bytes.len())?;
    self.favicons.write().insert(tab_id, png_bytes);
    Ok(())
  }

  pub fn clear_tab_favicon(&self, tab_id: TabId) {
    self.favicons.write().remove(&tab_id);
  }

  fn ensure_favicon_dimensions(&self, tab_id: TabId, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
      return Err(Error::Resource(ResourceError::new(
        Self::favicon_url(tab_id),
        format!("favicon dimensions are zero ({width}x{height})"),
      )));
    }

    if width > self.limits.max_favicon_edge_px || height > self.limits.max_favicon_edge_px {
      return Err(Error::Resource(ResourceError::new(
        Self::favicon_url(tab_id),
        format!(
          "favicon dimensions exceed limit ({}x{} > max edge {})",
          width, height, self.limits.max_favicon_edge_px
        ),
      )));
    }
    Ok(())
  }

  fn ensure_favicon_png_size(&self, tab_id: TabId, byte_len: usize) -> Result<()> {
    if byte_len > self.limits.max_favicon_png_bytes {
      return Err(Error::Resource(ResourceError::new(
        Self::favicon_url(tab_id),
        format!(
          "favicon PNG payload is {byte_len} bytes (limit {})",
          self.limits.max_favicon_png_bytes
        ),
      )));
    }
    Ok(())
  }

  fn try_fetch_favicon(&self, url: &str) -> Option<Result<FetchedResource>> {
    let tab_id = match parse_favicon_tab_id_from_url(url)? {
      Ok(tab_id) => tab_id,
      Err(err) => return Some(Err(err)),
    };

    let bytes = {
      let store = self.favicons.read();
      store.get(&tab_id).cloned()
    }
    .unwrap_or_else(|| transparent_png_bytes().clone());

    Some(Ok(FetchedResource::with_final_url(
      bytes,
      Some(FAVICON_CONTENT_TYPE.to_string()),
      Some(url.to_string()),
    )))
  }
}

impl ResourceFetcher for ChromeDynamicAssetFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if let Some(result) = self.try_fetch_favicon(url) {
      return result;
    }
    self.inner.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    if let Some(result) = self.try_fetch_favicon(req.url) {
      return result;
    }
    self.inner.fetch_with_request(req)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    if parse_favicon_tab_id_from_url(req.url).is_some() {
      return None;
    }
    self.inner.request_header_value(req, header_name)
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    self.inner.cookie_header_value(url)
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
    self.inner.store_cookie_from_document(url, cookie_string);
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    if let Some(result) = self.try_fetch_favicon(req.url) {
      let _ = (etag, last_modified);
      return result;
    }
    self
      .inner
      .fetch_with_request_and_validation(req, etag, last_modified)
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    if let Some(result) = self.try_fetch_favicon(req.fetch.url) {
      // Restrict to GET/HEAD for chrome favicon resources.
      if !(req.method.eq_ignore_ascii_case("GET") || req.method.eq_ignore_ascii_case("HEAD")) {
        return Err(Error::Resource(ResourceError::new(
          req.fetch.url,
          format!("unsupported method for chrome favicon: {}", req.method),
        )));
      }
      let mut res = result?;
      if req.method.eq_ignore_ascii_case("HEAD") {
        res.bytes.clear();
      }
      return Ok(res);
    }
    self.inner.fetch_http_request(req)
  }
}

fn transparent_png_bytes() -> &'static Vec<u8> {
  static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
  BYTES.get_or_init(|| {
    let pixmap = tiny_skia::Pixmap::new(1, 1).expect("1x1 pixmap"); // fastrender-allow-unwrap
    crate::image_output::encode_image(&pixmap, crate::OutputFormat::Png)
      .expect("encode 1x1 transparent png") // fastrender-allow-unwrap
  })
}

fn parse_favicon_tab_id_from_url(url: &str) -> Option<Result<TabId>> {
  let parsed = url::Url::parse(url).ok()?;
  if parsed.scheme() != "chrome" {
    return None;
  }
  let host = parsed.host_str()?;
  if host != "favicon" && host != "favicons" {
    return None;
  }

  // Keep chrome URL semantics strict and unambiguous (match `ChromeAssetsFetcher` invariants).
  if !parsed.username().is_empty()
    || parsed.password().is_some()
    || parsed.port().is_some()
    || parsed.query().is_some()
    || parsed.fragment().is_some()
  {
    return Some(Err(Error::Resource(ResourceError::new(
      url,
      "chrome favicon URL must not include credentials, ports, queries, or fragments",
    ))));
  }

  let path = parsed.path().trim_start_matches('/');
  if path.is_empty() {
    return Some(Err(Error::Resource(ResourceError::new(
      url,
      "chrome favicon URL missing tab id",
    ))));
  }
  // Only support the simplest stable form: a single path segment containing a numeric tab id.
  if path.contains('/') {
    return Some(Err(Error::Resource(ResourceError::new(
      url,
      "chrome favicon URL must be chrome://favicon/<tab_id> (single path segment)",
    ))));
  }
  let id_str = path.strip_suffix(".png").unwrap_or(path);
  if id_str.is_empty() || !id_str.bytes().all(|b| b.is_ascii_digit()) {
    return Some(Err(Error::Resource(ResourceError::new(
      url,
      "chrome favicon URL tab id must be a positive integer",
    ))));
  }
  let id = match id_str.parse::<u64>() {
    Ok(id) if id != 0 => id,
    _ => {
      return Some(Err(Error::Resource(ResourceError::new(
        url,
        "chrome favicon URL tab id is invalid",
      ))));
    }
  };
  Some(Ok(TabId(id)))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  #[derive(Clone)]
  struct PanicFetcher;

  impl ResourceFetcher for PanicFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      panic!("inner fetcher should not be called for {url}");
    }
  }

  fn tiny_png_bytes() -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(1, 1).expect("pixmap");
    // Premultiplied RGBA (opaque red).
    pixmap.data_mut().copy_from_slice(&[255, 0, 0, 255]);
    crate::image_output::encode_image(&pixmap, crate::OutputFormat::Png).expect("encode png")
  }

  #[test]
  fn insert_favicon_entry_and_fetch_it() {
    let inner: Arc<dyn ResourceFetcher> = Arc::new(PanicFetcher);
    let fetcher = ChromeDynamicAssetFetcher::new(inner);
    let tab_id = TabId(123);
    let png = tiny_png_bytes();

    fetcher
      .set_tab_favicon_png(tab_id, png.clone(), 1, 1)
      .expect("set favicon");

    let url = ChromeDynamicAssetFetcher::favicon_url(tab_id);
    let res = fetcher.fetch(&url).expect("fetch favicon");
    assert_eq!(res.content_type.as_deref(), Some("image/png"));
    assert_eq!(res.bytes, png);
  }

  #[test]
  fn fetch_supports_legacy_chrome_favicons_png_suffix() {
    let inner: Arc<dyn ResourceFetcher> = Arc::new(PanicFetcher);
    let fetcher = ChromeDynamicAssetFetcher::new(inner);
    let tab_id = TabId(123);
    let png = tiny_png_bytes();

    fetcher
      .set_tab_favicon_png(tab_id, png.clone(), 1, 1)
      .expect("set favicon");

    // Backwards-compatibility: support `chrome://favicons/<tab_id>.png` in addition to the
    // canonical `chrome://favicon/<tab_id>`.
    let res = fetcher
      .fetch("chrome://favicons/123.png")
      .expect("fetch favicon with legacy suffix");
    assert_eq!(res.content_type.as_deref(), Some("image/png"));
    assert_eq!(res.bytes, png);
  }

  #[test]
  fn missing_favicon_returns_transparent_png() {
    let inner: Arc<dyn ResourceFetcher> = Arc::new(PanicFetcher);
    let fetcher = ChromeDynamicAssetFetcher::new(inner);

    let res = fetcher
      .fetch("chrome://favicon/999")
      .expect("missing favicon should return transparent PNG");
    assert_eq!(res.content_type.as_deref(), Some("image/png"));
    assert!(res.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

    let decoder = png::Decoder::new(Cursor::new(&res.bytes));
    let mut reader = decoder.read_info().expect("png read_info");
    let out_size = reader.output_buffer_size().expect("png output buffer size");
    let mut buf = vec![0u8; out_size];
    let frame = reader.next_frame(&mut buf).expect("png decode");
    let data = &buf[..frame.buffer_size()];
    assert_eq!(frame.width, 1);
    assert_eq!(frame.height, 1);
    assert_eq!(data, &[0, 0, 0, 0]);
  }

  #[test]
  fn invalid_favicon_urls_error_without_panic() {
    let inner: Arc<dyn ResourceFetcher> = Arc::new(PanicFetcher);
    let fetcher = ChromeDynamicAssetFetcher::new(inner);

    for url in [
      "chrome://favicon/not-a-number",
      "chrome://favicon/0",
      "chrome://favicon/12/34",
      "chrome://favicon/123?query=1",
      "chrome://favicon/123#fragment",
      "chrome://favicon:80/123",
    ] {
      let err = fetcher.fetch(url).expect_err("invalid chrome favicon should error");
      let msg = err.to_string();
      assert!(
        msg.contains("favicon") || msg.contains("tab id") || msg.contains("tab"),
        "unexpected error message for {url}: {msg}"
      );
    }
  }
}
