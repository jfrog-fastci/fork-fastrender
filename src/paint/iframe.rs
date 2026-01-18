use crate::api::{render_html_with_shared_resources, ResourceContext, ResourceKind};
use crate::debug::runtime;
use crate::error::{Error, RenderStage};
use crate::geometry::Rect;
use crate::html::content_security_policy::CspPolicy;
use crate::html::encoding::decode_html_bytes;
use crate::html::iframe_url::{iframe_navigation_from_src, IframeNavigation};
use crate::image_loader::ImageCache;
use crate::paint::display_list::BorderRadii;
use crate::paint::display_list::ImageData;
use crate::paint::pixmap::new_pixmap;
use crate::render_control;
use crate::resource::{
  ensure_http_success, origin_from_url, FetchDestination, FetchRequest, ReferrerPolicy,
  ResourceAccessPolicy,
};
use crate::style::color::Rgba;
use crate::style::ComputedStyle;
use crate::text::font_loader::FontContext;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine as _;
use lru::LruCache;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use tiny_skia::Pixmap;

const IFRAME_NESTING_LIMIT_MESSAGE: &str = "iframe nesting limit exceeded";

/// Paint-time metadata for an iframe replaced element.
///
/// This is surfaced to embedders so browsers can:
/// - decide whether to render the iframe inline (same process), or
/// - treat it as an out-of-process frame and composite it separately.
#[derive(Debug, Clone)]
pub struct IframePaintInfo {
  /// Stable identifier for the iframe element within the document.
  ///
  /// This is derived from the iframe's per-document `frame_token` (styled DOM pre-order id) when
  /// available, falling back to the originating box id.
  pub stable_id: usize,
  /// Resolved iframe URL (e.g. `src` resolved against the document base URL, or `about:blank`).
  pub url: String,
  /// Content box rect in CSS pixels.
  pub content_rect: Rect,
  /// Content box corner radii in CSS pixels (after subtracting border/padding).
  ///
  /// This matches the clip that would be applied for `overflow: clip`/`hidden` (and is typically
  /// non-zero when the iframe has `border-radius`).
  pub clip_radii: BorderRadii,
  /// True when the iframe uses `srcdoc`.
  pub is_srcdoc: bool,
}

/// Result of an embedder iframe paint hook.
#[derive(Debug, Clone)]
pub enum IframePaintAction {
  /// Paint the iframe inline using the returned raster surface.
  Inline(Arc<ImageData>),
  /// Do not render the iframe in-process; the embedder will composite it separately.
  RemotePlaceholder,
  /// The embedder did not handle this iframe; fall back to the renderer's existing behavior
  /// (e.g. attempt to treat `src` as SVG/image content).
  Fallback,
}

/// Embedder callback invoked when an iframe replaced element is encountered during paint.
///
/// Embedders (e.g. a browser UI or a renderer host process) can use this hook to implement site
/// isolation: cross-origin iframes can be rendered out-of-process and composited by the browser
/// instead of being fetched/rendered recursively inside the parent frame's renderer.
pub trait IframeEmbedder: Send + Sync {
  fn iframe_paint_action(
    &self,
    info: &IframePaintInfo,
    srcdoc_html: Option<&str>,
    style: Option<&ComputedStyle>,
    image_cache: &ImageCache,
    font_ctx: &FontContext,
    device_pixel_ratio: f32,
    max_iframe_depth: usize,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> IframePaintAction;
}

/// Default iframe embedder implementation that preserves current single-process behavior.
///
/// This implementation:
/// - renders `srcdoc` iframes inline via [`render_iframe_srcdoc`],
/// - otherwise renders the iframe `src` URL inline via [`render_iframe_src`],
/// - and returns [`IframePaintAction::Fallback`] when the iframe cannot be rendered as a document
///   (allowing legacy fallback behavior such as trying to decode the URL as an image).
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultIframeEmbedder;

impl IframeEmbedder for DefaultIframeEmbedder {
  fn iframe_paint_action(
    &self,
    info: &IframePaintInfo,
    srcdoc_html: Option<&str>,
    style: Option<&ComputedStyle>,
    image_cache: &ImageCache,
    font_ctx: &FontContext,
    device_pixel_ratio: f32,
    max_iframe_depth: usize,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> IframePaintAction {
    if info.content_rect.width() <= 0.0 || info.content_rect.height() <= 0.0 {
      return IframePaintAction::Fallback;
    }

    if let Some(html) = srcdoc_html {
      if let Some(image) = render_iframe_srcdoc(
        html,
        Some(info.url.as_str()),
        referrer_policy,
        info.content_rect,
        style,
        image_cache,
        font_ctx,
        device_pixel_ratio,
        max_iframe_depth,
      ) {
        return IframePaintAction::Inline(image);
      }
      return IframePaintAction::Fallback;
    }

    if let Some(image) = render_iframe_src(
      info.url.as_str(),
      referrer_policy,
      info.content_rect,
      style,
      image_cache,
      font_ctx,
      device_pixel_ratio,
      max_iframe_depth,
    ) {
      return IframePaintAction::Inline(image);
    }

    IframePaintAction::Fallback
  }
}

/// Returns `true` when `iframe_url` should be treated as cross-origin relative to `document_origin`
/// for site isolation purposes.
///
/// Notes:
/// - `about:blank` and `about:srcdoc` inherit the initiator origin in browsers and are treated as
///   same-origin for the initial MVP.
pub fn iframe_is_cross_origin(
  document_origin: Option<&crate::resource::DocumentOrigin>,
  iframe_url: &str,
) -> bool {
  if is_about_like_url(iframe_url, "about:blank") || is_about_like_url(iframe_url, "about:srcdoc")
  {
    return false;
  }
  let Some(document_origin) = document_origin else {
    return false;
  };
  let Some(target_origin) = origin_from_url(iframe_url) else {
    return false;
  };
  !document_origin.same_origin(&target_origin)
}

fn is_about_like_url(url: &str, prefix: &str) -> bool {
  let Some(head) = url.get(..prefix.len()) else {
    return false;
  };
  if !head.eq_ignore_ascii_case(prefix) {
    return false;
  }
  matches!(
    url.as_bytes().get(prefix.len()),
    None | Some(b'#') | Some(b'?')
  )
}

const DEFAULT_IFRAME_RENDER_CACHE_MAX_ENTRIES: usize = 128;
const DEFAULT_IFRAME_RENDER_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const ENV_IFRAME_RENDER_CACHE_ITEMS: &str = "FASTR_IFRAME_RENDER_CACHE_ITEMS";
const ENV_IFRAME_RENDER_CACHE_BYTES: &str = "FASTR_IFRAME_RENDER_CACHE_BYTES";
const ENV_OOPIF_ENABLED: &str = "FASTR_OOPIF";
const ENV_OOPIF_RENDERER_BIN: &str = "FASTR_OOPIF_RENDERER_BIN";

#[derive(Debug)]
enum OopifError {
  RendererUnavailable,
  SpawnFailed(io::Error),
  Io(io::Error),
  ProcessExit { status: ExitStatus, stderr: Vec<u8> },
  ResponseDecode(String),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OopifRenderRequest {
  url: String,
  html: Option<String>,
  base_url: Option<String>,
  width: u32,
  height: u32,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OopifRenderResponse {
  status: String,
  message: Option<String>,
  pixel_width: Option<u32>,
  pixel_height: Option<u32>,
  premultiplied: Option<bool>,
  pixels_base64: Option<String>,
}

fn iframe_render_cache_limits_from_env() -> (usize, usize) {
  let toggles = runtime::runtime_toggles();
  let max_entries = toggles.usize_with_default(
    ENV_IFRAME_RENDER_CACHE_ITEMS,
    DEFAULT_IFRAME_RENDER_CACHE_MAX_ENTRIES,
  );
  let max_bytes = toggles.usize_with_default(
    ENV_IFRAME_RENDER_CACHE_BYTES,
    DEFAULT_IFRAME_RENDER_CACHE_MAX_BYTES,
  );
  (max_entries, max_bytes)
}

fn oopif_enabled() -> bool {
  runtime::runtime_toggles().truthy(ENV_OOPIF_ENABLED)
}

fn is_crash_url(url: &str) -> bool {
  url
    .as_bytes()
    .get(..8)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"crash://"))
}

fn iframe_renderer_bin_from_toggles() -> Option<PathBuf> {
  let toggles = runtime::runtime_toggles();
  let raw = toggles.get(ENV_OOPIF_RENDERER_BIN)?;
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  Some(PathBuf::from(trimmed))
}

fn iframe_renderer_bin_guess_from_current_exe() -> Option<PathBuf> {
  let exe = std::env::current_exe().ok()?;
  let mut dir = exe.parent()?.to_path_buf();
  // Integration test executables live in `target/{debug,release}/deps`; binaries are siblings.
  if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
    dir = dir.parent()?.to_path_buf();
  }
  let name = format!("iframe_renderer{}", std::env::consts::EXE_SUFFIX);
  let candidate = dir.join(name);
  candidate.is_file().then_some(candidate)
}

fn iframe_renderer_bin() -> Option<PathBuf> {
  iframe_renderer_bin_from_toggles().or_else(iframe_renderer_bin_guess_from_current_exe)
}

fn crashed_iframe_placeholder_image(
  css_width: u32,
  css_height: u32,
  device_pixel_ratio: f32,
) -> Arc<ImageData> {
  let device_width = ((css_width as f32) * device_pixel_ratio).round().max(1.0) as u32;
  let device_height = ((css_height as f32) * device_pixel_ratio).round().max(1.0) as u32;
  let mut pixels = vec![0u8; (device_width as usize) * (device_height as usize) * 4];
  let tile = 8u32.max(1);
  let light = [210u8, 210u8, 210u8, 255u8];
  let dark = [160u8, 160u8, 160u8, 255u8];
  for y in 0..device_height {
    for x in 0..device_width {
      let idx = (y as usize * device_width as usize + x as usize) * 4;
      let which = ((x / tile) + (y / tile)) % 2;
      let c = if which == 0 { light } else { dark };
      pixels[idx..idx + 4].copy_from_slice(&c);
    }
  }
  Arc::new(ImageData::new_premultiplied(
    device_width,
    device_height,
    css_width as f32,
    css_height as f32,
    pixels,
  ))
}

#[cfg(not(feature = "renderer_tools"))]
fn render_iframe_out_of_process(
  url: &str,
  html: Option<&str>,
  base_url: Option<&str>,
  css_width: u32,
  css_height: u32,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
) -> Result<Arc<ImageData>, OopifError> {
  let Some(bin) = iframe_renderer_bin() else {
    return Err(OopifError::RendererUnavailable);
  };

  let cmd = Command::new(bin);

  // Apply a minimal sandbox as early as possible in the child (after `fork`, before `exec`).
  //
  // This is defense-in-depth: the iframe renderer is a security boundary (see `src/bin/iframe_renderer.rs`).
  let mut cmd = crate::sandbox::spawn::configure_renderer_command(
    cmd,
    crate::sandbox::RendererSandboxConfig::default(),
    &[],
  )
  .map_err(|err| {
    OopifError::SpawnFailed(io::Error::new(
      io::ErrorKind::Other,
      format!("failed to configure iframe renderer sandbox: {err}"),
    ))
  })?;

  // Configure stdio after sandbox wrapping so wrapper implementations (e.g. macOS `sandbox-exec`)
  // don't discard the caller's pipe setup.
  cmd
    .command_mut()
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

  let mut child = cmd.spawn().map_err(OopifError::SpawnFailed)?;
  {
    let Some(mut stdin) = child.stdin.take() else {
      return Err(OopifError::Io(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "failed to open iframe renderer stdin",
      )));
    };
    let req = OopifRenderRequest {
      url: url.to_string(),
      html: html.map(str::to_string),
      base_url: base_url.map(str::to_string),
      width: css_width.max(1),
      height: css_height.max(1),
      device_pixel_ratio,
      max_iframe_depth,
    };
    serde_json::to_writer(&mut stdin, &req)
      .map_err(|err| OopifError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string())))?;
  }

  let output = child.wait_with_output().map_err(OopifError::Io)?;
  if !output.status.success() {
    return Err(OopifError::ProcessExit {
      status: output.status,
      stderr: output.stderr,
    });
  }

  let resp: OopifRenderResponse = serde_json::from_slice(&output.stdout)
    .map_err(|err| OopifError::ResponseDecode(err.to_string()))?;
  if resp.status != "ok" {
    return Err(OopifError::ResponseDecode(resp.message.unwrap_or_else(
      || format!("iframe renderer returned status {}", resp.status),
    )));
  }
  let pixel_width = resp
    .pixel_width
    .ok_or_else(|| OopifError::ResponseDecode("iframe renderer missing pixel_width".to_string()))?;
  let pixel_height = resp.pixel_height.ok_or_else(|| {
    OopifError::ResponseDecode("iframe renderer missing pixel_height".to_string())
  })?;
  let premultiplied = resp.premultiplied.unwrap_or(true);
  if !premultiplied {
    return Err(OopifError::ResponseDecode(
      "iframe renderer returned un-premultiplied pixels".to_string(),
    ));
  }
  let pixels_b64 = resp.pixels_base64.ok_or_else(|| {
    OopifError::ResponseDecode("iframe renderer missing pixels_base64".to_string())
  })?;
  let pixels = BASE64_STD
    .decode(pixels_b64.as_bytes())
    .map_err(|err| OopifError::ResponseDecode(err.to_string()))?;
  if pixels.len() != (pixel_width as usize) * (pixel_height as usize) * 4 {
    return Err(OopifError::ResponseDecode(format!(
      "iframe renderer returned {} bytes, expected {}",
      pixels.len(),
      (pixel_width as usize) * (pixel_height as usize) * 4
    )));
  }
  Ok(Arc::new(ImageData::new_premultiplied(
    pixel_width,
    pixel_height,
    css_width as f32,
    css_height as f32,
    pixels,
  )))
}

#[cfg(feature = "renderer_tools")]
fn render_iframe_out_of_process(
  _url: &str,
  _html: Option<&str>,
  _base_url: Option<&str>,
  _css_width: u32,
  _css_height: u32,
  _device_pixel_ratio: f32,
  _max_iframe_depth: usize,
) -> Result<Arc<ImageData>, OopifError> {
  // Out-of-process iframe rendering is only used by the multiprocess browser embedding. Offline
  // renderer tooling (`render_fixtures`, `diff_renders`) keeps builds lean by disabling the sandbox
  // and process-spawning subsystems.
  Err(OopifError::RendererUnavailable)
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BackgroundKey {
  r: u8,
  g: u8,
  b: u8,
  a_bits: u32,
}

impl From<Rgba> for BackgroundKey {
  fn from(value: Rgba) -> Self {
    Self {
      r: value.r,
      g: value.g,
      b: value.b,
      a_bits: f32_to_canonical_bits(value.a),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum IframeRenderCacheContent {
  Src {
    url: String,
  },
  Srcdoc {
    html_hash: u64,
    base_url: Option<String>,
  },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IframeRenderCacheKey {
  image_cache_id: u64,
  content: IframeRenderCacheContent,
  css_width: u32,
  css_height: u32,
  device_pixel_ratio_bits: u32,
  nested_depth: usize,
  background: BackgroundKey,
  policy_hash: u64,
  referrer_url_hash: Option<u64>,
  request_referrer_policy: ReferrerPolicy,
}

#[derive(Clone)]
enum SharedIframeResult {
  Success(Arc<ImageData>),
  None,
}

impl SharedIframeResult {
  fn as_option(&self) -> Option<Arc<ImageData>> {
    match self {
      Self::Success(image) => Some(Arc::clone(image)),
      Self::None => None,
    }
  }
}

struct IframeInFlight {
  result: Mutex<Option<SharedIframeResult>>,
  cv: Condvar,
}

impl IframeInFlight {
  fn new() -> Self {
    Self {
      result: Mutex::new(None),
      cv: Condvar::new(),
    }
  }

  fn set(&self, result: SharedIframeResult) {
    let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(result);
    self.cv.notify_all();
  }

  fn wait(&self) -> Option<Arc<ImageData>> {
    let mut guard = self.result.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = render_control::active_deadline().filter(|d| d.is_enabled());
    while guard.is_none() {
      if let Some(deadline) = deadline.as_ref() {
        if deadline.check(RenderStage::Paint).is_err() {
          return None;
        }
        let wait_for = if deadline.timeout_limit().is_some() {
          match deadline.remaining_timeout() {
            Some(remaining) if !remaining.is_zero() => remaining.min(Duration::from_millis(10)),
            _ => return None,
          }
        } else {
          Duration::from_millis(10)
        };
        guard = self
          .cv
          .wait_timeout(guard, wait_for)
          .unwrap_or_else(|e| e.into_inner())
          .0;
      } else {
        guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
      }
    }
    guard.as_ref()?.as_option()
  }
}

struct CachedIframeRenderEntry {
  image: Arc<ImageData>,
  bytes: usize,
}

struct IframeRenderCache {
  inner: LruCache<IframeRenderCacheKey, CachedIframeRenderEntry>,
  in_flight: HashMap<IframeRenderCacheKey, Arc<IframeInFlight>>,
  max_entries: usize,
  max_bytes: usize,
  current_bytes: usize,
}

impl IframeRenderCache {
  fn new(max_entries: usize, max_bytes: usize) -> Self {
    Self {
      inner: LruCache::unbounded(),
      in_flight: HashMap::new(),
      max_entries,
      max_bytes,
      current_bytes: 0,
    }
  }

  fn caching_disabled(&self) -> bool {
    self.max_entries == 0 || self.max_bytes == 0
  }

  fn get(&mut self, key: &IframeRenderCacheKey) -> Option<Arc<ImageData>> {
    if self.caching_disabled() {
      return None;
    }
    self.inner.get(key).map(|entry| Arc::clone(&entry.image))
  }

  fn join_inflight(&mut self, key: &IframeRenderCacheKey) -> (Arc<IframeInFlight>, bool) {
    if let Some(existing) = self.in_flight.get(key) {
      return (Arc::clone(existing), false);
    }

    let flight = Arc::new(IframeInFlight::new());
    self.in_flight.insert(key.clone(), Arc::clone(&flight));
    (flight, true)
  }

  fn insert(&mut self, key: IframeRenderCacheKey, image: Arc<ImageData>) {
    if self.caching_disabled() {
      return;
    }
    let bytes = image.pixels.len();
    if self.max_bytes > 0 && bytes > self.max_bytes {
      // Skip caching entries that would evict the entire cache on their own.
      return;
    }
    if let Some(entry) = self.inner.pop(&key) {
      self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
    }
    self
      .inner
      .put(key, CachedIframeRenderEntry { image, bytes });
    self.current_bytes = self.current_bytes.saturating_add(bytes);
    self.evict_if_needed();
  }

  fn evict_if_needed(&mut self) {
    while (self.max_entries > 0 && self.inner.len() > self.max_entries)
      || (self.max_bytes > 0 && self.current_bytes > self.max_bytes)
    {
      if let Some((_key, entry)) = self.inner.pop_lru() {
        self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
      } else {
        break;
      }
    }
  }
}

static IFRAME_RENDER_CACHE: OnceLock<Mutex<IframeRenderCache>> = OnceLock::new();

fn iframe_render_cache() -> &'static Mutex<IframeRenderCache> {
  IFRAME_RENDER_CACHE.get_or_init(|| {
    let (max_entries, max_bytes) = iframe_render_cache_limits_from_env();
    Mutex::new(IframeRenderCache::new(max_entries, max_bytes))
  })
}

fn finish_iframe_inflight(
  key: IframeRenderCacheKey,
  flight: &Arc<IframeInFlight>,
  result: Option<Arc<ImageData>>,
) {
  match result {
    Some(image) => {
      let mut cache = iframe_render_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
      cache.in_flight.remove(&key);
      cache.insert(key, Arc::clone(&image));
      drop(cache);
      flight.set(SharedIframeResult::Success(image));
    }
    None => {
      let mut cache = iframe_render_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
      cache.in_flight.remove(&key);
      drop(cache);
      flight.set(SharedIframeResult::None);
    }
  }
}

struct IframeInFlightOwnerGuard {
  key: Option<IframeRenderCacheKey>,
  flight: Arc<IframeInFlight>,
}

impl IframeInFlightOwnerGuard {
  fn new(key: IframeRenderCacheKey, flight: Arc<IframeInFlight>) -> Self {
    Self {
      key: Some(key),
      flight,
    }
  }

  fn finish(&mut self, result: Option<Arc<ImageData>>) {
    let Some(key) = self.key.take() else {
      return;
    };
    finish_iframe_inflight(key, &self.flight, result);
  }
}

impl Drop for IframeInFlightOwnerGuard {
  fn drop(&mut self) {
    let Some(key) = self.key.take() else {
      return;
    };
    finish_iframe_inflight(key, &self.flight, None);
  }
}

fn stable_hash_bytes(bytes: &[u8]) -> u64 {
  let mut hasher = DefaultHasher::new();
  bytes.hash(&mut hasher);
  hasher.finish()
}

fn policy_fingerprint(policy: &ResourceAccessPolicy) -> u64 {
  // The nested document origin is derived from the iframe's URL/base URL. That value is already
  // part of the cache key, so we avoid hashing it here. This allows identical iframe URLs to share
  // cached renders across different outer documents while still respecting policy knobs.
  let mut hasher = DefaultHasher::new();
  policy.allow_file_from_http.hash(&mut hasher);
  policy.block_mixed_content.hash(&mut hasher);
  policy.same_origin_only.hash(&mut hasher);
  let mut allowed: Vec<String> = policy
    .allowed_origins
    .iter()
    .map(ToString::to_string)
    .collect();
  allowed.sort();
  allowed.hash(&mut hasher);
  hasher.finish()
}

fn image_data_from_pixmap(pixmap: &Pixmap, css_width: u32, css_height: u32) -> Arc<ImageData> {
  Arc::new(ImageData::from_pixmap(
    pixmap,
    css_width as f32,
    css_height as f32,
  ))
}

#[cfg(test)]
thread_local! {
  static LAST_IFRAME_CACHE_HIT: std::cell::Cell<Option<bool>> = std::cell::Cell::new(None);
}

#[cfg(test)]
fn record_iframe_cache_hit(hit: bool) {
  LAST_IFRAME_CACHE_HIT.with(|cell| cell.set(Some(hit)));
}

#[cfg(test)]
pub(crate) fn take_last_iframe_cache_hit() -> Option<bool> {
  LAST_IFRAME_CACHE_HIT.with(|cell| cell.replace(None))
}

#[cfg(test)]
type IframeJoinHook = Arc<dyn Fn(&IframeRenderCacheKey, bool) + Send + Sync>;

#[cfg(test)]
static IFRAME_RENDER_CACHE_JOIN_HOOK: OnceLock<Mutex<Option<IframeJoinHook>>> = OnceLock::new();

#[cfg(test)]
fn iframe_render_cache_join_hook() -> &'static Mutex<Option<IframeJoinHook>> {
  IFRAME_RENDER_CACHE_JOIN_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn set_iframe_render_cache_join_hook(hook: Option<IframeJoinHook>) {
  let mut guard = iframe_render_cache_join_hook()
    .lock()
    .unwrap_or_else(|e| e.into_inner());
  *guard = hook;
}

#[cfg(test)]
fn run_iframe_render_cache_join_hook(key: &IframeRenderCacheKey, is_owner: bool) {
  let hook = iframe_render_cache_join_hook()
    .lock()
    .ok()
    .and_then(|guard| guard.as_ref().cloned());
  if let Some(hook) = hook {
    hook(key, is_owner);
  }
}

fn record_resource_error(
  ctx: &ResourceContext,
  kind: ResourceKind,
  requested_url: &str,
  err: &Error,
) {
  match err {
    Error::Resource(res) => {
      let final_url = res.final_url.as_deref().or(Some(res.url.as_str()));
      let mut message = res.message.clone();
      if let Some(status) = res.status {
        let lower = message.to_ascii_lowercase();
        if !lower.contains("status") {
          message = format!("{message} (status {status})");
        }
      }
      ctx.record_violation(kind, requested_url, final_url, message);
    }
    other => ctx.record_violation(kind, requested_url, None, other.to_string()),
  }
}

pub(crate) fn render_iframe_srcdoc(
  html: &str,
  _src: Option<&str>,
  referrer_policy: Option<ReferrerPolicy>,
  content_rect: Rect,
  _style: Option<&ComputedStyle>,
  image_cache: &ImageCache,
  font_ctx: &FontContext,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
) -> Option<Arc<ImageData>> {
  let width = content_rect.width().ceil() as u32;
  let height = content_rect.height().ceil() as u32;
  if width == 0 || height == 0 {
    return None;
  }

  // The URL of an `srcdoc` browsing context is always `about:srcdoc`, even if the iframe element
  // has an `src` attribute (used for base URL resolution). Using `about:srcdoc` here ensures
  // diagnostics like depth-limit violations match browser behavior.
  let iframe_url = "about:srcdoc".to_string();
  let context = image_cache.resource_context();
  let remaining_depth = context
    .as_ref()
    .and_then(|ctx| ctx.iframe_depth_remaining)
    .unwrap_or(max_iframe_depth);
  if remaining_depth == 0 {
    if let Some(ctx) = context.as_ref() {
      ctx.record_violation(
        ResourceKind::Document,
        &iframe_url,
        None,
        IFRAME_NESTING_LIMIT_MESSAGE.to_string(),
      );
    }
    let device_width = ((width as f32) * device_pixel_ratio).round().max(1.0) as u32;
    let device_height = ((height as f32) * device_pixel_ratio).round().max(1.0) as u32;
    // When iframe nesting is exhausted we intentionally skip painting the nested browsing context.
    // Return a fully transparent surface so the iframe element's own background/border (if any)
    // remains visible without forcing an opaque placeholder over the parent content.
    let pixmap = new_pixmap(device_width, device_height)?;
    let image = image_data_from_pixmap(&pixmap, width, height);
    #[cfg(test)]
    record_iframe_cache_hit(false);
    return Some(image);
  }

  // Render the nested document into a default white canvas, matching Chrome's iframe browsing
  // context background when the iframe document does not paint an explicit background.
  //
  // Note: This is *not* the iframe element's background-color (that is painted by the outer
  // document). This is the nested browsing context canvas background.
  let background = Rgba::WHITE;
  let base_url = image_cache.base_url();
  let mut cache = image_cache.clone();
  if let Some(base_url) = base_url.clone() {
    cache.set_base_url(base_url);
  }
  let context = cache.resource_context();
  let referrer_url = context
    .as_ref()
    .and_then(|ctx| ctx.document_url.as_deref())
    .or(base_url.as_deref());
  let doc_referrer_policy = context
    .as_ref()
    .map(|ctx| ctx.referrer_policy)
    .unwrap_or_default();
  let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
  let referrer_url_hash = referrer_url.map(|url| stable_hash_bytes(url.as_bytes()));
  let nested_depth = remaining_depth.saturating_sub(1);
  let nested_context = context.as_ref().map(|ctx| {
    let mut nested = ctx.clone().with_iframe_depth(nested_depth);
    nested.referrer_policy = request_referrer_policy;
    nested
  });
  let policy = nested_context
    .as_ref()
    .map(|c| c.policy.clone())
    .or_else(|| context.as_ref().map(|c| c.policy.clone()))
    .unwrap_or_default();

  let key = IframeRenderCacheKey {
    image_cache_id: image_cache.instance_id(),
    content: IframeRenderCacheContent::Srcdoc {
      html_hash: stable_hash_bytes(html.as_bytes()),
      base_url: base_url.clone(),
    },
    css_width: width,
    css_height: height,
    device_pixel_ratio_bits: f32_to_canonical_bits(device_pixel_ratio),
    nested_depth,
    background: background.into(),
    policy_hash: policy_fingerprint(&policy),
    referrer_url_hash,
    request_referrer_policy,
  };

  let (flight, is_owner) = {
    let mut cache = iframe_render_cache()
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    if let Some(image) = cache.get(&key) {
      #[cfg(test)]
      record_iframe_cache_hit(true);
      return Some(image);
    }
    cache.join_inflight(&key)
  };
  #[cfg(test)]
  run_iframe_render_cache_join_hook(&key, is_owner);
  if !is_owner {
    let image = flight.wait();
    #[cfg(test)]
    record_iframe_cache_hit(image.is_some());
    return image;
  }
  let mut owner_guard = IframeInFlightOwnerGuard::new(key, flight);

  let pixmap = match render_html_with_shared_resources(
    html,
    width,
    height,
    background,
    font_ctx,
    &cache,
    Arc::clone(cache.fetcher()),
    base_url,
    device_pixel_ratio,
    policy,
    nested_context,
    nested_depth,
  ) {
    Ok(pixmap) => pixmap,
    Err(err) => {
      if let Some(ctx) = context.as_ref() {
        record_resource_error(ctx, ResourceKind::Document, &iframe_url, &err);
      }
      owner_guard.finish(None);
      #[cfg(test)]
      record_iframe_cache_hit(false);
      return None;
    }
  };
  let image = image_data_from_pixmap(&pixmap, width, height);
  owner_guard.finish(Some(Arc::clone(&image)));
  #[cfg(test)]
  record_iframe_cache_hit(false);
  Some(image)
}

pub(crate) fn render_iframe_src(
  src: &str,
  referrer_policy: Option<ReferrerPolicy>,
  content_rect: Rect,
  _style: Option<&ComputedStyle>,
  image_cache: &ImageCache,
  font_ctx: &FontContext,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
) -> Option<Arc<ImageData>> {
  let base_url = image_cache.base_url();
  let (resolved, is_about_blank) =
    match iframe_navigation_from_src(Some(src), base_url.as_deref().unwrap_or("")) {
      IframeNavigation::None => return None,
      IframeNavigation::AboutBlank => ("about:blank".to_string(), true),
      IframeNavigation::Url(url) => (url, false),
    };

  if resolved.is_empty() {
    return None;
  }
  let width = content_rect.width().ceil() as u32;
  let height = content_rect.height().ceil() as u32;
  if width == 0 || height == 0 {
    return None;
  }

  let context = image_cache.resource_context();
  let use_oopif = oopif_enabled()
    && context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .and_then(|parent| origin_from_url(&resolved).map(|child| (parent, child)))
      .is_some_and(|(parent, child)| !parent.same_origin(&child));
  let remaining_depth = context
    .as_ref()
    .and_then(|ctx| ctx.iframe_depth_remaining)
    .unwrap_or(max_iframe_depth);
  if remaining_depth == 0 {
    if let Some(ctx) = context.as_ref() {
      ctx.record_violation(
        ResourceKind::Document,
        &resolved,
        Some(&resolved),
        IFRAME_NESTING_LIMIT_MESSAGE.to_string(),
      );
    }
    let device_width = ((width as f32) * device_pixel_ratio).round().max(1.0) as u32;
    let device_height = ((height as f32) * device_pixel_ratio).round().max(1.0) as u32;
    // When iframe nesting is exhausted we intentionally skip painting the nested browsing context.
    // Return a fully transparent surface so the iframe element's own background/border (if any)
    // remains visible without forcing an opaque placeholder over the parent content.
    let pixmap = new_pixmap(device_width, device_height)?;
    let image = image_data_from_pixmap(&pixmap, width, height);
    #[cfg(test)]
    record_iframe_cache_hit(false);
    return Some(image);
  }
  let nested_depth = remaining_depth.saturating_sub(1);
  let fetcher = Arc::clone(image_cache.fetcher());
  // See `render_iframe_srcdoc` for why iframe documents use a default white browsing context canvas.
  let background = Rgba::WHITE;

  if is_about_blank {
    // about:blank is a browser-provided empty document. Treat it as an empty iframe instead of a
    // resource fetch so offline fixtures do not record spurious fetch errors.
    let device_width = ((width as f32) * device_pixel_ratio).round().max(1.0) as u32;
    let device_height = ((height as f32) * device_pixel_ratio).round().max(1.0) as u32;
    // Preserve legacy FastRender behavior: treat about:blank as an empty *transparent* browsing
    // context so pages that use iframes as lazy-load placeholders do not get an opaque white box
    // painted over their fallback content.
    let pixmap = new_pixmap(device_width, device_height)?;
    let image = image_data_from_pixmap(&pixmap, width, height);
    #[cfg(test)]
    record_iframe_cache_hit(false);
    return Some(image);
  }
  if let Some(ctx) = context.as_ref() {
    if ctx
      .check_allowed(ResourceKind::Document, &resolved)
      .is_err()
    {
      return None;
    }
  }

  let policy = context
    .as_ref()
    .map(|ctx| ctx.policy.clone())
    .unwrap_or_default();

  let referrer_url = context
    .as_ref()
    .and_then(|ctx| ctx.document_url.as_deref())
    .or(base_url.as_deref());
  let doc_referrer_policy = context
    .as_ref()
    .map(|ctx| ctx.referrer_policy)
    .unwrap_or_default();
  let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
  let referrer_url_hash = referrer_url.map(|url| stable_hash_bytes(url.as_bytes()));

  let key = IframeRenderCacheKey {
    image_cache_id: image_cache.instance_id(),
    content: IframeRenderCacheContent::Src {
      url: resolved.clone(),
    },
    css_width: width,
    css_height: height,
    device_pixel_ratio_bits: f32_to_canonical_bits(device_pixel_ratio),
    nested_depth,
    background: background.into(),
    policy_hash: policy_fingerprint(&policy),
    referrer_url_hash,
    request_referrer_policy,
  };
  let (flight, is_owner) = {
    let mut cache = iframe_render_cache()
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    if let Some(image) = cache.get(&key) {
      #[cfg(test)]
      record_iframe_cache_hit(true);
      return Some(image);
    }
    cache.join_inflight(&key)
  };
  #[cfg(test)]
  run_iframe_render_cache_join_hook(&key, is_owner);
  if !is_owner {
    let image = flight.wait();
    #[cfg(test)]
    record_iframe_cache_hit(image.is_some());
    return image;
  }
  let mut owner_guard = IframeInFlightOwnerGuard::new(key, flight);

  // Deterministic crash trigger URLs bypass network fetch and instead instruct the out-of-process
  // renderer to crash. The browser process should remain alive and paint a fallback placeholder.
  if use_oopif && is_crash_url(&resolved) {
    let image = match render_iframe_out_of_process(
      &resolved,
      None,
      Some(&resolved),
      width,
      height,
      device_pixel_ratio,
      nested_depth,
    ) {
      Ok(image) => image,
      Err(_) => {
        if let Some(ctx) = context.as_ref() {
          ctx.record_violation(
            ResourceKind::Document,
            &resolved,
            Some(&resolved),
            "iframe renderer process crashed".to_string(),
          );
        }
        crashed_iframe_placeholder_image(width, height, device_pixel_ratio)
      }
    };
    owner_guard.finish(Some(Arc::clone(&image)));
    #[cfg(test)]
    record_iframe_cache_hit(false);
    return Some(image);
  }

  let origin_fallback = referrer_url.and_then(origin_from_url);
  let client_origin = context
    .as_ref()
    .and_then(|ctx| ctx.policy.document_origin.as_ref())
    .or(origin_fallback.as_ref());
  let mut request = FetchRequest::new(&resolved, FetchDestination::Iframe);
  if let Some(origin) = client_origin {
    request = request.with_client_origin(origin);
  }
  if let Some(referrer_url) = referrer_url {
    request = request.with_referrer_url(referrer_url);
  }
  request = request.with_referrer_policy(request_referrer_policy);
  let resource = match fetcher.fetch_with_request(request) {
    Ok(resource) => resource,
    Err(err) => {
      if let Some(ctx) = context.as_ref() {
        record_resource_error(ctx, ResourceKind::Document, &resolved, &err);
      }
      return None;
    }
  };
  let final_url = resource
    .final_url
    .clone()
    .unwrap_or_else(|| resolved.clone());
  if let Some(ctx) = context.as_ref() {
    if ctx
      .check_allowed_with_final(
        ResourceKind::Document,
        &resolved,
        resource.final_url.as_deref(),
      )
      .is_err()
    {
      return None;
    }
  }
  let content_type = resource.content_type.as_deref();
  let is_html = content_type
    .map(|ct| {
      let ct = ct.to_ascii_lowercase();
      ct.starts_with("text/html")
        || ct.starts_with("application/xhtml+xml")
        || ct.starts_with("application/html")
        || ct.contains("+html")
    })
    .unwrap_or_else(|| {
      let lower = resolved.to_ascii_lowercase();
      lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml")
    });
  if let Err(err) = ensure_http_success(&resource, &resolved) {
    // Unlike subresources (images/fonts/stylesheets), iframe navigations render as documents: most
    // browsers still display an HTML response body even when the HTTP status is an error (404,
    // 500, etc.). Record the failure for diagnostics, but continue rendering when the body is HTML.
    if let Some(ctx) = context.as_ref() {
      record_resource_error(ctx, ResourceKind::Document, &resolved, &err);
    }
    if !is_html {
      return None;
    }
  }
  if !is_html {
    if let Some(ctx) = context.as_ref() {
      let content_type = content_type.unwrap_or("<missing>");
      let status = resource
        .status
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<missing>".to_string());
      let final_url = resource.final_url.as_deref().unwrap_or(&resolved);
      ctx.record_violation(
        ResourceKind::Document,
        &resolved,
        resource.final_url.as_deref(),
        format!("unexpected content-type {content_type} (status {status}, final_url {final_url})"),
      );
    }
    return None;
  }

  let html = decode_html_bytes(&resource.bytes, content_type);
  let mut cache = image_cache.clone();
  cache.set_base_url(final_url.clone());
  let nested_origin = origin_from_url(&final_url);
  let response_referrer_policy = resource.response_referrer_policy;
  let response_csp = CspPolicy::from_response_headers(&resource);
  let nested_context = context.as_ref().map(|ctx| {
    let mut nested = ctx
      .for_origin(nested_origin)
      .with_iframe_depth(nested_depth);
    nested.document_url = resource
      .final_url
      .as_deref()
      .map(|u| u.to_string())
      .or_else(|| Some(resolved.clone()));
    // HTML documents inherit the referrer policy used for their navigation request unless
    // overridden by the `Referrer-Policy` response header or a `<meta name="referrer">` inside the
    // document. Ensure per-iframe `referrerpolicy` overrides apply to subsequent subresource loads.
    nested.referrer_policy = request_referrer_policy;
    if let Some(policy) = response_referrer_policy {
      nested.referrer_policy = policy;
    }
    // Iframe subresources are controlled by the iframe document's own CSP, not the parent.
    nested.csp = response_csp.clone();
    nested
  });
  let policy_for_render = nested_context
    .as_ref()
    .map(|ctx| ctx.policy.clone())
    .or_else(|| context.as_ref().map(|ctx| ctx.policy.clone()))
    .unwrap_or_default();

  if use_oopif {
    match render_iframe_out_of_process(
      &final_url,
      Some(&html),
      Some(&final_url),
      width,
      height,
      device_pixel_ratio,
      nested_depth,
    ) {
      Ok(image) => {
        owner_guard.finish(Some(Arc::clone(&image)));
        #[cfg(test)]
        record_iframe_cache_hit(false);
        return Some(image);
      }
      Err(OopifError::RendererUnavailable | OopifError::SpawnFailed(_)) => {
        // Best-effort fallback: if the renderer binary is not available, render in-process so we
        // preserve existing behavior.
      }
      Err(err) => {
        if let Some(ctx) = context.as_ref() {
          ctx.record_violation(
            ResourceKind::Document,
            &resolved,
            Some(&final_url),
            format!("iframe renderer process crashed: {err:?}"),
          );
        }
        let image = crashed_iframe_placeholder_image(width, height, device_pixel_ratio);
        owner_guard.finish(Some(Arc::clone(&image)));
        #[cfg(test)]
        record_iframe_cache_hit(false);
        return Some(image);
      }
    }
  }

  let pixmap = render_html_with_shared_resources(
    &html,
    width,
    height,
    background,
    font_ctx,
    &cache,
    fetcher,
    Some(final_url),
    device_pixel_ratio,
    policy_for_render,
    nested_context,
    nested_depth,
  )
  .ok()?;

  let image = image_data_from_pixmap(&pixmap, width, height);
  owner_guard.finish(Some(Arc::clone(&image)));
  #[cfg(test)]
  record_iframe_cache_hit(false);
  Some(image)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::Error;
  use crate::resource::ResourceFetcher;
  use crate::resource::{FetchRequest, FetchedResource};
  use std::io;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Mutex as StdMutex;
  use std::sync::{Arc, Barrier};

  #[derive(Clone, Default)]
  struct RejectingFetcher;

  impl ResourceFetcher for RejectingFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("unexpected fetch: {url}"),
      )))
    }
  }

  #[test]
  fn iframe_about_blank_renders_white_background() {
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::with_fetcher(Arc::new(RejectingFetcher::default()));
    let rect = Rect::from_xywh(0.0, 0.0, 2.0, 2.0);

    let image =
      render_iframe_src("about:blank", None, rect, None, &image_cache, &font_ctx, 1.0, 3)
        .expect("expected about:blank iframe render");

    assert_eq!(image.width, 2);
    assert_eq!(image.height, 2);
    assert!(image.premultiplied, "expected premultiplied pixels");
    for px in image.pixels.chunks_exact(4) {
      assert_eq!(px, [255, 255, 255, 255]);
    }
  }

  #[test]
  fn iframe_srcdoc_defaults_to_white_background() {
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::with_fetcher(Arc::new(RejectingFetcher::default()));
    let rect = Rect::from_xywh(0.0, 0.0, 2.0, 2.0);
    let html = "<html><body></body></html>";

    let image =
      render_iframe_srcdoc(html, None, None, rect, None, &image_cache, &font_ctx, 1.0, 3)
        .expect("expected srcdoc iframe render");

    assert_eq!(image.width, 2);
    assert_eq!(image.height, 2);
    assert!(image.premultiplied, "expected premultiplied pixels");
    for px in image.pixels.chunks_exact(4) {
      assert_eq!(px, [255, 255, 255, 255]);
    }
  }

  struct HtmlFetcher {
    expected_url: String,
    body: Vec<u8>,
    calls: AtomicUsize,
  }

  impl ResourceFetcher for HtmlFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      self.calls.fetch_add(1, Ordering::SeqCst);
      if url != self.expected_url {
        return Err(Error::Io(io::Error::new(
          io::ErrorKind::NotFound,
          format!("unexpected fetch: {url}"),
        )));
      }
      Ok(FetchedResource::new(
        self.body.clone(),
        Some("text/html; charset=utf-8".to_string()),
      ))
    }
  }

  struct CountingIframeFetcher {
    iframe_url: String,
    body: Vec<u8>,
    iframe_calls: AtomicUsize,
    iframe_requests: StdMutex<Vec<(Option<String>, ReferrerPolicy)>>,
  }

  impl ResourceFetcher for CountingIframeFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("unexpected fetch: {url}"),
      )))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> crate::error::Result<FetchedResource> {
      match req.destination {
        FetchDestination::Iframe => {
          assert_eq!(req.url, self.iframe_url);
          self.iframe_calls.fetch_add(1, Ordering::SeqCst);
          self
            .iframe_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((req.referrer_url.map(|u| u.to_string()), req.referrer_policy));
          Ok(FetchedResource::new(
            self.body.clone(),
            Some("text/html; charset=utf-8".to_string()),
          ))
        }
        other => Err(Error::Io(io::Error::new(
          io::ErrorKind::NotFound,
          format!("unexpected fetch destination {other:?} for url {}", req.url),
        ))),
      }
    }
  }

  struct RedirectingStylesheetFetcher {
    doc_url: String,
    final_url: String,
    css_url: String,
    requests: StdMutex<Vec<String>>,
  }

  impl ResourceFetcher for RedirectingStylesheetFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      self
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(url.to_string());

      if url == self.doc_url {
        let html = format!(
          r#"
            <link rel="stylesheet" href="style-redirect-113.css">
            <div data-fastr-test="iframe-redirect-base-url-113"></div>
          "#
        );
        return Ok(FetchedResource::with_final_url(
          html.into_bytes(),
          Some("text/html; charset=utf-8".to_string()),
          Some(self.final_url.clone()),
        ));
      }
      if url == self.css_url {
        return Ok(FetchedResource::new(
          b"html, body { margin: 0; padding: 0; }".to_vec(),
          Some("text/css; charset=utf-8".to_string()),
        ));
      }

      Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("unexpected fetch: {url}"),
      )))
    }
  }

  #[derive(Clone)]
  struct ReferrerPolicyHeaderIframeFetcher {
    iframe_url: String,
    css_url: String,
    observed_css_policy: Arc<StdMutex<Option<ReferrerPolicy>>>,
  }

  impl ReferrerPolicyHeaderIframeFetcher {
    fn new(
      iframe_url: &str,
      css_url: &str,
      observed_css_policy: Arc<StdMutex<Option<ReferrerPolicy>>>,
    ) -> Self {
      Self {
        iframe_url: iframe_url.to_string(),
        css_url: css_url.to_string(),
        observed_css_policy,
      }
    }
  }

  impl ResourceFetcher for ReferrerPolicyHeaderIframeFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("unexpected fetch: {url}"),
      )))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> crate::error::Result<FetchedResource> {
      match req.destination {
        FetchDestination::Iframe => {
          assert_eq!(req.url, self.iframe_url);
          let html = r#"
            <link rel="stylesheet" href="style.css">
            <div data-fastr-test="iframe-referrer-policy-header-113"></div>
          "#;
          let mut res = FetchedResource::with_final_url(
            html.as_bytes().to_vec(),
            Some("text/html; charset=utf-8".to_string()),
            Some(self.iframe_url.clone()),
          );
          res.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);
          Ok(res)
        }
        FetchDestination::Style => {
          assert_eq!(req.url, self.css_url);
          if let Ok(mut guard) = self.observed_css_policy.lock() {
            *guard = Some(req.referrer_policy);
          }
          Ok(FetchedResource::new(
            b"html, body { margin: 0; padding: 0; }".to_vec(),
            Some("text/css; charset=utf-8".to_string()),
          ))
        }
        other => Err(Error::Io(io::Error::new(
          io::ErrorKind::NotFound,
          format!("unexpected fetch destination {other:?} for url {}", req.url),
        ))),
      }
    }
  }

  #[test]
  fn iframe_render_cache_hits_for_repeated_srcdoc() {
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::with_fetcher(Arc::new(RejectingFetcher::default()));
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }</style>
      <div data-fastr-test="iframe-render-cache-113"></div>
    "#;

    let first = render_iframe_srcdoc(
      html,
      None,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("first iframe render");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "first render should miss cache"
    );

    let second = render_iframe_srcdoc(
      html,
      None,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("second iframe render");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(true),
      "second render should hit cache"
    );
    assert!(
      Arc::ptr_eq(&first, &second),
      "cache hit should return the same Arc<ImageData>"
    );
  }

  #[test]
  fn iframe_render_cache_key_canonicalizes_negative_zero_background_alpha() {
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::with_fetcher(Arc::new(RejectingFetcher::default()));
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }</style>
      <div data-fastr-test="iframe-render-cache-alpha-negzero-113"></div>
    "#;

    let mut style_pos = ComputedStyle::default();
    style_pos.background_color = Rgba {
      r: 0,
      g: 0,
      b: 0,
      a: 0.0,
    };
    let mut style_neg = ComputedStyle::default();
    style_neg.background_color = Rgba {
      r: 0,
      g: 0,
      b: 0,
      a: -0.0,
    };

    let first = render_iframe_srcdoc(
      html,
      None,
      None,
      rect,
      Some(&style_pos),
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("first iframe render");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "first render should miss cache"
    );

    let second = render_iframe_srcdoc(
      html,
      None,
      None,
      rect,
      Some(&style_neg),
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("second iframe render");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(true),
      "second render should hit cache"
    );
    assert!(
      Arc::ptr_eq(&first, &second),
      "cache hit should return the same Arc<ImageData>"
    );
  }

  #[test]
  fn iframe_render_cache_hits_for_repeated_src() {
    let font_ctx = FontContext::new();
    let url = "https://example.com/iframe-render-cache-src-113.html";
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(255, 0, 0); }</style>
      <div data-fastr-test="iframe-render-cache-src-113"></div>
    "#;
    let fetcher = Arc::new(HtmlFetcher {
      expected_url: url.to_string(),
      body: html.as_bytes().to_vec(),
      calls: AtomicUsize::new(0),
    });
    let image_cache = ImageCache::with_fetcher(fetcher.clone());
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

    let first = render_iframe_src(url, None, rect, None, &image_cache, &font_ctx, 1.0, 3)
      .expect("first iframe src render");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "first render should miss cache"
    );

    let second = render_iframe_src(url, None, rect, None, &image_cache, &font_ctx, 1.0, 3)
      .expect("second iframe src render");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(true),
      "second render should hit cache"
    );
    assert!(
      Arc::ptr_eq(&first, &second),
      "cache hit should return the same Arc<ImageData>"
    );
    assert_eq!(
      fetcher.calls.load(Ordering::SeqCst),
      1,
      "cache hit should avoid re-fetching iframe HTML"
    );
  }

  #[test]
  fn iframe_render_cache_partitions_by_navigation_referrer_url() {
    let font_ctx = FontContext::new();
    let iframe_url = "https://example.com/iframe-render-cache-referrer-url-113.html";
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(25, 50, 75); }</style>
      <div data-fastr-test="iframe-render-cache-referrer-url-113"></div>
    "#;
    let fetcher = Arc::new(CountingIframeFetcher {
      iframe_url: iframe_url.to_string(),
      body: html.as_bytes().to_vec(),
      iframe_calls: AtomicUsize::new(0),
      iframe_requests: StdMutex::new(Vec::new()),
    });
    let mut image_cache = ImageCache::with_fetcher(fetcher.clone());
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

    image_cache.set_resource_context(Some(ResourceContext {
      document_url: Some("https://parent-a.test/page-a.html".to_string()),
      referrer_policy: ReferrerPolicy::OriginWhenCrossOrigin,
      policy: ResourceAccessPolicy::default(),
      csp: None,
      diagnostics: None,
      iframe_depth_remaining: None,
      iframe_embedder: None,
    }));
    render_iframe_src(
      iframe_url,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("first iframe render should succeed");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "first render should miss cache"
    );

    image_cache.set_resource_context(Some(ResourceContext {
      document_url: Some("https://parent-b.test/page-b.html".to_string()),
      referrer_policy: ReferrerPolicy::OriginWhenCrossOrigin,
      policy: ResourceAccessPolicy::default(),
      csp: None,
      diagnostics: None,
      iframe_depth_remaining: None,
      iframe_embedder: None,
    }));
    render_iframe_src(
      iframe_url,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("second iframe render should succeed");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "second render should miss cache due to different navigation referrer URL"
    );
    assert_eq!(
      fetcher.iframe_calls.load(Ordering::SeqCst),
      2,
      "expected iframe HTML to be fetched again when the parent referrer URL changes"
    );

    let requests = fetcher
      .iframe_requests
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .clone();
    assert_eq!(
      requests,
      vec![
        (
          Some("https://parent-a.test/page-a.html".to_string()),
          ReferrerPolicy::OriginWhenCrossOrigin
        ),
        (
          Some("https://parent-b.test/page-b.html".to_string()),
          ReferrerPolicy::OriginWhenCrossOrigin
        ),
      ],
      "expected iframe fetch requests to use the active document URL as the referrer"
    );
  }

  #[test]
  fn iframe_render_cache_partitions_by_effective_navigation_referrer_policy() {
    let font_ctx = FontContext::new();
    let iframe_url = "https://example.com/iframe-render-cache-referrer-policy-113.html";
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(85, 40, 15); }</style>
      <div data-fastr-test="iframe-render-cache-referrer-policy-113"></div>
    "#;
    let fetcher = Arc::new(CountingIframeFetcher {
      iframe_url: iframe_url.to_string(),
      body: html.as_bytes().to_vec(),
      iframe_calls: AtomicUsize::new(0),
      iframe_requests: StdMutex::new(Vec::new()),
    });
    let mut image_cache = ImageCache::with_fetcher(fetcher.clone());
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

    image_cache.set_resource_context(Some(ResourceContext {
      document_url: Some("https://parent.test/shared.html".to_string()),
      referrer_policy: ReferrerPolicy::Origin,
      policy: ResourceAccessPolicy::default(),
      csp: None,
      diagnostics: None,
      iframe_depth_remaining: None,
      iframe_embedder: None,
    }));
    render_iframe_src(
      iframe_url,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("first iframe render should succeed");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "first render should miss cache"
    );

    image_cache.set_resource_context(Some(ResourceContext {
      document_url: Some("https://parent.test/shared.html".to_string()),
      referrer_policy: ReferrerPolicy::NoReferrer,
      policy: ResourceAccessPolicy::default(),
      csp: None,
      diagnostics: None,
      iframe_depth_remaining: None,
      iframe_embedder: None,
    }));
    render_iframe_src(
      iframe_url,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("second iframe render should succeed");
    assert_eq!(
      take_last_iframe_cache_hit(),
      Some(false),
      "second render should miss cache due to different effective referrer policy"
    );
    assert_eq!(
      fetcher.iframe_calls.load(Ordering::SeqCst),
      2,
      "expected iframe HTML to be fetched again when the effective referrer policy changes"
    );

    let requests = fetcher
      .iframe_requests
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .clone();
    assert_eq!(
      requests,
      vec![
        (
          Some("https://parent.test/shared.html".to_string()),
          ReferrerPolicy::Origin
        ),
        (
          Some("https://parent.test/shared.html".to_string()),
          ReferrerPolicy::NoReferrer
        ),
      ],
      "expected iframe fetch requests to reflect the active document referrer policy"
    );
  }

  #[test]
  fn iframe_src_allows_cross_origin_documents_when_same_origin_subresources_enabled() {
    let font_ctx = FontContext::new();
    let url = "https://other.test/iframe-cross-origin-policy-113.html";
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(10, 20, 30); }</style>
      <div data-fastr-test="iframe-cross-origin-policy-113"></div>
    "#;
    let fetcher = Arc::new(HtmlFetcher {
      expected_url: url.to_string(),
      body: html.as_bytes().to_vec(),
      calls: AtomicUsize::new(0),
    });
    let mut image_cache = ImageCache::with_fetcher(fetcher.clone());
    image_cache.set_resource_context(Some(ResourceContext {
      document_url: Some("https://example.test/".to_string()),
      referrer_policy: Default::default(),
      policy: ResourceAccessPolicy {
        document_origin: origin_from_url("https://example.test/"),
        same_origin_only: true,
        ..ResourceAccessPolicy::default()
      },
      csp: None,
      diagnostics: None,
      iframe_depth_remaining: None,
      iframe_embedder: None,
    }));
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

    let result = render_iframe_src(url, None, rect, None, &image_cache, &font_ctx, 1.0, 3);
    assert!(
      result.is_some(),
      "expected cross-origin iframe document to render even when same-origin subresource policy is enabled"
    );
    assert_eq!(
      fetcher.calls.load(Ordering::SeqCst),
      1,
      "expected iframe HTML fetch to occur (not be blocked by same-origin subresource policy)"
    );
  }

  #[test]
  fn iframe_src_uses_final_url_for_relative_resolution() {
    let font_ctx = FontContext::new();
    let requested_url = "https://example.com/original/iframe-redirect-113.html";
    let final_url = "https://example.com/final/iframe-redirect-113.html";
    let expected_css_url = "https://example.com/final/style-redirect-113.css";
    let fetcher = Arc::new(RedirectingStylesheetFetcher {
      doc_url: requested_url.to_string(),
      final_url: final_url.to_string(),
      css_url: expected_css_url.to_string(),
      requests: StdMutex::new(Vec::new()),
    });
    let image_cache = ImageCache::with_fetcher(fetcher.clone());
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

    render_iframe_src(
      requested_url,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("iframe src render should succeed");

    let urls = fetcher
      .requests
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .clone();
    assert!(
      urls.iter().any(|u| u == expected_css_url),
      "expected stylesheet URL {expected_css_url} to be requested; got {urls:?}"
    );
    assert!(
      !urls
        .iter()
        .any(|u| u == "https://example.com/original/style-redirect-113.css"),
      "stylesheet should resolve relative to the final URL, not the requested URL; got {urls:?}"
    );
  }

  #[test]
  fn iframe_referrer_policy_response_header_applies_to_subresources() {
    let font_ctx = FontContext::new();
    let iframe_url = "https://example.com/iframe-referrer-policy-header-113.html";
    let css_url = "https://example.com/style.css";
    let observed = Arc::new(StdMutex::new(None));
    let fetcher = Arc::new(ReferrerPolicyHeaderIframeFetcher::new(
      iframe_url,
      css_url,
      Arc::clone(&observed),
    ));

    let mut image_cache = ImageCache::with_fetcher(fetcher);
    image_cache.set_resource_context(Some(ResourceContext {
      document_url: Some("https://parent.test/".to_string()),
      referrer_policy: ReferrerPolicy::default(),
      policy: ResourceAccessPolicy::default(),
      csp: None,
      diagnostics: None,
      iframe_depth_remaining: None,
      iframe_embedder: None,
    }));

    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);
    render_iframe_src(
      iframe_url,
      None,
      rect,
      None,
      &image_cache,
      &font_ctx,
      1.0,
      3,
    )
    .expect("iframe render should succeed");

    let seen = observed.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
      *seen,
      Some(ReferrerPolicy::NoReferrer),
      "expected iframe subresource requests to inherit Referrer-Policy response header"
    );
  }

  #[test]
  fn iframe_render_cache_deduplicates_inflight_srcdoc_renders() {
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::with_fetcher(Arc::new(RejectingFetcher::default()));
    let rect = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);
    let html = r#"
      <style>html, body { margin: 0; padding: 0; background: rgb(0, 0, 255); }</style>
      <div data-fastr-test="iframe-render-cache-inflight-113"></div>
    "#;

    let key = IframeRenderCacheKey {
      image_cache_id: image_cache.instance_id(),
      content: IframeRenderCacheContent::Srcdoc {
        html_hash: stable_hash_bytes(html.as_bytes()),
        base_url: None,
      },
      css_width: 16,
      css_height: 16,
      device_pixel_ratio_bits: 1.0f32.to_bits(),
      nested_depth: 2,
      background: Rgba::WHITE.into(),
      policy_hash: policy_fingerprint(&ResourceAccessPolicy::default()),
      referrer_url_hash: None,
      request_referrer_policy: ReferrerPolicy::default(),
    };
    let barrier = Arc::new(Barrier::new(2));
    let owners = Arc::new(AtomicUsize::new(0));
    let waiters = Arc::new(AtomicUsize::new(0));

    struct HookReset;
    impl Drop for HookReset {
      fn drop(&mut self) {
        set_iframe_render_cache_join_hook(None);
      }
    }
    let _reset = HookReset;

    let key_for_hook = key.clone();
    let barrier_for_hook = Arc::clone(&barrier);
    let owners_for_hook = Arc::clone(&owners);
    let waiters_for_hook = Arc::clone(&waiters);
    set_iframe_render_cache_join_hook(Some(Arc::new(move |hook_key, is_owner| {
      if hook_key != &key_for_hook {
        return;
      }
      if is_owner {
        owners_for_hook.fetch_add(1, Ordering::SeqCst);
      } else {
        waiters_for_hook.fetch_add(1, Ordering::SeqCst);
      }
      barrier_for_hook.wait();
    })));

    let cache1 = image_cache.clone();
    let cache2 = image_cache.clone();
    let font1 = font_ctx.clone();
    let font2 = font_ctx.clone();
    let t1 = std::thread::spawn(move || {
      render_iframe_srcdoc(html, None, None, rect, None, &cache1, &font1, 1.0, 3)
        .expect("thread1 iframe render")
    });
    let t2 = std::thread::spawn(move || {
      render_iframe_srcdoc(html, None, None, rect, None, &cache2, &font2, 1.0, 3)
        .expect("thread2 iframe render")
    });
    let first = t1.join().expect("join thread1");
    let second = t2.join().expect("join thread2");

    assert_eq!(
      owners.load(Ordering::SeqCst),
      1,
      "exactly one thread should render the iframe"
    );
    assert_eq!(
      waiters.load(Ordering::SeqCst),
      1,
      "the other thread should wait on the in-flight render"
    );
    assert!(
      Arc::ptr_eq(&first, &second),
      "in-flight waiters should receive the same Arc<ImageData>"
    );
  }
}

#[cfg(test)]
mod diagnostics_tests {
  use super::*;
  use crate::api::{ResourceContext, ResourceKind, SharedRenderDiagnostics};
  use crate::error::{Error, ResourceError, Result};
  use crate::geometry::{Point, Size};
  use crate::resource::{FetchedResource, ResourceFetcher};
  use crate::text::font_db::FontDatabase;
  use std::sync::Arc;

  struct MockFetcher {
    handler: Box<dyn Fn(&str) -> Result<FetchedResource> + Send + Sync>,
  }

  impl MockFetcher {
    fn new<F>(handler: F) -> Self
    where
      F: Fn(&str) -> Result<FetchedResource> + Send + Sync + 'static,
    {
      Self {
        handler: Box::new(handler),
      }
    }
  }

  impl ResourceFetcher for MockFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      (self.handler)(url)
    }
  }

  fn test_font_context() -> FontContext {
    FontContext::with_database(Arc::new(FontDatabase::empty()))
  }

  fn test_image_cache(
    fetcher: Arc<dyn ResourceFetcher>,
    diagnostics: SharedRenderDiagnostics,
  ) -> ImageCache {
    let mut cache = ImageCache::with_fetcher(fetcher);
    cache.set_resource_context(Some(ResourceContext {
      diagnostics: Some(diagnostics),
      ..ResourceContext::default()
    }));
    cache
  }

  fn test_image_cache_with_iframe_depth(
    fetcher: Arc<dyn ResourceFetcher>,
    diagnostics: SharedRenderDiagnostics,
    iframe_depth_remaining: usize,
  ) -> ImageCache {
    let mut cache = ImageCache::with_fetcher(fetcher);
    cache.set_resource_context(Some(ResourceContext {
      diagnostics: Some(diagnostics),
      iframe_depth_remaining: Some(iframe_depth_remaining),
      ..ResourceContext::default()
    }));
    cache
  }

  #[test]
  fn iframe_fetch_network_error_records_diagnostics() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| {
      Err(Error::Resource(ResourceError::new(
        url.to_string(),
        "network error".to_string(),
      )))
    }));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      "/bad-network",
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_none());

    let diag = diagnostics.into_inner();
    assert!(
      diag
        .fetch_errors
        .iter()
        .any(|e| e.kind == ResourceKind::Document && e.url == "/bad-network"),
      "expected iframe fetch error diagnostic"
    );
  }

  #[test]
  fn iframe_http_error_status_records_diagnostics() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|_url| {
      let mut resource =
        FetchedResource::new(b"<html></html>".to_vec(), Some("text/html".to_string()));
      resource.status = Some(403);
      Ok(resource)
    }));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      "/bad-status",
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(
      result.is_some(),
      "expected iframe HTML to render even on an HTTP error status"
    );

    let diag = diagnostics.into_inner();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Document && e.url == "/bad-status")
      .expect("expected iframe fetch error diagnostic");
    assert_eq!(entry.status, Some(403));
  }

  #[test]
  fn iframe_about_blank_does_not_record_fetch_errors() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| panic!("unexpected fetch: {url}")));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      "about:blank",
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_some());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.is_empty(),
      "expected no diagnostics for about:blank, got {diag:?}"
    );
  }

  #[test]
  fn iframe_whitespace_src_does_not_fetch() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| panic!("unexpected fetch: {url}")));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      "   ",
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_none());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.is_empty(),
      "expected no diagnostics for whitespace src, got {diag:?}"
    );
  }

  #[test]
  fn iframe_fragment_only_src_does_not_fetch() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| panic!("unexpected fetch: {url}")));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src("#", None, rect, None, &cache, &test_font_context(), 1.0, 1);
    assert!(result.is_none());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.is_empty(),
      "expected no diagnostics for fragment-only src, got {diag:?}"
    );
  }

  #[test]
  fn iframe_javascript_src_does_not_fetch() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| panic!("unexpected fetch: {url}")));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      "javascript:alert(1)",
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_none());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.is_empty(),
      "expected no diagnostics for javascript: src, got {diag:?}"
    );
  }

  #[test]
  fn iframe_src_trims_whitespace_before_fetching() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| {
      assert_eq!(url, "https://example.com/");
      Ok(FetchedResource::new(
        b"<html></html>".to_vec(),
        Some("text/html".to_string()),
      ))
    }));
    let cache = test_image_cache(fetcher, diagnostics.clone());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      " \t  https://example.com",
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_some());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.is_empty(),
      "expected no diagnostics for trimmed src, got {diag:?}"
    );
  }

  #[test]
  fn iframe_src_does_not_trim_non_ascii_whitespace() {
    let diagnostics = SharedRenderDiagnostics::new();
    let nbsp = "\u{00A0}";
    let fetcher = Arc::new(MockFetcher::new(move |url| {
      assert_eq!(url, "https://example.com/foo%C2%A0");
      Ok(FetchedResource::new(
        b"<html></html>".to_vec(),
        Some("text/html".to_string()),
      ))
    }));
    let mut cache = test_image_cache(fetcher, diagnostics.clone());
    cache.set_base_url("https://example.com/".to_string());
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_src(
      &format!("foo{nbsp}"),
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_some());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.is_empty(),
      "expected no diagnostics for NBSP src, got {diag:?}"
    );
  }

  #[test]
  fn iframe_srcdoc_whitespace_src_records_nesting_violation_for_about_srcdoc() {
    let diagnostics = SharedRenderDiagnostics::new();
    let fetcher = Arc::new(MockFetcher::new(|url| panic!("unexpected fetch: {url}")));
    let cache = test_image_cache_with_iframe_depth(fetcher, diagnostics.clone(), 0);
    let rect = Rect::new(Point::ZERO, Size::new(10.0, 10.0));

    let result = render_iframe_srcdoc(
      "<html></html>",
      Some("   "),
      None,
      rect,
      None,
      &cache,
      &test_font_context(),
      1.0,
      1,
    );
    assert!(result.is_some());

    let diag = diagnostics.into_inner();
    assert!(
      diag.fetch_errors.iter().any(|e| {
        e.kind == ResourceKind::Document
          && e.url == "about:srcdoc"
          && e.message == IFRAME_NESTING_LIMIT_MESSAGE
      }),
      "expected about:srcdoc nesting violation, got {diag:?}"
    );
  }
}
