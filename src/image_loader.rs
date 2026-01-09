//! Image loading and caching
//!
//! This module provides image loading from various sources (HTTP, file, data URLs)
//! with in-memory caching and support for various image formats including SVG.

use crate::api::{RenderDiagnostics, ResourceContext, ResourceKind};
use crate::debug::runtime;
use crate::error::{Error, ImageError, RenderError, RenderStage, Result};
use crate::paint::painter::with_paint_diagnostics;
use crate::paint::pixmap::{new_pixmap, new_pixmap_with_context, MAX_PIXMAP_BYTES};
use crate::render_control::{self, check_root, check_root_periodic};
use crate::resource::CacheArtifactKind;
use crate::resource::CachingFetcher;
use crate::resource::CachingFetcherConfig;
use crate::resource::FetchContextKind;
use crate::resource::FetchCredentialsMode;
use crate::resource::FetchDestination;
use crate::resource::FetchRequest;
use crate::resource::FetchedResource;
use crate::resource::HttpFetcher;
use crate::resource::ReferrerPolicy;
use crate::resource::ResourceFetcher;
use crate::resource::{ensure_http_success, ensure_image_mime_sane, origin_from_url};
use crate::style::color::Rgba;
use crate::style::types::ImageResolution;
use crate::style::types::OrientationTransform;
use crate::text::font_db::FontConfig;
use crate::url_normalize::normalize_url_reference_for_resolution;
use crate::svg::{
  map_svg_aspect_ratio, parse_svg_length_px, parse_svg_view_box,
  svg_intrinsic_dimensions_from_attributes, svg_view_box_root_transform, SvgPreserveAspectRatio,
  SvgViewBox,
};
use crate::tree::box_tree::CrossOriginAttribute;
use avif_decode::Decoder as AvifDecoder;
use avif_decode::Image as AvifImage;
use avif_parse::AvifData;
use exif;
use flate2::read::GzDecoder;
use image::imageops;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageDecoder;
use image::ImageFormat;
use image::ImageReader;
use image::RgbaImage;
use lru::LruCache;
use percent_encoding::percent_decode_str;
use roxmltree::Document;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tiny_skia::{FilterQuality, IntSize, Pixmap};
use url::Url;

fn shared_svg_fontdb() -> Arc<resvg::usvg::fontdb::Database> {
  static SVG_FONT_DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
  Arc::clone(SVG_FONT_DB.get_or_init(|| {
    // usvg text shaping requires an explicit font database; otherwise `<text>` can be dropped
    // entirely. Populate the fontdb once and share it across SVG parses.
    //
    // Note: resvg/usvg currently depend on `fontdb` v0.21, while FastRender's HTML text engine
    // uses `fontdb` v0.23. The types are incompatible, so we load fonts directly into usvg's
    // fontdb (reusing the same in-repo bundled font *bytes* for deterministic runs).
    //
    // Respect FastRender's "bundled fonts" mode in CI (`CI` or `FASTR_USE_BUNDLED_FONTS=1`) by
    // skipping system font discovery when disabled.
    let config = FontConfig::default();
    let mut db = resvg::usvg::fontdb::Database::new();
    if config.use_system_fonts {
      db.load_system_fonts();
    }
    if config.use_bundled_fonts {
      for data in crate::text::font_db::bundled_font_data() {
        db.load_font_data(data.to_vec());
      }
      if crate::text::font_db::bundled_emoji_fonts_enabled() {
        for data in crate::text::font_db::bundled_emoji_font_data() {
          db.load_font_data(data.to_vec());
        }
      }
      if !config.use_system_fonts {
        // Match FastRender's bundled-font defaults: prefer stable Noto families for generic
        // `serif`/`sans-serif`/`monospace` resolution in hermetic runs.
        db.set_serif_family("Noto Serif".to_string());
        db.set_sans_serif_family("Noto Sans".to_string());
        db.set_monospace_family("Noto Sans Mono".to_string());
        db.set_cursive_family("Noto Sans".to_string());
        db.set_fantasy_family("Noto Sans".to_string());
      }
    }
    // Always load a tiny deterministic fixture font so SVG text renders even in minimal
    // environments (and tests remain hermetic).
    db.load_font_data(include_bytes!("../tests/fixtures/fonts/Cantarell-Test.ttf").to_vec());
    Arc::new(db)
  }))
}

fn usvg_options_for_url(url: &str) -> resvg::usvg::Options<'_> {
  let mut options = resvg::usvg::Options::default();
  options.fontdb = shared_svg_fontdb();

  if let Ok(parsed) = Url::parse(url) {
    if parsed.scheme() == "file" {
      if let Ok(path) = parsed.to_file_path() {
        if let Some(dir) = path.parent() {
          options.resources_dir =
            std::fs::canonicalize(dir).ok().or_else(|| Some(dir.to_path_buf()));
        }
      }
    }
  }

  options
}

fn fetch_credentials_mode_for_crossorigin(
  crossorigin: CrossOriginAttribute,
) -> FetchCredentialsMode {
  match crossorigin {
    CrossOriginAttribute::None => FetchCredentialsMode::Include,
    CrossOriginAttribute::Anonymous => crate::resource::CorsMode::Anonymous.credentials_mode(),
    CrossOriginAttribute::UseCredentials => crate::resource::CorsMode::UseCredentials.credentials_mode(),
  }
}

fn unescape_js_escapes(input: &str) -> Cow<'_, str> {
  if !input.contains('\\') {
    return Cow::Borrowed(input);
  }

  let mut out = String::with_capacity(input.len());
  let bytes = input.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\' {
      if i + 1 < bytes.len()
        && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'' || bytes[i + 1] == b'/')
      {
        out.push(bytes[i + 1] as char);
        i += 2;
        continue;
      }

      if i + 5 < bytes.len() && (bytes[i + 1] == b'u' || bytes[i + 1] == b'U') {
        if let Ok(code) = u16::from_str_radix(&input[i + 2..i + 6], 16) {
          if let Some(ch) = char::from_u32(code as u32) {
            out.push(ch);
            i += 6;
            continue;
          }
        }
      }
    }

    out.push(bytes[i] as char);
    i += 1;
  }

  Cow::Owned(out)
}

// HTML/CSS URL-ish values strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
// treat all Unicode whitespace (e.g. NBSP) as ignorable.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn trim_ascii_whitespace_start(value: &str) -> &str {
  value.trim_start_matches(|c: char| {
    matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
  })
}

fn svg_xml_base_chain_for_node<'a, 'input>(node: roxmltree::Node<'a, 'input>) -> Vec<&'a str> {
  let mut chain = Vec::new();
  for ancestor in node.ancestors().filter(|n| n.is_element()) {
    // `roxmltree` exposes namespaced attributes via `{namespace, name}` (without the prefix). For
    // `xml:base` the namespace is the standard XML namespace.
    const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
    let xml_base = ancestor.attribute("xml:base").or_else(|| {
      ancestor
        .attributes()
        .find(|attr| attr.name() == "base" && attr.namespace() == Some(XML_NS))
        .map(|attr| attr.value())
    });
    if let Some(value) = xml_base {
      let value = trim_ascii_whitespace(value);
      if !value.is_empty() {
        chain.push(value);
      }
    }
  }
  chain.reverse();
  chain
}

fn apply_svg_xml_base_chain(base: Option<&str>, chain: &[&str]) -> Option<String> {
  let mut current = base.map(|base| base.to_string());
  for value in chain {
    let value = trim_ascii_whitespace(value);
    if value.is_empty() {
      continue;
    }
    // If `xml:base` is itself an absolute URL, it resets the base URI.
    if Url::parse(value).is_ok() {
      current = Some(value.to_string());
      continue;
    }
    let Some(cur) = current.as_deref() else {
      return None;
    };
    current = resolve_against_base(cur, value);
  }
  current
}

fn strip_url_fragment(url: &str) -> Cow<'_, str> {
  url
    .split_once('#')
    .map(|(prefix, _)| Cow::Borrowed(prefix))
    .unwrap_or(Cow::Borrowed(url))
}

fn decode_inline_svg_url(url: &str) -> Option<String> {
  let trimmed = trim_ascii_whitespace_start(url);
  if trimmed.is_empty() {
    return None;
  }

  if trimmed.starts_with('<') {
    return Some(unescape_js_escapes(trimmed).into_owned());
  }

  // Inline SVG markup sometimes appears percent-encoded (e.g. `%3Csvg ...`). Treat it the same as
  // raw `<svg...>` strings so the renderer doesn't accidentally resolve it as a network URL.
  if trimmed
    .get(.."%3csvg".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%3csvg"))
  {
    if let Ok(decoded) = percent_decode_str(trimmed).decode_utf8() {
      let decoded = decoded.into_owned();
      let decoded_trimmed = trim_ascii_whitespace_start(&decoded);
      if decoded_trimmed.starts_with('<') {
        return Some(unescape_js_escapes(decoded_trimmed).into_owned());
      }
    }
  }

  None
}

fn image_profile_threshold_ms() -> Option<f64> {
  runtime::runtime_toggles().f64("FASTR_IMAGE_PROFILE_MS")
}

fn image_probe_max_bytes() -> usize {
  runtime::runtime_toggles()
    .usize("FASTR_IMAGE_PROBE_MAX_BYTES")
    .unwrap_or(64 * 1024)
    .clamp(1, 16 * 1024 * 1024)
}

const PANIC_REASON_MAX_BYTES: usize = 1024;
const MAX_SVGZ_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;

fn truncate_utf8_at_boundary(input: &str, max_bytes: usize) -> &str {
  if input.len() <= max_bytes {
    return input;
  }
  let mut end = max_bytes;
  while end > 0 && !input.is_char_boundary(end) {
    end -= 1;
  }
  &input[..end]
}

fn panic_payload_to_reason(panic: &(dyn std::any::Any + Send)) -> String {
  let message = panic
    .downcast_ref::<&str>()
    .map(|msg| (*msg).to_string())
    .or_else(|| panic.downcast_ref::<String>().cloned())
    .unwrap_or_else(|| "<unknown panic>".to_string());
  truncate_utf8_at_boundary(&message, PANIC_REASON_MAX_BYTES).to_string()
}

/// Image cache diagnostics collection.
#[derive(Debug, Default, Clone)]
pub struct ImageCacheDiagnostics {
  pub requests: usize,
  pub cache_hits: usize,
  pub cache_misses: usize,
  pub decode_ms: f64,
  /// Number of `ResourceFetcher::fetch_partial` calls made by image probes.
  pub probe_partial_requests: usize,
  /// Total bytes returned by partial probe fetches (sum of response body prefixes).
  pub probe_partial_bytes_total: usize,
  /// Number of times an image probe attempted partial fetches but ultimately fell back to a full
  /// `fetch()` due to insufficient bytes / unsupported partial responses.
  pub probe_partial_fallback_full: usize,
  pub raster_pixmap_cache_hits: usize,
  pub raster_pixmap_cache_misses: usize,
  /// Maximum cached bytes observed for the raster pixmap cache during the diagnostic window.
  pub raster_pixmap_cache_bytes: usize,
}

thread_local! {
  static IMAGE_CACHE_DIAGNOSTICS_ACTIVE: Cell<bool> = const { Cell::new(false) };
}
static IMAGE_CACHE_DIAGNOSTICS: Mutex<Option<ImageCacheDiagnostics>> = Mutex::new(None);
static NEXT_IMAGE_CACHE_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn enable_image_cache_diagnostics() {
  IMAGE_CACHE_DIAGNOSTICS_ACTIVE.with(|active| active.set(true));
  let mut guard = IMAGE_CACHE_DIAGNOSTICS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  *guard = Some(ImageCacheDiagnostics::default());
}

pub(crate) fn take_image_cache_diagnostics() -> Option<ImageCacheDiagnostics> {
  IMAGE_CACHE_DIAGNOSTICS_ACTIVE.with(|active| active.set(false));
  IMAGE_CACHE_DIAGNOSTICS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .take()
}

#[inline]
fn with_image_cache_diagnostics<F: FnOnce(&mut ImageCacheDiagnostics)>(f: F) {
  if !IMAGE_CACHE_DIAGNOSTICS_ACTIVE.with(|active| active.get()) {
    return;
  }
  let mut guard = IMAGE_CACHE_DIAGNOSTICS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  if let Some(stats) = guard.as_mut() {
    f(stats);
  }
}

fn record_image_cache_request() {
  with_image_cache_diagnostics(|stats| stats.requests += 1);
}

fn record_image_cache_hit() {
  with_image_cache_diagnostics(|stats| stats.cache_hits += 1);
}

fn record_image_cache_miss() {
  with_image_cache_diagnostics(|stats| stats.cache_misses += 1);
}

fn record_image_decode_ms(duration_ms: f64) {
  if !duration_ms.is_finite() || duration_ms <= 0.0 {
    return;
  }
  with_image_cache_diagnostics(|stats| stats.decode_ms += duration_ms);
}

fn record_probe_partial_fetch(bytes: usize) {
  with_image_cache_diagnostics(|stats| {
    stats.probe_partial_requests += 1;
    stats.probe_partial_bytes_total = stats.probe_partial_bytes_total.saturating_add(bytes);
  });
}

fn record_probe_partial_fallback_full() {
  with_image_cache_diagnostics(|stats| stats.probe_partial_fallback_full += 1);
}

fn record_raster_pixmap_cache_hit() {
  with_image_cache_diagnostics(|stats| stats.raster_pixmap_cache_hits += 1);
}

fn record_raster_pixmap_cache_miss() {
  with_image_cache_diagnostics(|stats| stats.raster_pixmap_cache_misses += 1);
}

fn record_raster_pixmap_cache_bytes(bytes: usize) {
  with_image_cache_diagnostics(|stats| {
    stats.raster_pixmap_cache_bytes = stats.raster_pixmap_cache_bytes.max(bytes);
  });
}

const IMAGE_DECODE_DEADLINE_STRIDE: usize = 8192;

struct DeadlineCursor<'a> {
  inner: Cursor<&'a [u8]>,
  deadline_counter: usize,
}

impl<'a> DeadlineCursor<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self {
      inner: Cursor::new(bytes),
      deadline_counter: 0,
    }
  }

  fn check_deadline(&mut self) -> io::Result<()> {
    check_root_periodic(
      &mut self.deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
  }
}

impl<'a> Read for DeadlineCursor<'a> {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    self.check_deadline()?;
    self.inner.read(buf)
  }
}

impl<'a> BufRead for DeadlineCursor<'a> {
  fn fill_buf(&mut self) -> io::Result<&[u8]> {
    self.check_deadline()?;
    self.inner.fill_buf()
  }

  fn consume(&mut self, amt: usize) {
    self.inner.consume(amt);
  }
}

impl<'a> Seek for DeadlineCursor<'a> {
  fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
    self.check_deadline()?;
    self.inner.seek(pos)
  }
}

enum AvifDecodeError {
  Timeout(RenderError),
  Image(image::ImageError),
}

impl From<RenderError> for AvifDecodeError {
  fn from(err: RenderError) -> Self {
    Self::Timeout(err)
  }
}

impl From<image::ImageError> for AvifDecodeError {
  fn from(err: image::ImageError) -> Self {
    Self::Image(err)
  }
}

#[derive(Clone)]
struct CacheEntry<V> {
  value: V,
  bytes: usize,
}

struct SizedLruCache<K, V> {
  inner: LruCache<K, CacheEntry<V>>,
  max_entries: Option<usize>,
  max_bytes: Option<usize>,
  current_bytes: usize,
}

impl<K: Eq + Hash, V> SizedLruCache<K, V> {
  fn new(max_entries: usize, max_bytes: usize) -> Self {
    Self {
      inner: LruCache::unbounded(),
      max_entries: (max_entries > 0).then_some(max_entries),
      max_bytes: (max_bytes > 0).then_some(max_bytes),
      current_bytes: 0,
    }
  }

  fn get_cloned<Q>(&mut self, key: &Q) -> Option<V>
  where
    V: Clone,
    K: Borrow<Q>,
    Q: Hash + Eq + ?Sized,
  {
    self.inner.get(key).map(|entry| entry.value.clone())
  }

  fn insert(&mut self, key: K, value: V, bytes: usize) {
    if let Some(entry) = self.inner.pop(&key) {
      self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
    }
    self.inner.put(key, CacheEntry { value, bytes });
    self.current_bytes = self.current_bytes.saturating_add(bytes);
    self.evict_if_needed();
  }

  fn evict_if_needed(&mut self) {
    while self
      .max_entries
      .is_some_and(|limit| self.inner.len() > limit)
      || self
        .max_bytes
        .is_some_and(|limit| self.current_bytes > limit)
    {
      if let Some((_key, entry)) = self.inner.pop_lru() {
        self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
      } else {
        break;
      }
    }
  }

  fn current_bytes(&self) -> usize {
    self.current_bytes
  }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct SvgPixmapKey {
  hash: u64,
  url_hash: u64,
  len: usize,
  width: u32,
  height: u32,
  device_pixel_ratio_bits: u32,
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct RasterPixmapKey {
  url_hash: u64,
  len: usize,
  orientation: OrientationTransform,
  decorative: bool,
  target_width: u32,
  target_height: u32,
  quality_bits: u8,
}

fn raster_pixmap_key(
  url: &str,
  orientation: OrientationTransform,
  decorative: bool,
  target_width: u32,
  target_height: u32,
  quality_bits: u8,
) -> RasterPixmapKey {
  let mut url_hasher = DefaultHasher::new();
  url.hash(&mut url_hasher);
  RasterPixmapKey {
    url_hash: url_hasher.finish(),
    len: url.len(),
    orientation,
    decorative,
    target_width,
    target_height,
    quality_bits,
  }
}

fn raster_pixmap_quality_bits(quality: FilterQuality) -> u8 {
  match quality {
    FilterQuality::Nearest => 0,
    FilterQuality::Bilinear => 1,
    // tiny-skia currently exposes a small fixed set of qualities. Store a conservative default so
    // future additions don't break hashing behaviour.
    _ => 255,
  }
}

fn raster_pixmap_full_key(
  url: &str,
  orientation: OrientationTransform,
  decorative: bool,
) -> RasterPixmapKey {
  raster_pixmap_key(url, orientation, decorative, 0, 0, 0)
}

fn svg_pixmap_key(
  svg_content: &str,
  url: &str,
  device_pixel_ratio: f32,
  width: u32,
  height: u32,
) -> SvgPixmapKey {
  let mut content_hasher = DefaultHasher::new();
  svg_content.hash(&mut content_hasher);
  let mut url_hasher = DefaultHasher::new();
  url.hash(&mut url_hasher);
  SvgPixmapKey {
    hash: content_hasher.finish(),
    url_hash: url_hasher.finish(),
    len: svg_content.len(),
    width,
    height,
    device_pixel_ratio_bits: f32_to_canonical_bits(device_pixel_ratio),
  }
}

fn inline_svg_cache_key(svg_content: &str) -> String {
  let mut hasher = DefaultHasher::new();
  svg_content.hash(&mut hasher);
  format!("inline-svg:{:016x}:{}", hasher.finish(), svg_content.len())
}

fn escape_xml_attr_value(value: &str) -> Cow<'_, str> {
  if !value.contains('&')
    && !value.contains('<')
    && !value.contains('>')
    && !value.contains('"')
    && !value.contains('\'')
  {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&apos;"),
      other => out.push(other),
    }
  }
  Cow::Owned(out)
}

fn append_url_fragment_if_missing<'a>(base_url: &'a str, requested_url: &str) -> Cow<'a, str> {
  if base_url.contains('#') {
    return Cow::Borrowed(base_url);
  }
  let Some((_, fragment)) = requested_url.split_once('#') else {
    return Cow::Borrowed(base_url);
  };
  if fragment.is_empty() {
    return Cow::Borrowed(base_url);
  }

  let mut out = String::with_capacity(base_url.len().saturating_add(fragment.len() + 1));
  out.push_str(base_url);
  out.push('#');
  out.push_str(fragment);
  Cow::Owned(out)
}

#[derive(Debug, Clone)]
struct SvgUseInlineElement {
  tag_name: String,
  range: std::ops::Range<usize>,
  inner_range: Option<std::ops::Range<usize>>,
  view_box: Option<String>,
  preserve_aspect_ratio: Option<String>,
}

#[derive(Debug, Clone)]
struct SvgUseInlineSprite {
  content: String,
  by_id: HashMap<String, SvgUseInlineElement>,
}

/// Best-effort preprocessor that expands external `<use href="sprite.svg#id">` references by
/// fetching the referenced SVG and inlining the matched `id` element.
///
/// This is intentionally narrow: it exists because `usvg`/`resvg` do not fetch HTTP(S) external
/// `<use>` targets by default, which causes common SVG sprite patterns to silently disappear.
fn inline_svg_use_references<'a>(
  svg_content: &'a str,
  svg_url: &str,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
) -> Result<Cow<'a, str>> {
  // Avoid parsing unless it looks like we might have `<use>` references.
  // Note: inline SVG content in HTML is sometimes namespaced (e.g. `<svg:use>`). The cheap string
  // check is intentionally conservative to avoid parsing most SVGs while still catching prefixed
  // `<use>` tags.
  if !svg_content.contains("<use") && !svg_content.contains(":use") {
    return Ok(Cow::Borrowed(svg_content));
  }

  const MAX_USE_EXPANSIONS: usize = 128;
  const MAX_INJECTED_BYTES: usize = 512 * 1024;

  check_root(RenderStage::Paint).map_err(Error::Render)?;

  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_content)
  })) {
    Ok(Ok(doc)) => doc,
    // Best-effort: if the SVG doesn't parse, don't fail the entire image decode.
    Ok(Err(_)) | Err(_) => return Ok(Cow::Borrowed(svg_content)),
  };

  let mut deadline_counter = 0usize;
  let mut sprite_cache: HashMap<String, SvgUseInlineSprite> = HashMap::new();
  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
  let mut injected_bytes = 0usize;
  let mut expansions = 0usize;

  for node in doc.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(Error::Render)?;

    if node.tag_name().name() != "use" {
      continue;
    }

    let mut href = None;
    for attr in node.attributes() {
      let name = attr.name();
      if name.eq_ignore_ascii_case("href")
        || name
          .rsplit_once(':')
          .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
      {
        href = Some(attr.value());
        break;
      }
    }
    if href.is_none() {
      for attr in node.attributes() {
        if attr.name() == "xlink:href" {
          href = Some(attr.value());
          break;
        }
      }
    }
    let href = href.map(trim_ascii_whitespace).filter(|v| !v.is_empty());

    let Some(href) = href else {
      continue;
    };

    let Some((href_url_part, fragment)) = href.split_once('#') else {
      continue;
    };
    let href_url_part = trim_ascii_whitespace(href_url_part);
    let fragment = trim_ascii_whitespace(fragment);

    // Internal-only references (`#id`) are handled by usvg; we only patch external sprite uses.
    if href_url_part.is_empty() || href_url_part.starts_with('#') || fragment.is_empty() {
      continue;
    }

    // Resolve the URL without the fragment for fetching.
    let xml_base_chain = svg_xml_base_chain_for_node(node);
    let resolve_with_base = |base: Option<&str>| {
      apply_svg_xml_base_chain(base, &xml_base_chain)
        .and_then(|base| resolve_against_base(&base, href_url_part))
    };
    let Some(resolved_base) = resolve_with_base(Some(svg_url))
      .or_else(|| {
        ctx
          .and_then(|ctx| ctx.document_url.as_deref())
          .and_then(|base| resolve_with_base(Some(base)))
      })
      .or_else(|| Url::parse(href_url_part).ok().map(|u| u.to_string()))
    else {
      continue;
    };
    let Ok(mut resolved_url) = Url::parse(&resolved_base) else {
      continue;
    };
    resolved_url.set_fragment(None);
    let resolved_url = resolved_url.to_string();

    if let Some(ctx) = ctx {
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, &resolved_url) {
        return Err(Error::Image(ImageError::LoadFailed {
          url: resolved_url.clone(),
          reason: err.reason,
        }));
      }
    }

    if !sprite_cache.contains_key(&resolved_url) {
      check_root(RenderStage::Paint).map_err(Error::Render)?;

      let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
      if let Some(ctx) = ctx {
        if let Some(origin) = ctx.policy.document_origin.as_ref() {
          req = req.with_client_origin(origin);
        }
        if let Some(referrer_url) = ctx.document_url.as_deref() {
          req = req.with_referrer_url(referrer_url);
        }
        req = req.with_referrer_policy(ctx.referrer_policy);
      }

      let res = match fetcher.fetch_with_request(req) {
        Ok(res) => res,
        // Best-effort: keep the `<use>` element intact if the sprite fetch fails.
        Err(_) => continue,
      };
      if let Some(ctx) = ctx {
        if let Err(err) =
          ctx.check_allowed_with_final(ResourceKind::Image, &resolved_url, res.final_url.as_deref())
        {
          return Err(Error::Image(ImageError::LoadFailed {
            url: resolved_url.clone(),
            reason: err.reason,
          }));
        }
      }
      if let Err(_) = ensure_http_success(&res, &resolved_url)
        .and_then(|()| ensure_image_mime_sane(&res, &resolved_url))
      {
        continue;
      }

      let sprite_base_url = res
        .final_url
        .clone()
        .unwrap_or_else(|| resolved_url.clone());

      let mut sprite_text = {
        let bytes = res.bytes;
        if bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B {
          let mut decoder = GzDecoder::new(bytes.as_slice());
          let mut out = Vec::new();
          let mut buf = [0u8; 8192];
          let mut decompression_deadline_counter = 0usize;
          let mut ok = true;
          loop {
            check_root_periodic(
              &mut decompression_deadline_counter,
              32,
              RenderStage::Paint,
            )
            .map_err(Error::Render)?;
            let n = match decoder.read(&mut buf) {
              Ok(n) => n,
              Err(_) => {
                // Best-effort: keep the `<use>` intact if sprite decompression fails.
                ok = false;
                break;
              }
            };
            if n == 0 {
              break;
            }
            if out.len().saturating_add(n) > MAX_SVGZ_DECOMPRESSED_BYTES {
              // Best-effort: treat oversized sprites like a parse failure.
              ok = false;
              break;
            }
            out.extend_from_slice(&buf[..n]);
          }
          if !ok {
            continue;
          }
          match String::from_utf8(out) {
            Ok(text) => text,
            Err(_) => continue,
          }
        } else {
          match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => continue,
          }
        }
      };

      // Inline any external raster images referenced by the sprite itself so that when we later
      // embed a single `<symbol>`/`<g>` fragment into the parent document, nested `<image>`
      // references continue to resolve relative to the sprite URL (not the parent document URL).
      sprite_text = inline_svg_image_references(
        &sprite_text,
        &sprite_base_url,
        fetcher,
        ctx,
      )?
      .into_owned();

      let sprite_doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        roxmltree::Document::parse(&sprite_text)
      })) {
        Ok(Ok(doc)) => doc,
        Ok(Err(_)) | Err(_) => continue,
      };

      let mut by_id = HashMap::new();
      for sprite_node in sprite_doc.descendants().filter(|n| n.is_element()) {
        check_root_periodic(
          &mut deadline_counter,
          IMAGE_DECODE_DEADLINE_STRIDE,
          RenderStage::Paint,
        )
        .map_err(Error::Render)?;

        let Some(id) = sprite_node
          .attribute("id")
          .map(trim_ascii_whitespace)
          .filter(|id| !id.is_empty())
        else {
          continue;
        };

        // Record the first element for a given id (SVG ids should be unique).
        by_id.entry(id.to_string()).or_insert_with(|| {
          let tag_name = sprite_node.tag_name().name().to_string();
          let range = sprite_node.range();
          let mut inner_start: Option<usize> = None;
          let mut inner_end: Option<usize> = None;
          for child in sprite_node.children() {
            let r = child.range();
            inner_start = Some(inner_start.map_or(r.start, |s| s.min(r.start)));
            inner_end = Some(inner_end.map_or(r.end, |e| e.max(r.end)));
          }
          let inner_range = match (inner_start, inner_end) {
            (Some(s), Some(e)) if s <= e => Some(s..e),
            _ => None,
          };

          SvgUseInlineElement {
            tag_name,
            range,
            inner_range,
            view_box: sprite_node.attribute("viewBox").map(|v| v.to_string()),
            preserve_aspect_ratio: sprite_node
              .attribute("preserveAspectRatio")
              .map(|v| v.to_string()),
          }
        });
      }

      let sprite = SvgUseInlineSprite {
        content: sprite_text,
        by_id,
      };
      sprite_cache.insert(resolved_url.clone(), sprite);
    }

    let Some(sprite) = sprite_cache.get(&resolved_url) else {
      continue;
    };

    let Some(element) = sprite.by_id.get(fragment) else {
      continue;
    };

    let referenced_markup = if element.tag_name == "symbol" {
      let inner = element
        .inner_range
        .as_ref()
        .and_then(|r| sprite.content.get(r.clone()))
        .unwrap_or_default();

      let width = node
        .attribute("width")
        .map(trim_ascii_whitespace)
        .filter(|v| !v.is_empty())
        .unwrap_or("100%");
      let height = node
        .attribute("height")
        .map(trim_ascii_whitespace)
        .filter(|v| !v.is_empty())
        .unwrap_or("100%");

      let mut out = String::new();
      out.push_str("<svg width=\"");
      out.push_str(&escape_xml_attr_value(width));
      out.push_str("\" height=\"");
      out.push_str(&escape_xml_attr_value(height));
      out.push('"');
      if let Some(view_box) = element.view_box.as_deref() {
        out.push_str(" viewBox=\"");
        out.push_str(&escape_xml_attr_value(view_box));
        out.push('"');
      }
      if let Some(par) = element.preserve_aspect_ratio.as_deref() {
        out.push_str(" preserveAspectRatio=\"");
        out.push_str(&escape_xml_attr_value(par));
        out.push('"');
      }
      out.push('>');
      out.push_str(inner);
      out.push_str("</svg>");
      out
    } else {
      match sprite.content.get(element.range.clone()) {
        Some(slice) => slice.to_string(),
        None => continue,
      }
    };

    let x = node
      .attribute("x")
      .and_then(parse_svg_length_px)
      .unwrap_or(0.0);
    let y = node
      .attribute("y")
      .and_then(parse_svg_length_px)
      .unwrap_or(0.0);
    let use_transform = node
      .attribute("transform")
      .map(trim_ascii_whitespace)
      .unwrap_or("");

    let mut transform = String::new();
    if x != 0.0 || y != 0.0 {
      transform.push_str(&format!("translate({x} {y})"));
    }
    if !use_transform.is_empty() {
      if !transform.is_empty() {
        transform.push(' ');
      }
      transform.push_str(use_transform);
    }

    let mut wrapper_attrs = String::new();
    for attr in node.attributes() {
      // Only copy non-namespaced attributes; otherwise we might lose the prefix (`xml:*`,
      // `xlink:*`, etc) and produce invalid markup.
      if attr.namespace().is_some() {
        continue;
      }

      match attr.name() {
        "href" | "xlink:href" | "x" | "y" | "width" | "height" | "transform" => continue,
        "xmlns" => continue,
        _ => {}
      }

      wrapper_attrs.push(' ');
      wrapper_attrs.push_str(attr.name());
      wrapper_attrs.push_str("=\"");
      wrapper_attrs.push_str(&escape_xml_attr_value(attr.value()));
      wrapper_attrs.push('"');
    }
    if !transform.is_empty() {
      wrapper_attrs.push_str(" transform=\"");
      wrapper_attrs.push_str(&escape_xml_attr_value(&transform));
      wrapper_attrs.push('"');
    }

    let replacement = format!("<g{wrapper_attrs}>{referenced_markup}</g>");

    expansions += 1;
    injected_bytes = injected_bytes.saturating_add(replacement.len());
    if expansions > MAX_USE_EXPANSIONS || injected_bytes > MAX_INJECTED_BYTES {
      break;
    }

    replacements.push((node.range(), replacement));
  }

  if replacements.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut out = String::with_capacity(svg_content.len().saturating_add(injected_bytes));
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      // If ranges overlap or are otherwise invalid, fall back to the original markup.
      return Ok(Cow::Borrowed(svg_content));
    }
    out.push_str(&svg_content[cursor..range.start]);
    out.push_str(&replacement);
    cursor = range.end;
  }
  out.push_str(&svg_content[cursor..]);
  Ok(Cow::Owned(out))
}

fn unescape_xml_attr_value(value: &str) -> Cow<'_, str> {
  if !value.contains('&') {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  let bytes = value.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() {
    if bytes[i] != b'&' {
      out.push(bytes[i] as char);
      i += 1;
      continue;
    }
    let Some(semi_rel) = value[i + 1..].find(';') else {
      out.push('&');
      i += 1;
      continue;
    };
    let semi = i + 1 + semi_rel;
    let entity = &value[i + 1..semi];
    let decoded = match entity {
      "amp" => Some('&'),
      "lt" => Some('<'),
      "gt" => Some('>'),
      "quot" => Some('"'),
      "apos" => Some('\''),
      _ if entity.starts_with("#x") || entity.starts_with("#X") => {
        u32::from_str_radix(&entity[2..], 16)
          .ok()
          .and_then(char::from_u32)
      }
      _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
      _ => None,
    };
    if let Some(ch) = decoded {
      out.push(ch);
      i = semi + 1;
      continue;
    }
    out.push('&');
    out.push_str(entity);
    out.push(';');
    i = semi + 1;
  }
  Cow::Owned(out)
}

fn find_xml_start_tag_end(svg_content: &str, start: usize, limit: usize) -> Option<usize> {
  let bytes = svg_content.as_bytes();
  let mut quote: Option<u8> = None;
  let mut i = start;
  let limit = limit.min(bytes.len());
  while i < limit {
    let b = bytes[i];
    if let Some(q) = quote {
      if b == q {
        quote = None;
      }
    } else if b == b'"' || b == b'\'' {
      quote = Some(b);
    } else if b == b'>' {
      return Some(i + 1);
    }
    i += 1;
  }
  None
}

fn svg_data_url_mime_from_content_type(content_type: Option<&str>) -> Option<String> {
  let content_type = content_type?;
  let mime = content_type
    .split(';')
    .next()
    .map(|ct| ct.trim_matches(|c: char| matches!(c, ' ' | '\t')))?;
  if mime.is_empty() {
    return None;
  }
  let lowered = mime.to_ascii_lowercase();
  if lowered == "image/jpg" {
    return Some("image/jpeg".to_string());
  }
  if lowered.starts_with("image/") {
    return Some(lowered);
  }
  None
}

fn sniff_svg_image_data_url_mime(bytes: &[u8]) -> Option<&'static str> {
  if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
    return Some("image/png");
  }
  if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
    return Some("image/jpeg");
  }
  if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
    return Some("image/gif");
  }
  if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
    return Some("image/webp");
  }
  let probe_len = bytes.len().min(1024);
  let Ok(prefix) = std::str::from_utf8(&bytes[..probe_len]) else {
    return None;
  };
  if svg_text_looks_like_markup(prefix) {
    return Some("image/svg+xml");
  }
  None
}

fn svg_data_url_mime_for_response(content_type: Option<&str>, bytes: &[u8]) -> String {
  svg_data_url_mime_from_content_type(content_type)
    .or_else(|| sniff_svg_image_data_url_mime(bytes).map(|m| m.to_string()))
    .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Best-effort preprocessor that inlines external raster image references (`<image>` / `<feImage>`)
/// by fetching the referenced resource and rewriting its `href` into a base64 `data:` URL.
fn inline_svg_image_references<'a>(
  svg_content: &'a str,
  svg_url: &str,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
) -> Result<Cow<'a, str>> {
  use base64::Engine;

  // Avoid parsing unless it looks like we might have `<image>` references. Inline SVG content in
  // HTML is sometimes namespaced (e.g. `<svg:image>`), so also scan for `:image`.
  if !svg_content.contains("<image")
    && !svg_content.contains(":image")
    && !svg_content.contains("<feImage")
    && !svg_content.contains(":feImage")
  {
    return Ok(Cow::Borrowed(svg_content));
  }

  const MAX_IMAGE_INLINES: usize = 64;
  const MAX_EMBEDDED_BYTES_TOTAL: usize = 4 * 1024 * 1024;
  const MAX_INJECTED_BYTES: usize = 8 * 1024 * 1024;

  check_root(RenderStage::Paint).map_err(Error::Render)?;

  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_content)
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Ok(Cow::Borrowed(svg_content)),
  };

  let base_url = Url::parse(svg_url)
    .ok()
    .map(|_| svg_url)
    .or_else(|| {
      ctx
        .and_then(|ctx| ctx.document_url.as_deref())
        .filter(|doc_url| Url::parse(doc_url).is_ok())
    });

  let mut deadline_counter = 0usize;
  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
  let mut embedded_bytes_total = 0usize;
  let mut injected_bytes = 0usize;
  let mut inlines = 0usize;
  let mut data_url_cache: HashMap<String, String> = HashMap::new();

  'node_loop: for node in doc.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(Error::Render)?;

    let name = node.tag_name().name();
    let is_image = name.eq_ignore_ascii_case("image");
    let is_fe_image = name.eq_ignore_ascii_case("feimage");
    if !is_image && !is_fe_image {
      continue;
    }

    let node_range = node.range();
    if node_range.end > svg_content.len() || node_range.start >= node_range.end {
      continue;
    }

    let Some(tag_end) = find_xml_start_tag_end(svg_content, node_range.start, node_range.end)
    else {
      continue;
    };
    if tag_end > svg_content.len() || tag_end <= node_range.start {
      continue;
    }

    let xml_base_chain = svg_xml_base_chain_for_node(node);
    let effective_base_url = apply_svg_xml_base_chain(base_url, &xml_base_chain);

    let tag = &svg_content[node_range.start..tag_end];
    let bytes = tag.as_bytes();
    let mut i = 0usize;

    // Skip `<` + element name.
    if bytes.get(i) == Some(&b'<') {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'>'
        && bytes[i] != b'/'
      {
        i += 1;
      }
    }

    while i < bytes.len() {
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      if i >= bytes.len() || bytes[i] == b'>' {
        break;
      }
      if bytes[i] == b'/' {
        i += 1;
        continue;
      }

      let name_start = i;
      while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'='
        && bytes[i] != b'>'
        && bytes[i] != b'/'
      {
        i += 1;
      }
      let name_end = i;
      if name_end == name_start {
        i = i.saturating_add(1);
        continue;
      }
      let attr_name = &tag[name_start..name_end];
      let local_name = attr_name
        .rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(attr_name);

      let is_candidate = local_name.eq_ignore_ascii_case("href");

      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i >= bytes.len() || bytes[i] != b'=' {
        continue;
      }
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      let value_start = i;
      if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        let value_end = i;
        if i < bytes.len() {
          i += 1;
        }
        let value_start = start;
        let value_range = (node_range.start + value_start)..(node_range.start + value_end);

        if !is_candidate {
          continue;
        }

        let raw_value = tag.get(value_start..value_end).unwrap_or_default();
        let decoded = unescape_xml_attr_value(raw_value);
        let trimmed = trim_ascii_whitespace(decoded.as_ref());
        if trimmed.is_empty()
          || trimmed.starts_with('#')
          || crate::resource::is_data_url(trimmed)
          || is_about_url(trimmed)
        {
          continue;
        }

        let (href_no_fragment, href_fragment) = trimmed
          .split_once('#')
          .map(|(before, frag)| (before, Some(frag)))
          .unwrap_or((trimmed, None));
        let href_no_fragment = trim_ascii_whitespace(href_no_fragment);
        let href_fragment = href_fragment.filter(|frag| !frag.is_empty());
        if href_no_fragment.is_empty() {
          continue;
        }

        let Some(resolved_base) = effective_base_url
          .as_deref()
          .and_then(|base| resolve_against_base(base, href_no_fragment))
          .or_else(|| Url::parse(href_no_fragment).ok().map(|u| u.to_string()))
        else {
          continue;
        };
        let Ok(mut resolved_url) = Url::parse(&resolved_base) else {
          continue;
        };
        resolved_url.set_fragment(None);
        if resolved_url.scheme() != "http" && resolved_url.scheme() != "https" {
          continue;
        }
        let resolved_url = resolved_url.to_string();

        if let Some(ctx) = ctx {
          if let Err(err) = ctx.check_allowed(ResourceKind::Image, &resolved_url) {
            return Err(Error::Image(ImageError::LoadFailed {
              url: resolved_url.clone(),
              reason: err.reason,
            }));
          }
        }

        let original_value_len = value_range.end.saturating_sub(value_range.start);

        let data_url = if let Some(cached) = data_url_cache.get(&resolved_url) {
          cached.clone()
        } else {
          if inlines >= MAX_IMAGE_INLINES {
            break 'node_loop;
          }

          check_root(RenderStage::Paint).map_err(Error::Render)?;

          let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
          if let Some(ctx) = ctx {
            if let Some(origin) = ctx.policy.document_origin.as_ref() {
              req = req.with_client_origin(origin);
            }
            if let Some(referrer_url) = ctx.document_url.as_deref() {
              req = req.with_referrer_url(referrer_url);
            }
            req = req.with_referrer_policy(ctx.referrer_policy);
          }

          let res = match fetcher.fetch_with_request(req) {
            Ok(res) => res,
            Err(_) => continue,
          };

          if let Some(ctx) = ctx {
            if let Err(err) = ctx.check_allowed_with_final(
              ResourceKind::Image,
              &resolved_url,
              res.final_url.as_deref(),
            ) {
              return Err(Error::Image(ImageError::LoadFailed {
                url: resolved_url.clone(),
                reason: err.reason,
              }));
            }
          }

          if let Err(_) = ensure_http_success(&res, &resolved_url)
            .and_then(|()| ensure_image_mime_sane(&res, &resolved_url))
          {
            continue;
          }

          let bytes_len = res.bytes.len();
          if embedded_bytes_total.saturating_add(bytes_len) > MAX_EMBEDDED_BYTES_TOTAL {
            break 'node_loop;
          }

          let mime = svg_data_url_mime_for_response(res.content_type.as_deref(), &res.bytes);

          let base64_len = u64::try_from(bytes_len)
            .ok()
            .and_then(|n| n.checked_add(2))
            .and_then(|n| n.checked_div(3))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| usize::try_from(n).ok());
          let Some(base64_len) = base64_len else {
            break 'node_loop;
          };

          let prefix_len = "data:".len() + mime.len() + ";base64,".len();
          let total_len = prefix_len.saturating_add(base64_len);
          let growth = total_len.saturating_sub(original_value_len);
          if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
            break 'node_loop;
          }

          let encoded = base64::engine::general_purpose::STANDARD.encode(&res.bytes);
          let data_url = format!("data:{mime};base64,{encoded}");

          embedded_bytes_total = embedded_bytes_total.saturating_add(bytes_len);
          inlines += 1;
          data_url_cache.insert(resolved_url.clone(), data_url.clone());
          data_url
        };

        let mut replacement = data_url;
        if let Some(fragment) = href_fragment {
          replacement.push('#');
          replacement.push_str(fragment);
        }
        let replacement = match escape_xml_attr_value(&replacement) {
          Cow::Borrowed(_) => replacement,
          Cow::Owned(escaped) => escaped,
        };

        let growth = replacement.len().saturating_sub(original_value_len);
        if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
          break 'node_loop;
        }
        injected_bytes = injected_bytes.saturating_add(growth);
        replacements.push((value_range, replacement));
        continue;
      }

      let start = value_start;
      while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'>'
        && bytes[i] != b'/'
      {
        i += 1;
      }
      let value_end = i;

      let value_range = (node_range.start + start)..(node_range.start + value_end);
      if !is_candidate {
        continue;
      }
      let raw_value = tag.get(start..value_end).unwrap_or_default();
      let decoded = unescape_xml_attr_value(raw_value);
      let trimmed = trim_ascii_whitespace(decoded.as_ref());
      if trimmed.is_empty()
        || trimmed.starts_with('#')
        || crate::resource::is_data_url(trimmed)
        || is_about_url(trimmed)
      {
        continue;
      }

      let (href_no_fragment, href_fragment) = trimmed
        .split_once('#')
        .map(|(before, frag)| (before, Some(frag)))
        .unwrap_or((trimmed, None));
      let href_no_fragment = trim_ascii_whitespace(href_no_fragment);
      let href_fragment = href_fragment.filter(|frag| !frag.is_empty());
      if href_no_fragment.is_empty() {
        continue;
      }

      let Some(resolved_base) = effective_base_url
        .as_deref()
        .and_then(|base| resolve_against_base(base, href_no_fragment))
        .or_else(|| Url::parse(href_no_fragment).ok().map(|u| u.to_string()))
      else {
        continue;
      };
      let Ok(mut resolved_url) = Url::parse(&resolved_base) else {
        continue;
      };
      resolved_url.set_fragment(None);
      if resolved_url.scheme() != "http" && resolved_url.scheme() != "https" {
        continue;
      }
      let resolved_url = resolved_url.to_string();

      if let Some(ctx) = ctx {
        if let Err(err) = ctx.check_allowed(ResourceKind::Image, &resolved_url) {
          return Err(Error::Image(ImageError::LoadFailed {
            url: resolved_url.clone(),
            reason: err.reason,
          }));
        }
      }

      let original_value_len = value_range.end.saturating_sub(value_range.start);
      let data_url = if let Some(cached) = data_url_cache.get(&resolved_url) {
        cached.clone()
      } else {
        if inlines >= MAX_IMAGE_INLINES {
          break 'node_loop;
        }

        check_root(RenderStage::Paint).map_err(Error::Render)?;

        let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
        if let Some(ctx) = ctx {
          if let Some(origin) = ctx.policy.document_origin.as_ref() {
            req = req.with_client_origin(origin);
          }
          if let Some(referrer_url) = ctx.document_url.as_deref() {
            req = req.with_referrer_url(referrer_url);
          }
          req = req.with_referrer_policy(ctx.referrer_policy);
        }

        let res = match fetcher.fetch_with_request(req) {
          Ok(res) => res,
          Err(_) => continue,
        };

        if let Some(ctx) = ctx {
          if let Err(err) = ctx.check_allowed_with_final(
            ResourceKind::Image,
            &resolved_url,
            res.final_url.as_deref(),
          ) {
            return Err(Error::Image(ImageError::LoadFailed {
              url: resolved_url.clone(),
              reason: err.reason,
            }));
          }
        }

        if let Err(_) = ensure_http_success(&res, &resolved_url)
          .and_then(|()| ensure_image_mime_sane(&res, &resolved_url))
        {
          continue;
        }

        let bytes_len = res.bytes.len();
        if embedded_bytes_total.saturating_add(bytes_len) > MAX_EMBEDDED_BYTES_TOTAL {
          break 'node_loop;
        }

        let mime = svg_data_url_mime_for_response(res.content_type.as_deref(), &res.bytes);

        let base64_len = u64::try_from(bytes_len)
          .ok()
          .and_then(|n| n.checked_add(2))
          .and_then(|n| n.checked_div(3))
          .and_then(|n| n.checked_mul(4))
          .and_then(|n| usize::try_from(n).ok());
        let Some(base64_len) = base64_len else {
          break 'node_loop;
        };

        let prefix_len = "data:".len() + mime.len() + ";base64,".len();
        let total_len = prefix_len.saturating_add(base64_len);
        let growth = total_len.saturating_sub(original_value_len);
        if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
          break 'node_loop;
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(&res.bytes);
        let data_url = format!("data:{mime};base64,{encoded}");

        embedded_bytes_total = embedded_bytes_total.saturating_add(bytes_len);
        inlines += 1;
        data_url_cache.insert(resolved_url.clone(), data_url.clone());
        data_url
      };

      let mut replacement = data_url;
      if let Some(fragment) = href_fragment {
        replacement.push('#');
        replacement.push_str(fragment);
      }
      let replacement = match escape_xml_attr_value(&replacement) {
        Cow::Borrowed(_) => replacement,
        Cow::Owned(escaped) => escaped,
      };

      let growth = replacement.len().saturating_sub(original_value_len);
      if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
        break 'node_loop;
      }
      injected_bytes = injected_bytes.saturating_add(growth);
      replacements.push((value_range, replacement));
    }
  }

  if replacements.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut out = String::with_capacity(svg_content.len().saturating_add(injected_bytes));
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      return Ok(Cow::Borrowed(svg_content));
    }
    out.push_str(&svg_content[cursor..range.start]);
    out.push_str(&replacement);
    cursor = range.end;
  }
  out.push_str(&svg_content[cursor..]);
  Ok(Cow::Owned(out))
}

fn apply_svg_url_fragment<'a>(svg_content: &'a str, requested_url: &str) -> Cow<'a, str> {
  let Some((_, fragment)) = requested_url.split_once('#') else {
    return Cow::Borrowed(svg_content);
  };
  let fragment = trim_ascii_whitespace(fragment);
  if fragment.is_empty() {
    return Cow::Borrowed(svg_content);
  }

  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_content)
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Cow::Borrowed(svg_content),
  };

  let root = doc.root_element();
  if !root.tag_name().name().eq_ignore_ascii_case("svg") {
    return Cow::Borrowed(svg_content);
  }

  // Only inject when the referenced element exists; otherwise we'd unnecessarily introduce a
  // `<use>` element into non-sprite SVGs (which can disable simple fast-path rendering).
  let Some(target) = doc.descendants().find(|node| {
    node.is_element()
      && *node != root
      && node
        .attribute("id")
        .is_some_and(|id| trim_ascii_whitespace(id) == fragment)
  }) else {
    return Cow::Borrowed(svg_content);
  };

  if target.tag_name().name().eq_ignore_ascii_case("symbol") {
    // Symbols are not directly rendered. Wrap their contents in a new <svg>, copying the root
    // viewport size and the symbol's viewBox/preserveAspectRatio when present.
    let symbol = target;
    let mut inner_start: Option<usize> = None;
    let mut inner_end: Option<usize> = None;
    for child in symbol.children() {
      let r = child.range();
      inner_start = Some(inner_start.map_or(r.start, |s| s.min(r.start)));
      inner_end = Some(inner_end.map_or(r.end, |e| e.max(r.end)));
    }
    let inner_range = match (inner_start, inner_end) {
      (Some(s), Some(e)) if s <= e => Some(s..e),
      _ => None,
    };
    let inner = inner_range
      .as_ref()
      .and_then(|r| svg_content.get(r.clone()))
      .unwrap_or_default();

    let root_width = root
      .attribute("width")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty());
    let root_height = root
      .attribute("height")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty());

    let view_box = symbol
      .attribute("viewBox")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
      .or_else(|| root.attribute("viewBox").map(trim_ascii_whitespace).filter(|v| !v.is_empty()));
    let preserve_aspect_ratio = symbol
      .attribute("preserveAspectRatio")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
      .or_else(|| {
        root
          .attribute("preserveAspectRatio")
          .map(trim_ascii_whitespace)
          .filter(|v| !v.is_empty())
      });

    let mut out = String::new();
    out.push_str("<svg");
    // Ensure the output still parses as SVG even when the original document had unusual namespace
    // placement.
    let mut had_xmlns = false;
    for attr in root.attributes() {
      let name = attr.name();
      if !name.starts_with("xmlns") {
        continue;
      }
      if name == "xmlns" {
        had_xmlns = true;
      }
      out.push(' ');
      out.push_str(name);
      out.push_str("=\"");
      out.push_str(&escape_xml_attr_value(attr.value()));
      out.push('"');
    }
    if !had_xmlns {
      out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
    }

    if let Some(width) = root_width {
      out.push_str(" width=\"");
      out.push_str(&escape_xml_attr_value(width));
      out.push('"');
    }
    if let Some(height) = root_height {
      out.push_str(" height=\"");
      out.push_str(&escape_xml_attr_value(height));
      out.push('"');
    }
    if let Some(view_box) = view_box {
      out.push_str(" viewBox=\"");
      out.push_str(&escape_xml_attr_value(view_box));
      out.push('"');
    }
    if let Some(par) = preserve_aspect_ratio {
      out.push_str(" preserveAspectRatio=\"");
      out.push_str(&escape_xml_attr_value(par));
      out.push('"');
    }
    out.push('>');

    // Keep root <defs>/<style> blocks so symbol content can reference gradients/filters and reuse
    // shared CSS.
    for child in root.children().filter(|n| n.is_element()) {
      let name = child.tag_name().name();
      let keep = name.eq_ignore_ascii_case("defs") || name.eq_ignore_ascii_case("style");
      if !keep {
        continue;
      }
      if let Some(slice) = svg_content.get(child.range()) {
        out.push_str(slice);
      }
    }

    out.push_str(inner);
    out.push_str("</svg>");
    return Cow::Owned(out);
  }

  let root_range = root.range();
  let Some(root_slice) = svg_content.get(root_range.clone()) else {
    return Cow::Borrowed(svg_content);
  };
  let Some(close_pos) = root_slice.rfind("</") else {
    return Cow::Borrowed(svg_content);
  };
  let insert_pos = root_range.start.saturating_add(close_pos);
  if insert_pos > svg_content.len() {
    return Cow::Borrowed(svg_content);
  }

  let escaped_id = escape_xml_attr_value(fragment);
  let mut out = String::with_capacity(svg_content.len().saturating_add(escaped_id.len() + 16));
  out.push_str(&svg_content[..insert_pos]);
  out.push_str("<use href=\"#");
  out.push_str(escaped_id.as_ref());
  out.push_str("\"/>");
  out.push_str(&svg_content[insert_pos..]);
  Cow::Owned(out)
}

fn svg_parse_fill_color(value: &str) -> Option<Rgba> {
  let trimmed = trim_ascii_whitespace(value);
  if trimmed.is_empty() {
    return None;
  }
  if trimmed.eq_ignore_ascii_case("none") {
    return Some(Rgba::new(0, 0, 0, 0.0));
  }
  if trimmed.eq_ignore_ascii_case("currentColor") {
    return None;
  }
  if let Some(hex) = crate::style::defaults::parse_color_attribute(trimmed) {
    return Some(hex);
  }
  trimmed.parse::<csscolorparser::Color>().ok().map(|c| {
    Rgba::new(
      (c.r * 255.0).round() as u8,
      (c.g * 255.0).round() as u8,
      (c.b * 255.0).round() as u8,
      c.a as f32,
    )
  })
}

fn multiply_alpha(mut color: Rgba, alpha: f32) -> Rgba {
  if !alpha.is_finite() {
    return color;
  }
  color.a = (color.a * alpha).clamp(0.0, 1.0);
  color
}

fn try_render_simple_svg_pixmap(
  svg_content: &str,
  render_width: u32,
  render_height: u32,
) -> std::result::Result<Option<Pixmap>, RenderError> {
  use tiny_skia::{FillRule, LineCap, LineJoin, Paint, Stroke, StrokeDash};

  if render_width == 0 || render_height == 0 {
    return Ok(None);
  }

  fn inherited_attr<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    for ancestor in node.ancestors().filter(|n| n.is_element()) {
      if let Some(value) = ancestor.attribute(name) {
        let trimmed = trim_ascii_whitespace(value);
        if trimmed.eq_ignore_ascii_case("inherit") {
          continue;
        }
        return Some(trimmed);
      }
    }
    None
  }

  fn parse_svg_dash_array(value: &str) -> Option<Vec<f32>> {
    let trimmed = trim_ascii_whitespace(value);
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
      return Some(Vec::new());
    }

    let mut values = Vec::new();
    for part in trimmed
      .split(|c: char| {
        c == ',' || matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
      })
      .filter(|s| !s.is_empty())
    {
      let v = parse_svg_length_px(part)?;
      if !v.is_finite() || v < 0.0 {
        return None;
      }
      values.push(v);
    }
    Some(values)
  }

  let mut deadline_counter = 0usize;
  check_root_periodic(
    &mut deadline_counter,
    IMAGE_DECODE_DEADLINE_STRIDE,
    RenderStage::Paint,
  )?;

  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    Document::parse(svg_content)
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Ok(None),
  };
  let root = doc.root_element();
  let has_view_box_attr = root.attribute("viewBox").is_some();
  if !root.tag_name().name().eq_ignore_ascii_case("svg") {
    return Ok(None);
  }

  for node in root.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let name = node.tag_name().name();
    let allowed = matches!(name, "svg" | "g" | "path" | "title" | "desc" | "metadata");
    if !allowed {
      return Ok(None);
    }
    if node.attribute("transform").is_some()
      || node.attribute("filter").is_some()
      || node.attribute("mask").is_some()
      || node.attribute("clip-path").is_some()
      || node.attribute("style").is_some()
    {
      return Ok(None);
    }
    if name.eq_ignore_ascii_case("path") {
      if node.attribute("d").is_none() {
        return Ok(None);
      }
    } else if node.attribute("opacity").is_some() {
      // `opacity` on groups/root requires separate compositing that this fast-path does not
      // implement (and is uncommon for icon SVGs). Only allow `opacity` on leaf paths.
      return Ok(None);
    }
  }

  let view_box = root
    .attribute("viewBox")
    .and_then(parse_svg_view_box)
    .or_else(|| {
      let w = root.attribute("width").and_then(parse_svg_length_px)?;
      let h = root.attribute("height").and_then(parse_svg_length_px)?;
      Some(SvgViewBox {
        min_x: 0.0,
        min_y: 0.0,
        width: w,
        height: h,
      })
    })
    .unwrap_or(SvgViewBox {
      min_x: 0.0,
      min_y: 0.0,
      width: render_width as f32,
      height: render_height as f32,
    });
  if !(view_box.width.is_finite()
    && view_box.height.is_finite()
    && view_box.width > 0.0
    && view_box.height > 0.0)
  {
    return Ok(None);
  }

  let mut preserve = SvgPreserveAspectRatio::parse(root.attribute("preserveAspectRatio"));
  // Without a viewBox, the viewport and user coordinate systems are the same, so the
  // viewBox-to-viewport preserveAspectRatio mapping must be ignored (equivalent to `none`).
  if !has_view_box_attr {
    preserve.none = true;
  }
  let transform = map_svg_aspect_ratio(
    view_box,
    preserve,
    render_width as f32,
    render_height as f32,
  );

  let Some(mut pixmap) = new_pixmap(render_width, render_height) else {
    return Ok(None);
  };
  for node in root.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    if !node.tag_name().name().eq_ignore_ascii_case("path") {
      continue;
    }
    let Some(d) = node.attribute("d") else {
      return Ok(None);
    };
    let path = match crate::svg_path::build_tiny_skia_path_from_svg_path_data(
      d,
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )? {
      Some(path) => path,
      None => return Ok(None),
    };

    let mut opacity = 1.0f32;
    if let Some(opacity_raw) = node.attribute("opacity") {
      if let Ok(alpha) = trim_ascii_whitespace(opacity_raw).parse::<f32>() {
        opacity = alpha;
      }
    }

    let fill_rule = match node.attribute("fill-rule").map(trim_ascii_whitespace) {
      None => FillRule::Winding,
      Some(v) if v.eq_ignore_ascii_case("nonzero") => FillRule::Winding,
      Some(v) if v.eq_ignore_ascii_case("evenodd") => FillRule::EvenOdd,
      Some(_) => return Ok(None),
    };

    let fill = inherited_attr(node, "fill");
    let mut fill_color = match fill {
      Some(v) => match svg_parse_fill_color(v) {
        Some(color) => color,
        None => return Ok(None),
      },
      None => Rgba::new(0, 0, 0, 1.0),
    };
    fill_color = multiply_alpha(fill_color, opacity);
    if let Some(opacity_raw) = inherited_attr(node, "fill-opacity") {
      if let Ok(alpha) = opacity_raw.parse::<f32>() {
        fill_color = multiply_alpha(fill_color, alpha);
      }
    }
    if fill_color.a > 0.0 {
      let mut paint = Paint::default();
      paint.set_color_rgba8(
        fill_color.r,
        fill_color.g,
        fill_color.b,
        fill_color.alpha_u8(),
      );
      paint.anti_alias = true;
      pixmap.fill_path(&path, &paint, fill_rule, transform, None);
    }

    let stroke = inherited_attr(node, "stroke");
    let mut stroke_color = match stroke {
      Some(v) => match svg_parse_fill_color(v) {
        Some(color) => color,
        None => return Ok(None),
      },
      None => Rgba::new(0, 0, 0, 0.0),
    };
    stroke_color = multiply_alpha(stroke_color, opacity);
    if let Some(opacity_raw) = inherited_attr(node, "stroke-opacity") {
      if let Ok(alpha) = opacity_raw.parse::<f32>() {
        stroke_color = multiply_alpha(stroke_color, alpha);
      }
    }
    if stroke_color.a > 0.0 {
      let stroke_width = inherited_attr(node, "stroke-width")
        .and_then(parse_svg_length_px)
        .unwrap_or(1.0);
      if stroke_width > 0.0 && stroke_width.is_finite() {
        let line_cap = match inherited_attr(node, "stroke-linecap") {
          None => LineCap::Butt,
          Some(v) if v.eq_ignore_ascii_case("butt") => LineCap::Butt,
          Some(v) if v.eq_ignore_ascii_case("round") => LineCap::Round,
          Some(v) if v.eq_ignore_ascii_case("square") => LineCap::Square,
          Some(_) => return Ok(None),
        };
        let line_join = match inherited_attr(node, "stroke-linejoin") {
          None => LineJoin::Miter,
          Some(v) if v.eq_ignore_ascii_case("miter") => LineJoin::Miter,
          Some(v) if v.eq_ignore_ascii_case("round") => LineJoin::Round,
          Some(v) if v.eq_ignore_ascii_case("bevel") => LineJoin::Bevel,
          Some(_) => return Ok(None),
        };
        let miter_limit = match inherited_attr(node, "stroke-miterlimit") {
          None => 4.0,
          Some(v) => match v.parse::<f32>() {
            Ok(val) if val.is_finite() && val > 0.0 => val,
            _ => return Ok(None),
          },
        };

        let mut dash = None;
        if let Some(raw) = inherited_attr(node, "stroke-dasharray") {
          let mut values = match parse_svg_dash_array(raw) {
            Some(values) => values,
            None => return Ok(None),
          };
          if !values.is_empty() {
            if values.iter().all(|v| *v == 0.0) {
              values.clear();
            }
          }
          if !values.is_empty() {
            if values.len() % 2 == 1 {
              let extra = values.clone();
              values.extend(extra);
            }
            let mut offset = 0.0;
            if let Some(raw) = inherited_attr(node, "stroke-dashoffset") {
              if let Some(v) = parse_svg_length_px(raw) {
                offset = v;
              }
            }
            dash = StrokeDash::new(values, offset);
          }
        }

        let mut stroke = Stroke::default();
        stroke.width = stroke_width;
        stroke.line_cap = line_cap;
        stroke.line_join = line_join;
        stroke.miter_limit = miter_limit;
        stroke.dash = dash;

        let mut paint = Paint::default();
        paint.set_color_rgba8(
          stroke_color.r,
          stroke_color.g,
          stroke_color.b,
          stroke_color.alpha_u8(),
        );
        paint.anti_alias = true;
        pixmap.stroke_path(&path, &paint, &stroke, transform, None);
      }
    }
  }

  Ok(Some(pixmap))
}

// ============================================================================
// CachedImage
// ============================================================================

/// Decoded image plus orientation metadata.
pub struct CachedImage {
  pub image: Arc<DynamicImage>,
  pub orientation: Option<OrientationTransform>,
  /// Resolution in image pixels per CSS px (dppx) when provided by metadata.
  pub resolution: Option<f32>,
  /// Whether this image originated from a vector source (SVG).
  pub is_vector: bool,
  /// Intrinsic aspect ratio when known. SVGs that opt out of aspect-ratio preservation keep this
  /// as `None` and set `aspect_ratio_none` to true.
  pub intrinsic_ratio: Option<f32>,
  /// True when the resource explicitly disables aspect-ratio preservation (e.g., SVG
  /// `preserveAspectRatio="none"`).
  pub aspect_ratio_none: bool,
  /// Raw SVG markup when the image originated from a vector source.
  pub svg_content: Option<Arc<str>>,
}

impl CachedImage {
  pub fn dimensions(&self) -> (u32, u32) {
    self.image.dimensions()
  }

  pub fn width(&self) -> u32 {
    self.image.width()
  }

  pub fn height(&self) -> u32 {
    self.image.height()
  }

  pub fn oriented_dimensions(&self, transform: OrientationTransform) -> (u32, u32) {
    let (w, h) = self.dimensions();
    transform.oriented_dimensions(w, h)
  }

  /// Computes CSS pixel dimensions after applying orientation and the provided image-resolution.
  pub fn css_dimensions(
    &self,
    transform: OrientationTransform,
    resolution: &ImageResolution,
    device_pixel_ratio: f32,
    override_resolution: Option<f32>,
  ) -> Option<(f32, f32)> {
    let (w, h) = self.oriented_dimensions(transform);
    if w == 0 || h == 0 {
      return None;
    }
    if self.is_vector {
      return Some((w as f32, h as f32));
    }
    let used = resolution.used_resolution(override_resolution, self.resolution, device_pixel_ratio);
    if used <= 0.0 || !used.is_finite() {
      return None;
    }
    Some((w as f32 / used, h as f32 / used))
  }

  /// Intrinsic aspect ratio, adjusted for EXIF orientation when present.
  pub fn intrinsic_ratio(&self, transform: OrientationTransform) -> Option<f32> {
    if self.aspect_ratio_none {
      return None;
    }

    let mut ratio = self.intrinsic_ratio;
    if ratio.is_none() {
      let (w, h) = self.oriented_dimensions(transform);
      if h > 0 {
        ratio = Some(w as f32 / h as f32);
      }
    }

    if let Some(r) = ratio {
      if transform.quarter_turns % 2 == 1 {
        return Some(1.0 / r);
      }
    }

    ratio
  }

  pub fn to_oriented_rgba(&self, transform: OrientationTransform) -> RgbaImage {
    let mut rgba = self.image.to_rgba8();

    match transform.quarter_turns % 4 {
      0 => {}
      1 => rgba = imageops::rotate90(&rgba),
      2 => rgba = imageops::rotate180(&rgba),
      3 => rgba = imageops::rotate270(&rgba),
      _ => {}
    }

    if transform.flip_x {
      rgba = imageops::flip_horizontal(&rgba);
    }

    rgba
  }
}

#[derive(Debug, Clone)]
pub struct CachedImageMetadata {
  pub width: u32,
  pub height: u32,
  pub orientation: Option<OrientationTransform>,
  pub resolution: Option<f32>,
  pub is_vector: bool,
  pub intrinsic_ratio: Option<f32>,
  pub aspect_ratio_none: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct DiskCachedOrientationTransformV1 {
  quarter_turns: u8,
  flip_x: bool,
}

impl From<OrientationTransform> for DiskCachedOrientationTransformV1 {
  fn from(value: OrientationTransform) -> Self {
    Self {
      quarter_turns: value.quarter_turns,
      flip_x: value.flip_x,
    }
  }
}

impl From<DiskCachedOrientationTransformV1> for OrientationTransform {
  fn from(value: DiskCachedOrientationTransformV1) -> Self {
    Self {
      quarter_turns: value.quarter_turns,
      flip_x: value.flip_x,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskCachedImageProbeMetadataV1 {
  width: u32,
  height: u32,
  orientation: Option<DiskCachedOrientationTransformV1>,
  resolution: Option<f32>,
  is_vector: bool,
  intrinsic_ratio: Option<f32>,
  aspect_ratio_none: bool,
}

impl From<&CachedImageMetadata> for DiskCachedImageProbeMetadataV1 {
  fn from(meta: &CachedImageMetadata) -> Self {
    Self {
      width: meta.width,
      height: meta.height,
      orientation: meta.orientation.map(Into::into),
      resolution: meta.resolution,
      is_vector: meta.is_vector,
      intrinsic_ratio: meta.intrinsic_ratio,
      aspect_ratio_none: meta.aspect_ratio_none,
    }
  }
}

impl From<DiskCachedImageProbeMetadataV1> for CachedImageMetadata {
  fn from(meta: DiskCachedImageProbeMetadataV1) -> Self {
    Self {
      width: meta.width,
      height: meta.height,
      orientation: meta.orientation.map(Into::into),
      resolution: meta.resolution,
      is_vector: meta.is_vector,
      intrinsic_ratio: meta.intrinsic_ratio,
      aspect_ratio_none: meta.aspect_ratio_none,
    }
  }
}

fn encode_probe_metadata_for_disk(meta: &CachedImageMetadata) -> Option<Vec<u8>> {
  serde_json::to_vec(&DiskCachedImageProbeMetadataV1::from(meta)).ok()
}

fn decode_probe_metadata_from_disk(bytes: &[u8]) -> Option<CachedImageMetadata> {
  serde_json::from_slice::<DiskCachedImageProbeMetadataV1>(bytes)
    .ok()
    .map(Into::into)
}

impl CachedImageMetadata {
  pub fn dimensions(&self) -> (u32, u32) {
    (self.width, self.height)
  }

  pub fn oriented_dimensions(&self, transform: OrientationTransform) -> (u32, u32) {
    transform.oriented_dimensions(self.width, self.height)
  }

  pub fn css_dimensions(
    &self,
    transform: OrientationTransform,
    resolution: &ImageResolution,
    device_pixel_ratio: f32,
    override_resolution: Option<f32>,
  ) -> Option<(f32, f32)> {
    let (w, h) = self.oriented_dimensions(transform);
    if w == 0 || h == 0 {
      return None;
    }
    if self.is_vector {
      return Some((w as f32, h as f32));
    }
    let used = resolution.used_resolution(override_resolution, self.resolution, device_pixel_ratio);
    if used <= 0.0 || !used.is_finite() {
      return None;
    }
    Some((w as f32 / used, h as f32 / used))
  }

  pub fn intrinsic_ratio(&self, transform: OrientationTransform) -> Option<f32> {
    if self.aspect_ratio_none {
      return None;
    }

    let mut ratio = self.intrinsic_ratio;
    if ratio.is_none() {
      let (w, h) = self.oriented_dimensions(transform);
      if h > 0 {
        ratio = Some(w as f32 / h as f32);
      }
    }

    if let Some(r) = ratio {
      if transform.quarter_turns % 2 == 1 {
        return Some(1.0 / r);
      }
    }

    ratio
  }
}

impl From<&CachedImage> for CachedImageMetadata {
  fn from(image: &CachedImage) -> Self {
    let (width, height) = image.dimensions();
    Self {
      width,
      height,
      orientation: image.orientation,
      resolution: image.resolution,
      is_vector: image.is_vector,
      intrinsic_ratio: image.intrinsic_ratio,
      aspect_ratio_none: image.aspect_ratio_none,
    }
  }
}

fn is_about_url(url: &str) -> bool {
  let trimmed = trim_ascii_whitespace_start(url);
  trimmed
    .get(..6)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("about:"))
}

fn url_ends_with_svgz(url: &str) -> bool {
  let trimmed = trim_ascii_whitespace(url);
  if trimmed.is_empty() {
    return false;
  }
  let base = trimmed
    .split(|c: char| c == '?' || c == '#')
    .next()
    .unwrap_or(trimmed);
  base
    .get(base.len().saturating_sub(5)..)
    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".svgz"))
}

fn svg_text_looks_like_markup(text: &str) -> bool {
  let without_bom = text.strip_prefix('\u{feff}').unwrap_or(text);
  let trimmed = trim_ascii_whitespace_start(without_bom);
  trimmed
    .get(..4)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("<svg"))
    || trimmed
      .get(..5)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case("<?xml"))
}

fn about_url_placeholder_image() -> Arc<CachedImage> {
  static PLACEHOLDER: OnceLock<Arc<CachedImage>> = OnceLock::new();
  Arc::clone(PLACEHOLDER.get_or_init(|| {
    // A 1×1 fully transparent RGBA buffer so layout/paint can proceed deterministically for
    // non-fetchable `about:` URLs (e.g. `about:blank`).
    let img = RgbaImage::new(1, 1);
    Arc::new(CachedImage {
      image: Arc::new(DynamicImage::ImageRgba8(img)),
      orientation: None,
      resolution: None,
      is_vector: false,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
      svg_content: None,
    })
  }))
}

fn about_url_placeholder_metadata() -> Arc<CachedImageMetadata> {
  static PLACEHOLDER_META: OnceLock<Arc<CachedImageMetadata>> = OnceLock::new();
  Arc::clone(
    PLACEHOLDER_META
      .get_or_init(|| Arc::new(CachedImageMetadata::from(&*about_url_placeholder_image()))),
  )
}

fn about_url_placeholder_pixmap() -> Result<Arc<tiny_skia::Pixmap>> {
  static PLACEHOLDER_PIXMAP: OnceLock<std::result::Result<Arc<tiny_skia::Pixmap>, RenderError>> =
    OnceLock::new();
  match PLACEHOLDER_PIXMAP.get_or_init(|| {
    // `new_pixmap_with_context` returns a zeroed RGBA buffer, which is already a premultiplied
    // fully-transparent pixel for tiny-skia.
    new_pixmap_with_context(1, 1, "about URL placeholder pixmap").map(Arc::new)
  }) {
    Ok(pixmap) => Ok(Arc::clone(pixmap)),
    Err(err) => Err(Error::Render(err.clone())),
  }
}

fn status_is_http_success(status: Option<u16>) -> bool {
  matches!(status, Some(200..=299))
}

fn is_empty_body_error_for_image(error: &Error) -> bool {
  matches!(
    error,
    Error::Resource(res)
      if status_is_http_success(res.status) && res.message.contains("empty HTTP response body")
  )
}

fn payload_looks_like_markup_but_not_svg(bytes: &[u8]) -> bool {
  let sample = &bytes[..bytes.len().min(256)];
  let mut i = 0;
  if sample.starts_with(b"\xef\xbb\xbf") {
    i = 3;
  }
  while i < sample.len() && sample[i].is_ascii_whitespace() {
    i += 1;
  }
  let rest = &sample[i..];
  if rest.is_empty() || rest[0] != b'<' {
    return false;
  }

  // Accept common SVG/XML prologs so that SVG documents aren't mistaken for HTML.
  if rest.len() >= 4
    && rest[0] == b'<'
    && rest[1].to_ascii_lowercase() == b's'
    && rest[2].to_ascii_lowercase() == b'v'
    && rest[3].to_ascii_lowercase() == b'g'
  {
    return false;
  }
  if rest.len() >= 5
    && rest[0] == b'<'
    && rest[1] == b'?'
    && rest[2].to_ascii_lowercase() == b'x'
    && rest[3].to_ascii_lowercase() == b'm'
    && rest[4].to_ascii_lowercase() == b'l'
  {
    return false;
  }
  if rest.len() >= 10
    && rest[0] == b'<'
    && rest[1] == b'!'
    && rest[2].to_ascii_lowercase() == b'd'
    && rest[3].to_ascii_lowercase() == b'o'
    && rest[4].to_ascii_lowercase() == b'c'
    && rest[5].to_ascii_lowercase() == b't'
    && rest[6].to_ascii_lowercase() == b'y'
    && rest[7].to_ascii_lowercase() == b'p'
    && rest[8].to_ascii_lowercase() == b'e'
  {
    let mut j = 9;
    while j < rest.len() && rest[j].is_ascii_whitespace() {
      j += 1;
    }
    if rest.len().saturating_sub(j) >= 3
      && rest[j].to_ascii_lowercase() == b's'
      && rest[j + 1].to_ascii_lowercase() == b'v'
      && rest[j + 2].to_ascii_lowercase() == b'g'
    {
      return false;
    }
  }

  true
}

fn should_substitute_markup_payload_for_image(
  requested_url: &str,
  final_url: Option<&str>,
  status: Option<u16>,
  bytes: &[u8],
) -> bool {
  if !status_is_http_success(status) || !payload_looks_like_markup_but_not_svg(bytes) {
    return false;
  }
  let final_url = final_url.unwrap_or(requested_url);
  // Avoid masking "HTML returned for a real image URL" cases (common bot-mitigation behavior).
  // For URLs that look like real image assets, prefer surfacing a `ResourceError` via
  // `ensure_image_mime_sane` over silently substituting a placeholder.
  if crate::resource::url_looks_like_image_asset(requested_url)
    || crate::resource::url_looks_like_image_asset(final_url)
  {
    return false;
  }

  true
}

// ============================================================================
// ImageCache
// ============================================================================

/// Configuration for [`ImageCache`].
#[derive(Debug, Clone, Copy)]
pub struct ImageCacheConfig {
  /// Maximum number of decoded pixels (width * height). `0` disables the limit.
  pub max_decoded_pixels: u64,
  /// Maximum allowed width or height for a decoded image. `0` disables the limit.
  pub max_decoded_dimension: u32,
  /// Maximum number of decoded images kept in memory (`0` disables eviction by count).
  pub max_cached_images: usize,
  /// Maximum estimated bytes of decoded images kept in memory (`0` disables eviction by size).
  pub max_cached_image_bytes: usize,
  /// Maximum number of rasterized SVG pixmaps kept in memory (`0` disables eviction by count).
  pub max_cached_svg_pixmaps: usize,
  /// Maximum estimated bytes of cached SVG pixmaps (`0` disables eviction by size).
  pub max_cached_svg_bytes: usize,
  /// Maximum number of cached premultiplied raster pixmaps (`0` disables eviction by count).
  pub max_cached_raster_pixmaps: usize,
  /// Maximum estimated bytes of cached raster pixmaps (`0` disables eviction by size).
  pub max_cached_raster_bytes: usize,
}

impl Default for ImageCacheConfig {
  fn default() -> Self {
    const DEFAULT_MAX_RASTER_PIXMAP_CACHE_ITEMS: usize = 256;
    const DEFAULT_MAX_RASTER_PIXMAP_CACHE_BYTES: usize = 128 * 1024 * 1024;

    let max_cached_raster_pixmaps = std::env::var("FASTR_IMAGE_RASTER_PIXMAP_CACHE_ITEMS")
      .ok()
      .and_then(|v| v.trim().parse::<usize>().ok())
      .unwrap_or(DEFAULT_MAX_RASTER_PIXMAP_CACHE_ITEMS);
    let max_cached_raster_bytes = std::env::var("FASTR_IMAGE_RASTER_PIXMAP_CACHE_BYTES")
      .ok()
      .and_then(|v| v.trim().parse::<usize>().ok())
      .unwrap_or(DEFAULT_MAX_RASTER_PIXMAP_CACHE_BYTES);

    Self {
      max_decoded_pixels: 100_000_000,
      max_decoded_dimension: 32768,
      max_cached_images: 256,
      max_cached_image_bytes: 256 * 1024 * 1024,
      max_cached_svg_pixmaps: 128,
      max_cached_svg_bytes: 128 * 1024 * 1024,
      max_cached_raster_pixmaps,
      max_cached_raster_bytes,
    }
  }
}

impl ImageCacheConfig {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_max_decoded_pixels(mut self, max: u64) -> Self {
    self.max_decoded_pixels = max;
    self
  }

  pub fn with_max_decoded_dimension(mut self, max: u32) -> Self {
    self.max_decoded_dimension = max;
    self
  }

  pub fn with_max_cached_images(mut self, max: usize) -> Self {
    self.max_cached_images = max;
    self
  }

  pub fn with_max_cached_image_bytes(mut self, max: usize) -> Self {
    self.max_cached_image_bytes = max;
    self
  }

  pub fn with_max_cached_svg_pixmaps(mut self, max: usize) -> Self {
    self.max_cached_svg_pixmaps = max;
    self
  }

  pub fn with_max_cached_svg_bytes(mut self, max: usize) -> Self {
    self.max_cached_svg_bytes = max;
    self
  }

  pub fn with_max_cached_raster_pixmaps(mut self, max: usize) -> Self {
    self.max_cached_raster_pixmaps = max;
    self
  }

  pub fn with_max_cached_raster_bytes(mut self, max: usize) -> Self {
    self.max_cached_raster_bytes = max;
    self
  }
}

#[derive(Clone)]
enum SharedImageResult {
  Success(Arc<CachedImage>),
  Error(Error),
}

impl SharedImageResult {
  fn as_result(&self) -> Result<Arc<CachedImage>> {
    match self {
      Self::Success(img) => Ok(Arc::clone(img)),
      Self::Error(err) => Err(err.clone()),
    }
  }
}

struct DecodeInFlight {
  result: Mutex<Option<SharedImageResult>>,
  cv: Condvar,
}

impl DecodeInFlight {
  fn new() -> Self {
    Self {
      result: Mutex::new(None),
      cv: Condvar::new(),
    }
  }

  fn set(&self, result: SharedImageResult) {
    let mut slot = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    self.cv.notify_all();
  }

  fn wait(&self, _url: &str) -> Result<Arc<CachedImage>> {
    let mut guard = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let deadline = render_control::root_deadline().filter(|d| d.is_enabled());
    while guard.is_none() {
      if let Some(deadline) = deadline.as_ref() {
        deadline.check(RenderStage::Paint).map_err(Error::Render)?;
        let wait_for = if deadline.timeout_limit().is_some() {
          match deadline.remaining_timeout() {
            Some(remaining) if !remaining.is_zero() => remaining.min(Duration::from_millis(10)),
            _ => {
              return Err(Error::Render(RenderError::Timeout {
                stage: RenderStage::Paint,
                elapsed: deadline.elapsed(),
              }));
            }
          }
        } else {
          Duration::from_millis(10)
        };
        guard = self
          .cv
          .wait_timeout(guard, wait_for)
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .0;
      } else {
        guard = self
          .cv
          .wait(guard)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
      }
    }
    guard.as_ref().unwrap().as_result()
  }
}

#[derive(Clone)]
enum SharedMetaResult {
  Success(Arc<CachedImageMetadata>),
  Error(Error),
}

impl SharedMetaResult {
  fn as_result(&self) -> Result<Arc<CachedImageMetadata>> {
    match self {
      Self::Success(meta) => Ok(Arc::clone(meta)),
      Self::Error(err) => Err(err.clone()),
    }
  }
}

struct ProbeInFlight {
  result: Mutex<Option<SharedMetaResult>>,
  cv: Condvar,
}

impl ProbeInFlight {
  fn new() -> Self {
    Self {
      result: Mutex::new(None),
      cv: Condvar::new(),
    }
  }

  fn set(&self, result: SharedMetaResult) {
    let mut slot = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    self.cv.notify_all();
  }

  fn wait(&self, _url: &str) -> Result<Arc<CachedImageMetadata>> {
    let mut guard = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let deadline = render_control::root_deadline().filter(|d| d.is_enabled());
    while guard.is_none() {
      if let Some(deadline) = deadline.as_ref() {
        deadline.check(RenderStage::Paint).map_err(Error::Render)?;
        let wait_for = if deadline.timeout_limit().is_some() {
          match deadline.remaining_timeout() {
            Some(remaining) if !remaining.is_zero() => remaining.min(Duration::from_millis(10)),
            _ => {
              return Err(Error::Render(RenderError::Timeout {
                stage: RenderStage::Paint,
                elapsed: deadline.elapsed(),
              }));
            }
          }
        } else {
          Duration::from_millis(10)
        };
        guard = self
          .cv
          .wait_timeout(guard, wait_for)
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .0;
      } else {
        guard = self
          .cv
          .wait(guard)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
      }
    }
    guard.as_ref().unwrap().as_result()
  }
}

/// Cache for loaded images
///
/// `ImageCache` provides in-memory caching of decoded images, with support for
/// loading from URLs, files, and data URLs. It uses a [`ResourceFetcher`] for
/// the actual byte fetching, allowing custom fetching strategies (caching,
/// mocking, etc.) to be injected.
///
/// # Example
///
/// ```rust,no_run
/// # use fastrender::image_loader::ImageCache;
/// # use fastrender::resource::HttpFetcher;
/// # use std::sync::Arc;
/// # fn main() -> fastrender::Result<()> {
///
/// let fetcher = Arc::new(HttpFetcher::new());
/// let cache = ImageCache::with_fetcher(fetcher);
/// let image = cache.load("https://example.com/image.png")?;
/// # Ok(())
/// # }
/// ```
pub struct ImageCache {
  instance_id: u64,
  /// In-memory cache of decoded images (keyed by resolved URL)
  cache: Arc<Mutex<SizedLruCache<String, Arc<CachedImage>>>>,
  /// In-flight decodes keyed by resolved URL to de-duplicate concurrent loads.
  in_flight: Arc<Mutex<HashMap<String, Arc<DecodeInFlight>>>>,
  /// In-memory cache of probed metadata (keyed by resolved URL).
  meta_cache: Arc<Mutex<HashMap<String, Arc<CachedImageMetadata>>>>,
  /// Raw resources captured during metadata probes to avoid duplicate fetches between layout and paint.
  raw_cache: Arc<Mutex<HashMap<String, Arc<FetchedResource>>>>,
  /// In-flight probes keyed by resolved URL to de-duplicate concurrent metadata loads.
  meta_in_flight: Arc<Mutex<HashMap<String, Arc<ProbeInFlight>>>>,
  /// In-memory cache of rendered inline SVG pixmaps keyed by (hash, size).
  svg_pixmap_cache: Arc<Mutex<SizedLruCache<SvgPixmapKey, Arc<tiny_skia::Pixmap>>>>,
  /// In-memory cache of premultiplied raster pixmaps keyed by URL + orientation.
  raster_pixmap_cache: Arc<Mutex<SizedLruCache<RasterPixmapKey, Arc<tiny_skia::Pixmap>>>>,
  /// Base URL for resolving relative image sources
  base_url: Option<String>,
  /// Resource fetcher for loading bytes from URLs
  fetcher: Arc<dyn ResourceFetcher>,
  /// Decode limits.
  config: ImageCacheConfig,
  /// Optional diagnostics sink for recording fetch failures.
  diagnostics: Option<Arc<Mutex<RenderDiagnostics>>>,
  /// Optional resource context (policy + diagnostics).
  resource_context: Option<ResourceContext>,
}

struct DecodeInFlightOwnerGuard<'a> {
  cache: &'a ImageCache,
  url: &'a str,
  flight: Arc<DecodeInFlight>,
  finished: bool,
}

impl<'a> DecodeInFlightOwnerGuard<'a> {
  fn new(cache: &'a ImageCache, url: &'a str, flight: Arc<DecodeInFlight>) -> Self {
    Self {
      cache,
      url,
      flight,
      finished: false,
    }
  }

  fn finish(&mut self, result: SharedImageResult) {
    if self.finished {
      return;
    }
    self.finished = true;
    self.cache.finish_inflight(self.url, &self.flight, result);
  }
}

impl Drop for DecodeInFlightOwnerGuard<'_> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }

    self.finished = true;
    let err = Error::Image(ImageError::LoadFailed {
      url: self.url.to_string(),
      reason: "in-flight image decode owner dropped without resolving".to_string(),
    });
    self.cache.record_image_error(self.url, &err);
    self
      .cache
      .finish_inflight(self.url, &self.flight, SharedImageResult::Error(err));
  }
}

struct ProbeInFlightOwnerGuard<'a> {
  cache: &'a ImageCache,
  url: &'a str,
  flight: Arc<ProbeInFlight>,
  finished: bool,
}

impl<'a> ProbeInFlightOwnerGuard<'a> {
  fn new(cache: &'a ImageCache, url: &'a str, flight: Arc<ProbeInFlight>) -> Self {
    Self {
      cache,
      url,
      flight,
      finished: false,
    }
  }

  fn finish(&mut self, result: SharedMetaResult) {
    if self.finished {
      return;
    }
    self.finished = true;
    self
      .cache
      .finish_meta_inflight(self.url, &self.flight, result);
  }
}

impl Drop for ProbeInFlightOwnerGuard<'_> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }

    self.finished = true;
    let err = Error::Image(ImageError::LoadFailed {
      url: self.url.to_string(),
      reason: "in-flight image probe owner dropped without resolving".to_string(),
    });
    self.cache.record_image_error(self.url, &err);
    self
      .cache
      .finish_meta_inflight(self.url, &self.flight, SharedMetaResult::Error(err));
  }
}

impl ImageCache {
  /// Create a new ImageCache with the default HTTP fetcher
  pub fn new() -> Self {
    Self::with_config(ImageCacheConfig::default())
  }

  /// Create a new ImageCache with a custom fetcher
  pub fn with_fetcher(fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self::with_fetcher_and_config(fetcher, ImageCacheConfig::default())
  }

  /// Create a new ImageCache with the default HTTP fetcher and custom limits.
  pub fn with_config(config: ImageCacheConfig) -> Self {
    Self::with_base_url_fetcher_and_config(
      None,
      Arc::new(CachingFetcher::with_config(
        HttpFetcher::new(),
        CachingFetcherConfig::default(),
      )),
      config,
    )
  }

  /// Create a new ImageCache with a custom fetcher and limits.
  pub fn with_fetcher_and_config(
    fetcher: Arc<dyn ResourceFetcher>,
    config: ImageCacheConfig,
  ) -> Self {
    Self::with_base_url_fetcher_and_config(None, fetcher, config)
  }

  /// Create a new ImageCache with a base URL and the default HTTP fetcher
  pub fn with_base_url(base_url: String) -> Self {
    Self::with_base_url_and_config(base_url, ImageCacheConfig::default())
  }

  /// Create a new ImageCache with a base URL, default fetcher, and custom limits.
  pub fn with_base_url_and_config(base_url: String, config: ImageCacheConfig) -> Self {
    Self::with_base_url_fetcher_and_config(
      Some(base_url),
      Arc::new(CachingFetcher::with_config(
        HttpFetcher::new(),
        CachingFetcherConfig::default(),
      )),
      config,
    )
  }

  /// Create a new ImageCache with both a base URL and a custom fetcher
  pub fn with_base_url_and_fetcher(base_url: String, fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self::with_base_url_fetcher_and_config(Some(base_url), fetcher, ImageCacheConfig::default())
  }

  /// Create a new ImageCache with a base URL, custom fetcher, and limits.
  pub fn with_base_url_and_fetcher_and_config(
    base_url: String,
    fetcher: Arc<dyn ResourceFetcher>,
    config: ImageCacheConfig,
  ) -> Self {
    Self::with_base_url_fetcher_and_config(Some(base_url), fetcher, config)
  }

  fn with_base_url_fetcher_and_config(
    base_url: Option<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    config: ImageCacheConfig,
  ) -> Self {
    Self {
      instance_id: NEXT_IMAGE_CACHE_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
      cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_images,
        config.max_cached_image_bytes,
      ))),
      in_flight: Arc::new(Mutex::new(HashMap::new())),
      meta_cache: Arc::new(Mutex::new(HashMap::new())),
      raw_cache: Arc::new(Mutex::new(HashMap::new())),
      meta_in_flight: Arc::new(Mutex::new(HashMap::new())),
      svg_pixmap_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_svg_pixmaps,
        config.max_cached_svg_bytes,
      ))),
      raster_pixmap_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_raster_pixmaps,
        config.max_cached_raster_bytes,
      ))),
      base_url,
      fetcher,
      config,
      diagnostics: None,
      resource_context: None,
    }
  }

  /// Sets or replaces the base URL used to resolve relative image sources.
  pub fn set_base_url(&mut self, base_url: impl Into<String>) {
    self.base_url = Some(base_url.into());
  }

  /// Clears any previously configured base URL.
  pub fn clear_base_url(&mut self) {
    self.base_url = None;
  }

  /// Returns the configured base URL for resolving relative paths.
  pub fn base_url(&self) -> Option<String> {
    self.base_url.clone()
  }

  /// Set the active resource context for policy and diagnostics.
  pub fn set_resource_context(&mut self, context: Option<ResourceContext>) {
    self.resource_context = context;
  }

  /// Retrieve the current resource context.
  pub fn resource_context(&self) -> Option<ResourceContext> {
    self.resource_context.clone()
  }

  /// Sets the resource fetcher
  pub fn set_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    self.fetcher = fetcher;
  }

  /// Returns a reference to the current fetcher
  pub fn fetcher(&self) -> &Arc<dyn ResourceFetcher> {
    &self.fetcher
  }

  pub(crate) fn instance_id(&self) -> u64 {
    self.instance_id
  }

  pub(crate) fn is_placeholder_image(&self, image: &Arc<CachedImage>) -> bool {
    Arc::ptr_eq(image, &about_url_placeholder_image())
  }

  /// Attach a diagnostics sink for recording fetch failures.
  pub fn set_diagnostics_sink(&mut self, diagnostics: Option<Arc<Mutex<RenderDiagnostics>>>) {
    self.diagnostics = diagnostics;
  }

  /// Resolve a potentially relative URL to an absolute URL
  pub fn resolve_url(&self, url: &str) -> String {
    let url = trim_ascii_whitespace(url);
    if url.is_empty() {
      return String::new();
    }

    // Absolute or data URLs can be returned directly.
    if crate::resource::is_data_url(url) {
      // Data URLs often appear inside CSS/JS strings with backslash-escaped quotes
      // (e.g. `data:image/svg+xml,<svg xmlns=\\\"...\\\">`). Unescape those so the SVG/XML parser
      // sees valid markup.
      if url.contains('\\') {
        return unescape_js_escapes(url).into_owned();
      }
      return url.to_string();
    }
    if let Ok(parsed) = url::Url::parse(url) {
      return parsed.to_string();
    }

    // Resolve against the configured base URL when present.
    if let Some(base) = &self.base_url {
      if let Some(resolved) = resolve_against_base(base, url) {
        return resolved;
      }
    }

    // No usable base; return the reference unchanged.
    url.to_string()
  }

  fn cache_key_for_crossorigin(
    &self,
    resolved_url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> String {
    // ImageCache keys must be partitioned by any request metadata that can affect the fetch
    // profile. Otherwise, we'd risk satisfying a request with cached results that were fetched
    // under a different referrer policy / referrer URL / CORS mode.
    //
    // Note: The key intentionally includes the *effective* referrer policy (after resolving the
    // empty-string state to the Chromium default) and a hashed representation of the document URL
    // used as the request referrer.
    let crossorigin_key = match crossorigin {
      CrossOriginAttribute::None => "none",
      CrossOriginAttribute::Anonymous => "anonymous",
      CrossOriginAttribute::UseCredentials => "use-credentials",
    };

    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let referrer_hash = referrer_url.map(|url| {
      let mut hasher = DefaultHasher::new();
      url.hash(&mut hasher);
      (hasher.finish(), url.len())
    });

    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let effective_referrer_policy = match request_referrer_policy {
      ReferrerPolicy::EmptyString => ReferrerPolicy::CHROMIUM_DEFAULT,
      other => other,
    };

    let mut key = resolved_url.to_string();
    key.push_str("@@crossorigin=");
    key.push_str(crossorigin_key);

    key.push_str("@@referrer=");
    match referrer_hash {
      Some((hash, len)) => {
        key.push_str(&format!("{hash:016x}:{len}"));
      }
      None => key.push_str("none"),
    }

    key.push_str("@@referrer_policy=");
    key.push_str(effective_referrer_policy.as_str());

    if crossorigin != CrossOriginAttribute::None {
      let document_origin = self
        .resource_context
        .as_ref()
        .and_then(|ctx| ctx.policy.document_origin.as_ref())
        .map(|origin| origin.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
      key.push_str("@@doc_origin=");
      key.push_str(&document_origin);
    }

    key
  }

  /// Load an image from a URL or file path
  ///
  /// The URL is first resolved against the base URL if one is configured.
  /// Results are cached in memory, so subsequent loads of the same URL
  /// return the cached image.
  pub fn load(&self, url: &str) -> Result<Arc<CachedImage>> {
    self.load_with_crossorigin(url, CrossOriginAttribute::None)
  }

  /// Load an image, using the provided `<img crossorigin>` state to control the fetch mode.
  ///
  /// When `crossorigin` is not [`CrossOriginAttribute::None`], the resource is fetched in CORS
  /// mode (headers: `Origin`, `Sec-Fetch-Mode: cors`) and, when the `FASTR_FETCH_ENFORCE_CORS`
  /// runtime toggle is enabled, the response's `Access-Control-Allow-Origin`/`Credentials` headers
  /// are validated.
  pub fn load_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImage>> {
    self.load_with_crossorigin_and_referrer_policy(url, crossorigin, None)
  }

  pub fn load_with_crossorigin_and_referrer_policy(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImage>> {
    let trimmed = trim_ascii_whitespace(url);
    if trimmed.is_empty() {
      return Ok(about_url_placeholder_image());
    }
    if let Some(svg) = decode_inline_svg_url(trimmed) {
      return self.render_svg(svg.as_str());
    }
    if is_about_url(trimmed) {
      return Ok(about_url_placeholder_image());
    }

    // Resolve the URL first.
    let resolved_url = self.resolve_url(trimmed);
    self.enforce_image_policy(&resolved_url)?;

    let destination = match crossorigin {
      CrossOriginAttribute::None => FetchDestination::Image,
      _ => FetchDestination::ImageCors,
    };
    let cache_key = self.cache_key_for_crossorigin(&resolved_url, crossorigin, referrer_policy);

    // Check cache first.
    record_image_cache_request();
    if let Some(img) = self.get_cached(&cache_key) {
      record_image_cache_hit();
      return Ok(img);
    }
    let (flight, is_owner) = self.join_inflight(&cache_key);
    if !is_owner {
      record_image_cache_hit();
      return flight.wait(&cache_key);
    }

    let mut inflight_guard = DecodeInFlightOwnerGuard::new(self, &cache_key, flight);

    if let Some(resource) = self.take_raw_cached_resource(&cache_key) {
      record_image_cache_hit();
      let result =
        self.decode_resource_into_cache(&cache_key, &resolved_url, &resource, crossorigin);
      let shared = match &result {
        Ok(img) => SharedImageResult::Success(Arc::clone(img)),
        Err(err) => {
          self.record_image_error(&resolved_url, err);
          SharedImageResult::Error(err.clone())
        }
      };
      inflight_guard.finish(shared);
      return result;
    }

    record_image_cache_miss();
    let result = self.fetch_and_decode(
      &cache_key,
      &resolved_url,
      destination,
      crossorigin,
      referrer_policy,
    );
    let shared = match &result {
      Ok(img) => SharedImageResult::Success(Arc::clone(img)),
      Err(err) => SharedImageResult::Error(err.clone()),
    };
    inflight_guard.finish(shared);

    result
  }

  /// Load a decoded raster image and convert it to a premultiplied [`tiny_skia::Pixmap`],
  /// caching the result for reuse in subsequent paint calls.
  ///
  /// Returns `Ok(None)` when the resource is a vector image (SVG) or the conversion fails.
  pub fn load_raster_pixmap(
    &self,
    url: &str,
    orientation: OrientationTransform,
    decorative: bool,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;

    let cache_key = self.cache_key_for_crossorigin(&resolved_url, CrossOriginAttribute::None, None);
    let key = raster_pixmap_full_key(&cache_key, orientation, decorative);
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load(&resolved_url)?;
    if image.is_vector {
      return Ok(None);
    }

    let (width, height) = image.oriented_dimensions(orientation);
    if width == 0 || height == 0 {
      return Ok(None);
    }

    let Some(bytes) = u64::from(width)
      .checked_mul(u64::from(height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };

    let rgba = image.to_oriented_rgba(orientation);
    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != width || rgba_h != height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(width, height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  pub fn load_raster_pixmap_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    orientation: OrientationTransform,
    decorative: bool,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    if crossorigin == CrossOriginAttribute::None {
      return self.load_raster_pixmap(url, orientation, decorative);
    }

    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;
    let cache_key = self.cache_key_for_crossorigin(&resolved_url, crossorigin, None);

    let key = raster_pixmap_full_key(&cache_key, orientation, decorative);
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load_with_crossorigin(&resolved_url, crossorigin)?;
    if image.is_vector {
      return Ok(None);
    }

    let (width, height) = image.oriented_dimensions(orientation);
    if width == 0 || height == 0 {
      return Ok(None);
    }

    let Some(bytes) = u64::from(width)
      .checked_mul(u64::from(height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };

    let rgba = image.to_oriented_rgba(orientation);
    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != width || rgba_h != height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(width, height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  /// Load a decoded raster image and convert it to a premultiplied [`tiny_skia::Pixmap`], but
  /// first resample it toward the requested target size.
  ///
  /// This is intended for paint call sites that render the image substantially smaller than its
  /// intrinsic dimensions: resampling before premultiplication keeps work proportional to the
  /// destination pixel count.
  ///
  /// Callers should pass target sizes in *device pixels* (after applying device pixel ratio) and
  /// only use this path when downscaling is desired (i.e., target <= intrinsic). If the requested
  /// target would require upscaling, this function falls back to [`Self::load_raster_pixmap`].
  pub fn load_raster_pixmap_at_size(
    &self,
    url: &str,
    orientation: OrientationTransform,
    decorative: bool,
    target_width: u32,
    target_height: u32,
    quality: FilterQuality,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    if target_width == 0 || target_height == 0 {
      return Ok(None);
    }
    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;

    let cache_key = self.cache_key_for_crossorigin(&resolved_url, CrossOriginAttribute::None, None);
    let key = raster_pixmap_key(
      &cache_key,
      orientation,
      decorative,
      target_width,
      target_height,
      raster_pixmap_quality_bits(quality),
    );
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load(&resolved_url)?;
    if image.is_vector {
      return Ok(None);
    }

    let (src_w, src_h) = image.oriented_dimensions(orientation);
    if src_w == 0 || src_h == 0 {
      return Ok(None);
    }
    if target_width >= src_w || target_height >= src_h {
      // Callers should only use this path when downscaling. If any axis would require upscaling,
      // fall back to the full-resolution pixmap cache so results stay stable (important for
      // pixelated/crisp-edges semantics and to avoid needless resampling work).
      return self.load_raster_pixmap(&resolved_url, orientation, decorative);
    }

    let Some(bytes) = u64::from(target_width)
      .checked_mul(u64::from(target_height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };

    let filter = match quality {
      FilterQuality::Nearest => image::imageops::FilterType::Nearest,
      FilterQuality::Bilinear => image::imageops::FilterType::Triangle,
      _ => image::imageops::FilterType::Triangle,
    };

    // We resize in the decoded image's native orientation and then apply the requested
    // orientation transform on the smaller output buffer.
    let (pre_w, pre_h) = if orientation.swaps_axes() {
      (target_height, target_width)
    } else {
      (target_width, target_height)
    };
    let resized = image.image.resize_exact(pre_w, pre_h, filter);
    let mut rgba = resized.to_rgba8();

    match orientation.quarter_turns % 4 {
      0 => {}
      1 => rgba = imageops::rotate90(&rgba),
      2 => rgba = imageops::rotate180(&rgba),
      3 => rgba = imageops::rotate270(&rgba),
      _ => {}
    }
    if orientation.flip_x {
      rgba = imageops::flip_horizontal(&rgba);
    }

    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != target_width || rgba_h != target_height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(target_width, target_height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  pub fn load_raster_pixmap_at_size_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    orientation: OrientationTransform,
    decorative: bool,
    target_width: u32,
    target_height: u32,
    quality: FilterQuality,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    if crossorigin == CrossOriginAttribute::None {
      return self.load_raster_pixmap_at_size(
        url,
        orientation,
        decorative,
        target_width,
        target_height,
        quality,
      );
    }

    if target_width == 0 || target_height == 0 {
      return Ok(None);
    }
    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;

    let cache_key = self.cache_key_for_crossorigin(&resolved_url, crossorigin, None);
    let key = raster_pixmap_key(
      &cache_key,
      orientation,
      decorative,
      target_width,
      target_height,
      raster_pixmap_quality_bits(quality),
    );
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load_with_crossorigin(&resolved_url, crossorigin)?;
    if image.is_vector {
      return Ok(None);
    }

    let (src_w, src_h) = image.oriented_dimensions(orientation);
    if src_w == 0 || src_h == 0 {
      return Ok(None);
    }
    if target_width >= src_w || target_height >= src_h {
      // Callers should only use this path when downscaling. If any axis would require upscaling,
      // fall back to the full-resolution pixmap cache so results stay stable (important for
      // pixelated/crisp-edges semantics and to avoid needless resampling work).
      return self.load_raster_pixmap_with_crossorigin(
        &resolved_url,
        crossorigin,
        orientation,
        decorative,
      );
    }

    let Some(bytes) = u64::from(target_width)
      .checked_mul(u64::from(target_height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };

    let filter = match quality {
      FilterQuality::Nearest => image::imageops::FilterType::Nearest,
      FilterQuality::Bilinear => image::imageops::FilterType::Triangle,
      _ => image::imageops::FilterType::Triangle,
    };

    // We resize in the decoded image's native orientation and then apply the requested
    // orientation transform on the smaller output buffer.
    let (pre_w, pre_h) = if orientation.swaps_axes() {
      (target_height, target_width)
    } else {
      (target_width, target_height)
    };
    let resized = image.image.resize_exact(pre_w, pre_h, filter);
    let mut rgba = resized.to_rgba8();

    match orientation.quarter_turns % 4 {
      0 => {}
      1 => rgba = imageops::rotate90(&rgba),
      2 => rgba = imageops::rotate180(&rgba),
      3 => rgba = imageops::rotate270(&rgba),
      _ => {}
    }
    if orientation.flip_x {
      rgba = imageops::flip_horizontal(&rgba);
    }

    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != target_width || rgba_h != target_height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(target_width, target_height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  /// Probe image metadata (dimensions, EXIF orientation/resolution, SVG intrinsic ratio)
  /// without fully decoding the image.
  pub fn probe(&self, url: &str) -> Result<Arc<CachedImageMetadata>> {
    self.probe_with_crossorigin(url, CrossOriginAttribute::None)
  }

  /// Probe image metadata, using the provided `<img crossorigin>` state to control the fetch mode.
  ///
  /// When `crossorigin` is not [`CrossOriginAttribute::None`], the probe issues a CORS-mode fetch
  /// (`Sec-Fetch-Mode: cors`, `Origin`) and, when `FASTR_FETCH_ENFORCE_CORS` is enabled, validates
  /// the response's `Access-Control-Allow-Origin`/`Credentials` headers.
  pub fn probe_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImageMetadata>> {
    self.probe_with_crossorigin_and_referrer_policy(url, crossorigin, None)
  }

  pub fn probe_with_crossorigin_and_referrer_policy(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    let trimmed = trim_ascii_whitespace(url);
    if trimmed.is_empty() {
      return Ok(about_url_placeholder_metadata());
    }
    if let Some(svg) = decode_inline_svg_url(trimmed) {
      let cache_key = inline_svg_cache_key(trim_ascii_whitespace_start(&svg));
      record_image_cache_request();

      if let Some(img) = self.get_cached(&cache_key) {
        record_image_cache_hit();
        return Ok(Arc::new(CachedImageMetadata::from(&*img)));
      }

      if let Some(meta) = self.get_cached_meta(&cache_key) {
        record_image_cache_hit();
        return Ok(meta);
      }

      let (flight, is_owner) = self.join_meta_inflight(&cache_key);
      if !is_owner {
        record_image_cache_hit();
        return flight.wait(&cache_key);
      }

      let mut inflight_guard = ProbeInFlightOwnerGuard::new(self, &cache_key, flight);
      record_image_cache_miss();
      let result = self
        .probe_svg_content(svg.as_str(), "inline-svg")
        .map(Arc::new);
      let shared = match &result {
        Ok(meta) => {
          if let Ok(mut cache) = self.meta_cache.lock() {
            cache.insert(cache_key.clone(), Arc::clone(meta));
          }
          SharedMetaResult::Success(Arc::clone(meta))
        }
        Err(err) => SharedMetaResult::Error(err.clone()),
      };
      inflight_guard.finish(shared);
      return result;
    }
    if is_about_url(trimmed) {
      return Ok(about_url_placeholder_metadata());
    }

    let resolved_url = self.resolve_url(trimmed);
    self.probe_resolved_with_crossorigin_and_referrer_policy(
      &resolved_url,
      crossorigin,
      referrer_policy,
    )
  }

  pub fn probe_resolved(&self, resolved_url: &str) -> Result<Arc<CachedImageMetadata>> {
    self.probe_resolved_with_crossorigin(resolved_url, CrossOriginAttribute::None)
  }

  pub fn probe_resolved_with_crossorigin(
    &self,
    resolved_url: &str,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImageMetadata>> {
    self.probe_resolved_with_crossorigin_and_referrer_policy(resolved_url, crossorigin, None)
  }

  pub fn probe_resolved_with_crossorigin_and_referrer_policy(
    &self,
    resolved_url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    let resolved_url = trim_ascii_whitespace(resolved_url);
    let trimmed = trim_ascii_whitespace_start(resolved_url);
    if trimmed.starts_with('<') {
      return self.probe_with_crossorigin(trimmed, crossorigin);
    }
    let cache_key = self.cache_key_for_crossorigin(resolved_url, crossorigin, referrer_policy);
    let kind = match crossorigin {
      CrossOriginAttribute::None => FetchContextKind::Image,
      _ => FetchContextKind::ImageCors,
    };
    self.probe_resolved_url(&cache_key, resolved_url, kind, crossorigin, referrer_policy)
  }

  fn probe_resolved_url(
    &self,
    cache_key: &str,
    resolved_url: &str,
    kind: FetchContextKind,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    if resolved_url.is_empty() {
      return Err(Error::Image(ImageError::LoadFailed {
        url: resolved_url.to_string(),
        reason: "image probe URL is empty".to_string(),
      }));
    }
    if is_about_url(resolved_url) {
      return Ok(about_url_placeholder_metadata());
    }

    self.enforce_image_policy(resolved_url)?;
    record_image_cache_request();
    if let Some(img) = self.get_cached(cache_key) {
      record_image_cache_hit();
      return Ok(Arc::new(CachedImageMetadata::from(&*img)));
    }

    if let Some(meta) = self.get_cached_meta(cache_key) {
      record_image_cache_hit();
      return Ok(meta);
    }

    let (flight, is_owner) = self.join_meta_inflight(cache_key);
    if !is_owner {
      record_image_cache_hit();
      return flight.wait(cache_key);
    }

    let mut inflight_guard = ProbeInFlightOwnerGuard::new(self, cache_key, flight);

    let destination = match kind {
      FetchContextKind::ImageCors => FetchDestination::ImageCors,
      _ => FetchDestination::Image,
    };
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let origin_fallback = referrer_url.and_then(origin_from_url);
    let client_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .or(origin_fallback.as_ref());
    let fetch_url_no_fragment = strip_url_fragment(resolved_url);
    let mut request = FetchRequest::new(fetch_url_no_fragment.as_ref(), destination)
      .with_credentials_mode(credentials_mode);
    if let Some(origin) = client_origin {
      request = request.with_client_origin(origin);
    }
    if let Some(referrer_url) = referrer_url {
      request = request.with_referrer_url(referrer_url);
    }
    request = request.with_referrer_policy(request_referrer_policy);

    if let Some(cached) = self
      .fetcher
      .read_cache_artifact_with_request(request, CacheArtifactKind::ImageProbeMetadata)
    {
      if let Some(ctx) = &self.resource_context {
        let policy_url = cached
          .final_url
          .as_deref()
          .unwrap_or(fetch_url_no_fragment.as_ref());
        if let Err(err) = ctx.check_allowed(ResourceKind::Image, policy_url) {
          let blocked = Error::Image(ImageError::LoadFailed {
            url: resolved_url.to_string(),
            reason: err.reason,
          });
          self.record_image_error(resolved_url, &blocked);
          inflight_guard.finish(SharedMetaResult::Error(blocked.clone()));
          return Err(blocked);
        }
      }
      if let Err(err) = self.enforce_image_cors(resolved_url, &cached, crossorigin) {
        self.record_image_error(resolved_url, &err);
        inflight_guard.finish(SharedMetaResult::Error(err.clone()));
        return Err(err);
      }

      if let Some(decoded) = decode_probe_metadata_from_disk(&cached.bytes) {
        let meta = Arc::new(decoded);
        if let Ok(mut cache) = self.meta_cache.lock() {
          cache.insert(cache_key.to_string(), Arc::clone(&meta));
        }
        record_image_cache_hit();
        inflight_guard.finish(SharedMetaResult::Success(Arc::clone(&meta)));
        return Ok(meta);
      }

      // Corrupt or incompatible cache entry; evict so we don't repeatedly reparse it.
      let artifact_url = cached
        .final_url
        .as_deref()
        .unwrap_or(fetch_url_no_fragment.as_ref());
      let mut remove_request =
        FetchRequest::new(artifact_url, destination).with_credentials_mode(credentials_mode);
      if let Some(origin) = client_origin {
        remove_request = remove_request.with_client_origin(origin);
      }
      if let Some(referrer_url) = referrer_url {
        remove_request = remove_request.with_referrer_url(referrer_url);
      }
      remove_request = remove_request.with_referrer_policy(request_referrer_policy);
      self
        .fetcher
        .remove_cache_artifact_with_request(remove_request, CacheArtifactKind::ImageProbeMetadata);
    }

    record_image_cache_miss();
    let result = self.fetch_and_probe(
      cache_key,
      resolved_url,
      destination,
      crossorigin,
      referrer_policy,
    );
    let shared = match &result {
      Ok(meta) => SharedMetaResult::Success(Arc::clone(meta)),
      Err(err) => SharedMetaResult::Error(err.clone()),
    };
    inflight_guard.finish(shared);

    result
  }

  fn get_cached(&self, resolved_url: &str) -> Option<Arc<CachedImage>> {
    self
      .cache
      .lock()
      .ok()
      .and_then(|mut cache| cache.get_cloned(resolved_url))
  }

  fn get_cached_meta(&self, resolved_url: &str) -> Option<Arc<CachedImageMetadata>> {
    self
      .meta_cache
      .lock()
      .ok()
      .and_then(|cache| cache.get(resolved_url).cloned())
  }

  fn take_raw_cached_resource(&self, resolved_url: &str) -> Option<Arc<FetchedResource>> {
    self
      .raw_cache
      .lock()
      .ok()
      .and_then(|mut cache| cache.remove(resolved_url))
  }

  fn cache_placeholder_image(&self, resolved_url: &str) -> Arc<CachedImage> {
    let image = about_url_placeholder_image();
    self.insert_cached_image(resolved_url, Arc::clone(&image));
    let meta = about_url_placeholder_metadata();
    if let Ok(mut cache) = self.meta_cache.lock() {
      cache.insert(resolved_url.to_string(), Arc::clone(&meta));
    }
    image
  }

  fn cache_placeholder_metadata(&self, resolved_url: &str) -> Arc<CachedImageMetadata> {
    let meta = about_url_placeholder_metadata();
    if let Ok(mut cache) = self.meta_cache.lock() {
      cache.insert(resolved_url.to_string(), Arc::clone(&meta));
    }
    meta
  }

  fn insert_cached_image(&self, resolved_url: &str, image: Arc<CachedImage>) {
    let mut bytes = Self::estimate_image_bytes(&image.image);
    if let Some(svg) = &image.svg_content {
      bytes = bytes.saturating_add(svg.len());
    }
    if let Ok(mut cache) = self.cache.lock() {
      cache.insert(resolved_url.to_string(), image, bytes);
    }
  }

  fn insert_svg_pixmap(&self, key: SvgPixmapKey, pixmap: Arc<tiny_skia::Pixmap>) {
    let bytes = pixmap.data().len();
    if let Ok(mut cache) = self.svg_pixmap_cache.lock() {
      cache.insert(key, pixmap, bytes);
    }
  }

  fn estimate_image_bytes(image: &DynamicImage) -> usize {
    let (width, height) = image.dimensions();
    let pixels = usize::try_from(width)
      .unwrap_or(0)
      .saturating_mul(usize::try_from(height).unwrap_or(0));
    let bpp = usize::from(image.color().bytes_per_pixel()).max(1);
    pixels.saturating_mul(bpp)
  }

  fn record_image_error(&self, url: &str, error: &Error) {
    if let Some(diag) = &self.diagnostics {
      if let Ok(mut guard) = diag.lock() {
        guard.record_error(ResourceKind::Image, url, error);
      }
    }
  }

  fn record_invalid_image(&self, url: &str) {
    const INVALID_IMAGE_LIMIT: usize = 64;
    let Some(diag) = &self.diagnostics else {
      return;
    };
    let Ok(mut guard) = diag.lock() else {
      return;
    };
    if guard.invalid_images.len() >= INVALID_IMAGE_LIMIT {
      return;
    }
    if guard.invalid_images.iter().any(|u| u == url) {
      return;
    }
    guard.invalid_images.push(url.to_string());
  }

  fn enforce_image_policy(&self, url: &str) -> Result<()> {
    if let Some(ctx) = &self.resource_context {
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, url) {
        let blocked = Error::Image(ImageError::LoadFailed {
          url: url.to_string(),
          reason: err.reason,
        });
        if ctx.diagnostics.is_none() {
          self.record_image_error(url, &blocked);
        }
        return Err(blocked);
      }
    }

    Ok(())
  }

  fn enforce_image_cors(
    &self,
    requested_url: &str,
    resource: &FetchedResource,
    crossorigin: CrossOriginAttribute,
  ) -> Result<()> {
    if crossorigin == CrossOriginAttribute::None {
      return Ok(());
    }
    if !crate::resource::cors_enforcement_enabled() {
      return Ok(());
    }

    let document_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref());
    let Some(document_origin) = document_origin else {
      // Without a known document origin, avoid over-blocking.
      return Ok(());
    };
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    if let Err(reason) =
      crate::resource::validate_cors_allow_origin(
        resource,
        requested_url,
        Some(document_origin),
        credentials_mode,
      )
    {
      return Err(Error::Image(ImageError::LoadFailed {
        url: requested_url.to_string(),
        reason,
      }));
    }
    Ok(())
  }

  /// Enforce the active resource policy for subresources referenced within an SVG document.
  fn enforce_svg_resource_policy(&self, svg_content: &str, svg_url: &str) -> Result<()> {
    let Some(ctx) = &self.resource_context else {
      return Ok(());
    };

    // SVGs can reference external resources via multiple vectors:
    // - `href` / `xlink:href` attributes on elements like <image>, <use>, <feImage>, ...
    // - inline CSS: `style="... url(...) ..."`
    // - <style> blocks: `url(...)` and `@import`
    //
    // Keep scanning bounded so adversarial SVGs can't cause unbounded work or memory usage.
    const MAX_URL_REFERENCES: usize = 128;
    const MAX_CSS_BYTES_SCANNED: usize = 512 * 1024;

    let mut url_refs_seen = 0usize;
    let mut css_budget_remaining = MAX_CSS_BYTES_SCANNED;

    let mut check_url = |raw: &str, base: &str, kind: ResourceKind| -> Result<()> {
      let href = trim_ascii_whitespace(raw);
      if href.is_empty()
        || href.starts_with('#')
        || crate::resource::is_data_url(href)
        || is_about_url(href)
      {
        return Ok(());
      }

      url_refs_seen = url_refs_seen.saturating_add(1);
      if url_refs_seen > MAX_URL_REFERENCES {
        return Err(Error::Image(ImageError::LoadFailed {
          url: svg_url.to_string(),
          reason: format!(
            "SVG subresource scan exceeded the maximum of {MAX_URL_REFERENCES} URL references"
          ),
        }));
      }

      let resolved = resolve_against_base(base, href)
        .or_else(|| {
          ctx
            .document_url
            .as_deref()
            .and_then(|doc_url| resolve_against_base(doc_url, href))
        })
        .unwrap_or_else(|| href.to_string());
      if let Err(err) = ctx.check_allowed(kind, &resolved) {
        return Err(Error::Image(ImageError::LoadFailed {
          url: resolved,
          reason: format!("Blocked SVG subresource by policy: {}", err.reason),
        }));
      }
      Ok(())
    };

    fn contains_ascii_case_insensitive_url_open_paren(value: &str) -> bool {
      let bytes = value.as_bytes();
      if bytes.len() < 4 {
        return false;
      }
      let mut i = 0usize;
      while i + 3 < bytes.len() {
        let b0 = bytes[i];
        if b0 != b'u' && b0 != b'U' {
          i += 1;
          continue;
        }
        let b1 = bytes[i + 1];
        if b1 != b'r' && b1 != b'R' {
          i += 1;
          continue;
        }
        let b2 = bytes[i + 2];
        if b2 != b'l' && b2 != b'L' {
          i += 1;
          continue;
        }
        if bytes[i + 3] == b'(' {
          return true;
        }
        i += 1;
      }
      false
    }

    fn scan_css_urls<F: FnMut(&str, ResourceKind) -> Result<()>>(
      css: &str,
      include_imports: bool,
      budget_remaining: &mut usize,
      svg_url: &str,
      record: &mut F,
    ) -> Result<()> {
      use cssparser::{Parser, ParserInput, Token};

      if css.is_empty() {
        return Ok(());
      }

      // Ensure CSS scanning remains bounded. Subtract based on the bytes actually scanned so this
      // cap works across multiple style attributes/blocks.
      let css_len = css.len();
      if css_len > *budget_remaining {
        return Err(Error::Image(ImageError::LoadFailed {
          url: svg_url.to_string(),
          reason: format!(
            "SVG embedded CSS exceeded the scan budget of {MAX_CSS_BYTES_SCANNED} bytes"
          ),
        }));
      }
      *budget_remaining -= css_len;

      let mut input = ParserInput::new(css);
      let mut parser = Parser::new(&mut input);

      fn scan_parser<'i, 't, F: FnMut(&str, ResourceKind) -> Result<()>>(
        parser: &mut cssparser::Parser<'i, 't>,
        include_imports: bool,
        svg_url: &str,
        record: &mut F,
        depth: usize,
      ) -> Result<()> {
        // Avoid pathological recursion on deeply nested blocks.
        const MAX_DEPTH: usize = 32;
        if depth > MAX_DEPTH {
          return Err(Error::Image(ImageError::LoadFailed {
            url: svg_url.to_string(),
            reason: "SVG embedded CSS exceeded the maximum nested parse depth".to_string(),
          }));
        }

        while let Ok(token) = parser.next_including_whitespace_and_comments() {
          match token {
            Token::UnquotedUrl(url) => {
              record(url.as_ref(), ResourceKind::Image)?;
            }
            Token::Function(ref name) if name.eq_ignore_ascii_case("url") => {
              let mut nested_error: Option<Error> = None;
              let parsed = parser.parse_nested_block(|nested| {
                let mut url: Option<cssparser::CowRcStr<'i>> = None;

                while !nested.is_exhausted() {
                  match nested.next_including_whitespace_and_comments() {
                    Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                    Ok(Token::QuotedString(s))
                    | Ok(Token::UnquotedUrl(s))
                    | Ok(Token::Ident(s)) => {
                      url = Some(s.clone());
                      break;
                    }
                    Ok(Token::BadUrl(_)) => {
                      url = None;
                      break;
                    }
                    Ok(Token::Function(_))
                    | Ok(Token::ParenthesisBlock)
                    | Ok(Token::SquareBracketBlock)
                    | Ok(Token::CurlyBracketBlock) => {
                      // Ignore nested blocks when parsing the url() argument; only first token
                      // matters for our best-effort scan.
                      let _ =
                        nested.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'i, ()>>(()));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                  }
                }

                Ok::<_, cssparser::ParseError<'i, ()>>(url)
              });

              if let Some(err) = nested_error.take() {
                return Err(err);
              }

              if let Ok(Some(url)) = parsed {
                record(url.as_ref(), ResourceKind::Image)?;
              }
            }
            Token::AtKeyword(ref name)
              if include_imports && name.eq_ignore_ascii_case("import") =>
            {
              // `@import` accepts either a quoted string or a `url(...)` token.
              loop {
                let token = match parser.next_including_whitespace_and_comments() {
                  Ok(t) => t,
                  Err(_) => break,
                };
                match token {
                  Token::WhiteSpace(_) | Token::Comment(_) => continue,
                  Token::QuotedString(s) => {
                    record(s.as_ref(), ResourceKind::Stylesheet)?;
                    break;
                  }
                  Token::UnquotedUrl(s) => {
                    record(s.as_ref(), ResourceKind::Stylesheet)?;
                    break;
                  }
                  Token::Function(ref func) if func.eq_ignore_ascii_case("url") => {
                    let mut url: Option<cssparser::CowRcStr<'i>> = None;
                    let _ = parser.parse_nested_block(|nested| {
                      while !nested.is_exhausted() {
                        match nested.next_including_whitespace_and_comments() {
                          Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                          Ok(Token::QuotedString(s))
                          | Ok(Token::UnquotedUrl(s))
                          | Ok(Token::Ident(s)) => {
                            url = Some(s.clone());
                            break;
                          }
                          Ok(Token::BadUrl(_)) => break,
                          Ok(_) => {}
                          Err(_) => break,
                        }
                      }
                      Ok::<_, cssparser::ParseError<'i, ()>>(())
                    });
                    if let Some(url) = url {
                      record(url.as_ref(), ResourceKind::Stylesheet)?;
                    }
                    break;
                  }
                  _ => break,
                }
              }
            }
            Token::Function(_)
            | Token::ParenthesisBlock
            | Token::SquareBracketBlock
            | Token::CurlyBracketBlock => {
              let mut nested_error: Option<Error> = None;
              let _ = parser.parse_nested_block(|nested| {
                if let Err(err) = scan_parser(nested, include_imports, svg_url, record, depth + 1) {
                  nested_error = Some(err);
                  return Err(nested.new_custom_error(()));
                }
                Ok::<_, cssparser::ParseError<'i, ()>>(())
              });
              if let Some(err) = nested_error {
                return Err(err);
              }
            }
            _ => {}
          }
        }

        Ok(())
      }

      scan_parser(&mut parser, include_imports, svg_url, record, 0)
    }

    let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      roxmltree::Document::parse(svg_content)
    })) {
      Ok(Ok(doc)) => doc,
      Ok(Err(_)) | Err(_) => return Ok(()),
    };

    for node in doc.descendants() {
      if node.is_element() {
        let xml_base_chain = svg_xml_base_chain_for_node(node);
        let base = apply_svg_xml_base_chain(Some(svg_url), &xml_base_chain)
          .unwrap_or_else(|| svg_url.to_string());

        for attr in node.attributes() {
          let local_name = attr
            .name()
            .rsplit_once(':')
            .map(|(_, name)| name)
            .unwrap_or(attr.name());
          let value = attr.value();

          let tag_name = node.tag_name().name();
          // SVG uses `href` in a variety of places (e.g. `<a href="...">` hyperlinks). We only
          // enforce the image subresource policy for elements that can actually trigger resource
          // fetches in our pipeline.
          let is_image_href = local_name == "href"
            && (tag_name.eq_ignore_ascii_case("image")
              || tag_name.eq_ignore_ascii_case("use")
              || tag_name.eq_ignore_ascii_case("feimage"));
          let is_image_src =
            local_name == "src" && tag_name.eq_ignore_ascii_case("image");
          if is_image_href || is_image_src {
            check_url(value, &base, ResourceKind::Image)?;
            continue;
          }

          if local_name == "style" {
            scan_css_urls(
              value,
              false,
              &mut css_budget_remaining,
              svg_url,
              &mut |url, kind| check_url(url, &base, kind),
            )?;
            continue;
          }

          // Only scan other attributes when they plausibly contain `url(...)` references. Some SVG
          // attributes (notably <path d="...">) can be extremely large but are not CSS, and
          // scanning them unconditionally can exhaust our embedded-CSS scan budget.
          if !contains_ascii_case_insensitive_url_open_paren(value) {
            continue;
          }

          scan_css_urls(
            value,
            false,
            &mut css_budget_remaining,
            svg_url,
            &mut |url, kind| check_url(url, &base, kind),
          )?;
        }

        if node.tag_name().name() == "style" {
          // roxmltree normalizes CDATA sections into text nodes, so scanning text nodes covers both.
          for child in node.children() {
            if child.is_text() {
              if let Some(text) = child.text() {
                scan_css_urls(text, true, &mut css_budget_remaining, svg_url, &mut |url, kind| {
                  check_url(url, &base, kind)
                })?;
              }
            }
          }
        }
      }
    }

    Ok(())
  }

  fn fetch_and_decode(
    &self,
    cache_key: &str,
    resolved_url: &str,
    destination: FetchDestination,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImage>> {
    let threshold_ms = image_profile_threshold_ms();
    let profile_enabled = threshold_ms.is_some();
    let total_start = profile_enabled.then(Instant::now);
    let fetch_start = profile_enabled.then(Instant::now);

    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    let origin_fallback = referrer_url.and_then(origin_from_url);
    let client_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .or(origin_fallback.as_ref());
    let fetch_url_no_fragment = strip_url_fragment(resolved_url);
    let mut request =
      FetchRequest::new(fetch_url_no_fragment.as_ref(), destination).with_credentials_mode(credentials_mode);
    if let Some(origin) = client_origin {
      request = request.with_client_origin(origin);
    }
    if let Some(referrer_url) = referrer_url {
      request = request.with_referrer_url(referrer_url);
    }
    request = request.with_referrer_policy(request_referrer_policy);
    let resource = match self.fetcher.fetch_with_request(request) {
      Ok(res) => res,
      Err(err) => {
        if is_empty_body_error_for_image(&err) {
          return Ok(self.cache_placeholder_image(cache_key));
        }
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    // Offline fixtures (and some tracking pixel endpoints) substitute missing/empty image bodies
    // with a deterministic 1×1 transparent PNG so layout/paint can proceed. Treat that specific
    // payload as the same placeholder image used for non-fetchable `about:` URLs so callers can
    // detect it (e.g. for replaced-content fallbacks).
    if resource.bytes.as_slice() == crate::resource::offline_placeholder_png_bytes() {
      return Ok(self.cache_placeholder_image(cache_key));
    }
    if let Some(ctx) = &self.resource_context {
      let policy_url = resource.final_url.as_deref().unwrap_or(resolved_url);
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, policy_url) {
        let blocked = Error::Image(ImageError::LoadFailed {
          url: resolved_url.to_string(),
          reason: err.reason,
        });
        self.record_image_error(resolved_url, &blocked);
        return Err(blocked);
      }
    }
    if resource.bytes.is_empty() {
      // Treat empty bodies the same as `about:` URL placeholders: callers (notably the painters)
      // rely on `ImageCache::is_placeholder_image` to detect missing images and render UA fallback
      // UI for `<img>` elements.
      return Ok(self.cache_placeholder_image(cache_key));
    }
    if should_substitute_markup_payload_for_image(
      resolved_url,
      resource.final_url.as_deref(),
      resource.status,
      &resource.bytes,
    ) {
      self.record_invalid_image(resolved_url);
      return Ok(self.cache_placeholder_image(cache_key));
    }
    if let Err(err) = ensure_http_success(&resource, resolved_url)
      .and_then(|()| ensure_image_mime_sane(&resource, resolved_url))
    {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    if let Err(err) = self.enforce_image_cors(resolved_url, &resource, crossorigin) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    let fetch_ms = fetch_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
    let decode_timer = Instant::now();
    let decode_start = profile_enabled.then_some(decode_timer);
    let (img, orientation, resolution, is_vector, intrinsic_ratio, aspect_ratio_none, svg_content) =
      match {
        // Image decoding can be triggered deep inside nested deadline budgets (e.g. display-list
        // builder time slices). Those scoped budgets are meant to bound renderer algorithm choice
        // (e.g. fall back to legacy paint) but should not cause subresource decoding to be dropped
        // when the overall render deadline still has time remaining.
        let deadline = render_control::root_deadline();
        render_control::with_deadline(deadline.as_ref(), || {
          self.decode_resource(&resource, resolved_url)
        })
      } {
        Ok(decoded) => decoded,
        Err(err) => {
          self.record_image_error(resolved_url, &err);
          return Err(err);
        }
      };
    let decode_ms_value = decode_timer.elapsed().as_secs_f64() * 1000.0;
    let decode_ms = decode_start.map(|_| decode_ms_value);
    record_image_decode_ms(decode_ms_value);

    let img_arc = Arc::new(CachedImage {
      image: Arc::new(img),
      orientation,
      resolution,
      is_vector,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content,
    });

    self.insert_cached_image(cache_key, Arc::clone(&img_arc));

    if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
      let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
      if total_ms >= threshold_ms {
        let content_type = resource
          .content_type
          .as_deref()
          .unwrap_or("<unknown>")
          .split(';')
          .next()
          .unwrap_or("<unknown>");
        eprintln!(
          "image_profile kind=decode total_ms={total_ms:.2} fetch_ms={:.2} decode_ms={:.2} bytes={} dims={}x{} vector={} url={}",
          fetch_ms.unwrap_or(0.0),
          decode_ms.unwrap_or(0.0),
          resource.bytes.len(),
          img_arc.image.width(),
          img_arc.image.height(),
          img_arc.is_vector,
          resolved_url
        );
        eprintln!(" image_profile content_type={content_type}");
      }
    }

    Ok(img_arc)
  }

  fn decode_resource_into_cache(
    &self,
    cache_key: &str,
    resolved_url: &str,
    resource: &FetchedResource,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImage>> {
    if resource.bytes.is_empty() {
      return Ok(self.cache_placeholder_image(cache_key));
    }
    if should_substitute_markup_payload_for_image(
      resolved_url,
      resource.final_url.as_deref(),
      resource.status,
      &resource.bytes,
    ) {
      self.record_invalid_image(resolved_url);
      return Ok(self.cache_placeholder_image(cache_key));
    }
    if let Err(err) = self.enforce_image_cors(resolved_url, resource, crossorigin) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    let threshold_ms = image_profile_threshold_ms();
    let profile_enabled = threshold_ms.is_some();
    let total_start = profile_enabled.then(Instant::now);
    let decode_timer = Instant::now();
    let decode_start = profile_enabled.then_some(decode_timer);
    let (img, orientation, resolution, is_vector, intrinsic_ratio, aspect_ratio_none, svg_content) =
      {
        // See `fetch_and_decode` for why we temporarily switch to the root deadline for decoding.
        let deadline = render_control::root_deadline();
        render_control::with_deadline(deadline.as_ref(), || {
          self.decode_resource(resource, resolved_url)
        })
      }?;
    let decode_ms_value = decode_timer.elapsed().as_secs_f64() * 1000.0;
    let decode_ms = decode_start.map(|_| decode_ms_value);
    record_image_decode_ms(decode_ms_value);

    let img_arc = Arc::new(CachedImage {
      image: Arc::new(img),
      orientation,
      resolution,
      is_vector,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content,
    });

    self.insert_cached_image(cache_key, Arc::clone(&img_arc));

    if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
      let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
      if total_ms >= threshold_ms {
        let content_type = resource
          .content_type
          .as_deref()
          .unwrap_or("<unknown>")
          .split(';')
          .next()
          .unwrap_or("<unknown>");
        eprintln!(
          "image_profile kind=decode total_ms={total_ms:.2} fetch_ms=0.00 decode_ms={:.2} bytes={} dims={}x{} vector={} url={}",
          decode_ms.unwrap_or(0.0),
          resource.bytes.len(),
          img_arc.image.width(),
          img_arc.image.height(),
          img_arc.is_vector,
          resolved_url
        );
        eprintln!(" image_profile content_type={content_type}");
      }
    }

    Ok(img_arc)
  }

  fn fetch_and_probe(
    &self,
    cache_key: &str,
    resolved_url: &str,
    destination: FetchDestination,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    let threshold_ms = image_profile_threshold_ms();
    let profile_enabled = threshold_ms.is_some();
    let total_start = profile_enabled.then(Instant::now);
    let fetch_start = profile_enabled.then(Instant::now);
    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    let origin_fallback = referrer_url.and_then(origin_from_url);
    let client_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .or(origin_fallback.as_ref());
    let fetch_url_no_fragment = strip_url_fragment(resolved_url);
    let mut request =
      FetchRequest::new(fetch_url_no_fragment.as_ref(), destination).with_credentials_mode(credentials_mode);
    if let Some(origin) = client_origin {
      request = request.with_client_origin(origin);
    }
    if let Some(referrer_url) = referrer_url {
      request = request.with_referrer_url(referrer_url);
    }
    request = request.with_referrer_policy(request_referrer_policy);

    let check_resource_allowed = |resource: &FetchedResource| -> Result<()> {
      if let Some(ctx) = &self.resource_context {
        let policy_url = resource.final_url.as_deref().unwrap_or(resolved_url);
        if let Err(err) = ctx.check_allowed(ResourceKind::Image, policy_url) {
          return Err(Error::Image(ImageError::LoadFailed {
            url: resolved_url.to_string(),
            reason: err.reason,
          }));
        }
      }
      ensure_http_success(resource, resolved_url)?;
      self.enforce_image_cors(resolved_url, resource, crossorigin)
    };

    let probe_limit = image_probe_max_bytes();
    let retry_limit = probe_limit
      .saturating_mul(8)
      .max(512 * 1024)
      .clamp(1, 64 * 1024 * 1024);

    const RAW_RESOURCE_CACHE_LIMIT_BYTES: usize = 5 * 1024 * 1024;

    for (idx, limit) in [probe_limit, retry_limit].into_iter().enumerate() {
      let resource = match self
        .fetcher
        .fetch_partial_with_request(request.clone(), limit)
      {
        Ok(res) => res,
        Err(err) => {
          if is_empty_body_error_for_image(&err) {
            return Ok(self.cache_placeholder_metadata(cache_key));
          }
          let _ = err;
          break;
        }
      };
      // Some servers reject `Range` requests for images (or bot-mitigation paths) with status codes
      // like 405/416 even though a full GET without a `Range` header would succeed. In that case,
      // skip reporting a fetch error from the probe and fall back to a full fetch.
      if matches!(resource.status, Some(405 | 416)) {
        record_probe_partial_fetch(resource.bytes.len());
        break;
      }
      record_probe_partial_fetch(resource.bytes.len());
      let resource = Arc::new(resource);

      if let Err(err) = check_resource_allowed(resource.as_ref()) {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
      if should_substitute_markup_payload_for_image(
        resolved_url,
        resource.final_url.as_deref(),
        resource.status,
        &resource.bytes,
      ) {
        self.record_invalid_image(resolved_url);
        return Ok(self.cache_placeholder_metadata(cache_key));
      }
      if let Err(err) = ensure_image_mime_sane(resource.as_ref(), resolved_url) {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }

      let fetch_ms = fetch_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
      let attempt_probe_start = profile_enabled.then(Instant::now);
      match self.probe_resource(&resource, resolved_url) {
        Ok(meta) => {
          let probe_ms = attempt_probe_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
          let meta = Arc::new(meta);

          if let Ok(mut cache) = self.meta_cache.lock() {
            cache.insert(cache_key.to_string(), Arc::clone(&meta));
          }
          if let Some(serialized) = encode_probe_metadata_for_disk(&meta) {
            self.fetcher.write_cache_artifact_with_request(
              request,
              CacheArtifactKind::ImageProbeMetadata,
              &serialized,
              Some(resource.as_ref()),
            );
          }

          // When the image is small enough to fit in the probe prefix, keep the bytes so a later
          // decode can reuse them without issuing another HTTP request.
          if resource.bytes.len() < limit && resource.bytes.len() <= RAW_RESOURCE_CACHE_LIMIT_BYTES
          {
            if let Ok(mut cache) = self.raw_cache.lock() {
              cache.insert(cache_key.to_string(), Arc::clone(&resource));
            }
          }

          if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
            let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
            if total_ms >= threshold_ms {
              let content_type = resource
                .content_type
                .as_deref()
                .unwrap_or("<unknown>")
                .split(';')
                .next()
                .unwrap_or("<unknown>");
              eprintln!(
                "image_profile kind=probe total_ms={total_ms:.2} fetch_ms={:.2} probe_ms={:.2} bytes={} dims={}x{} vector={} url={}",
                fetch_ms.unwrap_or(0.0),
                probe_ms.unwrap_or(0.0),
                resource.bytes.len(),
                meta.width,
                meta.height,
                meta.is_vector,
                resolved_url
              );
              eprintln!(" image_profile content_type={content_type}");
            }
          }

          return Ok(meta);
        }
        Err(err) => {
          // Only retry/fallback when it looks like the prefix may have been truncated.
          if resource.bytes.len() < limit {
            self.record_image_error(resolved_url, &err);
            return Err(err);
          }
          let _ = err;
          // Retry once with a larger prefix, then fall back to a full fetch.
          if idx == 0 {
            continue;
          }
          break;
        }
      }
    }

    if probe_limit > 0 {
      record_probe_partial_fallback_full();
    }

    let resource = match self.fetcher.fetch_with_request(request.clone()) {
      Ok(res) => res,
      Err(err) => {
        if is_empty_body_error_for_image(&err) {
          return Ok(self.cache_placeholder_metadata(cache_key));
        }
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    let resource = Arc::new(resource);
    if let Err(err) = check_resource_allowed(resource.as_ref()) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    if should_substitute_markup_payload_for_image(
      resolved_url,
      resource.final_url.as_deref(),
      resource.status,
      &resource.bytes,
    ) {
      self.record_invalid_image(resolved_url);
      return Ok(self.cache_placeholder_metadata(cache_key));
    }
    if let Err(err) = ensure_image_mime_sane(resource.as_ref(), resolved_url) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    let fetch_ms = fetch_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
    let probe_start = profile_enabled.then(Instant::now);
    let meta = match self.probe_resource(&resource, resolved_url) {
      Ok(meta) => meta,
      Err(err) => {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    let probe_ms = probe_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
    let meta = Arc::new(meta);

    if let Ok(mut cache) = self.meta_cache.lock() {
      cache.insert(cache_key.to_string(), Arc::clone(&meta));
    }
    if let Some(serialized) = encode_probe_metadata_for_disk(&meta) {
      self.fetcher.write_cache_artifact_with_request(
        request,
        CacheArtifactKind::ImageProbeMetadata,
        &serialized,
        Some(resource.as_ref()),
      );
    }

    if resource.bytes.len() <= RAW_RESOURCE_CACHE_LIMIT_BYTES {
      if let Ok(mut cache) = self.raw_cache.lock() {
        cache.insert(cache_key.to_string(), Arc::clone(&resource));
      }
    }

    if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
      let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
      if total_ms >= threshold_ms {
        let content_type = resource
          .content_type
          .as_deref()
          .unwrap_or("<unknown>")
          .split(';')
          .next()
          .unwrap_or("<unknown>");
        eprintln!(
          "image_profile kind=probe total_ms={total_ms:.2} fetch_ms={:.2} probe_ms={:.2} bytes={} dims={}x{} vector={} url={}",
          fetch_ms.unwrap_or(0.0),
          probe_ms.unwrap_or(0.0),
          resource.bytes.len(),
          meta.width,
          meta.height,
          meta.is_vector,
          resolved_url
        );
        eprintln!(" image_profile content_type={content_type}");
      }
    }

    Ok(meta)
  }

  fn join_inflight(&self, resolved_url: &str) -> (Arc<DecodeInFlight>, bool) {
    let mut map = match self.in_flight.lock() {
      Ok(map) => map,
      Err(poisoned) => {
        let mut map = poisoned.into_inner();
        map.clear();
        map
      }
    };
    if let Some(existing) = map.get(resolved_url) {
      return (Arc::clone(existing), false);
    }

    let flight = Arc::new(DecodeInFlight::new());
    map.insert(resolved_url.to_string(), Arc::clone(&flight));
    (flight, true)
  }

  fn join_meta_inflight(&self, resolved_url: &str) -> (Arc<ProbeInFlight>, bool) {
    let mut map = match self.meta_in_flight.lock() {
      Ok(map) => map,
      Err(poisoned) => {
        let mut map = poisoned.into_inner();
        map.clear();
        map
      }
    };
    if let Some(existing) = map.get(resolved_url) {
      return (Arc::clone(existing), false);
    }

    let flight = Arc::new(ProbeInFlight::new());
    map.insert(resolved_url.to_string(), Arc::clone(&flight));
    (flight, true)
  }

  fn finish_inflight(
    &self,
    resolved_url: &str,
    flight: &Arc<DecodeInFlight>,
    result: SharedImageResult,
  ) {
    flight.set(result);
    let mut map = self
      .in_flight
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(resolved_url);
  }

  fn finish_meta_inflight(
    &self,
    resolved_url: &str,
    flight: &Arc<ProbeInFlight>,
    result: SharedMetaResult,
  ) {
    flight.set(result);
    let mut map = self
      .meta_in_flight
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(resolved_url);
  }

  /// Render raw SVG content to an image, caching by content hash.
  pub fn render_svg(&self, svg_content: &str) -> Result<Arc<CachedImage>> {
    record_image_cache_request();
    let cache_key = inline_svg_cache_key(svg_content);
    if let Some(image) = self.get_cached(&cache_key) {
      self.enforce_svg_resource_policy(svg_content, "inline-svg")?;
      record_image_cache_hit();
      return Ok(image);
    }
    record_image_cache_miss();
    let decode_timer = Instant::now();
    let (img, intrinsic_ratio, aspect_ratio_none) = self.render_svg_to_image(svg_content)?;
    let svg_content = Arc::<str>::from(svg_content);
    record_image_decode_ms(decode_timer.elapsed().as_secs_f64() * 1000.0);
    let cached = Arc::new(CachedImage {
      image: Arc::new(img),
      orientation: None,
      resolution: None,
      is_vector: true,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content: Some(svg_content),
    });
    self.insert_cached_image(&cache_key, Arc::clone(&cached));
    Ok(cached)
  }

  pub fn render_svg_pixmap_at_size(
    &self,
    svg_content: &str,
    render_width: u32,
    render_height: u32,
    url: &str,
    device_pixel_ratio: f32,
  ) -> Result<Arc<tiny_skia::Pixmap>> {
    self.enforce_svg_resource_policy(svg_content, url)?;
    self.enforce_decode_limits(render_width, render_height, url)?;
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let key = svg_pixmap_key(
      svg_content,
      url,
      device_pixel_ratio,
      render_width,
      render_height,
    );
    record_image_cache_request();
    if let Ok(mut cache) = self.svg_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_image_cache_hit();
        return Ok(cached);
      }
    }

    record_image_cache_miss();
    self.render_svg_pixmap_at_size_uncached(svg_content, key, render_width, render_height, url)
  }

  pub fn render_svg_pixmap_at_size_with_injected_style(
    &self,
    svg_content: &str,
    insert_pos: usize,
    style_element: &str,
    render_width: u32,
    render_height: u32,
    url: &str,
    device_pixel_ratio: f32,
  ) -> Result<Arc<tiny_skia::Pixmap>> {
    // Policy enforcement is based on SVG markup. Injected CSS can introduce additional `url(...)`
    // references (filters/images/fonts), so scan both the original SVG and the injected `<style>`
    // element.
    self.enforce_svg_resource_policy(svg_content, url)?;
    self.enforce_svg_resource_policy(style_element, url)?;
    self.enforce_decode_limits(render_width, render_height, url)?;
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let Some(prefix) = svg_content.get(..insert_pos) else {
      return self.render_svg_pixmap_at_size(
        svg_content,
        render_width,
        render_height,
        url,
        device_pixel_ratio,
      );
    };
    let Some(suffix) = svg_content.get(insert_pos..) else {
      return self.render_svg_pixmap_at_size(
        svg_content,
        render_width,
        render_height,
        url,
        device_pixel_ratio,
      );
    };

    // Match `svg_pixmap_key` hashing semantics for a single combined string without allocating it.
    // `Hash` for `str` appends a 0xFF terminator byte, so hashing chunks separately would
    // introduce extra terminators and change the key. We instead hash the raw bytes and append
    // the terminator once.
    let mut content_hasher = DefaultHasher::new();
    content_hasher.write(prefix.as_bytes());
    content_hasher.write(style_element.as_bytes());
    content_hasher.write(suffix.as_bytes());
    content_hasher.write_u8(0xff);
    let mut url_hasher = DefaultHasher::new();
    url.hash(&mut url_hasher);
    let key = SvgPixmapKey {
      hash: content_hasher.finish(),
      url_hash: url_hasher.finish(),
      len: prefix.len() + style_element.len() + suffix.len(),
      width: render_width,
      height: render_height,
      device_pixel_ratio_bits: f32_to_canonical_bits(device_pixel_ratio),
    };

    record_image_cache_request();
    if let Ok(mut cache) = self.svg_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_image_cache_hit();
        return Ok(cached);
      }
    }

    record_image_cache_miss();

    let mut combined = String::with_capacity(key.len);
    combined.push_str(prefix);
    combined.push_str(style_element);
    combined.push_str(suffix);

    self.render_svg_pixmap_at_size_uncached(&combined, key, render_width, render_height, url)
  }

  fn render_svg_pixmap_at_size_uncached(
    &self,
    svg_content: &str,
    key: SvgPixmapKey,
    render_width: u32,
    render_height: u32,
    url: &str,
  ) -> Result<Arc<tiny_skia::Pixmap>> {
    use resvg::usvg;

    let render_timer = Instant::now();

    let url_no_fragment = strip_url_fragment(url);

    let svg_use_inlined = inline_svg_use_references(
      svg_content,
      url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
    )?;
    let svg_images_inlined = inline_svg_image_references(
      svg_use_inlined.as_ref(),
      url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
    )?;
    let svg_fragment_applied = apply_svg_url_fragment(svg_images_inlined.as_ref(), url);
    let svg_content = svg_fragment_applied.as_ref();
    if let Some(pixmap) = try_render_simple_svg_pixmap(svg_content, render_width, render_height)? {
      let pixmap = Arc::new(pixmap);
      record_image_decode_ms(render_timer.elapsed().as_secs_f64() * 1000.0);
      self.insert_svg_pixmap(key, Arc::clone(&pixmap));
      return Ok(pixmap);
    }

    let options = usvg_options_for_url(url);
    let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      usvg::Tree::from_str(svg_content, &options)
    })) {
      Ok(Ok(tree)) => tree,
      Ok(Err(e)) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("Failed to parse SVG: {}", e),
        }));
      }
      Err(panic) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVG parse panicked: {}", panic_payload_to_reason(&*panic)),
        }));
      }
    };

    let size = tree.size();
    let source_width = size.width();
    let source_height = size.height();
    if source_width <= 0.0 || source_height <= 0.0 {
      return Err(Error::Render(RenderError::CanvasCreationFailed {
        width: source_width as u32,
        height: source_height as u32,
      }));
    }

    let Some(mut pixmap) = new_pixmap(render_width, render_height) else {
      return Err(Error::Render(RenderError::CanvasCreationFailed {
        width: render_width,
        height: render_height,
      }));
    };

    let transform = match svg_view_box_root_transform(
      svg_content,
      source_width,
      source_height,
      render_width as f32,
      render_height as f32,
    ) {
      Some(transform) => transform,
      None => {
        let scale_x = render_width as f32 / source_width;
        let scale_y = render_height as f32 / source_height;
        tiny_skia::Transform::from_scale(scale_x, scale_y)
      }
    };
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      resvg::render(&tree, transform, &mut pixmap.as_mut());
    })) {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("SVG render panicked: {}", panic_payload_to_reason(&*panic)),
      }));
    }
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let pixmap = Arc::new(pixmap);
    record_image_decode_ms(render_timer.elapsed().as_secs_f64() * 1000.0);
    self.insert_svg_pixmap(key, Arc::clone(&pixmap));

    Ok(pixmap)
  }

  /// Probe intrinsic SVG metadata (dimensions/aspect ratio) from raw markup without rasterizing.
  pub fn probe_svg_content(
    &self,
    svg_content: &str,
    url_hint: &str,
  ) -> Result<CachedImageMetadata> {
    self.enforce_svg_resource_policy(svg_content, url_hint)?;

    let svg_with_fragment = apply_svg_url_fragment(svg_content, url_hint);
    let svg_content = svg_with_fragment.as_ref();

    let (meta_width, meta_height, meta_ratio, aspect_ratio_none) =
      svg_intrinsic_metadata(svg_content, 16.0, 16.0).unwrap_or((None, None, None, false));

    let ratio = meta_ratio.filter(|r| *r > 0.0);
    let (target_width, target_height) =
      svg_intrinsic_target_dimensions(meta_width, meta_height, ratio);

    let width = target_width.max(1.0).round() as u32;
    let height = target_height.max(1.0).round() as u32;
    self.enforce_decode_limits(width, height, url_hint)?;

    let intrinsic_ratio = if aspect_ratio_none {
      None
    } else {
      ratio.or_else(|| {
        if height > 0 {
          Some(width as f32 / height as f32)
        } else {
          None
        }
      })
    };

    Ok(CachedImageMetadata {
      width,
      height,
      orientation: None,
      resolution: None,
      is_vector: true,
      intrinsic_ratio,
      aspect_ratio_none,
    })
  }

  fn maybe_decompress_svgz(&self, bytes: &[u8], url: &str) -> Result<Option<Vec<u8>>> {
    if bytes.len() < 2 || bytes[0] != 0x1F || bytes[1] != 0x8B {
      return Ok(None);
    }

    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut deadline_counter = 0usize;

    loop {
      check_root_periodic(&mut deadline_counter, 32, RenderStage::Paint).map_err(Error::Render)?;
      let n = decoder.read(&mut buf).map_err(|e| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVGZ decompression failed: {e}"),
        })
      })?;
      if n == 0 {
        break;
      }
      if out.len().saturating_add(n) > MAX_SVGZ_DECOMPRESSED_BYTES {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVGZ decompressed payload exceeded {MAX_SVGZ_DECOMPRESSED_BYTES} bytes"),
        }));
      }
      out.extend_from_slice(&buf[..n]);
    }

    Ok(Some(out))
  }

  /// Decode a fetched resource into an image
  fn decode_resource(
    &self,
    resource: &FetchedResource,
    url: &str,
  ) -> Result<(
    DynamicImage,
    Option<OrientationTransform>,
    Option<f32>,
    bool,
    Option<f32>,
    bool,
    Option<Arc<str>>,
  )> {
    let bytes = &resource.bytes;
    let content_type = resource.content_type.as_deref();
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    if bytes.is_empty() {
      let img = RgbaImage::new(1, 1);
      return Ok((
        DynamicImage::ImageRgba8(img),
        None,
        None,
        false,
        None,
        false,
        None,
      ));
    }

    let url_hint =
      append_url_fragment_if_missing(resource.final_url.as_deref().unwrap_or(url), url);
    let url_hint_str = url_hint.as_ref();

    // Check if this is SVG (plain UTF-8 payload, or gzip-compressed `.svgz`).
    let mime_is_svg = content_type
      .map(|m| m.contains("image/svg"))
      .unwrap_or(false);
    let url_is_svgz = url_ends_with_svgz(url)
      || resource
        .final_url
        .as_deref()
        .is_some_and(url_ends_with_svgz);

    if let Ok(content) = std::str::from_utf8(bytes) {
      if mime_is_svg || svg_text_looks_like_markup(content) {
        let svg_content: Arc<str> = Arc::from(content);
        let (img, ratio, aspect_none) =
          self.render_svg_to_image_with_url(&svg_content, url_hint_str)?;
        return Ok((img, None, None, true, ratio, aspect_none, Some(svg_content)));
      }
    } else if url_is_svgz || mime_is_svg {
      if let Some(decompressed) = self.maybe_decompress_svgz(bytes, url)? {
        if let Ok(content) = std::str::from_utf8(&decompressed) {
          if mime_is_svg || svg_text_looks_like_markup(content) {
            let svg_content: Arc<str> = Arc::from(content);
            let (img, ratio, aspect_none) =
              self.render_svg_to_image_with_url(&svg_content, url_hint_str)?;
            return Ok((img, None, None, true, ratio, aspect_none, Some(svg_content)));
          }

          // Decompressed to UTF-8 but doesn't look like SVG markup; treat as a (possibly mislabelled)
          // bitmap payload.
          let (orientation, resolution) = Self::exif_metadata(&decompressed);
          return self
            .decode_bitmap(&decompressed, content_type, url)
            .map(|img| (img, orientation, resolution, false, None, false, None));
        }

        // Not valid UTF-8 after decompression; treat as a (possibly mislabelled) bitmap.
        let (orientation, resolution) = Self::exif_metadata(&decompressed);
        return self
          .decode_bitmap(&decompressed, content_type, url)
          .map(|img| (img, orientation, resolution, false, None, false, None));
      }
    }

    // Regular image - extract EXIF metadata and decode.
    let (orientation, resolution) = Self::exif_metadata(bytes);
    self
      .decode_bitmap(bytes, content_type, url)
      .map(|img| (img, orientation, resolution, false, None, false, None))
  }

  fn probe_resource(&self, resource: &FetchedResource, url: &str) -> Result<CachedImageMetadata> {
    let bytes = &resource.bytes;
    let content_type = resource.content_type.as_deref();
    if bytes.is_empty() {
      return Ok((*about_url_placeholder_metadata()).clone());
    }

    let url_hint =
      append_url_fragment_if_missing(resource.final_url.as_deref().unwrap_or(url), url);
    let url_hint_str = url_hint.as_ref();

    // SVG (including gzip-compressed `.svgz` responses).
    let mime_is_svg = content_type
      .map(|m| m.contains("image/svg"))
      .unwrap_or(false);
    let url_is_svgz = url_ends_with_svgz(url)
      || resource
        .final_url
        .as_deref()
        .is_some_and(url_ends_with_svgz);

    if let Ok(content) = std::str::from_utf8(bytes) {
      if mime_is_svg || svg_text_looks_like_markup(content) {
        return self.probe_svg_content(content, url_hint_str);
      }
    } else if url_is_svgz || mime_is_svg {
      if let Some(decompressed) = self.maybe_decompress_svgz(bytes, url)? {
        if let Ok(content) = std::str::from_utf8(&decompressed) {
          if mime_is_svg || svg_text_looks_like_markup(content) {
            return self.probe_svg_content(content, url_hint_str);
          }
        }

        // Not UTF-8 (or not SVG markup) after decompression; treat as a bitmap probe on the
        // decompressed bytes.
        let bytes = decompressed;
        let (orientation, resolution) = Self::exif_metadata(&bytes);
        let format_from_content_type = Self::format_from_content_type(content_type);
        let (sniffed_format, sniff_panic) = Self::sniff_image_format(&bytes);
        let (dims, dims_panic) =
          self.predecoded_dimensions(&bytes, format_from_content_type, sniffed_format);
        let (width, height) = dims.ok_or_else(|| {
          let reason = dims_panic
            .or(sniff_panic)
            .map(|panic| format!("Image probe panicked: {panic}"))
            .unwrap_or_else(|| "Unable to determine image dimensions".to_string());
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason,
          })
        })?;
        self.enforce_decode_limits(width, height, url)?;

        return Ok(CachedImageMetadata {
          width,
          height,
          orientation,
          resolution,
          is_vector: false,
          intrinsic_ratio: None,
          aspect_ratio_none: false,
        });
      }
    }

    let (orientation, resolution) = Self::exif_metadata(bytes);
    let format_from_content_type = Self::format_from_content_type(content_type);
    let (sniffed_format, sniff_panic) = Self::sniff_image_format(bytes);
    let (dims, dims_panic) =
      self.predecoded_dimensions(bytes, format_from_content_type, sniffed_format);
    let (width, height) = dims.ok_or_else(|| {
      let reason = dims_panic
        .or(sniff_panic)
        .map(|panic| format!("Image probe panicked: {panic}"))
        .unwrap_or_else(|| "Unable to determine image dimensions".to_string());
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason,
      })
    })?;
    self.enforce_decode_limits(width, height, url)?;

    Ok(CachedImageMetadata {
      width,
      height,
      orientation,
      resolution,
      is_vector: false,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
    })
  }

  fn decode_bitmap(
    &self,
    bytes: &[u8],
    content_type: Option<&str>,
    url: &str,
  ) -> Result<DynamicImage> {
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    let format_from_content_type = Self::format_from_content_type(content_type);
    let (sniffed_format, sniff_panic) = Self::sniff_image_format(bytes);
    let (pre_dims, dims_panic) =
      self.predecoded_dimensions(bytes, format_from_content_type, sniffed_format);
    let panic_error = dims_panic.or(sniff_panic).map(|panic| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("Image decode panicked: {panic}"),
      })
    });
    if let Some((width, height)) = pre_dims {
      self.enforce_decode_limits(width, height, url)?;
    }
    let mut last_error: Option<Error> = None;

    if matches!(format_from_content_type, Some(ImageFormat::Avif))
      || matches!(sniffed_format, Some(ImageFormat::Avif))
    {
      match Self::decode_avif(bytes) {
        Ok(img) => return self.finish_bitmap_decode(img, url),
        Err(AvifDecodeError::Timeout(err)) => return Err(Error::Render(err)),
        Err(AvifDecodeError::Image(err)) => last_error = Some(self.decode_error(url, err)),
      }
    }

    if let Some(format) = format_from_content_type {
      if format != ImageFormat::Avif {
        check_root(RenderStage::Paint).map_err(Error::Render)?;
        match self.decode_with_format(bytes, format, url) {
          Ok(img) => return self.finish_bitmap_decode(img, url),
          Err(err) => {
            if let Error::Render(_) = err {
              return Err(err);
            }
            last_error = Some(err);
          }
        }
      }
    }

    if let Some(format) = sniffed_format {
      if Some(format) != format_from_content_type && format != ImageFormat::Avif {
        check_root(RenderStage::Paint).map_err(Error::Render)?;
        match self.decode_with_format(bytes, format, url) {
          Ok(img) => return self.finish_bitmap_decode(img, url),
          Err(err) => {
            if let Error::Render(_) = err {
              return Err(err);
            }
            last_error = Some(err);
          }
        }
      }
    }

    check_root(RenderStage::Paint).map_err(Error::Render)?;
    match self.decode_with_guess(bytes, url) {
      Ok(img) => self.finish_bitmap_decode(img, url),
      Err(err) => Err(match err {
        Error::Render(_) => err,
        _ => panic_error.or(last_error).unwrap_or(err),
      }),
    }
  }

  fn finish_bitmap_decode(&self, img: DynamicImage, url: &str) -> Result<DynamicImage> {
    self.enforce_decode_limits(img.width(), img.height(), url)?;
    Ok(img)
  }

  fn format_from_content_type(content_type: Option<&str>) -> Option<ImageFormat> {
    let mime = content_type?.split(';').next().map(|ct| {
      ct.trim_matches(|c: char| matches!(c, ' ' | '\t'))
        .to_ascii_lowercase()
    })?;
    ImageFormat::from_mime_type(mime)
  }

  fn looks_like_avif(bytes: &[u8]) -> bool {
    // AVIF is an ISO-BMFF container. We only need a lightweight sniff (ftyp box contains the brand)
    // rather than invoking a full parser (which can panic in debug builds for malformed/trailing
    // data).
    if bytes.len() < 12 {
      return false;
    }

    let size = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
    if &bytes[4..8] != b"ftyp" {
      return false;
    }

    let (header_len, box_size) = match size {
      0 => (8usize, bytes.len()),
      1 => {
        if bytes.len() < 16 {
          return false;
        }
        let ext = u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
        let Ok(ext) = usize::try_from(ext) else {
          return false;
        };
        (16usize, ext)
      }
      n => (8usize, n as usize),
    };

    if box_size < header_len + 8 {
      return false;
    }

    let avail = bytes.len().min(box_size);
    if avail < header_len + 8 {
      return false;
    }

    let payload = &bytes[header_len..avail];
    let major_brand = &payload[0..4];
    if major_brand == b"avif" || major_brand == b"avis" {
      return true;
    }

    // Skip minor_version (4 bytes) and scan compatible brands.
    if payload.len() <= 8 {
      return false;
    }
    payload[8..]
      .chunks_exact(4)
      .any(|brand| brand == b"avif" || brand == b"avis")
  }

  fn sniff_image_format_fast(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
      return Some(ImageFormat::Png);
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
      return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
      return Some(ImageFormat::Gif);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
      return Some(ImageFormat::WebP);
    }
    if Self::looks_like_avif(bytes) {
      return Some(ImageFormat::Avif);
    }
    None
  }

  fn sniff_image_format(bytes: &[u8]) -> (Option<ImageFormat>, Option<String>) {
    if let Some(format) = Self::sniff_image_format_fast(bytes) {
      return (Some(format), None);
    }
    // `image::guess_format` may invoke codec sniffers that use debug assertions (notably for AVIF).
    // Guard against panics so metadata probing can't crash the renderer in debug builds. Preserve
    // the panic message so callers can surface it in decode/probe errors.
    match std::panic::catch_unwind(|| image::guess_format(bytes).ok()) {
      Ok(format) => (format, None),
      Err(panic) => (
        None,
        Some(format!(
          "image::guess_format panicked: {}",
          panic_payload_to_reason(&*panic)
        )),
      ),
    }
  }

  fn decode_with_format(
    &self,
    bytes: &[u8],
    format: ImageFormat,
    url: &str,
  ) -> Result<DynamicImage> {
    if format == ImageFormat::Png {
      // Decode PNG via the `png` crate so output buffers are allocated fallibly (avoiding process
      // aborts on OOM inside `image`'s decoders). Keep render deadlines responsive by reading
      // through `DeadlineCursor`.
      fn map_png_io_error(url: &str, io_err: &io::Error) -> Error {
        if let Some(render_err) = io_err
          .get_ref()
          .and_then(|source| source.downcast_ref::<RenderError>())
        {
          return Error::Render(render_err.clone());
        }

        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: io_err.to_string(),
        })
      }

      fn map_png_error(url: &str, err: &png::DecodingError) -> Error {
        match err {
          png::DecodingError::IoError(io_err) => map_png_io_error(url, io_err),
          _ => Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: err.to_string(),
          }),
        }
      }

      let mut decoder = png::Decoder::new(DeadlineCursor::new(bytes));
      decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
      let mut reader = decoder.read_info().map_err(|e| map_png_error(url, &e))?;
      let info = reader.info();
      let width = info.width;
      let height = info.height;
      if width == 0 || height == 0 {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG image size is zero ({width}x{height})"),
        }));
      }

      let rgba_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!("PNG dimensions overflow ({width}x{height})"),
          })
        })?;
      if rgba_bytes > MAX_PIXMAP_BYTES {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!(
            "PNG decoded image {width}x{height} is {rgba_bytes} bytes (limit {MAX_PIXMAP_BYTES})"
          ),
        }));
      }
      let rgba_len = usize::try_from(rgba_bytes).map_err(|_| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG decoded byte size does not fit in usize ({width}x{height})"),
        })
      })?;

      let out_size = reader.output_buffer_size().ok_or_else(|| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "PNG output buffer size not available".to_string(),
        })
      })?;
      let out_size_u64 = u64::try_from(out_size).map_err(|_| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "PNG output buffer size does not fit in u64".to_string(),
        })
      })?;
      if out_size_u64 > MAX_PIXMAP_BYTES {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG output buffer is {out_size_u64} bytes (limit {MAX_PIXMAP_BYTES})"),
        }));
      }
      let out_size = usize::try_from(out_size_u64).map_err(|_| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "PNG output buffer size does not fit in usize".to_string(),
        })
      })?;

      let mut buf = Vec::new();
      buf.try_reserve_exact(out_size).map_err(|err| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG output buffer allocation failed for {out_size} bytes: {err}"),
        })
      })?;
      buf.resize(out_size, 0);

      let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| map_png_error(url, &e))?;
      buf.truncate(frame.buffer_size());

      let rgba = match (frame.color_type, frame.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => {
          if buf.len() != rgba_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG RGBA output length mismatch (expected {rgba_len} bytes, got {})",
                buf.len()
              ),
            }));
          }
          RgbaImage::from_raw(width, height, buf).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG RGBA buffer was invalid".to_string(),
            })
          })?
        }
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
          let rgb_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|px| px.checked_mul(3))
            .ok_or_else(|| {
              Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "PNG RGB byte size overflow".to_string(),
              })
            })?;
          let rgb_len = usize::try_from(rgb_len).map_err(|_| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG RGB byte size does not fit in usize".to_string(),
            })
          })?;
          if buf.len() != rgb_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG RGB output length mismatch (expected {rgb_len} bytes, got {})",
                buf.len()
              ),
            }));
          }

          let mut out = Vec::new();
          out.try_reserve_exact(rgba_len).map_err(|err| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!("PNG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
            })
          })?;
          out.resize(rgba_len, 0);
          for (in_px, out_px) in buf.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
            out_px[0] = in_px[0];
            out_px[1] = in_px[1];
            out_px[2] = in_px[2];
            out_px[3] = 255;
          }
          RgbaImage::from_raw(width, height, out).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG RGB->RGBA buffer was invalid".to_string(),
            })
          })?
        }
        (png::ColorType::Grayscale, png::BitDepth::Eight) => {
          let gray_len = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| {
              Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "PNG grayscale byte size overflow".to_string(),
              })
            })?;
          let gray_len = usize::try_from(gray_len).map_err(|_| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale byte size does not fit in usize".to_string(),
            })
          })?;
          if buf.len() != gray_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG grayscale output length mismatch (expected {gray_len} bytes, got {})",
                buf.len()
              ),
            }));
          }

          let mut out = Vec::new();
          out.try_reserve_exact(rgba_len).map_err(|err| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!("PNG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
            })
          })?;
          out.resize(rgba_len, 0);
          for (gray, out_px) in buf.iter().zip(out.chunks_exact_mut(4)) {
            out_px[0] = *gray;
            out_px[1] = *gray;
            out_px[2] = *gray;
            out_px[3] = 255;
          }
          RgbaImage::from_raw(width, height, out).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale->RGBA buffer was invalid".to_string(),
            })
          })?
        }
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
          let ga_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|px| px.checked_mul(2))
            .ok_or_else(|| {
              Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "PNG grayscale-alpha byte size overflow".to_string(),
              })
            })?;
          let ga_len = usize::try_from(ga_len).map_err(|_| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale-alpha byte size does not fit in usize".to_string(),
            })
          })?;
          if buf.len() != ga_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG grayscale-alpha output length mismatch (expected {ga_len} bytes, got {})",
                buf.len()
              ),
            }));
          }

          let mut out = Vec::new();
          out.try_reserve_exact(rgba_len).map_err(|err| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!("PNG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
            })
          })?;
          out.resize(rgba_len, 0);
          for (in_px, out_px) in buf.chunks_exact(2).zip(out.chunks_exact_mut(4)) {
            let gray = in_px[0];
            out_px[0] = gray;
            out_px[1] = gray;
            out_px[2] = gray;
            out_px[3] = in_px[1];
          }
          RgbaImage::from_raw(width, height, out).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale-alpha->RGBA buffer was invalid".to_string(),
            })
          })?
        }
        _ => {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!(
              "Unsupported PNG color type {:?} ({:?})",
              frame.color_type, frame.bit_depth
            ),
          }));
        }
      };

      return Ok(DynamicImage::ImageRgba8(rgba));
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      ImageReader::with_format(DeadlineCursor::new(bytes), format).decode()
    })) {
      Ok(Ok(img)) => Ok(img),
      Ok(Err(err)) => Err(self.decode_error(url, err)),
      Err(panic) => Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "image decode panicked: {}",
          panic_payload_to_reason(&*panic)
        ),
      })),
    }
  }

  fn decode_with_guess(&self, bytes: &[u8], url: &str) -> Result<DynamicImage> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      ImageReader::new(DeadlineCursor::new(bytes))
        .with_guessed_format()?
        .decode()
    })) {
      Ok(Ok(img)) => Ok(img),
      Ok(Err(err)) => Err(self.decode_error(url, err)),
      Err(panic) => Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "image decode (guessed format) panicked: {}",
          panic_payload_to_reason(&*panic)
        ),
      })),
    }
  }

  fn decode_error(&self, url: &str, err: image::ImageError) -> Error {
    if let image::ImageError::IoError(io_err) = &err {
      if let Some(render_err) = io_err
        .get_ref()
        .and_then(|source| source.downcast_ref::<RenderError>())
      {
        return Error::Render(render_err.clone());
      }
    }
    Error::Image(ImageError::DecodeFailed {
      url: url.to_string(),
      reason: err.to_string(),
    })
  }

  fn predecoded_dimensions(
    &self,
    bytes: &[u8],
    format_from_content_type: Option<ImageFormat>,
    sniffed_format: Option<ImageFormat>,
  ) -> (Option<(u32, u32)>, Option<String>) {
    let mut panic_reason = None;

    if let Some(format) = format_from_content_type {
      let (dims, panic) = Self::dimensions_for_format(bytes, format);
      if dims.is_some() {
        return (dims, None);
      }
      if panic_reason.is_none() {
        panic_reason = panic;
      }
    }

    if let Some(format) = sniffed_format {
      let (dims, panic) = Self::dimensions_for_format(bytes, format);
      if dims.is_some() {
        return (dims, None);
      }
      if panic_reason.is_none() {
        panic_reason = panic;
      }
    }

    (None, panic_reason)
  }

  /// Returns a prefix of `bytes` that excludes any trailing data that does not form a valid top
  /// level ISO-BMFF box.
  ///
  /// Some AVIF payloads (including pageset content) include non-box trailer bytes that trip
  /// debug-only assertions inside `avif_parse`. Trimming to the last complete box keeps intrinsic
  /// probing robust without impacting real decoders (which generally ignore such trailers).
  fn trim_isobmff_trailing_bytes(bytes: &[u8]) -> &[u8] {
    let len = bytes.len();
    let mut offset = 0usize;

    while offset
      .checked_add(8)
      .is_some_and(|next_header| next_header <= len)
    {
      let Some(size_end) = offset.checked_add(4).filter(|end| *end <= len) else {
        break;
      };
      let Some(size_bytes) = bytes.get(offset..size_end) else {
        break;
      };
      let Ok(size_bytes) = <[u8; 4]>::try_from(size_bytes) else {
        break;
      };
      let size = u32::from_be_bytes(size_bytes);

      let box_size = match size {
        // Box extends to end of file.
        0 => len - offset,
        // Extended size stored in the next 8 bytes.
        1 => {
          let Some(ext_end) = offset.checked_add(16).filter(|end| *end <= len) else {
            break;
          };
          let Some(ext_start) = offset.checked_add(8) else {
            break;
          };
          let Some(ext_slice) = bytes.get(ext_start..ext_end) else {
            break;
          };
          let Ok(ext_bytes) = <[u8; 8]>::try_from(ext_slice) else {
            break;
          };
          let ext = u64::from_be_bytes(ext_bytes);
          if ext < 16 {
            break;
          }
          let Ok(ext) = usize::try_from(ext) else {
            break;
          };
          ext
        }
        // Regular 32-bit size.
        n => n as usize,
      };

      // Invalid box size.
      if box_size < 8 {
        break;
      }
      let Some(next) = offset.checked_add(box_size) else {
        break;
      };
      if next > len {
        break;
      }

      offset = next;
      if offset == len {
        return bytes;
      }
    }

    if offset > 0 {
      &bytes[..offset]
    } else {
      bytes
    }
  }

  fn dimensions_for_format(
    bytes: &[u8],
    format: ImageFormat,
  ) -> (Option<(u32, u32)>, Option<String>) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
      ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::Jpeg => image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::Avif => {
        // `avif_parse` includes debug assertions that can panic when the payload includes trailing
        // bytes (which is tolerated by other decoders and observed on pageset content). Trim and
        // catch panics so image probing doesn't crash the renderer in debug builds.
        let trimmed = Self::trim_isobmff_trailing_bytes(bytes);
        let mut cursor = Cursor::new(trimmed);
        let data = AvifData::from_reader(&mut cursor).ok()?;
        let meta = data.primary_item_metadata().ok()?;
        Some((meta.max_frame_width.get(), meta.max_frame_height.get()))
      }
      _ => None,
    })) {
      Ok(dims) => (dims, None),
      Err(panic) => (
        None,
        Some(format!(
          "{format:?} dimensions probe panicked: {}",
          panic_payload_to_reason(&*panic)
        )),
      ),
    }
  }

  fn enforce_decode_limits(&self, width: u32, height: u32, url: &str) -> Result<()> {
    if self.config.max_decoded_dimension > 0
      && (width > self.config.max_decoded_dimension || height > self.config.max_decoded_dimension)
    {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "Image dimensions {}x{} exceed maximum dimension {}",
          width, height, self.config.max_decoded_dimension
        ),
      }));
    }

    if self.config.max_decoded_pixels > 0 {
      let pixels = u64::from(width) * u64::from(height);
      if pixels > self.config.max_decoded_pixels {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!(
            "Image dimensions {}x{} exceed pixel budget of {}",
            width, height, self.config.max_decoded_pixels
          ),
        }));
      }
    }

    // Bound decoded images even when the caller disables `max_decoded_*` limits so we don't
    // allocate unbounded buffers that can abort the process on OOM.
    let pixels = u64::from(width) * u64::from(height);
    let bytes = pixels.checked_mul(4).ok_or_else(|| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "Image dimensions {}x{} overflow decoded byte size",
          width, height
        ),
      })
    })?;
    if bytes > MAX_PIXMAP_BYTES {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "Image dimensions {}x{} require {} bytes (limit {})",
          width, height, bytes, MAX_PIXMAP_BYTES
        ),
      }));
    }

    Ok(())
  }

  fn decode_avif(bytes: &[u8]) -> std::result::Result<DynamicImage, AvifDecodeError> {
    let decode_inner = |payload: &[u8]| -> std::result::Result<DynamicImage, AvifDecodeError> {
      check_root(RenderStage::Paint).map_err(AvifDecodeError::from)?;
      let decoder = AvifDecoder::from_avif(payload)
        .map_err(|err| AvifDecodeError::Image(Self::avif_error(err)))?;
      check_root(RenderStage::Paint).map_err(AvifDecodeError::from)?;
      let image = decoder
        .to_image()
        .map_err(|err| AvifDecodeError::Image(Self::avif_error(err)))?;
      let mut deadline_counter = 0usize;
      check_root(RenderStage::Paint).map_err(AvifDecodeError::from)?;
      Self::avif_image_to_dynamic(image, &mut deadline_counter)
    };

    let try_decode = |payload: &[u8]| -> std::result::Result<DynamicImage, AvifDecodeError> {
      match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_inner(payload))) {
        Ok(result) => result,
        Err(panic) => {
          let message = panic_payload_to_reason(&*panic);
          Err(AvifDecodeError::Image(Self::avif_error(format!(
            "avif decode panicked: {message}"
          ))))
        }
      }
    };

    let original = match try_decode(bytes) {
      Ok(img) => return Ok(img),
      Err(err) => err,
    };

    if matches!(original, AvifDecodeError::Timeout(_)) {
      return Err(original);
    }

    let trimmed = Self::trim_isobmff_trailing_bytes(bytes);
    if trimmed.len() < bytes.len() {
      match try_decode(trimmed) {
        Ok(img) => Ok(img),
        Err(err) => match err {
          AvifDecodeError::Timeout(_) => Err(err),
          AvifDecodeError::Image(_) => Err(original),
        },
      }
    } else {
      Err(original)
    }
  }

  fn reserve_for_bytes(bytes: u64, context: &str) -> std::result::Result<usize, image::ImageError> {
    if bytes > MAX_PIXMAP_BYTES {
      return Err(image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer would require {bytes} bytes (limit {MAX_PIXMAP_BYTES})"),
      )));
    }
    usize::try_from(bytes).map_err(|_| {
      image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer size {bytes} does not fit in usize"),
      ))
    })
  }

  fn reserve_image_buffer(
    bytes: u64,
    context: &str,
  ) -> std::result::Result<Vec<u8>, image::ImageError> {
    let len = Self::reserve_for_bytes(bytes, context)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|err| {
      image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer allocation failed for {bytes} bytes: {err}"),
      ))
    })?;
    Ok(buf)
  }

  fn reserve_image_buffer_u16(
    bytes: u64,
    context: &str,
  ) -> std::result::Result<Vec<u16>, image::ImageError> {
    if bytes % 2 != 0 {
      return Err(image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer size {bytes} is not aligned to u16"),
      )));
    }
    let len = Self::reserve_for_bytes(bytes, context)? / 2;
    let mut buf: Vec<u16> = Vec::new();
    buf.try_reserve_exact(len).map_err(|err| {
      image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer allocation failed for {bytes} bytes: {err}"),
      ))
    })?;
    Ok(buf)
  }

  fn avif_image_to_dynamic(
    image: AvifImage,
    deadline_counter: &mut usize,
  ) -> std::result::Result<DynamicImage, AvifDecodeError> {
    let dimension_error = || {
      AvifDecodeError::Image(image::ImageError::Parameter(
        image::error::ParameterError::from_kind(
          image::error::ParameterErrorKind::DimensionMismatch,
        ),
      ))
    };

    match image {
      AvifImage::Rgb8(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(3))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGB8 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer(bytes, "avif rgb8 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.extend_from_slice(&[px.r, px.g, px.b]);
        }
        image::RgbImage::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgb8)
          .ok_or_else(dimension_error)
      }
      AvifImage::Rgb16(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(3))
          .and_then(|px| px.checked_mul(2))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGB16 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer_u16(bytes, "avif rgb16 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.extend_from_slice(&[px.r, px.g, px.b]);
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgb16)
          .ok_or_else(dimension_error)
      }
      AvifImage::Rgba8(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(4))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGBA8 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer(bytes, "avif rgba8 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.extend_from_slice(&[px.r, px.g, px.b, px.a]);
        }
        image::RgbaImage::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgba8)
          .ok_or_else(dimension_error)
      }
      AvifImage::Rgba16(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(4))
          .and_then(|px| px.checked_mul(2))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGBA16 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer_u16(bytes, "avif rgba16 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.extend_from_slice(&[px.r, px.g, px.b, px.a]);
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgba16)
          .ok_or_else(dimension_error)
      }
      AvifImage::Gray8(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width).checked_mul(u64::from(height)) else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "Gray8 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer(bytes, "avif gray8 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.push(px.value());
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageLuma8)
          .ok_or_else(dimension_error)
      }
      AvifImage::Gray16(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(2))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "Gray16 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer_u16(bytes, "avif gray16 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.push(px.value());
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageLuma16)
          .ok_or_else(dimension_error)
      }
    }
  }

  fn avif_dimensions(
    width: usize,
    height: usize,
  ) -> std::result::Result<(u32, u32), image::ImageError> {
    let to_u32 = |v: usize| {
      u32::try_from(v).map_err(|_| {
        image::ImageError::Parameter(image::error::ParameterError::from_kind(
          image::error::ParameterErrorKind::DimensionMismatch,
        ))
      })
    };
    Ok((to_u32(width)?, to_u32(height)?))
  }

  fn avif_error(err: impl std::fmt::Display) -> image::ImageError {
    image::ImageError::Decoding(image::error::DecodingError::new(
      ImageFormat::Avif.into(),
      err.to_string(),
    ))
  }

  fn orientation_from_exif(value: u16) -> Option<OrientationTransform> {
    match value {
      1 => Some(OrientationTransform::IDENTITY),
      2 => Some(OrientationTransform {
        quarter_turns: 0,
        flip_x: true,
      }),
      3 => Some(OrientationTransform {
        quarter_turns: 2,
        flip_x: false,
      }),
      4 => Some(OrientationTransform {
        quarter_turns: 2,
        flip_x: true,
      }),
      5 => Some(OrientationTransform {
        quarter_turns: 1,
        flip_x: true,
      }),
      6 => Some(OrientationTransform {
        quarter_turns: 1,
        flip_x: false,
      }),
      7 => Some(OrientationTransform {
        quarter_turns: 3,
        flip_x: true,
      }),
      8 => Some(OrientationTransform {
        quarter_turns: 3,
        flip_x: false,
      }),
      _ => None,
    }
  }

  fn exif_metadata(bytes: &[u8]) -> (Option<OrientationTransform>, Option<f32>) {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut cursor = std::io::Cursor::new(bytes);
      let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) else {
        return (None, None);
      };

      let orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .and_then(|v| Self::orientation_from_exif(v as u16));

      let resolution_unit = exif
        .get_field(exif::Tag::ResolutionUnit, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(0);

      let rational_to_f32 = |r: exif::Rational| -> Option<f32> {
        if r.denom == 0 {
          None
        } else {
          Some(r.num as f32 / r.denom as f32)
        }
      };

      let x_res = exif
        .get_field(exif::Tag::XResolution, exif::In::PRIMARY)
        .and_then(|f| {
          if let exif::Value::Rational(ref vals) = f.value {
            vals.first().copied()
          } else {
            None
          }
        })
        .and_then(rational_to_f32);
      let y_res = exif
        .get_field(exif::Tag::YResolution, exif::In::PRIMARY)
        .and_then(|f| {
          if let exif::Value::Rational(ref vals) = f.value {
            vals.first().copied()
          } else {
            None
          }
        })
        .and_then(rational_to_f32);
      let avg_res = match (x_res, y_res) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() && x > 0.0 && y > 0.0 => {
          Some((x + y) / 2.0)
        }
        (Some(v), None) | (None, Some(v)) if v.is_finite() && v > 0.0 => Some(v),
        _ => None,
      };

      let resolution = avg_res.and_then(|res| match resolution_unit {
        2 => Some(res / 96.0),          // inch -> dppx
        3 => Some((res * 2.54) / 96.0), // cm -> dppx
        _ => None,
      });

      (orientation, resolution)
    }))
    .unwrap_or((None, None))
  }

  #[allow(dead_code)]
  fn exif_orientation(bytes: &[u8]) -> Option<OrientationTransform> {
    Self::exif_metadata(bytes).0
  }

  /// Renders raw SVG content to a raster image, returning any parsed intrinsic aspect ratio
  /// information.
  pub fn render_svg_to_image(
    &self,
    svg_content: &str,
  ) -> Result<(DynamicImage, Option<f32>, bool)> {
    self.render_svg_to_image_with_url(svg_content, "SVG content")
  }

  fn render_svg_to_image_with_url(
    &self,
    svg_content: &str,
    url: &str,
  ) -> Result<(DynamicImage, Option<f32>, bool)> {
    use resvg::usvg;

    check_root(RenderStage::Paint).map_err(Error::Render)?;
    let url_no_fragment = strip_url_fragment(url);

    // Parse SVG
    let options = usvg_options_for_url(url_no_fragment.as_ref());
    self.enforce_svg_resource_policy(svg_content, url_no_fragment.as_ref())?;
    let svg_use_inlined = inline_svg_use_references(
      svg_content,
      url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
    )?;
    let svg_images_inlined = inline_svg_image_references(
      svg_use_inlined.as_ref(),
      url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
    )?;
    let svg_fragment_applied = apply_svg_url_fragment(svg_images_inlined.as_ref(), url);
    let svg_content = svg_fragment_applied.as_ref();
    let (meta_width, meta_height, meta_ratio, aspect_ratio_none) =
      svg_intrinsic_metadata(svg_content, 16.0, 16.0).unwrap_or((None, None, None, false));
    let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      usvg::Tree::from_str(svg_content, &options)
    })) {
      Ok(Ok(tree)) => tree,
      Ok(Err(e)) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("Failed to parse SVG: {}", e),
        }));
      }
      Err(panic) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVG parse panicked: {}", panic_payload_to_reason(&*panic)),
        }));
      }
    };

    let size = tree.size();
    let source_width = size.width();
    let source_height = size.height();
    if source_width <= 0.0 || source_height <= 0.0 {
      return Err(Error::Render(RenderError::CanvasCreationFailed {
        width: source_width as u32,
        height: source_height as u32,
      }));
    }

    let ratio = meta_ratio.filter(|r| *r > 0.0);
    let (target_width, target_height) =
      svg_intrinsic_target_dimensions(meta_width, meta_height, ratio);

    let render_width = target_width.max(1.0).round() as u32;
    let render_height = target_height.max(1.0).round() as u32;

    self.enforce_decode_limits(render_width, render_height, url)?;
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    // Render SVG to pixmap, scaling to the target intrinsic dimensions when needed
    let mut pixmap = new_pixmap(render_width, render_height).ok_or(Error::Render(
      RenderError::CanvasCreationFailed {
        width: render_width,
        height: render_height,
      },
    ))?;

    let transform = match svg_view_box_root_transform(
      svg_content,
      source_width,
      source_height,
      render_width as f32,
      render_height as f32,
    ) {
      Some(transform) => transform,
      None => {
        let scale_x = render_width as f32 / source_width;
        let scale_y = render_height as f32 / source_height;
        tiny_skia::Transform::from_scale(scale_x, scale_y)
      }
    };
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      resvg::render(&tree, transform, &mut pixmap.as_mut());
    })) {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("SVG render panicked: {}", panic_payload_to_reason(&*panic)),
      }));
    }
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    // Convert pixmap to image
    let rgba_data = pixmap.take();
    let img =
      image::RgbaImage::from_raw(render_width, render_height, rgba_data).ok_or_else(|| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "Failed to create image from SVG pixmap".to_string(),
        })
      })?;

    let ratio = if aspect_ratio_none {
      None
    } else {
      ratio.or_else(|| {
        if render_height > 0 {
          Some(render_width as f32 / render_height as f32)
        } else {
          None
        }
      })
    };

    Ok((
      image::DynamicImage::ImageRgba8(img),
      ratio,
      aspect_ratio_none,
    ))
  }
}

fn svg_intrinsic_target_dimensions(
  meta_width: Option<f32>,
  meta_height: Option<f32>,
  intrinsic_ratio: Option<f32>,
) -> (f32, f32) {
  const DEFAULT_WIDTH: f32 = 300.0;
  const DEFAULT_HEIGHT: f32 = 150.0;

  let width = meta_width.filter(|w| *w > 0.0);
  let height = meta_height.filter(|h| *h > 0.0);
  let ratio = intrinsic_ratio.filter(|r| *r > 0.0);

  match (width, height, ratio) {
    (Some(w), Some(h), _) => (w, h),
    (Some(w), None, Some(r)) => (w, (w / r).max(1.0)),
    (None, Some(h), Some(r)) => ((h * r).max(1.0), h),
    (Some(w), None, None) => (w, DEFAULT_HEIGHT),
    (None, Some(h), None) => (DEFAULT_WIDTH, h),
    // When the SVG root doesn't specify an absolute intrinsic size (missing or percentage
    // widths/heights), HTML/CSS replaced elements default to 300x150 regardless of viewBox ratio.
    // Keep the ratio separately (see `intrinsic_ratio`) so layout can still infer the correct
    // aspect ratio.
    (None, None, _) => (DEFAULT_WIDTH, DEFAULT_HEIGHT),
  }
}

/// Returns intrinsic metadata extracted from the SVG root element: explicit width/height when
/// present (including common font-relative units when `font_size`/`root_font_size` are provided),
/// an intrinsic aspect ratio (if not disabled), and whether preserveAspectRatio="none" was
/// specified.
fn svg_intrinsic_metadata(
  svg_content: &str,
  font_size: f32,
  root_font_size: f32,
) -> Option<(Option<f32>, Option<f32>, Option<f32>, bool)> {
  std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let doc = Document::parse(svg_content).ok()?;
    let root = doc.root_element();
    if !root.tag_name().name().eq_ignore_ascii_case("svg") {
      return None;
    }

    let intrinsic = svg_intrinsic_dimensions_from_attributes(
      root.attribute("width"),
      root.attribute("height"),
      root.attribute("viewBox"),
      root.attribute("preserveAspectRatio"),
      font_size,
      root_font_size,
    );

    Some((
      intrinsic.width,
      intrinsic.height,
      intrinsic.aspect_ratio,
      intrinsic.aspect_ratio_none,
    ))
  }))
  .ok()
  .flatten()
}

// ============================================================================
// URL Resolution
// ============================================================================

pub(crate) fn resolve_against_base(base: &str, reference: &str) -> Option<String> {
  // Normalize file:// bases that point to directories so Url::join keeps the directory segment.
  let mut base_candidate = base.to_string();
  if base_candidate.starts_with("file://") {
    let path = &base_candidate["file://".len()..];
    if Path::new(path).is_dir() && !base_candidate.ends_with('/') {
      base_candidate.push('/');
    }
  }

  let mut base_url = Url::parse(&base_candidate)
    .or_else(|_| {
      Url::from_file_path(&base_candidate).map_err(|()| url::ParseError::RelativeUrlWithoutBase)
    })
    .ok()?;

  if base_url.scheme() == "file" {
    if let Ok(path) = base_url.to_file_path() {
      if path.is_dir() && !base_url.path().ends_with('/') {
        let mut path_str = base_url.path().to_string();
        path_str.push('/');
        base_url.set_path(&path_str);
      }
    }
  }

  let normalized = normalize_url_reference_for_resolution(reference);
  if normalized.as_ref() != reference {
    if let Ok(joined) = base_url.join(normalized.as_ref()) {
      return Some(joined.to_string());
    }
  }

  base_url.join(reference).ok().map(|u| u.to_string())
}

// ============================================================================
// Trait Implementations
// ============================================================================

impl Default for ImageCache {
  fn default() -> Self {
    Self::new()
  }
}

impl Clone for ImageCache {
  fn clone(&self) -> Self {
    Self {
      instance_id: self.instance_id,
      cache: Arc::clone(&self.cache),
      in_flight: Arc::clone(&self.in_flight),
      meta_cache: Arc::clone(&self.meta_cache),
      raw_cache: Arc::clone(&self.raw_cache),
      meta_in_flight: Arc::clone(&self.meta_in_flight),
      svg_pixmap_cache: Arc::clone(&self.svg_pixmap_cache),
      raster_pixmap_cache: Arc::clone(&self.raster_pixmap_cache),
      base_url: self.base_url.clone(),
      fetcher: Arc::clone(&self.fetcher),
      config: self.config,
      diagnostics: self.diagnostics.clone(),
      resource_context: self.resource_context.clone(),
    }
  }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;
  use crate::render_control::RenderDeadline;
  use crate::style::types::OrientationTransform;
  use base64::Engine;
  use image::codecs::png::PngEncoder;
  use image::ColorType;
  use image::ImageEncoder;
  use image::RgbaImage;
  use std::path::PathBuf;
  use std::time::Duration;
  use std::time::SystemTime;
  use url::Url;

  #[test]
  fn svg_text_renders_with_fontdb_configured() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="30">
      <text x="0" y="20" font-family="Cantarell" font-size="20" fill="red">F</text>
    </svg>"#;

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 80, 30, "svg-text", 1.0)
      .expect("render svg pixmap");

    let has_red_text_pixel = pixmap
      .data()
      .chunks_exact(4)
      .any(|px| px[3] > 0 && px[0] > 0);
    assert!(
      has_red_text_pixel,
      "expected SVG <text> to produce non-transparent red pixels; missing usvg fontdb?"
    );
  }

  #[test]
  fn resolve_against_base_normalizes_pipe_character() {
    let resolved = resolve_against_base("https://example.com/dir/", "a|b.svg").expect("resolved");
    assert_eq!(resolved, "https://example.com/dir/a%7Cb.svg");
  }

  #[test]
  fn resolve_against_base_preserves_nbsp() {
    let nbsp = "\u{00A0}";
    let reference = format!("a{nbsp}b.svg");
    let resolved = resolve_against_base("https://example.com/dir/", &reference).expect("resolved");
    assert_eq!(resolved, "https://example.com/dir/a%C2%A0b.svg");
  }

  #[derive(Clone, Default)]
  struct MapFetcher {
    responses: Arc<HashMap<String, FetchedResource>>,
    requests: Arc<Mutex<Vec<(String, FetchDestination, FetchCredentialsMode)>>>,
  }

  impl MapFetcher {
    fn with_entries(entries: impl IntoIterator<Item = (String, FetchedResource)>) -> Self {
      Self {
        responses: Arc::new(entries.into_iter().collect()),
        requests: Arc::new(Mutex::new(Vec::new())),
      }
    }

    fn requests(&self) -> Vec<(String, FetchDestination, FetchCredentialsMode)> {
      self
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .responses
        .get(url)
        .cloned()
        .map(|mut res| {
          res.final_url.get_or_insert_with(|| url.to_string());
          res
        })
        .ok_or_else(|| Error::Other(format!("unexpected fetch url {url}")))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((req.url.to_string(), req.destination, req.credentials_mode));
      self.fetch(req.url)
    }
  }

  fn gzip_bytes(input: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).expect("gzip input");
    encoder.finish().expect("finish gzip")
  }

  fn encode_single_pixel_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut pixels = RgbaImage::new(1, 1);
    pixels.pixels_mut().for_each(|p| *p = image::Rgba(rgba));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
      .expect("encode png");
    png
  }

  #[derive(Clone)]
  struct ReferrerAwarePngFetcher {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    variants: Arc<HashMap<(Option<String>, ReferrerPolicy), Vec<u8>>>,
  }

  impl ReferrerAwarePngFetcher {
    fn new(variants: HashMap<(Option<String>, ReferrerPolicy), Vec<u8>>) -> Self {
      Self {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        variants: Arc::new(variants),
      }
    }

    fn calls(&self) -> usize {
      self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
  }

  impl ResourceFetcher for ReferrerAwarePngFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      panic!("expected ImageCache image load to use fetch_with_request");
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
      let key = (req.referrer_url.map(|v| v.to_string()), req.referrer_policy);
      let bytes = self
        .variants
        .get(&key)
        .cloned()
        .ok_or_else(|| Error::Other(format!("unexpected image fetch request {key:?}")))?;
      let mut res = FetchedResource::new(bytes, Some("image/png".to_string()));
      res.status = Some(200);
      res.final_url = Some(req.url.to_string());
      Ok(res)
    }
  }

  #[test]
  fn svg_pixmap_key_canonicalizes_negative_zero() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
    let url = "https://example.com/negzero.svg";
    let key = svg_pixmap_key(svg, url, 0.0, 10, 10);
    let key_neg = svg_pixmap_key(svg, url, -0.0, 10, 10);
    assert_eq!(key, key_neg);
  }

  #[test]
  fn image_fetch_strips_http_fragment() {
    let url = "https://example.test/icon.svg";
    let url_with_fragment = "https://example.test/icon.svg#frag";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#;

    let mut res = FetchedResource::new(svg.as_bytes().to_vec(), Some("image/svg+xml".to_string()));
    res.status = Some(200);
    res.final_url = Some(url.to_string());

    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let img = cache.load(url_with_fragment).expect("image should load");
    assert!(img.is_vector, "expected SVG image");
    assert_eq!(img.dimensions(), (1, 1));
    assert_eq!(
      fetcher.requests(),
      vec![(
        url.to_string(),
        FetchDestination::Image,
        FetchCredentialsMode::Include,
      )]
    );
  }

  #[test]
  fn data_svg_url_with_fragment_decodes() {
    let url = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='1'%20height='1'%3E%3C/svg%3E#ignored";
    let cache = ImageCache::new();

    let img = cache.load(url).expect("data SVG should decode");
    assert!(img.is_vector, "expected SVG image");
    assert_eq!(img.dimensions(), (1, 1));
  }

  fn svg_policy_cache_same_origin_only(doc_url: &str) -> ImageCache {
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut cache = ImageCache::new();
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));
    cache
  }

  #[test]
  fn svg_policy_blocks_external_href() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://cross.test/a.png"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_css_url() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>rect{fill:url(https://cross.test/a.png)}</style><rect width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG CSS subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_css_import_by_csp() {
    use crate::html::content_security_policy::CspPolicy;

    let doc_url = "https://doc.test/";
    let doc_origin = origin_from_url(doc_url).expect("document origin");

    // Allow cross-origin network loads by default, but use CSP to block cross-origin stylesheets.
    let mut cache = ImageCache::new();
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = false;
    ctx.csp = CspPolicy::from_values(["style-src 'self'; img-src *"]);
    assert!(ctx.csp.is_some(), "CSP should parse");
    cache.set_resource_context(Some(ctx));

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url(https://cross.test/a.css); rect{fill:red}</style><rect width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG CSS @import to be blocked by CSP style-src");

    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.css");
        assert!(
          reason.contains("Content-Security-Policy") && reason.contains("style-src"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_url_in_fill_attribute() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(https://cross.test/a.png)" width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG attribute url() subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(reason.contains("Blocked cross-origin subresource"), "{reason}");
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_url_in_filter_attribute() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect filter="url(https://cross.test/f.svg#f)" width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG attribute url() subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/f.svg#f");
        assert!(reason.contains("Blocked cross-origin subresource"), "{reason}");
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_scheme_relative_href_in_inline_svg() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="//cross.test/a.png"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "inline-svg")
      .expect_err("expected SVG subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(reason.contains("Blocked cross-origin subresource"), "{reason}");
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_scheme_relative_css_url_in_inline_svg() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>rect{fill:url(//cross.test/a.png)}</style><rect width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "inline-svg")
      .expect_err("expected SVG CSS subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(reason.contains("Blocked cross-origin subresource"), "{reason}");
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_ignores_fragment_only_url_in_attributes() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(#grad)" width="10" height="10"/></svg>"#;
    cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect("expected fragment-only url() references to be ignored");
  }

  #[test]
  fn svg_policy_allows_data_urls_and_fragments() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><g id="id"/></defs><use href="#id"/><image href="data:image/png;base64,AAAA"/></svg>"##;
    cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect("expected data URLs and fragment-only hrefs to be ignored");
  }

  #[test]
  fn svg_policy_allows_large_non_css_attribute_values() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");

    // Large non-CSS attribute values (e.g. huge path data) should not consume the embedded CSS scan
    // budget when they cannot contain `url(...)` references.
    let d = "M".repeat(600 * 1024);
    assert!(d.len() > 512 * 1024, "expected test path data to exceed scan budget");
    let svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><path d="{d}"/></svg>"#
    );

    cache
      .probe_svg_content(&svg, "https://doc.test/icon.svg")
      .expect("expected large non-CSS attribute values to be allowed");
  }

  #[test]
  fn image_cache_diagnostics_survives_poisoned_lock() {
    let result = std::panic::catch_unwind(|| {
      let _guard = IMAGE_CACHE_DIAGNOSTICS.lock().unwrap();
      panic!("poison image cache diagnostics lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    assert!(
      IMAGE_CACHE_DIAGNOSTICS.is_poisoned(),
      "expected image cache diagnostics mutex to be poisoned"
    );

    enable_image_cache_diagnostics();
    record_image_cache_request();
    let stats = take_image_cache_diagnostics().expect("diagnostics enabled");
    assert_eq!(stats.requests, 1);
  }

  #[test]
  fn decode_inflight_recovers_from_poisoned_lock() {
    let inflight = DecodeInFlight::new();
    let result = std::panic::catch_unwind(|| {
      let _guard = inflight.result.lock().unwrap();
      panic!("poison decode inflight lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    let deadline = RenderDeadline::new(Some(Duration::from_millis(50)), None);
    render_control::with_deadline(Some(&deadline), || {
      inflight.set(SharedImageResult::Error(Error::Render(
        RenderError::Timeout {
          stage: RenderStage::Paint,
          elapsed: Duration::from_millis(0),
        },
      )));
      let err = match inflight.wait("https://example.com/image.png") {
        Ok(_) => panic!("expected error result"),
        Err(err) => err,
      };
      assert!(matches!(err, Error::Render(RenderError::Timeout { .. })));
    });
  }

  #[test]
  fn probe_inflight_recovers_from_poisoned_lock() {
    let inflight = ProbeInFlight::new();
    let result = std::panic::catch_unwind(|| {
      let _guard = inflight.result.lock().unwrap();
      panic!("poison probe inflight lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    let deadline = RenderDeadline::new(Some(Duration::from_millis(50)), None);
    render_control::with_deadline(Some(&deadline), || {
      inflight.set(SharedMetaResult::Error(Error::Render(
        RenderError::Timeout {
          stage: RenderStage::Paint,
          elapsed: Duration::from_millis(0),
        },
      )));
      let err = match inflight.wait("https://example.com/image.png") {
        Ok(_) => panic!("expected error result"),
        Err(err) => err,
      };
      assert!(matches!(err, Error::Render(RenderError::Timeout { .. })));
    });
  }

  #[test]
  fn image_decode_uses_root_deadline_instead_of_nested_budget() {
    // The paint pipeline installs nested deadlines to allocate small time budgets to internal
    // phases (e.g. display-list construction). Image decoding can be triggered inside those
    // phases, but should still be bounded by the overall render timeout (root deadline) rather
    // than the internal sub-budget.
    let root = RenderDeadline::new(Some(Duration::from_secs(5)), None);
    render_control::with_deadline(Some(&root), || {
      // A nested deadline that is immediately expired should not prevent file:// image decoding.
      let nested = RenderDeadline::new(Some(Duration::ZERO), None);
      render_control::with_deadline(Some(&nested), || {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
          .join("tests/fixtures/accuracy_png/baseline.png");
        let url = Url::from_file_path(path)
          .expect("baseline.png file URL")
          .to_string();
        let cache = ImageCache::new();
        let img = cache
          .load(&url)
          .expect("image should load under root deadline");
        assert!(
          !cache.is_placeholder_image(&img),
          "loaded image should not be the about: placeholder"
        );
      });
    });
  }

  #[test]
  fn img_crossorigin_enforces_acao_when_toggle_enabled() {
    let _toggles_guard = runtime::set_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(
      HashMap::from([("FASTR_FETCH_ENFORCE_CORS".to_string(), "1".to_string())]),
    )));

    let doc_url = "https://doc.test/";
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");

    let mut pixels = RgbaImage::new(1, 1);
    pixels
      .pixels_mut()
      .for_each(|p| *p = image::Rgba([255, 0, 0, 255]));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
      .expect("encode png");

    let cross_no_acao = "https://img.test/no-acao.png";
    let same_no_acao = "https://doc.test/same.png";
    let cross_star = "https://img.test/star.png";
    let cred_missing = "https://img.test/cred-missing.png";
    let cred_ok = "https://img.test/cred-ok.png";

    let make_res = |url: &str, acao: Option<&str>, acac: bool| {
      let mut res = FetchedResource::new(png.clone(), Some("image/png".to_string()));
      res.status = Some(200);
      res.final_url = Some(url.to_string());
      res.access_control_allow_origin = acao.map(|v| v.to_string());
      res.access_control_allow_credentials = acac;
      res
    };

    let fetcher = MapFetcher::with_entries([
      (
        cross_no_acao.to_string(),
        make_res(cross_no_acao, None, false),
      ),
      (
        same_no_acao.to_string(),
        make_res(same_no_acao, None, false),
      ),
      (
        cross_star.to_string(),
        make_res(cross_star, Some("*"), false),
      ),
      (
        cred_missing.to_string(),
        make_res(cred_missing, Some(doc_url), false),
      ),
      (cred_ok.to_string(), make_res(cred_ok, Some(doc_url), true)),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    // Baseline: cross-origin image without `crossorigin` should still load (no CORS enforcement).
    cache
      .load_with_crossorigin(cross_no_acao, CrossOriginAttribute::None)
      .expect("no-cors image should load");

    // CORS-mode fetch should fail without ACAO.
    let err = match cache.load_with_crossorigin(cross_no_acao, CrossOriginAttribute::Anonymous) {
      Ok(_) => panic!("crossorigin=anonymous should enforce ACAO"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(reason.contains("Access-Control-Allow-Origin"));
      }
      other => panic!("expected CORS image load failure, got {other:?}"),
    }

    // Same-origin images should always pass CORS checks.
    cache
      .load_with_crossorigin(same_no_acao, CrossOriginAttribute::Anonymous)
      .expect("same-origin image should pass CORS enforcement");

    // Anonymous mode accepts `*`.
    cache
      .load_with_crossorigin(cross_star, CrossOriginAttribute::Anonymous)
      .expect("crossorigin=anonymous with ACAO=* should load");

    // Credentialed mode requires exact origin match and ACAC=true.
    let err = match cache.load_with_crossorigin(cred_missing, CrossOriginAttribute::UseCredentials)
    {
      Ok(_) => panic!("credentialed CORS without ACAC should fail"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(reason.contains("Access-Control-Allow-Credentials"));
      }
      other => panic!("expected credentialed CORS failure, got {other:?}"),
    }
    cache
      .load_with_crossorigin(cred_ok, CrossOriginAttribute::UseCredentials)
      .expect("credentialed CORS with ACAO match + ACAC should load");

    let requests = fetcher.requests();
    assert_eq!(
      requests
        .iter()
        .filter(|(url, _, _)| url == cross_no_acao)
        .map(|(_, dest, _)| *dest)
        .collect::<Vec<_>>(),
      vec![FetchDestination::Image, FetchDestination::ImageCors],
      "no-cors and cors-mode loads must not share the same fetch profile"
    );
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == same_no_acao && *dest == FetchDestination::ImageCors),
      "same-origin crossorigin images should still use ImageCors destination"
    );
    assert!(
      requests.iter().any(|(url, dest, credentials)| {
        url == cross_star
          && *dest == FetchDestination::ImageCors
          && *credentials == FetchCredentialsMode::SameOrigin
      }),
      "crossorigin=anonymous requests must use SameOrigin credentials mode"
    );
  }

  #[test]
  fn img_crossorigin_anonymous_uses_same_origin_credentials_mode() {
    #[derive(Clone, Debug)]
    struct ExpectedRequest {
      url: String,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
    }

    #[derive(Clone)]
    struct CredentialsModeFetcher {
      expected: Arc<Mutex<Vec<ExpectedRequest>>>,
      png: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CredentialsModeFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected ImageCache load to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        let expected = self
          .expected
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .pop()
          .expect("unexpected fetch_with_request call");
        assert_eq!(req.url, expected.url);
        assert_eq!(req.destination, expected.destination);
        assert_eq!(req.credentials_mode, expected.credentials_mode);
        let mut res = FetchedResource::new((*self.png).clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(req.url.to_string());
        // Ensure this test remains stable even when `FASTR_FETCH_ENFORCE_CORS` is enabled globally
        // by other concurrent tests.
        res.access_control_allow_origin = Some("*".to_string());
        Ok(res)
      }
    }

    let doc_url = "https://example.com/doc.html";
    let same_origin_url = "https://example.com/same.png";
    let cross_origin_url = "https://cross.test/cross.png";
    let no_cors_url = "https://example.com/no-cors.png";

    let png = encode_single_pixel_png([255, 0, 0, 255]);

    // Use a stack so we can `pop()` in call order.
    let expected = vec![
      ExpectedRequest {
        url: no_cors_url.to_string(),
        destination: FetchDestination::Image,
        credentials_mode: FetchCredentialsMode::Include,
      },
      ExpectedRequest {
        url: cross_origin_url.to_string(),
        destination: FetchDestination::ImageCors,
        credentials_mode: FetchCredentialsMode::SameOrigin,
      },
      ExpectedRequest {
        url: same_origin_url.to_string(),
        destination: FetchDestination::ImageCors,
        credentials_mode: FetchCredentialsMode::SameOrigin,
      },
    ];

    let fetcher = CredentialsModeFetcher {
      expected: Arc::new(Mutex::new(expected)),
      png: Arc::new(png),
    };

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = crate::resource::origin_from_url(doc_url);
    cache.set_resource_context(Some(ctx));

    cache
      .load_with_crossorigin(same_origin_url, CrossOriginAttribute::Anonymous)
      .expect("same-origin crossorigin=anonymous should load");
    cache
      .load_with_crossorigin(cross_origin_url, CrossOriginAttribute::Anonymous)
      .expect("cross-origin crossorigin=anonymous should load");
    cache
      .load_with_crossorigin(no_cors_url, CrossOriginAttribute::None)
      .expect("no-cors image should load");

    assert!(
      fetcher
        .expected
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty(),
      "expected all fetch_with_request expectations to be consumed"
    );
  }

  #[test]
  fn probe_metadata_artifacts_are_partitioned_by_client_origin_for_cors_images() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug)]
    struct StoredArtifact {
      bytes: Vec<u8>,
      access_control_allow_origin: Option<String>,
      access_control_allow_credentials: bool,
      final_url: Option<String>,
    }

    #[derive(Clone, Default)]
    struct ArtifactFetcher {
      calls: Arc<AtomicUsize>,
      png: Arc<Vec<u8>>,
      artifacts: Arc<Mutex<HashMap<(String, Option<String>), StoredArtifact>>>,
    }

    impl ArtifactFetcher {
      fn origin_key_from_client_origin(
        origin: Option<&crate::resource::DocumentOrigin>,
      ) -> Option<String> {
        let origin = origin?;
        if !origin.is_http_like() {
          return Some("null".to_string());
        }
        let host = origin.host()?;
        let mut origin_str = format!("{}://{}", origin.scheme(), host);
        if let Some(port) = origin.port() {
          let default_port = match origin.scheme() {
            "http" => 80,
            "https" => 443,
            _ => port,
          };
          if port != default_port {
            origin_str.push_str(&format!(":{port}"));
          }
        }
        Some(origin_str)
      }
    }

    impl ResourceFetcher for ArtifactFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected ImageCache probe to use fetch_with_request/fetch_partial_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.fetch_partial_with_request(req, usize::MAX)
      }

      fn fetch_partial_with_request(
        &self,
        req: FetchRequest<'_>,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
          req.destination,
          FetchDestination::ImageCors,
          "expected probe to use ImageCors destination"
        );
        let origin = Self::origin_key_from_client_origin(req.client_origin)
          .expect("expected client origin to be provided for CORS-mode image probe");
        let mut bytes = (*self.png).clone();
        if bytes.len() > max_bytes {
          bytes.truncate(max_bytes);
        }
        let mut res = FetchedResource::new(bytes, Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(req.url.to_string());
        res.access_control_allow_origin = Some(origin);
        Ok(res)
      }

      fn read_cache_artifact(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _artifact: CacheArtifactKind,
      ) -> Option<FetchedResource> {
        panic!("expected ImageCache probe to call read_cache_artifact_with_request");
      }

      fn write_cache_artifact(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _artifact: CacheArtifactKind,
        _bytes: &[u8],
        _source: Option<&FetchedResource>,
      ) {
        panic!("expected ImageCache probe to call write_cache_artifact_with_request");
      }

      fn remove_cache_artifact(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _artifact: CacheArtifactKind,
      ) {
        panic!("expected ImageCache probe to call remove_cache_artifact_with_request");
      }

      fn read_cache_artifact_with_request(
        &self,
        req: FetchRequest<'_>,
        artifact: CacheArtifactKind,
      ) -> Option<FetchedResource> {
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        let key = (
          req.url.to_string(),
          Self::origin_key_from_client_origin(req.client_origin),
        );
        let stored = self
          .artifacts
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .get(&key)
          .cloned()?;
        let mut res = FetchedResource::new(stored.bytes, None);
        res.status = Some(200);
        res.final_url = stored.final_url;
        res.access_control_allow_origin = stored.access_control_allow_origin;
        res.access_control_allow_credentials = stored.access_control_allow_credentials;
        Some(res)
      }

      fn write_cache_artifact_with_request(
        &self,
        req: FetchRequest<'_>,
        artifact: CacheArtifactKind,
        bytes: &[u8],
        source: Option<&FetchedResource>,
      ) {
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        let key = (
          req.url.to_string(),
          Self::origin_key_from_client_origin(req.client_origin),
        );
        let stored = StoredArtifact {
          bytes: bytes.to_vec(),
          access_control_allow_origin: source.and_then(|s| s.access_control_allow_origin.clone()),
          access_control_allow_credentials: source
            .map(|s| s.access_control_allow_credentials)
            .unwrap_or(false),
          final_url: source
            .and_then(|s| s.final_url.clone())
            .or_else(|| Some(req.url.to_string())),
        };
        self
          .artifacts
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .insert(key, stored);
      }

      fn remove_cache_artifact_with_request(
        &self,
        req: FetchRequest<'_>,
        artifact: CacheArtifactKind,
      ) {
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        let key = (
          req.url.to_string(),
          Self::origin_key_from_client_origin(req.client_origin),
        );
        self
          .artifacts
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .remove(&key);
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let mut pixels = RgbaImage::new(1, 1);
      pixels
        .pixels_mut()
        .for_each(|p| *p = image::Rgba([255, 0, 0, 255]));
      let mut png = Vec::new();
      PngEncoder::new(&mut png)
        .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
        .expect("encode png");

      let fetcher = ArtifactFetcher {
        calls: Arc::new(AtomicUsize::new(0)),
        png: Arc::new(png),
        artifacts: Arc::new(Mutex::new(HashMap::new())),
      };
      let url = "https://img.test/probe.png";

      let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

      for doc_url in ["http://a.test/page", "http://b.test/page"] {
        let mut ctx = ResourceContext::default();
        ctx.document_url = Some(doc_url.to_string());
        ctx.policy.document_origin = crate::resource::origin_from_url(doc_url);
        cache.set_resource_context(Some(ctx));
        cache
          .probe_resolved_with_crossorigin(url, CrossOriginAttribute::Anonymous)
          .unwrap_or_else(|err| panic!("probe should succeed for {doc_url}: {err:?}"));
      }

      assert_eq!(
        fetcher.calls.load(Ordering::SeqCst),
        2,
        "expected first-time probes for distinct document origins to fetch separately"
      );

      // A new cache instance should be able to reuse the persisted artifacts. Each origin should be
      // resolved independently without triggering more network fetches.
      let mut cache_again = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
      for doc_url in ["http://a.test/page", "http://b.test/page"] {
        let mut ctx = ResourceContext::default();
        ctx.document_url = Some(doc_url.to_string());
        ctx.policy.document_origin = crate::resource::origin_from_url(doc_url);
        cache_again.set_resource_context(Some(ctx));
        cache_again
          .probe_resolved_with_crossorigin(url, CrossOriginAttribute::Anonymous)
          .unwrap_or_else(|err| panic!("probe artifact should succeed for {doc_url}: {err:?}"));
      }

      assert_eq!(
        fetcher.calls.load(Ordering::SeqCst),
        2,
        "expected probe artifacts to satisfy follow-up requests without additional fetches"
      );
    });
  }

  #[test]
  fn image_cache_partitions_by_referrer_url_for_no_cors_images() {
    let url = "https://img.test/pixel.png";
    let doc_a = "https://a.test/page";
    let doc_b = "https://b.test/page";

    let red_png = encode_single_pixel_png([255, 0, 0, 255]);
    let green_png = encode_single_pixel_png([0, 255, 0, 255]);

    let fetcher = ReferrerAwarePngFetcher::new(HashMap::from([
      (
        (Some(doc_a.to_string()), ReferrerPolicy::EmptyString),
        red_png.clone(),
      ),
      (
        (Some(doc_b.to_string()), ReferrerPolicy::EmptyString),
        green_png.clone(),
      ),
    ]));

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let mut ctx_a = ResourceContext::default();
    ctx_a.document_url = Some(doc_a.to_string());
    cache.set_resource_context(Some(ctx_a));

    let img_a = cache.load(url).expect("image load should succeed");
    let pix_a = img_a.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_a, [255, 0, 0, 255]);
    let pixmap_a = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_a.data()[..4], &[255, 0, 0, 255]);

    let mut ctx_b = ResourceContext::default();
    ctx_b.document_url = Some(doc_b.to_string());
    cache.set_resource_context(Some(ctx_b));

    let img_b = cache.load(url).expect("image load should succeed");
    let pix_b = img_b.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_b, [0, 255, 0, 255]);
    let pixmap_b = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_b.data()[..4], &[0, 255, 0, 255]);

    assert_eq!(
      fetcher.calls(),
      2,
      "expected referrer-partitioned image cache to fetch twice"
    );
  }

  #[test]
  fn image_cache_partitions_by_referrer_policy_for_no_cors_images() {
    let url = "https://img.test/pixel.png";
    let doc_url = "https://doc.test/page";

    let red_png = encode_single_pixel_png([255, 0, 0, 255]);
    let green_png = encode_single_pixel_png([0, 255, 0, 255]);

    let fetcher = ReferrerAwarePngFetcher::new(HashMap::from([
      (
        (Some(doc_url.to_string()), ReferrerPolicy::EmptyString),
        red_png.clone(),
      ),
      (
        (Some(doc_url.to_string()), ReferrerPolicy::NoReferrer),
        green_png.clone(),
      ),
    ]));

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let mut ctx_default = ResourceContext::default();
    ctx_default.document_url = Some(doc_url.to_string());
    cache.set_resource_context(Some(ctx_default));

    let img_default = cache.load(url).expect("image load should succeed");
    let pix_default = img_default.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_default, [255, 0, 0, 255]);
    let pixmap_default = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_default.data()[..4], &[255, 0, 0, 255]);

    let mut ctx_no_referrer = ResourceContext::default();
    ctx_no_referrer.document_url = Some(doc_url.to_string());
    ctx_no_referrer.referrer_policy = ReferrerPolicy::NoReferrer;
    cache.set_resource_context(Some(ctx_no_referrer));

    let img_no_referrer = cache.load(url).expect("image load should succeed");
    let pix_no_referrer = img_no_referrer.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_no_referrer, [0, 255, 0, 255]);
    let pixmap_no_referrer = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_no_referrer.data()[..4], &[0, 255, 0, 255]);

    assert_eq!(
      fetcher.calls(),
      2,
      "expected policy-partitioned image cache to fetch twice"
    );
  }

  #[test]
  fn http_403_image_reports_resource_error_with_status() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_403_image_reports_resource_error_with_status: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/blocked.png");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<html>Forbidden</html>";
      let response = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.load(&url) {
      Ok(_) => panic!("image load should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(403));
        assert_eq!(res.final_url.as_deref(), Some(url.as_str()));
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(403));
    assert_eq!(entry.final_url.as_deref(), Some(url.as_str()));

    server.join().unwrap();
  }

  #[test]
  fn http_200_html_for_jpg_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_200_html_for_jpg_is_reported_as_resource_error: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.load(&url) {
      Ok(_) => panic!("image load should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected content-type"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[test]
  fn http_200_html_for_jpg_with_image_mime_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_200_html_for_jpg_with_image_mime_is_reported_as_resource_error: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.load(&url) {
      Ok(_) => panic!("image load should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected markup"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[test]
  fn http_200_html_for_jpg_probe_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping http_200_html_for_jpg_probe_is_reported_as_resource_error: cannot bind localhost: {err}"
        );
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.probe(&url) {
      Ok(_) => panic!("image probe should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected content-type"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[test]
  fn http_200_html_for_jpg_with_image_mime_probe_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_200_html_for_jpg_with_image_mime_probe_is_reported_as_resource_error: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.probe(&url) {
      Ok(_) => panic!("image probe should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected markup"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[test]
  fn image_cache_load_about_blank_returns_transparent_placeholder() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called for about: URLs");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called for about: URLs");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let image = cache
      .load("about:blank")
      .expect("about:blank placeholder loads");
    assert!(!image.is_vector);
    assert_eq!(image.dimensions(), (1, 1));

    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.dimensions(), (1, 1));
    assert_eq!(rgba.get_pixel(0, 0).0, [0, 0, 0, 0]);
  }

  #[test]
  fn image_cache_load_empty_url_returns_transparent_placeholder() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called for empty URLs");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called for empty URLs");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let image = cache.load(" \t\r\n").expect("empty url placeholder loads");
    assert!(!image.is_vector);
    assert_eq!(image.dimensions(), (1, 1));

    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.dimensions(), (1, 1));
    assert_eq!(rgba.get_pixel(0, 0).0, [0, 0, 0, 0]);
  }

  #[test]
  fn image_cache_probe_about_blank_returns_placeholder_metadata() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called for about: URLs");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called for about: URLs");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let meta = cache
      .probe("about:blank")
      .expect("about:blank probe succeeds");
    assert!(!meta.is_vector);
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(
      meta.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(1.0)
    );
  }

  #[test]
  fn image_cache_load_empty_http_body_returns_placeholder_without_diagnostics() {
    #[derive(Clone)]
    struct EmptyBodyFetcher;

    impl ResourceFetcher for EmptyBodyFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Resource(
          crate::error::ResourceError::new(url, "empty HTTP response body").with_status(200),
        ))
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(EmptyBodyFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let image = cache
      .load("https://example.com/pixel.png")
      .expect("empty-body image loads as placeholder");
    assert_eq!(image.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(
      diag.fetch_errors.is_empty(),
      "placeholder images should not be recorded as fetch errors"
    );
  }

  #[test]
  fn image_cache_probe_empty_http_body_returns_placeholder_metadata_without_diagnostics() {
    #[derive(Clone)]
    struct EmptyBodyFetcher;

    impl ResourceFetcher for EmptyBodyFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Resource(
          crate::error::ResourceError::new(url, "empty HTTP response body").with_status(200),
        ))
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(EmptyBodyFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let meta = cache
      .probe("https://example.com/pixel.png")
      .expect("empty-body image probe returns placeholder");
    assert_eq!(meta.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(diag.fetch_errors.is_empty());
    assert!(
      diag.invalid_images.is_empty(),
      "empty-body placeholder images should not be treated as invalid images"
    );
  }

  #[test]
  fn image_cache_html_payload_returns_placeholder_without_diagnostics() {
    #[derive(Clone)]
    struct HtmlFetcher;

    impl ResourceFetcher for HtmlFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        let mut res = FetchedResource::new(
          b"<!doctype html><html><head></head><body>not an image</body></html>".to_vec(),
          Some("image/png".to_string()),
        );
        res.status = Some(200);
        Ok(res)
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HtmlFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let image = cache
      .load("https://example.com/not-really.html")
      .expect("HTML body treated as placeholder image");
    assert_eq!(image.dimensions(), (1, 1));

    let meta = cache
      .probe("https://example.com/not-really.html")
      .expect("HTML body treated as placeholder metadata");
    assert_eq!(meta.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(diag.fetch_errors.is_empty());
    assert!(
      diag
        .invalid_images
        .iter()
        .any(|u| u == "https://example.com/not-really.html"),
      "invalid image URLs should be tracked in diagnostics.invalid_images"
    );
  }

  #[test]
  fn image_cache_html_payload_with_206_status_returns_placeholder_without_diagnostics() {
    #[derive(Clone)]
    struct PartialHtmlFetcher;

    impl ResourceFetcher for PartialHtmlFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("full fetch should not be required for HTML payload probe");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        let mut res = FetchedResource::new(
          b"<!doctype html><html><body>partial content</body></html>".to_vec(),
          Some("image/png".to_string()),
        );
        res.status = Some(206);
        Ok(res)
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(PartialHtmlFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let meta = cache
      .probe("https://example.com/not-an-image.html")
      .expect("HTML probe returns placeholder metadata");
    assert_eq!(meta.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(diag.fetch_errors.is_empty());
    assert!(
      diag
        .invalid_images
        .iter()
        .any(|u| u == "https://example.com/not-an-image.html"),
      "invalid image URLs should be tracked in diagnostics.invalid_images"
    );
  }

  fn padded_png() -> Vec<u8> {
    // 1x1 RGBA PNG, padded with trailing bytes so that the probe must use a prefix fetch.
    const PNG_1X1: &[u8] = &[
      0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
      0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xB5,
      0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xFC,
      0xFF, 0x1F, 0x00, 0x03, 0x03, 0x01, 0x02, 0x94, 0x60, 0xC4, 0x1B, 0x00, 0x00, 0x00, 0x00,
      0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let mut bytes = PNG_1X1.to_vec();
    bytes.resize(128 * 1024, 0);
    bytes
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_persists_metadata_to_disk_cache_and_reuses_it() {
    use crate::resource::{CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct CountingPartialFetcher {
      calls: Arc<AtomicUsize>,
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CountingPartialFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected partial fetch only");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/probe.png";
    let body = Arc::new(padded_png());

    let calls = Arc::new(AtomicUsize::new(0));
    let disk = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk));
    let meta = cache.probe(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "first probe should hit the network via fetch_partial"
    );

    let calls2 = Arc::new(AtomicUsize::new(0));
    let disk2 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls2),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache2 = ImageCache::with_fetcher(Arc::new(disk2));
    let meta2 = cache2.probe(url).expect("probe succeeds from disk");
    assert_eq!(meta2.dimensions(), (1, 1));
    assert_eq!(
      calls2.load(Ordering::SeqCst),
      0,
      "second probe should reuse persisted probe metadata without network calls"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_resolved_reuses_persisted_metadata_to_disk_cache() {
    use crate::resource::{CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct CountingPartialFetcher {
      calls: Arc<AtomicUsize>,
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CountingPartialFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected partial fetch only");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/probe_resolved.png";
    let body = Arc::new(padded_png());

    let calls = Arc::new(AtomicUsize::new(0));
    let disk = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk));
    let meta = cache.probe_resolved(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "first resolved probe should hit the network via fetch_partial"
    );

    let calls2 = Arc::new(AtomicUsize::new(0));
    let disk2 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls2),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache2 = ImageCache::with_fetcher(Arc::new(disk2));
    let meta2 = cache2
      .probe_resolved(url)
      .expect("probe succeeds from disk");
    assert_eq!(meta2.dimensions(), (1, 1));
    assert_eq!(
      calls2.load(Ordering::SeqCst),
      0,
      "second resolved probe should reuse persisted probe metadata without network calls"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_artifact_inherits_stored_at_from_cached_resource() {
    use crate::resource::{
      CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher, FetchDestination, FetchRequest,
    };
    use std::fs;
    use std::sync::Arc;

    #[derive(Clone)]
    struct FullFetcher {
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for FullFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        Ok(res)
      }
    }

    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("network fetch should not be called");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("network partial fetch should not be called");
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/aged.png";
    let body = Arc::new(padded_png());

    // Persist the full image bytes into the disk cache so the probe can later derive metadata from
    // disk without touching the network.
    let disk = DiskCachingFetcher::with_configs(
      FullFetcher {
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    disk
      .fetch_with_request(FetchRequest::new(url, FetchDestination::Image))
      .expect("seed fetch");

    // Locate the primary cached image entry and force its `stored_at` to be very old so we can
    // assert the derived probe artifact inherits that age rather than refreshing it.
    let mut resource_meta_path = None;
    for entry in fs::read_dir(&cache_dir).expect("read cache dir") {
      let path = entry.expect("dir entry").path();
      if !path.to_string_lossy().ends_with(".bin.meta") {
        continue;
      }
      let bytes = fs::read(&path).expect("read meta");
      let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse meta json");
      let ct = value
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if ct == "image/png" {
        resource_meta_path = Some(path);
        break;
      }
    }
    let resource_meta_path = resource_meta_path.expect("cached image meta file");
    let meta_bytes = fs::read(&resource_meta_path).expect("read meta bytes");
    let mut value: serde_json::Value =
      serde_json::from_slice(&meta_bytes).expect("parse meta json");
    value["stored_at"] = serde_json::Value::from(0u64);
    fs::write(
      &resource_meta_path,
      serde_json::to_vec(&value).expect("serialize meta"),
    )
    .expect("write meta");

    // New fetcher instance (empty memory cache). The probe should be satisfied entirely from disk.
    let disk2 = DiskCachingFetcher::with_configs(
      PanicFetcher,
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk2));
    let meta = cache.probe(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    // The persisted probe metadata should inherit the primary resource `stored_at` timestamp so it
    // becomes stale alongside the cached image bytes.
    let mut probe_meta_path = None;
    for entry in fs::read_dir(&cache_dir).expect("read cache dir") {
      let path = entry.expect("dir entry").path();
      if !path.to_string_lossy().ends_with(".bin.meta") {
        continue;
      }
      let bytes = fs::read(&path).expect("read meta");
      let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse meta json");
      let ct = value
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if ct == "application/x-fastrender-image-probe+json" {
        probe_meta_path = Some(path);
        break;
      }
    }
    let probe_meta_path = probe_meta_path.expect("probe meta file");
    let probe_bytes = fs::read(&probe_meta_path).expect("read probe meta");
    let probe_value: serde_json::Value =
      serde_json::from_slice(&probe_bytes).expect("parse probe meta json");
    assert_eq!(
      probe_value.get("stored_at").and_then(|v| v.as_u64()),
      Some(0),
      "probe metadata should inherit stored_at from the cached resource"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_disk_cache_respects_staleness_and_recovers_from_corruption() {
    use crate::resource::{CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone)]
    struct CountingPartialFetcher {
      calls: Arc<AtomicUsize>,
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CountingPartialFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected partial fetch only");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/stale.png";
    let body = Arc::new(padded_png());

    let calls = Arc::new(AtomicUsize::new(0));
    let disk = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        max_age: Some(Duration::from_secs(1)),
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk));
    let meta = cache.probe(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Locate the probe-metadata cache entry and force it to be stale by setting stored_at=0.
    let mut meta_path = None;
    for entry in fs::read_dir(&cache_dir).expect("read cache dir") {
      let path = entry.expect("dir entry").path();
      if !path.to_string_lossy().ends_with(".bin.meta") {
        continue;
      }
      let bytes = fs::read(&path).expect("read meta");
      let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse meta json");
      let ct = value
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if ct == "application/x-fastrender-image-probe+json" {
        meta_path = Some(path);
        break;
      }
    }
    let meta_path = meta_path.expect("probe metadata entry meta file");
    let meta_bytes = fs::read(&meta_path).expect("read meta bytes");
    let mut value: serde_json::Value =
      serde_json::from_slice(&meta_bytes).expect("parse meta json");
    value["stored_at"] = serde_json::Value::from(0u64);
    fs::write(
      &meta_path,
      serde_json::to_vec(&value).expect("serialize meta"),
    )
    .expect("write meta");

    let calls2 = Arc::new(AtomicUsize::new(0));
    let disk2 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls2),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        max_age: Some(Duration::from_secs(1)),
        ..DiskCacheConfig::default()
      },
    );
    let cache2 = ImageCache::with_fetcher(Arc::new(disk2));
    let meta2 = cache2.probe(url).expect("probe succeeds after staleness");
    assert_eq!(meta2.dimensions(), (1, 1));
    assert_eq!(
      calls2.load(Ordering::SeqCst),
      1,
      "stale probe metadata should trigger a network refresh"
    );

    // Corrupt the on-disk probe data while keeping the length intact, then ensure we can recover.
    let len = value.get("len").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let meta_string = meta_path.to_string_lossy();
    let data_path =
      std::path::PathBuf::from(meta_string.strip_suffix(".meta").expect("meta path suffix"));
    fs::write(&data_path, vec![0u8; len.max(1)]).expect("write corrupt data");

    let calls3 = Arc::new(AtomicUsize::new(0));
    let disk3 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls3),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        max_age: Some(Duration::from_secs(1)),
        ..DiskCacheConfig::default()
      },
    );
    let cache3 = ImageCache::with_fetcher(Arc::new(disk3));
    let meta3 = cache3.probe(url).expect("probe succeeds after corruption");
    assert_eq!(meta3.dimensions(), (1, 1));
    assert_eq!(
      calls3.load(Ordering::SeqCst),
      1,
      "corrupt probe metadata should be treated as a miss and refreshed"
    );
  }

  fn read_http_headers(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;

    let mut buf = Vec::new();
    let mut scratch = [0u8; 1024];
    loop {
      match stream.read(&mut scratch) {
        Ok(0) => break,
        Ok(n) => {
          buf.extend_from_slice(&scratch[..n]);
          if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 32 * 1024 {
            break;
          }
        }
        Err(_) => break,
      }
    }
    String::from_utf8_lossy(&buf).to_string()
  }

  fn trim_http_whitespace(value: &str) -> &str {
    value.trim_matches(|c: char| matches!(c, ' ' | '\t'))
  }

  fn extract_range_header(req: &str) -> Option<String> {
    req.lines().find_map(|line| {
      let line = line.trim_end_matches('\r');
      let (name, value) = line.split_once(':')?;
      if trim_http_whitespace(name).eq_ignore_ascii_case("range") {
        Some(trim_http_whitespace(value).to_string())
      } else {
        None
      }
    })
  }

  fn parse_range_end(range: &str) -> Option<usize> {
    let range = trim_http_whitespace(range);
    let range = range.strip_prefix("bytes=")?;
    let (_start, end) = range.split_once('-')?;
    trim_http_whitespace(end).parse::<usize>().ok()
  }

  #[test]
  fn image_probe_uses_http_range_requests() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping image_probe_uses_http_range_requests: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/probe.png");
    let body = padded_png();

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let req = read_http_headers(&mut stream);

      let range = extract_range_header(&req).expect("missing Range header");
      let end = parse_range_end(&range).expect("invalid Range header");
      let prefix_len = (end + 1).min(body.len());
      let prefix = &body[..prefix_len];

      let header = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: image/png\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
        prefix.len(),
        prefix.len().saturating_sub(1),
        body.len()
      );
      stream.write_all(header.as_bytes()).expect("write header");
      stream.write_all(prefix).expect("write body");
      let _ = stream.flush();
    });

    let cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    let meta = cache.probe(&url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    server.join().unwrap();
  }

  #[test]
  fn image_probe_partial_fetch_handles_range_ignored() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping image_probe_partial_fetch_handles_range_ignored: cannot bind localhost: {err}"
        );
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/probe.png");
    let body = padded_png();

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let req = read_http_headers(&mut stream);

      let range = extract_range_header(&req).expect("missing Range header");
      let end = parse_range_end(&range).expect("invalid Range header");
      let prefix_len = (end + 1).min(body.len());
      let prefix = &body[..prefix_len];

      // Ignore Range and respond with 200 + the full content-length. Send only the prefix
      // immediately; if the client tried to read the entire body it would stall and hit the
      // request timeout below.
      let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(header.as_bytes()).expect("write header");
      stream.write_all(prefix).expect("write prefix");
      let _ = stream.flush();
      std::thread::sleep(Duration::from_millis(500));
      let _ = stream.write_all(&body[prefix_len..]);
      let _ = stream.flush();
    });

    let cache = ImageCache::with_fetcher(Arc::new(
      HttpFetcher::new().with_timeout(Duration::from_millis(150)),
    ));
    let meta = cache.probe(&url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    server.join().unwrap();
  }

  #[test]
  fn image_probe_partial_fetch_falls_back_on_http_405() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping image_probe_partial_fetch_falls_back_on_http_405: cannot bind localhost: {err}"
        );
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/probe.png");
    let body = padded_png();

    let server = std::thread::spawn(move || {
      // First request should be the partial probe with a Range header.
      {
        let (mut stream, _) = listener.accept().expect("accept range request");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let req = read_http_headers(&mut stream);
        assert!(
          extract_range_header(&req).is_some(),
          "expected Range header on probe request"
        );

        let header = "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(header.as_bytes()).expect("write header");
        let _ = stream.flush();
      }

      // Second request should be the fallback full fetch (no Range header).
      {
        let (mut stream, _) = listener.accept().expect("accept full request");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let req = read_http_headers(&mut stream);
        assert!(
          extract_range_header(&req).is_none(),
          "unexpected Range header on fallback full fetch"
        );

        let header = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        stream.write_all(header.as_bytes()).expect("write header");
        stream.write_all(&body).expect("write body");
        let _ = stream.flush();
      }
    });

    enable_image_cache_diagnostics();
    let cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    let meta = cache.probe(&url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    let stats = take_image_cache_diagnostics().expect("diagnostics enabled");
    assert_eq!(stats.probe_partial_requests, 1);
    assert_eq!(stats.probe_partial_fallback_full, 1);

    server.join().unwrap();
  }

  #[test]
  fn svgz_load_with_svg_content_type() {
    let url = "https://example.com/icon.svg";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("image/svg+xml".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let img = cache
      .load(url)
      .expect("load svgz content via svg content-type");
    assert!(img.is_vector);
    assert_eq!(img.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_load_with_octet_stream_content_type() {
    let url = "https://example.com/icon.SVGZ?version=1";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let img = cache
      .load(url)
      .expect("load svgz content via .svgz URL + octet-stream content-type");
    assert!(img.is_vector);
    assert_eq!(img.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_probe_metadata() {
    let url = "https://example.com/icon.svgz";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let meta = cache
      .probe_resolved(url)
      .expect("probe svgz content should succeed");
    assert!(meta.is_vector);
    assert_eq!(meta.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_load_uses_final_url_suffix() {
    let requested = "https://example.com/icon";
    let final_url = "https://example.com/icon.svgz?redirect=1";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    res.final_url = Some(final_url.to_string());
    let fetcher = MapFetcher::with_entries([(requested.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let img = cache
      .load(requested)
      .expect("load svgz content via final URL .svgz suffix");
    assert!(img.is_vector);
    assert_eq!(img.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_probe_uses_final_url_suffix() {
    let requested = "https://example.com/icon";
    let final_url = "https://example.com/icon.svgz?redirect=1";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    res.final_url = Some(final_url.to_string());
    let fetcher = MapFetcher::with_entries([(requested.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let meta = cache
      .probe_resolved(requested)
      .expect("probe svgz content via final URL .svgz suffix");
    assert!(meta.is_vector);
    assert_eq!(meta.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_gzipped_non_svg_payload_is_not_misidentified() {
    let url = "https://example.com/bad.svgz";
    let svgz = gzip_bytes(b"not an svg");

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    assert!(
      cache.load(url).is_err(),
      "gzipped non-svg payload must not decode as SVG"
    );
    assert!(
      cache.probe_resolved(url).is_err(),
      "gzipped non-svg payload must not probe as SVG"
    );
  }

  #[test]
  fn svg_viewbox_renders_with_default_letterboxing() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left padding");
    assert_eq!(left.alpha(), 0, "letterboxed area should be transparent");

    let center = pixmap.pixel(100, 50).expect("center pixel");
    assert_eq!(
      (center.red(), center.green(), center.blue(), center.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_viewbox_none_stretches_to_viewport() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100' preserveAspectRatio='none'><rect width='100' height='100' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left pixel");
    assert_eq!(
      (left.red(), left.green(), left.blue(), left.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_viewbox_aligns_min_min_meet() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100' preserveAspectRatio='xMinYMin meet'><rect width='100' height='100' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left pixel");
    assert_eq!(left.alpha(), 255);

    let right = pixmap.pixel(190, 50).expect("right padding");
    assert_eq!(right.alpha(), 0);
  }

  #[test]
  fn svg_viewbox_slice_fills_viewport() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100' preserveAspectRatio='xMidYMid slice'><circle cx='50' cy='50' r='50' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left pixel");
    assert_eq!(left.alpha(), 255);
    assert_eq!(left.red(), 255);
  }

  #[test]
  fn simple_svg_fast_path_stroke_renders() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 0 L10 10" stroke="red" stroke-width="2" fill="none"/></svg>"#;
    let pixmap = try_render_simple_svg_pixmap(svg, 20, 20)
      .expect("render should not error")
      .expect("expected simple SVG to use fast-path");

    let pixel = pixmap.pixel(10, 10).expect("diagonal pixel");
    assert!(
      pixel.alpha() > 0,
      "expected diagonal stroke pixel to be non-transparent"
    );
    assert_eq!(pixel.green(), 0);
    assert_eq!(pixel.blue(), 0);
    assert!(
      pixel.red().abs_diff(pixel.alpha()) <= 1,
      "expected premultiplied red to match alpha (got rgba=({}, {}, {}, {}))",
      pixel.red(),
      pixel.green(),
      pixel.blue(),
      pixel.alpha()
    );
  }

  #[test]
  fn simple_svg_fast_path_dasharray_renders() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 5 L10 5" stroke="red" stroke-width="2" stroke-dasharray="2 2" fill="none"/></svg>"#;
    let pixmap = try_render_simple_svg_pixmap(svg, 20, 20)
      .expect("render should not error")
      .expect("expected simple SVG to use fast-path");

    let dash = pixmap.pixel(2, 10).expect("dash pixel");
    assert!(
      dash.alpha() > 0,
      "expected dash pixel to be non-transparent"
    );
    assert_eq!(dash.green(), 0);
    assert_eq!(dash.blue(), 0);
    assert!(
      dash.red().abs_diff(dash.alpha()) <= 1,
      "expected premultiplied red to match alpha (got rgba=({}, {}, {}, {}))",
      dash.red(),
      dash.green(),
      dash.blue(),
      dash.alpha()
    );

    let gap = pixmap.pixel(6, 10).expect("gap pixel");
    assert_eq!(
      (gap.red(), gap.green(), gap.blue(), gap.alpha()),
      (0, 0, 0, 0),
      "expected dash gap pixel to be transparent"
    );
  }

  #[test]
  fn simple_svg_fast_path_rejects_filter_attribute() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 0 L10 10" stroke="red" stroke-width="2" fill="none" filter="url(#f)"/></svg>"#;
    assert!(
      try_render_simple_svg_pixmap(svg, 20, 20)
        .expect("render should not error")
        .is_none(),
      "expected SVG filter attribute to force slow-path fallback"
    );
  }

  #[test]
  fn svg_width_height_set_intrinsic_size_and_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='200' height='100'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    assert_eq!(img.width(), 200);
    assert_eq!(img.height(), 100);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
    assert!(!img.aspect_ratio_none);
  }

  #[test]
  fn svg_viewbox_defaults_to_300x150_with_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 40 20'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    assert_eq!(img.width(), 300);
    assert_eq!(img.height(), 150);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
    assert!(!img.aspect_ratio_none);
  }

  #[test]
  fn svg_viewbox_square_defaults_to_300x150_in_probe() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#;

    let meta = cache
      .probe_svg_content(svg, "inline viewBox-only svg")
      .expect("probe svg");
    assert_eq!(meta.width, 300);
    assert_eq!(meta.height, 150);
    assert_eq!(meta.intrinsic_ratio, Some(1.0));
    assert!(!meta.aspect_ratio_none);
  }

  #[test]
  fn svg_viewbox_square_defaults_to_300x150_in_render_svg_to_image() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#;

    let (image, ratio, aspect_none) = cache.render_svg_to_image(svg).expect("render svg");
    assert_eq!((image.width(), image.height()), (300, 150));
    assert_eq!(ratio, Some(1.0));
    assert!(!aspect_none);
  }

  #[test]
  fn render_svg_to_image_viewbox_only_defaults_to_300x150() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' fill='red'/></svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    assert_eq!((image.width(), image.height()), (300, 150));

    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(150, 75).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(10, 75).0[3], 0, "left padding should be transparent");
    assert_eq!(
      rgba.get_pixel(290, 75).0[3],
      0,
      "right padding should be transparent"
    );
  }

  #[test]
  fn probe_svg_content_viewbox_only_defaults_to_300x150() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' fill='red'/></svg>";

    let meta = cache
      .probe_svg_content(svg, "test://svg")
      .expect("probe svg content");
    assert_eq!(meta.width, 300);
    assert_eq!(meta.height, 150);
    assert_eq!(meta.intrinsic_ratio, Some(1.0));
  }

  #[test]
  fn render_svg_to_image_preserve_aspect_ratio_alignment_respected() {
    let cache = ImageCache::new();

    let svg_min = "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='150' viewBox='0 0 100 100' preserveAspectRatio='xMinYMin meet'><rect width='100' height='100' fill='red'/></svg>";
    let (image, _, _) = cache.render_svg_to_image(svg_min).expect("render svg xMin");
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 75).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(290, 75).0[3], 0);

    let svg_max = "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='150' viewBox='0 0 100 100' preserveAspectRatio='xMaxYMin meet'><rect width='100' height='100' fill='red'/></svg>";
    let (image, _, _) = cache.render_svg_to_image(svg_max).expect("render svg xMax");
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 75).0[3], 0);
    assert_eq!(rgba.get_pixel(290, 75).0, [255, 0, 0, 255]);
  }

  #[test]
  fn render_svg_to_image_preserve_aspect_ratio_slice_respected() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='150' viewBox='0 0 100 100' preserveAspectRatio='xMidYMid slice'>\
      <rect x='0' y='0' width='100' height='10' fill='red'/>\
      <rect x='0' y='45' width='100' height='10' fill='blue'/>\
      <rect x='0' y='90' width='100' height='10' fill='green'/>\
    </svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg slice");
    let rgba = image.to_rgba8();

    assert_eq!(rgba.get_pixel(150, 5).0[3], 0);
    assert_eq!(rgba.get_pixel(150, 75).0, [0, 0, 255, 255]);
    assert_eq!(rgba.get_pixel(150, 145).0[3], 0);
  }

  #[test]
  fn render_svg_to_image_viewbox_min_xy_translation_respected() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100' viewBox='50 50 100 100'>\
      <rect x='50' y='50' width='100' height='100' fill='red'/>\
    </svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 10).0, [255, 0, 0, 255]);
  }

  #[test]
  fn svg_preserve_aspect_ratio_none_disables_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 50' preserveAspectRatio='none'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    assert_eq!(img.width(), 300);
    assert_eq!(img.height(), 150);
    assert!(img.aspect_ratio_none);
    assert_eq!(img.intrinsic_ratio(OrientationTransform::IDENTITY), None);
  }

  #[test]
  fn svg_fast_path_without_viewbox_scales_non_uniformly() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
      <path d='M0 0 H100 V100 H0 Z' fill='red'/>\
    </svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://fast-path", 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(10, 50).expect("pixel");

    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_percent_width_height_ignored_defaults_with_viewbox_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='50%' height='25%' viewBox='0 0 200 100'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    // Percent lengths are ignored; fall back to 300x150 but keep the viewBox ratio (2:1).
    assert_eq!(img.width(), 300);
    assert_eq!(img.height(), 150);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
  }

  #[test]
  fn svg_absolute_units_convert_to_px() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='1in' height='0.5in'></svg>";

    let meta = cache
      .probe_svg_content(svg, "inline inches")
      .expect("probe inches svg");
    assert_eq!(meta.width, 96);
    assert_eq!(meta.height, 48);

    let img = cache.render_svg(svg).expect("rendered");
    assert_eq!(img.width(), 96);
    assert_eq!(img.height(), 48);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
  }

  #[test]
  fn svg_em_units_use_default_font_size_when_probing() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='1em' height='1em'></svg>";

    let meta = cache
      .probe_svg_content(svg, "inline em")
      .expect("probe em svg");
    assert_eq!(meta.width, 16);
    assert_eq!(meta.height, 16);
  }

  #[test]
  fn svg_metric_units_convert_to_px() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='2.54cm' height='25.4mm'></svg>";

    let meta = cache
      .probe_svg_content(svg, "inline metric")
      .expect("probe metric svg");
    assert_eq!(meta.width, 96);
    assert_eq!(meta.height, 96);

    let img = cache.render_svg(svg).expect("rendered");
    assert_eq!(img.width(), 96);
    assert_eq!(img.height(), 96);
  }

  #[test]
  fn render_inline_svg_returns_image() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="5"></svg>"#;
    let (image, ratio, aspect_none) = cache.render_svg_to_image(svg).expect("svg render");
    assert_eq!(image.width(), 10);
    assert_eq!(image.height(), 5);
    assert_eq!(ratio, Some(2.0));
    assert!(!aspect_none);
  }

  #[test]
  fn non_ascii_whitespace_svg_text_looks_like_markup_does_not_trim_nbsp() {
    assert!(svg_text_looks_like_markup("<svg></svg>"));
    assert!(svg_text_looks_like_markup("\n\t<svg></svg>"));
    assert!(!svg_text_looks_like_markup("\u{00A0}<svg></svg>"));
  }

  #[test]
  fn render_svg_to_image_width_only_viewbox_preserves_aspect_ratio() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' width='200' viewBox='0 0 100 100'>
        <rect x='0' y='0' width='100' height='100' fill='red'/>
      </svg>
    "#;
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    assert_eq!((image.width(), image.height()), (200, 200));
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(100, 10).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(100, 100).0, [255, 0, 0, 255]);
  }

  #[test]
  fn render_svg_to_image_respects_preserve_aspect_ratio_none_when_scaling() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' width='200' viewBox='0 0 100 100' preserveAspectRatio='none'>
        <rect x='0' y='0' width='100' height='100' fill='red'/>
      </svg>
    "#;
    let (image, _, aspect_none) = cache.render_svg_to_image(svg).expect("render svg");

    assert!(aspect_none);
    assert_eq!((image.width(), image.height()), (200, 150));
    let rgba = image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(100, 10).0,
      [255, 0, 0, 255],
      "preserveAspectRatio='none' should stretch to the top edge"
    );
  }

  #[test]
  fn render_svg_to_image_height_only_viewbox_preserves_aspect_ratio() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' height='200' viewBox='0 0 100 100'>
        <rect x='0' y='0' width='100' height='100' fill='red'/>
      </svg>
    "#;
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    assert_eq!((image.width(), image.height()), (200, 200));
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 100).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(100, 100).0, [255, 0, 0, 255]);
  }

  #[test]
  fn load_svg_data_url() {
    let cache = ImageCache::new();
    let data_url =
            "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E";
    let image = cache.load(data_url).expect("decode data URL");
    assert_eq!(image.width(), 1);
    assert_eq!(image.height(), 1);
  }

  #[test]
  fn svg_fragment_identifier_renders_symbol_sprite() {
    let fetch_url = "https://example.test/sprite.svg";
    let url = "https://example.test/sprite.svg#icon";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><symbol id="icon" viewBox="0 0 1 1"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(fetch_url.to_string());

    let fetcher = MapFetcher::with_entries([(fetch_url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let image = cache.load(url).expect("load svg fragment");
    assert!(image.is_vector);
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "symbol-only sprite should render when fragment identifier is applied"
    );

    let pixmap = cache
      .render_svg_pixmap_at_size(sprite_svg, 1, 1, url, 1.0)
      .expect("render svg fragment pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixmap pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let meta = cache.probe(url).expect("probe svg fragment");
    assert!(meta.is_vector);
    assert_eq!((meta.width, meta.height), (1, 1));

    for (req_url, _, _) in fetcher.requests() {
      assert!(
        !req_url.contains('#'),
        "SVG fetches must strip fragments; got request url {req_url}"
      );
    }
  }

  #[test]
  fn svg_fragment_identifier_renders_defs_g_element() {
    let fetch_url = "https://example.test/sprite.svg";
    let url = "https://example.test/sprite.svg#icon";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><defs><g id="icon"><rect width="1" height="1" fill="red"/></g></defs></svg>"#;

    let mut res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(fetch_url.to_string());

    let fetcher = MapFetcher::with_entries([(fetch_url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(url).expect("load svg fragment");
    assert!(image.is_vector);
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "<g> sprite inside <defs> should render when fragment identifier is applied"
    );

    let pixmap = cache
      .render_svg_pixmap_at_size(sprite_svg, 1, 1, url, 1.0)
      .expect("render svg fragment pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixmap pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_external_use_sprite_renders() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <use href> sprite should inline and render red pixel"
    );
  }

  #[test]
  fn svg_external_use_sprite_renders_with_xlink_href() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use xlink:href="/sprite.svg#icon" xmlns:xlink="http://www.w3.org/1999/xlink"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <use xlink:href> sprite should inline and render red pixel"
    );
  }

  #[test]
  fn svg_external_use_sprite_svgz_renders() {
    let sprite_url = "https://example.test/sprite.svgz";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let sprite_svgz = gzip_bytes(sprite_svg.as_bytes());
    let main_svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svgz#icon"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res =
      FetchedResource::new(sprite_svgz, Some("application/octet-stream".to_string()));
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <use> sprite should inline and render red pixel when sprite is gzipped"
    );
  }

  #[test]
  fn svg_external_image_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";
 
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;
 
    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());
 
    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());
 
    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));
 
    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_image_href_renders_with_child_elements() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"><desc>hi</desc></image></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should inline even when <image> has child elements"
    );
  }

  #[test]
  fn svg_external_feimage_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
        <filter id="f" x="0" y="0" width="1" height="1" filterUnits="userSpaceOnUse">
          <feImage href="/img.png" x="0" y="0" width="1" height="1" preserveAspectRatio="none"/>
        </filter>
        <rect width="1" height="1" filter="url(#f)"/>
      </svg>
    "#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <feImage href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_image_xlink_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1" height="1"><image xlink:href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image xlink:href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_image_href_fragment_preserved() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png#frag" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should inline and render even when the URL has a fragment"
    );

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|(url, _, _)| url == img_url),
      "expected fragment-less subresource fetch request"
    );
    assert!(
      requests
        .iter()
        .all(|(url, _, _)| url != &format!("{img_url}#frag")),
      "fetch URL must not include fragments"
    );
  }

  #[test]
  fn svg_external_image_href_renders_without_content_type() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    // Some endpoints omit Content-Type for images. The SVG preprocessor should sniff common formats
    // so the generated `data:` URL uses an image/* mime that resvg/usvg accept.
    let mut img_res = FetchedResource::new(encode_single_pixel_png([255, 0, 0, 255]), None);
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should render even when the fetched image has no Content-Type"
    );
  }

  #[test]
  fn svg_external_image_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let img_url = "https://cross.test/img.png";
 
    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="{img_url}" width="1" height="1"/></svg>"#
    );
 
    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());
 
    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());
 
    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
 
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));
 
    let load_err = match cache.load(main_url) {
      Ok(_) => panic!("cross-origin SVG image href should be blocked"),
      Err(err) => err,
    };
    match load_err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
 
    let probe_err = match cache.probe(main_url) {
      Ok(_) => panic!("cross-origin SVG image href should be blocked during probe"),
      Err(err) => err,
    };
    match probe_err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
 
    assert!(
      fetcher.requests().iter().all(|(url, _, _)| url != img_url),
      "blocked image should not be fetched"
    );
  }

  #[test]
  fn svg_anchor_href_not_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let cross_url = "https://cross.test/";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><a href="{cross_url}"><rect width="1" height="1" fill="red"/></a></svg>"#
    );

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let fetcher = MapFetcher::with_entries([(main_url.to_string(), main_res)]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let image = cache.load(main_url).expect("SVG with <a href> should not be blocked");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "<a href> hyperlink should not affect rendered output"
    );

    assert!(
      fetcher.requests().iter().all(|(url, _, _)| url != cross_url),
      "hyperlink should not be fetched"
    );
  }

  #[test]
  fn non_ascii_whitespace_inline_svg_use_references_does_not_trim_nbsp_in_sprite_id() {
    let nbsp = "\u{00A0}";
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="{nbsp}icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#
    );
    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;
    let expanded = inline_svg_use_references(main_svg, main_url, &fetcher, None).expect("expand");
    assert_eq!(
      expanded.as_ref(),
      main_svg,
      "NBSP must not be treated as whitespace when indexing sprite ids for <use> expansion"
    );
  }

  #[test]
  fn svg_external_use_sprite_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let sprite_url = "https://cross.test/sprite.svg";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="{sprite_url}#icon"/></svg>"#
    );
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let err = match cache.load(main_url) {
      Ok(_) => panic!("cross-origin sprite should be blocked"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    assert!(
      fetcher.requests().iter().all(|(url, _, _)| url != sprite_url),
      "blocked sprite should not be fetched"
    );
  }

  #[test]
  fn svg_external_use_sprite_redirect_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let sprite_url = "https://example.test/sprite.svg";
    let sprite_final_url = "https://cross.test/sprite.svg";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_final_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let err = match cache.load(main_url) {
      Ok(_) => panic!("redirected cross-origin sprite should be blocked"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
        assert!(
          reason.contains(sprite_final_url),
          "policy reason should mention final URL: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn inline_svg_external_use_sprite_uses_document_url_as_base() {
    let doc_url = "https://example.test/page.html";
    let sprite_url = "https://example.test/sprite.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_external_use_sprite_uses_xml_base_as_base() {
    let doc_url = "https://example.test/page.html";
    let sprite_url = "https://example.test/assets/sprite.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="assets/" width="1" height="1"><use href="sprite.svg#icon"/></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == sprite_url && *dest == FetchDestination::Image),
      "expected fetch for xml:base sprite href {sprite_url}, got: {requests:?}"
    );

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_external_use_sprite_inlines_sprite_images_with_sprite_base() {
    let doc_url = "https://example.test/page.html";
    let sprite_url = "https://example.test/assets/sprite.svg";
    let img_url = "https://example.test/assets/img.png";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><image href="img.png" width="1" height="1"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/assets/sprite.svg#icon"/></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (sprite_url.to_string(), sprite_res),
      (img_url.to_string(), img_res),
    ]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == sprite_url && *dest == FetchDestination::Image),
      "expected fetch for sprite URL {sprite_url}, got: {requests:?}"
    );
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for sprite-nested image URL {img_url}, got: {requests:?}"
    );

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_external_image_href_is_fetched_and_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res =
      FetchedResource::new(main_svg.as_bytes().to_vec(), Some("image/svg+xml".to_string()));
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);

    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let image = cache.load(main_url).expect("main svg should render");
    assert_eq!(image.dimensions(), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 255]);

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for image href {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn inline_svg_external_image_uses_document_url_as_base() {
    let doc_url = "https://example.test/page.html";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for inline svg image href {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn inline_svg_external_image_uses_xml_base_as_base() {
    let doc_url = "https://example.test/page.html";
    let img_url = "https://example.test/assets/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="assets/" width="1" height="1"><image href="img.png" width="1" height="1"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for xml:base image href {img_url}, got: {requests:?}"
    );

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_external_fe_image_href_is_inlined() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><defs><filter id="f"><feImage href="/img.png" x="0" y="0" width="1" height="1"/></filter></defs><rect width="1" height="1" filter="url(#f)"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let inlined = inline_svg_image_references(svg, main_url, &fetcher, None).expect("inlined svg");
    assert!(
      inlined.as_ref().contains("data:image/png;base64,"),
      "expected data URL rewrite, got: {}",
      inlined.as_ref()
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for feImage href {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_policy_blocks_external_image_during_render() {
    let doc_url = "https://doc.test/";
    let blocked_url = "https://cross.test/a.png";

    let fetcher = MapFetcher::default();
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="https://cross.test/a.png" width="1" height="1"/></svg>"#;

    let err = cache
      .render_svg_pixmap_at_size(svg, 1, 1, "inline-svg", 1.0)
      .expect_err("expected policy block during render");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, blocked_url);
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    let requests = fetcher.requests();
    assert!(
      requests.is_empty(),
      "fetcher should not be called for policy-blocked URLs, got: {requests:?}"
    );
  }

  #[test]
  fn svg_policy_scan_respects_xml_base() {
    let doc_url = "https://doc.test/";
    let svg_url = "https://doc.test/main.svg";
    let blocked_url = "https://cross.test/a.png";

    let fetcher = MapFetcher::default();
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="https://cross.test/" width="1" height="1"><image href="a.png" width="1" height="1"/></svg>"#;

    let err = cache
      .enforce_svg_resource_policy(svg, svg_url)
      .expect_err("expected policy block based on xml:base");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, blocked_url);
        assert!(
          reason.contains("Blocked SVG subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_mask_application() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'>
        <defs>
          <mask id='m'>
            <rect width='100%' height='100%' fill='white'/>
            <rect width='10' height='20' fill='black'/>
          </mask>
        </defs>
        <rect width='20' height='20' fill='red' mask='url(#m)' />
      </svg>
    "#;

    let image = cache.render_svg(svg).expect("render svg mask");
    let rgba = image.image.to_rgba8();
    let left_alpha = rgba.get_pixel(5, 10)[3];
    let right_alpha = rgba.get_pixel(15, 10)[3];
    assert!(
      left_alpha < right_alpha,
      "mask should reduce left side opacity"
    );
  }

  #[test]
  fn exposes_exif_orientation() {
    let cache = ImageCache::new();
    let image = cache
      .load("tests/fixtures/image_orientation/orientation-6.jpg")
      .expect("load oriented image");
    assert_eq!(
      image.orientation,
      Some(OrientationTransform {
        quarter_turns: 1,
        flip_x: false
      })
    );
  }

  #[test]
  fn resolves_relative_urls_against_base() {
    let mut cache = ImageCache::new();
    let mut path: PathBuf = std::env::temp_dir();
    path.push(format!(
      "fastrender_base_url_test_{}_{}.png",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    let dir = path.parent().unwrap().to_path_buf();
    let image = RgbaImage::from_raw(1, 1, vec![255, 0, 0, 255]).expect("build 1x1");
    image.save(&path).expect("encode png");
    let base_url = format!("file://{}", dir.display());
    cache.set_base_url(base_url);

    let image = cache
      .load(path.file_name().unwrap().to_str().unwrap())
      .expect("load via base");
    assert_eq!(image.width(), 1);
    assert_eq!(image.height(), 1);
  }

  #[test]
  fn resolves_relative_paths_against_http_base() {
    let cache = ImageCache::with_base_url("https://example.com/a/b/".to_string());
    assert_eq!(
      cache.resolve_url("../img.png"),
      "https://example.com/a/img.png".to_string()
    );
    assert_eq!(
      cache.resolve_url("./nested/icon.png"),
      "https://example.com/a/b/nested/icon.png".to_string()
    );
  }

  #[test]
  fn resolves_protocol_relative_urls_using_base_scheme() {
    let cache = ImageCache::with_base_url("https://example.com/base/".to_string());
    assert_eq!(
      cache.resolve_url("//cdn.example.com/asset.png"),
      "https://cdn.example.com/asset.png".to_string()
    );
  }

  #[test]
  fn load_preserves_non_ascii_whitespace_in_urls() {
    let nbsp = "\u{00A0}";
    let expected_url = "https://example.com/base/foo%C2%A0".to_string();
    let mut resource = FetchedResource::new(
      encode_single_pixel_png([0, 0, 0, 0]),
      Some("image/png".to_string()),
    );
    resource.status = Some(200);

    let fetcher = Arc::new(MapFetcher::with_entries([(expected_url.clone(), resource)]));
    let cache = ImageCache::with_base_url_and_fetcher(
      "https://example.com/base/".to_string(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    );

    cache
      .load(&format!("foo{nbsp}"))
      .expect("expected image load");

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|(url, _, _)| *url == expected_url),
      "expected fetcher to request {expected_url}, got: {requests:?}"
    );
  }

  #[test]
  fn image_format_from_content_type_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let content_type = format!("{nbsp}image/png");
    assert_eq!(
      ImageCache::format_from_content_type(Some(&content_type)),
      None
    );
    assert_eq!(
      ImageCache::format_from_content_type(Some("image/png")),
      Some(ImageFormat::Png)
    );
  }

  #[test]
  fn non_ascii_whitespace_svg_parse_fill_color_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert_eq!(
      svg_parse_fill_color(&format!("{nbsp}none")),
      None,
      "NBSP must not be treated as whitespace when parsing SVG fill colors"
    );
    assert_eq!(
      svg_parse_fill_color("none"),
      Some(Rgba::new(0, 0, 0, 0.0)),
      "baseline: none should parse to a transparent color sentinel"
    );
  }

  #[test]
  fn resolves_file_base_without_trailing_slash_as_directory() {
    let mut dir: PathBuf = std::env::temp_dir();
    dir.push(format!(
      "fastrender_url_base_{}_{}",
      std::process::id(),
      SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("assets")).expect("create temp dir");
    let base = format!("file://{}", dir.display());
    let cache = ImageCache::with_base_url(base);

    let resolved = cache.resolve_url("assets/image.png");
    assert!(
      resolved.ends_with("/assets/image.png"),
      "resolved path should keep directory: {}",
      resolved
    );

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn with_fetcher_uses_custom_fetcher() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct CountingFetcher {
      count: AtomicUsize,
    }

    impl ResourceFetcher for CountingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.count.fetch_add(1, Ordering::SeqCst);
        // Return a minimal valid PNG
        let png_data = vec![
          0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
          0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
          0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
          0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44,
          0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xff, 0xff, 0x3f, 0x00, 0x05, 0xfe, 0x02, 0xfe, 0xdc,
          0xcc, 0x59, 0xe7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        Ok(FetchedResource::new(
          png_data,
          Some("image/png".to_string()),
        ))
      }
    }

    let fetcher = Arc::new(CountingFetcher {
      count: AtomicUsize::new(0),
    });
    let cache = ImageCache::with_fetcher(Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>);

    let _ = cache.load("test://image.png");
    assert_eq!(fetcher.count.load(Ordering::SeqCst), 1);

    // Second load should use cache
    let _ = cache.load("test://image.png");
    assert_eq!(fetcher.count.load(Ordering::SeqCst), 1);

    // Different URL should fetch again
    let _ = cache.load("test://other.png");
    assert_eq!(fetcher.count.load(Ordering::SeqCst), 2);
  }

  struct StaticFetcher {
    bytes: Vec<u8>,
    content_type: Option<String>,
  }

  impl ResourceFetcher for StaticFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      Ok(FetchedResource::new(
        self.bytes.clone(),
        self.content_type.clone(),
      ))
    }
  }

  fn avif_fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/avif/solid.avif");
    std::fs::read(&path).expect("read avif fixture")
  }

  fn assert_green_pixel(pixel: [u8; 4]) {
    assert!(pixel[1] >= 180, "expected green channel, got {pixel:?}");
    assert!(
      pixel[0] < 50 && pixel[2] < 50,
      "expected low red/blue, got {pixel:?}"
    );
  }

  #[test]
  fn decodes_avif_with_declared_content_type() {
    let bytes = avif_fixture_bytes();
    let fetcher = Arc::new(StaticFetcher {
      bytes: bytes.clone(),
      content_type: Some("image/avif".to_string()),
    });
    let cache = ImageCache::with_fetcher(fetcher);

    let image = cache.load("test://avif.declared").expect("decode avif");
    assert_eq!(image.width(), 4);
    assert_eq!(image.height(), 4);

    let pixel = image.image.to_rgba8().get_pixel(0, 0).0;
    assert_green_pixel(pixel);
  }

  #[test]
  fn decodes_avif_when_content_type_is_incorrect() {
    let bytes = avif_fixture_bytes();
    let fetcher = Arc::new(StaticFetcher {
      bytes: bytes.clone(),
      content_type: Some("image/png".to_string()),
    });
    let cache = ImageCache::with_fetcher(fetcher);

    let image = cache
      .load("test://avif.sniff")
      .expect("decode avif via sniffing");
    assert_eq!(image.width(), 4);
    assert_eq!(image.height(), 4);

    let pixel = image.image.to_rgba8().get_pixel(2, 2).0;
    assert_green_pixel(pixel);
  }

  #[test]
  fn decode_inflight_wait_respects_render_deadline() {
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.load(&owner_url));

    // Wait until the owner has entered the fetcher (and therefore registered the in-flight entry).
    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.load(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        // Make sure we don't leave the owner thread blocked on the barrier.
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter decode did not complete under deadline: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter decode should fail under deadline"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    // Let the owner thread exit so it can resolve the in-flight entry.
    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  fn assert_decode_or_timeout(err: Error) {
    match err {
      Error::Image(_) | Error::Render(RenderError::Timeout { .. }) => {}
      other => panic!("unexpected error kind: {other:?}"),
    }
  }

  #[test]
  fn image_decode_uses_root_deadline_over_nested_budget_deadline() {
    let mut pixels = RgbaImage::new(1, 1);
    pixels
      .pixels_mut()
      .for_each(|p| *p = image::Rgba([0, 255, 0, 255]));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
      .expect("encode png");

    let url = "test://nested-deadline.png".to_string();
    let resource = FetchedResource::new(png, Some("image/png".to_string()));
    let fetcher = Arc::new(MapFetcher::with_entries([(url.clone(), resource)]));
    let cache = ImageCache::with_fetcher(fetcher);

    // Install a root deadline with enough budget, then a nested budget deadline that is already
    // expired. Image decoding should use the root deadline, so it still succeeds.
    let root_deadline = RenderDeadline::new(Some(Duration::from_secs(1)), None);
    render_control::with_deadline(Some(&root_deadline), || {
      let nested_deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
      let image = render_control::with_deadline(Some(&nested_deadline), || cache.load(&url))
        .expect("decode should ignore nested deadline budget");
      assert_eq!(image.width(), 1);
      assert_eq!(image.height(), 1);
    });
  }

  #[test]
  fn truncated_bitmap_headers_do_not_panic() {
    let cases: &[(&str, &[u8])] = &[
      ("png", b"\x89PNG\r\n\x1a\n"),
      ("jpeg", b"\xff\xd8\xff"),
      ("gif", b"GIF89a"),
      ("webp", b"RIFF\x00\x00\x00\x00WEBP"),
    ];

    let entries = cases.iter().map(|(name, bytes)| {
      (
        format!("test://truncated.{name}"),
        FetchedResource::new(bytes.to_vec(), None),
      )
    });
    let fetcher = Arc::new(MapFetcher::with_entries(entries));
    let cache = ImageCache::with_fetcher_and_config(
      fetcher,
      ImageCacheConfig::default()
        .with_max_decoded_dimension(1024)
        .with_max_decoded_pixels(1024 * 1024),
    );

    for (name, _) in cases {
      let url = format!("test://truncated.{name}");

      let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.probe(&url)));
      let probe = probe.unwrap_or_else(|_| panic!("probe panicked for {name}"));
      let probe_err = match probe {
        Ok(_) => panic!("probe should fail for truncated header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(probe_err);

      let load = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.load(&url)));
      let load = load.unwrap_or_else(|_| panic!("load panicked for {name}"));
      let load_err = match load {
        Ok(_) => panic!("load should fail for truncated header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(load_err);
    }
  }

  #[test]
  fn enforce_decode_limits_rejects_images_exceeding_max_pixmap_bytes() {
    // Even if callers disable the user-configurable decode limits, the decoder should still
    // refuse images that would require allocations larger than our global pixmap cap.
    let config = ImageCacheConfig {
      max_decoded_pixels: 0,
      max_decoded_dimension: 0,
      ..ImageCacheConfig::default()
    };
    let cache = ImageCache::with_config(config);

    // width * height * 4 > MAX_PIXMAP_BYTES
    let err = cache
      .enforce_decode_limits(20_000, 20_000, "test://too-big")
      .expect_err("expected size limit failure");
    match err {
      Error::Image(ImageError::DecodeFailed { reason, .. }) => {
        assert!(reason.contains("limit"), "unexpected reason: {reason}");
      }
      other => panic!("unexpected error: {other:?}"),
    }
  }

  #[test]
  fn malformed_isobmff_avif_headers_do_not_panic() {
    // ISO-BMFF box headers that have historically triggered debug-only assertions inside AVIF
    // sniffers/parsers.
    let cases: &[(&str, Vec<u8>)] = &[
      // Box declares an extended size but the header is truncated.
      (
        "short_ext_size",
        vec![
          0x00, 0x00, 0x00, 0x01, b'f', b't', b'y', b'p', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
      ),
      // Extended size is present but invalid (smaller than the 16-byte extended header).
      (
        "invalid_ext_size",
        vec![
          0x00, 0x00, 0x00, 0x01, b'f', b't', b'y', b'p', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
          0x08,
        ],
      ),
    ];

    let entries = cases.iter().map(|(name, bytes)| {
      (
        format!("test://malformed.{name}.avif"),
        FetchedResource::new(bytes.clone(), Some("image/avif".to_string())),
      )
    });
    let fetcher = Arc::new(MapFetcher::with_entries(entries));
    let cache = ImageCache::with_fetcher_and_config(
      fetcher,
      ImageCacheConfig::default()
        .with_max_decoded_dimension(1024)
        .with_max_decoded_pixels(1024 * 1024),
    );

    for (name, _) in cases {
      let url = format!("test://malformed.{name}.avif");

      let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.probe(&url)));
      let probe = probe.unwrap_or_else(|_| panic!("probe panicked for {name}"));
      let probe_err = match probe {
        Ok(_) => panic!("probe should fail for malformed avif header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(probe_err);

      let load = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.load(&url)));
      let load = load.unwrap_or_else(|_| panic!("load panicked for {name}"));
      let load_err = match load {
        Ok(_) => panic!("load should fail for malformed avif header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(load_err);
    }
  }

  fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a
      .iter()
      .zip(b.iter())
      .map(|(lhs, rhs)| i16::from(*lhs).abs_diff(i16::from(*rhs)) as u64)
      .sum();
    sum as f64 / a.len() as f64
  }

  #[test]
  fn raster_pixmap_at_size_matches_draw_scaled_output_with_orientation() {
    let mut image = RgbaImage::new(6, 4);
    for y in 0..image.height() {
      for x in 0..image.width() {
        let r = (x * 40).min(255) as u8;
        let g = (y * 60).min(255) as u8;
        let b = 128u8;
        image.put_pixel(x, y, image::Rgba([r, g, b, 255]));
      }
    }
    // Include one translucent pixel to exercise premultiplication.
    image.put_pixel(2, 1, image::Rgba([255, 0, 0, 128]));

    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(
        &image,
        image.width(),
        image.height(),
        ColorType::Rgba8.into(),
      )
      .expect("encode png");
    let src = format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&png)
    );

    let cache = ImageCache::new();
    let orientation = OrientationTransform {
      quarter_turns: 1,
      flip_x: true,
    };

    let full = cache
      .load_raster_pixmap(&src, orientation, false)
      .expect("load full pixmap")
      .expect("raster pixmap");

    for (target_w, target_h) in [(3u32, 4u32), (2u32, 3u32)] {
      let mut expected = Pixmap::new(target_w, target_h).expect("dst pixmap");
      let scale_x = target_w as f32 / full.width() as f32;
      let scale_y = target_h as f32 / full.height() as f32;
      let mut paint = tiny_skia::PixmapPaint::default();
      paint.quality = FilterQuality::Bilinear;
      expected.draw_pixmap(
        0,
        0,
        full.as_ref().as_ref(),
        &paint,
        tiny_skia::Transform::from_row(scale_x, 0.0, 0.0, scale_y, 0.0, 0.0),
        None,
      );

      let scaled = cache
        .load_raster_pixmap_at_size(
          &src,
          orientation,
          false,
          target_w,
          target_h,
          FilterQuality::Bilinear,
        )
        .expect("scaled pixmap")
        .expect("raster pixmap");

      assert_eq!((scaled.width(), scaled.height()), (target_w, target_h));
      let diff = mean_abs_diff(expected.data(), scaled.data());
      assert!(
        diff <= 10.0,
        "expected scaled output to match within tolerance (diff={diff})"
      );
    }

    let scaled_a = cache
      .load_raster_pixmap_at_size(&src, orientation, false, 3, 4, FilterQuality::Bilinear)
      .expect("scaled pixmap")
      .expect("raster pixmap");
    let scaled_a_again = cache
      .load_raster_pixmap_at_size(&src, orientation, false, 3, 4, FilterQuality::Bilinear)
      .expect("scaled pixmap")
      .expect("raster pixmap");
    assert!(Arc::ptr_eq(&scaled_a, &scaled_a_again));

    let scaled_b = cache
      .load_raster_pixmap_at_size(&src, orientation, false, 2, 3, FilterQuality::Bilinear)
      .expect("scaled pixmap")
      .expect("raster pixmap");
    assert!(!Arc::ptr_eq(&scaled_a, &scaled_b));
  }

  #[test]
  fn decode_inflight_wait_respects_cancel_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked-image.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.load(&owner_url));

    // Wait until the owner has entered the fetcher (and therefore registered the in-flight entry).
    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_worker = Arc::clone(&cancel_flag);
    let cancel: Arc<crate::render_control::CancelCallback> =
      Arc::new(move || cancel_flag_worker.load(Ordering::Relaxed));
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(None, Some(cancel));
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.load(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    thread::sleep(Duration::from_millis(50));
    cancel_flag.store(true, Ordering::Relaxed);

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter decode did not complete under cancel: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter decode should fail under cancel callback"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  #[test]
  fn probe_inflight_deduplicates_cache_artifact_reads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    struct ArtifactFetcher {
      url: String,
      bytes: Arc<Vec<u8>>,
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for ArtifactFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("network fetch should not be called");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("network partial fetch should not be called");
      }

      fn read_cache_artifact(
        &self,
        kind: FetchContextKind,
        url: &str,
        artifact: CacheArtifactKind,
      ) -> Option<FetchedResource> {
        assert_eq!(kind, FetchContextKind::Image);
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        if url != self.url {
          return None;
        }

        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
          // Simulate a slow disk read so concurrent probes overlap.
          thread::sleep(Duration::from_millis(50));
        }

        let mut res = FetchedResource::new(
          self.bytes.as_ref().clone(),
          Some("application/x-fastrender-image-probe+json".to_string()),
        );
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        Some(res)
      }
    }

    let meta = CachedImageMetadata {
      width: 42,
      height: 24,
      orientation: None,
      resolution: None,
      is_vector: false,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
    };
    let bytes = Arc::new(encode_probe_metadata_for_disk(&meta).expect("encode metadata"));
    let url = "https://example.com/probe-inflight-artifact.png".to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let cache = ImageCache::with_fetcher(Arc::new(ArtifactFetcher {
      url: url.clone(),
      bytes: Arc::clone(&bytes),
      calls: Arc::clone(&calls),
    }));

    let threads = 8usize;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for _ in 0..threads {
      let cache = cache.clone();
      let url = url.clone();
      let barrier = Arc::clone(&barrier);
      handles.push(thread::spawn(move || {
        barrier.wait();
        cache.probe_resolved(&url)
      }));
    }

    for handle in handles {
      let probed = handle
        .join()
        .expect("probe thread panicked")
        .expect("probe ok");
      assert_eq!(probed.dimensions(), (42, 24));
    }

    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "cache artifact reads should be single-flight under probe in-flight"
    );
  }

  #[test]
  fn probe_inflight_wait_respects_render_deadline() {
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked-probe.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.probe(&owner_url));

    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.probe(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter probe did not complete under deadline: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter probe should fail under deadline"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  #[test]
  fn probe_inflight_wait_respects_cancel_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked-probe.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.probe(&owner_url));

    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_worker = Arc::clone(&cancel_flag);
    let cancel: Arc<crate::render_control::CancelCallback> =
      Arc::new(move || cancel_flag_worker.load(Ordering::Relaxed));
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(None, Some(cancel));
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.probe(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    thread::sleep(Duration::from_millis(50));
    cancel_flag.store(true, Ordering::Relaxed);

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter probe did not complete under cancel: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter probe should fail under cancel callback"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }
}
