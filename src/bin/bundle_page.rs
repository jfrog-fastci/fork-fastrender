//! Capture and replay self-contained page bundles (HTML + assets).
#![allow(clippy::too_many_lines)]

mod common;

use clap::{ArgAction, Args, Parser, Subcommand};
use common::args::CompatArgs;
use common::asset_discovery::extract_inline_css_chunks;
use common::render_pipeline::{
  build_http_fetcher, build_render_configs, build_renderer_with_fetcher, decode_html_resource,
  render_document, RenderConfigBundle, RenderSurface,
};
use fastrender::css::encoding::decode_css_bytes;
use fastrender::css::loader::resolve_href;
use fastrender::dom::{parse_html_with_options, DomParseOptions};
use fastrender::geometry::Size;
use fastrender::html::image_prefetch::{discover_image_prefetch_requests, ImagePrefetchLimits};
use fastrender::html::images::ImageSelectionContext;
use fastrender::html::meta_refresh::{extract_js_location_redirect, extract_meta_refresh_url};
use fastrender::image_output::encode_image;
use fastrender::resource::bundle::{
  Bundle, BundleManifest, BundleRenderConfig, BundledDocument, BundledFetcher, BundledResourceInfo,
  request_partitioned_resource_key, BUNDLE_MANIFEST, BUNDLE_VERSION,
};
use fastrender::resource::{
  ensure_font_mime_sane, ensure_http_success, ensure_image_mime_sane, ensure_stylesheet_mime_sane,
  cors_enforcement_enabled, offline_placeholder_png_bytes, offline_placeholder_woff2_bytes,
  origin_from_url, DocumentOrigin,
  FetchContextKind, FetchDestination, FetchRequest, FetchedResource, ResourceAccessPolicy,
  ResourceFetcher, DEFAULT_ACCEPT_LANGUAGE, DEFAULT_USER_AGENT,
};
use fastrender::tree::box_tree::CrossOriginAttribute;
#[cfg(feature = "disk_cache")]
use fastrender::resource::{
  parse_cached_html_meta, CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher, ResourcePolicy,
};
use fastrender::style::media::{MediaContext, MediaType};
use fastrender::{OutputFormat, Result};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{collections::HashSet, collections::VecDeque};

fn is_data_url(url: &str) -> bool {
  url
    .trim_start()
    .get(..5)
    .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
    .unwrap_or(false)
}

fn is_about_url(url: &str) -> bool {
  url
    .trim_start()
    .get(..6)
    .map(|prefix| prefix.eq_ignore_ascii_case("about:"))
    .unwrap_or(false)
}

fn should_skip_crawl_url(url: &str) -> bool {
  is_data_url(url) || is_about_url(url)
}

#[derive(Parser, Debug)]
#[command(
  name = "bundle_page",
  version,
  about = "Bundle pages for offline replay"
)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
  /// Fetch a page and its subresources into a deterministic bundle
  Fetch(FetchArgs),
  /// Bundle a cached pageset entry (HTML + disk-backed assets) with no network access
  Cache(CacheArgs),
  /// Render strictly from a bundle with no network access
  Render(RenderArgs),
}

#[derive(Args, Debug)]
struct FetchArgs {
  /// URL (or file:// path) to fetch and bundle
  url: String,

  /// Output bundle path (directory or .tar)
  #[arg(long)]
  out: String,

  /// Capture subresources by parsing HTML/CSS without rendering.
  ///
  /// This mode is intended for pages that crash or time out during layout/paint,
  /// allowing offline fixtures to be captured anyway.
  #[arg(long, action = ArgAction::SetTrue, alias = "crawl")]
  no_render: bool,

  /// Per-request fetch timeout (seconds).
  ///
  /// This bounds network I/O while crawling large pages. Omit to use the default
  /// HTTP client timeout.
  #[arg(long)]
  fetch_timeout_secs: Option<u64>,

  /// Viewport size as WxH (e.g., 1200x800)
  #[arg(long, value_parser = parse_viewport, default_value = "1200x800")]
  viewport: (u32, u32),

  /// Device pixel ratio for media queries and srcset
  #[arg(long, default_value = "1.0")]
  dpr: f32,

  /// Horizontal scroll offset in CSS px
  #[arg(long, default_value = "0")]
  scroll_x: f32,

  /// Vertical scroll offset in CSS px
  #[arg(long, default_value = "0")]
  scroll_y: f32,

  /// Expand render target to the full document height
  #[arg(long, action = ArgAction::SetTrue)]
  full_page: bool,

  /// Restrict subresource loads (CSS/images/fonts/etc.) to the document origin unless allowlisted.
  ///
  /// Note: this does not block cross-origin iframe/embed document navigation (document loads).
  #[arg(long)]
  same_origin_subresources: bool,

  /// Allow additional origins when blocking cross-origin subresources (repeatable).
  #[arg(long, value_name = "ORIGIN")]
  allow_subresource_origin: Vec<String>,

  #[command(flatten)]
  compat: CompatArgs,
}

#[derive(Args, Debug)]
struct CacheArgs {
  /// Cached HTML stem under `--html-dir` (e.g. `example.com` or `example.com--deadbeef`)
  stem: String,

  /// Output bundle path (directory or .tar)
  #[arg(long)]
  out: String,

  /// Directory containing cached HTML (`*.html` + `*.html.meta`)
  #[arg(long, default_value = "fetches/html")]
  html_dir: PathBuf,

  /// Disk-backed subresource cache directory (defaults to fetches/assets)
  #[arg(
    long,
    default_value = "fetches/assets",
    value_name = "DIR",
    visible_alias = "cache-dir"
  )]
  asset_cache_dir: PathBuf,

  /// Override the User-Agent header used when computing the disk cache namespace.
  ///
  /// This must match the `--user-agent` value used when warming the pageset disk cache.
  #[arg(long, default_value = DEFAULT_USER_AGENT)]
  user_agent: String,

  /// Override the Accept-Language header used when computing the disk cache namespace.
  ///
  /// This must match the `--accept-language` value used when warming the pageset disk cache.
  #[arg(long, default_value = DEFAULT_ACCEPT_LANGUAGE)]
  accept_language: String,

  /// Allow missing subresources by inserting deterministic placeholders into the bundle.
  ///
  /// By default, cache capture fails when a required subresource is missing from the disk cache so
  /// the resulting bundle is deterministic and self-contained.
  #[arg(long, action = ArgAction::SetTrue)]
  allow_missing: bool,

  /// Viewport size as WxH (e.g., 1200x800)
  #[arg(long, value_parser = parse_viewport, default_value = "1200x800")]
  viewport: (u32, u32),

  /// Device pixel ratio for media queries and srcset
  #[arg(long, default_value = "1.0")]
  dpr: f32,

  /// Horizontal scroll offset in CSS px
  #[arg(long, default_value = "0")]
  scroll_x: f32,

  /// Vertical scroll offset in CSS px
  #[arg(long, default_value = "0")]
  scroll_y: f32,

  /// Expand render target to the full document height
  #[arg(long, action = ArgAction::SetTrue)]
  full_page: bool,

  /// Restrict subresource loads (CSS/images/fonts/etc.) to the document origin unless allowlisted.
  ///
  /// Note: this does not block cross-origin iframe/embed document navigation (document loads).
  #[arg(long)]
  same_origin_subresources: bool,

  /// Allow additional origins when blocking cross-origin subresources (repeatable).
  #[arg(long, value_name = "ORIGIN")]
  allow_subresource_origin: Vec<String>,

  #[command(flatten)]
  compat: CompatArgs,
}

#[derive(Args, Debug)]
struct RenderArgs {
  /// Bundle to render (directory or .tar)
  bundle: String,

  /// Output PNG path
  #[arg(long)]
  out: String,

  /// Override viewport size (WxH)
  #[arg(long, value_parser = parse_viewport)]
  viewport: Option<(u32, u32)>,

  /// Override device pixel ratio
  #[arg(long)]
  dpr: Option<f32>,

  /// Override horizontal scroll offset
  #[arg(long)]
  scroll_x: Option<f32>,

  /// Override vertical scroll offset
  #[arg(long)]
  scroll_y: Option<f32>,

  /// Force full-page rendering (overrides bundle)
  #[arg(long, action = ArgAction::SetTrue)]
  full_page: bool,

  /// Disable full-page rendering even if the bundle captured it
  #[arg(long, action = ArgAction::SetTrue)]
  no_full_page: bool,

  /// Restrict subresource loads (CSS/images/fonts/etc.) to the document origin unless allowlisted.
  ///
  /// Note: this does not block cross-origin iframe/embed document navigation (document loads).
  #[arg(long)]
  same_origin_subresources: bool,

  /// Allow additional origins when blocking cross-origin subresources (repeatable).
  #[arg(long, value_name = "ORIGIN")]
  allow_subresource_origin: Vec<String>,

  #[command(flatten)]
  compat: CompatArgs,
}

#[derive(Clone)]
struct RecordingFetcher {
  inner: Arc<dyn ResourceFetcher>,
  recorded: Arc<Mutex<HashMap<String, FetchedResource>>>,
  recorded_by_request: Arc<Mutex<HashMap<RecordingRequestKey, FetchedResource>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RecordingRequestKey {
  kind: FetchContextKind,
  url: String,
  origin: Option<DocumentOrigin>,
}

#[cfg(feature = "disk_cache")]
#[derive(Clone)]
struct CacheOfflineNetworkFetcher {
  inner: fastrender::resource::HttpFetcher,
}

#[cfg(feature = "disk_cache")]
impl CacheOfflineNetworkFetcher {
  fn new(user_agent: &str, accept_language: &str) -> Self {
    let offline_policy = ResourcePolicy::new().allow_http(false).allow_https(false);
    let inner = build_http_fetcher(user_agent, accept_language, None).with_policy(offline_policy);
    Self { inner }
  }

  fn is_http_like(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
      return false;
    }

    match url::Url::parse(trimmed) {
      Ok(parsed) => matches!(parsed.scheme(), "http" | "https"),
      Err(_) => {
        let lower = trimmed.to_ascii_lowercase();
        lower.starts_with("http://") || lower.starts_with("https://")
      }
    }
  }

  fn cache_miss_error() -> fastrender::Error {
    fastrender::Error::Other("cache miss (offline): not present in disk cache".to_string())
  }
}

#[cfg(feature = "disk_cache")]
impl ResourceFetcher for CacheOfflineNetworkFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if Self::is_http_like(url) {
      return Err(Self::cache_miss_error());
    }
    self.inner.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    if Self::is_http_like(req.url) {
      return Err(Self::cache_miss_error());
    }
    self.inner.fetch_with_request(req)
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    if Self::is_http_like(req.url) {
      return Err(Self::cache_miss_error());
    }
    self
      .inner
      .fetch_with_request_and_validation(req, etag, last_modified)
  }

  fn fetch_partial(&self, url: &str, max_bytes: usize) -> Result<FetchedResource> {
    if Self::is_http_like(url) {
      return Err(Self::cache_miss_error());
    }
    self.inner.fetch_partial(url, max_bytes)
  }

  fn fetch_with_validation(
    &self,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    if Self::is_http_like(url) {
      return Err(Self::cache_miss_error());
    }
    self.inner.fetch_with_validation(url, etag, last_modified)
  }
}

/// Wrapper for cache-capture mode that retries `FetchDestination::Other` misses against the disk
/// cache using a small ordered set of alternate destinations.
///
/// The disk cache keys include `FetchContextKind` (derived from destination). During bundle capture
/// we often infer destination from URL extensions, but extensionless URLs can be requested by the
/// renderer in an `Image`/`Font`/`Style` context. Retrying avoids false cache misses when the bytes
/// are already present under a different kind.
#[cfg(feature = "disk_cache")]
#[derive(Clone)]
struct CacheKindMismatchFallbackFetcher {
  inner: Arc<dyn ResourceFetcher>,
}

#[cfg(feature = "disk_cache")]
impl CacheKindMismatchFallbackFetcher {
  fn new(inner: Arc<dyn ResourceFetcher>) -> Self {
    Self { inner }
  }

  fn is_http_like(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
      return false;
    }

    match url::Url::parse(trimmed) {
      Ok(parsed) => matches!(parsed.scheme(), "http" | "https"),
      Err(_) => {
        let lower = trimmed.to_ascii_lowercase();
        lower.starts_with("http://") || lower.starts_with("https://")
      }
    }
  }
}

#[cfg(feature = "disk_cache")]
impl ResourceFetcher for CacheKindMismatchFallbackFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.inner.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    match self.inner.fetch_with_request(req) {
      Ok(res) => Ok(res),
      Err(err) => {
        if Self::is_http_like(req.url) {
          let retries: &[FetchDestination] = match req.destination {
            FetchDestination::Other => &[
              FetchDestination::Image,
              FetchDestination::ImageCors,
              FetchDestination::Font,
              FetchDestination::Style,
            ],
            FetchDestination::Image => &[FetchDestination::ImageCors],
            FetchDestination::ImageCors => &[FetchDestination::Image],
            _ => &[],
          };

          for dest in retries {
            let mut retry = FetchRequest::new(req.url, *dest);
            if let Some(referrer) = req.referrer {
              retry = retry.with_referrer(referrer);
            }
            if let Ok(res) = self.inner.fetch_with_request(retry) {
              return Ok(res);
            }
          }
        }

        Err(err)
      }
    }
  }
}

impl RecordingFetcher {
  fn new(inner: Arc<dyn ResourceFetcher>) -> Self {
    Self {
      inner,
      recorded: Arc::new(Mutex::new(HashMap::new())),
      recorded_by_request: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  fn record_override(&self, url: &str, resource: FetchedResource) {
    if let Ok(mut map) = self.recorded.lock() {
      map.insert(url.to_string(), resource);
    }
    if let Ok(mut map) = self.recorded_by_request.lock() {
      map.retain(|key, _| key.url != url);
    }
  }

  fn snapshot(&self) -> HashMap<String, FetchedResource> {
    let mut snapshot = self
      .recorded
      .lock()
      .map(|map| map.clone())
      .unwrap_or_default();

    if !cors_enforcement_enabled() {
      return snapshot;
    }

    let by_request = self
      .recorded_by_request
      .lock()
      .map(|map| map.clone())
      .unwrap_or_default();

    if by_request.is_empty() {
      return snapshot;
    }

    let mut origins_by_resource: HashMap<(FetchContextKind, String), HashSet<Option<DocumentOrigin>>> =
      HashMap::new();
    for (key, _) in &by_request {
      origins_by_resource
        .entry((key.kind, key.url.clone()))
        .or_default()
        .insert(key.origin.clone());
    }

    for (key, resource) in by_request {
      let Some(origins) = origins_by_resource.get(&(key.kind, key.url.clone())) else {
        continue;
      };
      // Only record per-origin variants when a single URL was fetched in CORS mode under multiple
      // initiating origins; otherwise the legacy URL-only entry is sufficient and keeps bundles
      // smaller.
      if origins.len() <= 1 {
        continue;
      }
      let Some(origin) = key.origin.as_ref() else {
        continue;
      };

      let manifest_key = request_partitioned_resource_key(key.kind, &key.url, origin);
      snapshot.entry(manifest_key).or_insert_with(|| resource.clone());
      if let Some(final_url) = resource.final_url.as_deref() {
        if final_url != key.url.as_str() {
          let manifest_key = request_partitioned_resource_key(key.kind, final_url, origin);
          snapshot.entry(manifest_key).or_insert_with(|| resource.clone());
        }
      }
    }

    snapshot
  }

  fn discard(&self, url: &str) {
    if let Ok(mut map) = self.recorded.lock() {
      map.remove(url);
    }
    if let Ok(mut map) = self.recorded_by_request.lock() {
      map.retain(|key, _| key.url != url);
    }
  }

  fn replace(&self, url: &str, resource: FetchedResource) {
    if let Ok(mut map) = self.recorded.lock() {
      map.insert(url.to_string(), resource);
    }
    if let Ok(mut map) = self.recorded_by_request.lock() {
      map.retain(|key, _| key.url != url);
    }
  }
}

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if let Ok(map) = self.recorded.lock() {
      if let Some(existing) = map.get(url) {
        return Ok(existing.clone());
      }
    }

    let result = self.inner.fetch(url)?;
    // Don't store `data:` URLs in bundle manifests: they can be extremely large (embedded
    // fonts/images) and are already self-contained.
    if !is_data_url(url) {
      if let Ok(mut map) = self.recorded.lock() {
        map.insert(url.to_string(), result.clone());
      }
    }
    Ok(result)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    let url = req.url.to_string();

    if cors_enforcement_enabled()
      && matches!(req.destination, FetchDestination::Font | FetchDestination::ImageCors)
    {
      let key = RecordingRequestKey {
        kind: req.destination.into(),
        url: url.clone(),
        origin: req
          .referrer
          .and_then(origin_from_url)
          .or_else(|| origin_from_url(req.url)),
      };

      if let Ok(map) = self.recorded_by_request.lock() {
        if let Some(existing) = map.get(&key) {
          return Ok(existing.clone());
        }
      }

      let result = self.inner.fetch_with_request(req)?;
      if !url
        .get(..5)
        .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
        .unwrap_or(false)
      {
        if let Ok(mut map) = self.recorded.lock() {
          map.entry(url.clone()).or_insert_with(|| result.clone());
        }
        if let Ok(mut map) = self.recorded_by_request.lock() {
          map.insert(key, result.clone());
        }
      }
      return Ok(result);
    }

    if let Ok(map) = self.recorded.lock() {
      if let Some(existing) = map.get(req.url) {
        return Ok(existing.clone());
      }
    }

    let result = self.inner.fetch_with_request(req)?;
    if !is_data_url(&url) {
      if let Ok(mut map) = self.recorded.lock() {
        map.insert(url, result.clone());
      }
    }
    Ok(result)
  }

  fn fetch_with_validation(
    &self,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    if let Ok(map) = self.recorded.lock() {
      if let Some(existing) = map.get(url) {
        return Ok(existing.clone());
      }
    }

    let result = self.inner.fetch_with_validation(url, etag, last_modified)?;
    if !is_data_url(url) {
      if let Ok(mut map) = self.recorded.lock() {
        map.insert(url.to_string(), result.clone());
      }
    }
    Ok(result)
  }

  fn fetch_with_validation_and_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    if let Ok(map) = self.recorded.lock() {
      if let Some(existing) = map.get(url) {
        return Ok(existing.clone());
      }
    }

    let result = self
      .inner
      .fetch_with_validation_and_context(kind, url, etag, last_modified)?;
    if let Ok(mut map) = self.recorded.lock() {
      map.insert(url.to_string(), result.clone());
    }
    Ok(result)
  }
}

struct ResourceEntry {
  info: BundledResourceInfo,
  bytes: Vec<u8>,
}

fn parse_viewport(s: &str) -> std::result::Result<(u32, u32), String> {
  let parts: Vec<&str> = s.split('x').collect();
  if parts.len() != 2 {
    return Err("viewport must be WxH (e.g., 1200x800)".to_string());
  }
  let w = parts[0].parse::<u32>().map_err(|_| "invalid width")?;
  let h = parts[1].parse::<u32>().map_err(|_| "invalid height")?;
  if w == 0 || h == 0 {
    return Err("width and height must be > 0".to_string());
  }
  Ok((w, h))
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  match cli.command {
    Command::Fetch(args) => fetch_bundle(args),
    Command::Cache(args) => cache_bundle(args),
    Command::Render(args) => render_bundle(args),
  }
}

fn fetch_bundle(args: FetchArgs) -> Result<()> {
  let out_path = PathBuf::from(&args.out);
  if out_path.exists() {
    return Err(fastrender::Error::Other(format!(
      "Output path already exists: {}",
      out_path.display()
    )));
  }

  let render = BundleRenderConfig {
    viewport: args.viewport,
    device_pixel_ratio: args.dpr,
    scroll_x: args.scroll_x,
    scroll_y: args.scroll_y,
    full_page: args.full_page,
    same_origin_subresources: args.same_origin_subresources,
    allowed_subresource_origins: args.allow_subresource_origin.clone(),
    compat_profile: args.compat.compat_profile(),
    dom_compat_mode: args.compat.dom_compat_mode(),
  };
  apply_full_page_env(render.full_page);

  let http = Arc::new(build_http_fetcher(
    DEFAULT_USER_AGENT,
    DEFAULT_ACCEPT_LANGUAGE,
    args.fetch_timeout_secs.map(std::time::Duration::from_secs),
  ));
  let recording = RecordingFetcher::new(http);
  let (prepared, document_resource) = fetch_document(&recording, &args.url)?;

  if args.no_render {
    crawl_document(&recording, &prepared, &render, CrawlMode::BestEffort)?;

    let recorded = recording.snapshot();
    let (manifest, resources, document_bytes) =
      build_manifest(args.url, render, document_resource, recorded);
    write_bundle(&out_path, &manifest, &resources, &document_bytes)?;

    println!(
      "✓ Captured {} resources into {}",
      manifest.resources.len(),
      out_path.display()
    );
    return Ok(());
  }

  let RenderConfigBundle { config, options } = build_render_configs(&RenderSurface {
    viewport: render.viewport,
    scroll_x: render.scroll_x,
    scroll_y: render.scroll_y,
    dpr: render.device_pixel_ratio,
    media_type: MediaType::Screen,
    css_limit: None,
    allow_partial: false,
    apply_meta_viewport: true,
    base_url: None,
    allow_file_from_http: false,
    block_mixed_content: false,
    same_origin_subresources: render.same_origin_subresources,
    allowed_subresource_origins: render.allowed_subresource_origins.clone(),
    trace_output: None,
    layout_parallelism: None,
    font_config: None,
    compat_profile: render.compat_profile,
    dom_compat_mode: render.dom_compat_mode,
  });
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(recording.clone());
  let mut renderer = build_renderer_with_fetcher(config, fetcher)?;

  // Render once to ensure all subresources are fetched and cached.
  let _ = render_document(&mut renderer, prepared, &options)?;

  let recorded = recording.snapshot();
  let (manifest, resources, document_bytes) =
    build_manifest(args.url, render, document_resource, recorded);
  write_bundle(&out_path, &manifest, &resources, &document_bytes)?;

  println!(
    "✓ Captured {} resources into {}",
    manifest.resources.len(),
    out_path.display()
  );
  Ok(())
}

fn cache_bundle(args: CacheArgs) -> Result<()> {
  #[cfg(feature = "disk_cache")]
  {
    cache_bundle_disk_cache(args)
  }
  #[cfg(not(feature = "disk_cache"))]
  {
    let _ = args;
    Err(fastrender::Error::Other(
      "bundle_page cache requires the `disk_cache` cargo feature".to_string(),
    ))
  }
}

#[cfg(feature = "disk_cache")]
fn cache_bundle_disk_cache(args: CacheArgs) -> Result<()> {
  use std::path::Component;

  let out_path = PathBuf::from(&args.out);
  if out_path.exists() {
    return Err(fastrender::Error::Other(format!(
      "Output path already exists: {}",
      out_path.display()
    )));
  }

  // Prevent directory traversal when resolving `<html-dir>/<stem>.html`.
  let stem_path = Path::new(&args.stem);
  let mut components = stem_path.components();
  let Some(first) = components.next() else {
    return Err(fastrender::Error::Other(
      "Cached HTML stem cannot be empty".to_string(),
    ));
  };
  if components.next().is_some() || !matches!(first, Component::Normal(_)) {
    return Err(fastrender::Error::Other(format!(
      "Invalid cached HTML stem: {}",
      args.stem
    )));
  }

  let html_path = args.html_dir.join(format!("{}.html", args.stem));
  if !html_path.exists() {
    return Err(fastrender::Error::Other(format!(
      "Cached HTML not found: {}",
      html_path.display()
    )));
  }

  let html_bytes = fs::read(&html_path)?;
  let meta_path = cached_html_meta_path(&html_path);
  let meta = fs::read_to_string(&meta_path).ok();
  let parsed_meta = meta
    .as_deref()
    .map(parse_cached_html_meta)
    .unwrap_or_default();

  let pageset_url_hint = parsed_meta.url.clone().or_else(|| {
    let entries = fastrender::pageset::pageset_entries();
    if let Some(entry) = entries.iter().find(|entry| entry.cache_stem == args.stem) {
      return Some(entry.url.clone());
    }

    let Some(stem) = fastrender::pageset::pageset_stem(&args.stem) else {
      return None;
    };
    let mut candidates = entries.into_iter().filter(|entry| entry.stem == stem);
    let entry = candidates.next()?;
    if candidates.next().is_some() {
      return None;
    }
    Some(entry.url)
  });
  let base_hint = pageset_url_hint
    .clone()
    .unwrap_or_else(|| format!("file://{}", html_path.display()));

  let mut document_resource = FetchedResource::with_final_url(
    html_bytes,
    parsed_meta.content_type.clone(),
    Some(base_hint.clone()),
  );
  document_resource.status = parsed_meta.status;

  let prepared = decode_html_resource(&document_resource, &base_hint);

  let render = BundleRenderConfig {
    viewport: args.viewport,
    device_pixel_ratio: args.dpr,
    scroll_x: args.scroll_x,
    scroll_y: args.scroll_y,
    full_page: args.full_page,
    same_origin_subresources: args.same_origin_subresources,
    allowed_subresource_origins: args.allow_subresource_origin.clone(),
    compat_profile: args.compat.compat_profile(),
    dom_compat_mode: args.compat.dom_compat_mode(),
  };
  apply_full_page_env(render.full_page);

  let http = CacheOfflineNetworkFetcher::new(&args.user_agent, &args.accept_language);

  let memory_config = CachingFetcherConfig {
    honor_http_cache_headers: false,
    honor_http_cache_freshness: false,
    allow_no_store: true,
    ..CachingFetcherConfig::default()
  };

  let mut disk_config = DiskCacheConfig::default();
  disk_config.allow_no_store = true;
  disk_config.namespace = Some(common::render_pipeline::disk_cache_namespace(
    &args.user_agent,
    &args.accept_language,
  ));

  let disk_fetcher: Arc<dyn ResourceFetcher> = Arc::new(DiskCachingFetcher::with_configs(
    http,
    args.asset_cache_dir,
    memory_config,
    disk_config,
  ));
  let disk_fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(CacheKindMismatchFallbackFetcher::new(disk_fetcher));

  let recording = RecordingFetcher::new(disk_fetcher);
  let crawl_mode = if args.allow_missing {
    CrawlMode::AllowMissing
  } else {
    CrawlMode::Strict
  };
  crawl_document(&recording, &prepared, &render, crawl_mode)?;

  let recorded = recording.snapshot();
  let original_url = pageset_url_hint.unwrap_or(base_hint);
  let (manifest, resources, document_bytes) =
    build_manifest(original_url, render, document_resource, recorded);
  write_bundle(&out_path, &manifest, &resources, &document_bytes)?;

  println!(
    "✓ Captured {} resources into {} (from cache)",
    manifest.resources.len(),
    out_path.display()
  );
  Ok(())
}

fn render_bundle(args: RenderArgs) -> Result<()> {
  let bundle = Bundle::load(&args.bundle)?;
  let manifest = bundle.manifest().clone();
  let (doc_meta_ref, doc_bytes) = bundle.document();
  let doc_meta = doc_meta_ref.clone();

  let mut render = manifest.render.clone();
  if let Some(viewport) = args.viewport {
    render.viewport = viewport;
  }
  if let Some(dpr) = args.dpr {
    render.device_pixel_ratio = dpr;
  }
  if let Some(scroll_x) = args.scroll_x {
    render.scroll_x = scroll_x;
  }
  if let Some(scroll_y) = args.scroll_y {
    render.scroll_y = scroll_y;
  }
  if args.full_page && args.no_full_page {
    return Err(fastrender::Error::Other(
      "Cannot combine --full-page and --no-full-page".to_string(),
    ));
  }
  if args.full_page {
    render.full_page = true;
  } else if args.no_full_page {
    render.full_page = false;
  }
  if args.same_origin_subresources {
    render.same_origin_subresources = true;
  }
  if !args.allow_subresource_origin.is_empty() {
    render.allowed_subresource_origins = args.allow_subresource_origin.clone();
  }
  if let Some(profile) = args.compat.compat_profile_arg() {
    render.compat_profile = profile.as_profile();
  }
  if let Some(mode) = args.compat.dom_compat_arg() {
    render.dom_compat_mode = mode.as_mode();
  }
  apply_full_page_env(render.full_page);

  let RenderConfigBundle { config, options } = build_render_configs(&RenderSurface {
    viewport: render.viewport,
    scroll_x: render.scroll_x,
    scroll_y: render.scroll_y,
    dpr: render.device_pixel_ratio,
    media_type: MediaType::Screen,
    css_limit: None,
    allow_partial: false,
    apply_meta_viewport: true,
    base_url: None,
    allow_file_from_http: false,
    block_mixed_content: false,
    same_origin_subresources: render.same_origin_subresources,
    allowed_subresource_origins: render.allowed_subresource_origins.clone(),
    trace_output: None,
    layout_parallelism: None,
    font_config: None,
    compat_profile: render.compat_profile,
    dom_compat_mode: render.dom_compat_mode,
  });

  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(BundledFetcher::new(bundle));
  let mut renderer = build_renderer_with_fetcher(config, fetcher)?;

  let base_hint = if doc_meta.final_url.is_empty() {
    manifest.original_url.clone()
  } else {
    doc_meta.final_url.clone()
  };
  let mut doc_resource = FetchedResource::with_final_url(
    (*doc_bytes).clone(),
    doc_meta.content_type.clone(),
    Some(base_hint.clone()),
  );
  doc_resource.status = doc_meta.status;
  doc_resource.etag = doc_meta.etag.clone();
  doc_resource.last_modified = doc_meta.last_modified.clone();
  doc_resource.access_control_allow_origin = doc_meta.access_control_allow_origin.clone();
  doc_resource.timing_allow_origin = doc_meta.timing_allow_origin.clone();
  let prepared = decode_html_resource(&doc_resource, &base_hint);
  let result = render_document(&mut renderer, prepared, &options)?;
  let png = encode_image(&result.pixmap, OutputFormat::Png)?;

  let out_path = PathBuf::from(&args.out);
  if let Some(parent) = out_path.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent)?;
    }
  }
  fs::write(&out_path, png)?;
  println!("✓ Rendered bundle to {}", out_path.display());
  Ok(())
}

fn build_manifest(
  original_url: String,
  render: BundleRenderConfig,
  document_resource: FetchedResource,
  mut recorded: HashMap<String, FetchedResource>,
) -> (BundleManifest, Vec<ResourceEntry>, Vec<u8>) {
  let final_url = document_resource
    .final_url
    .clone()
    .unwrap_or_else(|| original_url.clone());
  recorded.remove(&original_url);
  recorded.remove(&final_url);

  let doc_ext = extension_for_resource(&document_resource, &original_url);
  let document_path = format!("document.{}", doc_ext);
  let document = BundledDocument {
    path: document_path,
    content_type: document_resource.content_type.clone(),
    final_url: final_url.clone(),
    status: document_resource.status,
    etag: document_resource.etag.clone(),
    last_modified: document_resource.last_modified.clone(),
    access_control_allow_origin: document_resource.access_control_allow_origin.clone(),
    timing_allow_origin: document_resource.timing_allow_origin.clone(),
  };

  let mut resources: Vec<ResourceEntry> = Vec::new();
  let mut manifest_resources: BTreeMap<String, BundledResourceInfo> = BTreeMap::new();
  let mut urls: Vec<String> = recorded.keys().cloned().collect();
  urls.sort();
  for (idx, url) in urls.iter().enumerate() {
    let res = recorded.get(url).unwrap();
    // Bundles are often written as tar archives; `tar::Header::set_path` enforces the USTAR path
    // limit, so keep resource paths short to avoid failures on pages with very long URLs.
    let path = format!("resources/{:05}.{}", idx, extension_for_resource(res, url));
    let info = BundledResourceInfo {
      path: path.clone(),
      content_type: res.content_type.clone(),
      status: res.status,
      final_url: Some(res.final_url.clone().unwrap_or_else(|| url.clone())),
      etag: res.etag.clone(),
      last_modified: res.last_modified.clone(),
      access_control_allow_origin: res.access_control_allow_origin.clone(),
      timing_allow_origin: res.timing_allow_origin.clone(),
      access_control_allow_credentials: res.access_control_allow_credentials,
    };
    manifest_resources.insert(url.clone(), info.clone());
    resources.push(ResourceEntry {
      info,
      bytes: res.bytes.clone(),
    });
  }

  let manifest = BundleManifest {
    version: BUNDLE_VERSION,
    original_url,
    document,
    render,
    resources: manifest_resources,
  };

  (manifest, resources, document_resource.bytes.clone())
}

fn write_bundle(
  out: &Path,
  manifest: &BundleManifest,
  resources: &[ResourceEntry],
  document_bytes: &[u8],
) -> Result<()> {
  if out
    .extension()
    .and_then(|e| e.to_str())
    .map(|ext| ext.eq_ignore_ascii_case("tar"))
    .unwrap_or(false)
  {
    write_bundle_archive(out, manifest, resources, document_bytes)
  } else {
    write_bundle_directory(out, manifest, resources, document_bytes)
  }
}

fn write_bundle_directory(
  out: &Path,
  manifest: &BundleManifest,
  resources: &[ResourceEntry],
  document_bytes: &[u8],
) -> Result<()> {
  fs::create_dir_all(out)?;
  write_file(out.join(&manifest.document.path), document_bytes)?;
  for entry in resources {
    write_file(out.join(&entry.info.path), &entry.bytes)?;
  }
  let manifest_bytes = serde_json::to_vec_pretty(manifest)
    .map_err(|e| fastrender::Error::Other(format!("Failed to serialize manifest: {e}")))?;
  write_file(out.join(BUNDLE_MANIFEST), &manifest_bytes)?;
  Ok(())
}

fn write_bundle_archive(
  out: &Path,
  manifest: &BundleManifest,
  resources: &[ResourceEntry],
  document_bytes: &[u8],
) -> Result<()> {
  if let Some(parent) = out.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent)?;
    }
  }
  let file = fs::File::create(out).map_err(fastrender::Error::Io)?;
  let mut builder = tar::Builder::new(file);
  builder.mode(tar::HeaderMode::Deterministic);

  append_tar_entry(&mut builder, &manifest.document.path, document_bytes)?;
  for entry in resources {
    append_tar_entry(&mut builder, &entry.info.path, &entry.bytes)?;
  }
  let manifest_bytes = serde_json::to_vec_pretty(manifest)
    .map_err(|e| fastrender::Error::Other(format!("Failed to serialize manifest: {e}")))?;
  append_tar_entry(&mut builder, BUNDLE_MANIFEST, &manifest_bytes)?;
  builder.finish().map_err(fastrender::Error::Io)?;
  Ok(())
}

fn append_tar_entry<W: Write>(
  builder: &mut tar::Builder<W>,
  path: &str,
  bytes: &[u8],
) -> Result<()> {
  let mut header = tar::Header::new_gnu();
  header.set_size(bytes.len() as u64);
  header.set_mode(0o644);
  header.set_mtime(0);
  header.set_uid(0);
  header.set_gid(0);
  header.set_path(path).map_err(fastrender::Error::Io)?;
  header.set_cksum();
  builder
    .append(&header, bytes)
    .map_err(fastrender::Error::Io)
}

fn write_file(path: PathBuf, bytes: &[u8]) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  fs::write(path, bytes)?;
  Ok(())
}

fn extension_for_resource(res: &FetchedResource, url: &str) -> String {
  if let Some(ct_raw) = res.content_type.as_deref() {
    let ct = ct_raw.to_ascii_lowercase();
    if ct.starts_with("text/html") {
      return "html".to_string();
    }
    if ct.contains("text/css") {
      return "css".to_string();
    }
    if ct.contains("image/png") {
      return "png".to_string();
    }
    if ct.contains("image/jpeg") {
      return "jpg".to_string();
    }
    if ct.contains("image/gif") {
      return "gif".to_string();
    }
    if ct.contains("image/webp") {
      return "webp".to_string();
    }
    if ct.contains("image/avif") {
      return "avif".to_string();
    }
    if ct.contains("svg") {
      return "svg".to_string();
    }
    if ct.contains("font/woff2") {
      return "woff2".to_string();
    }
    if ct.contains("font/woff") {
      return "woff".to_string();
    }
    if ct.contains("font/ttf") {
      return "ttf".to_string();
    }
  }

  if let Ok(parsed) = url::Url::parse(url) {
    if let Some(ext) = Path::new(parsed.path())
      .extension()
      .and_then(|e| e.to_str())
    {
      let ext = ext.to_ascii_lowercase();
      if ext.len() <= 8 {
        return ext;
      }
    }
  }

  "bin".to_string()
}

fn cached_html_meta_path(html_path: &Path) -> PathBuf {
  let mut meta_path = html_path.to_path_buf();
  if let Some(ext) = meta_path.extension().and_then(|e| e.to_str()) {
    meta_path.set_extension(format!("{ext}.meta"));
  } else {
    meta_path.set_extension("meta");
  }
  meta_path
}

fn fetch_document(
  fetcher: &RecordingFetcher,
  url: &str,
) -> Result<(common::render_pipeline::PreparedDocument, FetchedResource)> {
  let mut resource = fetcher.fetch_with_request(FetchRequest::document(url))?;
  let mut base_hint = resource.final_url.as_deref().unwrap_or(url).to_string();
  let mut doc = decode_html_resource(&resource, &base_hint);

  if let Some(refresh) = extract_meta_refresh_url(&doc.html) {
    if let Some(target) = resolve_href(&doc.base_url, &refresh) {
      eprintln!("Following meta refresh to: {target}");
      let referrer = doc.base_hint.clone();
      match fetcher.fetch_with_request(FetchRequest::document(&target).with_referrer(&referrer)) {
        Ok(res) => {
          resource = res;
          base_hint = resource.final_url.as_deref().unwrap_or(&target).to_string();
          doc = decode_html_resource(&resource, &base_hint);
        }
        Err(err) => eprintln!("Warning: failed to follow meta refresh: {err}"),
      }
    }
  }

  if let Some(js_redirect) = extract_js_location_redirect(&doc.html) {
    if js_redirect.len() <= 2048 {
      if let Some(target) = resolve_href(&doc.base_url, &js_redirect) {
        eprintln!("Following JS redirect to: {target}");
        let referrer = doc.base_hint.clone();
        match fetcher.fetch_with_request(FetchRequest::document(&target).with_referrer(&referrer)) {
          Ok(res) => {
            resource = res;
            base_hint = resource.final_url.as_deref().unwrap_or(&target).to_string();
            doc = decode_html_resource(&resource, &base_hint);
          }
          Err(err) => eprintln!("Warning: failed to follow JS redirect: {err}"),
        }
      }
    } else {
      eprintln!(
        "Warning: skipping JS redirect longer than 2048 bytes (len={})",
        js_redirect.len()
      );
    }
  }

  Ok((doc, resource))
}

fn apply_full_page_env(full_page: bool) {
  if full_page {
    std::env::set_var("FASTR_FULL_PAGE", "1");
  } else {
    std::env::remove_var("FASTR_FULL_PAGE");
  }
}

#[derive(Clone, Copy, Debug)]
enum CrawlMode {
  /// Preserve historical behavior: warn and skip missing resources.
  BestEffort,
  /// Fail if any discovered subresource cannot be fetched.
  Strict,
  /// Insert deterministic placeholder resources for missing resources.
  AllowMissing,
}

fn placeholder_resource(destination: FetchDestination, url: &str, referrer: &str) -> FetchedResource {
  fn allow_origin_for_referrer(referrer: &str) -> Option<String> {
    let origin = origin_from_url(referrer)?;
    if origin.is_http_like() {
      let host = origin.host()?;
      let host = match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_string(),
      };
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
      return Some(origin_str);
    }
    // Non-HTTP(S) origins (notably file://) use `null` for CORS purposes.
    Some("null".to_string())
  }

  let allow_origin = allow_origin_for_referrer(referrer).unwrap_or_else(|| "*".to_string());
  match destination {
    FetchDestination::Image | FetchDestination::ImageCors => {
      let mut res = FetchedResource::with_final_url(
        offline_placeholder_png_bytes().to_vec(),
        Some("image/png".to_string()),
        Some(url.to_string()),
      );
      // When CORS enforcement is enabled (`FASTR_FETCH_ENFORCE_CORS`), `<img crossorigin>` loads
      // expect an `Access-Control-Allow-Origin` header. For missing resources we prefer a
      // deterministic placeholder over failing the entire render.
      res.access_control_allow_origin = Some(allow_origin.clone());
      // Placeholders should satisfy both anonymous and credentialed CORS checks.
      res.access_control_allow_credentials = true;
      res
    }
    FetchDestination::Font => {
      let mut res = FetchedResource::with_final_url(
        offline_placeholder_woff2_bytes().to_vec(),
        Some("font/woff2".to_string()),
        Some(url.to_string()),
      );
      // Fonts are always fetched in CORS mode; make placeholders usable when CORS enforcement is
      // enabled.
      res.access_control_allow_origin = Some(allow_origin.clone());
      res.access_control_allow_credentials = true;
      res
    }
    FetchDestination::Style => FetchedResource::with_final_url(
      b"/* missing */".to_vec(),
      Some("text/css".to_string()),
      Some(url.to_string()),
    ),
    FetchDestination::Document => FetchedResource::with_final_url(
      b"<!doctype html><html></html>".to_vec(),
      Some("text/html; charset=utf-8".to_string()),
      Some(url.to_string()),
    ),
    FetchDestination::Other => FetchedResource::with_final_url(
      Vec::new(),
      Some("application/octet-stream".to_string()),
      Some(url.to_string()),
    ),
  }
}

fn crawl_document(
  fetcher: &RecordingFetcher,
  document: &common::render_pipeline::PreparedDocument,
  render: &BundleRenderConfig,
  mode: CrawlMode,
) -> Result<()> {
  fn html_has_style_tag(html: &str) -> bool {
    let bytes = html.as_bytes();
    let mut search_from = 0;

    while search_from < bytes.len() {
      let Some(rel) = bytes[search_from..].iter().position(|b| *b == b'<') else {
        break;
      };
      let start = search_from + rel;
      let mut pos = start + 1;

      while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
      }

      if pos < bytes.len() && bytes[pos] == b'/' {
        search_from = start + 1;
        continue;
      }

      const STYLE_BYTES: &[u8] = b"style";
      if pos + STYLE_BYTES.len() <= bytes.len()
        && bytes[pos..pos + STYLE_BYTES.len()].eq_ignore_ascii_case(STYLE_BYTES)
      {
        let after = pos + STYLE_BYTES.len();
        if after >= bytes.len() {
          return true;
        }
        let next = bytes[after];
        if next == b'>' || next == b'/' || next.is_ascii_whitespace() {
          return true;
        }
      }

      search_from = start + 1;
    }

    false
  }

  fn enqueue_unique(
    queue: &mut VecDeque<(String, FetchDestination, String)>,
    seen_urls: &mut HashSet<String>,
    queued: &mut HashMap<(String, Option<DocumentOrigin>), FetchDestination>,
    url: String,
    destination: FetchDestination,
    referrer: &str,
  ) {
    if url.is_empty() {
      return;
    }
    if should_skip_crawl_url(&url) {
      return;
    }
    // Track unique URLs separately from per-document queue entries so that the same URL can be
    // retried when discovered under a different document origin. This matches renderer behavior
    // under `--same-origin-subresources` (a URL may be blocked for one document but allowed for
    // another).
    seen_urls.insert(url.clone());
    let origin = origin_from_url(referrer);
    let key = (url.clone(), origin.clone());
    match queued.get(&key).copied() {
      None => {
        queued.insert(key, destination);
        queue.push_back((url, destination, referrer.to_string()));
      }
      Some(existing) => {
        if existing == FetchDestination::Image && destination == FetchDestination::ImageCors {
          queued.insert(key, destination);
          // Upgrade the queued destination in-place so CORS-mode images are fetched first and
          // stored in the bundle manifest.
          for (queued_url, queued_destination, queued_referrer) in queue.iter_mut() {
            if queued_url.as_str() == url.as_str() && origin_from_url(queued_referrer) == origin {
              *queued_destination = FetchDestination::ImageCors;
              break;
            }
          }
        }
      }
    }
  }

  fn document_response_looks_like_html(res: &FetchedResource, requested_url: &str) -> bool {
    res
      .content_type
      .as_deref()
      .map(|ct| {
        let ct = ct.to_ascii_lowercase();
        ct.starts_with("text/html")
          || ct.starts_with("application/xhtml+xml")
          || ct.starts_with("application/html")
          || ct.contains("+html")
      })
      .unwrap_or_else(|| {
        let lower = requested_url.to_ascii_lowercase();
        lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml")
      })
  }

  fn discover_html_images(
    html: &str,
    base_url: &str,
    render: &BundleRenderConfig,
  ) -> Result<Vec<(String, FetchDestination)>> {
    let dom = parse_html_with_options(
      html,
      DomParseOptions {
        compatibility_mode: render.dom_compat_mode,
      },
    )?;
    let viewport = Size::new(render.viewport.0 as f32, render.viewport.1 as f32);
    let media_ctx = MediaContext::screen(viewport.width, viewport.height)
      .with_device_pixel_ratio(render.device_pixel_ratio)
      .with_env_overrides();
    let ctx = ImageSelectionContext {
      device_pixel_ratio: render.device_pixel_ratio,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: None,
      base_url: Some(base_url),
    };
    let discovery = discover_image_prefetch_requests(&dom, ctx, ImagePrefetchLimits::default());
    let mut out: Vec<(String, FetchDestination)> = Vec::new();
    let mut indexes: HashMap<String, usize> = HashMap::new();
    for req in discovery.requests {
      if should_skip_crawl_url(&req.url) {
        continue;
      }

      let destination = match req.crossorigin {
        CrossOriginAttribute::None => FetchDestination::Image,
        _ => FetchDestination::ImageCors,
      };
      match indexes.get(&req.url).copied() {
        Some(idx) => {
          // Prefer capturing ImageCors when any responsive candidate for a URL is marked
          // crossorigin so offline bundles include the CORS-mode response headers.
          if destination == FetchDestination::ImageCors
            && matches!(out[idx].1, FetchDestination::Image)
          {
            out[idx].1 = FetchDestination::ImageCors;
          }
        }
        None => {
          indexes.insert(req.url.clone(), out.len());
          out.push((req.url, destination));
        }
      }
    }

    Ok(out)
  }

  fn handle_crawl_failure(
    fetcher: &RecordingFetcher,
    fetch_errors: &mut Vec<(String, String)>,
    url: &str,
    destination: FetchDestination,
    referrer: &str,
    err: &fastrender::Error,
    mode: CrawlMode,
  ) {
    match mode {
      CrawlMode::BestEffort => {
        eprintln!("Warning: failed to crawl {url}: {err}");
      }
      CrawlMode::Strict => {
        fetch_errors.push((url.to_string(), err.to_string()));
      }
      CrawlMode::AllowMissing => {
        eprintln!("Warning: missing resource; inserting placeholder: {url}: {err}");
        fetcher.record_override(url, placeholder_resource(destination, url, referrer));
      }
    }
  }

  let allowed_origins = render
    .allowed_subresource_origins
    .iter()
    .filter_map(|origin| {
      if let Some(parsed) = origin_from_url(origin) {
        Some(parsed)
      } else {
        eprintln!("Warning: ignoring invalid --allow-subresource-origin value: {origin}");
        None
      }
    })
    .collect::<Vec<_>>();
  let policy = ResourceAccessPolicy {
    document_origin: None,
    allow_file_from_http: false,
    block_mixed_content: false,
    same_origin_only: render.same_origin_subresources,
    allowed_origins,
  };

  let mut queue: VecDeque<(String, FetchDestination, String)> = VecDeque::new();
  // Cap recursion based on distinct URLs discovered, not on per-document referrer contexts.
  let mut seen_urls: HashSet<String> = HashSet::new();
  let mut queued: HashMap<(String, Option<DocumentOrigin>), FetchDestination> = HashMap::new();
  // Track fetched resources using the same partitioning semantics as the renderer caches: kind is
  // always part of the key, and for CORS-mode requests under CORS enforcement we also include the
  // initiating origin so we don't accidentally drop per-origin metadata (notably
  // Access-Control-Allow-Origin).
  let mut fetched_urls: HashSet<(FetchContextKind, String, Option<DocumentOrigin>)> = HashSet::new();
  let mut fetch_errors: Vec<(String, String)> = Vec::new();
  let root_referrer = document.base_hint.as_str();

  let css_links = fastrender::css::loader::extract_css_links(
    &document.html,
    &document.base_url,
    MediaType::Screen,
  )
  .unwrap_or_default();
  let has_link_stylesheets = !css_links.is_empty();
  for css_url in css_links {
    enqueue_unique(
      &mut queue,
      &mut seen_urls,
      &mut queued,
      css_url,
      FetchDestination::Style,
      root_referrer,
    );
  }

  let has_style_tag = html_has_style_tag(&document.html);
  if !has_link_stylesheets && !has_style_tag {
    for css_url in
      fastrender::css::loader::extract_embedded_css_urls(&document.html, &document.base_url)
        .unwrap_or_default()
    {
      enqueue_unique(
        &mut queue,
        &mut seen_urls,
        &mut queued,
        css_url,
        FetchDestination::Style,
        root_referrer,
      );
    }
  }

  for css_chunk in extract_inline_css_chunks(&document.html) {
    for (url, destination) in discover_css_urls_with_destination(&css_chunk, &document.base_url) {
      enqueue_unique(
        &mut queue,
        &mut seen_urls,
        &mut queued,
        url,
        destination,
        root_referrer,
      );
    }
  }

  let fastrender::html::asset_discovery::HtmlAssetUrls {
    images: html_image_urls,
    documents: html_documents,
  } =
    fastrender::html::asset_discovery::discover_html_asset_urls(&document.html, &document.base_url);
  if matches!(mode, CrawlMode::BestEffort) {
    for url in html_image_urls {
      if should_skip_crawl_url(&url) {
        continue;
      }
      enqueue_unique(
        &mut queue,
        &mut seen_urls,
        &mut queued,
        url,
        FetchDestination::Image,
        root_referrer,
      );
    }
    // Best-effort mode historically uses regex-based discovery, which loses `<img crossorigin>`
    // metadata. Opportunistically run the renderer-aligned responsive image discovery and upgrade
    // any matching URLs to CORS mode.
    if let Ok(requests) = discover_html_images(&document.html, &document.base_url, render) {
      for (url, destination) in requests {
        enqueue_unique(
          &mut queue,
          &mut seen_urls,
          &mut queued,
          url,
          destination,
          root_referrer,
        );
      }
    }
  } else {
    for (url, destination) in discover_html_images(&document.html, &document.base_url, render)? {
      enqueue_unique(
        &mut queue,
        &mut seen_urls,
        &mut queued,
        url,
        destination,
        root_referrer,
      );
    }
  }
  for url in html_documents {
    enqueue_unique(
      &mut queue,
      &mut seen_urls,
      &mut queued,
      url,
      FetchDestination::Document,
      root_referrer,
    );
  }

  const MAX_CRAWL_URLS: usize = 10_000;
  // Keep offline bundles tractable by avoiding multi-megabyte image downloads becoming part of the
  // deterministic fixture. The HTML/CSS rewrite step will still produce local references for these
  // URLs, but the stored bytes are replaced with a deterministic 1x1 PNG placeholder.
  const MAX_CRAWL_IMAGE_BYTES: usize = 1_000_000;
  while let Some((url, destination, referrer)) = queue.pop_front() {
    if seen_urls.len() > MAX_CRAWL_URLS {
      eprintln!(
        "Warning: crawl URL limit exceeded ({}); skipping remaining discoveries",
        MAX_CRAWL_URLS
      );
      break;
    }

    let kind: FetchContextKind = destination.into();
    let document_origin = origin_from_url(&referrer);
    let origin_key = if cors_enforcement_enabled()
      && matches!(destination, FetchDestination::Font | FetchDestination::ImageCors)
    {
      document_origin.clone().or_else(|| origin_from_url(&url))
    } else {
      None
    };
    let fetch_key = (kind, url.clone(), origin_key.clone());
    if fetched_urls.contains(&fetch_key) {
      continue;
    }

    let policy_for_request = policy.for_origin(document_origin.clone());
    let allowed = match destination {
      FetchDestination::Document => policy_for_request.allows_document(&url),
      _ => policy_for_request.allows(&url),
    };
    if let Err(err) = allowed {
      eprintln!("Skipping blocked subresource {url}: {}", err.reason);
      continue;
    }

    let req = FetchRequest::new(&url, destination).with_referrer(&referrer);
    let res = match fetcher.fetch_with_request(req) {
      Ok(res) => res,
      Err(err) => {
        handle_crawl_failure(fetcher, &mut fetch_errors, &url, destination, &referrer, &err, mode);
        if matches!(mode, CrawlMode::AllowMissing) {
          fetched_urls.insert(fetch_key);
        }
        continue;
      }
    };

    let allowed = match destination {
      FetchDestination::Document => {
        policy_for_request.allows_document_with_final(&url, res.final_url.as_deref())
      }
      _ => policy_for_request.allows_with_final(&url, res.final_url.as_deref()),
    };
    if let Err(err) = allowed {
      eprintln!(
        "Skipping blocked subresource {url} (final {}): {}",
        res.final_url.as_deref().unwrap_or("<none>"),
        err.reason
      );
      fetcher.discard(&url);
      continue;
    }

    fetched_urls.insert(fetch_key);

    let validation = match destination {
      FetchDestination::Style => {
        ensure_http_success(&res, &url).and_then(|_| ensure_stylesheet_mime_sane(&res, &url))
      }
      FetchDestination::Font => {
        ensure_http_success(&res, &url).and_then(|_| ensure_font_mime_sane(&res, &url))
      }
      FetchDestination::Image => {
        ensure_http_success(&res, &url).and_then(|_| ensure_image_mime_sane(&res, &url))
      }
      FetchDestination::ImageCors => {
        ensure_http_success(&res, &url).and_then(|_| ensure_image_mime_sane(&res, &url))
      }
      FetchDestination::Document => ensure_http_success(&res, &url).and_then(|_| {
        if document_response_looks_like_html(&res, &url) {
          Ok(())
        } else {
          let content_type = res.content_type.as_deref().unwrap_or("<missing>");
          let status = res
            .status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<missing>".to_string());
          let final_url = res.final_url.as_deref().unwrap_or(&url);
          Err(fastrender::Error::Other(format!(
            "unexpected content-type {content_type} (status {status}, final_url {final_url})"
          )))
        }
      }),
      FetchDestination::Other => Ok(()),
    };
    if let Err(err) = validation {
      handle_crawl_failure(fetcher, &mut fetch_errors, &url, destination, &referrer, &err, mode);
      continue;
    }

    if res.bytes.len() > MAX_CRAWL_IMAGE_BYTES
      && (matches!(
        destination,
        FetchDestination::Image | FetchDestination::ImageCors
      ) || is_image_resource(&res, &url))
    {
      eprintln!(
        "Warning: truncating large image subresource {url} ({} bytes) for bundle size",
        res.bytes.len()
      );
      let mut truncated = res.clone();
      truncated.bytes = offline_placeholder_png_bytes().to_vec();
      truncated.content_type = Some("image/png".to_string());
      fetcher.replace(&url, truncated);
      continue;
    }

    match destination {
      FetchDestination::Style => {
        let css_base = res.final_url.as_deref().unwrap_or(&url);
        let css = decode_css_bytes(&res.bytes, res.content_type.as_deref());
        for (dep, destination) in discover_css_urls_with_destination(&css, css_base) {
          enqueue_unique(
            &mut queue,
            &mut seen_urls,
            &mut queued,
            dep,
            destination,
            &referrer,
          );
        }
      }
      FetchDestination::Document => {
        // Iframes/objects/embeds are rendered as nested documents, so crawl their HTML for the
        // same kinds of subresources we discover in the root document (CSS links, inline `url()`,
        // and responsive image candidates aligned with the renderer).
        let base_hint = res.final_url.as_deref().unwrap_or(&url);
        let doc = decode_html_resource(&res, base_hint);

        let css_links =
          fastrender::css::loader::extract_css_links(&doc.html, &doc.base_url, MediaType::Screen)
            .unwrap_or_default();
        let has_link_stylesheets = !css_links.is_empty();
        for css_url in css_links {
          enqueue_unique(
            &mut queue,
            &mut seen_urls,
            &mut queued,
            css_url,
            FetchDestination::Style,
            doc.base_hint.as_str(),
          );
        }

        let has_style_tag = html_has_style_tag(&doc.html);
        if !has_link_stylesheets && !has_style_tag {
          for css_url in
            fastrender::css::loader::extract_embedded_css_urls(&doc.html, &doc.base_url)
              .unwrap_or_default()
          {
            enqueue_unique(
              &mut queue,
              &mut seen_urls,
              &mut queued,
              css_url,
              FetchDestination::Style,
              doc.base_hint.as_str(),
            );
          }
        }

        for css_chunk in extract_inline_css_chunks(&doc.html) {
          for (url, destination) in discover_css_urls_with_destination(&css_chunk, &doc.base_url) {
            enqueue_unique(
              &mut queue,
              &mut seen_urls,
              &mut queued,
              url,
              destination,
              doc.base_hint.as_str(),
            );
          }
        }

        let fastrender::html::asset_discovery::HtmlAssetUrls {
          images: html_image_urls,
          documents: html_documents,
        } = fastrender::html::asset_discovery::discover_html_asset_urls(&doc.html, &doc.base_url);
        if matches!(mode, CrawlMode::BestEffort) {
          for url in html_image_urls {
            if should_skip_crawl_url(&url) {
              continue;
            }
            enqueue_unique(
              &mut queue,
              &mut seen_urls,
              &mut queued,
              url,
              FetchDestination::Image,
              doc.base_hint.as_str(),
            );
          }
          // As with the root document, attempt renderer-aligned discovery so `<img crossorigin>`
          // resources are fetched in CORS mode when possible.
          if let Ok(requests) = discover_html_images(&doc.html, &doc.base_url, render) {
            for (url, destination) in requests {
              enqueue_unique(
                &mut queue,
                &mut seen_urls,
                &mut queued,
                url,
                destination,
                doc.base_hint.as_str(),
              );
            }
          }
        } else {
          for (url, destination) in discover_html_images(&doc.html, &doc.base_url, render)? {
            enqueue_unique(
              &mut queue,
              &mut seen_urls,
              &mut queued,
              url,
              destination,
              doc.base_hint.as_str(),
            );
          }
        }

        for url in html_documents {
          enqueue_unique(
            &mut queue,
            &mut seen_urls,
            &mut queued,
            url,
            FetchDestination::Document,
            doc.base_hint.as_str(),
          );
        }
      }
      _ => {}
    }
  }

  if matches!(mode, CrawlMode::Strict) && !fetch_errors.is_empty() {
    fetch_errors.sort_by(|a, b| a.0.cmp(&b.0));
    let mut msg = format!(
      "Cache capture failed: {} subresource(s) missing from cache",
      fetch_errors.len()
    );
    msg.push_str("\n\nMissing resources:");
    for (url, err) in fetch_errors.iter().take(50) {
      msg.push_str(&format!("\n- {url}: {err}"));
    }
    if fetch_errors.len() > 50 {
      msg.push_str(&format!("\n... and {} more", fetch_errors.len() - 50));
    }
    msg.push_str("\n\nTip: warm the disk cache by running `prefetch_assets` / `pageset_progress` online (use the same `--cache-dir`), or pass `--allow-missing` to capture placeholders.");
    return Err(fastrender::Error::Other(msg));
  }

  Ok(())
}

fn discover_css_urls_with_destination(
  css: &str,
  base_url: &str,
) -> Vec<(String, FetchDestination)> {
  use cssparser::{Parser, ParserInput, Token};

  fn record(
    out: &mut Vec<(String, FetchDestination)>,
    base_url: &str,
    raw: &str,
    from_import: bool,
    in_font_face: bool,
  ) {
    if let Some(resolved) = resolve_href(base_url, raw) {
      if should_skip_crawl_url(&resolved) {
        return;
      }
      let destination = if from_import {
        FetchDestination::Style
      } else if in_font_face {
        // Treat all URLs inside `@font-face { ... }` blocks as fonts, regardless of file
        // extension. Legacy sources (e.g. SVG fonts) and extensionless font URLs are common on the
        // pageset; classifying these as images causes false disk-cache misses.
        FetchDestination::Font
      } else {
        // `url(...)` tokens in CSS are overwhelmingly image-like resources (backgrounds, cursors,
        // masks, etc). Treat unknown/extensionless URLs as images so disk-cache lookups use the same
        // kind the renderer/prefetchers use, and so `--allow-missing` substitutes a valid PNG
        // placeholder instead of empty bytes.
        FetchDestination::Image
      };
      out.push((resolved, destination));
    }
  }

  fn scan<'i, 't>(
    parser: &mut Parser<'i, 't>,
    base_url: &str,
    out: &mut Vec<(String, FetchDestination)>,
    in_font_face: bool,
  ) {
    let mut next_font_face_block = false;
    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(t) => t,
        Err(_) => break,
      };

      match token {
        Token::Semicolon => next_font_face_block = false,
        Token::UnquotedUrl(url) => record(out, base_url, url.as_ref(), false, in_font_face),
        Token::Function(name) if name.eq_ignore_ascii_case("url") => {
          let parse_result = parser.parse_nested_block(|nested| {
            let mut arg: Option<String> = None;
            while !nested.is_exhausted() {
              match nested.next_including_whitespace_and_comments() {
                Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                Ok(Token::QuotedString(s)) | Ok(Token::UnquotedUrl(s)) => {
                  arg = Some(s.as_ref().to_string());
                  break;
                }
                Ok(Token::Ident(s)) => {
                  arg = Some(s.as_ref().to_string());
                  break;
                }
                Ok(Token::BadUrl(_)) | Err(_) => break,
                Ok(_) => {}
              }
            }
            Ok::<_, cssparser::ParseError<'i, ()>>(arg)
          });

          if let Ok(Some(arg)) = parse_result {
            record(out, base_url, &arg, false, in_font_face);
          }
        }
        Token::AtKeyword(name) if name.eq_ignore_ascii_case("font-face") => {
          next_font_face_block = true;
        }
        Token::AtKeyword(name) if name.eq_ignore_ascii_case("import") => {
          let mut target: Option<String> = None;
          while !parser.is_exhausted() {
            let next = match parser.next_including_whitespace_and_comments() {
              Ok(t) => t,
              Err(_) => break,
            };
            match next {
              Token::WhiteSpace(_) | Token::Comment(_) => continue,
              Token::QuotedString(s) | Token::UnquotedUrl(s) => {
                target = Some(s.as_ref().to_string());
                break;
              }
              Token::Function(fname) if fname.eq_ignore_ascii_case("url") => {
                let parse_result = parser.parse_nested_block(|nested| {
                  let mut arg: Option<String> = None;
                  while !nested.is_exhausted() {
                    match nested.next_including_whitespace_and_comments() {
                      Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                      Ok(Token::QuotedString(s)) | Ok(Token::UnquotedUrl(s)) => {
                        arg = Some(s.as_ref().to_string());
                        break;
                      }
                      Ok(Token::Ident(s)) => {
                        arg = Some(s.as_ref().to_string());
                        break;
                      }
                      Ok(Token::BadUrl(_)) | Err(_) => break,
                      Ok(_) => {}
                    }
                  }
                  Ok::<_, cssparser::ParseError<'i, ()>>(arg)
                });
                target = parse_result.ok().flatten();
                break;
              }
              Token::Ident(s) => {
                target = Some(s.as_ref().to_string());
                break;
              }
              Token::Semicolon => break,
              _ => break,
            }
          }
          if let Some(target) = target {
            record(out, base_url, &target, true, false);
          }
        }
        Token::CurlyBracketBlock => {
          let nested_font_face = in_font_face || next_font_face_block;
          let _ = parser.parse_nested_block(|nested| {
            scan(nested, base_url, out, nested_font_face);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          next_font_face_block = false;
        }
        Token::Function(_) | Token::ParenthesisBlock | Token::SquareBracketBlock => {
          let _ = parser.parse_nested_block(|nested| {
            scan(nested, base_url, out, in_font_face);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
        }
        _ => {}
      }
    }
  }

  let mut out = Vec::new();
  let mut input = ParserInput::new(css);
  let mut parser = Parser::new(&mut input);
  scan(&mut parser, base_url, &mut out, false);
  out
}

fn is_image_resource(res: &FetchedResource, url: &str) -> bool {
  if res
    .content_type
    .as_deref()
    .map(|ct| ct.to_ascii_lowercase().starts_with("image/"))
    .unwrap_or(false)
  {
    return true;
  }

  let candidate = res.final_url.as_deref().unwrap_or(url);
  if let Ok(parsed) = url::Url::parse(candidate) {
    let path = parsed.path().to_ascii_lowercase();
    return matches!(
      path.rsplit('.').next().unwrap_or(""),
      "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg"
    );
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use std::collections::HashMap;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Mutex;

  #[derive(Default)]
  struct MockFetcher {
    calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
  }

  impl MockFetcher {
    fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
      self
        .calls
        .lock()
        .map(|calls| calls.clone())
        .unwrap_or_default()
    }
  }

  impl ResourceFetcher for MockFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.calls.lock().unwrap().push((
        req.url.to_string(),
        req.destination,
        req.referrer.map(|r| r.to_string()),
      ));

      match req.url {
        "https://example.com/" => Ok(FetchedResource::with_final_url(
          br#"<html><head><link rel="stylesheet" href="/style.css"></head><body><img src="/img.png"></body></html>"#
            .to_vec(),
          Some("text/html".to_string()),
          Some(req.url.to_string()),
        )),
        "https://example.com/style.css" => Ok(FetchedResource::with_final_url(
          b"body { background: url('/bg.png'); }".to_vec(),
          Some("text/css".to_string()),
          Some(req.url.to_string()),
        )),
        "https://example.com/img.png" | "https://example.com/bg.png" => Ok(
          FetchedResource::with_final_url(
            vec![0u8, 1, 2, 3],
            Some("image/png".to_string()),
            Some(req.url.to_string()),
          ),
        ),
        other => Err(fastrender::Error::Other(format!(
          "unexpected fetch: {other}"
        ))),
      }
    }
  }

  #[test]
  fn crawl_uses_destination_and_referrer() -> Result<()> {
    let inner = Arc::new(MockFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let base_hint = doc.base_hint.clone();
    let calls = inner.calls();

    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://example.com/style.css"
        && *dest == FetchDestination::Style
        && referrer.as_deref() == Some(base_hint.as_str())
    }));

    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://example.com/img.png"
        && *dest == FetchDestination::Image
        && referrer.as_deref() == Some(base_hint.as_str())
    }));

    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://example.com/bg.png"
        && *dest == FetchDestination::Image
        && referrer.as_deref() == Some(base_hint.as_str())
    }));

    Ok(())
  }

  #[derive(Default)]
  struct OriginSensitiveFetcher {
    calls: AtomicUsize,
  }

  impl OriginSensitiveFetcher {
    fn calls(&self) -> usize {
      self.calls.load(Ordering::SeqCst)
    }
  }

  impl ResourceFetcher for OriginSensitiveFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      panic!("expected RecordingFetcher to use fetch_with_request");
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.calls.fetch_add(1, Ordering::SeqCst);

      let origin = req
        .referrer
        .and_then(|referrer| url::Url::parse(referrer).ok())
        .and_then(|parsed| parsed.host_str().map(|host| format!("{}://{}", parsed.scheme(), host)));

      let mut res =
        FetchedResource::with_final_url(b"ok".to_vec(), Some("text/plain".to_string()), None);
      res.access_control_allow_origin = origin;
      Ok(res)
    }
  }

  #[test]
  fn recording_fetcher_partitions_cors_mode_requests_by_origin_when_enforced() -> Result<()> {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));

    with_thread_runtime_toggles(toggles, || {
      let inner = Arc::new(OriginSensitiveFetcher::default());
      let recording = RecordingFetcher::new(inner.clone());

      for (destination, url) in [
        (FetchDestination::Font, "https://example.com/font.woff2"),
        (FetchDestination::ImageCors, "https://example.com/img.png"),
      ] {
        let req_a =
          FetchRequest::new(url, destination).with_referrer("https://a.test/page.html");
        let req_b =
          FetchRequest::new(url, destination).with_referrer("https://b.test/page.html");

        let a = recording.fetch_with_request(req_a).expect("origin A fetch");
        let b = recording.fetch_with_request(req_b).expect("origin B fetch");

        assert_eq!(a.bytes, b"ok");
        assert_eq!(b.bytes, b"ok");
        assert_eq!(
          a.access_control_allow_origin.as_deref(),
          Some("https://a.test"),
          "unexpected ACAO for origin A"
        );
        assert_eq!(
          b.access_control_allow_origin.as_deref(),
          Some("https://b.test"),
          "unexpected ACAO for origin B"
        );
      }

      assert_eq!(
        inner.calls(),
        4,
        "expected RecordingFetcher to fetch separately per (destination, origin) when CORS enforcement is enabled"
      );
    });

    Ok(())
  }

  #[test]
  fn recording_fetcher_does_not_partition_cors_mode_requests_without_enforcement() -> Result<()> {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "0".to_string(),
    )])));

    with_thread_runtime_toggles(toggles, || {
      let inner = Arc::new(OriginSensitiveFetcher::default());
      let recording = RecordingFetcher::new(inner.clone());

      for (destination, url) in [
        (FetchDestination::Font, "https://example.com/font.woff2"),
        (FetchDestination::ImageCors, "https://example.com/img.png"),
      ] {
        let req_a =
          FetchRequest::new(url, destination).with_referrer("https://a.test/page.html");
        let req_b =
          FetchRequest::new(url, destination).with_referrer("https://b.test/page.html");

        let a = recording.fetch_with_request(req_a).expect("origin A fetch");
        let b = recording.fetch_with_request(req_b).expect("origin B fetch");

        assert_eq!(a.bytes, b"ok");
        assert_eq!(b.bytes, b"ok");
        assert_eq!(
          a.access_control_allow_origin.as_deref(),
          Some("https://a.test"),
          "unexpected ACAO for origin A"
        );
        assert_eq!(
          b.access_control_allow_origin.as_deref(),
          Some("https://a.test"),
          "expected RecordingFetcher to reuse the cached CORS-mode response when CORS enforcement is disabled"
        );
      }

      assert_eq!(
        inner.calls(),
        2,
        "expected RecordingFetcher to cache by URL when CORS enforcement is disabled"
      );
    });

    Ok(())
  }

  #[test]
  fn crawl_records_per_origin_cors_resources_in_snapshot_when_enforced() -> Result<()> {
    #[derive(Default)]
    struct CrawlFixtureFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl CrawlFixtureFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for CrawlFixtureFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected crawl to use fetch_with_request()");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        const ROOT: &str = "http://root.test/page.html";
        const FRAME_A: &str = "http://a.test/frame.html";
        const FRAME_B: &str = "http://b.test/frame.html";
        const FONT: &str = "http://cdn.test/font.woff2";

        match req.url {
          ROOT => Ok(FetchedResource::with_final_url(
            format!(
              r#"<html><body><iframe src="{FRAME_A}"></iframe><iframe src="{FRAME_B}"></iframe></body></html>"#
            )
            .into_bytes(),
            Some("text/html".to_string()),
            Some(ROOT.to_string()),
          )),
          FRAME_A | FRAME_B => Ok(FetchedResource::with_final_url(
            format!(
              r#"<html><head><style>@font-face{{font-family:"Test";src:url("{FONT}");}}</style></head><body>frame</body></html>"#
            )
            .into_bytes(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          FONT => {
            let origin = req
              .referrer
              .and_then(|referrer| url::Url::parse(referrer).ok())
              .and_then(|parsed| {
                parsed
                  .host_str()
                  .map(|host| format!("{}://{}", parsed.scheme(), host))
              });

            let mut res = FetchedResource::with_final_url(
              b"ok".to_vec(),
              Some("font/woff2".to_string()),
              Some(FONT.to_string()),
            );
            res.status = Some(200);
            res.access_control_allow_origin = origin;
            Ok(res)
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));

    with_thread_runtime_toggles(toggles, || {
      let inner = Arc::new(CrawlFixtureFetcher::default());
      let recording = RecordingFetcher::new(inner.clone());

      let doc = common::render_pipeline::PreparedDocument::new(
        r#"<html><body><iframe src="http://a.test/frame.html"></iframe><iframe src="http://b.test/frame.html"></iframe></body></html>"#.to_string(),
        "http://root.test/page.html".to_string(),
      );

      let render = BundleRenderConfig {
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: fastrender::compat::CompatProfile::default(),
        dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
      };

      crawl_document(&recording, &doc, &render, CrawlMode::Strict).expect("crawl");

      let calls = inner.calls();
      let font_calls: Vec<_> = calls
        .iter()
        .filter(|(url, destination, _)| {
          url == "http://cdn.test/font.woff2" && *destination == FetchDestination::Font
        })
        .collect();
      assert_eq!(
        font_calls.len(),
        2,
        "expected crawl to fetch the same CORS-mode font twice (once per initiating origin) when CORS enforcement is enabled"
      );

      let snapshot = recording.snapshot();
      let origin_a = origin_from_url("http://a.test/frame.html").expect("origin A");
      let origin_b = origin_from_url("http://b.test/frame.html").expect("origin B");
      let font_url = "http://cdn.test/font.woff2";
      let key_a =
        request_partitioned_resource_key(FetchContextKind::Font, font_url, &origin_a);
      let key_b =
        request_partitioned_resource_key(FetchContextKind::Font, font_url, &origin_b);

      assert!(
        snapshot.contains_key(font_url),
        "expected legacy URL entry to be present"
      );
      assert!(
        snapshot.contains_key(&key_a),
        "expected origin A partitioned entry to be present"
      );
      assert!(
        snapshot.contains_key(&key_b),
        "expected origin B partitioned entry to be present"
      );
      assert_eq!(
        snapshot
          .get(&key_a)
          .and_then(|res| res.access_control_allow_origin.as_deref()),
        Some("http://a.test")
      );
      assert_eq!(
        snapshot
          .get(&key_b)
          .and_then(|res| res.access_control_allow_origin.as_deref()),
        Some("http://b.test")
      );
    });

    Ok(())
  }

  #[test]
  fn crawl_prefers_image_cors_for_img_crossorigin_and_upgrades_existing_queue_entry() -> Result<()> {
    #[derive(Default)]
    struct CrossoriginFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl CrossoriginFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for CrossoriginFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><style>body{background:url('/shared.png');}</style></head><body><img src="/shared.png" crossorigin></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/shared.png" => Ok(FetchedResource::with_final_url(
            vec![0u8, 1, 2, 3],
            Some("image/png".to_string()),
            Some(req.url.to_string()),
          )),
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(CrossoriginFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let calls = inner.calls();
    assert!(
      calls.iter().any(|(url, dest, _)| {
        url == "https://example.com/shared.png" && *dest == FetchDestination::ImageCors
      }),
      "expected <img crossorigin> URL to be fetched with ImageCors destination"
    );

    Ok(())
  }

  #[test]
  fn crawl_best_effort_upgrades_img_crossorigin_to_image_cors() -> Result<()> {
    #[derive(Default)]
    struct CrossoriginFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl CrossoriginFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for CrossoriginFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><style>body{background:url('/shared.png');}</style></head><body><img src="/shared.png" crossorigin></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/shared.png" => Ok(FetchedResource::with_final_url(
            vec![0u8, 1, 2, 3],
            Some("image/png".to_string()),
            Some(req.url.to_string()),
          )),
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(CrossoriginFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::BestEffort)?;

    let calls = inner.calls();
    assert!(
      calls.iter().any(|(url, dest, _)| {
        url == "https://example.com/shared.png" && *dest == FetchDestination::ImageCors
      }),
      "expected best-effort crawl to upgrade <img crossorigin> URL to ImageCors destination"
    );
    assert!(
      !calls.iter().any(|(url, dest, _)| {
        url == "https://example.com/shared.png" && *dest == FetchDestination::Image
      }),
      "expected best-effort crawl to avoid fetching the <img crossorigin> URL in no-cors mode"
    );

    Ok(())
  }

  #[test]
  fn crawl_treats_font_face_urls_as_fonts() -> Result<()> {
    #[derive(Default)]
    struct FontFaceFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl FontFaceFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for FontFaceFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="/style.css"></head><body></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/style.css" => Ok(FetchedResource::with_final_url(
            b"@font-face { font-family: X; src: url('/font.svg'); } body { background: url('/bg.png'); }"
              .to_vec(),
            Some("text/css".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/font.svg" => {
            let mut res = FetchedResource::with_final_url(
              b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec(),
              Some("image/svg+xml".to_string()),
              Some(req.url.to_string()),
            );
            res.status = Some(200);
            Ok(res)
          }
          "https://example.com/bg.png" => {
            let mut res = FetchedResource::with_final_url(
              offline_placeholder_png_bytes().to_vec(),
              Some("image/png".to_string()),
              Some(req.url.to_string()),
            );
            res.status = Some(200);
            Ok(res)
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(FontFaceFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let calls = inner.calls();
    assert!(
      calls.iter().any(|(url, dest, _)| {
        url == "https://example.com/font.svg" && *dest == FetchDestination::Font
      }),
      "expected @font-face URL to be fetched as a font"
    );
    assert!(
      calls.iter().any(
        |(url, dest, _)| url == "https://example.com/bg.png" && *dest == FetchDestination::Image
      ),
      "expected non-font CSS url() asset to be fetched as an image"
    );

    Ok(())
  }

  #[test]
  fn crawl_skips_data_urls_discovered_in_css() -> Result<()> {
    #[derive(Default)]
    struct DataUrlCssFetcher {
      urls: Mutex<Vec<String>>,
    }

    impl DataUrlCssFetcher {
      fn urls(&self) -> Vec<String> {
        self
          .urls
          .lock()
          .map(|urls| urls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for DataUrlCssFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.urls.lock().unwrap().push(req.url.to_string());

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="/style.css"></head><body></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/style.css" => Ok(FetchedResource::with_final_url(
            br#"body { background-image: url("data:image/png;base64,AAAA"); }"#.to_vec(),
            Some("text/css".to_string()),
            Some(req.url.to_string()),
          )),
          other if other.to_ascii_lowercase().starts_with("data:") => {
            Err(fastrender::Error::Other(
              "data: URL should not be fetched during bundle crawling".to_string(),
            ))
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(DataUrlCssFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let urls = inner.urls();
    assert_eq!(
      urls,
      vec!["https://example.com/", "https://example.com/style.css"],
      "expected bundle crawler to skip data: URLs entirely"
    );

    Ok(())
  }

  #[test]
  fn crawl_skips_about_blank_documents_discovered_in_html() -> Result<()> {
    #[derive(Default)]
    struct AboutBlankIframeFetcher {
      urls: Mutex<Vec<String>>,
    }

    impl AboutBlankIframeFetcher {
      fn urls(&self) -> Vec<String> {
        self
          .urls
          .lock()
          .map(|urls| urls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for AboutBlankIframeFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.urls.lock().unwrap().push(req.url.to_string());

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><body><iframe src="about:blank"></iframe></body></html>"#.to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          other if other.to_ascii_lowercase().starts_with("about:") => {
            Err(fastrender::Error::Other(
              "about: URL should not be fetched during bundle crawling".to_string(),
            ))
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(AboutBlankIframeFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let urls = inner.urls();
    assert_eq!(
      urls,
      vec!["https://example.com/"],
      "expected bundle crawler to skip about:blank iframe documents"
    );

    Ok(())
  }

  #[test]
  fn crawl_allow_missing_inserts_png_placeholder_for_extensionless_css_urls() -> Result<()> {
    #[derive(Default)]
    struct MissingCssAssetFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl ResourceFetcher for MissingCssAssetFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="/style.css"></head><body></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/style.css" => Ok(FetchedResource::with_final_url(
            b"body { background-image: url('/bg'); }".to_vec(),
            Some("text/css".to_string()),
            Some(req.url.to_string()),
          )),
          // Simulate a cache miss / fetch failure for an extensionless CSS url() asset.
          "https://example.com/bg" => Err(fastrender::Error::Other("missing".to_string())),
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(MissingCssAssetFetcher::default());
    let recording = RecordingFetcher::new(inner);
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::AllowMissing)?;

    let recorded = recording.snapshot();
    let res = recorded
      .get("https://example.com/bg")
      .expect("expected placeholder resource");
    assert_eq!(res.content_type.as_deref(), Some("image/png"));
    assert!(
      res.bytes.starts_with(b"\x89PNG"),
      "expected PNG placeholder bytes"
    );

    Ok(())
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn cache_fetcher_falls_back_from_other_to_image_kind() -> Result<()> {
    #[derive(Default)]
    struct KindFetcher {
      calls: Mutex<Vec<FetchDestination>>,
    }

    impl KindFetcher {
      fn calls(&self) -> Vec<FetchDestination> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for KindFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push(req.destination);
        match req.destination {
          FetchDestination::Image => Ok(FetchedResource::with_final_url(
            b"image-bytes".to_vec(),
            Some("image/png".to_string()),
            Some(req.url.to_string()),
          )),
          _ => Err(fastrender::Error::Other("cache miss".to_string())),
        }
      }
    }

    let inner = Arc::new(KindFetcher::default());
    let inner_dyn: Arc<dyn ResourceFetcher> = inner.clone();
    let fetcher = CacheKindMismatchFallbackFetcher::new(inner_dyn);
    let res = fetcher.fetch_with_request(FetchRequest::new(
      "https://example.com/asset",
      FetchDestination::Other,
    ))?;
    assert_eq!(res.bytes, b"image-bytes".to_vec());

    let calls = inner.calls();
    assert_eq!(
      calls,
      vec![FetchDestination::Other, FetchDestination::Image],
      "expected fall back from Other to Image"
    );

    Ok(())
  }

  #[test]
  fn crawl_rejects_stylesheet_http_errors_without_discovering_deps() -> Result<()> {
    #[derive(Default)]
    struct CssErrorFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl CssErrorFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for CssErrorFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="/style.css"></head><body></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/style.css" => {
            let mut res = FetchedResource::with_final_url(
              br#"<html><body>body { background: url('/bg.png'); }</body></html>"#.to_vec(),
              Some("text/html".to_string()),
              Some(req.url.to_string()),
            );
            res.status = Some(403);
            Ok(res)
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(CssErrorFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    let err = crawl_document(&recording, &doc, &render, CrawlMode::Strict)
      .expect_err("crawl should fail in strict mode for HTTP error pages");
    let msg = err.to_string();
    assert!(msg.contains("https://example.com/style.css"));

    let calls = inner.calls();
    assert!(
      !calls
        .iter()
        .any(|(url, _, _)| url == "https://example.com/bg.png"),
      "should not crawl dependencies from stylesheet error body"
    );

    Ok(())
  }

  #[test]
  fn crawl_uses_frame_referrer_and_origin_for_nested_document_subresources() -> Result<()> {
    #[derive(Default)]
    struct NestedFrameFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl NestedFrameFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for NestedFrameFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://root.test/" => Ok(FetchedResource::with_final_url(
            br#"<html><body><iframe src="https://frame.test/frame.html"></iframe></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://frame.test/frame.html" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="/frame.css"></head><body></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://frame.test/frame.css" => Ok(FetchedResource::with_final_url(
            b"body { background: url('/bg.png'); }".to_vec(),
            Some("text/css".to_string()),
            Some(req.url.to_string()),
          )),
          "https://frame.test/bg.png" => Ok(FetchedResource::with_final_url(
            vec![0u8, 1, 2, 3],
            Some("image/png".to_string()),
            Some(req.url.to_string()),
          )),
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(NestedFrameFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://root.test/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: true,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let calls = inner.calls();
    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://frame.test/frame.html"
        && *dest == FetchDestination::Document
        && referrer.as_deref() == Some("https://root.test/")
    }));

    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://frame.test/frame.css"
        && *dest == FetchDestination::Style
        && referrer.as_deref() == Some("https://frame.test/frame.html")
    }));

    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://frame.test/bg.png"
        && *dest == FetchDestination::Image
        && referrer.as_deref() == Some("https://frame.test/frame.html")
    }));

    Ok(())
  }

  #[test]
  fn crawl_retries_blocked_subresource_for_nested_document_origin() -> Result<()> {
    #[derive(Default)]
    struct BlockedThenAllowedFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl BlockedThenAllowedFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for BlockedThenAllowedFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://root.test/" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="https://frame.test/frame.css"></head><body><iframe src="https://frame.test/frame.html"></iframe></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://frame.test/frame.html" => Ok(FetchedResource::with_final_url(
            br#"<html><head><link rel="stylesheet" href="/frame.css"></head><body></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://frame.test/frame.css" => Ok(FetchedResource::with_final_url(
            b"body { color: black; }".to_vec(),
            Some("text/css".to_string()),
            Some(req.url.to_string()),
          )),
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(BlockedThenAllowedFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://root.test/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: true,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let calls = inner.calls();
    assert!(
      !calls.iter().any(|(url, dest, referrer)| {
        url == "https://frame.test/frame.css"
          && *dest == FetchDestination::Style
          && referrer.as_deref() == Some("https://root.test/")
      }),
      "should not fetch frame.css as a root document subresource under same-origin-only mode"
    );
    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://frame.test/frame.html"
        && *dest == FetchDestination::Document
        && referrer.as_deref() == Some("https://root.test/")
    }));
    assert!(calls.iter().any(|(url, dest, referrer)| {
      url == "https://frame.test/frame.css"
        && *dest == FetchDestination::Style
        && referrer.as_deref() == Some("https://frame.test/frame.html")
    }));

    Ok(())
  }

  #[test]
  fn crawl_selects_responsive_srcset_candidate_matching_dpr() -> Result<()> {
    #[derive(Default)]
    struct ResponsiveImageFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl ResponsiveImageFetcher {
      fn calls(&self) -> Vec<(String, FetchDestination, Option<String>)> {
        self
          .calls
          .lock()
          .map(|calls| calls.clone())
          .unwrap_or_default()
      }
    }

    impl ResourceFetcher for ResponsiveImageFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><body><img srcset="/1x.png 1x, /2x.png 2x" sizes="100vw" src="/fallback.png"></body></html>"#
              .to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/2x.png" | "https://example.com/fallback.png" => Ok(
            FetchedResource::with_final_url(
              vec![0u8, 1, 2, 3],
              Some("image/png".to_string()),
              Some(req.url.to_string()),
            ),
          ),
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(ResponsiveImageFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 2.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let calls = inner.calls();
    assert!(calls
      .iter()
      .any(|(url, _, _)| url == "https://example.com/2x.png"));
    assert!(
      !calls
        .iter()
        .any(|(url, _, _)| url == "https://example.com/1x.png"),
      "should not fetch lower-density srcset candidate when dpr=2"
    );

    Ok(())
  }

  #[test]
  fn crawl_truncates_large_images_to_png_placeholder() -> Result<()> {
    #[derive(Default)]
    struct LargeImageFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl ResourceFetcher for LargeImageFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><body><img src="/large.png"></body></html>"#.to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/large.png" => {
            // Exceed `MAX_CRAWL_IMAGE_BYTES` (currently 1_000_000) to exercise truncation logic.
            let bytes = vec![0u8; 1_000_001];
            let mut res = FetchedResource::with_final_url(
              bytes,
              Some("image/png".to_string()),
              Some(req.url.to_string()),
            );
            res.status = Some(200);
            Ok(res)
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(LargeImageFetcher::default());
    let recording = RecordingFetcher::new(inner.clone());
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let recorded = recording.snapshot();
    let res = recorded
      .get("https://example.com/large.png")
      .expect("expected large image resource to be recorded");
    assert_eq!(res.content_type.as_deref(), Some("image/png"));
    assert_eq!(res.bytes, offline_placeholder_png_bytes().to_vec());

    Ok(())
  }

  #[test]
  fn crawl_truncates_large_images_without_content_type() -> Result<()> {
    #[derive(Default)]
    struct LargeImageNoContentTypeFetcher {
      calls: Mutex<Vec<(String, FetchDestination, Option<String>)>>,
    }

    impl ResourceFetcher for LargeImageNoContentTypeFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push((
          req.url.to_string(),
          req.destination,
          req.referrer.map(|r| r.to_string()),
        ));

        match req.url {
          "https://example.com/" => Ok(FetchedResource::with_final_url(
            br#"<html><body><img src="/large"></body></html>"#.to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          )),
          "https://example.com/large" => {
            // Exceed `MAX_CRAWL_IMAGE_BYTES` (currently 1_000_000) to exercise truncation logic.
            // This response intentionally omits a content-type; truncation should still apply when
            // the discovered destination is `Image`.
            let bytes = vec![0u8; 1_000_001];
            let mut res = FetchedResource::with_final_url(bytes, None, Some(req.url.to_string()));
            res.status = Some(200);
            Ok(res)
          }
          other => Err(fastrender::Error::Other(format!(
            "unexpected fetch: {other}"
          ))),
        }
      }
    }

    let inner = Arc::new(LargeImageNoContentTypeFetcher::default());
    let recording = RecordingFetcher::new(inner);
    let (doc, _) = fetch_document(&recording, "https://example.com/")?;

    let render = BundleRenderConfig {
      viewport: (1200, 800),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: fastrender::compat::CompatProfile::default(),
      dom_compat_mode: fastrender::dom::DomCompatibilityMode::default(),
    };

    crawl_document(&recording, &doc, &render, CrawlMode::Strict)?;

    let recorded = recording.snapshot();
    let res = recorded
      .get("https://example.com/large")
      .expect("expected large image resource to be recorded");
    assert_eq!(res.content_type.as_deref(), Some("image/png"));
    assert_eq!(res.bytes, offline_placeholder_png_bytes().to_vec());

    Ok(())
  }
}
