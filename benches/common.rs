use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, Once, OnceLock};
use std::time::Duration;

use criterion::Criterion;
use fastrender::{
  css::parser::{
    extract_css_sources, parse_stylesheet, rel_list_contains_stylesheet, StylesheetSource,
  },
  css::types::StyleSheet,
  dom::{parse_html, DomNode},
  geometry::Rect,
  paint::{
    display_list::DisplayList, display_list_builder::DisplayListBuilder,
    display_list_renderer::DisplayListRenderer, optimize::DisplayListOptimizer,
  },
  style::{
    cascade::{apply_styles_with_media, StyledNode},
    color::Rgba,
    media::{MediaContext, MediaQuery, MediaQueryCache},
  },
  text::{font_db::FontDatabase, font_loader::FontContext},
  tree::{box_generation::generate_box_tree, box_tree::BoxTree, fragment_tree::FragmentTree},
  LayoutConfig, LayoutEngine, Pixmap, Size,
};

pub const BLOCK_SIMPLE_HTML: &str = include_str!("../tests/fixtures/html/block_simple.html");
pub const FLEX_HTML: &str = include_str!("../tests/fixtures/html/flex_grow_shrink.html");
pub const FLEX_POSITIONED_HTML: &str =
  include_str!("../tests/fixtures/html/flex_positioned_children.html");
pub const GRID_HTML: &str = include_str!("../tests/fixtures/html/grid_template.html");
pub const TABLE_HTML: &str = include_str!("../tests/fixtures/html/table_span.html");
pub const TABLE_LARGE_ROWSPAN_HTML: &str =
  include_str!("../tests/fixtures/html/table_large_rowspan.html");
pub const TABLE_COLLAPSE_LARGE_HTML: &str =
  include_str!("../tests/fixtures/html/table_collapse_large.html");
pub const FORM_CONTROLS_HTML: &str = include_str!("../tests/fixtures/html/form_controls.html");

pub const SMALL_VIEWPORT: (u32, u32) = (800, 600);
pub const REALISTIC_VIEWPORT: (u32, u32) = (1100, 900);

static FIXED_FONT_CONTEXT: OnceLock<FontContext> = OnceLock::new();
const FIXED_FONT_TTF: &[u8] = include_bytes!("../tests/fixtures/fonts/DejaVuSans-subset.ttf");
const FIXED_FONT_WOFF2: &[u8] = include_bytes!("../tests/fixtures/fonts/DejaVuSans-subset.woff2");

pub fn fixed_font_context() -> FontContext {
  FIXED_FONT_CONTEXT
    .get_or_init(|| {
      let mut db = FontDatabase::empty();
      db.load_font_data(FIXED_FONT_TTF.to_vec())
        .expect("load bundled TTF");
      let _ = db.load_font_data(FIXED_FONT_WOFF2.to_vec());
      FontContext::with_database(Arc::new(db))
    })
    .clone()
}

pub fn parse_dom(html: &str) -> DomNode {
  parse_html(html).expect("parse DOM")
}

pub fn media_context(viewport: (u32, u32)) -> MediaContext {
  MediaContext::screen(viewport.0 as f32, viewport.1 as f32)
}

fn stylesheet_type_is_css(type_attr: Option<&str>) -> bool {
  match type_attr {
    None => true,
    Some(value) => {
      let mime = value.split(';').next().map(str::trim).unwrap_or("");
      mime.is_empty() || mime.eq_ignore_ascii_case("text/css")
    }
  }
}

fn media_matches(
  media: Option<&str>,
  media_ctx: &MediaContext,
  cache: &mut MediaQueryCache,
) -> bool {
  let Some(raw) = media else {
    return true;
  };
  if raw.trim().is_empty() {
    return true;
  }
  if let Ok(queries) = MediaQuery::parse_list(raw) {
    return media_ctx.evaluate_list_with_cache(&queries, Some(cache));
  }
  false
}

pub fn inline_css_text(dom: &DomNode, media_ctx: &MediaContext) -> String {
  let mut css_text = String::new();
  let mut cache = MediaQueryCache::default();

  for scoped in extract_css_sources(dom) {
    match scoped.source {
      StylesheetSource::Inline(inline) => {
        if inline.disabled || inline.css.trim().is_empty() {
          continue;
        }
        if !stylesheet_type_is_css(inline.type_attr.as_deref()) {
          continue;
        }
        if !media_matches(inline.media.as_deref(), media_ctx, &mut cache) {
          continue;
        }
        css_text.push_str(&inline.css);
        css_text.push('\n');
      }
      StylesheetSource::External(link) => {
        // Benchmarks stay offline; skip external stylesheets.
        if link.disabled
          || link.href.trim().is_empty()
          || !rel_list_contains_stylesheet(&link.rel)
          || !stylesheet_type_is_css(link.type_attr.as_deref())
        {
          continue;
        }
      }
    }
  }

  css_text
}

pub fn stylesheet_for_dom(dom: &DomNode, media_ctx: &MediaContext) -> StyleSheet {
  let mut rules = Vec::new();
  let mut cache = MediaQueryCache::default();

  for scoped in extract_css_sources(dom) {
    match scoped.source {
      StylesheetSource::Inline(inline) => {
        if inline.disabled || inline.css.trim().is_empty() {
          continue;
        }
        if !stylesheet_type_is_css(inline.type_attr.as_deref()) {
          continue;
        }
        if !media_matches(inline.media.as_deref(), media_ctx, &mut cache) {
          continue;
        }
        if let Ok(sheet) = parse_stylesheet(&inline.css) {
          rules.extend(sheet.rules);
        }
      }
      StylesheetSource::External(link) => {
        if link.disabled
          || link.href.trim().is_empty()
          || !rel_list_contains_stylesheet(&link.rel)
          || !stylesheet_type_is_css(link.type_attr.as_deref())
        {
          continue;
        }
      }
    }
  }

  StyleSheet {
    namespaces: Default::default(),
    rules,
  }
}

pub fn cascade(dom: &DomNode, stylesheet: &StyleSheet, media_ctx: &MediaContext) -> StyledNode {
  apply_styles_with_media(dom, stylesheet, media_ctx)
}

pub fn box_tree_from_styled(styled: &StyledNode) -> BoxTree {
  generate_box_tree(styled).expect("box generation")
}

pub fn layout_engine(viewport: (u32, u32), font_ctx: &FontContext) -> LayoutEngine {
  let config = LayoutConfig::for_viewport(Size::new(viewport.0 as f32, viewport.1 as f32));
  LayoutEngine::with_font_context(config, font_ctx.clone())
}

pub fn layout_fragment_tree(engine: &LayoutEngine, box_tree: &BoxTree) -> FragmentTree {
  engine.layout_tree(box_tree).expect("layout tree")
}

pub fn build_display_list(fragments: &FragmentTree, font_ctx: &FontContext) -> DisplayList {
  DisplayListBuilder::new()
    .with_font_context(font_ctx.clone())
    .with_device_pixel_ratio(1.0)
    .build_tree(fragments)
}

pub fn optimize_display_list(list: &DisplayList, viewport: (u32, u32)) -> DisplayList {
  let viewport_rect = Rect::from_xywh(0.0, 0.0, viewport.0 as f32, viewport.1 as f32);
  DisplayListOptimizer::new()
    .optimize(list.clone(), viewport_rect)
    .0
}

pub fn rasterize_display_list(
  list: &DisplayList,
  viewport: (u32, u32),
  font_ctx: &FontContext,
) -> Pixmap {
  DisplayListRenderer::new(viewport.0, viewport.1, Rgba::WHITE, font_ctx.clone())
    .expect("renderer")
    .render(list)
    .expect("render display list")
}

pub fn render_pipeline(html: &str, viewport: (u32, u32), font_ctx: &FontContext) -> Pixmap {
  let media_ctx = media_context(viewport);
  let dom = parse_dom(html);
  let stylesheet = stylesheet_for_dom(&dom, &media_ctx);
  let styled = cascade(&dom, &stylesheet, &media_ctx);
  let box_tree = box_tree_from_styled(&styled);
  let engine = layout_engine(viewport, font_ctx);
  let fragments = layout_fragment_tree(&engine, &box_tree);
  let display_list = build_display_list(&fragments, font_ctx);
  let optimized = optimize_display_list(&display_list, viewport);
  rasterize_display_list(&optimized, viewport, font_ctx)
}

pub fn parse_stylesheet_text(css: &str) -> StyleSheet {
  parse_stylesheet(css).expect("parse stylesheet")
}

pub fn perf_criterion() -> Criterion {
  Criterion::default()
    .sample_size(15)
    .warm_up_time(Duration::from_millis(500))
    .measurement_time(Duration::from_secs(1))
    .configure_from_args()
}

// -----------------------------------------------------------------------------
// Bench safety helpers
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct BenchLimits {
  /// Maximum bytes benches will read from an on-disk fixture.
  pub max_fixture_bytes: usize,
  /// Maximum number of threads benchmarks should use for parallel workloads.
  pub max_threads: usize,
  /// Maximum DOM nodes/HTML elements synthetic generators should create by default.
  pub max_dom_nodes: usize,
  /// Maximum number of display list items a synthetic generator should create by default.
  pub max_display_list_items: usize,
  /// Maximum recursion depth for synthetic tree builders.
  pub max_depth: usize,
}

impl BenchLimits {
  pub fn from_env() -> Self {
    Self {
      max_fixture_bytes: env_byte_limit("FASTR_BENCH_MAX_FIXTURE_BYTES").unwrap_or(8 * 1024 * 1024),
      max_threads: env_usize("FASTR_BENCH_MAX_THREADS")
        .map(|v| v.max(1))
        .unwrap_or(8),
      max_dom_nodes: env_usize("FASTR_BENCH_MAX_DOM_NODES").unwrap_or(100_000),
      max_display_list_items: env_usize("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS").unwrap_or(200_000),
      max_depth: env_usize("FASTR_BENCH_MAX_DEPTH").unwrap_or(256),
    }
  }
}

pub fn bench_limits() -> &'static BenchLimits {
  static LIMITS: OnceLock<BenchLimits> = OnceLock::new();
  LIMITS.get_or_init(BenchLimits::from_env)
}

pub fn bench_verbose() -> bool {
  env_flag("FASTR_BENCH_VERBOSE")
}

pub fn bench_print_config_once(bench_name: &str, extras: &[(&str, String)]) {
  if !bench_verbose() {
    return;
  }
  static PRINTED: Once = Once::new();
  PRINTED.call_once(|| {
    let limits = bench_limits();
    let mut msg = format!(
      "bench safety {bench_name}: max_dom_nodes={} max_display_list_items={} max_fixture_bytes={} max_threads={} max_depth={}",
      limits.max_dom_nodes,
      limits.max_display_list_items,
      limits.max_fixture_bytes,
      limits.max_threads,
      limits.max_depth
    );
    for (key, value) in extras {
      msg.push(' ');
      msg.push_str(key);
      msg.push('=');
      msg.push_str(value);
    }
    eprintln!("{msg}");
  });
}

/// Read a fixture file, failing if it exceeds `max_bytes`.
pub fn read_fixture_bytes_skip(path: impl AsRef<std::path::Path>, max_bytes: usize) -> io::Result<Vec<u8>> {
  let max_plus_one = max_bytes.saturating_add(1);
  let file = File::open(path.as_ref())?;
  let mut buf = Vec::new();
  file
    .take(max_plus_one as u64)
    .read_to_end(&mut buf)?;
  if buf.len() > max_bytes {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      format!(
        "fixture {} exceeds FASTR_BENCH_MAX_FIXTURE_BYTES ({max_bytes} bytes)",
        path.as_ref().display()
      ),
    ));
  }
  Ok(buf)
}

/// Read up to `max_bytes` from a fixture file, truncating deterministically.
pub fn read_fixture_bytes_truncate(
  path: impl AsRef<std::path::Path>,
  max_bytes: usize,
) -> io::Result<Vec<u8>> {
  let file = File::open(path.as_ref())?;
  let mut buf = Vec::new();
  file.take(max_bytes as u64).read_to_end(&mut buf)?;
  Ok(buf)
}

pub fn env_flag(name: &str) -> bool {
  std::env::var(name)
    .ok()
    .map(|value| {
      let trimmed = value.trim();
      !(trimmed.is_empty()
        || trimmed == "0"
        || trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no"))
    })
    .unwrap_or(false)
}

pub fn env_usize(name: &str) -> Option<usize> {
  let raw = std::env::var(name).ok()?;
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  let cleaned: String = trimmed.chars().filter(|ch| *ch != '_').collect();
  cleaned.parse().ok()
}

pub fn env_byte_limit(name: &str) -> Option<usize> {
  let raw = std::env::var(name).ok()?;
  parse_byte_size(raw.trim())
}

fn parse_byte_size(raw: &str) -> Option<usize> {
  if raw.is_empty() {
    return None;
  }
  let s = raw.trim().to_ascii_lowercase();
  let unit_start = s
    .find(|c: char| c.is_ascii_alphabetic())
    .unwrap_or_else(|| s.len());
  let (num, unit) = s.split_at(unit_start);
  let cleaned: String = num.chars().filter(|ch| *ch != '_').collect();
  let value: u64 = cleaned.parse().ok()?;
  let factor: u64 = match unit {
    "" | "b" => 1,
    "k" | "kb" | "kib" => 1024,
    "m" | "mb" | "mib" => 1024 * 1024,
    "g" | "gb" | "gib" => 1024 * 1024 * 1024,
    "t" | "tb" | "tib" => 1024_u64.pow(4),
    _ => return None,
  };
  let bytes = value.checked_mul(factor)?;
  usize::try_from(bytes).ok()
}
