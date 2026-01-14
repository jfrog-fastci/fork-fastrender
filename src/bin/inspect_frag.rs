use fastrender::cli_utils as common;

use clap::Parser;
use common::args::{
  parse_viewport, AnimationTimeArgs, CompatArgs, DiskCacheArgs, MediaPreferenceArgs, MediaTypeArg,
};
use common::media_prefs::MediaPreferences;
use common::render_pipeline::CLI_RENDER_STACK_SIZE;
use fastrender::api::FastRender;
use fastrender::debug::runtime::{self, RuntimeToggles};
use fastrender::dom::DomNodeType;
use fastrender::geometry::{Point, Rect};
use fastrender::image_output::encode_image;
use fastrender::pageset::{pageset_short_hash, pageset_stem};
#[cfg(not(feature = "disk_cache"))]
use fastrender::resource::CachingFetcher;
#[cfg(feature = "disk_cache")]
use fastrender::resource::DiskCachingFetcher;
use fastrender::resource::{
  CachingFetcherConfig, ResourceFetcher, ResourcePolicy, DEFAULT_ACCEPT_LANGUAGE,
  DEFAULT_USER_AGENT,
};
use fastrender::text::font_db::FontConfig;
use fastrender::tree::box_tree::{BoxNode, BoxTree};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
#[cfg(feature = "disk_cache")]
use fastrender::HttpFetcher;
use fastrender::{snapshot_pipeline, OutputFormat, RenderArtifactRequest, RenderOptions};
use serde_json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tiny_skia::{Color, Paint, PathBuilder, Stroke, Transform};
use url::Url;

const DEFAULT_HTML_DIR: &str = fastrender::pageset::CACHE_HTML_DIR;
const DEFAULT_ASSET_CACHE_DIR: &str = "fetches/assets";

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Inspect fragment/box trees for a given HTML file.
#[derive(Parser, Debug)]
#[command(name = "inspect_frag", version, about)]
struct Args {
  /// HTML file path (or file:// URL) to inspect.
  ///
  /// If the file is a cached pageset HTML (`*.html`) and a `*.html.meta` sidecar exists, the meta
  /// `url:` field is used as the base hint by default so relative subresources resolve correctly.
  #[arg(value_name = "FILE", required_unless_present = "pageset")]
  file: Option<String>,

  /// Load cached pageset HTML from `<html-dir>/<cache_stem>.html` by URL or stem.
  ///
  /// Examples:
  /// - `--pageset https://example.com`
  /// - `--pageset example.com`
  /// - `--pageset example.com--deadbeef` (collision-aware cache stem)
  #[arg(long, value_name = "URL_OR_STEM", conflicts_with = "file")]
  pageset: Option<String>,

  /// Directory containing cached HTML (defaults to fetches/html).
  #[arg(long, value_name = "DIR", default_value = DEFAULT_HTML_DIR)]
  html_dir: PathBuf,

  /// Override the base URL used to resolve relative subresources.
  ///
  /// This overrides any `.html.meta`-derived `url:` base hint.
  #[arg(long, value_name = "URL")]
  base_hint: Option<String>,

  /// Deny HTTP/HTTPS subresource loads (treat the input as offline).
  ///
  /// This is useful when inspecting offline fixtures that may still contain leftover remote URLs.
  /// Relative paths, `file://`, and `data:` URLs are still permitted.
  #[arg(long)]
  deny_network: bool,

  /// Patch fixture HTML before rendering to align with the Chrome baseline harness.
  ///
  /// This matches `render_fixtures --patch-html-for-chrome-baseline`:
  /// - forces a light color scheme + white root background (unless `--allow-dark-mode`),
  /// - injects an offline Content-Security-Policy tag,
  /// - hides scrollbars,
  /// - disables CSS animations/transitions (unless `--allow-animations`).
  #[arg(long)]
  patch_html_for_chrome_baseline: bool,

  /// Allow CSS animations/transitions while patching fixture HTML for Chrome baselines.
  ///
  /// Has no effect unless `--patch-html-for-chrome-baseline` is enabled.
  #[arg(long)]
  allow_animations: bool,

  /// Allow dark-mode / prefers-color-scheme defaults (do not force a white background).
  ///
  /// Has no effect unless `--patch-html-for-chrome-baseline` is enabled.
  #[arg(long)]
  allow_dark_mode: bool,

  /// Write deterministic pipeline stage snapshots into this directory.
  ///
  /// Writes `dom.json`, `composed_dom.json`, `styled.json`, `box_tree.json`, `fragment_tree.json`,
  /// and `display_list.json`. When filters are provided, the dumps are restricted to the first
  /// matching subtree.
  #[arg(long, value_name = "DIR")]
  dump_json: Option<PathBuf>,

  /// Write a deterministic snapshot of the imported `dom2::Document` into this JSON file.
  ///
  /// This imports the parsed renderer DOM into `dom2` and then serializes it via
  /// `fastrender::debug::snapshot::snapshot_dom2`. This is useful for debugging DOM connectedness
  /// and inert subtree handling without executing layout/paint.
  #[arg(long, value_name = "JSON")]
  dump_dom2_json: Option<PathBuf>,

  /// Dump the computed custom property store for the styled root node to JSON.
  ///
  /// When used together with `--filter-selector/--filter-id`, this dumps the custom properties for
  /// the matched node. The output is written into the `--dump-json` directory as
  /// `custom_properties.json`.
  #[arg(long, requires = "dump_json")]
  dump_custom_properties: bool,

  /// Only include custom properties whose name starts with this prefix (repeatable).
  ///
  /// When omitted, all custom properties are included.
  #[arg(long, value_name = "PREFIX", requires = "dump_custom_properties")]
  custom_property_prefix: Vec<String>,

  /// Maximum number of custom properties to dump (after filtering/sorting).
  #[arg(
    long,
    default_value_t = 512,
    value_name = "N",
    requires = "dump_custom_properties"
  )]
  custom_properties_limit: usize,

  /// Print a combined pipeline snapshot JSON to stdout.
  #[arg(long)]
  dump_snapshot: bool,

  /// Render the page to a PNG and draw debug overlays (fragment bounds).
  #[arg(long, value_name = "PNG")]
  render_overlay: Option<PathBuf>,

  /// Restrict dumps/overlays/traces to the first node matching this selector.
  #[arg(long, value_name = "SELECTOR")]
  filter_selector: Option<String>,

  /// Restrict dumps/overlays/traces to the first node matching this id attribute.
  #[arg(long, value_name = "ID")]
  filter_id: Option<String>,

  /// Trace the fragment ancestry path to the first text fragment containing this substring
  /// (repeatable).
  #[arg(long, value_name = "SUBSTRING")]
  trace_text: Vec<String>,

  /// Trace the fragment ancestry path to the first fragment associated with this box id
  /// (repeatable).
  #[arg(long, value_name = "BOX_ID")]
  trace_box: Vec<usize>,

  /// Dump the fragment subtree for the first fragment associated with this box id.
  #[arg(long, value_name = "BOX_ID")]
  dump_fragment: Option<usize>,

  /// Find tall skinny fragments (diagnostic).
  #[arg(long)]
  find_skinny_fragments: bool,

  /// Maximum width in CSS px for `--find-skinny-fragments`.
  #[arg(long, default_value_t = 5.0, value_name = "PX")]
  skinny_max_width: f32,

  /// Minimum height in CSS px for `--find-skinny-fragments`.
  #[arg(long, default_value_t = 600.0, value_name = "PX")]
  skinny_min_height: f32,

  /// Viewport size as WxH (e.g., 1200x800).
  #[arg(long, value_parser = parse_viewport, default_value = "1200x800")]
  viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset.
  #[arg(long, default_value = "1.0")]
  dpr: f32,

  #[command(flatten)]
  animation_time: AnimationTimeArgs,

  /// Additional deterministic font directories to load (can be repeated).
  #[arg(long, value_name = "DIR")]
  font_dir: Vec<PathBuf>,

  /// Enable system font discovery in addition to bundled fonts.
  ///
  /// When omitted, `inspect_frag` defaults to bundled fonts to keep page-loop/fixture workflows
  /// deterministic across machines.
  #[arg(long)]
  system_fonts: bool,

  /// Media type for evaluating media queries.
  #[arg(long, value_enum, default_value_t = MediaTypeArg::Screen)]
  media: MediaTypeArg,

  /// Horizontal scroll offset in CSS px.
  #[arg(long, default_value = "0.0")]
  scroll_x: f32,

  /// Vertical scroll offset in CSS px.
  #[arg(long, default_value = "0.0")]
  scroll_y: f32,

  #[command(flatten)]
  media_prefs: MediaPreferenceArgs,

  #[command(flatten)]
  compat: CompatArgs,

  /// Override the User-Agent header.
  #[arg(long, default_value = DEFAULT_USER_AGENT)]
  user_agent: String,

  /// Override the Accept-Language header.
  #[arg(long, default_value = DEFAULT_ACCEPT_LANGUAGE)]
  accept_language: String,

  /// Disable serving fresh cached HTTP responses without revalidation.
  ///
  /// This matches pageset tooling semantics when built with `disk_cache`.
  #[arg(long)]
  no_http_freshness: bool,

  /// Offline mode: forbid network I/O (requires `disk_cache`).
  ///
  /// Disk cache hits are still served. Cache misses surface as normal fetch errors/diagnostics.
  #[arg(long)]
  offline: bool,

  #[command(flatten)]
  disk_cache: DiskCacheArgs,

  /// Disk cache directory for subresources (defaults to fetches/assets).
  ///
  /// Note: this only has an effect when the binary is built with the `disk_cache` cargo feature.
  #[arg(long, default_value = DEFAULT_ASSET_CACHE_DIR)]
  cache_dir: PathBuf,

  /// Abort after this many seconds.
  #[arg(long)]
  timeout: Option<u64>,
}

#[derive(Debug, Clone)]
struct InputDocument {
  path: PathBuf,
  html: String,
  base_hint: String,
}

fn write_pretty_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent)?;
    }
  }
  let json = serde_json::to_string_pretty(value)
    .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
  fs::write(path, json)?;
  Ok(())
}

fn html_meta_path(html_path: &Path) -> PathBuf {
  let mut meta_path = html_path.to_path_buf();
  if let Some(ext) = meta_path.extension().and_then(|e| e.to_str()) {
    meta_path.set_extension(format!("{ext}.meta"));
  } else {
    meta_path.set_extension("meta");
  }
  meta_path
}

fn parse_collision_suffix(raw: &str) -> Option<(&str, &str)> {
  raw
    .rsplit_once("--")
    .filter(|(_, suffix)| suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()))
}

fn resolve_pageset_html_path(html_dir: &Path, url_or_stem: &str) -> io::Result<PathBuf> {
  let trimmed = url_or_stem.trim();
  let Some(stem) = pageset_stem(trimmed) else {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("invalid pageset URL/stem: {url_or_stem}"),
    ));
  };

  if let Some((base, suffix)) = parse_collision_suffix(trimmed) {
    if let Some(base_stem) = pageset_stem(base) {
      let cache_stem = format!("{base_stem}--{}", suffix.to_ascii_lowercase());
      let path = html_dir.join(format!("{cache_stem}.html"));
      if path.exists() {
        return Ok(path);
      }
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
          "pageset cache not found for {cache_stem} (expected {})",
          path.display()
        ),
      ));
    }
  }

  let direct = html_dir.join(format!("{stem}.html"));
  if direct.exists() {
    return Ok(direct);
  }

  // When the caller provides a full URL, try the collision-hash cache stem used by pageset tools.
  // This matches `fastrender::pageset::PagesetEntry` naming (stem + `--` + short hash) so that
  // `inspect_frag --pageset <URL>` works even for colliding stems.
  if matches!(Url::parse(trimmed).map(|u| u.scheme().to_ascii_lowercase()), Ok(s) if s == "http" || s == "https")
  {
    let cache_stem = format!("{stem}--{}", pageset_short_hash(trimmed));
    let hashed = html_dir.join(format!("{cache_stem}.html"));
    if hashed.exists() {
      return Ok(hashed);
    }
  }

  let mut collisions: Vec<PathBuf> = Vec::new();
  if let Ok(entries) = fs::read_dir(html_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if path.extension().and_then(|e| e.to_str()) != Some("html") {
        continue;
      }
      let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
      if let Some((base, _suffix)) = parse_collision_suffix(file_stem) {
        if base == stem {
          collisions.push(path);
        }
      }
    }
  }

  if collisions.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!(
        "pageset cache not found for {url_or_stem} (expected {} or {stem}--????????.html under {})",
        direct.display(),
        html_dir.display()
      ),
    ));
  }

  collisions.sort();
  if collisions.len() == 1 {
    return Ok(collisions[0].clone());
  }

  let listed = collisions
    .iter()
    .filter_map(|p| {
      p.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
    })
    .collect::<Vec<_>>()
    .join(", ");

  Err(io::Error::new(
    io::ErrorKind::Other,
    format!(
      "multiple cached pages match stem {stem} under {}: {listed}. Pass the full cache stem (e.g. {stem}--deadbeef) to disambiguate.",
      html_dir.display()
    ),
  ))
}

fn parse_file_arg_to_path(raw: &str) -> io::Result<PathBuf> {
  if let Ok(url) = Url::parse(raw) {
    if url.scheme() == "file" {
      return url
        .to_file_path()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file:// URL path"));
    }
    // Treat non-file URLs as paths (backwards compatible behavior).
  }
  Ok(PathBuf::from(raw))
}

fn load_input_document(args: &Args) -> io::Result<InputDocument> {
  let path = if let Some(pageset) = &args.pageset {
    resolve_pageset_html_path(&args.html_dir, pageset)?
  } else {
    let raw = args
      .file
      .as_deref()
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing FILE argument"))?;
    parse_file_arg_to_path(raw)?
  };

  let cached = common::render_pipeline::read_cached_document(&path).map_err(|err| {
    io::Error::new(
      io::ErrorKind::Other,
      format!("failed to read cached HTML {}: {err}", path.display()),
    )
  })?;
  let mut doc = cached.document;

  if let Some(base) = args.base_hint.as_deref() {
    doc = doc.with_base_override(Some(base));
  } else {
    let meta_exists = html_meta_path(&path).exists();
    // If there is no `.meta` URL hint, ensure we use a valid absolute file:// base hint (the shared
    // cached doc helper uses `file://{path.display()}`, which may be relative).
    if !meta_exists || doc.base_hint.starts_with("file://") {
      let file_url = common::file_url::file_url_for_path(&path);
      doc = doc.with_base_override(Some(&file_url));
    }
  }

  if args.patch_html_for_chrome_baseline {
    let patched = common::fixture_html_patch::patch_html_bytes(
      doc.html.as_bytes(),
      Some(&doc.base_hint),
      true, // disable JS
      !args.allow_animations,
      args.allow_dark_mode,
    );
    doc.html = String::from_utf8(patched).map_err(|err| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("patched HTML is not valid UTF-8: {err}"),
      )
    })?;
  }

  Ok(InputDocument {
    path,
    html: doc.html,
    base_hint: doc.base_hint,
  })
}

#[cfg(feature = "disk_cache")]
#[derive(Clone)]
struct OfflineAwareFetcher {
  offline: bool,
  http: HttpFetcher,
}

#[cfg(feature = "disk_cache")]
impl OfflineAwareFetcher {
  fn new(http: HttpFetcher, offline: bool) -> Self {
    Self { offline, http }
  }

  fn offline_error(url: &str) -> fastrender::Error {
    fastrender::Error::Resource(fastrender::error::ResourceError::new(
      url,
      "offline mode: network disabled",
    ))
  }
}

#[cfg(feature = "disk_cache")]
impl ResourceFetcher for OfflineAwareFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<fastrender::resource::FetchedResource> {
    if self.offline {
      Err(Self::offline_error(url))
    } else {
      self.http.fetch(url)
    }
  }

  fn fetch_with_request(
    &self,
    req: fastrender::resource::FetchRequest<'_>,
  ) -> fastrender::Result<fastrender::resource::FetchedResource> {
    if self.offline {
      Err(Self::offline_error(req.url))
    } else {
      self.http.fetch_with_request(req)
    }
  }

  fn fetch_with_request_and_validation(
    &self,
    req: fastrender::resource::FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> fastrender::Result<fastrender::resource::FetchedResource> {
    if self.offline {
      Err(Self::offline_error(req.url))
    } else {
      self
        .http
        .fetch_with_request_and_validation(req, etag, last_modified)
    }
  }

  fn request_header_value(
    &self,
    req: fastrender::resource::FetchRequest<'_>,
    header_name: &str,
  ) -> Option<String> {
    self.http.request_header_value(req, header_name)
  }
}

fn build_fetcher(args: &Args) -> io::Result<Arc<dyn ResourceFetcher>> {
  let timeout_budget = args.timeout.map(Duration::from_secs);
  let http = common::render_pipeline::build_http_fetcher(
    &args.user_agent,
    &args.accept_language,
    timeout_budget,
  );

  let honor_http_freshness = cfg!(feature = "disk_cache") && !args.no_http_freshness;
  let memory_config = CachingFetcherConfig {
    honor_http_cache_freshness: honor_http_freshness,
    ..CachingFetcherConfig::default()
  };

  #[cfg(feature = "disk_cache")]
  {
    let mut disk_config = args.disk_cache.to_config();
    disk_config.namespace = Some(common::render_pipeline::disk_cache_namespace(
      &args.user_agent,
      &args.accept_language,
    ));

    let base = OfflineAwareFetcher::new(http, args.offline);

    return Ok(Arc::new(DiskCachingFetcher::with_configs(
      base,
      &args.cache_dir,
      memory_config,
      disk_config,
    )));
  }

  #[cfg(not(feature = "disk_cache"))]
  {
    if args.offline {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "inspect_frag: --offline requires the binary to be built with --features disk_cache",
      ));
    }
    return Ok(Arc::new(CachingFetcher::with_config(http, memory_config)));
  }
}

fn find_dom_node_by_preorder_id(
  root: &fastrender::dom::DomNode,
  target_id: usize,
) -> Option<fastrender::dom::DomNode> {
  fn walk(
    node: &fastrender::dom::DomNode,
    next: &mut usize,
    target_id: usize,
  ) -> Option<fastrender::dom::DomNode> {
    let id = *next;
    *next += 1;
    if id == target_id {
      return Some(node.clone());
    }
    for child in &node.children {
      if let Some(found) = walk(child, next, target_id) {
        return Some(found);
      }
    }
    None
  }

  let mut next = 1usize;
  walk(root, &mut next, target_id)
}

fn find_styled_node_by_id(
  root: &fastrender::style::cascade::StyledNode,
  target_id: usize,
) -> Option<fastrender::style::cascade::StyledNode> {
  if root.node_id == target_id {
    return Some(root.clone());
  }
  for child in &root.children {
    if let Some(found) = find_styled_node_by_id(child, target_id) {
      return Some(found);
    }
  }
  None
}

fn collect_styled_node_ids(
  root: &fastrender::style::cascade::StyledNode,
  out: &mut HashSet<usize>,
) {
  out.insert(root.node_id);
  for child in &root.children {
    collect_styled_node_ids(child, out);
  }
}

fn filter_box_subtree(node: &BoxNode, allowed_styled_ids: &HashSet<usize>) -> Option<BoxNode> {
  let children: Vec<BoxNode> = node
    .children
    .iter()
    .filter_map(|child| filter_box_subtree(child, allowed_styled_ids))
    .collect();
  let keep_self = node
    .styled_node_id
    .is_some_and(|id| allowed_styled_ids.contains(&id));
  if !keep_self && children.is_empty() {
    return None;
  }
  Some(BoxNode {
    style: node.style.clone(),
    original_display: node.original_display,
    starting_style: node.starting_style.clone(),
    box_type: node.box_type.clone(),
    children,
    footnote_body: node.footnote_body.clone(),
    id: node.id,
    debug_info: node.debug_info.clone(),
    styled_node_id: node.styled_node_id,
    generated_pseudo: node.generated_pseudo,
    implicit_anchor_box_id: node.implicit_anchor_box_id,
    form_control: node.form_control.clone(),
    table_cell_span: node.table_cell_span,
    table_column_span: node.table_column_span,
    first_line_style: node.first_line_style.clone(),
    first_letter_style: node.first_letter_style.clone(),
  })
}

fn collect_box_ids(node: &BoxNode, out: &mut HashSet<usize>) {
  out.insert(node.id);
  for child in &node.children {
    collect_box_ids(child, out);
  }
}

fn fragment_box_id(node: &FragmentNode) -> Option<usize> {
  match &node.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Text { box_id, .. }
    | FragmentContent::Replaced { box_id, .. } => *box_id,
    FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. }
    | FragmentContent::Line { .. } => None,
  }
}

fn filter_fragment_subtree(
  node: &FragmentNode,
  allowed_box_ids: &HashSet<usize>,
) -> Option<FragmentNode> {
  let children: Vec<FragmentNode> = node
    .children
    .iter()
    .filter_map(|child| filter_fragment_subtree(child, allowed_box_ids))
    .collect();
  let keep_self = fragment_box_id(node).is_some_and(|id| allowed_box_ids.contains(&id));
  if !keep_self && children.is_empty() {
    return None;
  }
  let mut filtered = node.clone();
  filtered.set_children(children);
  Some(filtered)
}

fn overlay_rect_for_fragment(
  abs: Rect,
  dpr: f32,
  offset_x: f32,
  offset_y: f32,
  pixmap_w: u32,
  pixmap_h: u32,
) -> Option<tiny_skia::Rect> {
  let x = (abs.x() + offset_x) * dpr;
  let y = (abs.y() + offset_y) * dpr;
  let w = abs.width() * dpr;
  let h = abs.height() * dpr;

  if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
    return None;
  }
  if w <= 0.0 || h <= 0.0 {
    return None;
  }

  // `tiny_skia::Pixmap::stroke_path` can be very slow when asked to rasterize paths with huge
  // coordinates/bounds even if the result is fully clipped away. Real-world pages (including
  // imdb.com) often place visually-hidden fragments thousands of pixels off-screen, which can
  // lead to enormous fragment bounds. Since the overlay is rendered onto a viewport-sized pixmap,
  // clamp each overlay rect to the pixmap bounds to keep raster work bounded.
  let pixmap_w = pixmap_w as f32;
  let pixmap_h = pixmap_h as f32;

  let x1 = x + w;
  let y1 = y + h;
  if !(x1.is_finite() && y1.is_finite()) {
    return None;
  }

  let cx0 = x.max(0.0);
  let cy0 = y.max(0.0);
  let cx1 = x1.min(pixmap_w);
  let cy1 = y1.min(pixmap_h);
  let cw = cx1 - cx0;
  let ch = cy1 - cy0;
  if cw <= 0.0 || ch <= 0.0 {
    return None;
  }

  tiny_skia::Rect::from_xywh(cx0, cy0, cw, ch)
}

fn draw_fragment_overlays(
  pixmap: &mut tiny_skia::Pixmap,
  tree: &FragmentTree,
  dpr: f32,
  scroll_x: f32,
  scroll_y: f32,
) {
  fn color_for(fragment: &FragmentNode) -> Color {
    match &fragment.content {
      FragmentContent::Block { .. } => Color::from_rgba8(255, 0, 0, 160),
      FragmentContent::Inline { .. } => Color::from_rgba8(0, 200, 0, 160),
      FragmentContent::Line { .. } => Color::from_rgba8(255, 165, 0, 160),
      FragmentContent::Text { .. } => Color::from_rgba8(0, 128, 255, 160),
      FragmentContent::Replaced { .. } => Color::from_rgba8(200, 0, 200, 160),
      FragmentContent::RunningAnchor { .. } => Color::from_rgba8(0, 200, 200, 160),
      FragmentContent::FootnoteAnchor { .. } => Color::from_rgba8(0, 0, 0, 160),
    }
  }

  let offset_x = -scroll_x;
  let offset_y = -scroll_y;
  let pixmap_w = pixmap.width();
  let pixmap_h = pixmap.height();

  let mut stack: Vec<(Point, &FragmentNode)> = Vec::new();
  for root in tree.additional_fragments.iter().rev() {
    stack.push((Point::ZERO, root));
  }
  stack.push((Point::ZERO, &tree.root));

  let stroke = Stroke {
    width: (1.0 * dpr).max(1.0),
    ..Stroke::default()
  };

  while let Some((origin, fragment)) = stack.pop() {
    let rect = fragment.bounds;
    let abs = Rect::from_xywh(
      rect.x() + origin.x,
      rect.y() + origin.y,
      rect.width(),
      rect.height(),
    );
    if let Some(rect) = overlay_rect_for_fragment(abs, dpr, offset_x, offset_y, pixmap_w, pixmap_h)
    {
      let mut pb = PathBuilder::new();
      pb.push_rect(rect);
      if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(color_for(fragment));
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
      }
    }

    let next_origin = Point::new(abs.x(), abs.y());
    for child in fragment.children.iter().rev() {
      stack.push((next_origin, child));
    }
  }
}

fn format_debug_info(node: &BoxNode) -> String {
  let mut label = node
    .debug_info
    .as_ref()
    .map(|info| info.to_selector())
    .unwrap_or_else(|| format!("{:?}", node.box_type));

  let mut spans = Vec::new();
  let colspan = node.table_colspan();
  if colspan > 1 {
    spans.push(format!("colspan={colspan}"));
  }
  let rowspan = node.table_rowspan();
  if rowspan > 1 {
    spans.push(format!("rowspan={rowspan}"));
  }
  let column_span = node.table_column_span();
  if column_span > 1 {
    spans.push(format!("column-span={column_span}"));
  }
  if !spans.is_empty() {
    label.push_str(&format!(" ({})", spans.join(" ")));
  }

  label
}

fn collect_box_debug(node: &BoxNode, out: &mut HashMap<usize, String>) {
  let mut stack: Vec<&BoxNode> = vec![node];
  while let Some(node) = stack.pop() {
    out.insert(node.id, format_debug_info(node));
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

fn find_box_by_id<'a>(node: &'a BoxNode, target: usize) -> Option<&'a BoxNode> {
  let mut stack: Vec<&'a BoxNode> = vec![node];
  while let Some(node) = stack.pop() {
    if node.id == target {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn style_summary(style: &fastrender::style::ComputedStyle) -> String {
  let mut out = format!(
    "display={:?} position={:?} visibility={:?} opacity={:.2} width={:?} height={:?} min=({:?},{:?}) max=({:?},{:?}) overflow=({:?},{:?}) flex=({:.2},{:.2},{:?}) order={}",
    style.display,
    style.position,
    style.visibility,
    style.opacity,
    style.width,
    style.height,
    style.min_width,
    style.min_height,
    style.max_width,
    style.max_height,
    style.overflow_x,
    style.overflow_y,
    style.flex_grow,
    style.flex_shrink,
    style.flex_basis,
    style.order,
  );

  if !style.grid_template_columns.is_empty()
    || !style.grid_template_rows.is_empty()
    || style.grid_gap.value != 0.0
    || style.grid_row_gap.value != 0.0
    || style.grid_column_gap.value != 0.0
  {
    out.push_str(&format!(
      " grid_template=({}c,{}r) gap=({}{:?} row={} col={})",
      style.grid_template_columns.len(),
      style.grid_template_rows.len(),
      style.grid_gap.to_px(),
      style.grid_gap.unit,
      style.grid_row_gap.to_px(),
      style.grid_column_gap.to_px()
    ));
  }

  if style.grid_column_start != 0
    || style.grid_column_end != 0
    || style.grid_column_raw.is_some()
    || style.grid_row_start != 0
    || style.grid_row_end != 0
    || style.grid_row_raw.is_some()
  {
    out.push_str(&format!(
      " grid_place=col({},{},{:?}) row({},{},{:?})",
      style.grid_column_start,
      style.grid_column_end,
      style.grid_column_raw,
      style.grid_row_start,
      style.grid_row_end,
      style.grid_row_raw
    ));
  }

  if !style.background_layers.is_empty() {
    let summaries: Vec<String> = style
      .background_layers
      .iter()
      .map(|layer| match &layer.image {
        Some(fastrender::style::types::BackgroundImage::Url(url)) => format!("url({})", url.url),
        Some(fastrender::style::types::BackgroundImage::None) => "none".to_string(),
        Some(_) => "gradient".to_string(),
        None => "(none)".to_string(),
      })
      .collect();
    out.push_str(&format!(" backgrounds={:?}", summaries));
  }

  out
}

fn fmt_rgba_compact(rgba: fastrender::Rgba) -> String {
  format!("rgba({},{},{},{:.2})", rgba.r, rgba.g, rgba.b, rgba.a)
}

fn find_styled_element_by_tag<'a>(
  node: &'a fastrender::style::cascade::StyledNode,
  tag: &str,
) -> Option<&'a fastrender::style::cascade::StyledNode> {
  let mut stack: Vec<&'a fastrender::style::cascade::StyledNode> = vec![node];
  while let Some(node) = stack.pop() {
    if let DomNodeType::Element { tag_name, .. } = &node.node.node_type {
      if tag_name.eq_ignore_ascii_case(tag) {
        return Some(node);
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn absolute_rect(fragment: &FragmentNode, offset: Point) -> (Rect, Point) {
  let abs = Rect::from_xywh(
    fragment.bounds.x() + offset.x,
    fragment.bounds.y() + offset.y,
    fragment.bounds.width(),
    fragment.bounds.height(),
  );
  (abs, abs.origin)
}

fn label_fragment(
  fragment: &FragmentNode,
  abs: Rect,
  box_debug: &HashMap<usize, String>,
) -> String {
  let mut label = match &fragment.content {
    FragmentContent::Block { .. } => "block".to_string(),
    FragmentContent::Inline { .. } => "inline".to_string(),
    FragmentContent::Line { .. } => "line".to_string(),
    FragmentContent::Text { text, .. } => {
      format!("text {:?}", text.chars().take(40).collect::<String>())
    }
    FragmentContent::Replaced { .. } => "replaced".to_string(),
    FragmentContent::RunningAnchor { .. } => "running-anchor".to_string(),
    FragmentContent::FootnoteAnchor { .. } => "footnote-anchor".to_string(),
  };

  label.push_str(&format!(
    " @ ({:.1},{:.1},{:.1},{:.1})",
    abs.x(),
    abs.y(),
    abs.width(),
    abs.height()
  ));

  if let Some(style) = fragment.style.as_deref() {
    label.push_str(&format!(
      " display={:?} pos={:?} z={:?}",
      style.display, style.position, style.z_index
    ));
  }

  if fragment.fragment_count > 1 {
    label.push_str(&format!(
      " fragmentainer={}/{}",
      fragment.fragmentainer_index + 1,
      fragment.fragment_count
    ));
  }

  if let Some(box_id) = match &fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. }
    | FragmentContent::Text { box_id, .. } => *box_id,
    _ => None,
  } {
    if let Some(debug) = box_debug.get(&box_id) {
      label.push_str(&format!(" box#{box_id} {debug}"));
    } else {
      label.push_str(&format!(" box#{box_id}"));
    }
  }

  label
}

fn find_fragment_path_for_text(
  fragment: &FragmentNode,
  offset: Point,
  needle: &str,
  box_debug: &HashMap<usize, String>,
  out_path: &mut Vec<String>,
) -> bool {
  struct Frame<'a> {
    node: &'a FragmentNode,
    next_child: usize,
    label: String,
    child_offset: Point,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  let (abs, child_offset) = absolute_rect(fragment, offset);
  stack.push(Frame {
    node: fragment,
    next_child: 0,
    label: label_fragment(fragment, abs, box_debug),
    child_offset,
  });

  while let Some(frame) = stack.last_mut() {
    if let FragmentContent::Text { text, .. } = &frame.node.content {
      if text.contains(needle) {
        out_path.clear();
        out_path.extend(stack.iter().map(|f| f.label.clone()));
        return true;
      }
    }

    if frame.next_child >= frame.node.children.len() {
      stack.pop();
      continue;
    }

    let child_offset = frame.child_offset;
    let child = &frame.node.children[frame.next_child];
    frame.next_child += 1;
    let (abs, child_offset) = absolute_rect(child, child_offset);
    stack.push(Frame {
      node: child,
      next_child: 0,
      label: label_fragment(child, abs, box_debug),
      child_offset,
    });
  }

  false
}

fn find_fragment_path_for_box_id(
  fragment: &FragmentNode,
  offset: Point,
  target: usize,
  box_debug: &HashMap<usize, String>,
  out_path: &mut Vec<String>,
) -> bool {
  struct Frame<'a> {
    node: &'a FragmentNode,
    next_child: usize,
    label: String,
    child_offset: Point,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  let (abs, child_offset) = absolute_rect(fragment, offset);
  stack.push(Frame {
    node: fragment,
    next_child: 0,
    label: label_fragment(fragment, abs, box_debug),
    child_offset,
  });

  while let Some(frame) = stack.last_mut() {
    if fragment_box_id(frame.node) == Some(target) {
      out_path.clear();
      out_path.extend(stack.iter().map(|f| f.label.clone()));
      return true;
    }

    if frame.next_child >= frame.node.children.len() {
      stack.pop();
      continue;
    }

    let child_offset = frame.child_offset;
    let child = &frame.node.children[frame.next_child];
    frame.next_child += 1;
    let (abs, child_offset) = absolute_rect(child, child_offset);
    stack.push(Frame {
      node: child,
      next_child: 0,
      label: label_fragment(child, abs, box_debug),
      child_offset,
    });
  }

  false
}

fn find_fragment_node_for_box_id<'a>(
  fragment: &'a FragmentNode,
  offset: Point,
  target: usize,
) -> Option<(&'a FragmentNode, Rect)> {
  let mut stack: Vec<(Point, &'a FragmentNode)> = vec![(offset, fragment)];
  while let Some((offset, node)) = stack.pop() {
    let (abs, child_offset) = absolute_rect(node, offset);
    if fragment_box_id(node) == Some(target) {
      return Some((node, abs));
    }
    for child in node.children.iter().rev() {
      stack.push((child_offset, child));
    }
  }
  None
}

fn print_fragment_tree(node: &FragmentNode, indent: usize, max_lines: usize) {
  fn fmt_content(node: &FragmentNode) -> String {
    match &node.content {
      FragmentContent::Block { box_id } => format!("block box_id={:?}", box_id),
      FragmentContent::Inline { box_id, .. } => format!("inline box_id={:?}", box_id),
      FragmentContent::Line { .. } => "line".into(),
      FragmentContent::Text { text, .. } => format!("text {:?}", text),
      FragmentContent::Replaced { box_id, .. } => format!("replaced box_id={:?}", box_id),
      FragmentContent::RunningAnchor { name, .. } => format!("running-anchor name={name}"),
      FragmentContent::FootnoteAnchor { .. } => "footnote-anchor".into(),
    }
  }

  struct Frame<'a> {
    node: &'a FragmentNode,
    indent: usize,
    next_child: usize,
  }

  let mut remaining = max_lines;
  let mut stack: Vec<Frame<'_>> = vec![Frame {
    node,
    indent,
    next_child: 0,
  }];
  while let Some(frame) = stack.last_mut() {
    if remaining == 0 {
      break;
    }

    if frame.next_child == 0 {
      remaining -= 1;
      println!(
        "{space}{desc}",
        space = " ".repeat(frame.indent * 2),
        desc = fmt_content(frame.node)
      );
    }

    if frame.next_child >= frame.node.children.len() {
      stack.pop();
      continue;
    }

    let child = &frame.node.children[frame.next_child];
    let child_indent = frame.indent + 1;
    frame.next_child += 1;
    stack.push(Frame {
      node: child,
      indent: child_indent,
      next_child: 0,
    });
  }
}

#[derive(Debug)]
struct InspectionOutput {
  pixmap: tiny_skia::Pixmap,
  dom: fastrender::dom::DomNode,
  styled: fastrender::style::cascade::StyledNode,
  box_tree: BoxTree,
  fragment_tree: fastrender::FragmentTree,
  display_list: fastrender::DisplayList,
  diagnostics: fastrender::api::RenderDiagnostics,
}

fn filter_mutual_exclusion_error() -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    "inspect_frag: --filter-id and --filter-selector are mutually exclusive",
  )
}

fn validate_args(args: &Args) -> Result<(), DynError> {
  if args.filter_id.is_some() && args.filter_selector.is_some() {
    return Err(filter_mutual_exclusion_error().into());
  }
  Ok(())
}

fn inspect_pipeline(
  renderer: &mut FastRender,
  doc: &InputDocument,
  args: &Args,
) -> Result<InspectionOutput, DynError> {
  validate_args(args)?;

  let mut options = RenderOptions::new()
    .with_viewport(args.viewport.0, args.viewport.1)
    .with_device_pixel_ratio(args.dpr)
    .with_media_type(args.media.as_media_type())
    .with_scroll(args.scroll_x, args.scroll_y);
  if let Some(time_ms) = args.animation_time.animation_time_ms() {
    options = options.with_animation_time(time_ms);
  }

  let report = renderer.render_html_with_stylesheets_report(
    &doc.html,
    &doc.base_hint,
    options,
    RenderArtifactRequest::full(),
  )?;

  let fastrender::api::RenderReport {
    pixmap,
    artifacts,
    diagnostics,
    ..
  } = report;

  let mut dom = artifacts
    .dom
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "inspect_frag: missing DOM artifact"))?;
  let mut styled = artifacts.styled_tree.ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::Other,
      "inspect_frag: missing styled tree artifact",
    )
  })?;
  let mut box_tree = artifacts.box_tree.ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::Other,
      "inspect_frag: missing box tree artifact",
    )
  })?;
  let mut fragment_tree = artifacts.fragment_tree.ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::Other,
      "inspect_frag: missing fragment tree artifact",
    )
  })?;
  let mut display_list = artifacts.display_list.ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::Other,
      "inspect_frag: missing display list artifact",
    )
  })?;

  let target_node_id = match (&args.filter_id, &args.filter_selector) {
    (None, None) => None,
    (Some(id), None) => {
      let matches = fastrender::debug::inspect::inspect(
        &dom,
        &styled,
        &box_tree.root,
        &fragment_tree,
        fastrender::InspectQuery::Id(id.clone()),
      )?;
      matches.first().map(|m| m.node.node_id)
    }
    (None, Some(selector)) => {
      let matches = fastrender::debug::inspect::inspect(
        &dom,
        &styled,
        &box_tree.root,
        &fragment_tree,
        fastrender::InspectQuery::Selector(selector.clone()),
      )?;
      matches.first().map(|m| m.node.node_id)
    }
    (Some(_), Some(_)) => return Err(filter_mutual_exclusion_error().into()),
  };

  if let Some(node_id) = target_node_id {
    let document_quirks_mode = dom.document_quirks_mode();
    if let Some(subtree) = find_dom_node_by_preorder_id(&dom, node_id) {
      dom = match subtree.node_type {
        DomNodeType::Document { .. } => subtree,
        _ => fastrender::dom::DomNode {
          node_type: DomNodeType::Document {
            quirks_mode: document_quirks_mode,
            scripting_enabled: true,
            is_html_document: true,
          },
          children: vec![subtree],
        },
      };
    }
    if let Some(subtree) = find_styled_node_by_id(&styled, node_id) {
      styled = subtree;
    }

    let mut allowed_styled_ids = HashSet::new();
    collect_styled_node_ids(&styled, &mut allowed_styled_ids);

    if let Some(filtered_root) = filter_box_subtree(&box_tree.root, &allowed_styled_ids) {
      box_tree.root = filtered_root;
    }

    let mut allowed_box_ids = HashSet::new();
    collect_box_ids(&box_tree.root, &mut allowed_box_ids);

    let mut roots = Vec::new();
    if let Some(filtered_root) = filter_fragment_subtree(&fragment_tree.root, &allowed_box_ids) {
      roots.push(filtered_root);
    }
    for extra in &fragment_tree.additional_fragments {
      if let Some(filtered_extra) = filter_fragment_subtree(extra, &allowed_box_ids) {
        roots.push(filtered_extra);
      }
    }
    if roots.is_empty() {
      return Err(
        io::Error::new(
          io::ErrorKind::Other,
          format!("inspect_frag: filter matched node_id={node_id} but no fragments were retained"),
        )
        .into(),
      );
    }
    let viewport_size = fragment_tree.viewport_size();
    let mut filtered_tree = fastrender::FragmentTree::from_fragments(roots, viewport_size);
    filtered_tree.keyframes = fragment_tree.keyframes.clone();
    filtered_tree.svg_filter_defs = fragment_tree.svg_filter_defs.clone();
    filtered_tree.svg_id_defs = fragment_tree.svg_id_defs.clone();
    filtered_tree.svg_id_defs_raw = fragment_tree.svg_id_defs_raw.clone();
    filtered_tree.scroll_metadata = fragment_tree.scroll_metadata.clone();
    fragment_tree = filtered_tree;

    let bounds = fragment_tree.content_size();
    let filtered_items: Vec<fastrender::DisplayItem> = display_list
      .items()
      .iter()
      .cloned()
      .filter(|item| match item.bounds() {
        None => true,
        Some(rect) => rect.intersects(bounds),
      })
      .collect();
    display_list = fastrender::DisplayList::from_items(filtered_items);
  }

  Ok(InspectionOutput {
    pixmap,
    dom,
    styled,
    box_tree,
    fragment_tree,
    display_list,
    diagnostics,
  })
}

fn run(args: Args) -> Result<(), DynError> {
  validate_args(&args)?;
  let media_prefs = MediaPreferences::from(&args.media_prefs);
  let input = load_input_document(&args)?;

  media_prefs.apply_env();
  let raw_runtime = std::env::vars()
    .filter(|(k, _)| k.starts_with("FASTR_"))
    .collect::<HashMap<_, _>>();
  let runtime_toggles =
    inspect_runtime_toggles_from_env_map(raw_runtime, args.patch_html_for_chrome_baseline);
  let _runtime_guard = runtime::set_runtime_toggles(Arc::new(runtime_toggles.clone()));

  let fetcher = build_fetcher(&args)?;

  let font_config = inspect_frag_font_config(&args);
  let mut builder = FastRender::builder()
    .device_pixel_ratio(args.dpr)
    .compat_mode(args.compat.compat_profile())
    .dom_compatibility_mode(args.compat.dom_compat_mode())
    .font_sources(font_config)
    .fetcher(fetcher)
    .runtime_toggles(runtime_toggles);

  if args.deny_network {
    builder = builder.resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    );
    // When treating the document as offline (fixtures), match the fixture renderer: scripts are not
    // executed, so parse with scripting-disabled semantics so `<noscript>` fallback markup is
    // available for layout/paint.
    builder = builder.render_parse_scripting_enabled(false);
  }

  let mut renderer = builder.build()?;

  let output = inspect_pipeline(&mut renderer, &input, &args)?;

  if !output.diagnostics.fetch_errors.is_empty() {
    eprintln!(
      "inspect_frag: {} subresource fetch errors",
      output.diagnostics.fetch_errors.len()
    );
    for err in output.diagnostics.fetch_errors.iter().take(10) {
      eprintln!(
        "  {:?} {}: {}",
        err.kind,
        err.final_url.as_deref().unwrap_or(&err.url),
        err.message
      );
    }
    if output.diagnostics.fetch_errors.len() > 10 {
      eprintln!("  ...");
    }
  }

  if let Some(dir) = &args.dump_json {
    fs::create_dir_all(dir)?;
    let snapshot = snapshot_pipeline(
      &output.dom,
      &output.styled,
      &output.box_tree,
      &output.fragment_tree,
      &output.display_list,
    );
    write_pretty_json(&dir.join("dom.json"), &snapshot.dom)?;
    let composed_dom = fastrender::debug::snapshot::snapshot_composed_dom(&output.dom)?;
    write_pretty_json(&dir.join("composed_dom.json"), &composed_dom)?;
    write_pretty_json(&dir.join("styled.json"), &snapshot.styled)?;
    write_pretty_json(&dir.join("box_tree.json"), &snapshot.box_tree)?;
    write_pretty_json(&dir.join("fragment_tree.json"), &snapshot.fragment_tree)?;
    write_pretty_json(&dir.join("display_list.json"), &snapshot.display_list)?;

    if args.dump_custom_properties {
      let include_all_prefixes = args.custom_property_prefix.is_empty();
      let mut props: Vec<(String, String)> = output
        .styled
        .styles
        .custom_properties
        .iter()
        .filter(|(name, _)| {
          if include_all_prefixes {
            return true;
          }
          let name = name.as_ref();
          args
            .custom_property_prefix
            .iter()
            .any(|prefix| name.starts_with(prefix))
        })
        .map(|(name, value)| (name.as_ref().to_string(), value.value.clone()))
        .collect();
      props.sort_by(|a, b| a.0.cmp(&b.0));
      if props.len() > args.custom_properties_limit {
        props.truncate(args.custom_properties_limit);
      }

      let payload = serde_json::json!({
        "node_id": output.styled.node_id,
        "total_custom_properties": output.styled.styles.custom_properties.len(),
        "included": props.len(),
        "custom_properties": props
          .into_iter()
          .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
          .collect::<Vec<_>>(),
      });
      write_pretty_json(&dir.join("custom_properties.json"), &payload)?;
    }
  }

  if let Some(path) = &args.dump_dom2_json {
    let dom2_doc = fastrender::dom2::Document::from_renderer_dom(&output.dom);
    let dom2_snapshot = fastrender::debug::snapshot::snapshot_dom2(&dom2_doc);
    write_pretty_json(path, &dom2_snapshot)?;
  }

  if args.dump_snapshot {
    let snapshot = snapshot_pipeline(
      &output.dom,
      &output.styled,
      &output.box_tree,
      &output.fragment_tree,
      &output.display_list,
    );
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
  }

  if let Some(path) = &args.render_overlay {
    let mut pixmap = output.pixmap;
    draw_fragment_overlays(
      &mut pixmap,
      &output.fragment_tree,
      args.dpr,
      args.scroll_x,
      args.scroll_y,
    );
    let bytes = encode_image(&pixmap, OutputFormat::Png)?;
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
      }
    }
    fs::write(path, bytes)?;
    eprintln!("Overlay render written to {}", path.display());
  }

  let mut box_debug: HashMap<usize, String> = HashMap::new();
  collect_box_debug(&output.box_tree.root, &mut box_debug);

  if !args.trace_text.is_empty() {
    for needle in &args.trace_text {
      let mut found = None;
      for (idx, root) in std::iter::once(&output.fragment_tree.root)
        .chain(output.fragment_tree.additional_fragments.iter())
        .enumerate()
      {
        let mut path = Vec::new();
        if find_fragment_path_for_text(root, Point::ZERO, needle, &box_debug, &mut path) {
          found = Some((idx, path));
          break;
        }
      }
      match found {
        Some((root_idx, path)) => {
          println!("path to text containing {:?} (root {}):", needle, root_idx);
          for (idx, entry) in path.iter().enumerate() {
            println!("  {idx}: {entry}");
          }
        }
        None => println!("no fragment text found containing {:?}", needle),
      }
    }
  }

  if !args.trace_box.is_empty() {
    for target_id in &args.trace_box {
      if let Some(node) = find_box_by_id(&output.box_tree.root, *target_id) {
        println!(
          "box#{id}: {debug} {style}",
          id = node.id,
          debug = format_debug_info(node),
          style = style_summary(node.style.as_ref())
        );
      } else {
        println!("box#{target_id}: not found in box tree");
      }

      let mut found = None;
      for (idx, root) in std::iter::once(&output.fragment_tree.root)
        .chain(output.fragment_tree.additional_fragments.iter())
        .enumerate()
      {
        let mut path = Vec::new();
        if find_fragment_path_for_box_id(root, Point::ZERO, *target_id, &box_debug, &mut path) {
          found = Some((idx, path));
          break;
        }
      }
      match found {
        Some((root_idx, path)) => {
          println!("path to box_id {target_id} (root {root_idx}):");
          for (idx, entry) in path.iter().enumerate() {
            println!("  {idx}: {entry}");
          }
        }
        None => println!("box_id {target_id} not found in fragments"),
      }
    }
  }

  if let Some(target_id) = args.dump_fragment {
    let mut found = None;
    for (idx, root) in std::iter::once(&output.fragment_tree.root)
      .chain(output.fragment_tree.additional_fragments.iter())
      .enumerate()
    {
      if let Some((fragment, abs)) = find_fragment_node_for_box_id(root, Point::ZERO, target_id) {
        found = Some((idx, fragment, abs));
        break;
      }
    }
    match found {
      Some((root_idx, fragment, abs)) => {
        println!(
          "fragment subtree for box_id {target_id} @ ({:.1},{:.1},{:.1},{:.1}) [root {}]",
          abs.x(),
          abs.y(),
          abs.width(),
          abs.height(),
          root_idx
        );
        print_fragment_tree(fragment, 0, 2000);
      }
      None => println!("no fragment found for box_id {target_id}"),
    }
  }

  if args.find_skinny_fragments {
    let mut skinny: Vec<(Rect, String, usize)> = Vec::new();
    for (root_idx, root) in std::iter::once(&output.fragment_tree.root)
      .chain(output.fragment_tree.additional_fragments.iter())
      .enumerate()
    {
      let mut stack: Vec<(Point, &FragmentNode)> = vec![(Point::ZERO, root)];
      while let Some((offset, fragment)) = stack.pop() {
        let (abs, next_offset) = absolute_rect(fragment, offset);
        if abs.width() <= args.skinny_max_width && abs.height() >= args.skinny_min_height {
          skinny.push((abs, label_fragment(fragment, abs, &box_debug), root_idx));
        }
        for child in fragment.children.iter() {
          stack.push((next_offset, child));
        }
      }
    }
    skinny.sort_by(|a, b| {
      a.0
        .y()
        .partial_cmp(&b.0.y())
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| {
          a.0
            .x()
            .partial_cmp(&b.0.x())
            .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    println!(
      "skinny fragments (<= {:.1}px wide, >= {:.1}px tall): {}",
      args.skinny_max_width,
      args.skinny_min_height,
      skinny.len()
    );
    for (idx, (rect, label, root_idx)) in skinny.iter().take(50).enumerate() {
      println!(
        "  #{idx}: ({:.1},{:.1},{:.1},{:.1}) [root {}] {label}",
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        root_idx
      );
    }
    if skinny.len() > 50 {
      println!("  ...");
    }
  }

  let has_explicit_output = args.dump_json.is_some()
    || args.dump_dom2_json.is_some()
    || args.dump_snapshot
    || args.render_overlay.is_some()
    || !args.trace_text.is_empty()
    || !args.trace_box.is_empty()
    || args.dump_fragment.is_some()
    || args.find_skinny_fragments;

  if !has_explicit_output {
    if let Some(body) = find_styled_element_by_tag(&output.styled, "body") {
      let style = body.styles.as_ref();
      println!(
        "body bg={} color={}",
        fmt_rgba_compact(style.background_color),
        fmt_rgba_compact(style.color)
      );
    }
  }

  Ok(())
}

fn inspect_runtime_toggles_from_env_map(
  mut raw: HashMap<String, String>,
  patch_html_for_chrome_baseline: bool,
) -> RuntimeToggles {
  // Keep `inspect_frag --patch-html-for-chrome-baseline` aligned with the Chrome baseline harness.
  //
  // In particular, FastRender defaults `FASTR_COMPAT_REPLACED_MAX_WIDTH_100=1` (non-standard) to
  // preserve historical fixture behavior, but that diverges from Chrome and can materially change
  // layout (e.g. shrink-to-fit abspos boxes containing images).
  if patch_html_for_chrome_baseline {
    raw
      .entry("FASTR_COMPAT_REPLACED_MAX_WIDTH_100".to_string())
      .or_insert_with(|| "0".to_string());
  }
  RuntimeToggles::from_map(raw)
}

fn inspect_frag_font_config(args: &Args) -> FontConfig {
  let mut config = FontConfig::bundled_only();
  if args.system_fonts {
    config = config.with_system_fonts(true);
  }
  if !args.font_dir.is_empty() {
    config = config.with_font_dirs(args.font_dir.clone());
  }
  config
}

fn main() -> Result<(), DynError> {
  // Avoid panicking on SIGPIPE/BrokenPipe when piped through tools like `head`.
  let default_hook = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |info| {
    let mut msg = info.to_string();
    if let Some(s) = info.payload().downcast_ref::<&str>() {
      msg = (*s).to_string();
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
      msg = s.clone();
    }
    if msg.contains("Broken pipe") {
      std::process::exit(0);
    }
    default_hook(info);
  }));

  let args = Args::parse();

  if let Some(sec) = args.timeout {
    std::thread::spawn(move || {
      std::thread::sleep(Duration::from_secs(sec));
      eprintln!("inspect_frag: timed out after {}s", sec);
      std::process::exit(1);
    });
  }

  let handle = std::thread::Builder::new()
    .name("inspect_frag-worker".to_string())
    .stack_size(CLI_RENDER_STACK_SIZE)
    .spawn(move || run(args))?;

  match handle.join() {
    Ok(result) => result,
    Err(payload) => std::panic::resume_unwind(payload),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn meta_base_hint_selection_uses_meta_url() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_path = dir.path().join("page.html");
    fs::write(&html_path, "<!doctype html><html><body>ok</body></html>").expect("write html");
    fs::write(
      html_path.with_extension("html.meta"),
      "url: https://example.com/x\n",
    )
    .expect("write meta");

    let args =
      Args::try_parse_from(["inspect_frag", html_path.to_str().unwrap()]).expect("parse args");
    let input = load_input_document(&args).expect("load input");
    assert_eq!(input.base_hint, "https://example.com/x");
  }

  #[test]
  fn pageset_resolution_selects_cached_html() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_dir = dir.path().join("fetches/html");
    fs::create_dir_all(&html_dir).expect("mkdir");
    let cached = html_dir.join("example.com.html");
    fs::write(&cached, "<!doctype html><html><body>ok</body></html>").expect("write cached html");

    let args = Args::try_parse_from([
      "inspect_frag",
      "--pageset",
      "https://example.com",
      "--html-dir",
      html_dir.to_str().unwrap(),
    ])
    .expect("parse args");
    let input = load_input_document(&args).expect("load input");
    assert_eq!(input.path, cached);
  }

  #[test]
  fn patch_html_for_chrome_baseline_injects_offline_tags() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_path = dir.path().join("page.html");
    fs::write(
      &html_path,
      "<!doctype html><html><head></head><body>ok</body></html>",
    )
    .expect("write html");

    let args = Args::try_parse_from([
      "inspect_frag",
      html_path.to_str().unwrap(),
      "--patch-html-for-chrome-baseline",
    ])
    .expect("parse args");
    let input = load_input_document(&args).expect("load input");
    assert!(
      input
        .html
        .contains("<meta http-equiv=\"Content-Security-Policy\""),
      "expected patched HTML to include CSP injection"
    );
    assert!(
      input.html.contains("<meta name=\"color-scheme\""),
      "expected patched HTML to force light color scheme by default"
    );
    assert!(
      input.html.contains("scrollbar-width: none"),
      "expected patched HTML to hide scrollbars"
    );
    let expected_base = format!(
      "<base href=\"{}\">",
      common::file_url::file_url_for_path(&html_path)
    );
    assert!(
      input.html.contains(&expected_base),
      "expected patched HTML to include base href injection; expected fragment {expected_base:?}"
    );
  }

  #[test]
  fn patch_html_for_chrome_baseline_defaults_replaced_max_width_compat_off() {
    let toggles = inspect_runtime_toggles_from_env_map(HashMap::new(), true);
    assert_eq!(
      toggles.get("FASTR_COMPAT_REPLACED_MAX_WIDTH_100"),
      Some("0")
    );

    let mut raw = HashMap::new();
    raw.insert(
      "FASTR_COMPAT_REPLACED_MAX_WIDTH_100".to_string(),
      "1".to_string(),
    );
    let toggles = inspect_runtime_toggles_from_env_map(raw, true);
    assert_eq!(
      toggles.get("FASTR_COMPAT_REPLACED_MAX_WIDTH_100"),
      Some("1")
    );
  }

  #[test]
  fn filter_id_and_filter_selector_return_explicit_error() {
    let args = Args::try_parse_from([
      "inspect_frag",
      "dummy.html",
      "--filter-id",
      "example",
      "--filter-selector",
      "body",
    ])
    .expect("parse args");
    let err = run(args).err().expect("expected invalid-args error");
    assert!(
      err.to_string()
        .contains("--filter-id and --filter-selector are mutually exclusive"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn deny_network_blocks_http_subresources_before_hitting_the_fetcher() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_path = dir.path().join("page.html");
    fs::write(
      &html_path,
      "<!doctype html><html><body><img src=\"https://example.com/a.png\"></body></html>",
    )
    .expect("write html");

    let args = Args::try_parse_from([
      "inspect_frag",
      html_path.to_str().unwrap(),
      "--deny-network",
      "--viewport",
      "16x16",
    ])
    .expect("parse args");
    let input = load_input_document(&args).expect("load input");
    let fetcher = build_fetcher(&args).expect("build fetcher");
    let runtime_toggles = RuntimeToggles::from_env();
    let mut builder = FastRender::builder()
      .device_pixel_ratio(args.dpr)
      .compat_mode(args.compat.compat_profile())
      .dom_compatibility_mode(args.compat.dom_compat_mode())
      .fetcher(fetcher)
      .runtime_toggles(runtime_toggles);
    if args.deny_network {
      builder = builder.resource_policy(
        ResourcePolicy::default()
          .allow_http(false)
          .allow_https(false),
      );
    }
    let mut renderer = builder.build().expect("build renderer");
    let out = inspect_pipeline(&mut renderer, &input, &args).expect("render");

    let mut blocked = out
      .diagnostics
      .fetch_errors
      .iter()
      .chain(out.diagnostics.blocked_fetch_errors.iter())
      .filter(|e| e.url.contains("example.com/a.png"))
      .collect::<Vec<_>>();
    blocked.sort_by(|a, b| a.url.cmp(&b.url));
    assert!(
      !blocked.is_empty(),
      "expected blocked fetch diagnostics for the remote image URL"
    );
  }

  #[cfg(not(feature = "disk_cache"))]
  #[test]
  fn offline_requires_disk_cache_feature() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_path = dir.path().join("page.html");
    fs::write(&html_path, "<!doctype html><html><body>ok</body></html>").expect("write html");

    let args = Args::try_parse_from(["inspect_frag", html_path.to_str().unwrap(), "--offline"])
      .expect("parse args");

    let err = build_fetcher(&args).err().expect("expected offline error");
    assert!(
      err.to_string().contains("disk_cache"),
      "error should mention disk_cache: {err}"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn offline_mode_uses_disk_cache_and_never_hits_network() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    let dir = tempfile::tempdir().expect("temp dir");
    let cache_dir = dir.path().join("assets");
    fs::create_dir_all(&cache_dir).expect("mkdir cache");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");

    let hits = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let hits_thread = Arc::clone(&hits);
    let stop_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
      while !stop_thread.load(Ordering::SeqCst) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
              .lines()
              .next()
              .and_then(|line| line.split_whitespace().nth(1))
              .unwrap_or("/");
            let (status, body) = match path {
              "/hit.css" => (200, "body{color:red}"),
              _ => (404, "not found"),
            };
            let status_line = if status == 200 { "OK" } else { "Not Found" };
            let resp = format!(
              "HTTP/1.1 {status} {status_line}\r\nContent-Type: text/css\r\nCache-Control: max-age=3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
              body.len(),
              body
            );
            let _ = stream.write_all(resp.as_bytes());
          }
          Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(_) => break,
        }
      }
    });

    let html_path = dir.path().join("page.html");
    fs::write(
      &html_path,
      "<!doctype html><html><head>\
      <link rel=\"stylesheet\" href=\"/hit.css\">\
      <link rel=\"stylesheet\" href=\"/miss.css\">\
      </head><body>ok</body></html>",
    )
    .expect("write html");

    let base_hint = format!("http://{addr}/page.html");
    let hit_url = format!("http://{addr}/hit.css");
    let miss_url = format!("http://{addr}/miss.css");

    // Populate the disk cache with hit.css using the normal (online) fetcher stack.
    let populate_args = Args::try_parse_from([
      "inspect_frag",
      html_path.to_str().unwrap(),
      "--base-hint",
      &base_hint,
      "--cache-dir",
      cache_dir.to_str().unwrap(),
    ])
    .expect("parse populate args");
    let fetcher = build_fetcher(&populate_args).expect("build fetcher");
    fetcher
      .fetch_with_context(fastrender::resource::FetchContextKind::Stylesheet, &hit_url)
      .expect("populate fetch");

    // Ensure exactly one network request occurred so far.
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Render under offline mode; hit.css should be served from disk cache, and miss.css should fail
    // without touching the network.
    let offline_args = Args::try_parse_from([
      "inspect_frag",
      html_path.to_str().unwrap(),
      "--base-hint",
      &base_hint,
      "--cache-dir",
      cache_dir.to_str().unwrap(),
      "--offline",
    ])
    .expect("parse offline args");

    let input = load_input_document(&offline_args).expect("load input");
    let fetcher = build_fetcher(&offline_args).expect("build offline fetcher");
    let runtime_toggles = RuntimeToggles::from_env();
    let mut renderer = FastRender::builder()
      .device_pixel_ratio(offline_args.dpr)
      .compat_mode(offline_args.compat.compat_profile())
      .dom_compatibility_mode(offline_args.compat.dom_compat_mode())
      .font_sources(inspect_frag_font_config(&offline_args))
      .fetcher(fetcher)
      .runtime_toggles(runtime_toggles)
      .build()
      .expect("build renderer");

    let out = inspect_pipeline(&mut renderer, &input, &offline_args).expect("render");
    assert_eq!(
      hits.load(Ordering::SeqCst),
      1,
      "offline render should not hit the network"
    );

    assert!(
      out
        .diagnostics
        .fetch_errors
        .iter()
        .any(|e| e.url == miss_url),
      "expected cache miss to be recorded as a fetch error"
    );
    assert!(
      !out
        .diagnostics
        .fetch_errors
        .iter()
        .any(|e| e.url == hit_url),
      "expected cached hit.css to not produce a fetch error"
    );

    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
  }

  #[test]
  fn trace_helpers_handle_deep_fragment_trees_without_stack_overflow() {
    use fastrender::geometry::Rect;
    use std::sync::Arc;

    let needle = "needle";
    let mut node = FragmentNode::new(
      Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
      FragmentContent::Text {
        text: Arc::<str>::from(needle),
        box_id: None,
        source_range: None,
        baseline_offset: 0.0,
        shaped: None,
        is_marker: false,
        emphasis_offset: Default::default(),
        document_selection: None,
      },
      vec![],
    );

    let depth = 5000;
    for _ in 0..depth {
      node = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![node]);
    }

    let mut path = Vec::new();
    let ok = find_fragment_path_for_text(&node, Point::ZERO, needle, &HashMap::new(), &mut path);
    assert!(ok);
    assert_eq!(path.len(), depth + 1);
    assert!(path.first().is_some_and(|l| l.starts_with("block")));
    assert!(path.last().is_some_and(|l| l.starts_with("text")));
  }

  #[test]
  fn font_config_defaults_to_bundled_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_path = dir.path().join("page.html");
    fs::write(&html_path, "<!doctype html><html><body>ok</body></html>").expect("write html");

    let args =
      Args::try_parse_from(["inspect_frag", html_path.to_str().unwrap()]).expect("parse args");
    let config = inspect_frag_font_config(&args);
    assert!(config.use_bundled_fonts);
    assert!(!config.use_system_fonts);
  }

  #[test]
  fn font_config_system_fonts_keeps_bundled_fallbacks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let html_path = dir.path().join("page.html");
    fs::write(&html_path, "<!doctype html><html><body>ok</body></html>").expect("write html");

    let args = Args::try_parse_from([
      "inspect_frag",
      html_path.to_str().unwrap(),
      "--system-fonts",
    ])
    .expect("parse args");
    let config = inspect_frag_font_config(&args);
    assert!(config.use_bundled_fonts);
    assert!(config.use_system_fonts);
  }

  #[test]
  fn fragment_overlay_rect_is_clipped_to_pixmap_bounds() {
    // A fragment that is entirely off-screen should be skipped.
    let abs = Rect::from_xywh(-5000.0, 0.0, 10.0, 10.0);
    assert!(overlay_rect_for_fragment(abs, 1.0, 0.0, 0.0, 100, 100).is_none());

    // A fragment that partially overlaps the pixmap should be clipped to the pixmap bounds, so we
    // never attempt to stroke a path spanning the full 100k CSS-px offscreen extent.
    let abs = Rect::from_xywh(-5000.0, 0.0, 6000.0, 10.0);
    let rect = overlay_rect_for_fragment(abs, 1.0, 0.0, 0.0, 100, 100).expect("clipped rect");
    assert!(rect.x() >= 0.0);
    assert!(rect.width() <= 100.0);
  }
}
