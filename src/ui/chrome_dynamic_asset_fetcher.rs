use crate::error::{Error, ResourceError, Result};
use crate::resource::{FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use crate::ui::TabId;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

const DEFAULT_MAX_FAVICON_EDGE_PX: u32 = 64;
const DEFAULT_MAX_FAVICON_PNG_BYTES: usize = 32 * 1024;

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
      max_favicon_edge_px: DEFAULT_MAX_FAVICON_EDGE_PX,
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
  /// Example: `chrome://favicons/123.png`
  pub fn favicon_url(tab_id: TabId) -> String {
    format!("chrome://favicons/{}.png", tab_id.0)
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
    let tab_id = parse_favicon_tab_id_from_url(url)?;
    let store = self.favicons.read();
    let Some(bytes) = store.get(&tab_id) else {
      return Some(Err(Error::Resource(ResourceError::new(
        url,
        format!("unknown tab id {}", tab_id.0),
      ))));
    };
    Some(Ok(FetchedResource::with_final_url(
      bytes.clone(),
      Some("image/png".to_string()),
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

fn parse_favicon_tab_id_from_url(url: &str) -> Option<TabId> {
  let parsed = url::Url::parse(url).ok()?;
  if parsed.scheme() != "chrome" {
    return None;
  }
  let host = parsed.host_str()?;
  if host != "favicons" && host != "favicon" {
    return None;
  }
  let path = parsed.path().trim_start_matches('/');
  if path.is_empty() {
    return None;
  }
  // Only support the simplest stable form: a single path segment containing a numeric tab id.
  if path.contains('/') {
    return None;
  }
  let id_str = path.strip_suffix(".png").unwrap_or(path);
  let id = id_str.parse::<u64>().ok()?;
  if id == 0 {
    return None;
  }
  Some(TabId(id))
}

#[cfg(test)]
mod tests {
  use super::*;

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
  fn unknown_tab_id_errors() {
    let inner: Arc<dyn ResourceFetcher> = Arc::new(PanicFetcher);
    let fetcher = ChromeDynamicAssetFetcher::new(inner);

    let err = fetcher
      .fetch("chrome://favicons/999.png")
      .expect_err("unknown tab should error");

    match err {
      Error::Resource(resource) => {
        assert!(
          resource.message.contains("unknown tab id"),
          "unexpected error message: {}",
          resource.message
        );
      }
      other => panic!("expected Error::Resource, got {other:?}"),
    }
  }
}
