//! Main painter - converts FragmentTree to pixels
//!
//! This module implements the core painting algorithm that transforms
//! the positioned fragment tree into rasterized pixels.
//!
//! # CSS Painting Order
//!
//! Follows CSS 2.1 Appendix E painting order:
//! 1. Background colors and images
//! 2. Borders
//! 3. Child stacking contexts (negative z-index)
//! 4. In-flow non-positioned blocks
//! 5. Floats
//! 6. In-flow inline content
//! 7. Child stacking contexts (z-index: 0 and auto)
//! 8. Positioned descendants (positive z-index)
//!
//! # Architecture
//!
//! The painter walks the fragment tree depth-first, painting each
//! fragment's background, borders, and content. Text is rendered
//! using the system's default font.

use crate::api::ResourceContext;
#[cfg(test)]
use crate::css;
use crate::css::types::ColorStop;
use crate::css::types::RadialGradientShape;
use crate::css::types::RadialGradientSize;
use crate::debug::runtime;
use crate::debug::trace::TraceHandle;
use crate::error::Error;
use crate::error::RenderError;
use crate::error::RenderStage;
use crate::error::Result;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::image_loader::ImageCache;
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::layout::contexts::inline::line_builder::TextItem;
use crate::layout::utils::{resolve_font_relative_length, resolve_length_with_percentage_metrics};
use crate::math::MathFragment;
use crate::paint::blur::apply_gaussian_blur;
use crate::paint::canvas::draw_pixmap_with_plus_blend;
use crate::paint::clip_path::resolve_clip_path;
use crate::paint::clip_path::ResolvedClipPath;
use crate::paint::display_list::BorderGap;
use crate::paint::display_list::BorderRadii;
use crate::paint::display_list::ImageData;
use crate::paint::display_list::Transform2D;
use crate::paint::display_list::Transform3D;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::filter_outset::{compute_filter_outset, FilterOutsetExt};
use crate::paint::gradient::{
  gradient_bucket, rasterize_conic_gradient, rasterize_linear_gradient, GradientLutCache,
  GradientStats,
};
use crate::paint::homography::{quad_bounds, rect_corners, Homography};
use crate::paint::iframe::{
  render_iframe_src, render_iframe_srcdoc, DefaultIframeEmbedder, IframeEmbedder, IframePaintAction,
  IframePaintInfo,
};
use crate::paint::object_fit::compute_object_fit;
use crate::paint::object_fit::default_object_position;
use crate::paint::optimize::DisplayListOptimizer;
use crate::paint::pixmap::{
  copy_pixmap_rgba_into_strided_buffer, new_pixmap, reserve_buffer, MAX_PIXMAP_BYTES,
};
use crate::paint::projective_warp::warp_pixmap;
use crate::paint::rasterize::fill_rounded_rect;
use crate::paint::stacking::creates_stacking_context;
use crate::paint::svg_filter::SvgFilterResolver;
use crate::paint::text_decoration::{resolve_underline_side, UnderlineSide};
use crate::paint::text_shadow::resolve_text_shadows_with_viewport;
use crate::paint::text_shadow::PathBounds;
use crate::paint::text_shadow::ResolvedTextShadow;
use crate::paint::transform_resolver::{backface_is_hidden, resolve_transform3d};
use crate::render_control::{
  active_deadline, check_active, check_active_periodic, record_stage, with_deadline,
  RenderDeadline, StageHeartbeat,
};
use crate::scroll::ScrollState;
#[cfg(test)]
use crate::style::color::Color;
use crate::style::color::Rgba;
use crate::style::display::Display;
use crate::style::position::Position;
use crate::style::types::AccentColor;
use crate::style::types::Appearance;
use crate::style::types::BackfaceVisibility;
use crate::style::types::BackgroundAttachment;
use crate::style::types::BackgroundImage;
use crate::style::types::BackgroundLayer;
use crate::style::types::BackgroundPosition;
use crate::style::types::BackgroundRepeatKeyword;
use crate::style::types::BackgroundSize;
use crate::style::types::BackgroundSizeComponent;
use crate::style::types::BackgroundSizeKeyword;
use crate::style::types::BorderImage;
use crate::style::types::BorderImageOutsetValue;
use crate::style::types::BorderImageRepeat;
use crate::style::types::BorderImageSource;
use crate::style::types::BorderImageWidthValue;
use crate::style::types::BorderStyle as CssBorderStyle;
use crate::style::types::CaretColor;
use crate::style::types::ClipComponent;
use crate::style::types::FilterColor;
use crate::style::types::FilterFunction;
#[cfg(test)]
use crate::style::types::FontWeight;
use crate::style::types::ImageOrientation;
use crate::style::types::ImageRendering;
use crate::style::types::MaskBorderMode;
use crate::style::types::MaskClip;
use crate::style::types::MaskComposite;
use crate::style::types::MaskMode;
use crate::style::types::MaskOrigin;
use crate::style::types::MixBlendMode;
use crate::style::types::ObjectFit;
use crate::style::types::Overflow;
use crate::style::types::TextDecorationLine;
use crate::style::types::TextDecorationSkipInk;
use crate::style::types::TextDecorationStyle;
use crate::style::types::TextDecorationThickness;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::style::ComputedStyle;
use crate::style::PhysicalSide;
use crate::text::caret::{caret_stops_for_runs, caret_x_for_position, CaretAffinity};
use crate::text::font_db::FontStretch;
use crate::text::font_db::FontStyle;
use crate::text::font_db::ScaledMetrics;
use crate::text::font_instance::{glyph_transform, FontInstance};
use crate::text::font_loader::FontContext;
#[cfg(test)]
use crate::text::pipeline::Direction as TextDirection;
use crate::text::pipeline::ShapedRun;
use crate::text::pipeline::ShapingPipeline;
use crate::text::pipeline::{
  record_text_rasterize, text_diagnostics_timer, TextCacheStats, TextDiagnosticsStage,
};
#[cfg(test)]
use crate::text::RunRotation;
use crate::tree;
use crate::tree::box_tree::ReplacedBox;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SvgContent;
use crate::tree::box_tree::SvgDocumentCssInjection;
use crate::tree::box_tree::{
  CrossOriginAttribute, FormControl, FormControlKind, ImageDecodingAttribute,
  ImageLoadingAttribute, SelectItem, TextControlKind,
};
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;
#[cfg(test)]
use crate::FontDatabase;
#[cfg(test)]
use encoding_rs::Encoding;
use std::borrow::Cow;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::fs;
use std::ops::Range;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tiny_skia::BlendMode as SkiaBlendMode;
use tiny_skia::FilterQuality;
use tiny_skia::IntSize;
use tiny_skia::LinearGradient;
use tiny_skia::Mask;
#[cfg(test)]
use tiny_skia::MaskType;
use tiny_skia::Paint;
use tiny_skia::PathBuilder;
use tiny_skia::Pattern;
use tiny_skia::Pixmap;
use tiny_skia::PixmapPaint;
use tiny_skia::PixmapRef;
use tiny_skia::PremultipliedColorU8;
use tiny_skia::RadialGradient;
use tiny_skia::Rect as SkiaRect;
use tiny_skia::SpreadMode;
use tiny_skia::Stroke;
use tiny_skia::Transform;
#[cfg(test)]
use url::Url;

type RenderResult<T> = std::result::Result<T, RenderError>;

/// See `ASYNC_IMAGE_DECODE_MAX_DEST_PIXELS` in `paint/display_list_builder.rs` for motivation.
const ASYNC_IMAGE_DECODE_MAX_DEST_PIXELS: u64 = 200_000;

#[inline]
fn is_ascii_whitespace_html_css(ch: char) -> bool {
  matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace_html_css(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html_css)
}

fn trim_ascii_whitespace_start_html_css(value: &str) -> &str {
  value.trim_start_matches(is_ascii_whitespace_html_css)
}

/// Main painter that rasterizes a FragmentTree to pixels
pub struct Painter {
  /// The pixmap being painted to
  pixmap: Pixmap,
  /// CSS-to-device scale factor (device pixel ratio)
  scale: f32,
  /// CSS-space origin of this painter's pixmap.
  ///
  /// Display commands remain in absolute CSS coordinates. For intermediate stacking-context
  /// layers we allocate a smaller pixmap covering a sub-rectangle of the page and set
  /// `origin_offset_css` to that sub-rectangle's top-left so that CSS coordinates map into the
  /// layer's local (0,0)-based pixel space without cloning/rewriting the command tree.
  origin_offset_css: Point,
  /// Logical viewport width in CSS px
  css_width: f32,
  /// Logical viewport height in CSS px
  css_height: f32,
  /// Background color
  background: Rgba,
  /// Text shaping pipeline
  shaper: ShapingPipeline,
  /// Font context for resolution
  font_ctx: FontContext,
  /// Image cache for replaced content
  image_cache: ImageCache,
  /// Optional media provider used to supply decoded frames (e.g. `<video>`) during paint.
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  /// SVG defs elements (by id) serialized from the DOM (document-level registry).
  svg_id_defs: Option<Arc<HashMap<String, String>>>,
  /// Raw SVG defs elements (by id) serialized from the DOM (document-level registry).
  ///
  /// Used to inline same-document fragment references across sibling `<svg>` roots (e.g. sprite
  /// sheets referenced by `<use href="#...">`) while preserving `currentColor` semantics.
  svg_id_defs_raw: Option<Arc<HashMap<String, String>>>,
  /// Cache of shaped runs keyed by style and text to avoid reshaping identical content during paint
  text_shape_cache: Arc<Mutex<HashMap<TextCacheKey, Arc<Vec<ShapedRun>>>>>,
  /// Optional trace collector for Chrome trace output.
  trace: TraceHandle,
  /// Scroll offsets for viewport and element scroll containers.
  scroll_state: ScrollState,
  /// Remaining iframe nesting depth allowed for this document.
  max_iframe_depth: usize,
  /// Cached gradient lookup tables reused within a render.
  gradient_cache: GradientLutCache,
  /// Accumulated gradient raster statistics for diagnostics.
  gradient_stats: GradientStats,
  /// Whether diagnostics are enabled for this render.
  diagnostics_enabled: bool,
}

#[derive(Default)]
struct PaintStats {
  background_ms: f64,
  collect_ms: f64,
  execute_ms: f64,
  commands: usize,
  backgrounds: (usize, f64),
  borders: (usize, f64),
  text: (usize, f64),
  replaced: (usize, f64),
  outline: (usize, f64),
  stacking: (usize, f64),
}

#[derive(Debug, Default, Clone)]
pub struct PaintDiagnosticsSummary {
  pub command_count: usize,
  pub build_ms: f64,
  pub build_stacking_tree_ms: f64,
  pub build_stacking_tree_calls: u64,
  pub build_fragment_paint_bounds_ms: f64,
  pub build_fragment_paint_bounds_calls: u64,
  pub build_text_shape_ms: f64,
  pub build_text_shape_calls: u64,
  pub build_text_decoration_ms: f64,
  pub build_text_decoration_calls: u64,
  pub build_image_decode_ms: f64,
  pub build_image_decode_calls: u64,
  pub build_clip_path_ms: f64,
  pub build_clip_path_calls: u64,
  pub build_border_radii_ms: f64,
  pub build_border_radii_calls: u64,
  pub build_svg_filter_ms: f64,
  pub build_svg_filter_calls: u64,
  pub optimize_ms: f64,
  pub optimize_original_items: usize,
  pub optimize_final_items: usize,
  pub optimize_culled: usize,
  pub optimize_transparent_removed: usize,
  pub optimize_noop_removed: usize,
  pub optimize_merged: usize,
  pub raster_ms: f64,
  pub gradient_ms: f64,
  pub gradient_pixels: u64,
  pub gradient_pixmap_cache_hits: u64,
  pub gradient_pixmap_cache_misses: u64,
  pub gradient_pixmap_cache_bytes: u64,
  pub image_pixmap_cache_hits: u64,
  pub image_pixmap_cache_misses: u64,
  pub image_pixmap_ms: f64,
  pub background_tiles: u64,
  pub background_ms: f64,
  pub background_layers: u64,
  pub background_pattern_fast_paths: u64,
  pub clip_mask_calls: u64,
  pub clip_mask_ms: f64,
  pub clip_mask_pixels: u64,
  pub layer_allocations: u64,
  pub layer_alloc_bytes: u64,
  pub backdrop_composite_allocations: u64,
  pub backdrop_composite_bytes: u64,
  pub backdrop_composite_cache_hits: u64,
  pub backdrop_composite_cache_misses: u64,
  pub parallel_tasks: usize,
  pub parallel_threads: usize,
  pub parallel_fallback_reason: Option<String>,
  pub parallel_ms: f64,
  pub serial_ms: f64,
  pub filter_cache_hits: usize,
  pub filter_cache_misses: usize,
  pub svg_filter_recursion_limit_hits: usize,
  pub blur_cache_hits: usize,
  pub blur_cache_misses: usize,
  pub blur_tiles: usize,
  pub blur_calls: u64,
  pub blur_ms: f64,
  pub blur_pixels: u64,
  pub blur_bytes: u64,
  pub blur_cancellations: u64,
}

static PAINT_DIAGNOSTICS_ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
static PAINT_DIAGNOSTICS_NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static PAINT_DIAGNOSTICS_SESSIONS: OnceLock<Mutex<HashMap<u64, PaintDiagnosticsSummary>>> =
  OnceLock::new();

thread_local! {
  static PAINT_DIAGNOSTICS_SESSION_ID: Cell<u64> = const { Cell::new(0) };
}

fn paint_diagnostics_sessions() -> &'static Mutex<HashMap<u64, PaintDiagnosticsSummary>> {
  PAINT_DIAGNOSTICS_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Temporarily enables paint diagnostics collection for the current thread.
///
/// Paint work can execute on rayon worker threads (e.g., parallel tiling). The render thread must
/// explicitly opt worker threads into collection so thread-local `paint_diagnostics_enabled()`
/// checks remain cheap and so unrelated work running on other threads isn't counted.
pub(crate) struct PaintDiagnosticsThreadGuard {
  prev_session_id: u64,
}

impl PaintDiagnosticsThreadGuard {
  pub(crate) fn enter(session_id: u64) -> Self {
    let prev_session_id = PAINT_DIAGNOSTICS_SESSION_ID.with(|cell| {
      let prev = cell.get();
      cell.set(session_id);
      prev
    });
    Self { prev_session_id }
  }
}

impl Drop for PaintDiagnosticsThreadGuard {
  fn drop(&mut self) {
    PAINT_DIAGNOSTICS_SESSION_ID.with(|cell| cell.set(self.prev_session_id));
  }
}

pub(crate) fn paint_diagnostics_session_id() -> Option<u64> {
  if PAINT_DIAGNOSTICS_ACTIVE_SESSIONS.load(Ordering::Relaxed) == 0 {
    return None;
  }
  PAINT_DIAGNOSTICS_SESSION_ID.with(|cell| {
    let id = cell.get();
    (id != 0).then_some(id)
  })
}

pub(crate) fn enable_paint_diagnostics() {
  let session_id = PAINT_DIAGNOSTICS_NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
  let prev_session_id = PAINT_DIAGNOSTICS_SESSION_ID.with(|cell| {
    let prev = cell.get();
    cell.set(session_id);
    prev
  });
  let mut guard = paint_diagnostics_sessions()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  if prev_session_id != 0 {
    if guard.remove(&prev_session_id).is_some() {
      PAINT_DIAGNOSTICS_ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
  }
  guard.insert(session_id, PaintDiagnosticsSummary::default());
  PAINT_DIAGNOSTICS_ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn take_paint_diagnostics() -> Option<PaintDiagnosticsSummary> {
  let session_id = PAINT_DIAGNOSTICS_SESSION_ID.with(|cell| {
    let id = cell.get();
    cell.set(0);
    id
  });
  if session_id == 0 {
    return None;
  }
  let mut guard = paint_diagnostics_sessions()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let stats = guard.remove(&session_id);
  if stats.is_some() {
    PAINT_DIAGNOSTICS_ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
  }
  stats
}

pub(crate) fn paint_diagnostics_enabled() -> bool {
  paint_diagnostics_session_id().is_some()
}

pub(crate) fn with_paint_diagnostics<F: FnOnce(&mut PaintDiagnosticsSummary)>(f: F) {
  let Some(session_id) = paint_diagnostics_session_id() else {
    return;
  };
  let mut guard = paint_diagnostics_sessions()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  if let Some(stats) = guard.get_mut(&session_id) {
    f(stats);
  }
}

#[cfg(test)]
mod diagnostics_tests {
  use super::*;

  #[test]
  fn paint_diagnostics_sessions_are_thread_isolated() {
    assert!(!paint_diagnostics_enabled());
    assert!(take_paint_diagnostics().is_none());

    enable_paint_diagnostics();
    assert!(paint_diagnostics_enabled());
    with_paint_diagnostics(|diag| diag.blur_calls = 1);
    let stats = take_paint_diagnostics().expect("diagnostics enabled");
    assert_eq!(stats.blur_calls, 1);
    assert!(!paint_diagnostics_enabled());

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for idx in 0..2u64 {
      let barrier = barrier.clone();
      handles.push(std::thread::spawn(move || {
        assert!(!paint_diagnostics_enabled());
        enable_paint_diagnostics();
        with_paint_diagnostics(|diag| diag.blur_calls = idx + 1);
        barrier.wait();
        let stats = take_paint_diagnostics().expect("diagnostics enabled");
        assert_eq!(stats.blur_calls, idx + 1);
        assert!(!paint_diagnostics_enabled());
      }));
    }
    for handle in handles {
      handle.join().expect("thread should not panic");
    }
  }

  #[test]
  fn paint_diagnostics_survives_poisoned_lock() {
    let result = std::panic::catch_unwind(|| {
      let _guard = paint_diagnostics_sessions().lock().unwrap();
      panic!("poison paint diagnostics lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    assert!(
      paint_diagnostics_sessions().is_poisoned(),
      "expected paint diagnostics mutex to be poisoned"
    );

    enable_paint_diagnostics();
    with_paint_diagnostics(|diag| diag.blur_calls = 2);
    let stats = take_paint_diagnostics().expect("diagnostics enabled");
    assert_eq!(stats.blur_calls, 2);
  }

  #[test]
  fn display_list_build_budget_scales_with_remaining_deadline() {
    // When rendering under a long overall deadline (e.g. fixture/page-loop renders with ~minutes of
    // budget), we still want the display-list pipeline to run rather than immediately falling back
    // to the legacy painter. Historically the builder budget was capped at 1s, which was too
    // aggressive for real pages and caused large fidelity regressions (notably dropping fixed
    // chrome on fandom.com). Keep the cap generous and deterministic.
    assert_eq!(
      display_list_build_budget_from_remaining(Duration::from_secs(120)),
      DISPLAY_LIST_BUILD_BUDGET_CAP
    );
    assert_eq!(
      display_list_build_budget_from_remaining(Duration::from_secs(6)),
      Duration::from_secs(3)
    );
    assert_eq!(
      display_list_optimize_budget_from_remaining(Duration::from_secs(120)),
      DISPLAY_LIST_OPTIMIZE_BUDGET_CAP
    );
  }
}

#[derive(Copy, Clone)]
struct RootPaintOptions {
  use_root_background: bool,
  extend_background_to_viewport: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextCacheKey {
  style_ptr: usize,
  font_size_bits: u32,
  text: String,
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

impl TextCacheKey {
  fn new(style_ptr: usize, font_size: f32, text: &str) -> Self {
    Self {
      style_ptr,
      font_size_bits: f32_to_canonical_bits(font_size),
      text: text.to_string(),
    }
  }
}

fn dump_stack_enabled() -> bool {
  runtime::runtime_toggles().truthy("FASTR_DUMP_STACK")
}

fn dump_fragments_enabled() -> bool {
  runtime::runtime_toggles().truthy("FASTR_DUMP_FRAGMENTS")
}

fn trace_image_paint_limit() -> Option<usize> {
  runtime::runtime_toggles().usize("FASTR_TRACE_IMAGE_PAINT")
}

static TRACE_IMAGE_PAINT_COUNT: AtomicUsize = AtomicUsize::new(0);

fn dump_counts_enabled() -> bool {
  runtime::runtime_toggles().truthy("FASTR_DUMP_COUNTS")
}

fn stack_profile_threshold_ms() -> Option<f64> {
  runtime::runtime_toggles().f64("FASTR_STACK_PROFILE_MS")
}

fn text_profile_threshold_ms() -> Option<f64> {
  runtime::runtime_toggles().f64("FASTR_TEXT_PROFILE_MS")
}

fn cmd_profile_threshold_ms() -> Option<f64> {
  runtime::runtime_toggles().f64("FASTR_CMD_PROFILE_MS")
}

fn legacy_generated_gradient_tile_raster_bytes_threshold() -> u64 {
  // Legacy background painting for generated gradients rasterizes the entire tile into a pixmap.
  // Large origin boxes (e.g. when layout explodes) can cause huge allocations/work even if only a
  // tiny portion is visible. Clamp by default to keep work proportional to what's on-screen.
  runtime::runtime_toggles()
    .u64("FASTR_LEGACY_GRADIENT_TILE_MAX_BYTES")
    .unwrap_or(64 * 1024 * 1024)
}

fn pixmap_allocation_bytes(width: u32, height: u32) -> Option<u64> {
  u64::from(width)
    .checked_mul(u64::from(height))
    .and_then(|px| px.checked_mul(4))
}

#[inline]
fn record_layer_allocation(width: u32, height: u32) {
  with_paint_diagnostics(|diag| {
    diag.layer_allocations += 1;
    if let Some(bytes) = pixmap_allocation_bytes(width, height) {
      diag.layer_alloc_bytes += bytes;
    }
  });
}

fn nested_counts(cmds: &[DisplayCommand]) -> (usize, usize) {
  cmds.iter().fold((0, 0), |(total, text), cmd| match cmd {
    DisplayCommand::StackingContext { commands, .. } => {
      let (t, tx) = nested_counts(commands);
      (total + 1 + t, text + tx)
    }
    DisplayCommand::Text { .. } => (total + 1, text + 1),
    _ => (total + 1, text),
  })
}

impl PaintStats {
  fn record(&mut self, cmd: &DisplayCommand, duration: std::time::Duration) {
    let ms = duration.as_secs_f64() * 1000.0;
    self.commands += 1;
    match cmd {
      DisplayCommand::Background { .. } => {
        self.backgrounds.0 += 1;
        self.backgrounds.1 += ms;
      }
      DisplayCommand::Border { .. } => {
        self.borders.0 += 1;
        self.borders.1 += ms;
      }
      DisplayCommand::Text { .. } => {
        self.text.0 += 1;
        self.text.1 += ms;
      }
      DisplayCommand::Replaced { .. } => {
        self.replaced.0 += 1;
        self.replaced.1 += ms;
      }
      DisplayCommand::Outline { .. } => {
        self.outline.0 += 1;
        self.outline.1 += ms;
      }
      DisplayCommand::StackingContext { .. } => {
        self.stacking.0 += 1;
        self.stacking.1 += ms;
      }
    }
    self.execute_ms += ms;
  }

  fn log(&self) {
    eprintln!(
      "paint_stats commands={} background_ms_ms={:.2} collect_ms_ms={:.2} execute_ms_ms={:.2}",
      self.commands, self.background_ms, self.collect_ms, self.execute_ms
    );
    eprintln!(
            " paint_breakdown backgrounds count={} time_ms={:.2} borders count={} time_ms={:.2} text count={} time_ms={:.2} replaced count={} time_ms={:.2} outline count={} time_ms={:.2} stacking count={} time_ms={:.2}",
            self.backgrounds.0,
            self.backgrounds.1,
            self.borders.0,
            self.borders.1,
            self.text.0,
            self.text.1,
            self.replaced.0,
            self.replaced.1,
            self.outline.0,
            self.outline.1,
            self.stacking.0,
            self.stacking.1,
        );
  }
}

#[derive(Debug, Clone)]
enum ResolvedFilter {
  Blur(f32),
  Brightness(f32),
  Contrast(f32),
  Grayscale(f32),
  Sepia(f32),
  Saturate(f32),
  HueRotate(f32),
  Invert(f32),
  Opacity(f32),
  DropShadow {
    offset_x: f32,
    offset_y: f32,
    blur_radius: f32,
    spread: f32,
    color: Rgba,
  },
  SvgFilter(Arc<crate::paint::svg_filter::SvgFilter>),
}

impl FilterOutsetExt for ResolvedFilter {
  fn expand_outset(&self, bbox: Rect, scale: f32, out: &mut (f32, f32, f32, f32)) {
    match self {
      ResolvedFilter::Blur(radius) => {
        let delta = (radius * scale).abs() * 3.0;
        out.0 += delta;
        out.1 += delta;
        out.2 += delta;
        out.3 += delta;
      }
      ResolvedFilter::DropShadow {
        offset_x,
        offset_y,
        blur_radius,
        spread,
        ..
      } => {
        let dx = offset_x * scale;
        let dy = offset_y * scale;
        let blur = blur_radius * scale;
        let spread = spread * scale;
        let delta = (blur.abs() * 3.0 + spread).max(0.0);
        let shadow_left = out.0 + delta - dx;
        let shadow_right = out.2 + delta + dx;
        let shadow_top = out.1 + delta - dy;
        let shadow_bottom = out.3 + delta + dy;
        out.0 = out.0.max(shadow_left);
        out.2 = out.2.max(shadow_right);
        out.1 = out.1.max(shadow_top);
        out.3 = out.3.max(shadow_bottom);
      }
      ResolvedFilter::SvgFilter(filter) => {
        let region = filter.resolve_region(bbox);
        let delta_left = (bbox.min_x() - region.min_x()).max(0.0) * scale;
        let delta_top = (bbox.min_y() - region.min_y()).max(0.0) * scale;
        let delta_right = (region.max_x() - bbox.max_x()).max(0.0) * scale;
        let delta_bottom = (region.max_y() - bbox.max_y()).max(0.0) * scale;
        out.0 = out.0.max(delta_left);
        out.1 = out.1.max(delta_top);
        out.2 = out.2.max(delta_right);
        out.3 = out.3.max(delta_bottom);
      }
      _ => {}
    }
  }
}

#[derive(Debug, Clone)]
enum DisplayCommand {
  Background {
    rect: Rect,
    style: Arc<ComputedStyle>,
    text_clip: Option<Arc<[DisplayCommand]>>,
    scroll_delta: Point,
  },
  Border {
    rect: Rect,
    style: Arc<ComputedStyle>,
    gap: Option<BorderGap>,
  },
  Text {
    rect: Rect,
    baseline_offset: f32,
    text: Arc<str>,
    runs: Option<Arc<Vec<ShapedRun>>>,
    style: Arc<ComputedStyle>,
    document_selection: Option<Arc<Vec<Range<usize>>>>,
  },
  Replaced {
    rect: Rect,
    replaced_type: ReplacedType,
    box_id: Option<usize>,
    style: Arc<ComputedStyle>,
  },
  Outline {
    rect: Rect,
    style: Arc<ComputedStyle>,
  },
  StackingContext {
    rect: Rect,
    opacity: f32,
    transform: Option<Transform>,
    transform_3d: Option<Transform3D>,
    blend_mode: MixBlendMode,
    isolated: bool,
    mask: Option<Arc<ComputedStyle>>,
    mask_border: Option<Arc<ComputedStyle>>,
    filters: Vec<ResolvedFilter>,
    backdrop_filters: Vec<ResolvedFilter>,
    radii: BorderRadii,
    clip: Option<StackingClip>,
    /// Whether the stacking context root has a non-`none` `clip-path`.
    ///
    /// This is tracked separately from `clip_path` because certain `clip-path` values (such as a
    /// degenerate polygon/path, or an unresolved `url(#id)`) resolve to no mask but still need to
    /// force stacking-context allocation to keep backdrop-root semantics consistent.
    has_clip_path: bool,
    clip_path: Option<StackingClipPath>,
    /// Style for the stacking-context root fragment, used to disambiguate which background/border
    /// commands belong to the root when applying overflow clipping.
    ///
    /// Overflow clips apply to descendants, not the root element's own background/borders. The
    /// legacy painter renders overflow-clipped descendants into a secondary layer and composites it
    /// above the root decorations. Without this hint we'd have to guess which background commands
    /// belong to the root, and descendants that happen to cover the full rect (common) could be
    /// misclassified as root decorations and incorrectly moved behind clipped content.
    root_style: Option<Arc<ComputedStyle>>,
    commands: Vec<DisplayCommand>,
  },
}

impl Drop for DisplayCommand {
  fn drop(&mut self) {
    // Deeply nested `DisplayCommand::StackingContext` trees can overflow the stack when dropped
    // recursively (each context owns a `Vec<DisplayCommand>` of descendants). Drain nested command
    // vectors iteratively to keep drops stack-safe.
    let DisplayCommand::StackingContext { commands, .. } = self else {
      return;
    };
    if commands.is_empty() {
      return;
    }

    let mut stack = Vec::new();
    let mut pending = std::mem::take(commands);
    stack.append(&mut pending);

    while let Some(mut cmd) = stack.pop() {
      if let DisplayCommand::StackingContext { commands, .. } = &mut cmd {
        if !commands.is_empty() {
          let mut inner = std::mem::take(commands);
          stack.append(&mut inner);
        }
      }
      // `cmd` is dropped here with its nested commands already drained into `stack`.
    }
  }
}

/// Maximum allowed nesting depth when executing legacy display commands.
///
/// The legacy painter's `DisplayCommand::StackingContext` execution was historically recursive.
/// Deeply nested stacking contexts can overflow small thread stacks, so we enforce a conservative
/// limit and surface a structured error instead of crashing the process.
const LEGACY_DISPLAY_COMMAND_NESTING_LIMIT: usize = 64;

fn commands_have_backdrop_sensitive_descendants(commands: &[DisplayCommand]) -> bool {
  let mut stack: Vec<&[DisplayCommand]> = vec![commands];
  while let Some(cmds) = stack.pop() {
    for cmd in cmds {
      let DisplayCommand::StackingContext {
        blend_mode,
        backdrop_filters,
        commands,
        ..
      } = cmd
      else {
        continue;
      };
      if !matches!(blend_mode, MixBlendMode::Normal) || !backdrop_filters.is_empty() {
        return true;
      }
      if !commands.is_empty() {
        stack.push(commands);
      }
    }
  }
  false
}

#[derive(Debug, Clone, Copy)]
struct StackingClip {
  rect: Rect,
  radii: BorderRadii,
  clip_x: bool,
  clip_y: bool,
  /// Whether this clip should apply to the stacking-context root itself (e.g. CSS `clip`).
  /// Overflow clipping applies only to descendants.
  clip_root: bool,
}

#[derive(Debug, Clone)]
enum StackingClipPath {
  Shape(ResolvedClipPath),
  SvgFragment {
    /// Fragment id (without the leading `#`).
    id: String,
    /// Reference box rectangle in the stacking-context coordinate space.
    reference_rect: Rect,
  },
  SvgExternal {
    /// URL of the external SVG document (without the fragment).
    doc_url: String,
    /// Fragment id (without the leading `#`).
    id: String,
    /// Reference box rectangle in the stacking-context coordinate space.
    reference_rect: Rect,
  },
}

fn is_positioned(style: &ComputedStyle) -> bool {
  !matches!(style.position, Position::Static)
}

fn is_inline_level(style: &ComputedStyle, fragment: &FragmentNode) -> bool {
  let is_inline_display = matches!(
    style.display,
    Display::Inline
      | Display::InlineBlock
      | Display::InlineFlex
      | Display::InlineGrid
      | Display::InlineTable
  );

  let is_inline_content = matches!(
    fragment.content,
    FragmentContent::Inline { .. } | FragmentContent::Text { .. } | FragmentContent::Line { .. }
  );

  is_inline_display || is_inline_content
}

#[derive(Copy, Clone)]
enum EdgeOrientation {
  Horizontal,
  Vertical,
}

#[derive(Copy, Clone)]
enum BorderEdge {
  Top,
  Right,
  Bottom,
  Left,
}

impl BorderEdge {
  fn orientation(self) -> EdgeOrientation {
    match self {
      BorderEdge::Top | BorderEdge::Bottom => EdgeOrientation::Horizontal,
      BorderEdge::Left | BorderEdge::Right => EdgeOrientation::Vertical,
    }
  }

  /// Returns a pair of parallel paths offset by `offset` from the center line.
  fn parallel_lines(
    &self,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    offset: f32,
  ) -> (Option<tiny_skia::Path>, Option<tiny_skia::Path>) {
    match self.orientation() {
      EdgeOrientation::Horizontal => {
        let mut first = PathBuilder::new();
        first.move_to(x1, y1 - offset);
        first.line_to(x2, y2 - offset);

        let mut second = PathBuilder::new();
        second.move_to(x1, y1 + offset);
        second.line_to(x2, y2 + offset);

        (first.finish(), second.finish())
      }
      EdgeOrientation::Vertical => {
        let mut first = PathBuilder::new();
        first.move_to(x1 - offset, y1);
        first.line_to(x2 - offset, y2);

        let mut second = PathBuilder::new();
        second.move_to(x1 + offset, y1);
        second.line_to(x2 + offset, y2);

        (first.finish(), second.finish())
      }
    }
  }

  fn groove_ridge_colors(self, base: &Rgba, style: CssBorderStyle) -> (Rgba, Rgba) {
    let lighten = |c: &Rgba| shade_color(c, 1.25);
    let darken = |c: &Rgba| shade_color(c, 0.75);

    let (first_light, second_light) = match style {
      CssBorderStyle::Groove => (false, true),
      CssBorderStyle::Ridge => (true, false),
      _ => (false, false),
    };

    let (first, second) = match self {
      BorderEdge::Top | BorderEdge::Left => (first_light, second_light),
      BorderEdge::Right | BorderEdge::Bottom => (!first_light, !second_light),
    };

    (
      if first { lighten(base) } else { darken(base) },
      if second { lighten(base) } else { darken(base) },
    )
  }

  fn inset_outset_color(self, base: &Rgba, style: CssBorderStyle) -> Rgba {
    match style {
      CssBorderStyle::Inset => match self {
        BorderEdge::Top | BorderEdge::Left => shade_color(base, 0.75),
        BorderEdge::Right | BorderEdge::Bottom => shade_color(base, 1.25),
      },
      CssBorderStyle::Outset => match self {
        BorderEdge::Top | BorderEdge::Left => shade_color(base, 1.25),
        BorderEdge::Right | BorderEdge::Bottom => shade_color(base, 0.75),
      },
      _ => *base,
    }
  }
}

fn shade_color(color: &Rgba, factor: f32) -> Rgba {
  let clamp_to_u8 = |v: f32| v.clamp(0.0, 255.0) as u8;
  let r = clamp_to_u8(color.r as f32 * factor);
  let g = clamp_to_u8(color.g as f32 * factor);
  let b = clamp_to_u8(color.b as f32 * factor);
  Rgba::new(r, g, b, color.a)
}

pub(crate) fn snap_upscale(target: f32, raw: f32) -> Option<(f32, f32)> {
  if target <= 0.0 || raw <= 0.0 || target <= raw {
    return None;
  }
  let scale = target / raw;
  if scale <= 1.0 {
    return None;
  }
  let snapped = (scale.floor().max(1.0)) * raw;
  let snapped = snapped.min(target);
  let offset = (target - snapped) * 0.5;
  Some((snapped, offset))
}

impl Painter {
  fn resolve_scaled_metrics(&self, style: &ComputedStyle) -> Option<ScaledMetrics> {
    let italic = matches!(style.font_style, crate::style::types::FontStyle::Italic);
    let oblique = matches!(style.font_style, crate::style::types::FontStyle::Oblique(_));
    let stretch = FontStretch::from_percentage(style.font_stretch.to_percentage());
    let preferred_aspect = crate::text::pipeline::preferred_font_aspect(style, &self.font_ctx);

    self
      .font_ctx
      .get_font_full(
        &style.font_family,
        style.font_weight.to_u16(),
        if italic {
          FontStyle::Italic
        } else if oblique {
          FontStyle::Oblique
        } else {
          FontStyle::Normal
        },
        stretch,
      )
      .or_else(|| self.font_ctx.get_sans_serif())
      .and_then(|font| {
        let used_font_size =
          crate::text::pipeline::compute_adjusted_font_size(style, &font, preferred_aspect);
        let authored = crate::text::variations::authored_variations_from_style(style);
        let variations = crate::text::face_cache::with_face(&font, |face| {
          crate::text::variations::collect_variations_for_face(
            face,
            style,
            &font,
            used_font_size,
            &authored,
          )
        })
        .unwrap_or_else(|| authored.clone());
        self
          .font_ctx
          .get_scaled_metrics_with_variations(&font, used_font_size, &variations)
      })
  }

  fn resolve_scaled_metrics_static(
    style: &ComputedStyle,
    font_ctx: &FontContext,
  ) -> Option<ScaledMetrics> {
    let italic = matches!(style.font_style, crate::style::types::FontStyle::Italic);
    let oblique = matches!(style.font_style, crate::style::types::FontStyle::Oblique(_));
    let stretch = FontStretch::from_percentage(style.font_stretch.to_percentage());
    let preferred_aspect = crate::text::pipeline::preferred_font_aspect(style, font_ctx);

    font_ctx
      .get_font_full(
        &style.font_family,
        style.font_weight.to_u16(),
        if italic {
          FontStyle::Italic
        } else if oblique {
          FontStyle::Oblique
        } else {
          FontStyle::Normal
        },
        stretch,
      )
      .or_else(|| font_ctx.get_sans_serif())
      .and_then(|font| {
        let used_font_size =
          crate::text::pipeline::compute_adjusted_font_size(style, &font, preferred_aspect);
        let authored = crate::text::variations::authored_variations_from_style(style);
        let variations = crate::text::face_cache::with_face(&font, |face| {
          crate::text::variations::collect_variations_for_face(
            face,
            style,
            &font,
            used_font_size,
            &authored,
          )
        })
        .unwrap_or_else(|| authored.clone());
        font_ctx.get_scaled_metrics_with_variations(&font, used_font_size, &variations)
      })
  }

  fn element_scroll_offset(&self, fragment: &FragmentNode) -> Point {
    let Some(style) = fragment.style.as_deref() else {
      return Point::ZERO;
    };

    // Element scroll offsets apply only to scroll containers.
    //
    // CSS Overflow 3: `overflow: clip` forbids scrolling entirely (not a scroll container), while
    // `overflow: hidden|scroll|auto` are scroll containers (with `hidden` allowing programmatic
    // scrolling).
    let mut offset = fragment
      .box_id()
      .and_then(|id| self.scroll_state.elements.get(&id).copied())
      .unwrap_or(Point::ZERO);
    if !matches!(
      style.overflow_x,
      Overflow::Hidden | Overflow::Scroll | Overflow::Auto
    ) {
      offset.x = 0.0;
    }
    if !matches!(
      style.overflow_y,
      Overflow::Hidden | Overflow::Scroll | Overflow::Auto
    ) {
      offset.y = 0.0;
    }
    offset
  }

  fn filter_quality_for_image(style: Option<&ComputedStyle>) -> FilterQuality {
    match style.map(|s| s.image_rendering) {
      Some(ImageRendering::CrispEdges) | Some(ImageRendering::Pixelated) => FilterQuality::Nearest,
      _ => FilterQuality::Bilinear,
    }
  }

  fn should_use_scaled_raster_pixmap(
    quality: FilterQuality,
    src_width: u32,
    src_height: u32,
    target_width: u32,
    target_height: u32,
  ) -> bool {
    if quality != FilterQuality::Bilinear {
      return false;
    }
    if src_width == 0 || src_height == 0 || target_width == 0 || target_height == 0 {
      return false;
    }
    if target_width >= src_width || target_height >= src_height {
      return false;
    }

    let src_pixels = u64::from(src_width).saturating_mul(u64::from(src_height));
    let target_pixels = u64::from(target_width).saturating_mul(u64::from(target_height));

    // Generating a downscaled pixmap requires bilinear resampling (4 source taps per output pixel),
    // so it only pays off when the destination is substantially smaller than the intrinsic image.
    // Keep raster→pixmap conversion work proportional to on-screen pixel count without regressing
    // common "slightly downscaled" cases.
    target_pixels.saturating_mul(4) <= src_pixels
  }

  fn resolve_replaced_intrinsic_sizes(&self, node: &mut tree::box_tree::BoxNode, viewport: Size) {
    use tree::box_tree::BoxType;
    use tree::box_tree::MarkerContent;
    if let BoxType::Marker(marker_box) = &mut node.box_type {
      if let MarkerContent::Image(replaced) = &mut marker_box.content {
        self.resolve_intrinsic_for_replaced(replaced, node.style.as_ref(), None, viewport);
      }
    }

    if let BoxType::Replaced(replaced_box) = &mut node.box_type {
      let alt = match &replaced_box.replaced_type {
        ReplacedType::Image { alt, .. } => alt.clone(),
        _ => None,
      };
      self.resolve_intrinsic_for_replaced(
        replaced_box,
        node.style.as_ref(),
        alt.as_deref(),
        viewport,
      );
    }

    for child in &mut node.children {
      self.resolve_replaced_intrinsic_sizes(child, viewport);
    }
  }

  fn resolve_intrinsic_for_replaced(
    &self,
    replaced_box: &mut ReplacedBox,
    style: &ComputedStyle,
    alt: Option<&str>,
    viewport: Size,
  ) {
    if let ReplacedType::Math(math) = &mut replaced_box.replaced_type {
      if math.layout.is_none() {
        let layout = crate::math::layout_mathml(&math.root, style, &self.font_ctx);
        math.layout = Some(Arc::new(layout));
      }
      if replaced_box.intrinsic_size.is_none() {
        if let Some(layout) = &math.layout {
          replaced_box.intrinsic_size = Some(layout.size());
          if layout.height > 0.0 {
            replaced_box.aspect_ratio = Some(layout.width / layout.height);
          }
        }
      }
      return;
    }

    let replaced_type_snapshot = replaced_box.replaced_type.clone();
    match replaced_type_snapshot {
      ReplacedType::FormControl(control) => {
        if replaced_box.intrinsic_size.is_none() {
          let metrics = self.resolve_scaled_metrics(style);
          replaced_box.intrinsic_size = Some(
            crate::tree::form_control_intrinsic::intrinsic_content_size_for_form_control(
              &control,
              style,
              viewport,
              metrics.as_ref(),
              &self.font_ctx,
              Some(&self.shaper),
            ),
          );
        }
      }
      ReplacedType::Image {
        src,
        alt: stored_alt,
        srcset,
        picture_sources,
        ..
      } => {
        let needs_intrinsic = replaced_box.intrinsic_size.is_none();
        let needs_ratio = replaced_box.aspect_ratio.is_none();
        let mut have_resource_dimensions = false;

        let has_source = !src.is_empty() || !srcset.is_empty() || !picture_sources.is_empty();

        let selected = if (needs_intrinsic || needs_ratio) && has_source {
          let media_ctx =
            crate::style::media::MediaContext::screen(viewport.width, viewport.height)
              .with_device_pixel_ratio(self.scale)
              .with_env_overrides();
          let cache_base = self.image_cache.base_url();
          Some(
            replaced_box
              .replaced_type
              .selected_image_source_for_context(crate::tree::box_tree::ImageSelectionContext {
                device_pixel_ratio: self.scale,
                slot_width: None,
                viewport: Some(viewport),
                media_context: Some(&media_ctx),
                font_size: Some(style.font_size),
                root_font_size: Some(style.root_font_size),
                base_url: cache_base.as_deref(),
              }),
          )
        } else {
          None
        };

        if let Some(selected) = selected {
          if !selected.url.is_empty() {
            if let Ok(img) = self.image_cache.load(selected.url) {
              let orientation = style.image_orientation.resolve(img.orientation, false);
              if let Some((w, h)) = img.css_dimensions(
                orientation,
                &style.image_resolution,
                self.scale,
                selected.density,
              ) {
                if needs_intrinsic {
                  replaced_box.intrinsic_size = Some(Size::new(w, h));
                }
                if needs_ratio && h > 0.0 {
                  replaced_box.aspect_ratio = Some(w / h);
                }
                have_resource_dimensions = true;
              }
            }
          }
        }

        if have_resource_dimensions {
          return;
        }

        let inherited_alt = alt.or(stored_alt.as_deref()).unwrap_or("");
        if let Some(text_size) = self.measure_alt_text(inherited_alt, style) {
          if needs_intrinsic && replaced_box.intrinsic_size.is_none() {
            replaced_box.intrinsic_size = Some(text_size);
          }
          if needs_ratio && replaced_box.aspect_ratio.is_none() && text_size.height > 0.0 {
            replaced_box.aspect_ratio = Some(text_size.width / text_size.height);
          }
        }
      }
      ReplacedType::Svg { content } => {
        let needs_intrinsic = replaced_box.intrinsic_size.is_none();
        let needs_ratio = replaced_box.aspect_ratio.is_none();
        if !needs_intrinsic && !needs_ratio {
          return;
        }
        let image = if trim_ascii_whitespace_start_html_css(&content.svg).starts_with('<') {
          self.image_cache.render_svg(&content.svg)
        } else {
          self.image_cache.load(&content.svg)
        };
        if let Ok(image) = image {
          let orientation = style.image_orientation.resolve(image.orientation, false);
          if let Some((w, h)) =
            image.css_dimensions(orientation, &style.image_resolution, self.scale, None)
          {
            if needs_intrinsic {
              replaced_box.intrinsic_size = Some(Size::new(w, h));
            }
            if needs_ratio && h > 0.0 {
              replaced_box.aspect_ratio = Some(w / h);
            }
          }
        }
      }
      ReplacedType::Embed { src: content } | ReplacedType::Object { data: content } => {
        let needs_intrinsic = replaced_box.intrinsic_size.is_none();
        let needs_ratio = replaced_box.aspect_ratio.is_none();
        if !needs_intrinsic && !needs_ratio {
          return;
        }
        let image = if trim_ascii_whitespace_start_html_css(&content).starts_with('<') {
          self.image_cache.render_svg(&content)
        } else {
          self.image_cache.load(&content)
        };
        if let Ok(image) = image {
          let orientation = style.image_orientation.resolve(image.orientation, false);
          if let Some((w, h)) =
            image.css_dimensions(orientation, &style.image_resolution, self.scale, None)
          {
            if needs_intrinsic {
              replaced_box.intrinsic_size = Some(Size::new(w, h));
            }
            if needs_ratio && h > 0.0 {
              replaced_box.aspect_ratio = Some(w / h);
            }
          }
        }
      }
      ReplacedType::Video {
        src: _src,
        poster,
        crossorigin,
        referrer_policy,
        ..
      } => {
        let needs_intrinsic = replaced_box.intrinsic_size.is_none();
        let needs_ratio = replaced_box.aspect_ratio.is_none();
        if !needs_intrinsic && !needs_ratio {
          return;
        }
        if let Some(poster) = poster {
          if let Ok(img) = self.image_cache.load_with_crossorigin_and_referrer_policy(
            &poster,
            crossorigin,
            referrer_policy,
          ) {
            let orientation = style.image_orientation.resolve(img.orientation, false);
            if let Some((w, h)) =
              img.css_dimensions(orientation, &style.image_resolution, self.scale, None)
            {
              if needs_intrinsic {
                replaced_box.intrinsic_size = Some(Size::new(w, h));
              }
              if needs_ratio && h > 0.0 {
                replaced_box.aspect_ratio = Some(w / h);
              }
              return;
            }
          }
        }

        if needs_intrinsic {
          replaced_box.intrinsic_size = Some(Size::new(300.0, 150.0));
        }
        if needs_ratio {
          replaced_box.aspect_ratio = Some(2.0);
        }
      }
      ReplacedType::Canvas => {
        if replaced_box.intrinsic_size.is_none() {
          replaced_box.intrinsic_size = Some(Size::new(300.0, 150.0));
        }
        if replaced_box.aspect_ratio.is_none() {
          replaced_box.aspect_ratio = Some(2.0);
        }
      }
      ReplacedType::Audio { .. } => {
        if replaced_box.intrinsic_size.is_none() {
          replaced_box.intrinsic_size = Some(Size::new(300.0, 32.0));
        }
        if replaced_box.aspect_ratio.is_none() {
          replaced_box.aspect_ratio = Some(300.0 / 32.0);
        }
      }
      ReplacedType::Iframe { .. } => {}
      ReplacedType::Math(_) => {}
    }
  }

  /// Creates a new painter with the given dimensions
  pub fn new(width: u32, height: u32, background: Rgba) -> Result<Self> {
    Self::with_resources_scaled(
      width,
      height,
      background,
      FontContext::new(),
      ImageCache::new(),
      1.0,
    )
  }

  /// Creates a painter with explicit font and image resources
  pub fn with_resources(
    width: u32,
    height: u32,
    background: Rgba,
    font_ctx: FontContext,
    image_cache: ImageCache,
  ) -> Result<Self> {
    Self::with_resources_scaled(width, height, background, font_ctx, image_cache, 1.0)
  }

  /// Creates a painter with explicit font/image resources and a device scale.
  pub fn with_resources_scaled(
    width: u32,
    height: u32,
    background: Rgba,
    font_ctx: FontContext,
    image_cache: ImageCache,
    scale: f32,
  ) -> Result<Self> {
    let scale = if scale.is_finite() && scale > 0.0 {
      scale
    } else {
      1.0
    };
    let device_w = ((width as f32) * scale).round().max(1.0) as u32;
    let device_h = ((height as f32) * scale).round().max(1.0) as u32;
    let pixmap = new_pixmap(device_w, device_h).ok_or_else(|| RenderError::InvalidParameters {
      message: format!("Failed to create pixmap {}x{}", device_w, device_h),
    })?;

    Ok(Self {
      pixmap,
      scale,
      origin_offset_css: Point::ZERO,
      css_width: width as f32,
      css_height: height as f32,
      background,
      shaper: ShapingPipeline::new(),
      font_ctx,
      image_cache,
      media_provider: None,
      svg_id_defs: None,
      svg_id_defs_raw: None,
      text_shape_cache: Arc::new(Mutex::new(HashMap::new())),
      trace: TraceHandle::disabled(),
      scroll_state: ScrollState::default(),
      max_iframe_depth: crate::api::DEFAULT_MAX_IFRAME_DEPTH,
      gradient_cache: GradientLutCache::default(),
      gradient_stats: GradientStats::default(),
      diagnostics_enabled: paint_diagnostics_enabled(),
    })
  }

  /// Attach a trace handle used for Chrome trace export.
  fn with_trace(mut self, trace: TraceHandle) -> Self {
    self.trace = trace;
    self
  }

  /// Attach a scroll state used for translating scroll container contents during paint.
  fn with_scroll_state(mut self, scroll_state: ScrollState) -> Self {
    self.scroll_state = scroll_state;
    self
  }

  fn with_media_provider(
    mut self,
    media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  ) -> Self {
    self.media_provider = media_provider;
    self
  }

  fn with_max_iframe_depth(mut self, max_iframe_depth: usize) -> Self {
    self.max_iframe_depth = max_iframe_depth;
    self
  }

  #[inline]
  fn device_length(&self, value: f32) -> f32 {
    value * self.scale
  }

  #[inline]
  fn device_x(&self, x: f32) -> f32 {
    (x - self.origin_offset_css.x) * self.scale
  }

  #[inline]
  fn device_y(&self, y: f32) -> f32 {
    (y - self.origin_offset_css.y) * self.scale
  }

  #[inline]
  fn device_point(&self, point: Point) -> Point {
    Point::new(self.device_x(point.x), self.device_y(point.y))
  }

  #[inline]
  fn device_rect(&self, rect: Rect) -> Rect {
    Rect::from_xywh(
      self.device_x(rect.x()),
      self.device_y(rect.y()),
      self.device_length(rect.width()),
      self.device_length(rect.height()),
    )
  }

  #[inline]
  fn device_radii(&self, radii: BorderRadii) -> BorderRadii {
    BorderRadii {
      top_left: radii.top_left * self.scale,
      top_right: radii.top_right * self.scale,
      bottom_right: radii.bottom_right * self.scale,
      bottom_left: radii.bottom_left * self.scale,
    }
  }

  #[inline]
  fn device_transform(&self, transform: Option<Transform>) -> Option<Transform> {
    if let Some(mut t) = transform {
      t.tx = self.device_x(t.tx);
      t.ty = self.device_y(t.ty);
      Some(t)
    } else {
      None
    }
  }

  #[inline]
  fn record_gradient_usage(&mut self, pixels: u64, start: Instant) {
    if self.diagnostics_enabled {
      self.gradient_stats.record(pixels, start.elapsed());
    }
  }

  #[inline]
  fn record_background_layer(&mut self) {
    if self.diagnostics_enabled {
      with_paint_diagnostics(|diag| {
        diag.background_layers = diag.background_layers.saturating_add(1);
      });
    }
  }

  #[allow(dead_code)]
  #[inline]
  fn device_dimensions(&self, width: f32, height: f32) -> Option<(u32, u32)> {
    if width <= 0.0 || height <= 0.0 {
      return None;
    }
    let w = ((width * self.scale).round()).max(0.0) as u32;
    let h = ((height * self.scale).round()).max(0.0) as u32;
    Some((w, h))
  }

  /// Paints a fragment tree and returns the resulting pixmap
  pub fn paint(self, tree: &FragmentTree) -> Result<Pixmap> {
    self.paint_with_offset(tree, Point::ZERO)
  }

  /// Paints a fragment tree with an additional offset applied to all fragments.
  pub fn paint_with_offset(mut self, tree: &FragmentTree, offset: Point) -> Result<Pixmap> {
    let profiling = runtime::runtime_toggles().truthy("FASTR_PAINT_STATS");
    let diagnostics_enabled = paint_diagnostics_enabled();
    let mut stats = PaintStats::default();
    check_active(RenderStage::Paint).map_err(Error::Render)?;
    let trace = self.trace.clone();
    let _paint_span = trace.span("paint", "paint");
    self.svg_id_defs = tree.svg_id_defs.clone();
    self.svg_id_defs_raw = tree.svg_id_defs_raw.clone();

    if dump_counts_enabled() {
      let (total, text, replaced, lines, inline) = fragment_tree_counts(tree);
      eprintln!(
        "fragment counts total={} text={} lines={} inline={} replaced={}",
        total, text, lines, inline, replaced
      );
    }

    // Fill background
    let start = Instant::now();
    self.fill_background();
    if profiling {
      stats.background_ms = start.elapsed().as_secs_f64() * 1000.0;
    }

    // Build display list in stacking-context order then paint
    let mut items = Vec::new();
    let collect_start = Instant::now();
    let _display_list_span = trace.span("display_list_build", "paint");
    let root_paint = RootPaintOptions {
      use_root_background: tree.has_explicit_viewport(),
      extend_background_to_viewport: tree.has_explicit_viewport(),
    };
    let svg_filter_roots: Vec<&FragmentNode> = std::iter::once(&tree.root)
      .chain(tree.additional_fragments.iter())
      .collect();
    let mut svg_filter_resolver = SvgFilterResolver::new(
      tree.svg_filter_defs.clone(),
      svg_filter_roots.clone(),
      Some(&self.image_cache),
    );
    for root in svg_filter_roots {
      self
        .collect_stacking_context(
          root,
          offset,
          None,
          true,
          false,
          Point::ZERO,
          false,
          root_paint,
          &mut items,
          &mut svg_filter_resolver,
        )
        .map_err(Error::Render)?;
    }
    drop(_display_list_span);
    check_active(RenderStage::Paint).map_err(Error::Render)?;
    if profiling {
      stats.collect_ms = collect_start.elapsed().as_secs_f64() * 1000.0;
    }
    if diagnostics_enabled {
      let build_ms = collect_start.elapsed().as_secs_f64() * 1000.0;
      let count = items.len();
      with_paint_diagnostics(|diag| {
        diag.build_ms = build_ms;
        diag.command_count = count;
        diag.serial_ms += build_ms;
        diag.parallel_threads = diag.parallel_threads.max(1);
      });
    }
    if dump_stack_enabled() {
      let total_items = items.len();
      let mut stack_items = 0;
      let mut text_items = 0;
      fn nested_counts(cmds: &[DisplayCommand]) -> (usize, usize) {
        cmds.iter().fold((0, 0), |(total, text), cmd| match cmd {
          DisplayCommand::StackingContext { commands, .. } => {
            let (t, tx) = nested_counts(commands);
            (total + 1 + t, text + tx)
          }
          DisplayCommand::Text { .. } => (total + 1, text + 1),
          _ => (total + 1, text),
        })
      }
      for item in &items {
        match item {
          DisplayCommand::StackingContext { commands, .. } => {
            stack_items += 1;
            let (c, t) = nested_counts(commands);
            eprintln!(
              "stack list item: nested_commands={} nested_text={} bounds_not_logged_here",
              c, t
            );
          }
          DisplayCommand::Text { .. } => text_items += 1,
          _ => {}
        }
      }
      eprintln!(
        "stack list summary: total_items={} stack_items={} text_items={}",
        total_items, stack_items, text_items
      );
    }

    // Optional debug: dump a few text commands with positions/colors
    if let Some(limit) = runtime::runtime_toggles().usize("FASTR_DUMP_TEXT_ITEMS") {
      fn collect_text<'a>(cmds: &'a [DisplayCommand], out: &mut Vec<&'a DisplayCommand>) {
        for cmd in cmds {
          match cmd {
            DisplayCommand::Text { .. } => out.push(cmd),
            DisplayCommand::StackingContext { commands, .. } => collect_text(commands, out),
            _ => {}
          }
        }
      }
      let mut texts = Vec::new();
      collect_text(&items, &mut texts);
      eprintln!(
        "dumping first {} text commands ({} total)",
        limit.min(texts.len()),
        texts.len()
      );
      for (idx, cmd) in texts.iter().take(limit).enumerate() {
        if let DisplayCommand::Text {
          rect,
          baseline_offset,
          text,
          style,
          ..
        } = cmd
        {
          let (r, g, b, a) = (style.color.r, style.color.g, style.color.b, style.color.a);
          eprintln!(
                          "  [{idx}] text {:?} rect=({:.1},{:.1},{:.1},{:.1}) baseline_off={:.1} color=rgba({},{},{},{:.2})",
                          text.chars().take(60).collect::<String>(),
                          rect.x(),
                          rect.y(),
                          rect.width(),
                          rect.height(),
                          baseline_offset,
                          r,
                          g,
                          b,
                          a
                      );
        }
      }
    }

    // Optional debug: dump the first N display commands with their rects/types.
    if let Some(limit) = runtime::runtime_toggles().usize("FASTR_DUMP_COMMANDS") {
      fn collect_commands<'a>(
        cmds: &'a [DisplayCommand],
        out: &mut Vec<(&'a DisplayCommand, usize)>,
        depth: usize,
      ) {
        for cmd in cmds {
          out.push((cmd, depth));
          if let DisplayCommand::StackingContext { commands, .. } = cmd {
            collect_commands(commands, out, depth + 1);
          }
        }
      }

      let mut flat = Vec::new();
      collect_commands(&items, &mut flat, 0);
      eprintln!(
        "dumping first {} commands ({} total)",
        limit.min(flat.len()),
        flat.len()
      );

      for (idx, (cmd, depth)) in flat.iter().take(limit).enumerate() {
        match cmd {
          DisplayCommand::Background { rect, style, .. } => eprintln!(
            "  [{idx}] {:indent$}background ({:.1},{:.1},{:.1},{:.1}) color=rgba({},{},{},{:.2})",
            "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            style.background_color.r,
            style.background_color.g,
            style.background_color.b,
            style.background_color.a,
            indent = depth * 2
          ),
          DisplayCommand::Border { rect, .. } => eprintln!(
            "  [{idx}] {:indent$}border ({:.1},{:.1},{:.1},{:.1})",
            "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            indent = depth * 2
          ),
          DisplayCommand::Outline { rect, .. } => eprintln!(
            "  [{idx}] {:indent$}outline ({:.1},{:.1},{:.1},{:.1})",
            "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            indent = depth * 2
          ),
          DisplayCommand::Text { rect, .. } => eprintln!(
            "  [{idx}] {:indent$}text ({:.1},{:.1},{:.1},{:.1})",
            "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            indent = depth * 2
          ),
          DisplayCommand::Replaced { rect, .. } => eprintln!(
            "  [{idx}] {:indent$}replaced ({:.1},{:.1},{:.1},{:.1})",
            "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            indent = depth * 2
          ),
          DisplayCommand::StackingContext { rect, .. } => eprintln!(
            "  [{idx}] {:indent$}stack ({:.1},{:.1},{:.1},{:.1})",
            "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            indent = depth * 2
          ),
        }
      }
    }

    let command_len = items.len();
    let raster_start = diagnostics_enabled.then(Instant::now);
    let _raster_span = trace.span("rasterize", "paint");
    for item in items {
      if let Err(RenderError::Timeout { stage, elapsed }) = check_active(RenderStage::Paint) {
        return Err(Error::Render(RenderError::Timeout { stage, elapsed }));
      }
      if profiling {
        let exec_start = Instant::now();
        self.execute_command(item.clone())?;
        stats.record(&item, exec_start.elapsed());
      } else {
        self.execute_command(item)?;
      }
    }
    drop(_raster_span);
    if let (true, Some(start)) = (diagnostics_enabled, raster_start) {
      let raster_ms = start.elapsed().as_secs_f64() * 1000.0;
      with_paint_diagnostics(|diag| {
        diag.raster_ms = raster_ms;
        diag.serial_ms += raster_ms;
        diag.parallel_threads = diag.parallel_threads.max(1);
        if diag.command_count == 0 {
          diag.command_count = command_len;
        }
        diag.gradient_ms = self.gradient_stats.millis();
        diag.gradient_pixels = self.gradient_stats.pixels;
      });
    } else if diagnostics_enabled {
      with_paint_diagnostics(|diag| {
        diag.gradient_ms = self.gradient_stats.millis();
        diag.gradient_pixels = self.gradient_stats.pixels;
        diag.parallel_threads = diag.parallel_threads.max(1);
        if diag.command_count == 0 {
          diag.command_count = command_len;
        }
      });
    }

    // The per-command check above runs before executing each display item. Ensure we also check
    // once at the end so we don't miss a deadline that expires during the final command.
    check_active(RenderStage::Paint).map_err(Error::Render)?;

    if profiling {
      eprintln!("paint_stats enabled");
      stats.log();
    }

    Ok(self.pixmap)
  }

  /// Fills the canvas with the background color
  fn fill_background(&mut self) {
    let color = tiny_skia::Color::from_rgba8(
      self.background.r,
      self.background.g,
      self.background.b,
      self.background.alpha_u8(),
    );
    self.pixmap.fill(color);
  }

  fn stacking_clip_for_style(
    &self,
    style: &ComputedStyle,
    abs_bounds: Rect,
  ) -> Option<StackingClip> {
    // Honor overflow clipping: when overflow is hidden/scroll/clip, restrict painting. This
    // prevents offscreen children from flooding the viewport when their layout positions explode.
    let clip_x = matches!(
      style.overflow_x,
      Overflow::Hidden | Overflow::Scroll | Overflow::Auto | Overflow::Clip
    ) || style.containment.paint;
    let clip_y = matches!(
      style.overflow_y,
      Overflow::Hidden | Overflow::Scroll | Overflow::Auto | Overflow::Clip
    ) || style.containment.paint;

    let overflow_clip = if clip_x || clip_y {
      let rects = background_rects(
        abs_bounds.x(),
        abs_bounds.y(),
        abs_bounds.width(),
        abs_bounds.height(),
        style,
        Some((self.css_width, self.css_height)),
      );
      let viewport = (self.css_width, self.css_height);
      let percentage_base = rects.border.width().max(0.0);
      let raw_margin = &style.overflow_clip_margin.margin;
      let resolved_margin = resolve_length_for_paint(
        raw_margin,
        style.font_size,
        style.root_font_size,
        percentage_base,
        viewport,
      );
      let resolved_margin = if resolved_margin.is_finite() {
        resolved_margin.max(0.0)
      } else {
        0.0
      };

      // `contain: paint` applies an additional padding-box clip in the display-list backend.
      // The legacy backend represents overflow+containment clipping with a single rounded-rect
      // mask; prefer the containment padding box so `overflow-clip-margin` never expands the
      // effective clip beyond the paint containment boundary.
      let margin_x = if !style.containment.paint && matches!(style.overflow_x, Overflow::Clip) {
        resolved_margin
      } else {
        0.0
      };
      let margin_y = if !style.containment.paint && matches!(style.overflow_y, Overflow::Clip) {
        resolved_margin
      } else {
        0.0
      };

      let clip_box_x = if !style.containment.paint && matches!(style.overflow_x, Overflow::Clip) {
        style.overflow_clip_margin.visual_box
      } else {
        crate::style::types::VisualBox::PaddingBox
      };
      let clip_box_y = if !style.containment.paint && matches!(style.overflow_y, Overflow::Clip) {
        style.overflow_clip_margin.visual_box
      } else {
        crate::style::types::VisualBox::PaddingBox
      };

      let rect_for_visual_box = |vb: crate::style::types::VisualBox| -> Rect {
        match vb {
          crate::style::types::VisualBox::BorderBox => rects.border,
          crate::style::types::VisualBox::PaddingBox => rects.padding,
          crate::style::types::VisualBox::ContentBox => rects.content,
        }
      };

      let mut clip_rect = rects.padding;
      if clip_x {
        let base = rect_for_visual_box(clip_box_x);
        let min_x = base.min_x() - margin_x;
        let max_x = base.max_x() + margin_x;
        clip_rect.origin.x = min_x;
        clip_rect.size.width = (max_x - min_x).max(0.0);
      }
      if clip_y {
        let base = rect_for_visual_box(clip_box_y);
        let min_y = base.min_y() - margin_y;
        let max_y = base.max_y() + margin_y;
        clip_rect.origin.y = min_y;
        clip_rect.size.height = (max_y - min_y).max(0.0);
      }

      if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
        None
      } else {
        let clip_radii = if clip_x && clip_y {
          let base = resolve_border_radii(Some(style), rects.border);
          if base.is_zero() {
            BorderRadii::ZERO
          } else {
            let border = rects.border;
            let padding = rects.padding;
            let content = rects.content;

            let padding_inset_left = (padding.min_x() - border.min_x()).max(0.0);
            let padding_inset_right = (border.max_x() - padding.max_x()).max(0.0);
            let padding_inset_top = (padding.min_y() - border.min_y()).max(0.0);
            let padding_inset_bottom = (border.max_y() - padding.max_y()).max(0.0);

            let content_inset_left = (content.min_x() - border.min_x()).max(0.0);
            let content_inset_right = (border.max_x() - content.max_x()).max(0.0);
            let content_inset_top = (content.min_y() - border.min_y()).max(0.0);
            let content_inset_bottom = (border.max_y() - content.max_y()).max(0.0);

            let inset_x = |vb: crate::style::types::VisualBox| -> (f32, f32) {
              match vb {
                crate::style::types::VisualBox::BorderBox => (0.0, 0.0),
                crate::style::types::VisualBox::PaddingBox => {
                  (padding_inset_left, padding_inset_right)
                }
                crate::style::types::VisualBox::ContentBox => {
                  (content_inset_left, content_inset_right)
                }
              }
            };
            let inset_y = |vb: crate::style::types::VisualBox| -> (f32, f32) {
              match vb {
                crate::style::types::VisualBox::BorderBox => (0.0, 0.0),
                crate::style::types::VisualBox::PaddingBox => {
                  (padding_inset_top, padding_inset_bottom)
                }
                crate::style::types::VisualBox::ContentBox => {
                  (content_inset_top, content_inset_bottom)
                }
              }
            };

            let (inset_left, inset_right) = inset_x(clip_box_x);
            let (inset_top, inset_bottom) = inset_y(clip_box_y);

            let offset_left = -inset_left + margin_x;
            let offset_right = -inset_right + margin_x;
            let offset_top = -inset_top + margin_y;
            let offset_bottom = -inset_bottom + margin_y;

            BorderRadii {
              top_left: crate::paint::display_list::BorderRadius {
                x: (base.top_left.x + offset_left).max(0.0),
                y: (base.top_left.y + offset_top).max(0.0),
              },
              top_right: crate::paint::display_list::BorderRadius {
                x: (base.top_right.x + offset_right).max(0.0),
                y: (base.top_right.y + offset_top).max(0.0),
              },
              bottom_right: crate::paint::display_list::BorderRadius {
                x: (base.bottom_right.x + offset_right).max(0.0),
                y: (base.bottom_right.y + offset_bottom).max(0.0),
              },
              bottom_left: crate::paint::display_list::BorderRadius {
                x: (base.bottom_left.x + offset_left).max(0.0),
                y: (base.bottom_left.y + offset_bottom).max(0.0),
              },
            }
            .clamped(clip_rect.width(), clip_rect.height())
          }
        } else {
          BorderRadii::ZERO
        };

        Some(StackingClip {
          rect: clip_rect,
          radii: clip_radii,
          clip_x,
          clip_y,
          clip_root: false,
        })
      }
    } else {
      None
    };

    // CSS 2.1 `clip` applies only to absolutely positioned elements (position: absolute|fixed).
    let clip_property = matches!(style.position, Position::Absolute | Position::Fixed)
      .then(|| style.clip.as_ref())
      .flatten()
      .and_then(|clip| {
        let font_size = style.font_size;
        let root_font = style.root_font_size;
        let viewport = (self.css_width, self.css_height);
        let width = abs_bounds.width();
        let height = abs_bounds.height();

        let left = match &clip.left {
          ClipComponent::Auto => abs_bounds.x(),
          ClipComponent::Length(len) => {
            abs_bounds.x() + resolve_length_for_paint(len, font_size, root_font, width, viewport)
          }
        };
        let top = match &clip.top {
          ClipComponent::Auto => abs_bounds.y(),
          ClipComponent::Length(len) => {
            abs_bounds.y() + resolve_length_for_paint(len, font_size, root_font, height, viewport)
          }
        };
        let right = match &clip.right {
          ClipComponent::Auto => abs_bounds.x() + width,
          ClipComponent::Length(len) => {
            abs_bounds.x() + resolve_length_for_paint(len, font_size, root_font, width, viewport)
          }
        };
        let bottom = match &clip.bottom {
          ClipComponent::Auto => abs_bounds.y() + height,
          ClipComponent::Length(len) => {
            abs_bounds.y() + resolve_length_for_paint(len, font_size, root_font, height, viewport)
          }
        };

        // CSS 2.1 `clip: rect(...)` is allowed to collapse to an empty region (e.g.
        // `rect(0,0,0,0)` as used by sr-only patterns). That should clip all painting for the
        // element, not behave like `clip: auto`.
        let clip_w = (right - left).max(0.0);
        let clip_h = (bottom - top).max(0.0);
        if !left.is_finite() || !top.is_finite() || !clip_w.is_finite() || !clip_h.is_finite() {
          return None;
        }
        Some(Rect::from_xywh(left, top, clip_w, clip_h))
      });

    match (overflow_clip, clip_property) {
      (Some(overflow), Some(prop_rect)) => {
        let left = overflow.rect.min_x().max(prop_rect.min_x());
        let top = overflow.rect.min_y().max(prop_rect.min_y());
        let right = overflow.rect.max_x().min(prop_rect.max_x());
        let bottom = overflow.rect.max_y().min(prop_rect.max_y());
        Some(StackingClip {
          rect: Rect::from_xywh(left, top, (right - left).max(0.0), (bottom - top).max(0.0)),
          radii: BorderRadii::ZERO,
          clip_x: true,
          clip_y: true,
          clip_root: true,
        })
      }
      (Some(overflow), None) => Some(overflow),
      (None, Some(prop_rect)) => Some(StackingClip {
        rect: prop_rect,
        radii: BorderRadii::ZERO,
        clip_x: true,
        clip_y: true,
        clip_root: true,
      }),
      (None, None) => None,
    }
  }

  /// Collect display commands respecting stacking-context ordering.
  ///
  /// This follows the simplified CSS painting order for a stacking context:
  /// element background/border → negative z-index stacking contexts →
  /// in-flow/non-positioned content → z-index:auto/0 stacking contexts →
  /// positive z-index stacking contexts.
  fn collect_stacking_context(
    &self,
    fragment: &FragmentNode,
    offset: Point,
    parent_style: Option<&ComputedStyle>,
    is_root_context: bool,
    skip_viewport_scroll_cancel: bool,
    applied_element_scroll: Point,
    has_fixed_cb_ancestor: bool,
    root_paint: RootPaintOptions,
    items: &mut Vec<DisplayCommand>,
    svg_filters: &mut SvgFilterResolver,
  ) -> RenderResult<()> {
    #[derive(Clone, Copy)]
    enum Step {
      Child(usize),
      EnqueueContent,
    }

    struct Frame<'a> {
      fragment: &'a FragmentNode,
      is_root_context: bool,
      root_paint: RootPaintOptions,
      root_background: Option<bool>,
      style_ref: Option<&'a ComputedStyle>,
      abs_bounds: Rect,
      viewport: (f32, f32),
      establishes_context: bool,
      child_offset: Point,
      skip_viewport_scroll_cancel_for_children: bool,
      applied_element_scroll_for_children: Point,
      has_fixed_cb_ancestor_for_children: bool,
      local_commands: Vec<DisplayCommand>,
      steps: Vec<Step>,
      step_index: usize,
    }

    fn init_frame<'a>(
      painter: &Painter,
      fragment: &'a FragmentNode,
      offset: Point,
      parent_style: Option<&'a ComputedStyle>,
      is_root_context: bool,
      skip_viewport_scroll_cancel: bool,
      applied_element_scroll: Point,
      has_fixed_cb_ancestor: bool,
      root_paint: RootPaintOptions,
    ) -> RenderResult<Option<Frame<'a>>> {
      check_active(RenderStage::Paint)?;
      let debug_fragments = dump_fragments_enabled();
      let is_root_fragment = is_root_context && parent_style.is_none();
      let root_background = if is_root_fragment && root_paint.use_root_background {
        Some(root_paint.extend_background_to_viewport)
      } else {
        None
      };
      if let Some(style) = fragment.style.as_deref() {
        if !matches!(
          style.visibility,
          crate::style::computed::Visibility::Visible
        ) {
          return Ok(None);
        }
      }

      let style_ref = fragment.style.as_deref();
      let establishes_fixed_cb =
        style_ref.is_some_and(|style| style.establishes_fixed_containing_block());
      let is_viewport_fixed = style_ref
        .is_some_and(|style| matches!(style.position, Position::Fixed))
        && !has_fixed_cb_ancestor;
      let (offset, applied_element_scroll) = if is_viewport_fixed {
        (
          offset.translate(Point::new(
            -applied_element_scroll.x,
            -applied_element_scroll.y,
          )),
          Point::ZERO,
        )
      } else {
        (offset, applied_element_scroll)
      };
      let has_fixed_cb_ancestor_for_children = has_fixed_cb_ancestor || establishes_fixed_cb;
      let needs_viewport_scroll_cancel = style_ref
        .is_some_and(|style| matches!(style.position, Position::Fixed))
        && !skip_viewport_scroll_cancel;
      let skip_viewport_scroll_cancel_for_children =
        skip_viewport_scroll_cancel || establishes_fixed_cb || needs_viewport_scroll_cancel;
      let viewport_scroll = if painter.scroll_state.viewport.x.is_finite()
        && painter.scroll_state.viewport.y.is_finite()
      {
        painter.scroll_state.viewport
      } else {
        Point::ZERO
      };
      let offset = if needs_viewport_scroll_cancel {
        Point::new(offset.x + viewport_scroll.x, offset.y + viewport_scroll.y)
      } else {
        offset
      };

      let abs_bounds = Rect::from_xywh(
        fragment.bounds.x() + offset.x,
        fragment.bounds.y() + offset.y,
        fragment.bounds.width(),
        fragment.bounds.height(),
      );
      let viewport = (painter.css_width, painter.css_height);

      if let Some(style) = fragment.style.as_deref() {
        if matches!(style.backface_visibility, BackfaceVisibility::Hidden)
          && (style.has_transform() || style.perspective.is_some() || style.has_motion_path())
        {
          if let Some(transform) = resolve_transform3d(style, abs_bounds, Some(viewport)) {
            if backface_is_hidden(&transform) {
              return Ok(None);
            }
          }
        }
      }

      if debug_fragments {
        eprintln!(
          "fragment {:?} bounds=({}, {}, {}, {}) children={} establishes={} display={:?} overflow=({:?},{:?}) overflow_clip_margin={:?}",
          describe_content(&fragment.content),
          abs_bounds.x(),
          abs_bounds.y(),
          abs_bounds.width(),
          abs_bounds.height(),
          fragment.children.len(),
          fragment
            .style
            .as_deref()
            .map(|s| creates_stacking_context(s, parent_style, is_root_context))
            .unwrap_or(is_root_context),
          fragment.style.as_deref().map(|s| s.display),
          fragment.style.as_deref().map(|s| s.overflow_x),
          fragment.style.as_deref().map(|s| s.overflow_y),
          fragment.style.as_deref().map(|s| s.overflow_clip_margin),
        );
      }

      let forced_z_index = fragment.stacking_context.forced_z_index();
      let establishes_context = forced_z_index.is_some()
        || style_ref
          .map(|s| creates_stacking_context(s, parent_style, is_root_context))
          .unwrap_or(is_root_context);
      let element_scroll = painter.element_scroll_offset(fragment);
      let scroll_delta = Point::new(-element_scroll.x, -element_scroll.y);
      let child_offset = Point::new(
        abs_bounds.x() + scroll_delta.x,
        abs_bounds.y() + scroll_delta.y,
      );
      let applied_element_scroll_for_children = applied_element_scroll.translate(scroll_delta);

      // Collect commands for this subtree locally so we can wrap the context (opacity, etc.)
      let mut local_commands = Vec::new();

      painter.enqueue_background_and_borders(
        fragment,
        abs_bounds,
        scroll_delta,
        root_background,
        &mut local_commands,
      );

      if !establishes_context {
        painter.enqueue_content(fragment, abs_bounds, &mut local_commands);
      }

      // CSS2.1 Appendix E requires "tree order" (DOM/box-tree order), not fragment emission order.
      // Layout can emit out-of-flow positioned fragments (e.g. `position: absolute`) after in-flow
      // content, reordering siblings in the fragment tree. Prefer the originating `box_id` when
      // available so layer-6 merging (positioned auto/0 + z-index:0 stacking contexts) is correct.
      let tree_order = |child: &FragmentNode, fallback: usize| child.box_id().unwrap_or(fallback);

      let mut negative_contexts: Vec<(i32, usize, usize)> = Vec::new(); // (z, order, idx)
      let mut zero_contexts: Vec<(usize, usize)> = Vec::new(); // (order, idx)
      let mut positive_contexts: Vec<(i32, usize, usize)> = Vec::new(); // (z, order, idx)
      // In-flow, non-inline-level descendants (CSS2 stacking level 3).
      let mut blocks: Vec<usize> = Vec::new();
      // Non-positioned floats (CSS2 stacking level 4).
      let mut floats: Vec<usize> = Vec::new();
      // Paint in-flow inline-level content after blocks/floats (CSS2 stacking level 5)
      let mut inlines: Vec<usize> = Vec::new();
      // Positioned elements with auto/0 z-index but not establishing a stacking context (CSS2 level 6)
      let mut positioned_auto: Vec<(usize, usize)> = Vec::new(); // (order, idx)

      for (idx, child) in fragment.children.iter().enumerate() {
        let order = tree_order(child, idx);
        if let Some(z) = child.stacking_context.forced_z_index() {
          match z.cmp(&0) {
            std::cmp::Ordering::Less => negative_contexts.push((z, order, idx)),
            std::cmp::Ordering::Equal => zero_contexts.push((order, idx)),
            std::cmp::Ordering::Greater => positive_contexts.push((z, order, idx)),
          }
          continue;
        }

        if let Some(style) = child.style.as_deref() {
          if creates_stacking_context(style, style_ref, false) {
            let z = style.z_index.unwrap_or(0);
            match z.cmp(&0) {
              std::cmp::Ordering::Less => negative_contexts.push((z, order, idx)),
              std::cmp::Ordering::Equal => zero_contexts.push((order, idx)),
              std::cmp::Ordering::Greater => positive_contexts.push((z, order, idx)),
            }
            continue;
          }
          if is_positioned(style) {
            positioned_auto.push((order, idx));
          } else if style.float.is_floating() {
            floats.push(idx);
          } else if is_inline_level(style, child) {
            inlines.push(idx);
          } else {
            blocks.push(idx);
          }
        } else if matches!(
          child.content,
          FragmentContent::Inline { .. }
            | FragmentContent::Text { .. }
            | FragmentContent::Line { .. }
        ) {
          inlines.push(idx);
        } else {
          blocks.push(idx);
        }
      }

      negative_contexts.sort_by(|(z1, o1, i1), (z2, o2, i2)| {
        z1.cmp(z2).then_with(|| o1.cmp(o2)).then_with(|| i1.cmp(i2))
      });
      positive_contexts.sort_by(|(z1, o1, i1), (z2, o2, i2)| {
        z1.cmp(z2).then_with(|| o1.cmp(o2)).then_with(|| i1.cmp(i2))
      });

      let mut steps =
        Vec::with_capacity(fragment.children.len() + usize::from(establishes_context));

      for (_, _, idx) in negative_contexts {
        steps.push(Step::Child(idx));
      }
      if establishes_context {
        steps.push(Step::EnqueueContent);
      }
      for idx in blocks {
        steps.push(Step::Child(idx));
      }
      for idx in floats {
        steps.push(Step::Child(idx));
      }
      for idx in inlines {
        steps.push(Step::Child(idx));
      }

      // CSS2.1 Appendix E layer 6 requires positioned `z-index:auto/0` descendants and `z-index:0`
      // child stacking contexts to be painted *together* in tree order.
      let mut layer6: Vec<(usize, u8, usize)> =
        Vec::with_capacity(positioned_auto.len() + zero_contexts.len());
      for (order, idx) in positioned_auto {
        layer6.push((order, 0, idx));
      }
      for (order, idx) in &zero_contexts {
        layer6.push((*order, 1, *idx));
      }
      layer6.sort_by(|(o1, k1, i1), (o2, k2, i2)| {
        o1.cmp(o2).then_with(|| k1.cmp(k2)).then_with(|| i1.cmp(i2))
      });
      for (_, _, idx) in layer6 {
        steps.push(Step::Child(idx));
      }

      for (_, _, idx) in positive_contexts {
        steps.push(Step::Child(idx));
      }

      Ok(Some(Frame {
        fragment,
        is_root_context,
        root_paint,
        root_background,
        style_ref,
        abs_bounds,
        viewport,
        establishes_context,
        child_offset,
        skip_viewport_scroll_cancel_for_children,
        applied_element_scroll_for_children,
        has_fixed_cb_ancestor_for_children,
        local_commands,
        steps,
        step_index: 0,
      }))
    }

    let mut stack: Vec<Frame> = Vec::new();
    if let Some(frame) = init_frame(
      self,
      fragment,
      offset,
      parent_style,
      is_root_context,
      skip_viewport_scroll_cancel,
      applied_element_scroll,
      has_fixed_cb_ancestor,
      root_paint,
    )? {
      stack.push(frame);
    } else {
      return Ok(());
    }

    while let Some(mut frame) = stack.pop() {
      if let Some(step) = frame.steps.get(frame.step_index).copied() {
        frame.step_index += 1;
        match step {
          Step::EnqueueContent => {
            self.enqueue_content(frame.fragment, frame.abs_bounds, &mut frame.local_commands);
            stack.push(frame);
          }
          Step::Child(idx) => {
            let child = &frame.fragment.children[idx];
            let child_frame = init_frame(
              self,
              child,
              frame.child_offset,
              frame.style_ref,
              false,
              frame.skip_viewport_scroll_cancel_for_children,
              frame.applied_element_scroll_for_children,
              frame.has_fixed_cb_ancestor_for_children,
              frame.root_paint,
            )?;
            stack.push(frame);
            if let Some(child_frame) = child_frame {
              stack.push(child_frame);
            }
          }
        }
        continue;
      }

      if !frame.establishes_context {
        let clip = frame
          .style_ref
          .and_then(|style| self.stacking_clip_for_style(style, frame.abs_bounds));
        if let Some(clip) = clip {
          let cmd = DisplayCommand::StackingContext {
            rect: frame.abs_bounds,
            opacity: 1.0,
            transform: None,
            transform_3d: None,
            blend_mode: MixBlendMode::Normal,
            isolated: false,
            mask: None,
            mask_border: None,
            filters: Vec::new(),
            backdrop_filters: Vec::new(),
            // Overflow clips (and CSS `clip`) should not impose an additional border-radius clip when
            // compositing the layer; only the explicit `clip` should apply.
            radii: BorderRadii::ZERO,
            clip: Some(clip),
            has_clip_path: false,
            clip_path: None,
            root_style: if frame.root_background.is_some() {
              Self::root_background_style(frame.fragment)
            } else {
              frame.fragment.style.clone()
            },
            commands: frame.local_commands,
          };
          if let Some(parent) = stack.last_mut() {
            parent.local_commands.push(cmd);
          } else {
            items.push(cmd);
          }
        } else if let Some(parent) = stack.last_mut() {
          parent.local_commands.append(&mut frame.local_commands);
        } else {
          items.append(&mut frame.local_commands);
        }
        continue;
      }

      // Wrap the stacking context if it applies an effect (opacity/transform); otherwise flatten
      let opacity = frame
        .style_ref
        .map(|s| s.opacity)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
      let transform_3d = build_transform_3d(frame.style_ref, frame.abs_bounds, Some(frame.viewport));
      let transform = transform_3d
        .as_ref()
        .and_then(|t| t.to_2d())
        .map(transform2d_to_skia);
      let blend_mode = frame
        .style_ref
        .map(|s| s.mix_blend_mode)
        .unwrap_or(MixBlendMode::Normal);
      let has_blend_mode_children = !frame.is_root_context
        && frame.local_commands.iter().any(|cmd| match cmd {
          DisplayCommand::StackingContext { blend_mode, .. } => {
            !matches!(blend_mode, MixBlendMode::Normal)
          }
          _ => false,
        });
      let will_change_needs_backdrop_root_boundary = matches!(blend_mode, MixBlendMode::Normal)
        && frame
          .style_ref
          .is_some_and(|s| s.will_change.establishes_backdrop_root())
        && commands_have_backdrop_sensitive_descendants(&frame.local_commands);
      let isolated = frame
        .style_ref
        .map(|s| matches!(s.isolation, crate::style::types::Isolation::Isolate))
        .unwrap_or(false)
        // `will-change` hints for Backdrop Root triggers (Filter Effects 2) must behave as if the
        // trigger were present, scoping descendant backdrop-filter sampling. In the legacy paint
        // path we reuse the "isolated group" layer allocation to create that boundary, but only
        // when the subtree contains effects that actually sample the backdrop.
        || will_change_needs_backdrop_root_boundary
        || has_blend_mode_children;
      let filters = frame
        .style_ref
        .map(|s| resolve_filters(&s.filter, s, frame.viewport, &self.font_ctx, svg_filters))
        .unwrap_or_default();
      let has_filters = !filters.is_empty();
      let backdrop_filters = frame
        .style_ref
        .map(|s| resolve_filters(&s.backdrop_filter, s, frame.viewport, &self.font_ctx, svg_filters))
        .unwrap_or_default();
      let has_backdrop = !backdrop_filters.is_empty();
      let clip: Option<StackingClip> =
        frame
          .style_ref
          .and_then(|style| self.stacking_clip_for_style(style, frame.abs_bounds));
      let clip_path = match frame.style_ref {
        Some(style) => match &style.clip_path {
          crate::style::types::ClipPath::Url(src, reference_override) => {
            let trimmed = trim_ascii_whitespace_html_css(src);
            let reference =
              reference_override.unwrap_or(crate::style::types::ReferenceBox::BorderBox);
            let reference_rect = crate::paint::clip_path::resolve_clip_path_reference_box_rect(
              style,
              frame.abs_bounds,
              (self.css_width, self.css_height),
              &self.font_ctx,
              reference,
            );

            if let Some(id) = trimmed.strip_prefix('#').filter(|id| !id.is_empty()) {
              Some(StackingClipPath::SvgFragment {
                id: id.to_string(),
                reference_rect,
              })
            } else if let Some((doc_url, id)) = trimmed.rsplit_once('#') {
              if doc_url.is_empty() || id.is_empty() {
                None
              } else {
                Some(StackingClipPath::SvgExternal {
                  doc_url: doc_url.to_string(),
                  id: id.to_string(),
                  reference_rect,
                })
              }
            } else {
              None
            }
          }
          _ => resolve_clip_path(
            style,
            frame.abs_bounds,
            (self.css_width, self.css_height),
            &self.font_ctx,
          )?
          .map(StackingClipPath::Shape),
        },
        None => None,
      };
      let style_has_clip_path = frame.style_ref.is_some_and(|style| {
        !matches!(style.clip_path, crate::style::types::ClipPath::None)
      });
      let mask = frame
        .fragment
        .style
        .clone()
        .filter(|s| s.mask_layers.iter().any(|layer| layer.image.is_some()));
      let mask_border = frame
        .fragment
        .style
        .clone()
        .filter(|s| s.mask_border.is_active());
      if opacity < 1.0
        || transform.is_some()
        || transform_3d.is_some()
        || !matches!(blend_mode, MixBlendMode::Normal)
        || isolated
        || has_filters
        || has_backdrop
        || clip.is_some()
        || style_has_clip_path
        || mask.is_some()
        || mask_border.is_some()
      {
        let radii = resolve_border_radii(frame.style_ref, frame.abs_bounds);
        let cmd = DisplayCommand::StackingContext {
          rect: frame.abs_bounds,
          opacity,
          transform,
          transform_3d,
          blend_mode,
          isolated,
          mask,
          mask_border,
          filters,
          backdrop_filters,
          radii,
          clip,
          has_clip_path: style_has_clip_path,
          clip_path,
          root_style: if frame.root_background.is_some() {
            Self::root_background_style(frame.fragment)
          } else {
            frame.fragment.style.clone()
          },
          commands: frame.local_commands,
        };
        if let Some(parent) = stack.last_mut() {
          parent.local_commands.push(cmd);
        } else {
          items.push(cmd);
        }
      } else if let Some(parent) = stack.last_mut() {
        parent.local_commands.append(&mut frame.local_commands);
      } else {
        items.append(&mut frame.local_commands);
      }
    }

    Ok(())
  }

  fn fieldset_legend_border_gap(
    &self,
    fragment: &FragmentNode,
    rect: Rect,
    style: &ComputedStyle,
  ) -> Option<BorderGap> {
    let legend = fragment.children.iter().find(|child| {
      child
        .style
        .as_deref()
        .is_some_and(|s| s.shrink_to_fit_inline_size)
    })?;

    let horizontal = crate::style::block_axis_is_horizontal(style.writing_mode);
    let positive = crate::style::block_axis_positive(style.writing_mode);
    let edge = if horizontal {
      if positive {
        PhysicalSide::Left
      } else {
        PhysicalSide::Right
      }
    } else if positive {
      PhysicalSide::Top
    } else {
      PhysicalSide::Bottom
    };

    let pad = 1.0f32;
    let legend_rect = Rect::from_xywh(
      rect.x() + legend.bounds.x(),
      rect.y() + legend.bounds.y(),
      legend.bounds.width(),
      legend.bounds.height(),
    );
    let (start, end) = match edge {
      PhysicalSide::Top | PhysicalSide::Bottom => {
        let start = (legend_rect.x() - pad).max(rect.x());
        let end = (legend_rect.x() + legend_rect.width() + pad).min(rect.x() + rect.width());
        (start, end)
      }
      PhysicalSide::Left | PhysicalSide::Right => {
        let start = (legend_rect.y() - pad).max(rect.y());
        let end = (legend_rect.y() + legend_rect.height() + pad).min(rect.y() + rect.height());
        (start, end)
      }
    };

    (end > start && start.is_finite() && end.is_finite()).then_some(BorderGap { edge, start, end })
  }

  /// Enqueue background and border commands for a fragment (no children).
  fn enqueue_background_and_borders(
    &self,
    fragment: &FragmentNode,
    abs_bounds: Rect,
    scroll_delta: Point,
    root_background: Option<bool>,
    items: &mut Vec<DisplayCommand>,
  ) {
    let style = if root_background.is_some() {
      Self::root_background_style(fragment)
    } else {
      fragment.style.clone()
    };
    let Some(style) = style else { return };

    let has_background = Self::has_paintable_background(&style);
    if has_background {
      let background_rect = if matches!(root_background, Some(true)) {
        let width = self.css_width.max(abs_bounds.width());
        let height = self.css_height.max(abs_bounds.height());
        Rect::from_xywh(0.0, 0.0, width, height)
      } else {
        abs_bounds
      };
      let wants_text_clip = style
        .background_layers
        .iter()
        .any(|layer| layer.clip == crate::style::types::BackgroundBox::Text);
      let text_clip = if wants_text_clip {
        let mut clip_commands = Vec::new();
        let offset = Point::new(
          abs_bounds.x() - fragment.bounds.x(),
          abs_bounds.y() - fragment.bounds.y(),
        );
        self.collect_text_commands_for_clip(fragment, offset, &mut clip_commands);
        Some(Arc::from(clip_commands.into_boxed_slice()))
      } else {
        None
      };
      items.push(DisplayCommand::Background {
        rect: background_rect,
        style: style.clone(),
        text_clip,
        scroll_delta,
      });
    }

    let viewport = (self.css_width, self.css_height);
    let base = abs_bounds.width().max(0.0);
    let has_border = resolve_length_for_paint(
      &style.used_border_top_width(),
      style.font_size,
      style.root_font_size,
      base,
      viewport,
    )
    .max(0.0)
      > 0.0
      || resolve_length_for_paint(
        &style.used_border_right_width(),
        style.font_size,
        style.root_font_size,
        base,
        viewport,
      )
      .max(0.0)
        > 0.0
      || resolve_length_for_paint(
        &style.used_border_bottom_width(),
        style.font_size,
        style.root_font_size,
        base,
        viewport,
      )
      .max(0.0)
        > 0.0
      || resolve_length_for_paint(
        &style.used_border_left_width(),
        style.font_size,
        style.root_font_size,
        base,
        viewport,
      )
      .max(0.0)
        > 0.0;
    if has_border {
      let gap = self.fieldset_legend_border_gap(fragment, abs_bounds, style.as_ref());
      items.push(DisplayCommand::Border {
        rect: abs_bounds,
        style: style.clone(),
        gap,
      });
    }

    if let Some(outline_rect) = Self::outline_bounds(abs_bounds, &style, viewport) {
      items.push(DisplayCommand::Outline {
        rect: outline_rect,
        style,
      });
    }
  }

  fn outline_bounds(abs_bounds: Rect, style: &ComputedStyle, viewport: (f32, f32)) -> Option<Rect> {
    let outline_style = style.outline_style.to_border_style();
    let width = resolve_length_for_paint(
      &style.outline_width,
      style.font_size,
      style.root_font_size,
      abs_bounds.width(),
      viewport,
    )
    .max(0.0);
    if width <= 0.0 || matches!(outline_style, CssBorderStyle::None | CssBorderStyle::Hidden) {
      return None;
    }
    let expand = resolve_length_for_paint(
      &style.outline_offset,
      style.font_size,
      style.root_font_size,
      abs_bounds.width(),
      viewport,
    ) + width;
    Some(Rect::from_xywh(
      abs_bounds.x() - expand,
      abs_bounds.y() - expand,
      abs_bounds.width() + 2.0 * expand,
      abs_bounds.height() + 2.0 * expand,
    ))
  }

  fn has_paintable_background(style: &ComputedStyle) -> bool {
    style.background_color.alpha_u8() > 0
      || style.background_layers.iter().any(|l| l.image.is_some())
  }

  fn root_background_style(fragment: &FragmentNode) -> Option<Arc<ComputedStyle>> {
    // Some test helpers construct a synthetic viewport fragment with a single real root child. In
    // renderer-produced trees the root fragment is the root element itself (which may still have a
    // single child), so only treat a single-child root as a wrapper when it has no originating box.
    let mut html = if fragment.box_id().is_none() && fragment.children.len() == 1 {
      fragment.children.first().unwrap_or(fragment)
    } else {
      fragment
    };

    // Some renderer-produced trees wrap the `<html>` element in an anonymous root box (e.g. when
    // fixed-position roots are hoisted). That wrapper is DOM-less but still has a `box_id` and
    // typically carries the default `ComputedStyle` (`display: inline`). Treat it like a wrapper
    // and locate the first non-fixed DOM-backed child so HTML canvas background propagation can
    // still find `<html>`/`<body>`.
    if html.box_id().is_some()
      && html
        .style
        .as_deref()
        .is_some_and(|style| matches!(style.display, crate::style::display::Display::Inline))
    {
      if let Some(child) = html.children.iter().find(|child| {
        child.box_id().is_some()
          && !child
            .style
            .as_deref()
            .is_some_and(|style| style.position == Position::Fixed)
      }) {
        html = child;
      }
    }

    if let Some(style) = html.style.clone() {
      if Self::has_paintable_background(&style) {
        return Some(style);
      }
    }

    // HTML canvas background propagation: when the root element background is transparent,
    // propagate the body element background. The layout tree may flatten the body box when it has
    // no paintable background, so avoid falling back to "first paintable child" heuristics for
    // renderer-produced trees (it can incorrectly promote a descendant background to the canvas).
    if let Some(html_id) = html.box_id() {
      if let Some(body_id) = html_id.checked_add(1) {
        // `<body>` is normally the first child of `<html>` (and therefore `box_id == html_id+1`),
        // but it may not be a direct fragment child in all layout modes. Search by box id rather
        // than assuming a fixed tree shape.
        let mut stack: Vec<&FragmentNode> = vec![html];
        while let Some(node) = stack.pop() {
          if node.box_id() == Some(body_id) {
            if let Some(style) = node.style.clone() {
              if Self::has_paintable_background(&style) {
                return Some(style);
              }
            }
            break;
          }
          for child in node.children.iter().rev() {
            stack.push(child);
          }
        }
      }
      return fragment.style.clone();
    }

    // Fallback for fragment trees without box IDs (mostly unit tests): treat the first paintable
    // child as the body element.
    for child in html.children.iter() {
      if let Some(style) = child.style.clone() {
        if Self::has_paintable_background(&style) {
          return Some(style);
        }
      }
    }

    fragment.style.clone()
  }

  /// Enqueue paint commands for the fragment's own content (text/replaced).
  fn enqueue_content(
    &self,
    fragment: &FragmentNode,
    abs_bounds: Rect,
    items: &mut Vec<DisplayCommand>,
  ) {
    match &fragment.content {
      FragmentContent::Text {
        text,
        baseline_offset,
        shaped,
        document_selection,
        ..
      } => {
        if let Some(style) = fragment.style.clone() {
          items.push(DisplayCommand::Text {
            rect: abs_bounds,
            baseline_offset: *baseline_offset,
            text: text.clone(),
            runs: shaped.clone(),
            style,
            document_selection: document_selection.clone(),
          });
        }
      }
      FragmentContent::Replaced {
        replaced_type,
        box_id,
      } => {
        if let Some(style) = fragment.style.clone() {
          items.push(DisplayCommand::Replaced {
            rect: abs_bounds,
            replaced_type: replaced_type.clone(),
            box_id: *box_id,
            style,
          });
        }
      }
      _ => {}
    }
  }

  fn collect_text_commands_for_clip(
    &self,
    fragment: &FragmentNode,
    offset: Point,
    out: &mut Vec<DisplayCommand>,
  ) {
    let style_opt = fragment.style.as_deref();
    if style_opt.is_some_and(|style| {
      !matches!(
        style.visibility,
        crate::style::computed::Visibility::Visible
      )
    }) {
      return;
    }
    let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
    if opacity <= f32::EPSILON {
      return;
    }

    if matches!(
      fragment.content,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
    ) {
      return;
    }

    let abs_bounds = Rect::from_xywh(
      fragment.bounds.x() + offset.x,
      fragment.bounds.y() + offset.y,
      fragment.bounds.width(),
      fragment.bounds.height(),
    );

    if let FragmentContent::Text {
      text,
      baseline_offset,
      shaped,
      is_marker,
      ..
    } = &fragment.content
    {
      if !text.is_empty() && !is_marker {
        if let Some(style) = fragment.style.clone() {
          out.push(DisplayCommand::Text {
            rect: abs_bounds,
            baseline_offset: *baseline_offset,
            text: text.clone(),
            runs: shaped.clone(),
            style,
            document_selection: None,
          });
        }
      }
    }

    let element_scroll = self.element_scroll_offset(fragment);
    let child_offset = Point::new(
      abs_bounds.x() - element_scroll.x,
      abs_bounds.y() - element_scroll.y,
    );
    for child in fragment.children.iter() {
      self.collect_text_commands_for_clip(child, child_offset, out);
    }
  }

  fn execute_command(&mut self, command: DisplayCommand) -> Result<()> {
    self.execute_command_with_depth(command, 0)
  }

  fn execute_command_with_depth(&mut self, mut command: DisplayCommand, depth: usize) -> Result<()> {
    if depth >= LEGACY_DISPLAY_COMMAND_NESTING_LIMIT {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: format!(
          "legacy painter display command nesting too deep (depth={depth}, limit={LEGACY_DISPLAY_COMMAND_NESTING_LIMIT})"
        ),
      }));
    }
    let cmd_profile_threshold_ms = cmd_profile_threshold_ms();
    let cmd_profile_enabled = cmd_profile_threshold_ms.is_some();
    let cmd_profile_start = cmd_profile_enabled.then(Instant::now);
    let cmd_profile_summary = cmd_profile_enabled.then(|| match &command {
      DisplayCommand::Background { rect, .. } => format!(
        "background rect=({:.1},{:.1},{:.1},{:.1})",
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height()
      ),
      DisplayCommand::Border { rect, .. } => format!(
        "border rect=({:.1},{:.1},{:.1},{:.1})",
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height()
      ),
      DisplayCommand::Outline { rect, .. } => format!(
        "outline rect=({:.1},{:.1},{:.1},{:.1})",
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height()
      ),
      DisplayCommand::Text { rect, text, style, .. } => {
        let preview: String = text.chars().take(60).collect();
        format!(
          "text font_size={:.2} rect=({:.1},{:.1},{:.1},{:.1}) writing_mode={:?} text=\"{}\"",
          style.font_size,
          rect.x(),
          rect.y(),
          rect.width(),
          rect.height(),
          style.writing_mode,
          preview
        )
      }
      DisplayCommand::Replaced { rect, replaced_type, .. } => format!(
        "replaced {:?} rect=({:.1},{:.1},{:.1},{:.1})",
        replaced_type,
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height()
      ),
      DisplayCommand::StackingContext { rect, commands, .. } => {
        let (nested_cmds, nested_text) = nested_counts(commands);
        format!(
          "stack rect=({:.1},{:.1},{:.1},{:.1}) nested_cmds={nested_cmds} nested_text={nested_text}",
          rect.x(),
          rect.y(),
          rect.width(),
          rect.height()
        )
      }
    });
    match &mut command {
      DisplayCommand::Background {
        rect,
        style,
        text_clip,
        scroll_delta,
      } => {
        let rect = *rect;
        let scroll_delta = *scroll_delta;
        let style = style.as_ref();
        if let Some(text_clip) = text_clip.as_deref() {
          self.paint_background_with_text_clip(rect, style, text_clip, scroll_delta)?;
        } else {
          self.paint_background(
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            style,
            scroll_delta,
          );
        }
      }
      DisplayCommand::Border { rect, style, gap } => {
        let rect = *rect;
        let gap = *gap;
        self.paint_borders(
          rect.x(),
          rect.y(),
          rect.width(),
          rect.height(),
          style.as_ref(),
          gap,
        );
      }
      DisplayCommand::Outline { rect, style } => {
        let rect = *rect;
        self.paint_outline(rect.x(), rect.y(), rect.width(), rect.height(), style.as_ref());
      }
      DisplayCommand::Text {
        rect,
        baseline_offset,
        text,
        runs,
        style,
        document_selection,
      } => {
        let rect = *rect;
        let baseline_offset = *baseline_offset;
        let style = style.as_ref();
        let text = text.as_ref();
        let runs = runs.clone();

        let text_profile_threshold_ms = text_profile_threshold_ms();
        let text_profile_enabled = text_profile_threshold_ms.is_some();
        let text_total_start = text_profile_enabled.then(Instant::now);
        let shape_start = text_profile_enabled.then(Instant::now);
        let color = style.color;
        let shaped_runs: Option<Arc<Vec<ShapedRun>>> = runs.clone().or_else(|| {
          self
            .shaper
            .shape_arc(text, style, &self.font_ctx)
            .ok()
        });
        let shape_ms = shape_start
          .map(|start| start.elapsed().as_secs_f64() * 1000.0)
          .unwrap_or(0.0);
        let paint_start = text_profile_enabled.then(Instant::now);

        if let Some(selection_ranges) = document_selection.as_deref() {
          let selection_color = Rgba {
            r: 0,
            g: 120,
            b: 215,
            a: 0.35,
          };
          let runs: &[ShapedRun] = shaped_runs.as_deref().map(Vec::as_slice).unwrap_or(&[]);
          for range in selection_ranges {
            if range.start >= range.end {
              continue;
            }
            for (x1, x2) in crate::text::caret::selection_segments_for_char_range(
              &text,
              runs,
              range.start,
              range.end,
            ) {
              let width = x2 - x1;
              if !width.is_finite() || width <= f32::EPSILON {
                continue;
              }
              let sel_rect = Rect::from_xywh(
                rect.x() + x1,
                rect.y(),
                width.max(0.0),
                rect.height().max(0.0),
              );
              if sel_rect.width() <= f32::EPSILON || sel_rect.height() <= f32::EPSILON {
                continue;
              }
              let device_rect = self.device_rect(sel_rect);
              crate::paint::rasterize::fill_rect(
                &mut self.pixmap,
                device_rect.x(),
                device_rect.y(),
                device_rect.width(),
                device_rect.height(),
                selection_color,
              );
            }
          }
        }

        let inline_vertical = matches!(
          style.writing_mode,
          crate::style::types::WritingMode::VerticalRl
            | crate::style::types::WritingMode::VerticalLr
            | crate::style::types::WritingMode::SidewaysRl
            | crate::style::types::WritingMode::SidewaysLr
        );
        let (block_baseline, inline_start, inline_len) = if inline_vertical {
          (rect.x() + baseline_offset, rect.y(), rect.height())
        } else {
          (rect.y() + baseline_offset, rect.x(), rect.width())
        };
        if let Some(ref shaped) = shaped_runs {
          if inline_vertical {
            self.paint_shaped_runs_vertical(
              shaped,
              block_baseline,
              inline_start,
              color,
              Some(style),
              None,
            );
          } else {
            self.paint_shaped_runs(
              shaped,
              inline_start,
              block_baseline,
              color,
              Some(style),
              None,
            );
          }
        } else {
          if inline_vertical {
            // Fallback: approximate vertical flow by painting horizontal text at the inline start.
            self.paint_text(
              text,
              Some(style),
              inline_start,
              rect.y(),
              style.font_size,
              color,
            )?;
          } else {
            self.paint_text(
              text,
              Some(style),
              inline_start,
              block_baseline,
              style.font_size,
              color,
            )?;
          }
        }
        self.paint_text_decoration(
          style,
          shaped_runs.as_deref().map(Vec::as_slice),
          inline_start,
          block_baseline,
          inline_len,
          inline_vertical,
        );
        self.paint_text_emphasis(
          style,
          shaped_runs.as_deref().map(Vec::as_slice),
          inline_start,
          block_baseline,
          inline_vertical,
        );
        let paint_ms = paint_start
          .map(|start| start.elapsed().as_secs_f64() * 1000.0)
          .unwrap_or(0.0);
        if let (Some(threshold_ms), Some(total_start)) =
          (text_profile_threshold_ms, text_total_start)
        {
          let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
          if total_ms >= threshold_ms {
            let (run_count, glyph_count) = shaped_runs
              .as_deref()
              .map(|runs| {
                (
                  runs.len(),
                  runs.iter().map(|r| r.glyphs.len()).sum::<usize>(),
                )
              })
              .unwrap_or((0, 0));
            let preview: String = text.chars().take(80).collect();
            eprintln!(
              "text_profile total_ms={total_ms:.2} shape_ms={shape_ms:.2} paint_ms={paint_ms:.2} runs={run_count} glyphs={glyph_count} font_size={:.2} rect=({:.1},{:.1},{:.1},{:.1}) baseline_off={:.2} writing_mode={:?} text=\"{}\"",
              style.font_size,
              rect.x(),
              rect.y(),
              rect.width(),
              rect.height(),
              baseline_offset,
              style.writing_mode,
              preview
            );
          }
        }
      }
      DisplayCommand::Replaced {
        rect,
        replaced_type,
        box_id,
        style,
      } => {
        let rect = *rect;
        let box_id = *box_id;
        self.paint_replaced(
          replaced_type,
          box_id,
          Some(style.as_ref()),
          rect.x(),
          rect.y(),
          rect.width(),
          rect.height(),
        );
      }
      DisplayCommand::StackingContext {
        rect: context_rect,
        opacity,
        transform: _,
        transform_3d,
        blend_mode,
        isolated,
        mask,
        mask_border,
        filters,
        backdrop_filters,
        radii,
        clip,
        has_clip_path,
        clip_path,
        root_style,
        commands,
      } => {
        let context_rect = *context_rect;
        let opacity = *opacity;
        let transform_3d = *transform_3d;
        let blend_mode = *blend_mode;
        let isolated = *isolated;
        let radii = *radii;
        let clip = *clip;
        let has_clip_path = *has_clip_path;
        let mask = std::mem::take(mask);
        let mask_border = std::mem::take(mask_border);
        let filters = std::mem::take(filters);
        let backdrop_filters = std::mem::take(backdrop_filters);
        let clip_path = std::mem::take(clip_path);
        let root_style = std::mem::take(root_style);
        let commands = std::mem::take(commands);

        let profile_threshold_ms = stack_profile_threshold_ms();
        let profile_enabled = profile_threshold_ms.is_some();
        let total_start = profile_enabled.then(Instant::now);
        let transform_identity = transform_3d.as_ref().map_or(true, Transform3D::is_identity);
        let mut bounds_ms = 0.0;
        let mut translate_ms = 0.0;
        let mut base_paint_ms = 0.0;
        let mut clip_paint_ms = 0.0;
        let mut overflow_clip_ms = 0.0;
        let mut clip_path_ms = 0.0;
        let mut mask_ms = 0.0;
        let mut outline_ms = 0.0;
        let mut filter_ms = 0.0;
        let mut radii_clip_ms = 0.0;
        let mut backdrop_ms = 0.0;
        let mut composite_ms = 0.0;

        let debug_stack = dump_stack_enabled();
        let nested_info = (debug_stack || profile_enabled).then(|| nested_counts(&commands));
        if debug_stack {
          let (nested_cmds, nested_text) = nested_info.unwrap_or((0, 0));
          eprintln!(
            "stack exec: preclip_bounds=({}, {}, {}, {}) nested_cmds={} nested_text={}",
            context_rect.x(),
            context_rect.y(),
            context_rect.width(),
            context_rect.height(),
            nested_cmds,
            nested_text
          );
        }
        if opacity <= 0.0 {
          return Ok(());
        }

        // Fast path: a "transform: (identity)" stacking context with no other effects can be
        // flattened without affecting pixels. This avoids allocating intermediate layers for
        // common patterns like `translate3d(0, 0, 0)`.
        if opacity >= 1.0 - 1e-6
          && transform_identity
          && matches!(blend_mode, MixBlendMode::Normal)
          && !isolated
          && mask.is_none()
          && mask_border.is_none()
          && filters.is_empty()
          && backdrop_filters.is_empty()
          && clip.is_none()
          && !has_clip_path
        {
          let mut outlines = Vec::new();
          for cmd in commands {
            if matches!(cmd, DisplayCommand::Outline { .. }) {
              outlines.push(cmd);
            } else {
              self.execute_command_with_depth(cmd, depth.saturating_add(1))?;
            }
          }
          for cmd in outlines {
            self.execute_command_with_depth(cmd, depth.saturating_add(1))?;
          }
          return Ok(());
        }

        let wants_text_clip = mask.as_ref().is_some_and(|style| {
          style
            .mask_layers
            .iter()
            .any(|layer| matches!(layer.clip, MaskClip::Text))
        });
        let text_clip_commands = wants_text_clip.then(|| commands.clone());

        // When clipping transformed stacking contexts to the viewport, we must compute the layer
        // bounds in the *pre-transform* coordinate space. Otherwise, classic centering patterns
        // like `left: 50%; transform: translateX(-50%)` can have all required source pixels culled
        // before the transform is applied, producing an empty layer.
        //
        // For 2D affine transforms we map the viewport back into the layer's source coordinate
        // space using the inverse transform. For non-invertible / non-affine transforms we fall
        // back to the previous conservative behavior.
        let viewport_inverse_2d = transform_3d
          .as_ref()
          .filter(|t| !t.is_identity())
          .and_then(|t| t.to_2d())
          .and_then(|t| t.inverse());

        let bounds_start = profile_enabled.then(Instant::now);
        let Some(mut bounds) = (match viewport_inverse_2d {
          Some(_) => stacking_context_bounds(
            &commands,
            &filters,
            &backdrop_filters,
            context_rect,
            None,
            clip.as_ref(),
            clip_path.as_ref(),
            (self.css_width, self.css_height),
          ),
          None => stacking_context_bounds(
            &commands,
            &filters,
            &backdrop_filters,
            context_rect,
            transform_3d.as_ref(),
            clip.as_ref(),
            clip_path.as_ref(),
            (self.css_width, self.css_height),
          ),
        }) else {
          return Ok(());
        };
        if let Some(start) = bounds_start {
          bounds_ms = start.elapsed().as_secs_f64() * 1000.0;
        }

        // Constrain the stacking layer to the visible viewport (expanded by filter outsets).
        // If a context is entirely outside the viewport, skip it.
        let (filter_l, filter_t, filter_r, filter_b) = compute_filter_outset(&filters, bounds, 1.0);
        let (back_l, back_t, back_r, back_b) =
          compute_filter_outset(&backdrop_filters, bounds, 1.0);
        let expand_l = filter_l.max(back_l);
        let expand_t = filter_t.max(back_t);
        let expand_r = filter_r.max(back_r);
        let expand_b = filter_b.max(back_b);
        let view_bounds = match viewport_inverse_2d {
          Some(inv) => {
            let viewport_rect = Rect::from_xywh(0.0, 0.0, self.css_width, self.css_height);
            let inv_viewport = inv.transform_rect(viewport_rect);
            Rect::from_xywh(
              inv_viewport.x() - expand_l,
              inv_viewport.y() - expand_t,
              inv_viewport.width() + expand_l + expand_r,
              inv_viewport.height() + expand_t + expand_b,
            )
          }
          None => Rect::from_xywh(
            -expand_l,
            -expand_t,
            self.css_width + expand_l + expand_r,
            self.css_height + expand_t + expand_b,
          ),
        };
        match bounds.intersection(view_bounds) {
          Some(clipped) if clipped.width() > 0.0 && clipped.height() > 0.0 => bounds = clipped,
          _ => return Ok(()),
        }
        if debug_stack {
          eprintln!(
            "stack exec: postclip_bounds=({}, {}, {}, {})",
            bounds.x(),
            bounds.y(),
            bounds.width(),
            bounds.height()
          );
        }
        if !bounds.width().is_finite()
          || !bounds.height().is_finite()
          || bounds.width() <= 0.0
          || bounds.height() <= 0.0
        {
          return Ok(());
        }

        // Optional debug: report nested command counts before translating
        if dump_stack_enabled() {
          fn count_commands(cmds: &[DisplayCommand]) -> (usize, usize) {
            cmds.iter().fold((0, 0), |(total, text), cmd| match cmd {
              DisplayCommand::StackingContext { commands, .. } => {
                let (t, tx) = count_commands(commands);
                (total + 1 + t, text + tx)
              }
              DisplayCommand::Text { .. } => (total + 1, text + 1),
              _ => (total + 1, text),
            })
          }
          let (total, text) = count_commands(&commands);
          eprintln!(
            "stacking debug: commands={} text={} bounds=({}, {}, {}, {})",
            total,
            text,
            bounds.min_x(),
            bounds.min_y(),
            bounds.width(),
            bounds.height()
          );
        }

        // Reduce intermediate layer size by rendering into a pixmap whose origin is the
        // top-left of the computed bounds. Display commands remain in absolute CSS coordinates;
        // the child painters use `origin_offset_css` to map them into the local layer space.
        let offset = Point::new(bounds.min_x(), bounds.min_y());
        translate_ms = 0.0;

        let device_bounds = self.device_rect(bounds);
        let width = device_bounds.width().ceil().max(0.0);
        let height = device_bounds.height().ceil().max(0.0);
        if width <= 0.0 || height <= 0.0 {
          return Ok(());
        }

        let root_rect = match context_rect.intersection(bounds) {
          Some(r) if r.width() > 0.0 && r.height() > 0.0 => r,
          _ => bounds,
        };
        let has_clip = clip.is_some();
        let clip_root = clip.map(|clip| clip.clip_root).unwrap_or(false);
        let root_style_ptr = root_style
          .as_ref()
          .map(|style| Arc::as_ptr(style) as *const () as usize);
        let mut outline_commands = Vec::new();
        let mut unclipped = Vec::new();
        let mut clipped = Vec::new();
        if has_clip {
          for cmd in commands {
            if matches!(cmd, DisplayCommand::Outline { .. }) {
              outline_commands.push(cmd);
              continue;
            }

            let treat_as_root_decoration = if clip_root {
              false
            } else if let Some(ptr) = root_style_ptr {
              match &cmd {
                DisplayCommand::Background { style, .. } | DisplayCommand::Border { style, .. } => {
                  Arc::as_ptr(style) as *const () as usize == ptr
                }
                _ => false,
              }
            } else {
              // Fallback for synthetic/test command lists without a root style pointer: preserve the
              // prior rect-based heuristic.
              match &cmd {
                DisplayCommand::Background { rect, .. } | DisplayCommand::Border { rect, .. } => {
                  (rect.x() - root_rect.x()).abs() < f32::EPSILON
                    && (rect.y() - root_rect.y()).abs() < f32::EPSILON
                    && (rect.width() - root_rect.width()).abs() < f32::EPSILON
                    && (rect.height() - root_rect.height()).abs() < f32::EPSILON
                }
                _ => false,
              }
            };

            if treat_as_root_decoration {
              unclipped.push(cmd);
            } else {
              clipped.push(cmd);
            }
          }
        } else {
          // Without overflow clipping there's no need for a secondary clip layer; execute
          // commands directly into the base layer while keeping outlines separate.
          for cmd in commands {
            if matches!(cmd, DisplayCommand::Outline { .. }) {
              outline_commands.push(cmd);
            } else {
              unclipped.push(cmd);
            }
          }
        }

        let layer_w = width as u32;
        let layer_h = height as u32;
        let layer = match new_pixmap(layer_w, layer_h) {
          Some(p) => {
            if self.diagnostics_enabled {
              record_layer_allocation(layer_w, layer_h);
            }
            p
          }
          None => return Ok(()),
        };

        let mut base_painter = Painter {
          pixmap: layer,
          scale: self.scale,
          origin_offset_css: offset,
          css_width: self.css_width,
          css_height: self.css_height,
          background: Rgba::new(0, 0, 0, 0.0),
          shaper: ShapingPipeline::new(),
          font_ctx: self.font_ctx.clone(),
          image_cache: self.image_cache.clone(),
          media_provider: self.media_provider.clone(),
          svg_id_defs: self.svg_id_defs.clone(),
          svg_id_defs_raw: self.svg_id_defs_raw.clone(),
          text_shape_cache: Arc::clone(&self.text_shape_cache),
          trace: self.trace.clone(),
          scroll_state: self.scroll_state.clone(),
          gradient_cache: self.gradient_cache.clone(),
          gradient_stats: GradientStats::default(),
          diagnostics_enabled: self.diagnostics_enabled,
          max_iframe_depth: self.max_iframe_depth,
        };
        let device_root_rect = base_painter.device_rect(root_rect);
        let paint_unclipped_start = profile_enabled.then(Instant::now);
        for cmd in unclipped {
          base_painter.execute_command_with_depth(cmd, depth.saturating_add(1))?;
        }
        if let Some(start) = paint_unclipped_start {
          base_paint_ms = start.elapsed().as_secs_f64() * 1000.0;
        }

        if !clipped.is_empty() {
          let clip_layer = match new_pixmap(layer_w, layer_h) {
            Some(p) => {
              if self.diagnostics_enabled {
                record_layer_allocation(layer_w, layer_h);
              }
              p
            }
            None => return Ok(()),
          };
          let mut clip_painter = Painter {
            pixmap: clip_layer,
            scale: self.scale,
            origin_offset_css: offset,
            css_width: self.css_width,
            css_height: self.css_height,
            background: Rgba::new(0, 0, 0, 0.0),
            shaper: ShapingPipeline::new(),
            font_ctx: self.font_ctx.clone(),
            image_cache: self.image_cache.clone(),
            media_provider: self.media_provider.clone(),
            svg_id_defs: self.svg_id_defs.clone(),
            svg_id_defs_raw: self.svg_id_defs_raw.clone(),
            text_shape_cache: Arc::clone(&self.text_shape_cache),
            trace: self.trace.clone(),
            scroll_state: self.scroll_state.clone(),
            gradient_cache: self.gradient_cache.clone(),
            gradient_stats: GradientStats::default(),
            diagnostics_enabled: self.diagnostics_enabled,
            max_iframe_depth: self.max_iframe_depth,
          };
          let paint_clipped_start = profile_enabled.then(Instant::now);
          for cmd in clipped {
            clip_painter.execute_command_with_depth(cmd, depth.saturating_add(1))?;
          }
          if let Some(start) = paint_clipped_start {
            clip_paint_ms = start.elapsed().as_secs_f64() * 1000.0;
          }
          base_painter
            .gradient_stats
            .merge(&clip_painter.gradient_stats);
          let mut clip_pixmap = clip_painter.pixmap;
          if let Some(clip) = clip {
            let mut clip_rect = clip.rect;
            let mut clip_radii = clip.radii;
            let clip_x = clip.clip_x;
            let clip_y = clip.clip_y;
            if !clip_x {
              clip_rect = Rect::from_xywh(
                bounds.min_x(),
                clip_rect.y(),
                bounds.width(),
                clip_rect.height(),
              );
              clip_radii = BorderRadii::ZERO;
            }
            if !clip_y {
              clip_rect = Rect::from_xywh(
                clip_rect.x(),
                bounds.min_y(),
                clip_rect.width(),
                bounds.height(),
              );
              clip_radii = BorderRadii::ZERO;
            }
            let local_clip = clip_rect_axes(clip_rect, bounds, true, true);
            if local_clip.width() > 0.0 && local_clip.height() > 0.0 {
              let clip_apply_start = profile_enabled.then(Instant::now);
              apply_clip_mask_rect(
                &mut clip_pixmap,
                base_painter.device_rect(local_clip),
                base_painter.device_radii(clip_radii),
              )?;
              if let Some(start) = clip_apply_start {
                overflow_clip_ms = start.elapsed().as_secs_f64() * 1000.0;
              }
              base_painter.pixmap.draw_pixmap(
                0,
                0,
                clip_pixmap.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
              );
            }
          } else {
            base_painter.pixmap.draw_pixmap(
              0,
              0,
              clip_pixmap.as_ref(),
              &PixmapPaint::default(),
              Transform::identity(),
              None,
            );
          }
        }

        if let Some(ref clip_path) = clip_path {
          let clip_path_start = profile_enabled.then(Instant::now);
          match clip_path {
            StackingClipPath::Shape(clip_path) => {
              if let Some(size) =
                IntSize::from_wh(base_painter.pixmap.width(), base_painter.pixmap.height())
              {
                let transform =
                  Transform::from_translate(-offset.x * self.scale, -offset.y * self.scale);
                if let Some(mask) = clip_path.mask(self.scale, size, transform) {
                  let clip_bounds_device = base_painter.device_rect(clip_path.bounds());
                  let dirty = clip_mask_dirty_bounds(
                    clip_bounds_device,
                    base_painter.pixmap.width(),
                    base_painter.pixmap.height(),
                  );
                  apply_mask_with_dirty_bounds_rgba(&mut base_painter.pixmap, &mask, dirty)?;
                }
              }
            }
            StackingClipPath::SvgFragment { id, reference_rect } => {
              (|| -> RenderResult<()> {
                let canvas_w = base_painter.pixmap.width();
                let canvas_h = base_painter.pixmap.height();
                if canvas_w == 0 || canvas_h == 0 {
                  return Ok(());
                }

                let reference_width = reference_rect.width();
                let reference_height = reference_rect.height();
                if reference_width <= 0.0
                  || reference_height <= 0.0
                  || !reference_width.is_finite()
                  || !reference_height.is_finite()
                {
                  // A degenerate reference box clips away the entire stacking context.
                  check_active(RenderStage::Paint)?;
                  base_painter.pixmap.data_mut().fill(0);
                  return Ok(());
                }

                let Some(defs) = base_painter.svg_id_defs.as_ref() else {
                  // Unresolvable fragment-only clip-path URLs behave like `clip-path: none`.
                  return Ok(());
                };

                // Rasterize the SVG clip-path over the full stacking context bounds (not just the
                // reference box) so `clipPathUnits="userSpaceOnUse"` clip paths can expose pixels
                // outside the border box.
                let mask_bounds_css = bounds;
                let viewbox_x = mask_bounds_css.x() - reference_rect.x();
                let viewbox_y = mask_bounds_css.y() - reference_rect.y();
                if !viewbox_x.is_finite() || !viewbox_y.is_finite() {
                  return Ok(());
                }

                let view_w = if mask_bounds_css.width().is_finite() && mask_bounds_css.width() > 0.0
                {
                  mask_bounds_css.width()
                } else {
                  1.0
                };
                let view_h =
                  if mask_bounds_css.height().is_finite() && mask_bounds_css.height() > 0.0 {
                    mask_bounds_css.height()
                  } else {
                    1.0
                  };

                let Some(svg) =
                  crate::paint::svg_mask_image::inline_svg_for_clip_path_id_with_view_box_offset(
                    defs,
                    id,
                    reference_width,
                    reference_height,
                    viewbox_x,
                    viewbox_y,
                    view_w,
                    view_h,
                    canvas_w,
                    canvas_h,
                  )
                else {
                  // Missing defs: treat as `clip-path: none`.
                  return Ok(());
                };

                let cache_key = format!("svg-clip-path-fragment:{id}");
                let clip_pixmap = match base_painter
                  .image_cache
                  .render_svg_pixmap_at_size(&svg, canvas_w, canvas_h, &cache_key, self.scale)
                {
                  Ok(pixmap) => pixmap,
                  Err(crate::Error::Render(RenderError::Timeout { stage, elapsed })) => {
                    return Err(RenderError::Timeout { stage, elapsed });
                  }
                  Err(_) => return Ok(()),
                };

                let mask =
                  Mask::from_pixmap(clip_pixmap.as_ref().as_ref(), tiny_skia::MaskType::Alpha);
                let dirty = Some(ClipMaskDirtyRect {
                  x0: 0,
                  y0: 0,
                  x1: canvas_w,
                  y1: canvas_h,
                });
                apply_mask_with_dirty_bounds_rgba(&mut base_painter.pixmap, &mask, dirty)?;
                Ok(())
              })()?;
            }
            StackingClipPath::SvgExternal {
              doc_url,
              id,
              reference_rect,
            } => {
              (|| -> RenderResult<()> {
                let canvas_w = base_painter.pixmap.width();
                let canvas_h = base_painter.pixmap.height();
                if canvas_w == 0 || canvas_h == 0 {
                  return Ok(());
                }

                let reference_width = reference_rect.width();
                let reference_height = reference_rect.height();
                if reference_width <= 0.0
                  || reference_height <= 0.0
                  || !reference_width.is_finite()
                  || !reference_height.is_finite()
                {
                  // A degenerate reference box clips away the entire stacking context.
                  check_active(RenderStage::Paint)?;
                  base_painter.pixmap.data_mut().fill(0);
                  return Ok(());
                }

                let resolved_doc_url = base_painter.image_cache.resolve_url(doc_url);
                let cached = match base_painter
                  .image_cache
                  .load_with_crossorigin(&resolved_doc_url, CrossOriginAttribute::None)
                {
                  Ok(img) => img,
                  Err(_) => return Ok(()),
                };
                if !cached.is_vector {
                  return Ok(());
                }
                let Some(svg_markup) = cached.svg_content.as_deref() else {
                  return Ok(());
                };
                let defs =
                  crate::paint::svg_mask_image::collect_svg_id_defs_from_svg_document(svg_markup);

                // Rasterize the SVG clip-path over the full stacking context bounds (not just the
                // reference box) so `clipPathUnits="userSpaceOnUse"` clip paths can expose pixels
                // outside the border box.
                let mask_bounds_css = bounds;
                let viewbox_x = mask_bounds_css.x() - reference_rect.x();
                let viewbox_y = mask_bounds_css.y() - reference_rect.y();
                if !viewbox_x.is_finite() || !viewbox_y.is_finite() {
                  return Ok(());
                }

                let view_w = if mask_bounds_css.width().is_finite() && mask_bounds_css.width() > 0.0
                {
                  mask_bounds_css.width()
                } else {
                  1.0
                };
                let view_h =
                  if mask_bounds_css.height().is_finite() && mask_bounds_css.height() > 0.0 {
                    mask_bounds_css.height()
                  } else {
                    1.0
                  };

                let Some(svg) =
                  crate::paint::svg_mask_image::inline_svg_for_clip_path_id_with_view_box_offset(
                    &defs,
                    id,
                    reference_width,
                    reference_height,
                    viewbox_x,
                    viewbox_y,
                    view_w,
                    view_h,
                    canvas_w,
                    canvas_h,
                  )
                else {
                  // Missing defs: treat as `clip-path: none`.
                  return Ok(());
                };

                let cache_key = format!("svg-clip-path-external:{id}");
                let clip_pixmap = match base_painter
                  .image_cache
                  .render_svg_pixmap_at_size(&svg, canvas_w, canvas_h, &cache_key, self.scale)
                {
                  Ok(pixmap) => pixmap,
                  Err(crate::Error::Render(RenderError::Timeout { stage, elapsed })) => {
                    return Err(RenderError::Timeout { stage, elapsed });
                  }
                  Err(_) => return Ok(()),
                };

                let mask =
                  Mask::from_pixmap(clip_pixmap.as_ref().as_ref(), tiny_skia::MaskType::Alpha);
                let dirty = Some(ClipMaskDirtyRect {
                  x0: 0,
                  y0: 0,
                  x1: canvas_w,
                  y1: canvas_h,
                });
                apply_mask_with_dirty_bounds_rgba(&mut base_painter.pixmap, &mask, dirty)?;
                Ok(())
              })()?;
            }
          }
          if let Some(start) = clip_path_start {
            clip_path_ms = start.elapsed().as_secs_f64() * 1000.0;
          }
        }
        if let Some(ref mask_style) = mask {
          let mask_start = profile_enabled.then(Instant::now);
          let text_clip_mask = if wants_text_clip {
            base_painter.build_text_clip_mask_from_commands(
              text_clip_commands.as_deref().unwrap_or(&[]),
              (base_painter.pixmap.width(), base_painter.pixmap.height()),
            )?
          } else {
            None
          };
          if let Some(rendered) = base_painter.render_mask(
            mask_style,
            context_rect,
            bounds,
            (base_painter.pixmap.width(), base_painter.pixmap.height()),
            text_clip_mask.as_ref(),
          )? {
            apply_mask_with_dirty_bounds_rgba(
              &mut base_painter.pixmap,
              rendered.mask(),
              rendered.dirty,
            )?;
          }
          if let Some(start) = mask_start {
            mask_ms += start.elapsed().as_secs_f64() * 1000.0;
          }
        }
        if let Some(ref mask_border_style) = mask_border {
          let mask_start = profile_enabled.then(Instant::now);
          if let Some(rendered) = base_painter.render_mask_border(
            mask_border_style,
            context_rect,
            bounds,
            (base_painter.pixmap.width(), base_painter.pixmap.height()),
          )? {
            apply_mask_with_dirty_bounds_rgba(
              &mut base_painter.pixmap,
              rendered.mask(),
              rendered.dirty,
            )?;
          }
          if let Some(start) = mask_start {
            mask_ms += start.elapsed().as_secs_f64() * 1000.0;
          }
        }
        if !outline_commands.is_empty() {
          let outline_start = profile_enabled.then(Instant::now);
          for cmd in outline_commands {
            base_painter.execute_command_with_depth(cmd, depth.saturating_add(1))?;
          }
          if let Some(start) = outline_start {
            outline_ms = start.elapsed().as_secs_f64() * 1000.0;
          }
        }

        if !filters.is_empty() {
          let filter_start = profile_enabled.then(Instant::now);
          apply_filters(
            &mut base_painter.pixmap,
            &filters,
            self.scale,
            device_root_rect,
          )?;
          if let Some(start) = filter_start {
            filter_ms = start.elapsed().as_secs_f64() * 1000.0;
          }
        }

        let device_radii = base_painter.device_radii(radii);
        if !filters.is_empty() {
          let (device_out_l, device_out_t, device_out_r, device_out_b) =
            compute_filter_outset(&filters, root_rect, self.scale);
          let clip_rect = Rect::from_xywh(
            device_root_rect.x() - device_out_l,
            device_root_rect.y() - device_out_t,
            device_root_rect.width() + device_out_l + device_out_r,
            device_root_rect.height() + device_out_t + device_out_b,
          );
          let radii_clip_start = profile_enabled.then(Instant::now);
          apply_clip_mask_rect(&mut base_painter.pixmap, clip_rect, BorderRadii::ZERO)?;
          if let Some(start) = radii_clip_start {
            radii_clip_ms = start.elapsed().as_secs_f64() * 1000.0;
          }
        }

        self.gradient_stats.merge(&base_painter.gradient_stats);
        let mut layer_pixmap = base_painter.pixmap;

        let combined_transform = transform_3d
          .unwrap_or_else(Transform3D::identity)
          .multiply(&Transform3D::translate(offset.x, offset.y, 0.0));
        let affine_2d = combined_transform.to_2d();

        let src_quad = [
          Point::new(0.0, 0.0),
          Point::new(layer_pixmap.width() as f32, 0.0),
          Point::new(layer_pixmap.width() as f32, layer_pixmap.height() as f32),
          Point::new(0.0, layer_pixmap.height() as f32),
        ];
        let mut dest_quad_device = src_quad;
        let mut projected = true;
        let layer_bounds_css = Rect::from_xywh(0.0, 0.0, bounds.width(), bounds.height());
        for (idx, corner) in rect_corners(layer_bounds_css).iter().enumerate() {
          let (tx, ty, _tz, tw) = combined_transform.transform_point(corner.x, corner.y, 0.0);
          if !tx.is_finite() || !ty.is_finite() || !tw.is_finite() || tw.abs() < 1e-6 || tw < 0.0 {
            projected = false;
            break;
          }
          dest_quad_device[idx] = self.device_point(Point::new(tx / tw, ty / tw));
        }
        if !projected {
          dest_quad_device = src_quad;
        }
        let dest_bounds_device = quad_bounds(&dest_quad_device);

        if !backdrop_filters.is_empty() {
          let backdrop_start = profile_enabled.then(Instant::now);
          // `backdrop-filter` is scoped to the stacking context root border box, not the
          // entire stacking layer bounds (which may include descendant overflow or the
          // union of pre-/post-transform bounds used for viewport culling).
          let root_local = Rect::from_xywh(
            root_rect.x() - offset.x,
            root_rect.y() - offset.y,
            root_rect.width(),
            root_rect.height(),
          );
          let mut backdrop_quad_device = [Point::ZERO; 4];
          let mut projected = true;
          for (idx, corner) in rect_corners(root_local).iter().enumerate() {
            let (tx, ty, _tz, tw) = combined_transform.transform_point(corner.x, corner.y, 0.0);
            if !tx.is_finite() || !ty.is_finite() || !tw.is_finite() || tw.abs() < 1e-6 || tw < 0.0
            {
              projected = false;
              break;
            }
            backdrop_quad_device[idx] = self.device_point(Point::new(tx / tw, ty / tw));
          }
          let backdrop_bounds_device = if projected {
            quad_bounds(&backdrop_quad_device)
          } else {
            self.device_rect(root_rect)
          };
          apply_backdrop_filters(
            &mut self.pixmap,
            &backdrop_bounds_device,
            &backdrop_filters,
            device_radii,
            self.scale,
            root_rect,
          )?;
          if let Some(start) = backdrop_start {
            backdrop_ms = start.elapsed().as_secs_f64() * 1000.0;
          }
        }
        let composite_start = profile_enabled.then(Instant::now);
        let fallback_blend = map_blend_mode(blend_mode);
        if let Some(affine) = affine_2d {
          let mut final_transform = self
            .device_transform(Some(transform2d_to_skia(affine)))
            .unwrap_or_else(Transform::identity);
          if is_hsl_blend(blend_mode) {
            if let Some(mut transformed) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
              if self.diagnostics_enabled {
                record_layer_allocation(self.pixmap.width(), self.pixmap.height());
              }
              let mut paint = PixmapPaint::default();
              paint.opacity = 1.0;
              paint.blend_mode = SkiaBlendMode::SourceOver;
              transformed.draw_pixmap(0, 0, layer_pixmap.as_ref(), &paint, final_transform, None);
              composite_hsl_layer(
                &mut self.pixmap,
                &transformed,
                opacity.min(1.0),
                blend_mode,
                Some(dest_bounds_device),
              );
            } else {
              let mut paint = PixmapPaint::default();
              paint.opacity = opacity.min(1.0);
              paint.blend_mode = fallback_blend;
              if paint.blend_mode == SkiaBlendMode::Plus {
                draw_pixmap_with_plus_blend(
                  &mut self.pixmap,
                  0,
                  0,
                  layer_pixmap.as_ref(),
                  paint.opacity,
                  paint.quality,
                  final_transform,
                  None,
                );
              } else {
                self
                  .pixmap
                  .draw_pixmap(0, 0, layer_pixmap.as_ref(), &paint, final_transform, None);
              }
            }
          } else {
            let mut paint = PixmapPaint::default();
            paint.opacity = opacity.min(1.0);
            paint.blend_mode = fallback_blend;
            if paint.blend_mode == SkiaBlendMode::Plus {
              draw_pixmap_with_plus_blend(
                &mut self.pixmap,
                0,
                0,
                layer_pixmap.as_ref(),
                paint.opacity,
                paint.quality,
                final_transform,
                None,
              );
            } else {
              self
                .pixmap
                .draw_pixmap(0, 0, layer_pixmap.as_ref(), &paint, final_transform, None);
            }
          }
        } else if let Some(homography) = Homography::from_quads(src_quad, dest_quad_device) {
          let dst_quad: [(f32, f32); 4] = dest_quad_device.map(|p| (p.x, p.y));
          let target_size = (self.pixmap.width(), self.pixmap.height());
          if is_hsl_blend(blend_mode) {
            if let Some(warped) =
              warp_pixmap(&layer_pixmap, &homography, &dst_quad, target_size, None)?
            {
              if let Some(mut transformed) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
                if self.diagnostics_enabled {
                  record_layer_allocation(self.pixmap.width(), self.pixmap.height());
                }
                let mut paint = PixmapPaint::default();
                paint.opacity = 1.0;
                paint.blend_mode = SkiaBlendMode::SourceOver;
                transformed.draw_pixmap(
                  warped.offset.0,
                  warped.offset.1,
                  warped.pixmap.as_ref(),
                  &paint,
                  Transform::identity(),
                  None,
                );
                composite_hsl_layer(
                  &mut self.pixmap,
                  &transformed,
                  opacity.min(1.0),
                  blend_mode,
                  Some(dest_bounds_device),
                );
              } else {
                let mut paint = PixmapPaint::default();
                paint.opacity = opacity.min(1.0);
                paint.blend_mode = fallback_blend;
                if paint.blend_mode == SkiaBlendMode::Plus {
                  draw_pixmap_with_plus_blend(
                    &mut self.pixmap,
                    warped.offset.0,
                    warped.offset.1,
                    warped.pixmap.as_ref(),
                    paint.opacity,
                    paint.quality,
                    Transform::identity(),
                    None,
                  );
                } else {
                  self.pixmap.draw_pixmap(
                    warped.offset.0,
                    warped.offset.1,
                    warped.pixmap.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                  );
                }
              }
            }
          } else if let Some(warped) =
            warp_pixmap(&layer_pixmap, &homography, &dst_quad, target_size, None)?
          {
            let mut paint = PixmapPaint::default();
            paint.opacity = opacity.min(1.0);
            paint.blend_mode = fallback_blend;
            if paint.blend_mode == SkiaBlendMode::Plus {
              draw_pixmap_with_plus_blend(
                &mut self.pixmap,
                warped.offset.0,
                warped.offset.1,
                warped.pixmap.as_ref(),
                paint.opacity,
                paint.quality,
                Transform::identity(),
                None,
              );
            } else {
              self.pixmap.draw_pixmap(
                warped.offset.0,
                warped.offset.1,
                warped.pixmap.as_ref(),
                &paint,
                Transform::identity(),
                None,
              );
            }
          }
        }
        if let Some(start) = composite_start {
          composite_ms = start.elapsed().as_secs_f64() * 1000.0;
        }

        if let (Some(threshold_ms), Some(total_start)) = (profile_threshold_ms, total_start) {
          let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
          if total_ms >= threshold_ms {
            let (nested_cmds, nested_text) = nested_info.unwrap_or((0, 0));
            eprintln!(
              "stack_profile total_ms={total_ms:.2} bounds_ms={bounds_ms:.2} translate_ms={translate_ms:.2} base_paint_ms={base_paint_ms:.2} clip_paint_ms={clip_paint_ms:.2} overflow_clip_ms={overflow_clip_ms:.2} clip_path_ms={clip_path_ms:.2} mask_ms={mask_ms:.2} outline_ms={outline_ms:.2} filter_ms={filter_ms:.2} radii_clip_ms={radii_clip_ms:.2} backdrop_ms={backdrop_ms:.2} composite_ms={composite_ms:.2} layer_px={}x{} nested_cmds={nested_cmds} nested_text={nested_text} filters={} backdrop_filters={} clip={} clip_path={} mask={} opacity={:.2} transform_identity={} blend_mode={:?} isolated={}",
              layer_pixmap.width(),
              layer_pixmap.height(),
              filters.len(),
              backdrop_filters.len(),
              clip.is_some(),
              clip_path.is_some(),
              mask.is_some(),
              opacity,
              transform_identity,
              blend_mode,
              isolated
            );
          }
        }
      }
    }
    if let (Some(threshold_ms), Some(start), Some(summary)) = (
      cmd_profile_threshold_ms,
      cmd_profile_start,
      cmd_profile_summary,
    ) {
      let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
      if elapsed_ms >= threshold_ms {
        eprintln!("cmd_profile ms={elapsed_ms:.2} {summary}");
      }
    }
    Ok(())
  }

  fn build_text_clip_mask_from_commands(
    &self,
    commands: &[DisplayCommand],
    device_size: (u32, u32),
  ) -> RenderResult<Option<Mask>> {
    let Some(mut pixmap) = new_pixmap(device_size.0, device_size.1) else {
      return Ok(None);
    };
    pixmap.data_mut().fill(0);

    let mut painter = Painter {
      pixmap,
      scale: self.scale,
      origin_offset_css: self.origin_offset_css,
      css_width: self.css_width,
      css_height: self.css_height,
      background: Rgba::new(0, 0, 0, 0.0),
      shaper: ShapingPipeline::new(),
      font_ctx: self.font_ctx.clone(),
      image_cache: self.image_cache.clone(),
      media_provider: self.media_provider.clone(),
      svg_id_defs: self.svg_id_defs.clone(),
      svg_id_defs_raw: self.svg_id_defs_raw.clone(),
      text_shape_cache: Arc::clone(&self.text_shape_cache),
      trace: self.trace.clone(),
      scroll_state: self.scroll_state.clone(),
      max_iframe_depth: self.max_iframe_depth,
      gradient_cache: self.gradient_cache.clone(),
      gradient_stats: GradientStats::default(),
      diagnostics_enabled: self.diagnostics_enabled,
    };

    fn paint_text_commands_recursive(
      painter: &mut Painter,
      commands: &[DisplayCommand],
      painted_any: &mut bool,
    ) {
        for cmd in commands {
          match cmd {
           DisplayCommand::Text {
             rect,
             baseline_offset,
             text,
             runs,
             style,
             ..
           } => {
              if text.is_empty() {
                continue;
              }

            let inline_vertical = matches!(
              style.writing_mode,
              crate::style::types::WritingMode::VerticalRl
                | crate::style::types::WritingMode::VerticalLr
                | crate::style::types::WritingMode::SidewaysRl
                | crate::style::types::WritingMode::SidewaysLr
            );
            let (block_baseline, inline_start) = if inline_vertical {
              (rect.x() + baseline_offset, rect.y())
            } else {
              (rect.y() + baseline_offset, rect.x())
            };

            let shaped_runs: Option<Arc<Vec<ShapedRun>>> = runs.clone().or_else(|| {
              painter
                .shaper
                .shape_arc(text, style, &painter.font_ctx)
                .ok()
            });
            let Some(shaped_runs) = shaped_runs else {
              continue;
            };
            if shaped_runs.is_empty() {
              continue;
            }

            *painted_any = true;
            if inline_vertical {
              painter.paint_shaped_runs_vertical(
                &shaped_runs,
                block_baseline,
                inline_start,
                Rgba::WHITE,
                None,
                None,
              );
            } else {
              painter.paint_shaped_runs(
                &shaped_runs,
                inline_start,
                block_baseline,
                Rgba::WHITE,
                None,
                None,
              );
            }
          }
          DisplayCommand::StackingContext {
            opacity, commands, ..
          } => {
            if *opacity > 0.0 {
              paint_text_commands_recursive(painter, commands, painted_any);
            }
          }
          _ => {}
        }
      }
    }

    let mut painted_any = false;
    paint_text_commands_recursive(&mut painter, commands, &mut painted_any);
    if !painted_any {
      return Ok(None);
    }

    Ok(Some(Mask::from_pixmap(
      painter.pixmap.as_ref(),
      tiny_skia::MaskType::Alpha,
    )))
  }

  fn render_mask(
    &mut self,
    style: &ComputedStyle,
    css_bounds: Rect,
    layer_bounds: Rect,
    device_size: (u32, u32),
    text_clip: Option<&Mask>,
  ) -> RenderResult<Option<RenderedMask>> {
    let viewport = (self.css_width, self.css_height);
    let rects = background_rects(
      css_bounds.x(),
      css_bounds.y(),
      css_bounds.width(),
      css_bounds.height(),
      style,
      Some(viewport),
    );
    let mut combined: Option<Mask> = None;
    let mut combined_bounds: Option<ClipMaskDirtyRect> = None;
    let canvas_clip = layer_bounds;

    struct CssMaskScratchGuard {
      scratch: CssMaskScratch,
    }

    impl CssMaskScratchGuard {
      fn take() -> Self {
        let scratch = CSS_MASK_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        Self { scratch }
      }
    }

    impl Drop for CssMaskScratchGuard {
      fn drop(&mut self) {
        let scratch = std::mem::take(&mut self.scratch);
        CSS_MASK_SCRATCH.with(|cell| {
          *cell.borrow_mut() = scratch;
        });
      }
    }

    let mut css_mask_scratch = CssMaskScratchGuard::take();

    if let Some(existing) = css_mask_scratch.scratch.mask.as_ref() {
      if existing.width() != device_size.0 || existing.height() != device_size.1 {
        css_mask_scratch.scratch.mask = None;
        css_mask_scratch.scratch.last_dirty = None;
      }
    }
    if let Some(prev_dirty) = css_mask_scratch.scratch.last_dirty {
      if let Some(mask) = css_mask_scratch.scratch.mask.as_mut() {
        clear_mask_rect(mask, prev_dirty)?;
      }
      css_mask_scratch.scratch.last_dirty = None;
    }

    for layer in style.mask_layers.iter().rev() {
      let Some(image) = &layer.image else { continue };

      let origin_rect_css = match layer.origin {
        MaskOrigin::BorderBox => rects.border,
        MaskOrigin::PaddingBox => rects.padding,
        MaskOrigin::ContentBox | MaskOrigin::Text => rects.content,
      };
      let clip_rect_css = match layer.clip {
        MaskClip::BorderBox => rects.border,
        MaskClip::PaddingBox => rects.padding,
        MaskClip::ContentBox | MaskClip::Text => rects.content,
        MaskClip::NoClip => canvas_clip,
      };
      if origin_rect_css.width() <= 0.0
        || origin_rect_css.height() <= 0.0
        || clip_rect_css.width() <= 0.0
        || clip_rect_css.height() <= 0.0
      {
        continue;
      }

      let mut dummy = BackgroundLayer::default();
      dummy.size = layer.size;

      let mut resolved_mode = layer.mode;
      let mut img_w = 0.0f32;
      let mut img_h = 0.0f32;
      let mut intrinsic_ratio: Option<f32> = None;
      let mut tile_pixmap: Option<Arc<Pixmap>> = None;

      enum MaskUrlTile {
        SvgFragment {
          id: String,
          svg: String,
          render_w: u32,
          render_h: u32,
        },
        Image {
          resolved_src: String,
          image: Arc<crate::image_loader::CachedImage>,
        },
      }

      let url_tile = match image {
        BackgroundImage::Url(src) => {
          let trimmed = src.url.trim_matches(|c: char| {
            matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
          });
          if let Some(id) = trimmed.strip_prefix('#').filter(|id| !id.is_empty()) {
            let Some(defs) = self.svg_id_defs.as_ref() else {
              continue;
            };
            if !defs.contains_key(id) {
              continue;
            }

            let view_w = css_bounds.width().ceil().max(1.0) as u32;
            let view_h = css_bounds.height().ceil().max(1.0) as u32;
            img_w = view_w as f32;
            img_h = view_h as f32;
            let Some(svg) =
              crate::paint::svg_mask_image::inline_svg_for_mask_id(defs, id, view_w, view_h)
            else {
              continue;
            };

            // Rasterize the SVG mask at device resolution so the mask tile isn't later upscaled by
            // `device_pixel_ratio`.
            let render_w = ((view_w as f32) * self.scale).ceil().max(1.0) as u32;
            let render_h = ((view_h as f32) * self.scale).ceil().max(1.0) as u32;

            // The synthesized SVG always produces a white image with mask coverage in the alpha
            // channel, so interpret it as an alpha mask.
            resolved_mode = MaskMode::Alpha;
            Some(MaskUrlTile::SvgFragment {
              id: id.to_string(),
              svg,
              render_w,
              render_h,
            })
          } else {
            let resolved_src = self.image_cache.resolve_url(&src.url);
            let image = match self.image_cache.load(&resolved_src) {
              Ok(img) => img,
              Err(_) => continue,
            };
            let orientation = style.image_orientation.resolve(image.orientation, true);
            intrinsic_ratio = image.intrinsic_ratio(orientation);
            let (w, h) =
              image.css_natural_dimensions(
                orientation,
                &style.image_resolution,
                self.scale,
                src.override_resolution,
              );
            img_w = w.unwrap_or(0.0);
            img_h = h.unwrap_or(0.0);

            resolved_mode = match layer.mode {
              MaskMode::MatchSource => {
                let trimmed = src.url.trim_start_matches(|c: char| {
                  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
                });
                if trimmed.starts_with('<') {
                  MaskMode::Alpha
                } else if image.is_vector || image.has_alpha {
                  MaskMode::Alpha
                } else {
                  MaskMode::Luminance
                }
              }
              other => other,
            };

            Some(MaskUrlTile::Image {
              resolved_src,
              image: Arc::clone(&image),
            })
          }
        }
        _ => None,
      };

      let (mut tile_w, mut tile_h) = compute_background_size(
        &dummy,
        style.font_size,
        style.root_font_size,
        viewport,
        origin_rect_css.width(),
        origin_rect_css.height(),
        img_w,
        img_h,
        intrinsic_ratio,
      );
      if tile_w <= 0.0 || tile_h <= 0.0 {
        continue;
      }

      let mut rounded_x = false;
      let mut rounded_y = false;
      if layer.repeat.x == BackgroundRepeatKeyword::Round {
        tile_w = round_tile_length(origin_rect_css.width(), tile_w);
        rounded_x = true;
      }
      if layer.repeat.y == BackgroundRepeatKeyword::Round {
        tile_h = round_tile_length(origin_rect_css.height(), tile_h);
        rounded_y = true;
      }
      if rounded_x ^ rounded_y
        && matches!(
          layer.size,
          BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto)
        )
      {
        if let Some(aspect) = intrinsic_ratio.filter(|r| r.is_finite() && *r > 0.0) {
          if rounded_x {
            tile_h = tile_w / aspect;
          } else {
            tile_w = tile_h * aspect;
          }
        }
      }

      let (offset_x, offset_y) = resolve_background_offset(
        layer.position,
        origin_rect_css.width(),
        origin_rect_css.height(),
        tile_w,
        tile_h,
        style.font_size,
        style.root_font_size,
        viewport,
      );

      let positions_x = tile_positions(
        layer.repeat.x,
        origin_rect_css.x(),
        origin_rect_css.width(),
        tile_w,
        offset_x,
        clip_rect_css.min_x(),
        clip_rect_css.max_x(),
      );
      let positions_y = tile_positions(
        layer.repeat.y,
        origin_rect_css.y(),
        origin_rect_css.height(),
        tile_h,
        offset_y,
        clip_rect_css.min_y(),
        clip_rect_css.max_y(),
      );

      let pixmap_w = tile_w.ceil().max(1.0) as u32;
      let pixmap_h = tile_h.ceil().max(1.0) as u32;

      match image {
        BackgroundImage::LinearGradient { .. }
        | BackgroundImage::RepeatingLinearGradient { .. }
        | BackgroundImage::RadialGradient { .. }
        | BackgroundImage::RepeatingRadialGradient { .. }
        | BackgroundImage::ConicGradient { .. }
        | BackgroundImage::RepeatingConicGradient { .. } => {
          tile_pixmap = self
            .render_generated_image(image, style, pixmap_w, pixmap_h)
            .map(Arc::new);
        }
        BackgroundImage::Url(_) => match url_tile {
          Some(MaskUrlTile::SvgFragment {
            id,
            svg,
            render_w,
            render_h,
          }) => {
            let cache_key = format!("svg-mask-fragment:{}", id);
            tile_pixmap = match self
              .image_cache
              .render_svg_pixmap_at_size(&svg, render_w, render_h, &cache_key, self.scale)
            {
              Ok(pixmap) => Some(pixmap),
              Err(_) => None,
            };
          }
          Some(MaskUrlTile::Image {
            resolved_src,
            image,
          }) => {
            let orientation = style.image_orientation.resolve(image.orientation, true);
            if image.is_vector {
              let Some(svg) = &image.svg_content else {
                continue;
              };
              let (render_w, render_h) = image.oriented_dimensions(orientation);
              if render_w == 0 || render_h == 0 {
                continue;
              }
              tile_pixmap = match self.image_cache.render_svg_pixmap_at_size(
                svg,
                render_w,
                render_h,
                &resolved_src,
                self.scale,
              ) {
                Ok(pixmap) => Some(pixmap),
                Err(_) => None,
              };
            } else {
              tile_pixmap =
                match self
                  .image_cache
                  .load_raster_pixmap(&resolved_src, orientation, true)
                {
                  Ok(Some(pixmap)) => Some(pixmap),
                  _ => None,
                };
            }
          }
          None => {}
        },
        BackgroundImage::None => {}
      }

      let Some(tile) = tile_pixmap else { continue };
      let mut mask_tile = tile;
      if matches!(resolved_mode, MaskMode::Luminance) {
        let Some(converted) = mask_tile_from_image(mask_tile.as_ref(), resolved_mode)? else {
          continue;
        };
        mask_tile = Arc::new(converted);
      }

      let device_clip = self.device_rect(clip_rect_css);
      let dirty = clip_mask_dirty_bounds(device_clip, device_size.0, device_size.1);

      let apply_text_clip = matches!(layer.clip, MaskClip::Text);
      let layer_result = MASK_LAYER_PIXMAP_SCRATCH.with(|cell| -> RenderResult<Option<()>> {
        let mut scratch = cell.borrow_mut();

        let replace = match scratch.pixmap.as_ref() {
          Some(existing) => existing.width() != device_size.0 || existing.height() != device_size.1,
          None => true,
        };
        if replace {
          scratch.pixmap = new_pixmap(device_size.0, device_size.1);
          scratch.last_dirty = None;
        }

        let prev_dirty = scratch.last_dirty;

        let Some(mask_pixmap) = scratch.pixmap.as_mut() else {
          return Ok(None);
        };

        // The scratch pixmap is reused between layers/calls, so ensure the pixels that might
        // contain stale data are cleared back to transparent before painting the new layer.
        let clear = match (prev_dirty, dirty) {
          (Some(prev), Some(curr)) => ClipMaskDirtyRect {
            x0: prev.x0.min(curr.x0),
            y0: prev.y0.min(curr.y0),
            x1: prev.x1.max(curr.x1),
            y1: prev.y1.max(curr.y1),
          },
          (Some(prev), None) => prev,
          (None, Some(curr)) => curr,
          (None, None) => ClipMaskDirtyRect {
            x0: 0,
            y0: 0,
            x1: device_size.0,
            y1: device_size.1,
          },
        };
        clear_pixmap_rect_rgba(mask_pixmap, clear)?;

        if dirty.is_some() && !(apply_text_clip && text_clip.is_none()) {
          let mut deadline_counter = 0usize;
          for ty in positions_y.iter().copied() {
            for tx in positions_x.iter().copied() {
              check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
              paint_mask_tile(
                mask_pixmap,
                mask_tile.as_ref(),
                tx,
                ty,
                tile_w,
                tile_h,
                clip_rect_css,
                self.origin_offset_css,
                self.scale,
              );
            }
          }
        }

        if apply_text_clip {
          if let (Some(text_mask), Some(dirty)) = (text_clip, dirty) {
            apply_text_clip_mask_to_pixmap_alpha(mask_pixmap, text_mask, dirty)?;
          }
        }

        if combined.is_none() {
          let mut mask = if let Some(mask) = css_mask_scratch.scratch.mask.take() {
            mask
          } else {
            let Some(mut mask) = Mask::new(device_size.0, device_size.1) else {
              return Ok(None);
            };
            mask.data_mut().fill(0);
            mask
          };
          if let Some(dirty) = dirty {
            copy_pixmap_alpha_to_mask(&mut mask, mask_pixmap, dirty)?;
          }
          combined_bounds = dirty;
          combined = Some(mask);
        } else if let Some(dest) = combined.as_mut() {
          let op = layer.composite;
          match dirty {
            None => match op {
              MaskComposite::Add | MaskComposite::Exclude => {}
              MaskComposite::Intersect | MaskComposite::Subtract => {
                dest.data_mut().fill(0);
                combined_bounds = None;
              }
            },
            Some(dirty) => {
              let prev_bounds = combined_bounds;

              if matches!(op, MaskComposite::Intersect | MaskComposite::Subtract) {
                if let Some(bounds) = prev_bounds {
                  hard_clip_mask_outside_rect_within_bounds(dest, dirty, bounds)?;
                }
              }

              let process = match op {
                MaskComposite::Intersect => {
                  prev_bounds.and_then(|bounds| dirty_intersection(bounds, dirty))
                }
                _ => Some(dirty),
              };
              if let Some(process) = process {
                apply_mask_composite_from_pixmap_alpha(dest, mask_pixmap, process, op)?;
              }

              combined_bounds = match op {
                MaskComposite::Add | MaskComposite::Exclude => Some(match prev_bounds {
                  Some(prev) => dirty_union(prev, dirty),
                  None => dirty,
                }),
                MaskComposite::Intersect => {
                  prev_bounds.and_then(|bounds| dirty_intersection(bounds, dirty))
                }
                MaskComposite::Subtract => Some(dirty),
              };
            }
          }
        }

        scratch.last_dirty = dirty;
        Ok(Some(()))
      })?;
      if layer_result.is_none() {
        return Ok(None);
      }
    }

    Ok(match combined {
      Some(mask) => Some(RenderedMask {
        mask: Some(mask),
        dirty: combined_bounds,
      }),
      None => None,
    })
  }

  fn render_mask_border(
    &mut self,
    style: &ComputedStyle,
    css_bounds: Rect,
    layer_bounds: Rect,
    device_size: (u32, u32),
  ) -> RenderResult<Option<RenderedMask>> {
    let BorderImageSource::Image(bg) = &style.mask_border.source else {
      return Ok(None);
    };
    let viewport = (self.css_width, self.css_height);
    let rect_css = css_bounds;

    // Resolve used border widths for `<number>` values in `mask-border-width/outset`.
    let percentage_base = rect_css.width().max(0.0);
    let border_widths = BorderWidths {
      top: resolve_length_for_paint(
        &style.used_border_top_width(),
        style.font_size,
        style.root_font_size,
        percentage_base,
        viewport,
      )
      .max(0.0),
      right: resolve_length_for_paint(
        &style.used_border_right_width(),
        style.font_size,
        style.root_font_size,
        percentage_base,
        viewport,
      )
      .max(0.0),
      bottom: resolve_length_for_paint(
        &style.used_border_bottom_width(),
        style.font_size,
        style.root_font_size,
        percentage_base,
        viewport,
      )
      .max(0.0),
      left: resolve_length_for_paint(
        &style.used_border_left_width(),
        style.font_size,
        style.root_font_size,
        percentage_base,
        viewport,
      )
      .max(0.0),
    };

    // Use the same mask scratch used for `mask-image`, but render into it via the border-image
    // nine-slice tiling algorithm.
    struct CssMaskScratchGuard {
      scratch: CssMaskScratch,
    }

    impl CssMaskScratchGuard {
      fn take() -> Self {
        let scratch = CSS_MASK_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        Self { scratch }
      }
    }

    impl Drop for CssMaskScratchGuard {
      fn drop(&mut self) {
        let scratch = std::mem::take(&mut self.scratch);
        CSS_MASK_SCRATCH.with(|cell| {
          *cell.borrow_mut() = scratch;
        });
      }
    }

    let mut css_mask_scratch = CssMaskScratchGuard::take();
    if let Some(existing) = css_mask_scratch.scratch.mask.as_ref() {
      if existing.width() != device_size.0 || existing.height() != device_size.1 {
        css_mask_scratch.scratch.mask = None;
        css_mask_scratch.scratch.last_dirty = None;
      }
    }
    if let Some(prev_dirty) = css_mask_scratch.scratch.last_dirty {
      if let Some(mask) = css_mask_scratch.scratch.mask.as_mut() {
        clear_mask_rect(mask, prev_dirty)?;
      }
      css_mask_scratch.scratch.last_dirty = None;
    }

    let mut mask = if let Some(mask) = css_mask_scratch.scratch.mask.take() {
      mask
    } else {
      let Some(mut mask) = Mask::new(device_size.0, device_size.1) else {
        return Ok(None);
      };
      mask.data_mut().fill(0);
      mask
    };

    // Resolve the source image and resolve `mask-border-mode: match-source` while we still have
    // access to the original image metadata.
    let (source_pixmap, img_w, img_h, target_widths, outer_rect_css, mode) = match bg.as_ref() {
      BackgroundImage::Url(src) => {
        let resolved_src = self.image_cache.resolve_url(&src.url);
        let image = match self.image_cache.load(&resolved_src) {
          Ok(img) => img,
          Err(_) => return Ok(None),
        };
        let resolved_mode = match style.mask_border.mode {
          MaskBorderMode::MatchSource => {
            if image.is_vector || image.has_alpha {
              MaskMode::Alpha
            } else {
              MaskMode::Luminance
            }
          }
          MaskBorderMode::Alpha => MaskMode::Alpha,
          MaskBorderMode::Luminance => MaskMode::Luminance,
        };
        let orientation = style.image_orientation.resolve(image.orientation, true);
        let intrinsic_css_size =
          image.css_dimensions(orientation, &style.image_resolution, self.scale, src.override_resolution);

        let pixmap = if image.is_vector {
          let Some(svg) = &image.svg_content else {
            return Ok(None);
          };
          let (render_w, render_h) = image.oriented_dimensions(orientation);
          if render_w == 0 || render_h == 0 {
            return Ok(None);
          }
          match self.image_cache.render_svg_pixmap_at_size(
            svg,
            render_w,
            render_h,
            &resolved_src,
            self.scale,
          ) {
            Ok(pixmap) => pixmap,
            Err(_) => return Ok(None),
          }
        } else {
          match self
            .image_cache
            .load_raster_pixmap(&resolved_src, orientation, true)
          {
            Ok(Some(pixmap)) => pixmap,
            _ => return Ok(None),
          }
        };
        let img_w = pixmap.width();
        let img_h = pixmap.height();
        if img_w == 0 || img_h == 0 {
          return Ok(None);
        }

        let slice_top_px = resolve_slice_value(style.mask_border.slice.top, img_h);
        let slice_right_px = resolve_slice_value(style.mask_border.slice.right, img_w);
        let slice_bottom_px = resolve_slice_value(style.mask_border.slice.bottom, img_h);
        let slice_left_px = resolve_slice_value(style.mask_border.slice.left, img_w);
        let auto_slice_css = intrinsic_css_size.and_then(|(css_w, css_h)| {
          if css_w.is_finite()
            && css_h.is_finite()
            && css_w > 0.0
            && css_h > 0.0
            && img_w > 0
            && img_h > 0
          {
            Some(BorderWidths {
              top: slice_top_px * (css_h / img_h as f32),
              right: slice_right_px * (css_w / img_w as f32),
              bottom: slice_bottom_px * (css_h / img_h as f32),
              left: slice_left_px * (css_w / img_w as f32),
            })
          } else {
            None
          }
        });

        let resolve_width = |value: BorderImageWidthValue,
                             border: f32,
                             axis: f32,
                             auto: Option<f32>|
         -> f32 {
          match value {
            BorderImageWidthValue::Auto => auto.unwrap_or(border).max(0.0),
            BorderImageWidthValue::Number(n) => (n * border).max(0.0),
            BorderImageWidthValue::Length(len) => {
              resolve_length_for_paint(&len, style.font_size, style.root_font_size, axis, viewport)
                .max(0.0)
            }
            BorderImageWidthValue::Percentage(p) => ((p / 100.0) * axis).max(0.0),
          }
        };
        let target_widths = BorderWidths {
          top: resolve_width(
            style.mask_border.width.top,
            border_widths.top,
            rect_css.height(),
            auto_slice_css.map(|w| w.top),
          ),
          right: resolve_width(
            style.mask_border.width.right,
            border_widths.right,
            rect_css.width(),
            auto_slice_css.map(|w| w.right),
          ),
          bottom: resolve_width(
            style.mask_border.width.bottom,
            border_widths.bottom,
            rect_css.height(),
            auto_slice_css.map(|w| w.bottom),
          ),
          left: resolve_width(
            style.mask_border.width.left,
            border_widths.left,
            rect_css.width(),
            auto_slice_css.map(|w| w.left),
          ),
        };
        let outsets = resolve_border_image_outset(
          &style.mask_border.outset,
          target_widths,
          style.font_size,
          style.root_font_size,
          viewport,
        );
        let outer_rect_css = Rect::from_xywh(
          rect_css.x() - outsets.left,
          rect_css.y() - outsets.top,
          rect_css.width() + outsets.left + outsets.right,
          rect_css.height() + outsets.top + outsets.bottom,
        );
        (
          pixmap,
          img_w,
          img_h,
          target_widths,
          outer_rect_css,
          resolved_mode,
        )
      }
      BackgroundImage::LinearGradient { .. }
      | BackgroundImage::RepeatingLinearGradient { .. }
      | BackgroundImage::RadialGradient { .. }
      | BackgroundImage::RepeatingRadialGradient { .. }
      | BackgroundImage::ConicGradient { .. }
      | BackgroundImage::RepeatingConicGradient { .. } => {
        // For generated images we don't have intrinsic CSS sizing information, so treat `auto`
        // widths as the used border widths (matching current border-image behavior).
        let resolved_mode = match style.mask_border.mode {
          MaskBorderMode::Luminance => MaskMode::Luminance,
          MaskBorderMode::Alpha | MaskBorderMode::MatchSource => MaskMode::Alpha,
        };
        let resolve_width = |value: BorderImageWidthValue, border: f32, axis: f32| -> f32 {
          match value {
            BorderImageWidthValue::Auto => border,
            BorderImageWidthValue::Number(n) => (n * border).max(0.0),
            BorderImageWidthValue::Length(len) => {
              resolve_length_for_paint(&len, style.font_size, style.root_font_size, axis, viewport)
                .max(0.0)
            }
            BorderImageWidthValue::Percentage(p) => ((p / 100.0) * axis).max(0.0),
          }
        };
        let target_widths = BorderWidths {
          top: resolve_width(
            style.mask_border.width.top,
            border_widths.top,
            rect_css.height(),
          ),
          right: resolve_width(
            style.mask_border.width.right,
            border_widths.right,
            rect_css.width(),
          ),
          bottom: resolve_width(
            style.mask_border.width.bottom,
            border_widths.bottom,
            rect_css.height(),
          ),
          left: resolve_width(
            style.mask_border.width.left,
            border_widths.left,
            rect_css.width(),
          ),
        };
        let outsets = resolve_border_image_outset(
          &style.mask_border.outset,
          target_widths,
          style.font_size,
          style.root_font_size,
          viewport,
        );
        let outer_rect_css = Rect::from_xywh(
          rect_css.x() - outsets.left,
          rect_css.y() - outsets.top,
          rect_css.width() + outsets.left + outsets.right,
          rect_css.height() + outsets.top + outsets.bottom,
        );
        if outer_rect_css.width() <= 0.0 || outer_rect_css.height() <= 0.0 {
          return Ok(None);
        }
        let img_w = outer_rect_css.width().max(1.0).round() as u32;
        let img_h = outer_rect_css.height().max(1.0).round() as u32;
        let Some(pixmap) = self.render_generated_image(bg, style, img_w, img_h) else {
          return Ok(None);
        };
        let pixmap = Arc::new(pixmap);
        (
          pixmap,
          img_w,
          img_h,
          target_widths,
          outer_rect_css,
          resolved_mode,
        )
      }
      BackgroundImage::None => return Ok(None),
    };

    let slice_top_px = resolve_slice_value(style.mask_border.slice.top, img_h);
    let slice_right_px = resolve_slice_value(style.mask_border.slice.right, img_w);
    let slice_bottom_px = resolve_slice_value(style.mask_border.slice.bottom, img_h);
    let slice_left_px = resolve_slice_value(style.mask_border.slice.left, img_w);

    let inner_rect_css = Rect::from_xywh(
      outer_rect_css.x() + target_widths.left,
      outer_rect_css.y() + target_widths.top,
      outer_rect_css.width() - target_widths.left - target_widths.right,
      outer_rect_css.height() - target_widths.top - target_widths.bottom,
    );

    let Some(clip_rect_css) = outer_rect_css.intersection(layer_bounds) else {
      // The border-mask area is entirely outside the stacking layer; applying the mask should
      // clear all painted pixels.
      mask.data_mut().fill(0);
      return Ok(Some(RenderedMask {
        mask: Some(mask),
        dirty: None,
      }));
    };
    if clip_rect_css.width() <= 0.0 || clip_rect_css.height() <= 0.0 {
      mask.data_mut().fill(0);
      return Ok(Some(RenderedMask {
        mask: Some(mask),
        dirty: None,
      }));
    }

    let device_clip = self.device_rect(clip_rect_css);
    let dirty = clip_mask_dirty_bounds(device_clip, device_size.0, device_size.1);
    let Some(dirty) = dirty else {
      mask.data_mut().fill(0);
      return Ok(Some(RenderedMask {
        mask: Some(mask),
        dirty: None,
      }));
    };

    // Convert source pixels to an alpha mask when luminance mode is used.
    let converted_source = match mode {
      MaskMode::Luminance => {
        let Some(tile) = mask_tile_from_image(source_pixmap.as_ref(), mode)? else {
          return Ok(None);
        };
        Some(tile)
      }
      _ => None,
    };
    let source_for_paint = converted_source
      .as_ref()
      .unwrap_or_else(|| source_pixmap.as_ref());

    let sx0 = 0.0;
    let sx1 = slice_left_px.min(img_w as f32);
    let sx2 = (img_w as f32 - slice_right_px).max(sx1);
    let sx3 = img_w as f32;
    let sy0 = 0.0;
    let sy1 = slice_top_px.min(img_h as f32);
    let sy2 = (img_h as f32 - slice_bottom_px).max(sy1);
    let sy3 = img_h as f32;

    let (repeat_x, repeat_y) = style.mask_border.repeat;

    // Convert geometry into the local device-space coordinate system of the stacking layer.
    let clip_rect_device = self.device_rect(clip_rect_css);
    let outer_rect_device = self.device_rect(outer_rect_css);
    let inner_rect_device = self.device_rect(inner_rect_css);
    let target_widths_device = BorderWidths {
      top: target_widths.top * self.scale,
      right: target_widths.right * self.scale,
      bottom: target_widths.bottom * self.scale,
      left: target_widths.left * self.scale,
    };

    let layer_result = MASK_LAYER_PIXMAP_SCRATCH.with(|cell| -> RenderResult<Option<()>> {
      let mut scratch = cell.borrow_mut();
      let replace = match scratch.pixmap.as_ref() {
        Some(existing) => existing.width() != device_size.0 || existing.height() != device_size.1,
        None => true,
      };
      if replace {
        scratch.pixmap = new_pixmap(device_size.0, device_size.1);
        scratch.last_dirty = None;
      }
      let prev_dirty = scratch.last_dirty;
      let Some(mask_pixmap) = scratch.pixmap.as_mut() else {
        return Ok(None);
      };

      // Clear pixels that might contain stale data from a previous use of the scratch.
      let clear = prev_dirty.map_or(dirty, |prev| ClipMaskDirtyRect {
        x0: prev.x0.min(dirty.x0),
        y0: prev.y0.min(dirty.y0),
        x1: prev.x1.max(dirty.x1),
        y1: prev.y1.max(dirty.y1),
      });
      clear_pixmap_rect_rgba(mask_pixmap, clear)?;

      // `mask-border-slice` default behavior: when `fill` is not set, the center is treated as
      // fully opaque.
      if !style.mask_border.slice.fill
        && inner_rect_device.width() > 0.0
        && inner_rect_device.height() > 0.0
      {
        if let Some(inner_clip) = inner_rect_device.intersection(clip_rect_device) {
          if inner_clip.width() > 0.0 && inner_clip.height() > 0.0 {
            if let Some(rect) = SkiaRect::from_xywh(
              inner_clip.x(),
              inner_clip.y(),
              inner_clip.width(),
              inner_clip.height(),
            ) {
              let mut paint = Paint::default();
              paint.set_color_rgba8(255, 255, 255, 255);
              paint.anti_alias = false;
              mask_pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
          }
        }
      }

      // Corners.
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx0, sy0, sx1 - sx0, sy1 - sy0),
        Rect::from_xywh(
          outer_rect_device.x(),
          outer_rect_device.y(),
          target_widths_device.left,
          target_widths_device.top,
        ),
        BorderImageRepeat::Stretch,
        BorderImageRepeat::Stretch,
        clip_rect_device,
      );
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx2, sy0, sx3 - sx2, sy1 - sy0),
        Rect::from_xywh(
          outer_rect_device.x() + outer_rect_device.width() - target_widths_device.right,
          outer_rect_device.y(),
          target_widths_device.right,
          target_widths_device.top,
        ),
        BorderImageRepeat::Stretch,
        BorderImageRepeat::Stretch,
        clip_rect_device,
      );
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx0, sy2, sx1 - sx0, sy3 - sy2),
        Rect::from_xywh(
          outer_rect_device.x(),
          outer_rect_device.y() + outer_rect_device.height() - target_widths_device.bottom,
          target_widths_device.left,
          target_widths_device.bottom,
        ),
        BorderImageRepeat::Stretch,
        BorderImageRepeat::Stretch,
        clip_rect_device,
      );
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx2, sy2, sx3 - sx2, sy3 - sy2),
        Rect::from_xywh(
          outer_rect_device.x() + outer_rect_device.width() - target_widths_device.right,
          outer_rect_device.y() + outer_rect_device.height() - target_widths_device.bottom,
          target_widths_device.right,
          target_widths_device.bottom,
        ),
        BorderImageRepeat::Stretch,
        BorderImageRepeat::Stretch,
        clip_rect_device,
      );

      // Edges.
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx1, sy0, sx2 - sx1, sy1 - sy0),
        Rect::from_xywh(
          inner_rect_device.x(),
          outer_rect_device.y(),
          inner_rect_device.width(),
          target_widths_device.top,
        ),
        repeat_x,
        BorderImageRepeat::Stretch,
        clip_rect_device,
      );
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx1, sy2, sx2 - sx1, sy3 - sy2),
        Rect::from_xywh(
          inner_rect_device.x(),
          outer_rect_device.y() + outer_rect_device.height() - target_widths_device.bottom,
          inner_rect_device.width(),
          target_widths_device.bottom,
        ),
        repeat_x,
        BorderImageRepeat::Stretch,
        clip_rect_device,
      );
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx0, sy1, sx1 - sx0, sy2 - sy1),
        Rect::from_xywh(
          outer_rect_device.x(),
          inner_rect_device.y(),
          target_widths_device.left,
          inner_rect_device.height(),
        ),
        BorderImageRepeat::Stretch,
        repeat_y,
        clip_rect_device,
      );
      paint_mask_border_patch(
        mask_pixmap,
        source_for_paint,
        Rect::from_xywh(sx2, sy1, sx3 - sx2, sy2 - sy1),
        Rect::from_xywh(
          outer_rect_device.x() + outer_rect_device.width() - target_widths_device.right,
          inner_rect_device.y(),
          target_widths_device.right,
          inner_rect_device.height(),
        ),
        BorderImageRepeat::Stretch,
        repeat_y,
        clip_rect_device,
      );

      // Center.
      if style.mask_border.slice.fill {
        paint_mask_border_patch(
          mask_pixmap,
          source_for_paint,
          Rect::from_xywh(sx1, sy1, sx2 - sx1, sy2 - sy1),
          inner_rect_device,
          repeat_x,
          repeat_y,
          clip_rect_device,
        );
      }

      copy_pixmap_alpha_to_mask(&mut mask, mask_pixmap, dirty)?;
      scratch.last_dirty = Some(dirty);
      Ok(Some(()))
    })?;
    if layer_result.is_none() {
      return Ok(None);
    }

    Ok(Some(RenderedMask {
      mask: Some(mask),
      dirty: Some(dirty),
    }))
  }

  fn paint_background_with_text_clip(
    &mut self,
    rect: Rect,
    style: &ComputedStyle,
    text_clip: &[DisplayCommand],
    scroll_delta: Point,
  ) -> RenderResult<()> {
    check_active(RenderStage::Paint)?;

    let timer = self.diagnostics_enabled.then(Instant::now);
    let rects = background_rects(
      rect.x(),
      rect.y(),
      rect.width(),
      rect.height(),
      style,
      Some((self.css_width, self.css_height)),
    );
    let scaled_rects = self.scale_background_rects(&rects);

    let fallback_layer = BackgroundLayer::default();
    let color_clip_layer = style.background_layers.first().unwrap_or(&fallback_layer);
    let color_clip_rect = match color_clip_layer.clip {
      crate::style::types::BackgroundBox::BorderBox => scaled_rects.border,
      crate::style::types::BackgroundBox::PaddingBox => scaled_rects.padding,
      crate::style::types::BackgroundBox::ContentBox | crate::style::types::BackgroundBox::Text => {
        scaled_rects.content
      }
    };

    if color_clip_rect.width() <= 0.0 || color_clip_rect.height() <= 0.0 {
      if let Some(start) = timer {
        with_paint_diagnostics(|diag| {
          diag.background_ms += start.elapsed().as_secs_f64() * 1000.0;
        });
      }
      return Ok(());
    }

    let color_clip_radii = self.device_radii(resolve_clip_radii(
      style,
      &rects,
      color_clip_layer.clip,
      Some((self.css_width, self.css_height)),
    ));

    let wants_text_clip = style
      .background_layers
      .iter()
      .any(|layer| layer.clip == crate::style::types::BackgroundBox::Text);

    let mut text_clip_layer_origin_css = Point::new(0.0, 0.0);
    let mut text_clip_layer_x0 = 0i32;
    let mut text_clip_layer_y0 = 0i32;
    let mut text_clip_layer_w = 0u32;
    let mut text_clip_layer_h = 0u32;
    let mut text_mask: Option<Mask> = None;

    if wants_text_clip && !text_clip.is_empty() {
      let canvas_rect_css = if self.scale.is_finite() && self.scale.abs() > f32::EPSILON {
        Rect::from_xywh(
          self.origin_offset_css.x,
          self.origin_offset_css.y,
          self.pixmap.width() as f32 / self.scale,
          self.pixmap.height() as f32 / self.scale,
        )
      } else {
        Rect::from_xywh(0.0, 0.0, self.css_width, self.css_height)
      };
      if let Some(visible_content_css) = rects.content.intersection(canvas_rect_css) {
        if visible_content_css.width() > 0.0 && visible_content_css.height() > 0.0 {
          let device_rect = self.device_rect(visible_content_css);
          if device_rect.width().is_finite()
            && device_rect.height().is_finite()
            && device_rect.width() > 0.0
            && device_rect.height() > 0.0
          {
            let canvas_w = self.pixmap.width() as i32;
            let canvas_h = self.pixmap.height() as i32;
            if canvas_w > 0 && canvas_h > 0 {
              const MARGIN_PX: i32 = 2;
              let mut x0 = device_rect.x().floor() as i32 - MARGIN_PX;
              let mut y0 = device_rect.y().floor() as i32 - MARGIN_PX;
              let mut x1 = (device_rect.x() + device_rect.width()).ceil() as i32 + MARGIN_PX;
              let mut y1 = (device_rect.y() + device_rect.height()).ceil() as i32 + MARGIN_PX;

              x0 = x0.clamp(0, canvas_w);
              y0 = y0.clamp(0, canvas_h);
              x1 = x1.clamp(0, canvas_w);
              y1 = y1.clamp(0, canvas_h);

              if x1 > x0 && y1 > y0 {
                text_clip_layer_x0 = x0;
                text_clip_layer_y0 = y0;
                text_clip_layer_w = (x1 - x0) as u32;
                text_clip_layer_h = (y1 - y0) as u32;
                text_clip_layer_origin_css = Point::new(
                  self.origin_offset_css.x + (x0 as f32) / self.scale,
                  self.origin_offset_css.y + (y0 as f32) / self.scale,
                );

                if let Some(dummy_pixmap) = new_pixmap(1, 1) {
                  let mask_ctx = Painter {
                    pixmap: dummy_pixmap,
                    scale: self.scale,
                    origin_offset_css: text_clip_layer_origin_css,
                    css_width: self.css_width,
                    css_height: self.css_height,
                    background: Rgba::new(0, 0, 0, 0.0),
                    shaper: ShapingPipeline::new(),
                    font_ctx: self.font_ctx.clone(),
                    image_cache: self.image_cache.clone(),
                    media_provider: self.media_provider.clone(),
                    svg_id_defs: self.svg_id_defs.clone(),
                    svg_id_defs_raw: self.svg_id_defs_raw.clone(),
                    text_shape_cache: Arc::clone(&self.text_shape_cache),
                    trace: self.trace.clone(),
                    scroll_state: self.scroll_state.clone(),
                    max_iframe_depth: self.max_iframe_depth,
                    gradient_cache: self.gradient_cache.clone(),
                    gradient_stats: GradientStats::default(),
                    diagnostics_enabled: self.diagnostics_enabled,
                  };
                  text_mask = mask_ctx.build_text_clip_mask_from_commands(
                    text_clip,
                    (text_clip_layer_w, text_clip_layer_h),
                  )?;
                }
              }
            }
          }
        }
      }
    }

    if style.background_color.alpha_u8() > 0 {
      let clips_text = color_clip_layer.clip == crate::style::types::BackgroundBox::Text;
      if clips_text {
        if let Some(text_mask) = text_mask.as_ref() {
          self.paint_text_clipped_offscreen_layer(
            text_clip_layer_x0,
            text_clip_layer_y0,
            text_clip_layer_origin_css,
            text_clip_layer_w,
            text_clip_layer_h,
            text_mask,
            MixBlendMode::Normal,
            |painter| {
              let scaled_rects = painter.scale_background_rects(&rects);
              let color_clip_rect = match color_clip_layer.clip {
                crate::style::types::BackgroundBox::BorderBox => scaled_rects.border,
                crate::style::types::BackgroundBox::PaddingBox => scaled_rects.padding,
                crate::style::types::BackgroundBox::ContentBox
                | crate::style::types::BackgroundBox::Text => scaled_rects.content,
              };
              let radii = painter.device_radii(resolve_clip_radii(
                style,
                &rects,
                color_clip_layer.clip,
                Some((painter.css_width, painter.css_height)),
              ));
              let _ = fill_rounded_rect(
                &mut painter.pixmap,
                color_clip_rect.x(),
                color_clip_rect.y(),
                color_clip_rect.width(),
                color_clip_rect.height(),
                &radii,
                style.background_color,
              );
            },
          )?;
        }
      } else {
        let _ = fill_rounded_rect(
          &mut self.pixmap,
          color_clip_rect.x(),
          color_clip_rect.y(),
          color_clip_rect.width(),
          color_clip_rect.height(),
          &color_clip_radii,
          style.background_color,
        );
      }
    }

    for layer in style.background_layers.iter().rev() {
      let Some(image) = &layer.image else { continue };
      let clips_text = layer.clip == crate::style::types::BackgroundBox::Text;
      if clips_text {
        if let Some(text_mask) = text_mask.as_ref() {
          let mut layer_src = layer.clone();
          layer_src.blend_mode = MixBlendMode::Normal;
          let blend_mode = layer.blend_mode;
          self.paint_text_clipped_offscreen_layer(
            text_clip_layer_x0,
            text_clip_layer_y0,
            text_clip_layer_origin_css,
            text_clip_layer_w,
            text_clip_layer_h,
            text_mask,
            blend_mode,
            |painter| {
              painter.paint_background_image_layer(&rects, style, &layer_src, image, scroll_delta);
            },
          )?;
        }
      } else {
        self.paint_background_image_layer(&rects, style, layer, image, scroll_delta);
      }
    }

    if let Some(start) = timer {
      with_paint_diagnostics(|diag| {
        diag.background_ms += start.elapsed().as_secs_f64() * 1000.0;
      });
    }

    Ok(())
  }

  fn composite_pixmap_with_blend_mode(
    &mut self,
    x0: i32,
    y0: i32,
    pixmap: &Pixmap,
    mode: MixBlendMode,
  ) {
    if is_hsl_blend(mode) {
      composite_hsl_layer_offset(&mut self.pixmap, pixmap, 1.0, mode, x0, y0);
      return;
    }

    let mut paint = PixmapPaint::default();
    paint.opacity = 1.0;
    paint.blend_mode = map_blend_mode(mode);
    if paint.blend_mode == SkiaBlendMode::Plus {
      draw_pixmap_with_plus_blend(
        &mut self.pixmap,
        x0,
        y0,
        pixmap.as_ref(),
        paint.opacity,
        paint.quality,
        Transform::identity(),
        None,
      );
    } else {
      self
        .pixmap
        .draw_pixmap(x0, y0, pixmap.as_ref(), &paint, Transform::identity(), None);
    }
  }

  fn paint_text_clipped_offscreen_layer<F>(
    &mut self,
    x0: i32,
    y0: i32,
    origin_offset_css: Point,
    layer_w: u32,
    layer_h: u32,
    text_mask: &Mask,
    blend_mode: MixBlendMode,
    paint: F,
  ) -> RenderResult<()>
  where
    F: FnOnce(&mut Painter),
  {
    check_active(RenderStage::Paint)?;
    if layer_w == 0 || layer_h == 0 {
      return Ok(());
    }
    let Some(mut layer_pixmap) = new_pixmap(layer_w, layer_h) else {
      return Ok(());
    };
    if self.diagnostics_enabled {
      record_layer_allocation(layer_w, layer_h);
    }
    layer_pixmap.data_mut().fill(0);

    let mut layer_painter = Painter {
      pixmap: layer_pixmap,
      scale: self.scale,
      origin_offset_css,
      css_width: self.css_width,
      css_height: self.css_height,
      background: Rgba::new(0, 0, 0, 0.0),
      shaper: ShapingPipeline::new(),
      font_ctx: self.font_ctx.clone(),
      image_cache: self.image_cache.clone(),
      media_provider: self.media_provider.clone(),
      svg_id_defs: self.svg_id_defs.clone(),
      svg_id_defs_raw: self.svg_id_defs_raw.clone(),
      text_shape_cache: Arc::clone(&self.text_shape_cache),
      trace: self.trace.clone(),
      scroll_state: self.scroll_state.clone(),
      max_iframe_depth: self.max_iframe_depth,
      gradient_cache: self.gradient_cache.clone(),
      gradient_stats: GradientStats::default(),
      diagnostics_enabled: self.diagnostics_enabled,
    };

    paint(&mut layer_painter);

    apply_text_clip_mask_to_pixmap_alpha(
      &mut layer_painter.pixmap,
      text_mask,
      ClipMaskDirtyRect {
        x0: 0,
        y0: 0,
        x1: layer_w,
        y1: layer_h,
      },
    )?;

    self.gradient_stats.merge(&layer_painter.gradient_stats);
    self.composite_pixmap_with_blend_mode(x0, y0, &layer_painter.pixmap, blend_mode);
    Ok(())
  }

  /// Paints the background of a fragment
  fn paint_background(
    &mut self,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    style: &ComputedStyle,
    scroll_delta: Point,
  ) {
    let timer = self.diagnostics_enabled.then(Instant::now);
    let rects = background_rects(
      x,
      y,
      width,
      height,
      style,
      Some((self.css_width, self.css_height)),
    );
    let scaled_rects = self.scale_background_rects(&rects);
    let fallback_layer = BackgroundLayer::default();
    let color_clip_layer = style.background_layers.first().unwrap_or(&fallback_layer);
    let color_clip_rect = match color_clip_layer.clip {
      crate::style::types::BackgroundBox::BorderBox => scaled_rects.border,
      crate::style::types::BackgroundBox::PaddingBox => scaled_rects.padding,
      crate::style::types::BackgroundBox::ContentBox | crate::style::types::BackgroundBox::Text => {
        scaled_rects.content
      }
    };

    if color_clip_rect.width() <= 0.0 || color_clip_rect.height() <= 0.0 {
      if let Some(start) = timer {
        with_paint_diagnostics(|diag| {
          diag.background_ms += start.elapsed().as_secs_f64() * 1000.0;
        });
      }
      return;
    }

    let color_clip_radii = self.device_radii(resolve_clip_radii(
      style,
      &rects,
      color_clip_layer.clip,
      Some((self.css_width, self.css_height)),
    ));

    if style.background_color.alpha_u8() > 0 {
      let _ = fill_rounded_rect(
        &mut self.pixmap,
        color_clip_rect.x(),
        color_clip_rect.y(),
        color_clip_rect.width(),
        color_clip_rect.height(),
        &color_clip_radii,
        style.background_color,
      );
    }

    for layer in style.background_layers.iter().rev() {
      if let Some(image) = &layer.image {
        self.paint_background_image_layer(&rects, style, layer, image, scroll_delta);
      }
    }
    if let Some(start) = timer {
      with_paint_diagnostics(|diag| {
        diag.background_ms += start.elapsed().as_secs_f64() * 1000.0;
      });
    }
  }

  fn scale_background_rects(&self, rects: &BackgroundRects) -> BackgroundRects {
    BackgroundRects {
      border: self.device_rect(rects.border),
      padding: self.device_rect(rects.padding),
      content: self.device_rect(rects.content),
    }
  }

  fn paint_background_image_layer(
    &mut self,
    rects: &BackgroundRects,
    style: &ComputedStyle,
    layer: &BackgroundLayer,
    bg: &BackgroundImage,
    scroll_delta: Point,
  ) {
    let mut tiles_painted = 0u64;
    let record_tiles = self.diagnostics_enabled;
    let is_local = layer.attachment == BackgroundAttachment::Local;
    let clip_box = if is_local {
      match layer.clip {
        crate::style::types::BackgroundBox::ContentBox
        | crate::style::types::BackgroundBox::Text => {
          crate::style::types::BackgroundBox::ContentBox
        }
        _ => crate::style::types::BackgroundBox::PaddingBox,
      }
    } else {
      layer.clip
    };
    let clip_rect_css = match clip_box {
      crate::style::types::BackgroundBox::BorderBox => rects.border,
      crate::style::types::BackgroundBox::PaddingBox => rects.padding,
      crate::style::types::BackgroundBox::ContentBox | crate::style::types::BackgroundBox::Text => {
        rects.content
      }
    };
    let clip_radii = resolve_clip_radii(
      style,
      rects,
      clip_box,
      Some((self.css_width, self.css_height)),
    );
    let origin_rect_css = if layer.attachment == BackgroundAttachment::Fixed {
      Rect::from_xywh(0.0, 0.0, self.css_width, self.css_height)
    } else if is_local {
      match layer.origin {
        crate::style::types::BackgroundBox::ContentBox
        | crate::style::types::BackgroundBox::Text => rects.content,
        _ => rects.padding,
      }
    } else {
      match layer.origin {
        crate::style::types::BackgroundBox::BorderBox => rects.border,
        crate::style::types::BackgroundBox::PaddingBox => rects.padding,
        crate::style::types::BackgroundBox::ContentBox
        | crate::style::types::BackgroundBox::Text => rects.content,
      }
    };
    let origin_rect_css = if is_local {
      origin_rect_css.translate(scroll_delta)
    } else {
      origin_rect_css
    };

    if clip_rect_css.width() <= 0.0 || clip_rect_css.height() <= 0.0 {
      return;
    }
    if origin_rect_css.width() <= 0.0 || origin_rect_css.height() <= 0.0 {
      return;
    }

    // `css_width`/`css_height` represent the layout viewport size (used for vw/vh, fixed
    // backgrounds, etc.), but this painter may be rendering into an offscreen layer whose origin
    // is offset within the global CSS coordinate space. Cull background tiles against the actual
    // pixmap bounds in CSS coordinates so transformed/translated stacking-context layers paint the
    // correct visible region.
    let canvas_rect_css = if self.scale.is_finite() && self.scale.abs() > f32::EPSILON {
      Rect::from_xywh(
        self.origin_offset_css.x,
        self.origin_offset_css.y,
        self.pixmap.width() as f32 / self.scale,
        self.pixmap.height() as f32 / self.scale,
      )
    } else {
      Rect::from_xywh(0.0, 0.0, self.css_width, self.css_height)
    };
    let Some(visible_clip_css) = clip_rect_css.intersection(canvas_rect_css) else {
      return;
    };
    if visible_clip_css.width() <= 0.0 || visible_clip_css.height() <= 0.0 {
      return;
    }

    if !matches!(bg, BackgroundImage::None) {
      self.record_background_layer();
    }

    let clip_rect = self.device_rect(clip_rect_css);
    let clip_radii = self.device_radii(clip_radii);

    let mut clip_mask_guard = if clip_radii.is_zero() {
      None
    } else {
      Some(BackgroundClipMaskGuard::take())
    };
    let clip_mask = clip_mask_guard.as_mut().and_then(|guard| {
      guard.mask(
        clip_rect,
        clip_radii,
        self.pixmap.width(),
        self.pixmap.height(),
      )
    });

    match bg {
      BackgroundImage::LinearGradient { .. }
      | BackgroundImage::RepeatingLinearGradient { .. }
      | BackgroundImage::RadialGradient { .. }
      | BackgroundImage::RepeatingRadialGradient { .. }
      | BackgroundImage::ConicGradient { .. }
      | BackgroundImage::RepeatingConicGradient { .. } => {
        let (mut tile_w, mut tile_h) = compute_background_size(
          layer,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          origin_rect_css.width(),
          origin_rect_css.height(),
          0.0,
          0.0,
          None,
        );
        if tile_w <= 0.0 || tile_h <= 0.0 {
          return;
        }

        let mut rounded_x = false;
        let mut rounded_y = false;
        if layer.repeat.x == BackgroundRepeatKeyword::Round {
          tile_w = round_tile_length(origin_rect_css.width(), tile_w);
          rounded_x = true;
        }
        if layer.repeat.y == BackgroundRepeatKeyword::Round {
          tile_h = round_tile_length(origin_rect_css.height(), tile_h);
          rounded_y = true;
        }
        if rounded_x ^ rounded_y
          && matches!(
            layer.size,
            BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto)
          )
        {
          let aspect = 1.0;
          if rounded_x {
            tile_h = tile_w / aspect;
          } else {
            tile_w = tile_h * aspect;
          }
        }

        let (offset_x, offset_y) = resolve_background_offset(
          layer.position,
          origin_rect_css.width(),
          origin_rect_css.height(),
          tile_w,
          tile_h,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );

        let pixmap_w = tile_w.ceil().max(1.0) as u32;
        let pixmap_h = tile_h.ceil().max(1.0) as u32;
        let quality = Self::filter_quality_for_image(Some(style));

        let tile_bytes = pixmap_allocation_bytes(pixmap_w, pixmap_h);
        let tile_too_large = tile_bytes
          .map(|bytes| {
            bytes > legacy_generated_gradient_tile_raster_bytes_threshold()
              || bytes > MAX_PIXMAP_BYTES
          })
          .unwrap_or(true);

        if !tile_too_large {
          if let Some(pixmap) = self.render_generated_image(bg, style, pixmap_w, pixmap_h) {
            let repeat_both_axes = matches!(
              layer.repeat.x,
              BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round
            ) && matches!(
              layer.repeat.y,
              BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round
            );

            if repeat_both_axes {
              let anchor_x = origin_rect_css.x() + offset_x;
              let anchor_y = origin_rect_css.y() + offset_y;
              if self.paint_background_repeat_pattern(
                &pixmap,
                anchor_x,
                anchor_y,
                tile_w,
                tile_h,
                visible_clip_css,
                clip_mask,
                layer.blend_mode,
                quality,
              ) {
                return;
              }
            }

            let positions_x = tile_positions(
              layer.repeat.x,
              origin_rect_css.x(),
              origin_rect_css.width(),
              tile_w,
              offset_x,
              visible_clip_css.min_x(),
              visible_clip_css.max_x(),
            );
            let positions_y = tile_positions(
              layer.repeat.y,
              origin_rect_css.y(),
              origin_rect_css.height(),
              tile_h,
              offset_y,
              visible_clip_css.min_y(),
              visible_clip_css.max_y(),
            );
            let max_x = visible_clip_css.max_x();
            let max_y = visible_clip_css.max_y();

            for ty in positions_y.iter().copied() {
              for tx in positions_x.iter().copied() {
                if tx >= max_x || ty >= max_y {
                  continue;
                }
                if record_tiles {
                  tiles_painted += 1;
                }
                self.paint_background_tile(
                  &pixmap,
                  tx,
                  ty,
                  tile_w,
                  tile_h,
                  visible_clip_css,
                  clip_mask,
                  layer.blend_mode,
                  quality,
                );
              }
            }
            if record_tiles && tiles_painted > 0 {
              with_paint_diagnostics(|diag| {
                diag.background_tiles += tiles_painted;
              });
            }
            return;
          }
        }

        let positions_x = tile_positions(
          layer.repeat.x,
          origin_rect_css.x(),
          origin_rect_css.width(),
          tile_w,
          offset_x,
          visible_clip_css.min_x(),
          visible_clip_css.max_x(),
        );
        let positions_y = tile_positions(
          layer.repeat.y,
          origin_rect_css.y(),
          origin_rect_css.height(),
          tile_h,
          offset_y,
          visible_clip_css.min_y(),
          visible_clip_css.max_y(),
        );
        let max_x = visible_clip_css.max_x();
        let max_y = visible_clip_css.max_y();

        for ty in positions_y.iter().copied() {
          for tx in positions_x.iter().copied() {
            if tx >= max_x || ty >= max_y {
              continue;
            }
            let tile_rect_css = Rect::from_xywh(tx, ty, tile_w, tile_h);
            let Some(intersection_css) = tile_rect_css.intersection(visible_clip_css) else {
              continue;
            };
            if intersection_css.width() <= 0.0 || intersection_css.height() <= 0.0 {
              continue;
            }

            if record_tiles {
              tiles_painted += 1;
            }
            self.paint_generated_image_intersection(
              bg,
              style,
              tile_rect_css,
              intersection_css,
              pixmap_w,
              pixmap_h,
              clip_mask,
              layer.blend_mode,
              quality,
            );
          }
        }
      }
      BackgroundImage::None => {
        return;
      }
      BackgroundImage::Url(src) => {
        let resolved_src = self.image_cache.resolve_url(&src.url);
        let image = match self.image_cache.load(&resolved_src) {
          Ok(img) => img,
          Err(_) => return,
        };

        let orientation = style.image_orientation.resolve(image.orientation, true);
        let (img_w_raw, img_h_raw) = image.oriented_dimensions(orientation);
        let (img_w_opt, img_h_opt) =
          image.css_natural_dimensions(
            orientation,
            &style.image_resolution,
            self.scale,
            src.override_resolution,
          );
        let img_w = img_w_opt.unwrap_or(0.0);
        let img_h = img_h_opt.unwrap_or(0.0);
        let intrinsic_ratio = image.intrinsic_ratio(orientation);

        if img_w_raw == 0 || img_h_raw == 0 {
          return;
        }
        if !image.is_vector {
          if img_w_opt.is_none() || img_h_opt.is_none() || img_w <= 0.0 || img_h <= 0.0 {
            return;
          }
        }
        let (mut tile_w, mut tile_h) = compute_background_size(
          layer,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          origin_rect_css.width(),
          origin_rect_css.height(),
          img_w,
          img_h,
          intrinsic_ratio,
        );
        if tile_w <= 0.0 || tile_h <= 0.0 {
          return;
        }

        let mut rounded_x = false;
        let mut rounded_y = false;
        if layer.repeat.x == BackgroundRepeatKeyword::Round {
          tile_w = round_tile_length(origin_rect_css.width(), tile_w);
          rounded_x = true;
        }
        if layer.repeat.y == BackgroundRepeatKeyword::Round {
          tile_h = round_tile_length(origin_rect_css.height(), tile_h);
          rounded_y = true;
        }
        if rounded_x ^ rounded_y
          && matches!(
            layer.size,
            BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto)
          )
        {
          if let Some(aspect) = intrinsic_ratio.filter(|r| r.is_finite() && *r > 0.0) {
            if rounded_x {
              tile_h = tile_w / aspect;
            } else {
              tile_w = tile_h * aspect;
            }
          }
        }

        let quality = Self::filter_quality_for_image(Some(style));
        let target_w = self.device_length(tile_w).ceil().max(1.0) as u32;
        let target_h = self.device_length(tile_h).ceil().max(1.0) as u32;

        let pixmap_timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = if image.is_vector {
          let Some(svg) = &image.svg_content else {
            return;
          };
          fn contains_foreign_object_tag(svg: &str) -> bool {
            const NEEDLE: &[u8] = b"foreignobject";
            let bytes = svg.as_bytes();
            bytes
              .windows(NEEDLE.len())
              .any(|window| window.eq_ignore_ascii_case(NEEDLE))
          }

          let svg = svg.as_ref();
          let resolved_svg = if contains_foreign_object_tag(svg) {
            let foreign_object_dpr =
              crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
                svg, self.scale, tile_w, tile_h, img_w, img_h,
              );
            crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
              svg,
              "",
              &self.font_ctx,
              &self.image_cache,
              foreign_object_dpr,
              self.max_iframe_depth,
            )
          } else {
            None
          };
          let svg = resolved_svg.as_deref().unwrap_or(svg);

          let render_w = target_w;
          let render_h = target_h;
          match self.image_cache.render_svg_pixmap_at_size(
            svg,
            render_w,
            render_h,
            &resolved_src,
            self.scale,
          ) {
            Ok(pixmap) => pixmap,
            Err(_) => return,
          }
        } else {
          let should_scale = Self::should_use_scaled_raster_pixmap(
            quality, img_w_raw, img_h_raw, target_w, target_h,
          );
          let result = if should_scale {
            self.image_cache.load_raster_pixmap_at_size(
              &resolved_src,
              orientation,
              true,
              target_w,
              target_h,
              quality,
            )
          } else {
            self
              .image_cache
              .load_raster_pixmap(&resolved_src, orientation, true)
          };
          match result {
            Ok(Some(pixmap)) => pixmap,
            _ => return,
          }
        };
        if let Some(start) = pixmap_timer {
          with_paint_diagnostics(|diag| {
            diag.image_pixmap_ms += start.elapsed().as_secs_f64() * 1000.0;
          });
        }

        let (offset_x, offset_y) = resolve_background_offset(
          layer.position,
          origin_rect_css.width(),
          origin_rect_css.height(),
          tile_w,
          tile_h,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );

        let repeat_both_axes = matches!(
          layer.repeat.x,
          BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round
        ) && matches!(
          layer.repeat.y,
          BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round
        );

        if repeat_both_axes {
          let anchor_x = origin_rect_css.x() + offset_x;
          let anchor_y = origin_rect_css.y() + offset_y;
          if self.paint_background_repeat_pattern(
            &pixmap,
            anchor_x,
            anchor_y,
            tile_w,
            tile_h,
            visible_clip_css,
            clip_mask,
            layer.blend_mode,
            quality,
          ) {
            return;
          }
        }

        let positions_x = tile_positions(
          layer.repeat.x,
          origin_rect_css.x(),
          origin_rect_css.width(),
          tile_w,
          offset_x,
          visible_clip_css.min_x(),
          visible_clip_css.max_x(),
        );
        let positions_y = tile_positions(
          layer.repeat.y,
          origin_rect_css.y(),
          origin_rect_css.height(),
          tile_h,
          offset_y,
          visible_clip_css.min_y(),
          visible_clip_css.max_y(),
        );

        let max_x = visible_clip_css.max_x();
        let max_y = visible_clip_css.max_y();

        for ty in positions_y.iter().copied() {
          for tx in positions_x.iter().copied() {
            if tx >= max_x || ty >= max_y {
              continue;
            }
            if record_tiles {
              tiles_painted += 1;
            }
            self.paint_background_tile(
              pixmap.as_ref(),
              tx,
              ty,
              tile_w,
              tile_h,
              visible_clip_css,
              clip_mask,
              layer.blend_mode,
              quality,
            );
          }
        }
      }
    }
    if record_tiles && tiles_painted > 0 {
      with_paint_diagnostics(|diag| {
        diag.background_tiles += tiles_painted;
      });
    }
  }

  #[allow(dead_code)]
  fn paint_linear_gradient(
    &mut self,
    gradient_rect: Rect,
    paint_rect: Rect,
    clip_mask: Option<&Mask>,
    angle: f32,
    stops: &[(f32, Rgba)],
    spread: SpreadMode,
    blend_mode: MixBlendMode,
  ) {
    if stops.is_empty() {
      return;
    }

    let gradient_rect = self.device_rect(gradient_rect);
    let paint_rect = self.device_rect(paint_rect);

    let skia_stops = gradient_stops(stops);
    let rad = angle.to_radians();
    let dx = rad.sin();
    let dy = -rad.cos(); // CSS 0deg points up
    let len = 0.5 * (gradient_rect.width() * dx.abs() + gradient_rect.height() * dy.abs());
    let cx = gradient_rect.x() + gradient_rect.width() / 2.0;
    let cy = gradient_rect.y() + gradient_rect.height() / 2.0;

    let start = tiny_skia::Point::from_xy(cx - dx * len, cy - dy * len);
    let end = tiny_skia::Point::from_xy(cx + dx * len, cy + dy * len);
    let Some(shader) = LinearGradient::new(start, end, skia_stops, spread, Transform::identity())
    else {
      return;
    };

    let Some(skia_rect) = SkiaRect::from_xywh(
      paint_rect.x(),
      paint_rect.y(),
      paint_rect.width(),
      paint_rect.height(),
    ) else {
      return;
    };
    let path = PathBuilder::from_rect(skia_rect);

    let mut paint = Paint::default();
    paint.shader = shader;
    paint.anti_alias = true;

    if is_hsl_blend(blend_mode) {
      if let Some(mut layer) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
        paint.blend_mode = SkiaBlendMode::SourceOver;
        layer.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          clip_mask,
        );
        composite_hsl_layer(&mut self.pixmap, &layer, 1.0, blend_mode, Some(paint_rect));
      } else {
        paint.blend_mode = map_blend_mode(blend_mode);
        self.pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          clip_mask,
        );
      }
    } else {
      paint.blend_mode = map_blend_mode(blend_mode);
      self.pixmap.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip_mask,
      );
    }
  }

  #[allow(dead_code)]
  fn paint_conic_gradient(
    &mut self,
    gradient_rect: Rect,
    paint_rect: Rect,
    clip_mask: Option<&Mask>,
    position: &BackgroundPosition,
    from_angle_deg: f32,
    stops: &[(f32, Rgba)],
    repeating: bool,
    font_size: f32,
    root_font_size: f32,
    blend_mode: MixBlendMode,
  ) {
    if stops.is_empty() {
      return;
    }
    let paint_rect = self.device_rect(paint_rect);
    let gradient_rect = self.device_rect(gradient_rect);
    let width = paint_rect.width().ceil() as u32;
    let height = paint_rect.height().ceil() as u32;
    if width == 0 || height == 0 {
      return;
    }

    let center = resolve_gradient_center(
      gradient_rect,
      position,
      paint_rect,
      font_size,
      root_font_size,
      (self.css_width, self.css_height),
    );
    let spread = if repeating {
      SpreadMode::Repeat
    } else {
      SpreadMode::Pad
    };
    let timer = self.diagnostics_enabled.then(Instant::now);
    let Some(pix) = rasterize_conic_gradient(
      width,
      height,
      center,
      from_angle_deg.to_radians(),
      spread,
      stops,
      &self.gradient_cache,
      gradient_bucket(width.max(height).saturating_mul(2)),
    )
    .ok()
    .flatten() else {
      return;
    };
    if let Some(start) = timer {
      self.record_gradient_usage((width * height) as u64, start);
    }

    let mut paint = PixmapPaint::default();
    paint.blend_mode = map_blend_mode(blend_mode);
    let transform = Transform::from_translate(paint_rect.x(), paint_rect.y());
    if paint.blend_mode == SkiaBlendMode::Plus {
      draw_pixmap_with_plus_blend(
        &mut self.pixmap,
        0,
        0,
        pix.as_ref(),
        paint.opacity,
        paint.quality,
        transform,
        clip_mask.cloned().as_ref(),
      );
    } else {
      self.pixmap.draw_pixmap(
        0,
        0,
        pix.as_ref(),
        &paint,
        transform,
        clip_mask.cloned().as_ref(),
      );
    }
  }

  #[allow(dead_code)]
  fn paint_radial_gradient(
    &mut self,
    gradient_rect: Rect,
    paint_rect: Rect,
    clip_mask: Option<&Mask>,
    position: &BackgroundPosition,
    size: &RadialGradientSize,
    shape: RadialGradientShape,
    font_size: f32,
    stops: &[(f32, Rgba)],
    spread: SpreadMode,
    blend_mode: MixBlendMode,
  ) {
    if stops.is_empty() {
      return;
    }

    let gradient_rect = self.device_rect(gradient_rect);
    let paint_rect = self.device_rect(paint_rect);
    let skia_stops = gradient_stops(stops);
    let (cx, cy, radius_x, radius_y) = radial_geometry(
      gradient_rect,
      position,
      size,
      shape,
      font_size,
      font_size,
      (self.css_width, self.css_height),
    );
    let transform = Transform::from_translate(cx, cy).pre_scale(radius_x, radius_y);
    let Some(shader) = RadialGradient::new(
      tiny_skia::Point::from_xy(0.0, 0.0),
      tiny_skia::Point::from_xy(0.0, 0.0),
      1.0,
      skia_stops,
      spread,
      transform,
    ) else {
      return;
    };

    let Some(skia_rect) = SkiaRect::from_xywh(
      paint_rect.x(),
      paint_rect.y(),
      paint_rect.width(),
      paint_rect.height(),
    ) else {
      return;
    };
    let path = PathBuilder::from_rect(skia_rect);

    let mut paint = Paint::default();
    paint.shader = shader;
    paint.anti_alias = true;

    if is_hsl_blend(blend_mode) {
      if let Some(mut layer) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
        paint.blend_mode = SkiaBlendMode::SourceOver;
        layer.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          clip_mask,
        );
        composite_hsl_layer(&mut self.pixmap, &layer, 1.0, blend_mode, Some(paint_rect));
      } else {
        paint.blend_mode = map_blend_mode(blend_mode);
        self.pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          clip_mask,
        );
      }
    } else {
      paint.blend_mode = map_blend_mode(blend_mode);
      self.pixmap.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip_mask,
      );
    }
  }

  fn paint_background_repeat_pattern(
    &mut self,
    pixmap: &Pixmap,
    anchor_x: f32,
    anchor_y: f32,
    tile_w: f32,
    tile_h: f32,
    visible_clip_css: Rect,
    mask: Option<&Mask>,
    blend_mode: MixBlendMode,
    quality: FilterQuality,
  ) -> bool {
    if tile_w <= 0.0
      || tile_h <= 0.0
      || !tile_w.is_finite()
      || !tile_h.is_finite()
      || visible_clip_css.width() <= 0.0
      || visible_clip_css.height() <= 0.0
      || pixmap.width() == 0
      || pixmap.height() == 0
    {
      return false;
    }

    let start_x = aligned_start(anchor_x, tile_w, visible_clip_css.min_x());
    let start_y = aligned_start(anchor_y, tile_h, visible_clip_css.min_y());
    if !start_x.is_finite() || !start_y.is_finite() {
      return false;
    }

    let span_x = visible_clip_css.max_x() - start_x;
    let span_y = visible_clip_css.max_y() - start_y;
    let tiles_x = if span_x > 0.0 && span_x.is_finite() {
      (span_x / tile_w).ceil().max(0.0) as u64
    } else {
      0
    };
    let tiles_y = if span_y > 0.0 && span_y.is_finite() {
      (span_y / tile_h).ceil().max(0.0) as u64
    } else {
      0
    };

    let clip_rect = self.device_rect(visible_clip_css);
    if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
      return false;
    }
    let Some(dest_rect) = SkiaRect::from_xywh(
      clip_rect.x(),
      clip_rect.y(),
      clip_rect.width(),
      clip_rect.height(),
    ) else {
      return false;
    };

    let scale_x = self.device_length(tile_w) / pixmap.width() as f32;
    let scale_y = self.device_length(tile_h) / pixmap.height() as f32;
    if !scale_x.is_finite()
      || !scale_y.is_finite()
      || scale_x <= 0.0
      || scale_y <= 0.0
      || !start_x.is_finite()
      || !start_y.is_finite()
    {
      return false;
    }

    let translate_x = self.device_x(start_x);
    let translate_y = self.device_y(start_y);
    let tiles_painted = tiles_x.saturating_mul(tiles_y);
    if self.diagnostics_enabled {
      with_paint_diagnostics(|diag| {
        diag.background_pattern_fast_paths = diag.background_pattern_fast_paths.saturating_add(1);
        if tiles_painted > 0 {
          diag.background_tiles = diag.background_tiles.saturating_add(tiles_painted);
        }
      });
    }

    let mut paint = Paint::default();
    paint.shader = Pattern::new(
      pixmap.as_ref(),
      SpreadMode::Repeat,
      quality,
      1.0,
      Transform::from_row(scale_x, 0.0, 0.0, scale_y, translate_x, translate_y),
    );
    paint.anti_alias = false;

    if is_hsl_blend(blend_mode) {
      if let Some(mut layer) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
        paint.blend_mode = SkiaBlendMode::SourceOver;
        layer.fill_rect(dest_rect, &paint, Transform::identity(), mask);
        composite_hsl_layer(&mut self.pixmap, &layer, 1.0, blend_mode, Some(clip_rect));
      } else {
        paint.blend_mode = map_blend_mode(blend_mode);
        self
          .pixmap
          .fill_rect(dest_rect, &paint, Transform::identity(), mask);
      }
    } else {
      paint.blend_mode = map_blend_mode(blend_mode);
      self
        .pixmap
        .fill_rect(dest_rect, &paint, Transform::identity(), mask);
    }

    true
  }

  fn paint_background_tile(
    &mut self,
    pixmap: &Pixmap,
    tile_x: f32,
    tile_y: f32,
    tile_w: f32,
    tile_h: f32,
    clip: Rect,
    mask: Option<&Mask>,
    blend_mode: MixBlendMode,
    quality: FilterQuality,
  ) {
    if tile_w <= 0.0 || tile_h <= 0.0 {
      return;
    }

    let mut tile_rect = self.device_rect(Rect::from_xywh(tile_x, tile_y, tile_w, tile_h));
    let clip_rect = self.device_rect(clip);
    let Some(intersection) = tile_rect.intersection(clip_rect) else {
      return;
    };
    if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
      return;
    }

    if quality == FilterQuality::Nearest
      && (tile_rect.width() > pixmap.width() as f32 || tile_rect.height() > pixmap.height() as f32)
    {
      let (snapped_w, offset_x) = snap_upscale(tile_rect.width(), pixmap.width() as f32)
        .unwrap_or_else(|| (tile_rect.width(), 0.0));
      let (snapped_h, offset_y) = snap_upscale(tile_rect.height(), pixmap.height() as f32)
        .unwrap_or_else(|| (tile_rect.height(), 0.0));
      tile_rect = Rect::from_xywh(
        tile_rect.x() + offset_x,
        tile_rect.y() + offset_y,
        snapped_w,
        snapped_h,
      );
    }

    let scale_x = tile_rect.width() / pixmap.width() as f32;
    let scale_y = tile_rect.height() / pixmap.height() as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() {
      return;
    }

    let mut paint = Paint::default();
    paint.shader = Pattern::new(
      pixmap.as_ref(),
      SpreadMode::Pad,
      quality,
      1.0,
      Transform::from_row(scale_x, 0.0, 0.0, scale_y, tile_rect.x(), tile_rect.y()),
    );
    paint.anti_alias = false;

    if let Some(rect) = SkiaRect::from_xywh(
      intersection.x(),
      intersection.y(),
      intersection.width(),
      intersection.height(),
    ) {
      if is_hsl_blend(blend_mode) {
        if let Some(mut layer) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
          paint.blend_mode = SkiaBlendMode::SourceOver;
          layer.fill_rect(rect, &paint, Transform::identity(), mask);
          composite_hsl_layer(
            &mut self.pixmap,
            &layer,
            1.0,
            blend_mode,
            Some(intersection),
          );
        } else {
          paint.blend_mode = map_blend_mode(blend_mode);
          self
            .pixmap
            .fill_rect(rect, &paint, Transform::identity(), mask);
        }
      } else {
        paint.blend_mode = map_blend_mode(blend_mode);
        self
          .pixmap
          .fill_rect(rect, &paint, Transform::identity(), mask);
      }
    }
  }

  /// Paints the borders of a fragment
  fn paint_borders(
    &mut self,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    style: &ComputedStyle,
    gap: Option<BorderGap>,
  ) {
    // Only paint if there are borders
    let viewport = (self.css_width, self.css_height);
    let border_bounds_css = Rect::from_xywh(x, y, width, height);
    let radii_css = resolve_border_radii(Some(style), border_bounds_css);
    let base = width.max(0.0);
    let top_css = resolve_length_for_paint(
      &style.used_border_top_width(),
      style.font_size,
      style.root_font_size,
      base,
      viewport,
    )
    .max(0.0);
    let right_css = resolve_length_for_paint(
      &style.used_border_right_width(),
      style.font_size,
      style.root_font_size,
      base,
      viewport,
    )
    .max(0.0);
    let bottom_css = resolve_length_for_paint(
      &style.used_border_bottom_width(),
      style.font_size,
      style.root_font_size,
      base,
      viewport,
    )
    .max(0.0);
    let left_css = resolve_length_for_paint(
      &style.used_border_left_width(),
      style.font_size,
      style.root_font_size,
      base,
      viewport,
    )
    .max(0.0);

    if matches!(style.border_image.source, BorderImageSource::Image(_)) {
      if self.paint_border_image(
        x, y, width, height, style, top_css, right_css, bottom_css, left_css,
      ) {
        return;
      }
    }

    let top = top_css * self.scale;
    let right = right_css * self.scale;
    let bottom = bottom_css * self.scale;
    let left = left_css * self.scale;

    let x = self.device_x(x);
    let y = self.device_y(y);
    let width = self.device_length(width);
    let height = self.device_length(height);
    let radii = self.device_radii(radii_css);
    let gap = gap.map(|gap| {
      let (start, end) = match gap.edge {
        PhysicalSide::Top | PhysicalSide::Bottom => {
          (self.device_x(gap.start), self.device_x(gap.end))
        }
        PhysicalSide::Left | PhysicalSide::Right => {
          (self.device_y(gap.start), self.device_y(gap.end))
        }
      };
      (gap.edge, start.min(end), start.max(end))
    });

    if top <= 0.0 && right <= 0.0 && bottom <= 0.0 && left <= 0.0 {
      return;
    }

    // Rounded borders must follow the corner curves. The legacy border painter draws each side as
    // a straight stroked segment, which leaves visible gaps near rounded corners.
    //
    // When all four sides are identical solid strokes, we can render the border as the (outer
    // rounded rect) minus (inner rounded rect) region. This matches the CSS border geometry and
    // produces correct corner arcs.
    if !radii.is_zero()
      && gap.is_none()
      && matches!(
        (
          style.border_top_style,
          style.border_right_style,
          style.border_bottom_style,
          style.border_left_style
        ),
        (
          CssBorderStyle::Solid,
          CssBorderStyle::Solid,
          CssBorderStyle::Solid,
          CssBorderStyle::Solid
        )
      )
      && style.border_top_color == style.border_right_color
      && style.border_top_color == style.border_bottom_color
      && style.border_top_color == style.border_left_color
      && !style.border_top_color.is_transparent()
    {
      let border_width = top;
      let eps = 1e-6;
      let uniform_widths = border_width > 0.0
        && (border_width - right).abs() <= eps
        && (border_width - bottom).abs() <= eps
        && (border_width - left).abs() <= eps;
      if uniform_widths && width > 0.0 && height > 0.0 {
        if let Some(path) = crate::paint::rasterize::build_rounded_rect_ring_path(
          x,
          y,
          width,
          height,
          &radii,
          border_width,
        ) {
          let mut paint = Paint::default();
          paint.set_color_rgba8(
            style.border_top_color.r,
            style.border_top_color.g,
            style.border_top_color.b,
            style.border_top_color.alpha_u8(),
          );
          paint.blend_mode = SkiaBlendMode::SourceOver;
          paint.anti_alias = true;
          self.pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::EvenOdd,
            Transform::identity(),
            None,
          );
          return;
        }
      }
    }

    // For solid borders with square corners, the CSS border area is composed of trapezoids /
    // triangles meeting at diagonal miters. Stroking each edge produces rectangles, which is
    // incorrect when border widths/colors differ (e.g. CSS border triangles using transparent
    // borders).
    if radii.is_zero()
      && gap.is_none()
      && matches!(
        style.border_top_style,
        CssBorderStyle::Solid | CssBorderStyle::None | CssBorderStyle::Hidden
      )
      && matches!(
        style.border_right_style,
        CssBorderStyle::Solid | CssBorderStyle::None | CssBorderStyle::Hidden
      )
      && matches!(
        style.border_bottom_style,
        CssBorderStyle::Solid | CssBorderStyle::None | CssBorderStyle::Hidden
      )
      && matches!(
        style.border_left_style,
        CssBorderStyle::Solid | CssBorderStyle::None | CssBorderStyle::Hidden
      )
    {
      let eps = 1e-6;
      let uniform_solid = matches!(
        (
          style.border_top_style,
          style.border_right_style,
          style.border_bottom_style,
          style.border_left_style
        ),
        (
          CssBorderStyle::Solid,
          CssBorderStyle::Solid,
          CssBorderStyle::Solid,
          CssBorderStyle::Solid
        )
      ) && (top - right).abs() <= eps
        && (top - bottom).abs() <= eps
        && (top - left).abs() <= eps
        && style.border_top_color == style.border_right_color
        && style.border_top_color == style.border_bottom_color
        && style.border_top_color == style.border_left_color;

      if !uniform_solid {
        let widths = crate::paint::rasterize::BorderWidths {
          top: if style.border_top_style == CssBorderStyle::Solid {
            top
          } else {
            0.0
          },
          right: if style.border_right_style == CssBorderStyle::Solid {
            right
          } else {
            0.0
          },
          bottom: if style.border_bottom_style == CssBorderStyle::Solid {
            bottom
          } else {
            0.0
          },
          left: if style.border_left_style == CssBorderStyle::Solid {
            left
          } else {
            0.0
          },
        };
        let colors = crate::paint::rasterize::BorderColors {
          top: style.border_top_color,
          right: style.border_right_color,
          bottom: style.border_bottom_color,
          left: style.border_left_color,
        };
        crate::paint::rasterize::render_borders(
          &mut self.pixmap,
          x,
          y,
          width,
          height,
          &widths,
          &colors,
          &radii,
        );
        return;
      }
    }

    // Center strokes on the border edges so paint remains within the border box.
    let top_center = y + top * 0.5;
    let bottom_center = y + height - bottom * 0.5;
    let left_center = x + left * 0.5;
    let right_center = x + width - right * 0.5;

    // Use outer border-box corners as endpoints so each edge meets cleanly at corners (avoids
    // leaving visible gaps when stroking each edge independently).
    //
    // Dotted borders use round caps which can extend outside the border box. The display-list
    // backend clips dotted borders to the border box; the legacy painter doesn't, so keep the
    // legacy center endpoints in that case to avoid drawing outside the border box.
    let wants_center_endpoints = matches!(
      (
        style.border_top_style,
        style.border_right_style,
        style.border_bottom_style,
        style.border_left_style
      ),
      (CssBorderStyle::Dotted, _, _, _)
        | (_, CssBorderStyle::Dotted, _, _)
        | (_, _, CssBorderStyle::Dotted, _)
        | (_, _, _, CssBorderStyle::Dotted)
    );
    let (h_start, h_end) = if wants_center_endpoints {
      (left_center, right_center)
    } else {
      (x, x + width)
    };
    let (v_start, v_end) = if wants_center_endpoints {
      (top_center, bottom_center)
    } else {
      (y, y + height)
    };
    let top_start = h_start;
    let top_end = h_end;
    let bottom_start = h_start;
    let bottom_end = h_end;
    let left_start = v_start;
    let left_end = v_end;
    let right_start = v_start;
    let right_end = v_end;

    // Top border
    if top > 0.0 {
      if let Some((PhysicalSide::Top, gap_start, gap_end)) = gap {
        let start = gap_start.clamp(top_start, top_end);
        let end = gap_end.clamp(top_start, top_end);
        if end > start {
          if start > top_start {
            self.paint_border_edge(
              BorderEdge::Top,
              top_start,
              top_center,
              start,
              top_center,
              top,
              style.border_top_style,
              &style.border_top_color,
            );
          }
          if end < top_end {
            self.paint_border_edge(
              BorderEdge::Top,
              end,
              top_center,
              top_end,
              top_center,
              top,
              style.border_top_style,
              &style.border_top_color,
            );
          }
        } else {
          self.paint_border_edge(
            BorderEdge::Top,
            top_start,
            top_center,
            top_end,
            top_center,
            top,
            style.border_top_style,
            &style.border_top_color,
          );
        }
      } else {
        self.paint_border_edge(
          BorderEdge::Top,
          top_start,
          top_center,
          top_end,
          top_center,
          top,
          style.border_top_style,
          &style.border_top_color,
        );
      }
    }

    // Right border
    if right > 0.0 {
      if let Some((PhysicalSide::Right, gap_start, gap_end)) = gap {
        let start = gap_start.clamp(right_start, right_end);
        let end = gap_end.clamp(right_start, right_end);
        if end > start {
          if start > right_start {
            self.paint_border_edge(
              BorderEdge::Right,
              right_center,
              right_start,
              right_center,
              start,
              right,
              style.border_right_style,
              &style.border_right_color,
            );
          }
          if end < right_end {
            self.paint_border_edge(
              BorderEdge::Right,
              right_center,
              end,
              right_center,
              right_end,
              right,
              style.border_right_style,
              &style.border_right_color,
            );
          }
        } else {
          self.paint_border_edge(
            BorderEdge::Right,
            right_center,
            right_start,
            right_center,
            right_end,
            right,
            style.border_right_style,
            &style.border_right_color,
          );
        }
      } else {
        self.paint_border_edge(
          BorderEdge::Right,
          right_center,
          right_start,
          right_center,
          right_end,
          right,
          style.border_right_style,
          &style.border_right_color,
        );
      }
    }

    // Bottom border
    if bottom > 0.0 {
      if let Some((PhysicalSide::Bottom, gap_start, gap_end)) = gap {
        let start = gap_start.clamp(bottom_start, bottom_end);
        let end = gap_end.clamp(bottom_start, bottom_end);
        if end > start {
          if start > bottom_start {
            self.paint_border_edge(
              BorderEdge::Bottom,
              bottom_start,
              bottom_center,
              start,
              bottom_center,
              bottom,
              style.border_bottom_style,
              &style.border_bottom_color,
            );
          }
          if end < bottom_end {
            self.paint_border_edge(
              BorderEdge::Bottom,
              end,
              bottom_center,
              bottom_end,
              bottom_center,
              bottom,
              style.border_bottom_style,
              &style.border_bottom_color,
            );
          }
        } else {
          self.paint_border_edge(
            BorderEdge::Bottom,
            bottom_start,
            bottom_center,
            bottom_end,
            bottom_center,
            bottom,
            style.border_bottom_style,
            &style.border_bottom_color,
          );
        }
      } else {
        self.paint_border_edge(
          BorderEdge::Bottom,
          bottom_start,
          bottom_center,
          bottom_end,
          bottom_center,
          bottom,
          style.border_bottom_style,
          &style.border_bottom_color,
        );
      }
    }

    // Left border
    if left > 0.0 {
      if let Some((PhysicalSide::Left, gap_start, gap_end)) = gap {
        let start = gap_start.clamp(left_start, left_end);
        let end = gap_end.clamp(left_start, left_end);
        if end > start {
          if start > left_start {
            self.paint_border_edge(
              BorderEdge::Left,
              left_center,
              left_start,
              left_center,
              start,
              left,
              style.border_left_style,
              &style.border_left_color,
            );
          }
          if end < left_end {
            self.paint_border_edge(
              BorderEdge::Left,
              left_center,
              end,
              left_center,
              left_end,
              left,
              style.border_left_style,
              &style.border_left_color,
            );
          }
        } else {
          self.paint_border_edge(
            BorderEdge::Left,
            left_center,
            left_start,
            left_center,
            left_end,
            left,
            style.border_left_style,
            &style.border_left_color,
          );
        }
      } else {
        self.paint_border_edge(
          BorderEdge::Left,
          left_center,
          left_start,
          left_center,
          left_end,
          left,
          style.border_left_style,
          &style.border_left_color,
        );
      }
    }
  }

  fn paint_border_image(
    &mut self,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    style: &ComputedStyle,
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
  ) -> bool {
    let BorderImage {
      source,
      slice,
      width: img_widths,
      outset,
      repeat,
    } = &style.border_image;

    let border_widths = BorderWidths {
      top,
      right,
      bottom,
      left,
    };

    let viewport = (self.css_width, self.css_height);
    let target_widths = resolve_border_image_widths(
      img_widths,
      border_widths,
      width,
      height,
      style.font_size,
      style.root_font_size,
      viewport,
    );
    let outsets = resolve_border_image_outset(
      outset,
      target_widths,
      style.font_size,
      style.root_font_size,
      viewport,
    );

    let outer_rect = Rect::from_xywh(
      x - outsets.left,
      y - outsets.top,
      width + outsets.left + outsets.right,
      height + outsets.top + outsets.bottom,
    );
    let inner_rect = Rect::from_xywh(
      outer_rect.x() + target_widths.left,
      outer_rect.y() + target_widths.top,
      outer_rect.width() - target_widths.left - target_widths.right,
      outer_rect.height() - target_widths.top - target_widths.bottom,
    );
    if inner_rect.width() <= 0.0 || inner_rect.height() <= 0.0 {
      return true;
    }

    let bg = match source {
      BorderImageSource::Image(img) => img,
      BorderImageSource::None => return false,
    };

    let (pixmap, img_w, img_h) = match bg.as_ref() {
      BackgroundImage::Url(src) => {
        let resolved_src = self.image_cache.resolve_url(&src.url);
        let image = match self.image_cache.load(&resolved_src) {
          Ok(img) => img,
          Err(_) => return false,
        };
        let orientation = style.image_orientation.resolve(image.orientation, true);
        let pixmap = if image.is_vector {
          let Some(svg) = &image.svg_content else {
            return false;
          };
          let (render_w, render_h) = image.oriented_dimensions(orientation);
          if render_w == 0 || render_h == 0 {
            return false;
          }
          match self.image_cache.render_svg_pixmap_at_size(
            svg,
            render_w,
            render_h,
            &resolved_src,
            self.scale,
          ) {
            Ok(pixmap) => pixmap,
            Err(_) => return false,
          }
        } else {
          match self
            .image_cache
            .load_raster_pixmap(&resolved_src, orientation, true)
          {
            Ok(Some(pixmap)) => pixmap,
            _ => return false,
          }
        };
        let img_w = pixmap.width();
        let img_h = pixmap.height();
        if img_w == 0 || img_h == 0 {
          return false;
        }
        (pixmap, img_w, img_h)
      }
      BackgroundImage::LinearGradient { .. }
      | BackgroundImage::RepeatingLinearGradient { .. }
      | BackgroundImage::RadialGradient { .. }
      | BackgroundImage::RepeatingRadialGradient { .. }
      | BackgroundImage::ConicGradient { .. }
      | BackgroundImage::RepeatingConicGradient { .. } => {
        let img_w = outer_rect.width().max(1.0).round() as u32;
        let img_h = outer_rect.height().max(1.0).round() as u32;
        let Some(pixmap) = self.render_generated_image(bg, style, img_w, img_h) else {
          return false;
        };
        let pixmap = Arc::new(pixmap);
        let img_w = pixmap.width();
        let img_h = pixmap.height();
        (pixmap, img_w, img_h)
      }
      BackgroundImage::None => return false,
    };

    let slice_top = resolve_slice_value(slice.top, img_h);
    let slice_right = resolve_slice_value(slice.right, img_w);
    let slice_bottom = resolve_slice_value(slice.bottom, img_h);
    let slice_left = resolve_slice_value(slice.left, img_w);

    let sx0 = 0.0;
    let sx1 = slice_left.min(img_w as f32);
    let sx2 = (img_w as f32 - slice_right).max(sx1);
    let sx3 = img_w as f32;

    let sy0 = 0.0;
    let sy1 = slice_top.min(img_h as f32);
    let sy2 = (img_h as f32 - slice_bottom).max(sy1);
    let sy3 = img_h as f32;

    let (repeat_x, repeat_y) = *repeat;

    // Corners
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx0, sy0, sx1 - sx0, sy1 - sy0),
      Rect::from_xywh(
        outer_rect.x(),
        outer_rect.y(),
        target_widths.left,
        target_widths.top,
      ),
      BorderImageRepeat::Stretch,
      BorderImageRepeat::Stretch,
    );
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx2, sy0, sx3 - sx2, sy1 - sy0),
      Rect::from_xywh(
        outer_rect.x() + outer_rect.width() - target_widths.right,
        outer_rect.y(),
        target_widths.right,
        target_widths.top,
      ),
      BorderImageRepeat::Stretch,
      BorderImageRepeat::Stretch,
    );
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx0, sy2, sx1 - sx0, sy3 - sy2),
      Rect::from_xywh(
        outer_rect.x(),
        outer_rect.y() + outer_rect.height() - target_widths.bottom,
        target_widths.left,
        target_widths.bottom,
      ),
      BorderImageRepeat::Stretch,
      BorderImageRepeat::Stretch,
    );
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx2, sy2, sx3 - sx2, sy3 - sy2),
      Rect::from_xywh(
        outer_rect.x() + outer_rect.width() - target_widths.right,
        outer_rect.y() + outer_rect.height() - target_widths.bottom,
        target_widths.right,
        target_widths.bottom,
      ),
      BorderImageRepeat::Stretch,
      BorderImageRepeat::Stretch,
    );

    // Edges
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx1, sy0, sx2 - sx1, sy1 - sy0),
      Rect::from_xywh(
        inner_rect.x(),
        outer_rect.y(),
        inner_rect.width(),
        target_widths.top,
      ),
      repeat_x,
      BorderImageRepeat::Stretch,
    );
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx1, sy2, sx2 - sx1, sy3 - sy2),
      Rect::from_xywh(
        inner_rect.x(),
        outer_rect.y() + outer_rect.height() - target_widths.bottom,
        inner_rect.width(),
        target_widths.bottom,
      ),
      repeat_x,
      BorderImageRepeat::Stretch,
    );
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx0, sy1, sx1 - sx0, sy2 - sy1),
      Rect::from_xywh(
        outer_rect.x(),
        inner_rect.y(),
        target_widths.left,
        inner_rect.height(),
      ),
      BorderImageRepeat::Stretch,
      repeat_y,
    );
    self.paint_border_patch(
      pixmap.as_ref(),
      Rect::from_xywh(sx2, sy1, sx3 - sx2, sy2 - sy1),
      Rect::from_xywh(
        outer_rect.x() + outer_rect.width() - target_widths.right,
        inner_rect.y(),
        target_widths.right,
        inner_rect.height(),
      ),
      BorderImageRepeat::Stretch,
      repeat_y,
    );

    if slice.fill {
      self.paint_border_patch(
        pixmap.as_ref(),
        Rect::from_xywh(sx1, sy1, sx2 - sx1, sy2 - sy1),
        inner_rect,
        repeat_x,
        repeat_y,
      );
    }

    true
  }

  fn paint_border_patch(
    &mut self,
    source: &Pixmap,
    src_rect: Rect,
    dest_rect: Rect,
    repeat_x: BorderImageRepeat,
    repeat_y: BorderImageRepeat,
  ) {
    if src_rect.width() <= 0.0
      || src_rect.height() <= 0.0
      || dest_rect.width() <= 0.0
      || dest_rect.height() <= 0.0
    {
      return;
    }

    let sx0 = src_rect.x().max(0.0).floor() as u32;
    let sy0 = src_rect.y().max(0.0).floor() as u32;
    let sx1 = (src_rect.x() + src_rect.width())
      .ceil()
      .min(source.width() as f32)
      .max(0.0) as u32;
    let sy1 = (src_rect.y() + src_rect.height())
      .ceil()
      .min(source.height() as f32)
      .max(0.0) as u32;
    if sx1 <= sx0 || sy1 <= sy0 {
      return;
    }
    let width = sx1 - sx0;
    let height = sy1 - sy0;

    let data = source.data();
    let Some(bytes) = u64::from(width)
      .checked_mul(u64::from(height))
      .and_then(|px| px.checked_mul(4))
    else {
      eprintln!(
        "border image patch {}x{} overflow (source {}x{})",
        width,
        height,
        source.width(),
        source.height()
      );
      return;
    };
    let mut patch = match reserve_buffer(bytes, "border-image patch") {
      Ok(buf) => buf,
      Err(err) => {
        eprintln!(
          "border image patch {}x{} ({} bytes) dropped: {}",
          width, height, bytes, err
        );
        return;
      }
    };
    let Some(size) = IntSize::from_wh(width, height) else {
      return;
    };
    let Some(row_bytes) = u64::from(width).checked_mul(4) else {
      return;
    };
    for row in sy0..sy1 {
      let Some(start) = u64::from(row)
        .checked_mul(u64::from(source.width()))
        .and_then(|v| v.checked_add(u64::from(sx0)))
        .and_then(|v| v.checked_mul(4))
      else {
        return;
      };
      let Some(end) = start.checked_add(row_bytes) else {
        return;
      };
      let Ok(start) = usize::try_from(start) else {
        return;
      };
      let Ok(end) = usize::try_from(end) else {
        return;
      };
      if end > data.len() || start > end {
        return;
      }
      patch.extend_from_slice(&data[start..end]);
    }
    let Some(patch_pixmap) = Pixmap::from_vec(patch, size) else {
      return;
    };

    let mut tile_w = dest_rect.width();
    let mut tile_h = dest_rect.height();

    let mut scale_x = tile_w / width as f32;
    let mut scale_y = tile_h / height as f32;

    if repeat_x != BorderImageRepeat::Stretch {
      scale_x = scale_y;
      tile_w = width as f32 * scale_x;
    }
    if repeat_y != BorderImageRepeat::Stretch {
      scale_y = scale_x;
      tile_h = height as f32 * scale_y;
    }

    if repeat_x == BorderImageRepeat::Round && tile_w > 0.0 {
      let count = (dest_rect.width() / tile_w).round().max(1.0);
      tile_w = dest_rect.width() / count;
      scale_x = tile_w / width as f32;
    }
    if repeat_y == BorderImageRepeat::Round && tile_h > 0.0 {
      let count = (dest_rect.height() / tile_h).round().max(1.0);
      tile_h = dest_rect.height() / count;
      scale_y = tile_h / height as f32;
    }

    // Avoid panics/aborts when the destination is extremely thin and would require an unbounded
    // number of tiles (e.g. a huge width with a near-zero height and repeat-x=space).
    // Bound the work and fall back to a repeating pattern shader in pathological cases.
    const MAX_BORDER_IMAGE_TILES_PER_AXIS: f32 = 4096.0;
    let paint_repeated_patch = |painter: &mut Self| {
      let device_clip = painter.device_rect(dest_rect);
      let Some(dest_sk_rect) = SkiaRect::from_xywh(
        device_clip.x(),
        device_clip.y(),
        device_clip.width(),
        device_clip.height(),
      ) else {
        return;
      };
      let mut paint = Paint::default();
      paint.shader = Pattern::new(
        patch_pixmap.as_ref(),
        SpreadMode::Repeat,
        FilterQuality::Bilinear,
        1.0,
        Transform::from_row(
          scale_x * painter.scale,
          0.0,
          0.0,
          scale_y * painter.scale,
          painter.device_x(dest_rect.x()),
          painter.device_y(dest_rect.y()),
        ),
      );
      paint.anti_alias = false;
      painter
        .pixmap
        .fill_rect(dest_sk_rect, &paint, Transform::identity(), None);
    };

    let tiles_x = dest_rect.width() / tile_w;
    let tiles_y = dest_rect.height() / tile_h;
    let too_many_x = repeat_x != BorderImageRepeat::Stretch
      && (!tiles_x.is_finite() || tiles_x > MAX_BORDER_IMAGE_TILES_PER_AXIS);
    let too_many_y = repeat_y != BorderImageRepeat::Stretch
      && (!tiles_y.is_finite() || tiles_y > MAX_BORDER_IMAGE_TILES_PER_AXIS);
    if too_many_x || too_many_y {
      paint_repeated_patch(self);
      return;
    }

    let positions_x = match repeat_x {
      BorderImageRepeat::Stretch => vec![dest_rect.x()],
      BorderImageRepeat::Round => {
        let end = dest_rect.x() + dest_rect.width();
        if tile_w <= 0.0 {
          return;
        }
        let count =
          (tiles_x.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
        let mut pos = Vec::new();
        if pos.try_reserve_exact(count).is_err() {
          paint_repeated_patch(self);
          return;
        }
        let mut cursor = dest_rect.x();
        for _ in 0..count {
          if cursor >= end - 1e-3 {
            break;
          }
          pos.push(cursor);
          cursor += tile_w;
        }
        pos
      }
      BorderImageRepeat::Space => {
        if tile_w <= 0.0 {
          return;
        }
        let count = tiles_x.floor();
        if count < 1.0 {
          vec![dest_rect.x() + (dest_rect.width() - tile_w) * 0.5]
        } else if count < 2.0 {
          vec![dest_rect.x() + (dest_rect.width() - tile_w) * 0.5]
        } else {
          let spacing = (dest_rect.width() - tile_w * count) / (count - 1.0);
          let count = count as usize;
          let mut pos = Vec::new();
          if pos.try_reserve_exact(count).is_err() {
            paint_repeated_patch(self);
            return;
          }
          let mut cursor = dest_rect.x();
          for _ in 0..count {
            pos.push(cursor);
            cursor += tile_w + spacing;
          }
          pos
        }
      }
      BorderImageRepeat::Repeat => {
        let end = dest_rect.x() + dest_rect.width();
        if tile_w <= 0.0 {
          return;
        }
        let count =
          (tiles_x.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
        let mut pos = Vec::new();
        if pos.try_reserve_exact(count).is_err() {
          paint_repeated_patch(self);
          return;
        }
        let mut cursor = dest_rect.x();
        for _ in 0..count {
          if cursor >= end - 1e-3 {
            break;
          }
          pos.push(cursor);
          cursor += tile_w;
        }
        pos
      }
    };

    let positions_y = match repeat_y {
      BorderImageRepeat::Stretch => vec![dest_rect.y()],
      BorderImageRepeat::Round => {
        let end = dest_rect.y() + dest_rect.height();
        if tile_h <= 0.0 {
          return;
        }
        let count =
          (tiles_y.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
        let mut pos = Vec::new();
        if pos.try_reserve_exact(count).is_err() {
          paint_repeated_patch(self);
          return;
        }
        let mut cursor = dest_rect.y();
        for _ in 0..count {
          if cursor >= end - 1e-3 {
            break;
          }
          pos.push(cursor);
          cursor += tile_h;
        }
        pos
      }
      BorderImageRepeat::Space => {
        if tile_h <= 0.0 {
          return;
        }
        let count = tiles_y.floor();
        if count < 1.0 {
          vec![dest_rect.y() + (dest_rect.height() - tile_h) * 0.5]
        } else if count < 2.0 {
          vec![dest_rect.y() + (dest_rect.height() - tile_h) * 0.5]
        } else {
          let spacing = (dest_rect.height() - tile_h * count) / (count - 1.0);
          let count = count as usize;
          let mut pos = Vec::new();
          if pos.try_reserve_exact(count).is_err() {
            paint_repeated_patch(self);
            return;
          }
          let mut cursor = dest_rect.y();
          for _ in 0..count {
            pos.push(cursor);
            cursor += tile_h + spacing;
          }
          pos
        }
      }
      BorderImageRepeat::Repeat => {
        let end = dest_rect.y() + dest_rect.height();
        if tile_h <= 0.0 {
          return;
        }
        let count =
          (tiles_y.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
        let mut pos = Vec::new();
        if pos.try_reserve_exact(count).is_err() {
          paint_repeated_patch(self);
          return;
        }
        let mut cursor = dest_rect.y();
        for _ in 0..count {
          if cursor >= end - 1e-3 {
            break;
          }
          pos.push(cursor);
          cursor += tile_h;
        }
        pos
      }
    };

    let clip = dest_rect;
    for ty in positions_y.iter().copied() {
      for tx in positions_x.iter().copied() {
        let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
        let Some(intersection) = tile_rect.intersection(clip) else {
          continue;
        };
        if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
          continue;
        }
        let device_clip = self.device_rect(intersection);
        let Some(src_rect) = SkiaRect::from_xywh(
          device_clip.x(),
          device_clip.y(),
          device_clip.width(),
          device_clip.height(),
        ) else {
          continue;
        };
        let mut paint = Paint::default();
        paint.shader = Pattern::new(
          patch_pixmap.as_ref(),
          SpreadMode::Pad,
          FilterQuality::Bilinear,
          1.0,
          Transform::from_row(
            scale_x * self.scale,
            0.0,
            0.0,
            scale_y * self.scale,
            self.device_x(tx),
            self.device_y(ty),
          ),
        );
        paint.anti_alias = false;

        self
          .pixmap
          .fill_rect(src_rect, &paint, Transform::identity(), None);
      }
    }
  }

  fn paint_border_edge(
    &mut self,
    edge: BorderEdge,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    width: f32,
    style: CssBorderStyle,
    color: &Rgba,
  ) {
    // Prefer crisp pixel-snapped solid border strokes over anti-aliased strokes. See the
    // display-list backend (`DisplayListRenderer::render_border_edge`) for the primary rationale.
    let anti_alias = !matches!(style, CssBorderStyle::Solid);
    self.paint_border_edge_with_mode(
      edge,
      x1,
      y1,
      x2,
      y2,
      width,
      style,
      color,
      SkiaBlendMode::SourceOver,
      anti_alias,
    );
  }

  fn paint_border_edge_with_mode(
    &mut self,
    edge: BorderEdge,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    width: f32,
    style: CssBorderStyle,
    color: &Rgba,
    blend_mode: SkiaBlendMode,
    anti_alias: bool,
  ) {
    if width <= 0.0 || matches!(style, CssBorderStyle::None | CssBorderStyle::Hidden) {
      return;
    }

    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
    paint.blend_mode = blend_mode;
    paint.anti_alias = anti_alias;

    let mut stroke = Stroke::default();
    stroke.width = width;
    stroke.line_cap = match style {
      CssBorderStyle::Dotted => tiny_skia::LineCap::Round,
      _ => tiny_skia::LineCap::Butt,
    };

    // Dash patterns per CSS styles
    match style {
      CssBorderStyle::Dotted => {
        // CSS `dotted` borders are rendered as round dots with diameter equal to the border width.
        //
        // Using a dash length equal to the stroke width with round caps causes adjacent dashes to
        // touch (each cap extends by half the stroke width), producing a solid line. Use a gap that
        // is larger than the stroke width so caps don't overlap.
        // Use a relatively large gap so anti-aliased round caps don't visually merge into a
        // continuous line at small border widths.
        stroke.dash = tiny_skia::StrokeDash::new(vec![width, 3.0 * width], 0.0);
      }
      CssBorderStyle::Dashed => {
        stroke.dash = tiny_skia::StrokeDash::new(vec![3.0 * width, width], 0.0);
      }
      _ => {}
    }

    let mut path = PathBuilder::new();
    path.move_to(x1, y1);
    path.line_to(x2, y2);
    let base_path = match path.finish() {
      Some(p) => p,
      None => return,
    };

    match style {
      CssBorderStyle::Double => {
        // When too thin to draw two strokes and a gap, fall back to a solid line.
        if width < 3.0 {
          self
            .pixmap
            .stroke_path(&base_path, &paint, &stroke, Transform::identity(), None);
          return;
        }

        let third = width / 3.0;
        let offset = third + third * 0.5;

        let (outer_path, inner_path) = edge.parallel_lines(x1, y1, x2, y2, offset);

        let mut inner_stroke = stroke.clone();
        inner_stroke.width = third;
        stroke.width = third;

        if let Some(outer) = outer_path {
          self
            .pixmap
            .stroke_path(&outer, &paint, &stroke, Transform::identity(), None);
        }
        if let Some(inner) = inner_path {
          self
            .pixmap
            .stroke_path(&inner, &paint, &inner_stroke, Transform::identity(), None);
        }
      }
      CssBorderStyle::Groove | CssBorderStyle::Ridge => {
        let half = width / 2.0;
        let offset = half * 0.5;
        let (first_path, second_path) = edge.parallel_lines(x1, y1, x2, y2, offset);

        let mut first_paint = paint.clone();
        let mut second_paint = paint.clone();
        let mut first_stroke = stroke.clone();
        let mut second_stroke = stroke.clone();
        first_stroke.width = half;
        second_stroke.width = half;

        let (first_color, second_color) = edge.groove_ridge_colors(color, style);
        first_paint.set_color_rgba8(
          first_color.r,
          first_color.g,
          first_color.b,
          first_color.alpha_u8(),
        );
        second_paint.set_color_rgba8(
          second_color.r,
          second_color.g,
          second_color.b,
          second_color.alpha_u8(),
        );

        if let Some(first) = first_path {
          self.pixmap.stroke_path(
            &first,
            &first_paint,
            &first_stroke,
            Transform::identity(),
            None,
          );
        }
        if let Some(second) = second_path {
          self.pixmap.stroke_path(
            &second,
            &second_paint,
            &second_stroke,
            Transform::identity(),
            None,
          );
        }
      }
      CssBorderStyle::Inset | CssBorderStyle::Outset => {
        let shaded = edge.inset_outset_color(color, style);
        paint.set_color_rgba8(shaded.r, shaded.g, shaded.b, shaded.alpha_u8());
        self
          .pixmap
          .stroke_path(&base_path, &paint, &stroke, Transform::identity(), None);
      }
      _ => {
        self
          .pixmap
          .stroke_path(&base_path, &paint, &stroke, Transform::identity(), None);
      }
    }
  }

  fn paint_outline(&mut self, x: f32, y: f32, width: f32, height: f32, style: &ComputedStyle) {
    let viewport = (self.css_width, self.css_height);
    let ow = resolve_length_for_paint(
      &style.outline_width,
      style.font_size,
      style.root_font_size,
      width.max(0.0),
      viewport,
    )
    .max(0.0)
      * self.scale;
    let outline_style = style.outline_style.to_border_style();
    if ow <= 0.0 || matches!(outline_style, CssBorderStyle::None | CssBorderStyle::Hidden) {
      return;
    }
    let outer_x = self.device_x(x);
    let outer_y = self.device_y(y);
    let outer_w = self.device_length(width);
    let outer_h = self.device_length(height);
    let (color, invert) = style.outline_color.resolve(style.color);
    let blend_mode = if invert {
      SkiaBlendMode::Difference
    } else {
      SkiaBlendMode::SourceOver
    };

    let top_center = outer_y + ow * 0.5;
    let bottom_center = outer_y + outer_h - ow * 0.5;
    let left_center = outer_x + ow * 0.5;
    let right_center = outer_x + outer_w - ow * 0.5;

    self.paint_border_edge_with_mode(
      BorderEdge::Top,
      left_center,
      top_center,
      right_center,
      top_center,
      ow,
      outline_style,
      &color,
      blend_mode,
      false,
    );
    self.paint_border_edge_with_mode(
      BorderEdge::Bottom,
      left_center,
      bottom_center,
      right_center,
      bottom_center,
      ow,
      outline_style,
      &color,
      blend_mode,
      false,
    );
    self.paint_border_edge_with_mode(
      BorderEdge::Left,
      left_center,
      top_center,
      left_center,
      bottom_center,
      ow,
      outline_style,
      &color,
      blend_mode,
      false,
    );
    self.paint_border_edge_with_mode(
      BorderEdge::Right,
      right_center,
      top_center,
      right_center,
      bottom_center,
      ow,
      outline_style,
      &color,
      blend_mode,
      false,
    );
  }

  /// Paints text by shaping and rasterizing glyph outlines.
  fn paint_text(
    &mut self,
    text: &str,
    style: Option<&ComputedStyle>,
    x: f32,
    baseline_y: f32,
    font_size: f32,
    color: Rgba,
  ) -> Result<()> {
    if text.is_empty() {
      return Ok(());
    }

    // Use computed style when available; otherwise construct a minimal fallback
    let style_for_shaping: Cow<ComputedStyle> = match style {
      Some(s) => Cow::Borrowed(s),
      None => {
        let mut s = ComputedStyle::default();
        s.font_size = font_size;
        Cow::Owned(s)
      }
    };

    // Shape text with the full pipeline (bidi, script, fallback fonts)
    let style_ptr = style_for_shaping.as_ref() as *const ComputedStyle as usize;
    let key = TextCacheKey::new(style_ptr, style_for_shaping.font_size, text);

    let shaped_runs: Arc<Vec<ShapedRun>> = if let Ok(cache) = self.text_shape_cache.lock() {
      cache.get(&key).cloned()
    } else {
      None
    }
    .unwrap_or_else(|| {
      let runs = match self
        .shaper
        .shape_arc(text, &style_for_shaping, &self.font_ctx)
      {
        Ok(runs) => runs,
        Err(_) => return Arc::new(Vec::new()),
      };
      if let Ok(mut cache) = self.text_shape_cache.lock() {
        cache.insert(key, Arc::clone(&runs));
      }
      runs
    });

    if shaped_runs.is_empty() {
      return Ok(());
    }

    self.paint_shaped_runs(shaped_runs.as_ref(), x, baseline_y, color, style, None);
    Ok(())
  }

  fn paint_shaped_runs(
    &mut self,
    runs: &[ShapedRun],
    origin_x: f32,
    baseline_y: f32,
    color: Rgba,
    style: Option<&ComputedStyle>,
    clip_mask: Option<&Mask>,
  ) {
    let mut pen_x = origin_x;

    for run in runs {
      let run_origin = pen_x;
      self.paint_shaped_run(run, run_origin, baseline_y, color, style, clip_mask);
      pen_x += run.advance;
    }
  }

  fn paint_shaped_runs_vertical(
    &mut self,
    runs: &[ShapedRun],
    block_origin: f32,
    inline_origin: f32,
    color: Rgba,
    style: Option<&ComputedStyle>,
    clip_mask: Option<&Mask>,
  ) {
    let mut pen_inline = inline_origin;
    for run in runs {
      let run_origin_inline = pen_inline;
      self.paint_shaped_run_vertical(
        run,
        block_origin,
        run_origin_inline,
        color,
        style,
        clip_mask,
      );
      pen_inline += run.advance;
    }
  }

  fn paint_shaped_run(
    &mut self,
    run: &ShapedRun,
    origin_x: f32,
    baseline_y: f32,
    color: Rgba,
    style: Option<&ComputedStyle>,
    clip_mask: Option<&Mask>,
  ) {
    let origin_x = self.device_x(origin_x);
    let baseline_y = self.device_y(baseline_y);
    let Some(instance) = FontInstance::new(&run.font, &run.variations) else {
      return;
    };
    let units_per_em = instance.units_per_em();
    if units_per_em == 0.0 {
      return;
    }
    let raster_timer = text_diagnostics_timer(TextDiagnosticsStage::Rasterize);
    let diag_enabled = raster_timer.is_some();
    let mut scale = run.font_size / units_per_em;
    scale *= run.scale * self.scale;

    let mut glyph_paths = Vec::with_capacity(run.glyphs.len());
    let mut bounds = PathBounds::new();

    let mut pen_x = origin_x;
    let mut pen_y = 0.0_f32;

    for glyph in &run.glyphs {
      let glyph_x = pen_x + glyph.x_offset * self.scale;
      let glyph_y = baseline_y + pen_y - glyph.y_offset * self.scale;

      if let Some(path) = Self::build_glyph_path(
        &instance,
        glyph.glyph_id,
        glyph_x,
        glyph_y,
        scale,
        run.synthetic_oblique,
      ) {
        bounds.include(&path.bounds());
        glyph_paths.push(path);
      }

      pen_x += glyph.x_advance * self.scale;
      pen_y += glyph.y_advance * self.scale;
    }

    if glyph_paths.is_empty() || !bounds.is_valid() {
      return;
    }

    // Rotate sideways runs for vertical text-orientation.
    if !matches!(run.rotation, crate::text::pipeline::RunRotation::None) {
      let angle = match run.rotation {
        crate::text::pipeline::RunRotation::Ccw90 => -90.0_f32.to_radians(),
        crate::text::pipeline::RunRotation::Cw90 => 90.0_f32.to_radians(),
        crate::text::pipeline::RunRotation::None => 0.0,
      };
      let (sin, cos) = angle.sin_cos();
      let tx = origin_x - origin_x * cos + baseline_y * sin;
      let ty = baseline_y - origin_x * sin - baseline_y * cos;
      let rotate = tiny_skia::Transform::from_row(cos, sin, -sin, cos, tx, ty);

      let mut rotated_paths = Vec::with_capacity(glyph_paths.len());
      let mut rotated_bounds = PathBounds::new();
      for path in glyph_paths {
        if let Some(rotated) = path.clone().transform(rotate) {
          let rect = path.bounds();
          let corners = [
            (rect.left(), rect.top()),
            (rect.right(), rect.top()),
            (rect.right(), rect.bottom()),
            (rect.left(), rect.bottom()),
          ];
          let mut min_x = f32::INFINITY;
          let mut min_y = f32::INFINITY;
          let mut max_x = f32::NEG_INFINITY;
          let mut max_y = f32::NEG_INFINITY;
          for (x, y) in corners {
            let mapped_x = x * cos + y * -sin + tx;
            let mapped_y = x * sin + y * cos + ty;
            min_x = min_x.min(mapped_x);
            min_y = min_y.min(mapped_y);
            max_x = max_x.max(mapped_x);
            max_y = max_y.max(mapped_y);
          }
          if let Some(mapped) = tiny_skia::Rect::from_ltrb(min_x, min_y, max_x, max_y) {
            rotated_bounds.include(&mapped);
          }
          rotated_paths.push(rotated);
        }
      }
      if rotated_bounds.is_valid() && !rotated_paths.is_empty() {
        glyph_paths = rotated_paths;
        bounds = rotated_bounds;
      } else {
        glyph_paths = rotated_paths;
      }
    }

    if let Some(style) = style {
      if !style.text_shadow.is_empty() {
        let shadows =
          resolve_text_shadows_with_viewport(style, Some((self.css_width, self.css_height)));
        if !shadows.is_empty() {
          let _ = self.paint_text_shadows(&glyph_paths, &bounds, &shadows, clip_mask);
        }
      }
    }

    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
    paint.anti_alias = true;

    for path in &glyph_paths {
      self.pixmap.fill_path(
        path,
        &paint,
        tiny_skia::FillRule::EvenOdd,
        Transform::identity(),
        clip_mask,
      );
    }
    if diag_enabled {
      record_text_rasterize(
        raster_timer,
        0,
        TextCacheStats::default(),
        TextCacheStats::default(),
      );
    }
  }

  fn paint_shaped_run_vertical(
    &mut self,
    run: &ShapedRun,
    block_origin: f32,
    inline_origin: f32,
    color: Rgba,
    style: Option<&ComputedStyle>,
    clip_mask: Option<&Mask>,
  ) {
    let block_origin = self.device_x(block_origin);
    let inline_origin = self.device_y(inline_origin);

    let Some(instance) = FontInstance::new(&run.font, &run.variations) else {
      return;
    };
    let units_per_em = instance.units_per_em();
    if units_per_em == 0.0 {
      return;
    }
    let raster_timer = text_diagnostics_timer(TextDiagnosticsStage::Rasterize);
    let diag_enabled = raster_timer.is_some();
    let mut scale = run.font_size / units_per_em;
    scale *= run.scale * self.scale;

    let mut glyph_paths = Vec::with_capacity(run.glyphs.len());
    let mut bounds = PathBounds::new();

    let mut pen_inline = inline_origin;
    let mut pen_block = 0.0_f32;

    for glyph in &run.glyphs {
      let inline_step_raw = if glyph.y_advance.abs() > glyph.x_advance.abs() {
        glyph.y_advance
      } else {
        glyph.x_advance
      };
      let block_step_raw = if inline_step_raw == glyph.y_advance {
        glyph.x_advance
      } else {
        glyph.y_advance
      };
      let inline_step = inline_step_raw * self.scale;
      let block_step = block_step_raw * self.scale;
      let inline_pos = pen_inline + glyph.x_offset * self.scale;
      let block_pos = block_origin + pen_block + glyph.y_offset * self.scale;
      if let Some(path) = Self::build_glyph_path(
        &instance,
        glyph.glyph_id,
        block_pos,
        inline_pos,
        scale,
        run.synthetic_oblique,
      ) {
        bounds.include(&path.bounds());
        glyph_paths.push(path);
      }

      pen_inline += inline_step;
      pen_block += block_step;
    }

    if glyph_paths.is_empty() || !bounds.is_valid() {
      return;
    }

    if !matches!(run.rotation, crate::text::pipeline::RunRotation::None) {
      let angle = match run.rotation {
        crate::text::pipeline::RunRotation::Ccw90 => -90.0_f32.to_radians(),
        crate::text::pipeline::RunRotation::Cw90 => 90.0_f32.to_radians(),
        crate::text::pipeline::RunRotation::None => 0.0,
      };
      let (sin, cos) = angle.sin_cos();
      let tx = block_origin - block_origin * cos + inline_origin * sin;
      let ty = inline_origin - block_origin * sin - inline_origin * cos;
      let rotate = tiny_skia::Transform::from_row(cos, sin, -sin, cos, tx, ty);

      let mut rotated_paths = Vec::with_capacity(glyph_paths.len());
      let mut rotated_bounds = PathBounds::new();
      for path in glyph_paths {
        if let Some(rotated) = path.clone().transform(rotate) {
          let mapped = rotated.bounds();
          rotated_bounds.include(&mapped);
          rotated_paths.push(rotated);
        }
      }
      if rotated_bounds.is_valid() && !rotated_paths.is_empty() {
        glyph_paths = rotated_paths;
        bounds = rotated_bounds;
      } else {
        glyph_paths = rotated_paths;
      }
    }

    if let Some(style) = style {
      if !style.text_shadow.is_empty() {
        let shadows =
          resolve_text_shadows_with_viewport(style, Some((self.css_width, self.css_height)));
        if !shadows.is_empty() {
          let _ = self.paint_text_shadows(&glyph_paths, &bounds, &shadows, clip_mask);
        }
      }
    }

    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
    paint.anti_alias = true;

    for path in &glyph_paths {
      self.pixmap.fill_path(
        path,
        &paint,
        tiny_skia::FillRule::EvenOdd,
        Transform::identity(),
        clip_mask,
      );
    }
    if diag_enabled {
      record_text_rasterize(
        raster_timer,
        0,
        TextCacheStats::default(),
        TextCacheStats::default(),
      );
    }
  }

  fn build_glyph_path(
    instance: &FontInstance,
    glyph_id: u32,
    x: f32,
    baseline_y: f32,
    scale: f32,
    skew: f32,
  ) -> Option<tiny_skia::Path> {
    let outline = instance.glyph_outline(glyph_id)?;
    let path = outline.path?;
    let transform = glyph_transform(scale, skew, x, baseline_y);
    path.transform(transform)
  }

  fn paint_text_shadows(
    &mut self,
    paths: &[tiny_skia::Path],
    bounds: &PathBounds,
    shadows: &[ResolvedTextShadow],
    clip_mask: Option<&Mask>,
  ) -> RenderResult<()> {
    for shadow in shadows {
      let offset_x = shadow.offset_x * self.scale;
      let offset_y = shadow.offset_y * self.scale;
      let blur = shadow.blur_radius * self.scale;

      let blur_margin = (blur.abs() * 3.0).ceil();
      let shadow_min_x = bounds.min_x + offset_x - blur_margin;
      let shadow_max_x = bounds.max_x + offset_x + blur_margin;
      let shadow_min_y = bounds.min_y + offset_y - blur_margin;
      let shadow_max_y = bounds.max_y + offset_y + blur_margin;

      let shadow_width = (shadow_max_x - shadow_min_x).ceil().max(0.0) as u32;
      let shadow_height = (shadow_max_y - shadow_min_y).ceil().max(0.0) as u32;
      if shadow_width == 0 || shadow_height == 0 {
        continue;
      }

      let Some(mut shadow_pixmap) = new_pixmap(shadow_width, shadow_height) else {
        continue;
      };
      if self.diagnostics_enabled {
        record_layer_allocation(shadow_width, shadow_height);
      }

      let mut paint = Paint::default();
      paint.set_color_rgba8(
        shadow.color.r,
        shadow.color.g,
        shadow.color.b,
        shadow.color.alpha_u8(),
      );
      paint.anti_alias = true;

      let translate_x = -bounds.min_x + blur_margin;
      let translate_y = -bounds.min_y + blur_margin;
      let transform = Transform::from_translate(translate_x, translate_y);
      for path in paths {
        shadow_pixmap.fill_path(path, &paint, tiny_skia::FillRule::EvenOdd, transform, None);
      }

      if blur > 0.0 {
        apply_gaussian_blur(&mut shadow_pixmap, blur)?;
      }

      let dest_x = shadow_min_x.floor() as i32;
      let dest_y = shadow_min_y.floor() as i32;
      let frac_x = shadow_min_x - dest_x as f32;
      let frac_y = shadow_min_y - dest_y as f32;
      let pixmap_paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: SkiaBlendMode::SourceOver,
        ..Default::default()
      };
      self.pixmap.draw_pixmap(
        dest_x,
        dest_y,
        shadow_pixmap.as_ref(),
        &pixmap_paint,
        Transform::from_translate(frac_x, frac_y),
        clip_mask,
      );
    }
    Ok(())
  }

  fn should_defer_async_image_decode(
    &self,
    dest_width: f32,
    dest_height: f32,
    src: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
  ) -> bool {
    if !dest_width.is_finite()
      || !dest_height.is_finite()
      || dest_width <= 0.0
      || dest_height <= 0.0
    {
      return false;
    }
    let dpr = if self.scale.is_finite() && self.scale > 0.0 {
      self.scale
    } else {
      1.0
    };
    let device_w = (dest_width * dpr).ceil().max(0.0) as u64;
    let device_h = (dest_height * dpr).ceil().max(0.0) as u64;
    let dest_pixels = device_w.saturating_mul(device_h);
    if dest_pixels <= ASYNC_IMAGE_DECODE_MAX_DEST_PIXELS {
      return false;
    }
    let meta = match self.image_cache.probe_with_crossorigin_and_referrer_policy(
      src,
      crossorigin,
      referrer_policy,
    ) {
      Ok(meta) => meta,
      Err(_) => return false,
    };
    if meta.is_vector {
      return false;
    }
    true
  }

  /// Paints a replaced element (image, etc.)
  fn paint_replaced(
    &mut self,
    replaced_type: &ReplacedType,
    box_id: Option<usize>,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
  ) {
    if width <= 0.0 || height <= 0.0 {
      return;
    }

    let viewport = (self.css_width, self.css_height);
    let (content_rect, padding_rect, rects) = if let Some(style) = style {
      let rects = background_rects(x, y, width, height, style, Some(viewport));
      (rects.content, rects.padding, Some(rects))
    } else {
      let rect = Rect::from_xywh(x, y, width, height);
      (rect, rect, None)
    };
    if content_rect.width() <= 0.0 || content_rect.height() <= 0.0 {
      return;
    }

    // Replaced elements default to `overflow: clip` in the UA stylesheet, so their content needs
    // to be clipped. Most replaced content is clipped to the content box (and its inner border
    // radius).
    //
    // Form controls are special-cased:
    // - When `overflow` clips, use the padding box so UA affordances (e.g. `<select>` arrows) that
    //   live in the padding area are not clipped away.
    // - Otherwise, still clip internal painting by default to the content box so values do not
    //   bleed into padding.
    //
    // The display-list backend handles this via explicit clip items; mirror that behavior in the
    // legacy painter.
    let mut clip_mask_guard: Option<BackgroundClipMaskGuard> = None;
    let mut clip_mask: Option<&Mask> = None;
    let mut clip_radii_css = BorderRadii::ZERO;
    if let (Some(style), Some(rects)) = (style, rects.as_ref()) {
      let mut clip_x = matches!(
        style.overflow_x,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto | Overflow::Clip
      ) || style.containment.paint;
      let mut clip_y = matches!(
        style.overflow_y,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto | Overflow::Clip
      ) || style.containment.paint;
      let internal_clip_form_control =
        matches!(replaced_type, ReplacedType::FormControl(_)) && !clip_x && !clip_y;
      if internal_clip_form_control {
        clip_x = true;
        clip_y = true;
      }

      if clip_x || clip_y {
        let (clip_bounds, clip_box_for_radii, allow_radii) = match replaced_type {
          ReplacedType::FormControl(control) => {
            if internal_clip_form_control {
              if matches!(&control.control, FormControlKind::Text { .. }) {
                // CSS UI text inputs clip inline to the content edge and block to the padding
                // edge, so vertically-centered text can use the padding area.
                let inline_vertical = crate::style::block_axis_is_horizontal(style.writing_mode);
                let clip_bounds = if inline_vertical {
                  Rect::from_xywh(
                    padding_rect.x(),
                    content_rect.y(),
                    padding_rect.width(),
                    content_rect.height(),
                  )
                } else {
                  Rect::from_xywh(
                    content_rect.x(),
                    padding_rect.y(),
                    content_rect.width(),
                    padding_rect.height(),
                  )
                };
                (
                  clip_bounds,
                  crate::style::types::BackgroundBox::ContentBox,
                  false,
                )
              } else {
                (
                  content_rect,
                  crate::style::types::BackgroundBox::ContentBox,
                  true,
                )
              }
            } else {
              (
                rects.padding,
                crate::style::types::BackgroundBox::PaddingBox,
                true,
              )
            }
          }
          _ => (
            content_rect,
            crate::style::types::BackgroundBox::ContentBox,
            true,
          ),
        };
        let canvas_w = self.pixmap.width();
        let canvas_h = self.pixmap.height();
        let mut clip_rect = self.device_rect(clip_bounds);
        clip_radii_css = if clip_x && clip_y && allow_radii {
          resolve_clip_radii(style, rects, clip_box_for_radii, Some(viewport))
        } else {
          BorderRadii::ZERO
        };
        let clip_radii = self.device_radii(clip_radii_css);
        if !clip_x {
          clip_rect.origin.x = 0.0;
          clip_rect.size.width = canvas_w as f32;
        }
        if !clip_y {
          clip_rect.origin.y = 0.0;
          clip_rect.size.height = canvas_h as f32;
        }
        clip_mask_guard = Some(BackgroundClipMaskGuard::take());
        clip_mask = clip_mask_guard
          .as_mut()
          .and_then(|guard| guard.mask(clip_rect, clip_radii, canvas_w, canvas_h));
      }
    }

    // Try to render actual content for images and SVG
    match replaced_type {
      ReplacedType::FormControl(control) => {
        if let Some(style) = style {
          if self.paint_form_control(
            control,
            style,
            content_rect,
            padding_rect,
            clip_mask,
            box_id,
          ) {
            return;
          }
        }
      }
      ReplacedType::Image {
        alt,
        loading,
        decoding,
        crossorigin,
        referrer_policy,
        ..
      } => {
        if *loading == ImageLoadingAttribute::Lazy {
          // `loading="lazy"` is a hint; Chrome still paints lazy images that intersect the initial
          // viewport in headless screenshot runs. Defer only when the image is entirely outside the
          // viewport, keeping the image transparent so author-supplied placeholders remain visible.
          let vw = self.css_width;
          let vh = self.css_height;
          if vw.is_finite()
            && vh.is_finite()
            && vw > 0.0
            && vh > 0.0
            && content_rect.x().is_finite()
            && content_rect.y().is_finite()
            && content_rect.width().is_finite()
            && content_rect.height().is_finite()
          {
            let x0 = content_rect.x();
            let y0 = content_rect.y();
            let x1 = x0 + content_rect.width();
            let y1 = y0 + content_rect.height();
            if x1 <= 0.0 || y1 <= 0.0 || x0 >= vw || y0 >= vh {
              return;
            }
          }
        }
        let media_ctx = crate::style::media::MediaContext::screen(self.css_width, self.css_height)
          .with_device_pixel_ratio(self.scale)
          .with_env_overrides();
        let cache_base = self.image_cache.base_url();
        let sources =
          replaced_type.image_sources_with_fallback(crate::tree::box_tree::ImageSelectionContext {
            device_pixel_ratio: self.scale,
            // HTML `srcset` width descriptor selection uses the `sizes` attribute (or 100vw) to
            // compute the "source size", rather than the element's eventual layout width.
            //
            // Using the post-layout slot width can diverge from Chrome and, for pageset fixtures,
            // can cause us to select placeholder/missing srcset candidates.
            slot_width: None,
            viewport: Some(Size::new(self.css_width, self.css_height)),
            media_context: Some(&media_ctx),
            font_size: style.map(|s| s.font_size),
            root_font_size: style.map(|s| s.root_font_size),
            base_url: cache_base.as_deref(),
          });
        let primary = sources.first();
        if *decoding == ImageDecodingAttribute::Async {
          if let Some(primary) = primary {
            if self.should_defer_async_image_decode(
              content_rect.width(),
              content_rect.height(),
              primary.url,
              *crossorigin,
              *referrer_policy,
            ) {
              // `decoding="async"` is a hint that decoding may happen asynchronously, so the image
              // should remain transparent until decoding completes.
              return;
            }
          }
        }
        if let Some(primary) = primary {
          if self.paint_image_from_src_reject_placeholder(
            primary,
            *crossorigin,
            *referrer_policy,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
        }
        if let (Some(style), Some(alt_text)) = (style, alt.as_deref()) {
          if self.paint_alt_text(alt_text, style, content_rect, clip_mask) {
            return;
          }
        }
      }
      ReplacedType::Iframe {
        srcdoc: Some(html),
        src,
        referrer_policy,
        frame_token,
        ..
      } => {
        let context = self.image_cache.resource_context();
        let remaining_depth = self.iframe_depth_remaining(context.as_ref());
        let resolved_url = self.image_cache.resolve_url(src);
        let stable_id = frame_token
          .as_ref()
          .and_then(|token| usize::try_from(*token).ok())
          .or(box_id)
          .unwrap_or(0);
        let info = IframePaintInfo {
          stable_id,
          url: resolved_url,
          content_rect,
          clip_radii: clip_radii_css,
          is_srcdoc: true,
        };
        let action = context
          .as_ref()
          .and_then(|ctx| ctx.iframe_embedder.as_ref())
          .map(|embedder| {
            embedder.iframe_paint_action(
              &info,
              Some(html),
              style,
              &self.image_cache,
              &self.font_ctx,
              self.scale,
              remaining_depth,
              *referrer_policy,
            )
          })
          .unwrap_or_else(|| {
            DefaultIframeEmbedder.iframe_paint_action(
              &info,
              Some(html),
              style,
              &self.image_cache,
              &self.font_ctx,
              self.scale,
              remaining_depth,
              *referrer_policy,
            )
          });
        match action {
          IframePaintAction::Inline(image) => {
            if let Some(pixmap) =
              PixmapRef::from_bytes(image.pixels.as_ref(), image.width, image.height)
            {
              let device_x = self.device_x(content_rect.x());
              let device_y = self.device_y(content_rect.y());
              let paint = PixmapPaint::default();
              self.pixmap.draw_pixmap(
                device_x as i32,
                device_y as i32,
                pixmap,
                &paint,
                Transform::identity(),
                clip_mask,
              );
              return;
            }
          }
          IframePaintAction::RemotePlaceholder => return,
          IframePaintAction::Fallback => {}
        }

        if self.paint_svg(
          src,
          style,
          content_rect.x(),
          content_rect.y(),
          content_rect.width(),
          content_rect.height(),
          clip_mask,
        ) {
          return;
        }
        let media_ctx = crate::style::media::MediaContext::screen(self.css_width, self.css_height)
          .with_device_pixel_ratio(self.scale)
          .with_env_overrides();
        let cache_base = self.image_cache.base_url();
        let sources =
          replaced_type.image_sources_with_fallback(crate::tree::box_tree::ImageSelectionContext {
            device_pixel_ratio: self.scale,
            slot_width: None,
            viewport: Some(Size::new(self.css_width, self.css_height)),
            media_context: Some(&media_ctx),
            font_size: style.map(|s| s.font_size),
            root_font_size: style.map(|s| s.root_font_size),
            base_url: cache_base.as_deref(),
          });
        for candidate in sources {
          if self.paint_image_from_src(
            &candidate,
            CrossOriginAttribute::None,
            *referrer_policy,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
        }
      }
      ReplacedType::Svg { content } => {
        if self.paint_inline_svg(
          content,
          style,
          content_rect.x(),
          content_rect.y(),
          content_rect.width(),
          content_rect.height(),
          clip_mask,
        ) {
          return;
        }
        if !content.fallback_svg.is_empty() {
          if self.paint_inline_svg_markup_with_document_injection(
            &content.fallback_svg,
            content.document_css_injection.as_ref(),
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
          let fallback_source = crate::tree::box_tree::SelectedImageSource {
            url: content.fallback_svg.as_str(),
            descriptor: None,
            density: None,
            from_picture: false,
          };
          if self.paint_image_from_src(
            &fallback_source,
            CrossOriginAttribute::None,
            None,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
        }
      }
      ReplacedType::Iframe {
        src: content,
        srcdoc: None,
        referrer_policy,
        frame_token,
        ..
      } => {
        let context = self.image_cache.resource_context();
        let remaining_depth = self.iframe_depth_remaining(context.as_ref());
        let resolved_url = self.image_cache.resolve_url(content);
        let stable_id = frame_token
          .as_ref()
          .and_then(|token| usize::try_from(*token).ok())
          .or(box_id)
          .unwrap_or(0);
        let info = IframePaintInfo {
          stable_id,
          url: resolved_url,
          content_rect,
          clip_radii: clip_radii_css,
          is_srcdoc: false,
        };
        let action = context
          .as_ref()
          .and_then(|ctx| ctx.iframe_embedder.as_ref())
          .map(|embedder| {
            embedder.iframe_paint_action(
              &info,
              None,
              style,
              &self.image_cache,
              &self.font_ctx,
              self.scale,
              remaining_depth,
              *referrer_policy,
            )
          })
          .unwrap_or_else(|| {
            DefaultIframeEmbedder.iframe_paint_action(
              &info,
              None,
              style,
              &self.image_cache,
              &self.font_ctx,
              self.scale,
              remaining_depth,
              *referrer_policy,
            )
          });
        match action {
          IframePaintAction::Inline(image) => {
            if let Some(pixmap) =
              PixmapRef::from_bytes(image.pixels.as_ref(), image.width, image.height)
            {
              let device_x = self.device_x(content_rect.x());
              let device_y = self.device_y(content_rect.y());
              let paint = PixmapPaint::default();
              self.pixmap.draw_pixmap(
                device_x as i32,
                device_y as i32,
                pixmap,
                &paint,
                Transform::identity(),
                clip_mask,
              );
              return;
            }
          }
          IframePaintAction::RemotePlaceholder => return,
          IframePaintAction::Fallback => {}
        }

        if self.paint_svg(
          content,
          style,
          content_rect.x(),
          content_rect.y(),
          content_rect.width(),
          content_rect.height(),
          clip_mask,
        ) {
          return;
        }
        let media_ctx = crate::style::media::MediaContext::screen(self.css_width, self.css_height)
          .with_device_pixel_ratio(self.scale)
          .with_env_overrides();
        let cache_base = self.image_cache.base_url();
        let sources =
          replaced_type.image_sources_with_fallback(crate::tree::box_tree::ImageSelectionContext {
            device_pixel_ratio: self.scale,
            slot_width: None,
            viewport: Some(Size::new(self.css_width, self.css_height)),
            media_context: Some(&media_ctx),
            font_size: style.map(|s| s.font_size),
            root_font_size: style.map(|s| s.root_font_size),
            base_url: cache_base.as_deref(),
          });
        for candidate in sources {
          if self.paint_image_from_src(
            &candidate,
            CrossOriginAttribute::None,
            *referrer_policy,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
        }
      }
      ReplacedType::Embed { .. } | ReplacedType::Object { .. } => {
        let media_ctx = crate::style::media::MediaContext::screen(self.css_width, self.css_height)
          .with_device_pixel_ratio(self.scale)
          .with_env_overrides();
        let cache_base = self.image_cache.base_url();
        let sources =
          replaced_type.image_sources_with_fallback(crate::tree::box_tree::ImageSelectionContext {
            device_pixel_ratio: self.scale,
            slot_width: None,
            viewport: Some(Size::new(self.css_width, self.css_height)),
            media_context: Some(&media_ctx),
            font_size: style.map(|s| s.font_size),
            root_font_size: style.map(|s| s.root_font_size),
            base_url: cache_base.as_deref(),
          });
        for candidate in &sources {
          if self.paint_svg(
            candidate.url,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
          if self.paint_image_from_src_reject_placeholder(
            candidate,
            CrossOriginAttribute::None,
            None,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
        }

        if let Some(candidate) = sources.first() {
          if let Some(image) = self.render_iframe_src(candidate.url, None, content_rect, style) {
            if let Some(pixmap) =
              PixmapRef::from_bytes(image.pixels.as_ref(), image.width, image.height)
            {
              let device_x = self.device_x(content_rect.x());
              let device_y = self.device_y(content_rect.y());
              let paint = PixmapPaint::default();
              self.pixmap.draw_pixmap(
                device_x as i32,
                device_y as i32,
                pixmap,
                &paint,
                Transform::identity(),
                clip_mask,
              );
              return;
            }
          }
        }
      }
      ReplacedType::Video {
        src,
        crossorigin,
        referrer_policy,
        ..
      } => {
        let crossorigin = *crossorigin;
        let referrer_policy = *referrer_policy;
        // Prefer an actual decoded video frame if one is available.
        if let Some(provider) = self.media_provider.as_ref() {
          let size_hint = crate::media::MediaFrameSizeHint::new(
            Size::new(content_rect.width(), content_rect.height()),
            self.scale,
          );
          if let Some(frame) = provider.video_frame(box_id, src.as_str(), Some(size_hint)) {
            if self.paint_image_data(
              frame.as_ref(),
              style,
              content_rect.x(),
              content_rect.y(),
              content_rect.width(),
              content_rect.height(),
              clip_mask,
            ) {
              return;
            }
          }
        }

        // Fall back to painting the poster image (if any).
        let media_ctx = crate::style::media::MediaContext::screen(self.css_width, self.css_height)
          .with_device_pixel_ratio(self.scale)
          .with_env_overrides();
        let cache_base = self.image_cache.base_url();
        let sources =
          replaced_type.image_sources_with_fallback(crate::tree::box_tree::ImageSelectionContext {
            device_pixel_ratio: self.scale,
            slot_width: None,
            viewport: Some(Size::new(self.css_width, self.css_height)),
            media_context: Some(&media_ctx),
            font_size: style.map(|s| s.font_size),
            root_font_size: style.map(|s| s.root_font_size),
            base_url: cache_base.as_deref(),
          });
        for candidate in sources {
          if self.paint_image_from_src(
            &candidate,
            crossorigin,
            referrer_policy,
            style,
            content_rect.x(),
            content_rect.y(),
            content_rect.width(),
            content_rect.height(),
            clip_mask,
          ) {
            return;
          }
        }
      }
      ReplacedType::Math(math) => {
        // Match display-list MathML painting: render math fragments into the replaced element's
        // content box (already handled via `content_rect`), respecting any replaced-element clip
        // mask so overflow: clip does not bleed into padding/border.
        let fallback_style = ComputedStyle::default();
        let style_ref = style.unwrap_or(&fallback_style);
        let mut fallback_layout = None;
        let layout = match math.layout.as_deref() {
          Some(layout) => layout,
          None => {
            fallback_layout = Some(crate::math::layout_mathml(
              &math.root,
              style_ref,
              &self.font_ctx,
            ));
            fallback_layout.as_ref().expect("inserted layout")
          }
        };

        let base_color = style_ref.color;
        let used_dark_color_scheme = style_ref.used_dark_color_scheme;
        let layout_w = layout.width.max(0.01);
        let layout_h = layout.height.max(0.01);
        let scale_x = if layout_w > 0.0 {
          content_rect.width() / layout_w
        } else {
          1.0
        };
        let scale_y = if layout_h > 0.0 {
          content_rect.height() / layout_h
        } else {
          1.0
        };

        let scale_run = |run: &ShapedRun| -> ShapedRun {
          if (scale_x - 1.0).abs() < 0.001 && (scale_y - 1.0).abs() < 0.001 {
            return run.clone();
          }
          let mut scaled = run.clone();
          let uniform = ((scale_x + scale_y) * 0.5).max(0.0);
          scaled.font_size *= uniform;
          scaled.advance *= scale_x;
          scaled.baseline_shift *= scale_y;
          scaled.synthetic_bold *= uniform;
          scaled.scale *= uniform.max(1e-3);
          for glyph in &mut scaled.glyphs {
            glyph.x_offset *= scale_x;
            glyph.y_offset *= scale_y;
            glyph.x_advance *= scale_x;
            glyph.y_advance *= scale_y;
          }
          scaled
        };

        for frag in &layout.fragments {
          match frag {
            MathFragment::Glyph { origin, run, color } => {
              let current = color.unwrap_or(base_color);
              let paint_color = style_ref
                .webkit_text_fill_color
                .to_rgba_with_scheme_and_forced_colors(
                  current,
                  used_dark_color_scheme,
                  style_ref.forced_colors,
                );
              let scaled_run = scale_run(run);
              let baseline_y = content_rect.y() + origin.y * scale_y;
              let start_x = content_rect.x() + origin.x * scale_x;
              let runs = [scaled_run];
              self.paint_shaped_runs(
                &runs,
                start_x,
                baseline_y,
                paint_color,
                Some(style_ref),
                clip_mask,
              );
            }
            MathFragment::Rule { rect: r, color } => {
              let current = color.unwrap_or(base_color);
              let paint_color = style_ref
                .webkit_text_fill_color
                .to_rgba_with_scheme_and_forced_colors(
                  current,
                  used_dark_color_scheme,
                  style_ref.forced_colors,
                );
              let scaled_rect = Rect::from_xywh(
                content_rect.x() + r.x() * scale_x,
                content_rect.y() + r.y() * scale_y,
                r.width() * scale_x,
                r.height() * scale_y,
              );
              let device_rect = self.device_rect(scaled_rect);
              let Some(rect) = SkiaRect::from_xywh(
                device_rect.x(),
                device_rect.y(),
                device_rect.width(),
                device_rect.height(),
              ) else {
                continue;
              };
              let mut paint = Paint::default();
              paint.set_color_rgba8(
                paint_color.r,
                paint_color.g,
                paint_color.b,
                paint_color.alpha_u8(),
              );
              paint.anti_alias = true;
              self
                .pixmap
                .fill_rect(rect, &paint, Transform::identity(), clip_mask);
            }
            MathFragment::Line {
              from,
              to,
              width,
              color,
            } => {
              let current = color.unwrap_or(base_color);
              let paint_color = style_ref
                .webkit_text_fill_color
                .to_rgba_with_scheme_and_forced_colors(
                  current,
                  used_dark_color_scheme,
                  style_ref.forced_colors,
                );
              let start = Point::new(
                content_rect.x() + from.x * scale_x,
                content_rect.y() + from.y * scale_y,
              );
              let end = Point::new(
                content_rect.x() + to.x * scale_x,
                content_rect.y() + to.y * scale_y,
              );
              let dx = end.x - start.x;
              let dy = end.y - start.y;
              let len = (dx * dx + dy * dy).sqrt();
              let uniform = ((scale_x + scale_y) * 0.5).max(0.0);
              let thickness = *width * uniform;
              if !(len.is_finite() && len > 0.0 && thickness.is_finite() && thickness > 0.0) {
                continue;
              }
              let angle = dy.atan2(dx);
              let cos = angle.cos();
              let sin = angle.sin();

              let start_x = start.x * self.scale;
              let start_y = start.y * self.scale;
              let len = len * self.scale;
              let thickness = thickness * self.scale;
              let Some(rect) = SkiaRect::from_xywh(0.0, -thickness * 0.5, len, thickness) else {
                continue;
              };
              let transform = Transform::from_row(cos, sin, -sin, cos, start_x, start_y);

              let mut paint = Paint::default();
              paint.set_color_rgba8(
                paint_color.r,
                paint_color.g,
                paint_color.b,
                paint_color.alpha_u8(),
              );
              paint.anti_alias = true;
              self.pixmap.fill_rect(rect, &paint, transform, clip_mask);
            }
            MathFragment::StrokeRect {
              rect: stroke_rect,
              radius,
              width,
              color,
            } => {
              let current = color.unwrap_or(base_color);
              let paint_color = style_ref
                .webkit_text_fill_color
                .to_rgba_with_scheme_and_forced_colors(
                  current,
                  used_dark_color_scheme,
                  style_ref.forced_colors,
                );
              let scaled_rect = Rect::from_xywh(
                content_rect.x() + stroke_rect.x() * scale_x,
                content_rect.y() + stroke_rect.y() * scale_y,
                stroke_rect.width() * scale_x,
                stroke_rect.height() * scale_y,
              );
              let uniform = ((scale_x + scale_y) * 0.5).max(0.0);
              let stroke_width = *width * uniform;
              let scaled_radius = *radius * uniform;

              let device_rect = self.device_rect(scaled_rect);
              let Some(rect) = SkiaRect::from_xywh(
                device_rect.x(),
                device_rect.y(),
                device_rect.width(),
                device_rect.height(),
              ) else {
                continue;
              };

              let mut paint = Paint::default();
              paint.set_color_rgba8(
                paint_color.r,
                paint_color.g,
                paint_color.b,
                paint_color.alpha_u8(),
              );
              paint.anti_alias = true;

              let stroke = tiny_skia::Stroke {
                width: stroke_width * self.scale,
                ..Default::default()
              };

              if scaled_radius > 0.0 {
                let radii = self.device_radii(BorderRadii::uniform(scaled_radius));
                let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
                  rect.x(),
                  rect.y(),
                  rect.width(),
                  rect.height(),
                  &radii,
                ) else {
                  continue;
                };
                self
                  .pixmap
                  .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
              } else {
                let path = PathBuilder::from_rect(rect);
                self
                  .pixmap
                  .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
              }
            }
            MathFragment::StrokeRoundedRect {
              rect: stroke_rect,
              radii: (radius_x, radius_y),
              width,
              color,
            } => {
              let current = color.unwrap_or(base_color);
              let paint_color = style_ref
                .webkit_text_fill_color
                .to_rgba_with_scheme_and_forced_colors(
                  current,
                  used_dark_color_scheme,
                  style_ref.forced_colors,
                );
              let scaled_rect = Rect::from_xywh(
                content_rect.x() + stroke_rect.x() * scale_x,
                content_rect.y() + stroke_rect.y() * scale_y,
                stroke_rect.width() * scale_x,
                stroke_rect.height() * scale_y,
              );
              let uniform = ((scale_x + scale_y) * 0.5).max(0.0);
              let stroke_width = *width * uniform;
              let scaled_radius_x = *radius_x * scale_x;
              let scaled_radius_y = *radius_y * scale_y;

              let device_rect = self.device_rect(scaled_rect);
              let Some(rect) = SkiaRect::from_xywh(
                device_rect.x(),
                device_rect.y(),
                device_rect.width(),
                device_rect.height(),
              ) else {
                continue;
              };

              let mut paint = Paint::default();
              paint.set_color_rgba8(
                paint_color.r,
                paint_color.g,
                paint_color.b,
                paint_color.alpha_u8(),
              );
              paint.anti_alias = true;

              let stroke = tiny_skia::Stroke {
                width: stroke_width * self.scale,
                ..Default::default()
              };

              if scaled_radius_x > 0.0 || scaled_radius_y > 0.0 {
                let corner_radius = crate::paint::display_list::BorderRadius {
                  x: scaled_radius_x.max(0.0),
                  y: scaled_radius_y.max(0.0),
                };
                let radii = self.device_radii(BorderRadii {
                  top_left: corner_radius,
                  top_right: corner_radius,
                  bottom_right: corner_radius,
                  bottom_left: corner_radius,
                });
                let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
                  rect.x(),
                  rect.y(),
                  rect.width(),
                  rect.height(),
                  &radii,
                ) else {
                  continue;
                };
                self
                  .pixmap
                  .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
              } else {
                let path = PathBuilder::from_rect(rect);
                self
                  .pixmap
                  .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
              }
            }
          }
        }
        return;
      }
      _ => {}
    }

    self.paint_replaced_placeholder(replaced_type, style, content_rect, clip_mask);
  }

  fn paint_inline_svg(
    &mut self,
    content: &SvgContent,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    let style_injection = content.document_css_injection.as_ref();
    if content.foreign_objects.is_empty() {
      return self.paint_inline_svg_markup_with_document_injection(
        &content.svg,
        style_injection,
        style,
        x,
        y,
        width,
        height,
        clip_mask,
      );
    }

    let foreign_object_dpr = (|| {
      let meta = self
        .image_cache
        .probe_svg_content(&content.svg, "inline-svg")
        .ok()?;
      let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
      let orientation = style
        .map(|s| s.image_orientation.resolve(meta.orientation, false))
        .unwrap_or_else(|| ImageOrientation::default().resolve(meta.orientation, false));
      let (img_w_raw, img_h_raw) = meta.oriented_dimensions(orientation);
      if img_w_raw == 0 || img_h_raw == 0 {
        return None;
      }
      let (img_w_css, img_h_css) =
        meta.css_dimensions(orientation, &image_resolution, self.scale, None)?;
      let has_intrinsic_ratio = meta.intrinsic_ratio(orientation).is_some();

      let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
      let pos = style
        .map(|s| s.object_position)
        .unwrap_or_else(default_object_position);
      let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
      let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);
      let (_dest_x, _dest_y, dest_w, dest_h) = compute_object_fit(
        fit,
        pos,
        width,
        height,
        img_w_css,
        img_h_css,
        has_intrinsic_ratio,
        font_size,
        root_font_size,
        Some((self.css_width, self.css_height)),
      )?;
      if dest_w <= 0.0 || dest_h <= 0.0 {
        return None;
      }
      Some(
        crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
          &content.svg,
          self.scale,
          dest_w,
          dest_h,
          img_w_css,
          img_h_css,
        ),
      )
    })()
    .unwrap_or(self.scale);

    if let Some(svg) = crate::paint::svg_foreign_object::inline_svg_with_foreign_objects(
      &content.svg,
      &content.foreign_objects,
      &content.shared_css,
      &self.font_ctx,
      &self.image_cache,
      foreign_object_dpr,
      self.max_iframe_depth,
    ) {
      return self.paint_inline_svg_markup_with_document_injection(
        &svg,
        style_injection,
        style,
        x,
        y,
        width,
        height,
        clip_mask,
      );
    }

    false
  }

  fn paint_inline_svg_markup_with_document_injection(
    &mut self,
    svg_markup: &str,
    style_injection: Option<&SvgDocumentCssInjection>,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    if svg_markup.is_empty() {
      return false;
    }

    let mut injected_svg: Option<String> = None;
    let mut injected_insert_pos: Option<usize> = None;
    if let Some(defs) = self.svg_id_defs_raw.as_ref() {
      if let Some((svg, pos)) =
        crate::paint::svg_id_defs_injection::inject_svg_id_defs_raw(svg_markup, defs.as_ref())
      {
        injected_svg = Some(svg);
        injected_insert_pos = Some(pos);
      }
    }
    let svg_markup = injected_svg.as_deref().unwrap_or(svg_markup);

    let defs_injection = self.svg_id_defs.as_ref().and_then(|defs| {
      crate::paint::svg_mask_image::defs_injection_for_svg_fragment(defs, svg_markup)
    });

    let mut insert_pos = injected_insert_pos.or_else(|| style_injection.map(|inj| inj.insert_pos));
    if let Some(pos) = insert_pos {
      if svg_markup.get(..pos).is_none() || svg_markup.get(pos..).is_none() {
        insert_pos = None;
      }
    }
    if insert_pos.is_none() && (defs_injection.is_some() || style_injection.is_some()) {
      insert_pos = crate::paint::svg_mask_image::svg_root_start_tag_end(svg_markup);
    }

    let injected: Option<(usize, std::borrow::Cow<'_, str>)> =
      match (insert_pos, defs_injection, style_injection) {
        (Some(pos), Some(defs), Some(style)) => {
          let mut combined = defs;
          combined.reserve(style.style_element.len());
          combined.push_str(style.style_element.as_ref());
          Some((pos, std::borrow::Cow::Owned(combined)))
        }
        (Some(pos), Some(defs), None) => Some((pos, std::borrow::Cow::Owned(defs))),
        (Some(pos), None, Some(style)) => Some((
          pos,
          std::borrow::Cow::Borrowed(style.style_element.as_ref()),
        )),
        _ => None,
      };

    if let Some((insert_pos, injected)) = injected.as_ref() {
      return self.paint_inline_svg_with_injected_markup(
        svg_markup,
        *insert_pos,
        injected.as_ref(),
        style,
        x,
        y,
        width,
        height,
        clip_mask,
      );
    }

    self.paint_svg(svg_markup, style, x, y, width, height, clip_mask)
  }

  fn paint_inline_svg_with_injected_markup(
    &mut self,
    content: &str,
    insert_pos: usize,
    injected_markup: &str,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    if content.is_empty() {
      return false;
    }

    // Defensive: allow callers to fall back to the generic SVG paint path if this does not
    // look like inline SVG markup.
    let trimmed = trim_ascii_whitespace_start_html_css(content);
    let inline_svg = trimmed.starts_with("<svg") || trimmed.starts_with("<?xml");
    if !inline_svg {
      return self.paint_svg(content, style, x, y, width, height, clip_mask);
    }

    let meta = match self.image_cache.probe_svg_content(content, "inline-svg") {
      Ok(meta) => meta,
      Err(_) => return false,
    };

    let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
    let orientation = style
      .map(|s| s.image_orientation.resolve(meta.orientation, false))
      .unwrap_or_else(|| ImageOrientation::default().resolve(meta.orientation, false));
    let (img_w_raw, img_h_raw) = meta.oriented_dimensions(orientation);
    if img_w_raw == 0 || img_h_raw == 0 {
      return false;
    }
    let Some((img_w_css, img_h_css)) =
      meta.css_dimensions(orientation, &image_resolution, self.scale, None)
    else {
      return false;
    };
    let has_intrinsic_ratio = meta.intrinsic_ratio(orientation).is_some();

    let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
    let pos = style
      .map(|s| s.object_position)
      .unwrap_or_else(default_object_position);
    let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
    let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);

    let (dest_x, dest_y, dest_w, dest_h) = match compute_object_fit(
      fit,
      pos,
      width,
      height,
      img_w_css,
      img_h_css,
      has_intrinsic_ratio,
      font_size,
      root_font_size,
      Some((self.css_width, self.css_height)),
    ) {
      Some(v) => v,
      None => return false,
    };

    let dest_x_device = self.device_length(dest_x);
    let dest_y_device = self.device_length(dest_y);
    let dest_w_device = self.device_length(dest_w);
    let dest_h_device = self.device_length(dest_h);
    if dest_w_device <= 0.0 || dest_h_device <= 0.0 {
      return false;
    }

    let render_w = dest_w_device.ceil().max(1.0) as u32;
    let render_h = dest_h_device.ceil().max(1.0) as u32;
    let pixmap = self
      .image_cache
      .render_svg_pixmap_at_size_with_injected_style(
        content,
        insert_pos,
        injected_markup,
        render_w,
        render_h,
        "inline-svg",
        self.scale,
      );
    let pixmap = match pixmap {
      Ok(pixmap) => pixmap,
      Err(_) => return false,
    };

    let scale_x = dest_w_device / render_w as f32;
    let scale_y = dest_h_device / render_h as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() {
      return false;
    }

    let mut paint = PixmapPaint::default();
    paint.quality = Self::filter_quality_for_image(style);

    let transform = Transform::from_row(
      scale_x,
      0.0,
      0.0,
      scale_y,
      self.device_x(x) + dest_x_device,
      self.device_y(y) + dest_y_device,
    );
    self
      .pixmap
      .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
    true
  }

  fn resolved_accent_color(style: &ComputedStyle) -> Rgba {
    match style.accent_color {
      AccentColor::Color(c) => c,
      // `accent-color: auto` is a UA-defined value. Use a stable, browser-like default rather
      // than inheriting from the text color so checkbox/radio controls don't render as black.
      AccentColor::Auto => Rgba::rgb(26, 115, 232),
    }
  }

  fn paint_with_opacity_layer<F>(
    &mut self,
    opacity: f32,
    bounds: Rect,
    clip_mask: Option<&Mask>,
    paint: F,
  ) where
    F: FnOnce(&mut Painter, Option<&Mask>),
  {
    fn crop_clip_mask(
      mask: &Mask,
      origin_x: i32,
      origin_y: i32,
      width: u32,
      height: u32,
    ) -> Option<Mask> {
      if width == 0 || height == 0 {
        return None;
      }
      let mask_w = mask.width() as i32;
      let mask_h = mask.height() as i32;
      if mask_w <= 0 || mask_h <= 0 {
        return None;
      }

      let layer_w = width as i32;
      let layer_h = height as i32;
      let x0 = origin_x.max(0);
      let y0 = origin_y.max(0);
      let x1 = origin_x.saturating_add(layer_w).min(mask_w);
      let y1 = origin_y.saturating_add(layer_h).min(mask_h);
      if x1 <= x0 || y1 <= y0 {
        return None;
      }

      let mut out = Mask::new(width, height)?;
      out.data_mut().fill(0);

      let src = mask.data();
      let dst = out.data_mut();
      let src_stride = mask.width() as usize;
      let dst_stride = width as usize;
      let copy_w = (x1 - x0) as usize;
      let dst_x0 = (x0 - origin_x) as usize;
      let dst_y0 = (y0 - origin_y) as usize;
      let src_x0 = x0 as usize;
      let src_y0 = y0 as usize;
      let copy_h = (y1 - y0) as usize;
      for row in 0..copy_h {
        let src_idx = (src_y0 + row) * src_stride + src_x0;
        let dst_idx = (dst_y0 + row) * dst_stride + dst_x0;
        dst[dst_idx..dst_idx + copy_w].copy_from_slice(&src[src_idx..src_idx + copy_w]);
      }
      Some(out)
    }

    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 {
      return;
    }
    if opacity >= 1.0 - 1e-6 {
      paint(self, clip_mask);
      return;
    }

    let device_bounds = self.device_rect(bounds);
    let x0 = device_bounds.min_x().floor() as i32;
    let y0 = device_bounds.min_y().floor() as i32;
    let x1 = device_bounds.max_x().ceil() as i32;
    let y1 = device_bounds.max_y().ceil() as i32;
    let width_i32 = x1 - x0;
    let height_i32 = y1 - y0;
    if width_i32 <= 0 || height_i32 <= 0 {
      return;
    }
    let width = width_i32 as u32;
    let height = height_i32 as u32;
    if width == 0 || height == 0 {
      return;
    }

    let layer = match new_pixmap(width, height) {
      Some(p) => {
        if self.diagnostics_enabled {
          record_layer_allocation(width, height);
        }
        p
      }
      None => return,
    };

    let scale = self.scale;
    let offset = Point::new(
      self.origin_offset_css.x + x0 as f32 / scale,
      self.origin_offset_css.y + y0 as f32 / scale,
    );
    let mut layer_painter = Painter {
      pixmap: layer,
      scale: self.scale,
      origin_offset_css: offset,
      css_width: self.css_width,
      css_height: self.css_height,
      background: Rgba::new(0, 0, 0, 0.0),
      shaper: ShapingPipeline::new(),
      font_ctx: self.font_ctx.clone(),
      image_cache: self.image_cache.clone(),
      media_provider: self.media_provider.clone(),
      svg_id_defs: self.svg_id_defs.clone(),
      svg_id_defs_raw: self.svg_id_defs_raw.clone(),
      text_shape_cache: Arc::clone(&self.text_shape_cache),
      trace: self.trace.clone(),
      scroll_state: self.scroll_state.clone(),
      max_iframe_depth: self.max_iframe_depth,
      gradient_cache: self.gradient_cache.clone(),
      gradient_stats: GradientStats::default(),
      diagnostics_enabled: self.diagnostics_enabled,
    };

    let cropped_clip_mask = if let Some(mask) = clip_mask {
      let mask_w = mask.width() as i32;
      let mask_h = mask.height() as i32;
      if mask_w <= 0 || mask_h <= 0 {
        None
      } else {
        let layer_w = width as i32;
        let layer_h = height as i32;
        let inter_x0 = x0.max(0);
        let inter_y0 = y0.max(0);
        let inter_x1 = x0.saturating_add(layer_w).min(mask_w);
        let inter_y1 = y0.saturating_add(layer_h).min(mask_h);
        if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
          // Clip mask does not overlap the opacity layer at all.
          return;
        }
        crop_clip_mask(mask, x0, y0, width, height)
      }
    } else {
      None
    };
    let paint_clip_mask = cropped_clip_mask.as_ref();

    paint(&mut layer_painter, paint_clip_mask);
    self.gradient_stats.merge(&layer_painter.gradient_stats);
    let layer_pixmap = layer_painter.pixmap;

    let mut paint = PixmapPaint::default();
    paint.opacity = opacity;
    let composite_clip_mask = if paint_clip_mask.is_some() {
      None
    } else {
      clip_mask
    };
    let transform = Transform::from_translate(x0 as f32, y0 as f32);
    self.pixmap.draw_pixmap(
      0,
      0,
      layer_pixmap.as_ref(),
      &paint,
      transform,
      composite_clip_mask,
    );
  }

  fn paint_form_control(
    &mut self,
    control: &FormControl,
    style: &ComputedStyle,
    content_rect: Rect,
    padding_rect: Rect,
    clip_mask: Option<&Mask>,
    box_id: Option<usize>,
  ) -> bool {
    if content_rect.width() <= 0.0 || content_rect.height() <= 0.0 {
      return true;
    }

    fn fill_rounded_rect_masked(
      pixmap: &mut Pixmap,
      rect: Rect,
      radii: BorderRadii,
      color: Rgba,
      clip_mask: Option<&Mask>,
    ) {
      if color.a <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
      }
      let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        &radii,
      ) else {
        return;
      };
      let mut paint = Paint::default();
      paint.set_color(tiny_skia::Color::from_rgba8(
        color.r,
        color.g,
        color.b,
        color.alpha_u8(),
      ));
      paint.anti_alias = true;
      pixmap.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip_mask,
      );
    }

    fn fill_rect_masked(pixmap: &mut Pixmap, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
      if color.a <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
      }
      let Some(sk_rect) = SkiaRect::from_xywh(rect.x(), rect.y(), rect.width(), rect.height())
      else {
        return;
      };
      let path = PathBuilder::from_rect(sk_rect);
      let mut paint = Paint::default();
      paint.set_color(tiny_skia::Color::from_rgba8(
        color.r,
        color.g,
        color.b,
        color.alpha_u8(),
      ));
      paint.anti_alias = true;
      pixmap.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip_mask,
      );
    }

    fn fill_rect_masked_crisp(
      pixmap: &mut Pixmap,
      rect: Rect,
      color: Rgba,
      clip_mask: Option<&Mask>,
    ) {
      if color.a <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
      }
      let Some(sk_rect) = SkiaRect::from_xywh(rect.x(), rect.y(), rect.width(), rect.height())
      else {
        return;
      };
      let mut paint = Paint::default();
      paint.set_color(tiny_skia::Color::from_rgba8(
        color.r,
        color.g,
        color.b,
        color.alpha_u8(),
      ));
      paint.anti_alias = false;
      pixmap.fill_rect(sk_rect, &paint, Transform::identity(), clip_mask);
    }

    let mut accent = Self::resolved_accent_color(style);
    if control.invalid {
      accent = Rgba {
        r: 212,
        g: 43,
        b: 43,
        a: 1.0,
      };
    }
    let muted_accent = if control.disabled {
      accent.with_alpha((accent.a * 0.7).clamp(0.0, 1.0))
    } else {
      accent
    };
    let inset_rect = |rect: Rect, inset: f32| {
      Rect::from_xywh(
        rect.x() + inset,
        rect.y() + inset,
        (rect.width() - 2.0 * inset).max(0.0),
        (rect.height() - 2.0 * inset).max(0.0),
      )
    };
    // Do not paint internal focus/required/invalid "tint" overlays for native controls.
    //
    // These states are exposed via pseudo-classes (e.g. `:focus-visible`, `:required`, `:invalid`)
    // and should be styled via CSS rather than added as extra paint operations.

    let measure_shaped_advance = |painter: &mut Self, text: &str, style: &ComputedStyle| -> f32 {
      if text.is_empty() {
        return 0.0;
      }

      if style.letter_spacing == 0.0 && style.word_spacing == 0.0 {
        if let Ok(runs) = painter.shaper.shape_arc(text, style, &painter.font_ctx) {
          if !runs.is_empty() {
            return runs.iter().map(|run| run.advance).sum();
          }
        }
        return text.chars().count() as f32 * style.font_size * 0.6;
      }

      let Ok(mut runs) = painter.shaper.shape(text, style, &painter.font_ctx) else {
        return text.chars().count() as f32 * style.font_size * 0.6;
      };
      if runs.is_empty() {
        return text.chars().count() as f32 * style.font_size * 0.6;
      }
      TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
      runs.iter().map(|run| run.advance).sum()
    };
    let shape_text_runs =
      |painter: &mut Self, text: &str, style: &ComputedStyle| -> Vec<ShapedRun> {
        if text.is_empty() {
          return Vec::new();
        }
        painter
          .shaper
          .shape(text, style, &painter.font_ctx)
          .ok()
          .map(|mut runs| {
            TextItem::apply_spacing_to_runs(
              &mut runs,
              text,
              style.letter_spacing,
              style.word_spacing,
            );
            runs
          })
          .unwrap_or_default()
      };

    match &control.control {
      FormControlKind::Text {
        value,
        placeholder,
        placeholder_style,
        kind,
        caret,
        caret_affinity,
        selection,
        ..
      } => {
        let base_color = if control.invalid { accent } else { style.color };
        let placeholder_color = base_color.with_alpha(0.6);
        let value_is_empty = value.is_empty();
        let preedit = control
          .ime_preedit
          .as_ref()
          .filter(|state| !state.text.is_empty());
        let display_is_empty = value_is_empty && preedit.is_none();

        let mut generated: Option<String> = None;
        let mut paint_text: Option<&str> = None;
        let mut fallback_color = base_color;
        let mut is_placeholder = false;
        let mut ime_caret_idx: Option<usize> = None;
        let mut ime_selection: Option<(usize, usize)> = None;
        let mut preedit_range: Option<(usize, usize)> = None;

        let byte_offset_for_char_idx = |text: &str, char_idx: usize| -> usize {
          if char_idx == 0 {
            return 0;
          }
          let mut count = 0usize;
          for (byte_idx, _) in text.char_indices() {
            if count == char_idx {
              return byte_idx;
            }
            count += 1;
          }
          text.len()
        };

        match kind {
          TextControlKind::Password => {
            if !display_is_empty {
              let committed_len = value.chars().count();
              let preedit_len = preedit.map(|state| state.text.chars().count()).unwrap_or(0);
              let (replace_start, replace_end) = if preedit.is_some() {
                (*selection).unwrap_or((*caret, *caret))
              } else {
                (*caret, *caret)
              };
              let replace_start = replace_start.min(committed_len);
              let replace_end = replace_end.min(committed_len);
              let replaced_len = if preedit.is_some() {
                replace_end.saturating_sub(replace_start)
              } else {
                0
              };
              let total_len = committed_len
                .saturating_sub(replaced_len)
                .saturating_add(preedit_len);
              let mask_len = total_len.clamp(3, 50);
              generated = Some("•".repeat(mask_len));
              paint_text = generated.as_deref();
              fallback_color = base_color;

              if let Some(preedit) = preedit {
                let preedit_len = preedit.text.chars().count();
                preedit_range = Some((replace_start, replace_start.saturating_add(preedit_len)));
                let (cursor_start, cursor_end) = preedit
                  .cursor
                  .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
                  .unwrap_or((preedit_len, preedit_len));
                let cursor_start = cursor_start.min(preedit_len);
                let cursor_end = cursor_end.min(preedit_len);
                ime_caret_idx = Some(replace_start.saturating_add(cursor_end));
                if cursor_start != cursor_end {
                  ime_selection = Some((
                    replace_start.saturating_add(cursor_start),
                    replace_start.saturating_add(cursor_end),
                  ));
                }
              }
            } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
              paint_text = Some(ph);
              fallback_color = placeholder_color;
              is_placeholder = true;
            }
          }
          TextControlKind::Number => {
            if let Some(preedit) = preedit {
              let committed_len = value.chars().count();
              let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
              let replace_start = replace_start.min(committed_len);
              let replace_end = replace_end.min(committed_len);
              let start_byte = byte_offset_for_char_idx(value, replace_start);
              let end_byte = byte_offset_for_char_idx(value, replace_end);

              let mut combined = String::with_capacity(
                value
                  .len()
                  .saturating_sub(end_byte.saturating_sub(start_byte))
                  .saturating_add(preedit.text.len()),
              );
              combined.push_str(&value[..start_byte]);
              combined.push_str(&preedit.text);
              combined.push_str(&value[end_byte..]);
              generated = Some(combined);
              paint_text = generated.as_deref();
              fallback_color = base_color;

              let preedit_len = preedit.text.chars().count();
              preedit_range = Some((replace_start, replace_start.saturating_add(preedit_len)));
              let (cursor_start, cursor_end) = preedit
                .cursor
                .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
                .unwrap_or((preedit_len, preedit_len));
              let cursor_start = cursor_start.min(preedit_len);
              let cursor_end = cursor_end.min(preedit_len);
              ime_caret_idx = Some(replace_start.saturating_add(cursor_end));
              if cursor_start != cursor_end {
                ime_selection = Some((
                  replace_start.saturating_add(cursor_start),
                  replace_start.saturating_add(cursor_end),
                ));
              }
            } else if !value_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
              paint_text = Some(ph);
              fallback_color = placeholder_color;
              is_placeholder = true;
            }
          }
          TextControlKind::Date => {
            if let Some(preedit) = preedit {
              let committed_len = value.chars().count();
              let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
              let replace_start = replace_start.min(committed_len);
              let replace_end = replace_end.min(committed_len);
              let start_byte = byte_offset_for_char_idx(value, replace_start);
              let end_byte = byte_offset_for_char_idx(value, replace_end);

              let mut combined = String::with_capacity(
                value
                  .len()
                  .saturating_sub(end_byte.saturating_sub(start_byte))
                  .saturating_add(preedit.text.len()),
              );
              combined.push_str(&value[..start_byte]);
              combined.push_str(&preedit.text);
              combined.push_str(&value[end_byte..]);
              generated = Some(combined);
              paint_text = generated.as_deref();
              fallback_color = base_color;

              let preedit_len = preedit.text.chars().count();
              preedit_range = Some((replace_start, replace_start.saturating_add(preedit_len)));
              let (cursor_start, cursor_end) = preedit
                .cursor
                .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
                .unwrap_or((preedit_len, preedit_len));
              let cursor_start = cursor_start.min(preedit_len);
              let cursor_end = cursor_end.min(preedit_len);
              ime_caret_idx = Some(replace_start.saturating_add(cursor_end));
              if cursor_start != cursor_end {
                ime_selection = Some((
                  replace_start.saturating_add(cursor_start),
                  replace_start.saturating_add(cursor_end),
                ));
              }
            } else if !value_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
              paint_text = Some(ph);
              fallback_color = placeholder_color;
              is_placeholder = true;
            } else {
              paint_text = Some("yyyy-mm-dd");
              fallback_color = placeholder_color;
              is_placeholder = true;
            }
          }
          TextControlKind::Plain => {
            if let Some(preedit) = preedit {
              let committed_len = value.chars().count();
              let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
              let replace_start = replace_start.min(committed_len);
              let replace_end = replace_end.min(committed_len);
              let start_byte = byte_offset_for_char_idx(value, replace_start);
              let end_byte = byte_offset_for_char_idx(value, replace_end);

              let mut combined = String::with_capacity(
                value
                  .len()
                  .saturating_sub(end_byte.saturating_sub(start_byte))
                  .saturating_add(preedit.text.len()),
              );
              combined.push_str(&value[..start_byte]);
              combined.push_str(&preedit.text);
              combined.push_str(&value[end_byte..]);
              generated = Some(combined);
              paint_text = generated.as_deref();
              fallback_color = base_color;

              let preedit_len = preedit.text.chars().count();
              preedit_range = Some((replace_start, replace_start.saturating_add(preedit_len)));
              let (cursor_start, cursor_end) = preedit
                .cursor
                .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
                .unwrap_or((preedit_len, preedit_len));
              let cursor_start = cursor_start.min(preedit_len);
              let cursor_end = cursor_end.min(preedit_len);
              ime_caret_idx = Some(replace_start.saturating_add(cursor_end));
              if cursor_start != cursor_end {
                ime_selection = Some((
                  replace_start.saturating_add(cursor_start),
                  replace_start.saturating_add(cursor_end),
                ));
              }
            } else if !value_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
              paint_text = Some(ph);
              fallback_color = placeholder_color;
              is_placeholder = true;
            }
          }
        }

        let placeholder_pseudo_style = if is_placeholder {
          placeholder_style.as_deref()
        } else {
          None
        };
        let text_style = if let Some(pseudo_style) = placeholder_pseudo_style {
          let mut style = pseudo_style.clone();
          let opacity = style.opacity.clamp(0.0, 1.0);
          style.opacity = 1.0;
          style.color = style
            .color
            .with_alpha((style.color.a * opacity).clamp(0.0, 1.0));
          style
        } else {
          let mut style = style.clone();
          style.color = fallback_color;
          style
        };
        let mut rect = inset_rect(content_rect, 2.0);
        let mut affordance_space = 0.0;
        if !matches!(control.appearance, Appearance::None) {
          match kind {
            TextControlKind::Number => affordance_space = 14.0,
            TextControlKind::Date => affordance_space = 12.0,
            _ => {}
          }
        }
        if affordance_space > 0.0 {
          rect = Rect::from_xywh(
            rect.x(),
            rect.y(),
            (rect.width() - affordance_space).max(0.0),
            rect.height(),
          );
        }

        let metrics_scaled = self.resolve_scaled_metrics(&text_style);
        let line_height = compute_line_height_with_metrics_viewport(
          &text_style,
          metrics_scaled.as_ref(),
          Some(Size::new(self.css_width, self.css_height)),
          self.font_ctx.root_font_metrics(),
        );
        if line_height <= 0.0 || !line_height.is_finite() {
          return true;
        }
        let baseline_offset_y = if line_height.is_finite() {
          (rect.height() - line_height) / 2.0
        } else {
          0.0
        };
        let baseline_offset_y = if baseline_offset_y.is_finite() {
          baseline_offset_y
        } else {
          0.0
        };
        let centered_rect = Rect::from_xywh(
          rect.x(),
          rect.y() + baseline_offset_y,
          rect.width(),
          rect.height(),
        );
        let selection_color = Rgba {
          r: 0,
          g: 120,
          b: 215,
          a: 0.35,
        };
        let mut selection_rects: Vec<Rect> = Vec::new();
        let mut preedit_underline_rects: Vec<Rect> = Vec::new();
        let mut caret_rect: Option<(Rect, Rgba)> = None;

        if control.focused && !control.disabled {
          let mut sample_text = paint_text.unwrap_or("M");
          if trim_ascii_whitespace_html_css(sample_text).is_empty() {
            sample_text = "M";
          }

          let metrics_runs = shape_text_runs(self, sample_text, &text_style);
          let metrics = TextItem::metrics_from_runs(
            &self.font_ctx,
            &metrics_runs,
            line_height,
            text_style.font_size,
          );
          let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
          let baseline_y = rect.y() + baseline_offset_y + half_leading + metrics.baseline_offset;
          let top = baseline_y - metrics.ascent;
          let bottom = baseline_y + metrics.descent;

          let display_text = paint_text.unwrap_or("");
          let text_runs = shape_text_runs(self, display_text, &text_style);
          let fallback_advance = display_text.chars().count() as f32 * text_style.font_size * 0.6;
          let total_advance: f32 = if !text_runs.is_empty() {
            text_runs.iter().map(|run| run.advance).sum()
          } else {
            fallback_advance
          };
          let start_x = Self::aligned_text_start_x(&text_style, rect, total_advance);
          let max_chars = display_text.chars().count();
          let fallback_char_advance = if max_chars > 0 {
            (fallback_advance / max_chars as f32).max(0.0)
          } else {
            0.0
          };
          let caret_stops = caret_stops_for_runs(display_text, &text_runs, total_advance);

          if !matches!(kind, TextControlKind::Password) && text_style.color.a > f32::EPSILON {
            if let Some((pre_start, pre_end)) = preedit_range {
              let pre_start = pre_start.min(max_chars);
              let pre_end = pre_end.min(max_chars);
              if pre_start < pre_end {
                let underline_y = (bottom - 1.0).max(top);
                let segments = if text_runs.is_empty() {
                  let x1 = fallback_char_advance * pre_start as f32;
                  let x2 = fallback_char_advance * pre_end as f32;
                  vec![(x1.min(x2), x1.max(x2))]
                } else {
                  crate::text::caret::selection_segments_for_char_range(
                    display_text,
                    &text_runs,
                    pre_start,
                    pre_end,
                  )
                };
                for (seg_start, seg_end) in segments {
                  let underline_rect = Rect::from_xywh(
                    start_x + seg_start,
                    underline_y,
                    (seg_end - seg_start).max(0.0),
                    1.0,
                  );
                  if let Some(clipped) = underline_rect
                    .intersection(padding_rect)
                    .filter(|r| r.width() > 0.0 && r.height() > 0.0)
                  {
                    preedit_underline_rects.push(clipped);
                  }
                }
              }
            }
          }

          let selection_for_paint = if preedit.is_some() { ime_selection } else { *selection };
          if let Some((sel_start, sel_end)) = selection_for_paint {
            let sel_start = sel_start.min(max_chars);
            let sel_end = sel_end.min(max_chars);
            if sel_start != sel_end {
              let segments = if text_runs.is_empty() {
                let x1 = fallback_char_advance * sel_start as f32;
                let x2 = fallback_char_advance * sel_end as f32;
                vec![(x1.min(x2), x1.max(x2))]
              } else {
                crate::text::caret::selection_segments_for_char_range(
                  display_text,
                  &text_runs,
                  sel_start,
                  sel_end,
                )
              };
              for (seg_start, seg_end) in segments {
                let left = start_x + seg_start;
                let right = start_x + seg_end;
                let sel_rect =
                  Rect::from_xywh(left, top, (right - left).max(0.0), (bottom - top).max(0.0));
                if let Some(clipped) = sel_rect
                  .intersection(rect)
                  .filter(|r| r.width() > 0.0 && r.height() > 0.0)
                {
                  selection_rects.push(clipped);
                }
              }
            }
          }

          let caret_color = match style.caret_color {
            CaretColor::Color(c) => c,
            CaretColor::Auto => style.color,
          };
          if !caret_color.is_transparent() {
            let caret_idx = ime_caret_idx
              .unwrap_or_else(|| (*caret).min(max_chars))
              .min(max_chars);
            let caret_affinity_for_paint = if preedit.is_some() {
              CaretAffinity::Downstream
            } else {
              *caret_affinity
            };
            let caret_x = start_x
              + caret_x_for_position(&caret_stops, caret_idx, caret_affinity_for_paint)
                .unwrap_or(0.0);
            let max_caret_x = (rect.max_x() - 1.0).max(rect.x());
            let caret_x = caret_x.clamp(rect.x(), max_caret_x);

            let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            if let Some(clipped) = caret_rect_raw.intersection(padding_rect) {
              if clipped.width() > 0.0 && clipped.height() > 0.0 {
                caret_rect = Some((clipped, caret_color));
              }
            }
          }
        }

        for selection_rect in selection_rects {
          let device_rect = self.device_rect(selection_rect);
          fill_rect_masked(&mut self.pixmap, device_rect, selection_color, clip_mask);
        }
        if text_style.color.a > f32::EPSILON {
          if let Some(text) = paint_text {
            let _ = self.paint_alt_text_raw(text, &text_style, centered_rect, clip_mask);
          }
        }
        if affordance_space > 0.0 {
          let mut affordance_style = style.clone();
          affordance_style.color = muted_accent;
          let affordance_rect = Rect::from_xywh(
            rect.x() + rect.width(),
            rect.y(),
            affordance_space,
            rect.height(),
          );
          match kind {
            TextControlKind::Number => {
              let half = affordance_rect.height() / 2.0;
              let upper = Rect::from_xywh(
                affordance_rect.x(),
                affordance_rect.y(),
                affordance_rect.width(),
                half,
              );
              let lower = Rect::from_xywh(
                affordance_rect.x(),
                affordance_rect.y() + half,
                affordance_rect.width(),
                affordance_rect.height() - half,
              );
              let _ = self.paint_alt_text_raw("▲", &affordance_style, upper, clip_mask);
              let _ = self.paint_alt_text_raw("▼", &affordance_style, lower, clip_mask);
            }
            TextControlKind::Date => {
              let _ = self.paint_alt_text_raw("▾", &affordance_style, affordance_rect, clip_mask);
            }
            _ => {}
          }
        }

        for underline_rect in preedit_underline_rects {
          let device_rect_raw = self.device_rect(underline_rect);
          let device_rect = Rect::from_xywh(
            device_rect_raw.x().round(),
            device_rect_raw.y().round(),
            device_rect_raw.width().round().max(1.0),
            device_rect_raw.height().round().max(1.0),
          );
          fill_rect_masked_crisp(&mut self.pixmap, device_rect, text_style.color, clip_mask);
        }
        if let Some((caret_rect, caret_color)) = caret_rect {
          let device_rect_raw = self.device_rect(caret_rect);
          let device_rect = Rect::from_xywh(
            device_rect_raw.x().round(),
            device_rect_raw.y().round(),
            device_rect_raw.width().round().max(1.0),
            device_rect_raw.height().round().max(1.0),
          );
          fill_rect_masked_crisp(&mut self.pixmap, device_rect, caret_color, clip_mask);
        }
        true
      }
      FormControlKind::TextArea {
        value,
        placeholder,
        placeholder_style,
        caret,
        caret_affinity,
        selection,
        ..
      } => {
        let base_color = if control.invalid { accent } else { style.color };
        let placeholder_color = base_color.with_alpha(0.6);

        let preedit = control
          .ime_preedit
          .as_ref()
          .filter(|state| !state.text.is_empty());
        let value_is_empty = value.is_empty();
        let rect = inset_rect(content_rect, 2.0);

        let mut generated: Option<String> = None;
        let mut paint_text: Option<&str> = None;
        let mut fallback_color = base_color;
        let mut is_placeholder = false;
        let mut ime_caret_idx: Option<usize> = None;
        let mut ime_selection: Option<(usize, usize)> = None;
        let mut preedit_range: Option<(usize, usize)> = None;

        let byte_offset_for_char_idx = |text: &str, char_idx: usize| -> usize {
          if char_idx == 0 {
            return 0;
          }
          let mut count = 0usize;
          for (byte_idx, _) in text.char_indices() {
            if count == char_idx {
              return byte_idx;
            }
            count += 1;
          }
          text.len()
        };

        if let Some(preedit) = preedit {
          let committed_len = value.chars().count();
          let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
          let replace_start = replace_start.min(committed_len);
          let replace_end = replace_end.min(committed_len);
          let start_byte = byte_offset_for_char_idx(value, replace_start);
          let end_byte = byte_offset_for_char_idx(value, replace_end);

          let mut combined = String::with_capacity(
            value
              .len()
              .saturating_sub(end_byte.saturating_sub(start_byte))
              .saturating_add(preedit.text.len()),
          );
          combined.push_str(&value[..start_byte]);
          combined.push_str(&preedit.text);
          combined.push_str(&value[end_byte..]);
          generated = Some(combined);
          paint_text = generated.as_deref();
          fallback_color = base_color;

          let preedit_len = preedit.text.chars().count();
          preedit_range = Some((replace_start, replace_start.saturating_add(preedit_len)));
          let (cursor_start, cursor_end) = preedit
            .cursor
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
            .unwrap_or((preedit_len, preedit_len));
          let cursor_start = cursor_start.min(preedit_len);
          let cursor_end = cursor_end.min(preedit_len);
          ime_caret_idx = Some(replace_start.saturating_add(cursor_end));
          if cursor_start != cursor_end {
            ime_selection = Some((
              replace_start.saturating_add(cursor_start),
              replace_start.saturating_add(cursor_end),
            ));
          }
        } else if !value_is_empty {
          paint_text = Some(value.as_str());
          fallback_color = base_color;
        } else if let Some(placeholder) = placeholder.as_deref().filter(|p| !p.is_empty()) {
          paint_text = Some(placeholder);
          fallback_color = placeholder_color;
          is_placeholder = true;
        }

        let placeholder_pseudo_style = if is_placeholder {
          placeholder_style.as_deref()
        } else {
          None
        };
        let text_style = if let Some(pseudo_style) = placeholder_pseudo_style {
          let mut style = pseudo_style.clone();
          let opacity = style.opacity.clamp(0.0, 1.0);
          style.opacity = 1.0;
          style.color = style
            .color
            .with_alpha((style.color.a * opacity).clamp(0.0, 1.0));
          style
        } else {
          let mut style = style.clone();
          style.color = fallback_color;
          style
        };
        let selection_color = Rgba {
          r: 0,
          g: 120,
          b: 215,
          a: 0.35,
        };
        let metrics_scaled = self.resolve_scaled_metrics(&text_style);
        let line_height = compute_line_height_with_metrics_viewport(
          &text_style,
          metrics_scaled.as_ref(),
          Some(Size::new(self.css_width, self.css_height)),
          self.font_ctx.root_font_metrics(),
        );
        if line_height <= 0.0 || !line_height.is_finite() {
          return true;
        }

        let display_text = paint_text.unwrap_or("");
        let max_chars = display_text.chars().count();
        let caret_idx = ime_caret_idx
          .unwrap_or_else(|| (*caret).min(max_chars))
          .min(max_chars);
        let caret_affinity_for_paint = if preedit.is_some() {
          CaretAffinity::Downstream
        } else {
          *caret_affinity
        };
        let selection_for_paint = if preedit.is_some() { ime_selection } else { *selection };
        let selection =
          selection_for_paint.map(|(start, end)| (start.min(max_chars), end.min(max_chars)));
        let mut caret_rect: Option<(Rect, Rgba)> = None;

        let mut metrics_sample = display_text;
        if trim_ascii_whitespace_html_css(metrics_sample).is_empty() {
          metrics_sample = "M";
        }
        let metrics_runs = shape_text_runs(self, metrics_sample, &text_style);
        let metrics = TextItem::metrics_from_runs(
          &self.font_ctx,
          &metrics_runs,
          line_height,
          text_style.font_size,
        );
        let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;

        let caret_color = match style.caret_color {
          CaretColor::Color(c) => c,
          CaretColor::Auto => style.color,
        };

        let mut scroll_y = box_id
          .map(|id| self.scroll_state.element_offset(id).y)
          .unwrap_or(0.0);
        if !scroll_y.is_finite() {
          scroll_y = 0.0;
        }
        scroll_y = scroll_y.max(0.0);

        let chars_per_line = crate::textarea::textarea_chars_per_line(&text_style, rect.width());
        let layout = crate::textarea::build_textarea_visual_lines(display_text, chars_per_line);
        let content_height = layout.lines.len() as f32 * line_height;
        let viewport_height = rect.height().max(0.0);
        let max_scroll_y = if content_height.is_finite() {
          (content_height - viewport_height).max(0.0)
        } else {
          0.0
        };
        if max_scroll_y.is_finite() {
          scroll_y = scroll_y.clamp(0.0, max_scroll_y);
        }

        let mut local_clip_mask: Option<Mask> = None;
        let textarea_clip_mask = clip_mask.or_else(|| {
          let canvas_w = self.pixmap.width();
          let canvas_h = self.pixmap.height();
          local_clip_mask = build_rounded_rect_mask(
            self.device_rect(rect),
            BorderRadii::ZERO,
            canvas_w,
            canvas_h,
          );
          local_clip_mask.as_ref()
        });

        let caret_line_idx = crate::textarea::textarea_visual_line_index_for_caret(
          display_text,
          &layout,
          caret_idx,
        );

        let start_line = (scroll_y / line_height).floor().max(0.0) as usize;
        let end_line = ((scroll_y + viewport_height) / line_height)
          .ceil()
          .max(start_line as f32) as usize;
        let end_line = end_line.min(layout.lines.len());
        let y_offset = scroll_y - start_line as f32 * line_height;
        let mut y = rect.y() - y_offset;

        for line_idx in start_line..end_line {
          let Some(line) = layout.lines.get(line_idx).copied() else {
            continue;
          };
          let line_text = line.text(display_text);
          let line_len = line.len_chars();
          let line_rect = Rect::from_xywh(rect.x(), y, rect.width(), line_height);
          y += line_height;

          if line_rect.max_y() <= rect.y() || line_rect.y() >= rect.max_y() {
            continue;
          }
          if line_rect.width() <= 0.0 || line_rect.height() <= 0.0 {
            continue;
          }

          let line_runs = shape_text_runs(self, line_text, &text_style);
          let fallback_advance = line_len as f32 * text_style.font_size * 0.6;
          let total_advance: f32 = if !line_runs.is_empty() {
            line_runs.iter().map(|run| run.advance).sum()
          } else {
            fallback_advance
          };
          let start_x = Self::aligned_text_start_x(&text_style, line_rect, total_advance);
          let caret_stops = caret_stops_for_runs(line_text, &line_runs, total_advance);

          let baseline_y = line_rect.y() + half_leading + metrics.baseline_offset;
          let top = baseline_y - metrics.ascent;
          let bottom = baseline_y + metrics.descent;

          if control.focused && !control.disabled {
            if let Some((sel_start, sel_end)) = selection {
              let seg_start = sel_start.max(line.start_char).min(line.end_char);
              let seg_end = sel_end.max(line.start_char).min(line.end_char);
              if seg_start < seg_end && line_len > 0 {
                let start_col = seg_start - line.start_char;
                let end_col = seg_end - line.start_char;
                let fallback_char_advance = if line_len > 0 {
                  (fallback_advance / line_len as f32).max(0.0)
                } else {
                  0.0
                };
                let segments = if line_runs.is_empty() {
                  let x1 = fallback_char_advance * start_col as f32;
                  let x2 = fallback_char_advance * end_col as f32;
                  vec![(x1.min(x2), x1.max(x2))]
                } else {
                  crate::text::caret::selection_segments_for_char_range(
                    line_text,
                    &line_runs,
                    start_col,
                    end_col,
                  )
                };
                for (seg_start, seg_end) in segments {
                  let left = start_x + seg_start;
                  let right = start_x + seg_end;
                  let sel_rect =
                    Rect::from_xywh(left, top, (right - left).max(0.0), (bottom - top).max(0.0));
                  if let Some(clipped) = sel_rect.intersection(rect) {
                    if clipped.width() > 0.0 && clipped.height() > 0.0 {
                      let device_rect = self.device_rect(clipped);
                      fill_rect_masked(
                        &mut self.pixmap,
                        device_rect,
                        selection_color,
                        textarea_clip_mask,
                      );
                    }
                  }
                }
              }
            }
          }

          if text_style.color.a > f32::EPSILON && !line_text.is_empty() {
            let _ = self.paint_alt_text_raw(
              line_text,
              &text_style,
              line_rect,
              textarea_clip_mask,
            );
          }

          if control.focused && !control.disabled {
            if let Some((pre_start, pre_end)) = preedit_range {
              let seg_start = pre_start.max(line.start_char).min(line.end_char);
              let seg_end = pre_end.max(line.start_char).min(line.end_char);
              if seg_start < seg_end && text_style.color.a > f32::EPSILON && line_len > 0 {
                let start_col = seg_start - line.start_char;
                let end_col = seg_end - line.start_char;
                let underline_y = (bottom - 1.0).max(top);
                let fallback_char_advance = if line_len > 0 {
                  (fallback_advance / line_len as f32).max(0.0)
                } else {
                  0.0
                };
                let segments = if line_runs.is_empty() {
                  let x1 = fallback_char_advance * start_col as f32;
                  let x2 = fallback_char_advance * end_col as f32;
                  vec![(x1.min(x2), x1.max(x2))]
                } else {
                  crate::text::caret::selection_segments_for_char_range(
                    line_text,
                    &line_runs,
                    start_col,
                    end_col,
                  )
                };
                for (seg_start, seg_end) in segments {
                  let underline_rect = Rect::from_xywh(
                    start_x + seg_start,
                    underline_y,
                    (seg_end - seg_start).max(0.0),
                    1.0,
                  );
                  if let Some(clipped) = underline_rect.intersection(rect) {
                    if clipped.width() > 0.0 && clipped.height() > 0.0 {
                      let device_rect_raw = self.device_rect(clipped);
                      let device_rect = Rect::from_xywh(
                        device_rect_raw.x().round(),
                        device_rect_raw.y().round(),
                        device_rect_raw.width().round().max(1.0),
                        device_rect_raw.height().round().max(1.0),
                      );
                      fill_rect_masked_crisp(
                        &mut self.pixmap,
                        device_rect,
                        text_style.color,
                        textarea_clip_mask,
                      );
                    }
                  }
                }
              }
            }
          }

          if caret_rect.is_none()
            && line_idx == caret_line_idx
            && control.focused
            && !control.disabled
            && !caret_color.is_transparent()
          {
            let caret_col = caret_idx.saturating_sub(line.start_char).min(line_len);
            let caret_x = start_x
              + caret_x_for_position(&caret_stops, caret_col, caret_affinity_for_paint).unwrap_or(0.0);
            let max_caret_x = (line_rect.max_x() - 1.0).max(line_rect.x());
            let caret_x = caret_x.clamp(line_rect.x(), max_caret_x);

            let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            caret_rect = caret_rect_raw
              .intersection(rect)
              .filter(|r| r.width() > 0.0 && r.height() > 0.0)
              .map(|r| (r, caret_color));
          }
        }

        if let Some((caret_rect, caret_color)) = caret_rect {
          let device_rect_raw = self.device_rect(caret_rect);
          let device_rect = Rect::from_xywh(
            device_rect_raw.x().round(),
            device_rect_raw.y().round(),
            device_rect_raw.width().round().max(1.0),
            device_rect_raw.height().round().max(1.0),
          );
          fill_rect_masked_crisp(&mut self.pixmap, device_rect, caret_color, clip_mask);
        }
        true
      }
      FormControlKind::Select(select) => {
        let is_listbox = select.multiple || select.size > 1;

        if is_listbox {
          let metrics = self.resolve_scaled_metrics(style);
          // Keep listbox row geometry in sync with interaction hit-testing:
          // - base row height from `line-height`,
          // - but when the listbox is explicitly taller than its intrinsic size, stretch rows so
          //   exactly `size` rows fill the content rect.
          let mut row_height = compute_line_height_with_metrics_viewport(
            style,
            metrics.as_ref(),
            Some(Size::new(self.css_width, self.css_height)),
            self.font_ctx.root_font_metrics(),
          );
          if row_height <= 0.0 || !row_height.is_finite() {
            return true;
          }

          let total_rows = select.items.len();
          let viewport_height = content_rect.height().max(0.0);
          let size_rows = select.size.max(1) as f32;
          let stretched_row_height = viewport_height / size_rows;
          if stretched_row_height.is_finite() && stretched_row_height > 0.0 {
            row_height = row_height.max(stretched_row_height);
          }
          let content_height = row_height * total_rows as f32;
          let mut scroll_y = box_id
            .map(|id| self.scroll_state.element_offset(id).y)
            .unwrap_or(0.0);
          if !scroll_y.is_finite() {
            scroll_y = 0.0;
          }
          let max_scroll_y = (content_height - viewport_height).max(0.0);
          scroll_y = scroll_y.clamp(0.0, max_scroll_y);

          let scrollbar_width = if max_scroll_y > 0.0 {
            crate::layout::utils::resolve_scrollbar_width(style).min(content_rect.width().max(0.0))
          } else {
            0.0
          };

          let text_rect = Rect::from_xywh(
            content_rect.x(),
            content_rect.y(),
            (content_rect.width() - scrollbar_width).max(0.0),
            content_rect.height(),
          );

          let mut local_clip_mask: Option<Mask> = None;
          let list_clip_mask = clip_mask.or_else(|| {
            let canvas_w = self.pixmap.width();
            let canvas_h = self.pixmap.height();
            local_clip_mask = build_rounded_rect_mask(
              self.device_rect(content_rect),
              BorderRadii::ZERO,
              canvas_w,
              canvas_h,
            );
            local_clip_mask.as_ref()
          });

          let start_row = (scroll_y / row_height).floor().max(0.0) as usize;
          let end_row = ((scroll_y + viewport_height) / row_height)
            .ceil()
            .max(start_row as f32) as usize;
          let end_row = end_row.min(total_rows);
          let y_offset = scroll_y - start_row as f32 * row_height;
          let mut y = content_rect.y() - y_offset;

          for item_idx in start_row..end_row {
            let row_rect = Rect::from_xywh(text_rect.x(), y, text_rect.width(), row_height);
            y += row_height;

            if row_rect.max_y() <= content_rect.y() || row_rect.y() >= content_rect.max_y() {
              continue;
            }
            if row_rect.width() <= 0.0 || row_rect.height() <= 0.0 {
              continue;
            }

            match &select.items[item_idx] {
              SelectItem::OptGroupLabel { label, disabled } => {
                let row_disabled = control.disabled || *disabled;
                let mut row_style = style.clone();
                row_style.font_weight = crate::style::types::FontWeight::Bold;
                row_style.color = if row_disabled {
                  style.color.with_alpha(0.5)
                } else {
                  style.color.with_alpha(0.75)
                };
                let row_rect = Rect::from_xywh(
                  row_rect.x() + 2.0,
                  row_rect.y(),
                  (row_rect.width() - 2.0).max(0.0),
                  row_rect.height(),
                );
                let _ = self.paint_alt_text_raw(label, &row_style, row_rect, list_clip_mask);
              }
              SelectItem::Option {
                label,
                selected,
                disabled,
                in_optgroup,
                ..
              } => {
                let row_disabled = control.disabled || *disabled;
                if *selected {
                  let mut alpha = (muted_accent.a * 0.25).max(0.15).min(0.4);
                  if row_disabled {
                    alpha = (alpha * 0.6).max(0.1);
                  }
                  let highlight = muted_accent.with_alpha(alpha);
                  let device_rect = self.device_rect(row_rect);
                  fill_rounded_rect_masked(
                    &mut self.pixmap,
                    device_rect,
                    BorderRadii::ZERO,
                    highlight,
                    list_clip_mask,
                  );
                }

                let mut row_style = style.clone();
                row_style.color = if row_disabled {
                  style.color.with_alpha(0.5)
                } else if control.invalid {
                  accent
                } else {
                  style.color
                };
                let indent = if *in_optgroup { 10.0 } else { 2.0 };
                let row_rect = Rect::from_xywh(
                  row_rect.x() + indent,
                  row_rect.y(),
                  (row_rect.width() - indent).max(0.0),
                  row_rect.height(),
                );
                let _ = self.paint_alt_text_raw(label, &row_style, row_rect, list_clip_mask);
              }
            }
          }

          if scrollbar_width > 0.0 && max_scroll_y > 0.0 && content_rect.height() > 0.0 {
            let track_rect = Rect::from_xywh(
              content_rect.max_x() - scrollbar_width,
              content_rect.y(),
              scrollbar_width,
              content_rect.height(),
            );
            let device_track = self.device_rect(track_rect);
            fill_rounded_rect_masked(
              &mut self.pixmap,
              device_track,
              BorderRadii::ZERO,
              Rgba::rgb(240, 240, 240),
              list_clip_mask,
            );

            let visible_fraction = (viewport_height / content_height.max(1.0)).clamp(0.0, 1.0);
            let thumb_height = (track_rect.height() * visible_fraction).max(8.0);
            let thumb_height = thumb_height.min(track_rect.height());
            let travel = (track_rect.height() - thumb_height).max(0.0);
            let thumb_y = track_rect.y() + travel * (scroll_y / max_scroll_y.max(1.0));
            let thumb_rect =
              Rect::from_xywh(track_rect.x(), thumb_y, track_rect.width(), thumb_height);
            let device_thumb = self.device_rect(thumb_rect);
            fill_rounded_rect_masked(
              &mut self.pixmap,
              device_thumb,
              BorderRadii::ZERO,
              Rgba::rgb(180, 180, 180),
              list_clip_mask,
            );
          }

          true
        } else {
          let label = select
            .selected
            .first()
            .and_then(|&idx| match select.items.get(idx) {
              Some(SelectItem::Option { label, .. }) => Some(label.as_str()),
              _ => None,
            })
            .unwrap_or("Select");

          let rect = inset_rect(content_rect, 2.0);
          let mut select_style = style.clone();
          if control.invalid {
            select_style.color = accent;
          }

          let arrow_space = if matches!(control.appearance, Appearance::None) {
            0.0
          } else {
            14.0_f32.min(padding_rect.width().max(0.0))
          };
          // The `<select>` value is painted in the content box. The arrow affordance is a UA
          // decoration that lives in the padding area when the author provides enough
          // `padding-inline-end` space.
          let mut value_rect = rect;
          let mut arrow_rect: Option<Rect> = None;
          let mut arrow_needs_content_reserve = false;
          if arrow_space > 0.0 {
            use crate::style::types::Direction;

            let padding_start = (content_rect.x() - padding_rect.x()).max(0.0);
            let padding_end = (padding_rect.max_x() - content_rect.max_x()).max(0.0);
            let padding_for_arrow = if style.direction == Direction::Rtl {
              padding_start
            } else {
              padding_end
            };

            let (resolved_arrow_rect, needs_content_reserve) = if padding_for_arrow >= arrow_space {
              // Enough padding space: paint entirely in the padding box.
              let arrow_rect = if style.direction == Direction::Rtl {
                Rect::from_xywh(padding_rect.x(), rect.y(), arrow_space, rect.height())
              } else {
                Rect::from_xywh(
                  padding_rect.max_x() - arrow_space,
                  rect.y(),
                  arrow_space,
                  rect.height(),
                )
              };
              (arrow_rect, false)
            } else {
              // Not enough padding space: fall back to painting in the content box and reserving
              // width from the value so we don't overlap.
              let arrow_rect = if style.direction == Direction::Rtl {
                Rect::from_xywh(rect.x(), rect.y(), arrow_space, rect.height())
              } else {
                Rect::from_xywh(
                  rect.max_x() - arrow_space,
                  rect.y(),
                  arrow_space,
                  rect.height(),
                )
              };
              (arrow_rect, true)
            };
            arrow_rect = Some(resolved_arrow_rect);
            arrow_needs_content_reserve = needs_content_reserve;

            if arrow_needs_content_reserve {
              value_rect = if style.direction == Direction::Rtl {
                Rect::from_xywh(
                  rect.x() + arrow_space,
                  rect.y(),
                  (rect.width() - arrow_space).max(0.0),
                  rect.height(),
                )
              } else {
                Rect::from_xywh(
                  rect.x(),
                  rect.y(),
                  (rect.width() - arrow_space).max(0.0),
                  rect.height(),
                )
              };
            }
          }
          let _ = self.paint_alt_text_raw(label, &select_style, value_rect, clip_mask);

          if let Some(arrow_rect) = arrow_rect {
            if arrow_rect.width() <= 0.0 || arrow_rect.height() <= 0.0 {
              return true;
            }
            let mut arrow_clip_mask_guard = None;
            let arrow_clip_mask = if arrow_needs_content_reserve
              || style.overflow_x != crate::style::types::Overflow::Visible
              || style.overflow_y != crate::style::types::Overflow::Visible
              || style.containment.paint
            {
              clip_mask
            } else {
              // Internal form-control clipping uses the content box by default so values don't
              // bleed into padding. The `<select>` arrow lives in padding, so override the clip
              // mask to the padding box.
              let canvas_w = self.pixmap.width();
              let canvas_h = self.pixmap.height();
              let device_padding = self.device_rect(padding_rect);
              arrow_clip_mask_guard = Some(BackgroundClipMaskGuard::take());
              arrow_clip_mask_guard
                .as_mut()
                .and_then(|guard| guard.mask(device_padding, BorderRadii::ZERO, canvas_w, canvas_h))
            };
            let mut arrow_style = select_style;
            arrow_style.color = muted_accent;
            arrow_style.font_size = (style.font_size * 0.9).max(8.0);
            arrow_style.text_align = crate::style::types::TextAlign::Center;
            let _ = self.paint_alt_text_raw("▾", &arrow_style, arrow_rect, arrow_clip_mask);
          }
          true
        }
      }
      FormControlKind::Button { label } => {
        if label.is_empty() {
          return true;
        }

        let mut button_style = style.clone();
        if control.invalid {
          button_style.color = accent;
        }

        // Keep vertical centering (matching text inputs), but let `paint_alt_text_raw` compute the
        // horizontal start position from computed `text-align` (and direction) via
        // `aligned_text_start_x`/`effective_text_align`.
        //
        // Note: this preserves the historical centered default because the UA stylesheet sets
        // `text-align: center` for input buttons.
        let metrics_scaled = self.resolve_scaled_metrics(&button_style);
        let line_height = compute_line_height_with_metrics_viewport(
          &button_style,
          metrics_scaled.as_ref(),
          Some(Size::new(self.css_width, self.css_height)),
          self.font_ctx.root_font_metrics(),
        );
        let baseline_offset_y = if line_height.is_finite() {
          (content_rect.height() - line_height) / 2.0
        } else {
          0.0
        };
        let baseline_offset_y = if baseline_offset_y.is_finite() {
          baseline_offset_y
        } else {
          0.0
        };
        let centered_rect = Rect::from_xywh(
          content_rect.x(),
          content_rect.y() + baseline_offset_y,
          content_rect.width(),
          content_rect.height(),
        );
        let _ = self.paint_alt_text_raw(label, &button_style, centered_rect, clip_mask);
        true
      }
      FormControlKind::Checkbox {
        is_radio,
        checked,
        indeterminate,
      } => {
        if (!*checked && !*indeterminate) || matches!(control.appearance, Appearance::None) {
          return true;
        }
        if *is_radio {
          if *checked {
            let diameter = content_rect.width().min(content_rect.height()) * 0.55;
            let dot_rect = Rect::from_xywh(
              content_rect.x() + (content_rect.width() - diameter) / 2.0,
              content_rect.y() + (content_rect.height() - diameter) / 2.0,
              diameter,
              diameter,
            );
            let radii = BorderRadii::uniform(diameter / 2.0);
            let device_rect = self.device_rect(dot_rect);
            let radii = self.device_radii(radii);
            let _ = fill_rounded_rect(
              &mut self.pixmap,
              device_rect.x(),
              device_rect.y(),
              device_rect.width(),
              device_rect.height(),
              &radii,
              muted_accent,
            );
          }
          return true;
        }

        let fill_rect = content_rect;
        let radii =
          BorderRadii::uniform((fill_rect.height().min(fill_rect.width()) / 6.0).max(2.0));
        let device_rect = self.device_rect(fill_rect);
        let device_radii = self.device_radii(radii);
        let _ = fill_rounded_rect(
          &mut self.pixmap,
          device_rect.x(),
          device_rect.y(),
          device_rect.width(),
          device_rect.height(),
          &device_radii,
          muted_accent,
        );

        let luminance = (0.299 * muted_accent.r as f32
          + 0.587 * muted_accent.g as f32
          + 0.114 * muted_accent.b as f32)
          / 255.0;
        let mark_color = if luminance > 0.5 {
          Rgba::rgb(24, 24, 24)
        } else {
          Rgba::rgb(245, 245, 245)
        };
        let glyph = if *indeterminate { "−" } else { "✓" };
        let mut mark_style = style.clone();
        mark_style.color = mark_color;
        mark_style.font_size = (fill_rect.height() * 0.9).max(8.0);

        let mark_rect = if let Some(size) = self.measure_alt_text(glyph, &mark_style) {
          let start_x = fill_rect.x() + ((fill_rect.width() - size.width).max(0.0) / 2.0);
          let start_y = fill_rect.y() + ((fill_rect.height() - size.height).max(0.0) / 2.0);
          Rect::from_xywh(
            start_x,
            start_y,
            size.width.min(fill_rect.width()),
            size.height.min(fill_rect.height()),
          )
        } else {
          fill_rect
        };
        let _ = self.paint_alt_text(glyph, &mark_style, mark_rect, clip_mask);
        true
      }
      FormControlKind::Range { value, min, max } => {
        let appearance_none = matches!(control.appearance, Appearance::None);
        let min_val = *min;
        let max_val = *max;
        let span = (max_val - min_val).abs().max(0.0001);
        let clamped = ((*value - min_val) / span).clamp(0.0, 1.0);
        let track_style = control.slider_track_style.as_deref();
        let thumb_style = control.slider_thumb_style.as_deref();
        let viewport = (self.css_width, self.css_height);

        let resolve_px = |style: &ComputedStyle, len: Length, percentage_base: f32| -> f32 {
          resolve_length_for_paint(
            &len,
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
        };

        let paint_outset_box_shadows =
          |painter: &mut Painter,
           rect: Rect,
           radii: BorderRadii,
           style: &ComputedStyle,
           clip_mask: Option<&Mask>| {
            if style.box_shadow.is_empty() || rect.width() <= 0.0 || rect.height() <= 0.0 {
              return;
            }
            let percentage_base = rect.width().max(0.0);
            let device_rect = painter.device_rect(rect);
            let device_radii = painter.device_radii(radii);
            // CSS Backgrounds and Borders: box-shadow lists are ordered front-to-back (first is on
            // top), but we paint back-to-front so later shadows don't cover earlier ones.
            for shadow in style.box_shadow.iter().rev() {
              if shadow.inset {
                continue;
              }
              let offset_x = resolve_length_for_paint(
                &shadow.offset_x,
                style.font_size,
                style.root_font_size,
                percentage_base,
                viewport,
              );
              let offset_y = resolve_length_for_paint(
                &shadow.offset_y,
                style.font_size,
                style.root_font_size,
                percentage_base,
                viewport,
              );
              let blur_radius = resolve_length_for_paint(
                &shadow.blur_radius,
                style.font_size,
                style.root_font_size,
                percentage_base,
                viewport,
              )
              .max(0.0);
              let spread = resolve_length_for_paint(
                &shadow.spread_radius,
                style.font_size,
                style.root_font_size,
                percentage_base,
                viewport,
              )
              .max(-1e6);
              if !offset_x.is_finite()
                || !offset_y.is_finite()
                || !blur_radius.is_finite()
                || !spread.is_finite()
              {
                continue;
              }
              let shadow = crate::paint::rasterize::BoxShadow {
                offset_x: painter.device_length(offset_x),
                offset_y: painter.device_length(offset_y),
                blur_radius: painter.device_length(blur_radius),
                spread_radius: painter.device_length(spread),
                color: shadow.color,
                inset: false,
              };
              let _ = crate::paint::rasterize::render_box_shadow_masked(
                &mut painter.pixmap,
                device_rect.x(),
                device_rect.y(),
                device_rect.width(),
                device_rect.height(),
                &device_radii,
                &shadow,
                clip_mask,
              );
            }
          };

        let paint_rounded_border = |painter: &mut Painter,
                                    rect: Rect,
                                    device_rect: Rect,
                                    radii: BorderRadii,
                                    style: &ComputedStyle,
                                    clip_mask: Option<&Mask>| {
          let border_width = resolve_length_for_paint(
            &style.used_border_top_width(),
            style.font_size,
            style.root_font_size,
            rect.width().max(0.0),
            viewport,
          )
          .max(0.0);
          let border_style = style.border_top_style;
          let border_color = style.border_top_color;
          if border_width <= 0.0
            || matches!(
              border_style,
              crate::style::types::BorderStyle::None | crate::style::types::BorderStyle::Hidden
            )
            || border_color.is_transparent()
          {
            return;
          }

          let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
            device_rect.x(),
            device_rect.y(),
            device_rect.width(),
            device_rect.height(),
            &radii,
          ) else {
            return;
          };
          let mut paint = Paint::default();
          paint.set_color(tiny_skia::Color::from_rgba8(
            border_color.r,
            border_color.g,
            border_color.b,
            border_color.alpha_u8(),
          ));
          paint.anti_alias = true;
          let stroke = tiny_skia::Stroke {
            width: border_width * painter.scale,
            ..Default::default()
          };
          painter
            .pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
        };

        let default_knob_diameter = padding_rect.height().min(16.0);
        let knob_width = thumb_style
          .and_then(|style| {
            style
              .width
              .map(|len| resolve_px(style, len, padding_rect.width()))
          })
          .filter(|px| px.is_finite() && *px > 0.0)
          .unwrap_or(default_knob_diameter);
        let knob_height = thumb_style
          .and_then(|style| {
            style
              .height
              .map(|len| resolve_px(style, len, padding_rect.height()))
          })
          .filter(|px| px.is_finite() && *px > 0.0)
          .unwrap_or(default_knob_diameter);

        let knob_travel = (padding_rect.width() - knob_width).max(0.0);
        let knob_center_x = if style.direction == crate::style::types::Direction::Rtl {
          padding_rect.max_x() - knob_width / 2.0 - clamped * knob_travel
        } else {
          padding_rect.x() + knob_width / 2.0 + clamped * knob_travel
        };
        let mut knob_center_y = padding_rect.y() + padding_rect.height() / 2.0;
        if let Some(thumb_style) = thumb_style {
          if let Some(margin_top) = thumb_style.margin_top {
            knob_center_y += resolve_px(thumb_style, margin_top, padding_rect.height());
          }
        }
        let knob_rect = Rect::from_xywh(
          knob_center_x - knob_width / 2.0,
          knob_center_y - knob_height / 2.0,
          knob_width,
          knob_height,
        );

        let border_radius_is_zero = |style: &ComputedStyle| {
          style.border_top_left_radius.x.is_zero()
            && style.border_top_left_radius.y.is_zero()
            && style.border_top_right_radius.x.is_zero()
            && style.border_top_right_radius.y.is_zero()
            && style.border_bottom_right_radius.x.is_zero()
            && style.border_bottom_right_radius.y.is_zero()
            && style.border_bottom_left_radius.x.is_zero()
            && style.border_bottom_left_radius.y.is_zero()
        };

        // Default range styling draws a track + accent fill; `appearance: none` should avoid
        // injecting UA-specific fills, but author track pseudo-element styling is still expected
        // to paint.
        let should_paint_track = if appearance_none {
          track_style.is_some()
        } else {
          true
        };

        let opacity_layer_bounds = |rect: Rect, style: &ComputedStyle| -> Rect {
          if clip_mask.is_some() {
            return padding_rect;
          }

          let percentage_base = rect.width().max(0.0);
          let border_left = resolve_length_for_paint(
            &style.used_border_left_width(),
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
          .max(0.0)
            / 2.0;
          let border_right = resolve_length_for_paint(
            &style.used_border_right_width(),
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
          .max(0.0)
            / 2.0;
          let border_top = resolve_length_for_paint(
            &style.used_border_top_width(),
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
          .max(0.0)
            / 2.0;
          let border_bottom = resolve_length_for_paint(
            &style.used_border_bottom_width(),
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
          .max(0.0)
            / 2.0;

          let mut left = border_left;
          let mut right = border_right;
          let mut top = border_top;
          let mut bottom = border_bottom;

          for shadow in &style.box_shadow {
            if shadow.inset {
              continue;
            }
            let offset_x = resolve_length_for_paint(
              &shadow.offset_x,
              style.font_size,
              style.root_font_size,
              percentage_base,
              viewport,
            );
            let offset_y = resolve_length_for_paint(
              &shadow.offset_y,
              style.font_size,
              style.root_font_size,
              percentage_base,
              viewport,
            );
            let blur_radius = resolve_length_for_paint(
              &shadow.blur_radius,
              style.font_size,
              style.root_font_size,
              percentage_base,
              viewport,
            )
            .max(0.0);
            let spread = resolve_length_for_paint(
              &shadow.spread_radius,
              style.font_size,
              style.root_font_size,
              percentage_base,
              viewport,
            )
            .max(-1e6);
            if !offset_x.is_finite()
              || !offset_y.is_finite()
              || !blur_radius.is_finite()
              || !spread.is_finite()
            {
              continue;
            }
            let blur_sigma = crate::paint::rasterize::box_shadow_blur_radius_to_sigma(blur_radius);
            let delta = (blur_sigma * 3.0 + spread).max(0.0);
            left = left.max((delta - offset_x).max(0.0));
            right = right.max((delta + offset_x).max(0.0));
            top = top.max((delta - offset_y).max(0.0));
            bottom = bottom.max((delta + offset_y).max(0.0));
          }

          Rect::from_xywh(
            rect.x() - left,
            rect.y() - top,
            (rect.width() + left + right).max(0.0),
            (rect.height() + top + bottom).max(0.0),
          )
        };

        let paint_track =
          |painter: &mut Painter, clip_mask: Option<&Mask>, track_rect: Rect, track_height: f32| {
            if let Some(track_style) = track_style {
              let track_radii = resolve_border_radii(Some(track_style), track_rect);
              paint_outset_box_shadows(painter, track_rect, track_radii, track_style, clip_mask);
              painter.paint_background(
                track_rect.x(),
                track_rect.y(),
                track_rect.width(),
                track_rect.height(),
                track_style,
                Point::ZERO,
              );
            } else {
              let radii = BorderRadii::uniform(track_height / 2.0);
              let device_track_rect = painter.device_rect(track_rect);
              let device_radii = painter.device_radii(radii);
              fill_rounded_rect_masked(
                &mut painter.pixmap,
                device_track_rect,
                device_radii,
                Rgba::rgb(190, 190, 190),
                clip_mask,
              );
            }

            if !appearance_none {
              let filled_rect = if style.direction == crate::style::types::Direction::Rtl {
                Rect::from_xywh(
                  knob_center_x,
                  track_rect.y(),
                  (track_rect.max_x() - knob_center_x).max(0.0),
                  track_rect.height(),
                )
              } else {
                Rect::from_xywh(
                  track_rect.x(),
                  track_rect.y(),
                  (knob_center_x - track_rect.x()).max(0.0),
                  track_rect.height(),
                )
              };
              if filled_rect.width() > 0.0 {
                let radii = BorderRadii::uniform(track_height / 2.0)
                  .clamped(filled_rect.width(), filled_rect.height());
                let device_fill_rect = painter.device_rect(filled_rect);
                let device_radii = painter.device_radii(radii);
                fill_rounded_rect_masked(
                  &mut painter.pixmap,
                  device_fill_rect,
                  device_radii,
                  muted_accent,
                  clip_mask,
                );
              }
            }

            if let Some(track_style) = track_style {
              let device_track_rect = painter.device_rect(track_rect);
              let device_radii =
                painter.device_radii(resolve_border_radii(Some(track_style), track_rect));
              paint_rounded_border(
                painter,
                track_rect,
                device_track_rect,
                device_radii,
                track_style,
                clip_mask,
              );
            }
          };

        if should_paint_track {
          let track_height = track_style
            .and_then(|style| {
              style
                .height
                .map(|len| resolve_px(style, len, padding_rect.height()))
            })
            .filter(|px| px.is_finite() && *px > 0.0)
            .unwrap_or_else(|| 4.0_f32.min(padding_rect.height()));

          if track_height > 0.0 {
            let track_y = padding_rect.y() + (padding_rect.height() - track_height) / 2.0;
            let track_rect = Rect::from_xywh(
              padding_rect.x(),
              track_y,
              padding_rect.width(),
              track_height,
            );
            if let Some(track_style) = track_style {
              let track_opacity = track_style.opacity.clamp(0.0, 1.0);
              if track_opacity <= 0.0 {
                // Fully transparent track pseudo-element.
              } else if track_opacity < 1.0 - 1e-6 {
                let bounds = opacity_layer_bounds(track_rect, track_style);
                self.paint_with_opacity_layer(
                  track_opacity,
                  bounds,
                  clip_mask,
                  |painter, layer_clip_mask| {
                    paint_track(painter, layer_clip_mask, track_rect, track_height);
                  },
                );
              } else {
                paint_track(self, clip_mask, track_rect, track_height);
              }
            } else {
              paint_track(self, clip_mask, track_rect, track_height);
            }
          }
        }

        if let Some(thumb_style) = thumb_style {
          let thumb_opacity = thumb_style.opacity.clamp(0.0, 1.0);
          let mut style_for_thumb;
          let style_for_thumb = if border_radius_is_zero(thumb_style) {
            style_for_thumb = (*thumb_style).clone();
            let radius = knob_width.min(knob_height) / 2.0;
            let corner =
              crate::style::types::BorderCornerRadius::uniform(Length::px(radius.max(0.0)));
            style_for_thumb.border_top_left_radius = corner;
            style_for_thumb.border_top_right_radius = corner;
            style_for_thumb.border_bottom_right_radius = corner;
            style_for_thumb.border_bottom_left_radius = corner;
            &style_for_thumb
          } else {
            thumb_style
          };

          let mut knob_radii = resolve_border_radii(Some(style_for_thumb), knob_rect);
          if knob_radii.is_zero() {
            knob_radii = BorderRadii::uniform((knob_width.min(knob_height)) / 2.0);
          }

          let paint_thumb = |painter: &mut Painter, clip_mask: Option<&Mask>| {
            paint_outset_box_shadows(painter, knob_rect, knob_radii, style_for_thumb, clip_mask);

            let device_knob_rect = painter.device_rect(knob_rect);
            let device_radii = painter.device_radii(knob_radii);
            fill_rounded_rect_masked(
              &mut painter.pixmap,
              device_knob_rect,
              device_radii,
              style_for_thumb.background_color,
              clip_mask,
            );
            paint_rounded_border(
              painter,
              knob_rect,
              device_knob_rect,
              device_radii,
              style_for_thumb,
              clip_mask,
            );
          };

          if thumb_opacity <= 0.0 {
            // Fully transparent thumb pseudo-element.
          } else if thumb_opacity < 1.0 - 1e-6 {
            let bounds = opacity_layer_bounds(knob_rect, style_for_thumb);
            self.paint_with_opacity_layer(
              thumb_opacity,
              bounds,
              clip_mask,
              |painter, layer_clip_mask| {
                paint_thumb(painter, layer_clip_mask);
              },
            );
          } else {
            paint_thumb(self, clip_mask);
          }
        } else {
          let knob_radius = (knob_width.min(knob_height)) / 2.0;
          let knob_center_x = self.device_x(knob_center_x);
          let knob_center_y = self.device_y(knob_center_y);
          let knob_radius = self.device_length(knob_radius);
          if let Some(path) = PathBuilder::from_circle(knob_center_x, knob_center_y, knob_radius) {
            let mut paint = Paint::default();
            paint.set_color_rgba8(255, 255, 255, 255);
            paint.anti_alias = true;
            self.pixmap.fill_path(
              &path,
              &paint,
              tiny_skia::FillRule::Winding,
              Transform::identity(),
              clip_mask,
            );

            let mut stroke_paint = Paint::default();
            stroke_paint.set_color_rgba8(130, 130, 130, 255);
            stroke_paint.anti_alias = true;
            let stroke = tiny_skia::Stroke {
              width: 1.0 * self.scale,
              ..Default::default()
            };
            self.pixmap.stroke_path(
              &path,
              &stroke_paint,
              &stroke,
              Transform::identity(),
              clip_mask,
            );
          }
        }
        true
      }
      FormControlKind::Progress { value, max } => {
        if matches!(control.appearance, Appearance::None) {
          return true;
        }

        let track_style = control.progress_bar_style.as_deref();
        let value_style = control.progress_value_style.as_deref();

        let track_rect = content_rect;
        if track_rect.width() <= 0.0 || track_rect.height() <= 0.0 {
          return true;
        }

        let mut track_color = track_style
          .map(|s| s.background_color)
          .filter(|c| !c.is_transparent())
          .unwrap_or(style.background_color);
        if track_color.is_transparent() {
          track_color = Rgba::rgb(230, 230, 230);
        }
        if control.disabled {
          track_color = track_color.with_alpha((track_color.a * 0.85).clamp(0.0, 1.0));
        }

        let track_radii = resolve_border_radii(track_style.or(Some(style)), track_rect)
          .clamped(track_rect.width(), track_rect.height());
        let device_track_rect = self.device_rect(track_rect);
        let device_track_radii = self.device_radii(track_radii);
        fill_rounded_rect_masked(
          &mut self.pixmap,
          device_track_rect,
          device_track_radii,
          track_color,
          clip_mask,
        );

        let is_indeterminate = *value < 0.0 || !value.is_finite();
        let fill_rect = if is_indeterminate {
          Rect::from_xywh(
            track_rect.x() + track_rect.width() * 0.25,
            track_rect.y(),
            (track_rect.width() * 0.5).max(0.0),
            track_rect.height(),
          )
        } else {
          let denom = if *max > 0.0 && max.is_finite() {
            *max
          } else {
            1.0
          };
          let ratio = (*value / denom).clamp(0.0, 1.0);
          let fill_w = (track_rect.width() * ratio).max(0.0);
          let fill_x = if style.direction == crate::style::types::Direction::Rtl {
            (track_rect.max_x() - fill_w).max(track_rect.x())
          } else {
            track_rect.x()
          };
          Rect::from_xywh(fill_x, track_rect.y(), fill_w, track_rect.height())
        };
        if fill_rect.width() <= 0.0 {
          return true;
        }

        let fill_color = value_style
          .map(|s| s.background_color)
          .filter(|c| !c.is_transparent())
          .unwrap_or(muted_accent);

        let mut fill_radii = track_radii.clamped(fill_rect.width(), fill_rect.height());
        if !is_indeterminate && fill_rect.width() + 0.01 < track_rect.width() {
          if style.direction == crate::style::types::Direction::Rtl {
            fill_radii.top_left = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
            fill_radii.bottom_left = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
          } else {
            fill_radii.top_right = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
            fill_radii.bottom_right = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
          }
        }

        let device_fill_rect = self.device_rect(fill_rect);
        let device_fill_radii = self.device_radii(fill_radii);
        fill_rounded_rect_masked(
          &mut self.pixmap,
          device_fill_rect,
          device_fill_radii,
          fill_color,
          clip_mask,
        );
        true
      }
      FormControlKind::Meter {
        value,
        min,
        max,
        low,
        high,
        optimum,
      } => {
        if matches!(control.appearance, Appearance::None) {
          return true;
        }

        let track_style = control.meter_bar_style.as_deref();

        let track_rect = content_rect;
        if track_rect.width() <= 0.0 || track_rect.height() <= 0.0 {
          return true;
        }

        let mut track_color = track_style
          .map(|s| s.background_color)
          .filter(|c| !c.is_transparent())
          .unwrap_or(style.background_color);
        if track_color.is_transparent() {
          track_color = Rgba::rgb(230, 230, 230);
        }
        if control.disabled {
          track_color = track_color.with_alpha((track_color.a * 0.85).clamp(0.0, 1.0));
        }

        let track_radii = resolve_border_radii(track_style.or(Some(style)), track_rect)
          .clamped(track_rect.width(), track_rect.height());
        let device_track_rect = self.device_rect(track_rect);
        let device_track_radii = self.device_radii(track_radii);
        fill_rounded_rect_masked(
          &mut self.pixmap,
          device_track_rect,
          device_track_radii,
          track_color,
          clip_mask,
        );

        let span = (*max - *min).abs().max(0.0001);
        let ratio = ((*value - *min) / span).clamp(0.0, 1.0);
        let fill_w = (track_rect.width() * ratio).max(0.0);
        let fill_x = if style.direction == crate::style::types::Direction::Rtl {
          (track_rect.max_x() - fill_w).max(track_rect.x())
        } else {
          track_rect.x()
        };
        let fill_rect = Rect::from_xywh(fill_x, track_rect.y(), fill_w, track_rect.height());
        if fill_rect.width() <= 0.0 {
          return true;
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum MeterZone {
          Low,
          Mid,
          High,
        }

        let warn_color = Rgba::rgb(245, 169, 26);
        let bad_color = Rgba::rgb(212, 43, 43);

        let mut fill_color = muted_accent;
        let mut fill_kind_is_optimum = true;
        let mut fill_kind_is_suboptimum = false;
        if let Some(optimum) = (*optimum).filter(|v| v.is_finite()) {
          let mut low_val = (*low).unwrap_or(*min).clamp(*min, *max);
          let mut high_val = (*high).unwrap_or(*max).clamp(*min, *max);
          if low_val > high_val {
            low_val = high_val;
          }

          let value_zone = if *value < low_val {
            MeterZone::Low
          } else if *value > high_val {
            MeterZone::High
          } else {
            MeterZone::Mid
          };
          let optimum_zone = if optimum < low_val {
            MeterZone::Low
          } else if optimum > high_val {
            MeterZone::High
          } else {
            MeterZone::Mid
          };

          fill_color = if value_zone == optimum_zone {
            muted_accent
          } else if optimum_zone == MeterZone::Mid {
            warn_color
          } else if optimum_zone == MeterZone::Low {
            if value_zone == MeterZone::Mid {
              warn_color
            } else {
              bad_color
            }
          } else if value_zone == MeterZone::Mid {
            warn_color
          } else {
            bad_color
          };
          fill_kind_is_optimum = value_zone == optimum_zone;
          fill_kind_is_suboptimum = !fill_kind_is_optimum
            && (optimum_zone == MeterZone::Mid || value_zone == MeterZone::Mid);

          if control.disabled {
            fill_color = fill_color.with_alpha((fill_color.a * 0.7).clamp(0.0, 1.0));
          }
        }

        let value_style = if fill_kind_is_optimum {
          control.meter_optimum_value_style.as_deref()
        } else if fill_kind_is_suboptimum {
          control.meter_suboptimum_value_style.as_deref()
        } else {
          control.meter_even_less_good_value_style.as_deref()
        };
        if let Some(style) = value_style {
          let bg = style.background_color;
          if !bg.is_transparent() {
            fill_color = bg;
            if control.disabled {
              fill_color = fill_color.with_alpha((fill_color.a * 0.7).clamp(0.0, 1.0));
            }
          }
        }

        let mut fill_radii = track_radii.clamped(fill_rect.width(), fill_rect.height());
        if fill_rect.width() + 0.01 < track_rect.width() {
          if style.direction == crate::style::types::Direction::Rtl {
            fill_radii.top_left = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
            fill_radii.bottom_left = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
          } else {
            fill_radii.top_right = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
            fill_radii.bottom_right = crate::paint::display_list::BorderRadius { x: 0.0, y: 0.0 };
          }
        }

        let device_fill_rect = self.device_rect(fill_rect);
        let device_fill_radii = self.device_radii(fill_radii);
        fill_rounded_rect_masked(
          &mut self.pixmap,
          device_fill_rect,
          device_fill_radii,
          fill_color,
          clip_mask,
        );
        true
      }
      FormControlKind::Color { value, raw } => {
        if matches!(control.appearance, Appearance::None) {
          // `appearance: none` leaves the author-provided background/border visible; suppress the
          // native swatch/label painting.
          return true;
        }
        let rect = inset_rect(content_rect, 2.0);
        let radii = BorderRadii::uniform((rect.height().min(rect.width()) / 5.0).max(2.0));
        let device_rect = self.device_rect(rect);
        let radii = self.device_radii(radii);
        fill_rounded_rect_masked(&mut self.pixmap, device_rect, radii, *value, clip_mask);
        let luminance =
          (0.299 * value.r as f32 + 0.587 * value.g as f32 + 0.114 * value.b as f32) / 255.0;
        let text_color = if luminance > 0.5 {
          Rgba {
            r: 24,
            g: 24,
            b: 24,
            a: 1.0,
          }
        } else {
          Rgba {
            r: 245,
            g: 245,
            b: 245,
            a: 1.0,
          }
        };
        let mut text_style = style.clone();
        text_style.color = text_color;
        let label = raw
          .clone()
          .unwrap_or_else(|| format!("#{:02X}{:02X}{:02X}", value.r, value.g, value.b));
        let label_rect = if let Some(size) = self.measure_alt_text_raw(&label, &text_style) {
          let start_x = rect.x() + ((rect.width() - size.width).max(0.0) / 2.0);
          let start_y = rect.y() + ((rect.height() - size.height).max(0.0) / 2.0);
          Rect::from_xywh(
            start_x,
            start_y,
            size.width.min(rect.width()),
            size.height.min(rect.height()),
          )
        } else {
          rect
        };
        let _ = self.paint_alt_text_raw(&label, &text_style, label_rect, clip_mask);
        true
      }
      FormControlKind::File { value } => {
        let appearance_none = matches!(control.appearance, Appearance::None);
        let button_pseudo_style = control.file_selector_button_style.as_deref();

        let button_label = "Choose File";
        let file_label = value
          .as_deref()
          .filter(|v| !v.is_empty())
          .map(|v| {
            let name = v.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(v);
            if name.is_empty() {
              v
            } else {
              name
            }
          })
          .unwrap_or("No file chosen");

        let base_color = if control.invalid { accent } else { style.color };
        let mut file_style = style.clone();
        file_style.color = if control.disabled {
          base_color.with_alpha(0.5)
        } else {
          base_color
        };

        let mut button_text_style = button_pseudo_style
          .map(|s| (*s).clone())
          .unwrap_or_else(|| style.clone());
        if button_pseudo_style.is_none() {
          button_text_style.color = file_style.color;
        }

        let rect = inset_rect(content_rect, 2.0);
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
          return true;
        }

        let viewport = (self.css_width, self.css_height);
        let resolve_px = |style: &ComputedStyle, len: Length, percentage_base: f32| -> f32 {
          resolve_length_for_paint(
            &len,
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
        };

        let measured_button_text_w = measure_shaped_advance(self, button_label, &button_text_style);
        let default_button_w = (measured_button_text_w + button_text_style.font_size * 1.4)
          .max(button_text_style.font_size * 3.0)
          .min(rect.width());
        let button_w = button_pseudo_style
          .and_then(|style| {
            style
              .width
              .map(|len| resolve_px(style, len, rect.width()))
              .filter(|px| px.is_finite() && *px > 0.0)
          })
          .unwrap_or(default_button_w)
          .min(rect.width());

        let button_h = button_pseudo_style
          .and_then(|style| {
            style
              .height
              .map(|len| resolve_px(style, len, rect.height()))
              .filter(|px| px.is_finite() && *px > 0.0)
          })
          .unwrap_or(rect.height())
          .min(rect.height());

        let button_rect = Rect::from_xywh(
          rect.x(),
          rect.y() + (rect.height() - button_h) / 2.0,
          button_w,
          button_h,
        );
        let gap = 6.0_f32.min(rect.width().max(0.0));
        let file_rect = Rect::from_xywh(
          (button_rect.max_x() + gap).min(rect.max_x()),
          rect.y(),
          (rect.max_x() - (button_rect.max_x() + gap)).max(0.0),
          rect.height(),
        );

        if let Some(button_style) = button_pseudo_style {
          let button_radii = resolve_border_radii(Some(button_style), button_rect);
          let device_button_rect = self.device_rect(button_rect);
          let device_radii = self.device_radii(button_radii);
          fill_rounded_rect_masked(
            &mut self.pixmap,
            device_button_rect,
            device_radii,
            button_style.background_color,
            clip_mask,
          );

          let border_width = resolve_length_for_paint(
            &button_style.used_border_top_width(),
            button_style.font_size,
            button_style.root_font_size,
            button_rect.width().max(0.0),
            viewport,
          )
          .max(0.0);
          let border_style = button_style.border_top_style;
          let border_color = button_style.border_top_color;
          if border_width > 0.0
            && !matches!(
              border_style,
              crate::style::types::BorderStyle::None | crate::style::types::BorderStyle::Hidden
            )
            && !border_color.is_transparent()
          {
            if let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
              device_button_rect.x(),
              device_button_rect.y(),
              device_button_rect.width(),
              device_button_rect.height(),
              &device_radii,
            ) {
              let mut paint = Paint::default();
              paint.set_color(tiny_skia::Color::from_rgba8(
                border_color.r,
                border_color.g,
                border_color.b,
                border_color.alpha_u8(),
              ));
              paint.anti_alias = true;
              let stroke = tiny_skia::Stroke {
                width: border_width * self.scale,
                ..Default::default()
              };
              self
                .pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
            }
          }
        } else if !appearance_none {
          let button_bg = if control.disabled {
            Rgba::rgb(235, 235, 235)
          } else {
            Rgba::rgb(245, 245, 245)
          };
          let border_color = Rgba::rgb(180, 180, 180);
          let radii = BorderRadii::uniform((button_rect.height() / 6.0).max(2.0));
          let device_rect = self.device_rect(button_rect);
          let device_radii = self.device_radii(radii);
          fill_rounded_rect_masked(
            &mut self.pixmap,
            device_rect,
            device_radii,
            button_bg,
            clip_mask,
          );

          if let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
            device_rect.x(),
            device_rect.y(),
            device_rect.width(),
            device_rect.height(),
            &device_radii,
          ) {
            let mut paint = Paint::default();
            paint.set_color(tiny_skia::Color::from_rgba8(
              border_color.r,
              border_color.g,
              border_color.b,
              border_color.alpha_u8(),
            ));
            paint.anti_alias = true;
            let stroke = tiny_skia::Stroke {
              width: 1.0 * self.scale,
              ..Default::default()
            };
            self
              .pixmap
              .stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
          }
        }

        if button_rect.width() > 0.0 && button_rect.height() > 0.0 {
          let label_rect =
            if let Some(size) = self.measure_alt_text(button_label, &button_text_style) {
              let start_x = button_rect.x() + ((button_rect.width() - size.width).max(0.0) / 2.0);
              Rect::from_xywh(
                start_x,
                button_rect.y(),
                size.width.min(button_rect.width()),
                button_rect.height(),
              )
            } else {
              button_rect
            };
          let _ = self.paint_alt_text(button_label, &button_text_style, label_rect, clip_mask);
        }

        if file_rect.width() > 0.0 && file_rect.height() > 0.0 {
          let text_rect = inset_rect(file_rect, 2.0);
          let _ = self.paint_alt_text(file_label, &file_style, text_rect, clip_mask);
        }

        true
      }
      FormControlKind::Unknown { label } => {
        if let Some(text) = label {
          let rect = inset_rect(content_rect, 2.0);
          let mut unknown_style = style.clone();
          if control.invalid {
            unknown_style.color = accent;
          }
          let _ = self.paint_alt_text_raw(text, &unknown_style, rect, clip_mask);
        }
        true
      }
    }
  }

  fn iframe_depth_remaining(&self, context: Option<&ResourceContext>) -> usize {
    context
      .and_then(|ctx| ctx.iframe_depth_remaining)
      .unwrap_or(self.max_iframe_depth)
  }

  fn render_iframe_srcdoc(
    &self,
    html: &str,
    src: &str,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    content_rect: Rect,
    style: Option<&ComputedStyle>,
  ) -> Option<Arc<ImageData>> {
    let remaining_depth = self.iframe_depth_remaining(self.image_cache.resource_context().as_ref());
    render_iframe_srcdoc(
      html,
      Some(src),
      referrer_policy,
      content_rect,
      style,
      &self.image_cache,
      &self.font_ctx,
      self.scale,
      remaining_depth,
    )
  }

  fn render_iframe_src(
    &self,
    src: &str,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    content_rect: Rect,
    style: Option<&ComputedStyle>,
  ) -> Option<Arc<ImageData>> {
    let remaining_depth = self.iframe_depth_remaining(self.image_cache.resource_context().as_ref());
    render_iframe_src(
      src,
      referrer_policy,
      content_rect,
      style,
      &self.image_cache,
      &self.font_ctx,
      self.scale,
      remaining_depth,
    )
  }

  fn paint_image_from_src(
    &mut self,
    src: &crate::tree::box_tree::SelectedImageSource<'_>,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    self.paint_image_from_src_impl(
      src,
      crossorigin,
      referrer_policy,
      style,
      x,
      y,
      width,
      height,
      clip_mask,
      false,
    )
  }

  fn paint_image_from_src_reject_placeholder(
    &mut self,
    src: &crate::tree::box_tree::SelectedImageSource<'_>,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    self.paint_image_from_src_impl(
      src,
      crossorigin,
      referrer_policy,
      style,
      x,
      y,
      width,
      height,
      clip_mask,
      true,
    )
  }

  fn paint_image_from_src_impl(
    &mut self,
    src: &crate::tree::box_tree::SelectedImageSource<'_>,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
    reject_placeholder: bool,
  ) -> bool {
    let trimmed = trim_ascii_whitespace_start_html_css(src.url);
    if trimmed.is_empty() {
      return false;
    }
    let inline_svg = trimmed.starts_with('<');
    let resolved_src = if inline_svg {
      "inline-svg".to_string()
    } else {
      self.image_cache.resolve_url(src.url)
    };
    if let Some(limit) = trace_image_paint_limit() {
      let idx = TRACE_IMAGE_PAINT_COUNT.fetch_add(1, Ordering::Relaxed);
      if idx < limit {
        eprintln!(
          "[image-paint] #{idx} src={} resolved={} rect=({:.1},{:.1},{:.1},{:.1})",
          src.url, resolved_src, x, y, width, height
        );
      }
    }

    let log_image_fail = runtime::runtime_toggles().truthy("FASTR_LOG_IMAGE_FAIL");

    let image = if inline_svg {
      match self.image_cache.render_svg(trimmed) {
        Ok(img) => img,
        Err(e) => {
          if log_image_fail {
            eprintln!(
              "[image-load-fail] src={} stage=render-svg err={}",
              src.url, e
            );
          }
          return false;
        }
      }
    } else {
      match self.image_cache.load_with_crossorigin_and_referrer_policy(
        &resolved_src,
        crossorigin,
        referrer_policy,
      ) {
        Ok(img) => img,
        Err(e) => {
          if log_image_fail {
            eprintln!("[image-load-fail] src={} stage=load err={}", src.url, e);
          }
          return false;
        }
      }
    };

    if reject_placeholder && !inline_svg && self.image_cache.is_placeholder_image(&image) {
      return false;
    }

    if let Some(limit) = trace_image_paint_limit() {
      let seen = TRACE_IMAGE_PAINT_COUNT.load(Ordering::Relaxed);
      if seen <= limit {
        eprintln!(
          " [image-paint-loaded] src={} cached_image_ptr={:p} dyn_ptr={:p} dims={}x{}",
          src.url,
          Arc::as_ptr(&image),
          Arc::as_ptr(&image.image),
          image.width(),
          image.height()
        );
      }
    }

    let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
    let orientation = style
      .map(|s| s.image_orientation.resolve(image.orientation, false))
      .unwrap_or_else(|| ImageOrientation::default().resolve(image.orientation, false));
    let (img_w_raw, img_h_raw) = image.oriented_dimensions(orientation);
    if img_w_raw == 0 || img_h_raw == 0 {
      if log_image_fail {
        eprintln!(
          "[image-load-fail] src={} stage=oriented-dimensions w={} h={}",
          src.url, img_w_raw, img_h_raw
        );
      }
      return false;
    }
    let Some((img_w_css, img_h_css)) =
      image.css_dimensions(orientation, &image_resolution, self.scale, src.density)
    else {
      if log_image_fail {
        eprintln!(
          "[image-load-fail] src={} stage=css-dimensions orientation={:?} resolution={:?}",
          src.url, orientation, image_resolution
        );
      }
      return false;
    };
    let has_intrinsic_ratio = image.intrinsic_ratio(orientation).is_some();

    let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
    let pos = style
      .map(|s| s.object_position)
      .unwrap_or_else(default_object_position);
    let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
    let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);

    let (dest_x, dest_y, mut dest_w, mut dest_h) = match compute_object_fit(
      fit,
      pos,
      width,
      height,
      img_w_css,
      img_h_css,
      has_intrinsic_ratio,
      font_size,
      root_font_size,
      Some((self.css_width, self.css_height)),
    ) {
      Some(v) => v,
      None => return false,
    };

    let dest_x_device = self.device_length(dest_x);
    let dest_y_device = self.device_length(dest_y);
    let dest_w_device = self.device_length(dest_w);
    let dest_h_device = self.device_length(dest_h);

    if image.is_vector {
      if let Some(svg) = &image.svg_content {
        fn contains_foreign_object_tag(svg: &str) -> bool {
          const NEEDLE: &[u8] = b"foreignobject";
          let bytes = svg.as_bytes();
          bytes
            .windows(NEEDLE.len())
            .any(|window| window.eq_ignore_ascii_case(NEEDLE))
        }

        let svg = svg.as_ref();
        let resolved_svg = if contains_foreign_object_tag(svg) {
          let foreign_object_dpr =
            crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
              svg, self.scale, dest_w, dest_h, img_w_css, img_h_css,
            );
          crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
            svg,
            "",
            &self.font_ctx,
            &self.image_cache,
            foreign_object_dpr,
            self.max_iframe_depth,
          )
        } else {
          None
        };
        let svg = resolved_svg.as_deref().unwrap_or(svg);

        let render_w = dest_w_device.ceil().max(1.0) as u32;
        let render_h = dest_h_device.ceil().max(1.0) as u32;
        if render_w > 0 && render_h > 0 {
          let pixmap_timer = self.diagnostics_enabled.then(Instant::now);
          if let Ok(pixmap) = self.image_cache.render_svg_pixmap_at_size(
            svg,
            render_w,
            render_h,
            &resolved_src,
            self.scale,
          ) {
            if let Some(start) = pixmap_timer {
              with_paint_diagnostics(|diag| {
                diag.image_pixmap_ms += start.elapsed().as_secs_f64() * 1000.0;
              });
            }
            let scale_x = dest_w_device / render_w as f32;
            let scale_y = dest_h_device / render_h as f32;
            if scale_x.is_finite() && scale_y.is_finite() {
              let mut paint = PixmapPaint::default();
              paint.quality = Self::filter_quality_for_image(style);
              let transform = Transform::from_row(
                scale_x,
                0.0,
                0.0,
                scale_y,
                self.device_x(x) + dest_x_device,
                self.device_y(y) + dest_y_device,
              );
              self
                .pixmap
                .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
              return true;
            }
          }
        }
      }
    }

    let quality = Self::filter_quality_for_image(style);
    let target_w = dest_w_device.ceil().max(1.0) as u32;
    let target_h = dest_h_device.ceil().max(1.0) as u32;

    let pixmap_timer = self.diagnostics_enabled.then(Instant::now);
    let should_scale =
      Self::should_use_scaled_raster_pixmap(quality, img_w_raw, img_h_raw, target_w, target_h);
    let pixmap = match if should_scale {
      self
        .image_cache
        .load_raster_pixmap_at_size_with_crossorigin(
          &resolved_src,
          crossorigin,
          orientation,
          false,
          target_w,
          target_h,
          quality,
        )
    } else {
      self.image_cache.load_raster_pixmap_with_crossorigin(
        &resolved_src,
        crossorigin,
        orientation,
        false,
      )
    } {
      Ok(Some(pixmap)) => pixmap,
      Ok(None) | Err(_) => {
        if log_image_fail {
          eprintln!("[image-load-fail] src={} stage=pixmap", src.url);
        }
        return false;
      }
    };
    if let Some(start) = pixmap_timer {
      with_paint_diagnostics(|diag| {
        diag.image_pixmap_ms += start.elapsed().as_secs_f64() * 1000.0;
      });
    }

    if matches!(
      style.map(|s| s.image_rendering),
      Some(ImageRendering::Pixelated | ImageRendering::CrispEdges)
    ) && (dest_w > img_w_raw as f32 || dest_h > img_h_raw as f32)
    {
      let (snapped_w, offset_x) = snap_upscale(dest_w, img_w_raw as f32).unwrap_or((dest_w, 0.0));
      let (snapped_h, offset_y) = snap_upscale(dest_h, img_h_raw as f32).unwrap_or((dest_h, 0.0));
      dest_w = snapped_w;
      dest_h = snapped_h;
      let dest_x = dest_x + offset_x;
      let dest_y = dest_y + offset_y;
      let dest_x = self.device_length(dest_x);
      let dest_y = self.device_length(dest_y);
      let dest_w = self.device_length(dest_w);
      let dest_h = self.device_length(dest_h);

      let scale_x = dest_w / img_w_raw as f32;
      let scale_y = dest_h / img_h_raw as f32;
      if !scale_x.is_finite() || !scale_y.is_finite() || dest_w <= 0.0 || dest_h <= 0.0 {
        return false;
      }

      let mut paint = PixmapPaint::default();
      paint.quality = Self::filter_quality_for_image(style);

      let transform = Transform::from_row(
        scale_x,
        0.0,
        0.0,
        scale_y,
        self.device_x(x) + dest_x,
        self.device_y(y) + dest_y,
      );
      self
        .pixmap
        .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
      return true;
    }

    let dest_x = self.device_length(dest_x);
    let dest_y = self.device_length(dest_y);
    let dest_w = self.device_length(dest_w);
    let dest_h = self.device_length(dest_h);

    let scale_x = dest_w / pixmap.width() as f32;
    let scale_y = dest_h / pixmap.height() as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() {
      return false;
    }

    let mut paint = PixmapPaint::default();
    paint.quality = quality;

    let transform = Transform::from_row(
      scale_x,
      0.0,
      0.0,
      scale_y,
      self.device_x(x) + dest_x,
      self.device_y(y) + dest_y,
    );
    self
      .pixmap
      .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
    true
  }

  fn paint_image_data(
    &mut self,
    image: &ImageData,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    if width <= 0.0 || height <= 0.0 {
      return false;
    }
    if image.width == 0 || image.height == 0 {
      return false;
    }
    if image.pixels.len() != (image.width as usize)
      .saturating_mul(image.height as usize)
      .saturating_mul(4)
    {
      return false;
    }

    // Resolve natural size for object-fit calculations. When missing, treat the pixel size as the
    // natural CSS size (1dppx).
    let img_w_css = if image.css_width.is_finite() && image.css_width > 0.0 {
      image.css_width
    } else {
      image.width as f32
    };
    let img_h_css = if image.css_height.is_finite() && image.css_height > 0.0 {
      image.css_height
    } else {
      image.height as f32
    };

    let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
    let pos = style
      .map(|s| s.object_position)
      .unwrap_or_else(default_object_position);
    let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
    let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);

    let (dest_x, dest_y, dest_w, dest_h) = match compute_object_fit(
      fit,
      pos,
      width,
      height,
      img_w_css,
      img_h_css,
      image.has_intrinsic_ratio,
      font_size,
      root_font_size,
      Some((self.css_width, self.css_height)),
    ) {
      Some(v) => v,
      None => return false,
    };

    let dest_x_device = self.device_length(dest_x);
    let dest_y_device = self.device_length(dest_y);
    let dest_w_device = self.device_length(dest_w);
    let dest_h_device = self.device_length(dest_h);

    let scale_x = dest_w_device / image.width as f32;
    let scale_y = dest_h_device / image.height as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() {
      return false;
    }

    let bytes: Cow<'_, [u8]> = if image.premultiplied {
      Cow::Borrowed(image.pixels.as_ref().as_slice())
    } else {
      // tiny-skia expects premultiplied RGBA; premultiply into a scratch buffer.
      let mut buf = image.pixels.as_ref().clone();
      for px in buf.chunks_exact_mut(4) {
        let a = px[3] as u16;
        px[0] = ((px[0] as u16 * a + 127) / 255) as u8;
        px[1] = ((px[1] as u16 * a + 127) / 255) as u8;
        px[2] = ((px[2] as u16 * a + 127) / 255) as u8;
      }
      Cow::Owned(buf)
    };

    let Some(pixmap) = PixmapRef::from_bytes(bytes.as_ref(), image.width, image.height) else {
      return false;
    };

    let mut paint = PixmapPaint::default();
    paint.quality = Self::filter_quality_for_image(style);
    let transform = Transform::from_row(
      scale_x,
      0.0,
      0.0,
      scale_y,
      self.device_x(x) + dest_x_device,
      self.device_y(y) + dest_y_device,
    );
    self
      .pixmap
      .draw_pixmap(0, 0, pixmap, &paint, transform, clip_mask);
    true
  }

  fn paint_svg(
    &mut self,
    content: &str,
    style: Option<&ComputedStyle>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    if content.is_empty() {
      return false;
    }

    fn contains_foreign_object_tag(svg: &str) -> bool {
      const NEEDLE: &[u8] = b"foreignobject";
      let bytes = svg.as_bytes();
      bytes
        .windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
    }

    let trimmed = trim_ascii_whitespace_start_html_css(content);
    let inline_svg = trimmed.starts_with("<svg") || trimmed.starts_with("<?xml");

    if inline_svg {
      let meta = match self.image_cache.probe_svg_content(content, "inline-svg") {
        Ok(meta) => meta,
        Err(_) => return false,
      };

      let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
      let orientation = style
        .map(|s| s.image_orientation.resolve(meta.orientation, false))
        .unwrap_or_else(|| ImageOrientation::default().resolve(meta.orientation, false));
      let (img_w_raw, img_h_raw) = meta.oriented_dimensions(orientation);
      if img_w_raw == 0 || img_h_raw == 0 {
        return false;
      }
      let Some((img_w_css, img_h_css)) =
        meta.css_dimensions(orientation, &image_resolution, self.scale, None)
      else {
        return false;
      };
      let has_intrinsic_ratio = meta.intrinsic_ratio(orientation).is_some();

      let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
      let pos = style
        .map(|s| s.object_position)
        .unwrap_or_else(default_object_position);
      let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
      let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);

      let (dest_x, dest_y, dest_w, dest_h) = match compute_object_fit(
        fit,
        pos,
        width,
        height,
        img_w_css,
        img_h_css,
        has_intrinsic_ratio,
        font_size,
        root_font_size,
        Some((self.css_width, self.css_height)),
      ) {
        Some(v) => v,
        None => return false,
      };

      let resolved_svg = if contains_foreign_object_tag(content) {
        let foreign_object_dpr =
          crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
            content, self.scale, dest_w, dest_h, img_w_css, img_h_css,
          );
        crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
          content,
          "",
          &self.font_ctx,
          &self.image_cache,
          foreign_object_dpr,
          self.max_iframe_depth,
        )
      } else {
        None
      };
      let svg = resolved_svg.as_deref().unwrap_or(content);

      let dest_x_device = self.device_length(dest_x);
      let dest_y_device = self.device_length(dest_y);
      let dest_w_device = self.device_length(dest_w);
      let dest_h_device = self.device_length(dest_h);
      if dest_w_device <= 0.0 || dest_h_device <= 0.0 {
        return false;
      }

      let render_w = dest_w_device.ceil().max(1.0) as u32;
      let render_h = dest_h_device.ceil().max(1.0) as u32;
      let pixmap = match self.image_cache.render_svg_pixmap_at_size(
        svg,
        render_w,
        render_h,
        "inline-svg",
        self.scale,
      ) {
        Ok(pixmap) => pixmap,
        Err(_) => return false,
      };

      let scale_x = dest_w_device / render_w as f32;
      let scale_y = dest_h_device / render_h as f32;
      if !scale_x.is_finite() || !scale_y.is_finite() {
        return false;
      }

      let mut paint = PixmapPaint::default();
      paint.quality = Self::filter_quality_for_image(style);

      let transform = Transform::from_row(
        scale_x,
        0.0,
        0.0,
        scale_y,
        self.device_x(x) + dest_x_device,
        self.device_y(y) + dest_y_device,
      );
      self
        .pixmap
        .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
      return true;
    }

    let resolved_src = self.image_cache.resolve_url(content);
    let image = match self.image_cache.load(&resolved_src) {
      Ok(img) => img,
      Err(_) => return false,
    };
    if self.image_cache.is_placeholder_image(&image) {
      return false;
    }

    let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
    let orientation = style
      .map(|s| s.image_orientation.resolve(image.orientation, false))
      .unwrap_or_else(|| ImageOrientation::default().resolve(image.orientation, false));
    let (img_w_raw, img_h_raw) = image.oriented_dimensions(orientation);
    if img_w_raw == 0 || img_h_raw == 0 {
      return false;
    }
    let Some((img_w_css, img_h_css)) =
      image.css_dimensions(orientation, &image_resolution, self.scale, None)
    else {
      return false;
    };
    let has_intrinsic_ratio = image.intrinsic_ratio(orientation).is_some();
    let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
    let pos = style
      .map(|s| s.object_position)
      .unwrap_or_else(default_object_position);
    let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
    let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);

    let (dest_x, dest_y, mut dest_w, mut dest_h) = match compute_object_fit(
      fit,
      pos,
      width,
      height,
      img_w_css,
      img_h_css,
      has_intrinsic_ratio,
      font_size,
      root_font_size,
      Some((self.css_width, self.css_height)),
    ) {
      Some(v) => v,
      None => return false,
    };

    let dest_x_device = self.device_length(dest_x);
    let dest_y_device = self.device_length(dest_y);
    let dest_w_device = self.device_length(dest_w);
    let dest_h_device = self.device_length(dest_h);

    if image.is_vector {
      if let Some(svg) = &image.svg_content {
        let svg = svg.as_ref();
        let resolved_svg = if contains_foreign_object_tag(svg) {
          let foreign_object_dpr =
            crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
              svg, self.scale, dest_w, dest_h, img_w_css, img_h_css,
            );
          crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
            svg,
            "",
            &self.font_ctx,
            &self.image_cache,
            foreign_object_dpr,
            self.max_iframe_depth,
          )
        } else {
          None
        };
        let svg = resolved_svg.as_deref().unwrap_or(svg);

        let render_w = dest_w_device.ceil().max(1.0) as u32;
        let render_h = dest_h_device.ceil().max(1.0) as u32;
        if render_w > 0 && render_h > 0 {
          if let Ok(pixmap) = self.image_cache.render_svg_pixmap_at_size(
            svg,
            render_w,
            render_h,
            &resolved_src,
            self.scale,
          ) {
            let scale_x = dest_w_device / render_w as f32;
            let scale_y = dest_h_device / render_h as f32;
            if scale_x.is_finite() && scale_y.is_finite() {
              let mut paint = PixmapPaint::default();
              paint.quality = Self::filter_quality_for_image(style);
              let transform = Transform::from_row(
                scale_x,
                0.0,
                0.0,
                scale_y,
                self.device_x(x) + dest_x_device,
                self.device_y(y) + dest_y_device,
              );
              self
                .pixmap
                .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
              return true;
            }
          }
        }
      }
    }

    let quality = Self::filter_quality_for_image(style);
    let target_w = dest_w_device.ceil().max(1.0) as u32;
    let target_h = dest_h_device.ceil().max(1.0) as u32;

    let pixmap_timer = self.diagnostics_enabled.then(Instant::now);
    let should_scale =
      Self::should_use_scaled_raster_pixmap(quality, img_w_raw, img_h_raw, target_w, target_h);
    let pixmap = match if should_scale {
      self.image_cache.load_raster_pixmap_at_size(
        &resolved_src,
        orientation,
        false,
        target_w,
        target_h,
        quality,
      )
    } else {
      self
        .image_cache
        .load_raster_pixmap(&resolved_src, orientation, false)
    } {
      Ok(Some(pixmap)) => pixmap,
      _ => return false,
    };
    if let Some(start) = pixmap_timer {
      with_paint_diagnostics(|diag| {
        diag.image_pixmap_ms += start.elapsed().as_secs_f64() * 1000.0;
      });
    }

    if matches!(
      style.map(|s| s.image_rendering),
      Some(ImageRendering::Pixelated | ImageRendering::CrispEdges)
    ) && (dest_w > img_w_raw as f32 || dest_h > img_h_raw as f32)
    {
      let (snapped_w, offset_x) = snap_upscale(dest_w, img_w_raw as f32).unwrap_or((dest_w, 0.0));
      let (snapped_h, offset_y) = snap_upscale(dest_h, img_h_raw as f32).unwrap_or((dest_h, 0.0));
      dest_w = snapped_w;
      dest_h = snapped_h;
      let dest_x = dest_x + offset_x;
      let dest_y = dest_y + offset_y;
      let dest_x = self.device_length(dest_x);
      let dest_y = self.device_length(dest_y);
      let dest_w = self.device_length(dest_w);
      let dest_h = self.device_length(dest_h);

      let scale_x = dest_w / img_w_raw as f32;
      let scale_y = dest_h / img_h_raw as f32;
      if !scale_x.is_finite() || !scale_y.is_finite() || dest_w <= 0.0 || dest_h <= 0.0 {
        return false;
      }

      let mut paint = PixmapPaint::default();
      paint.quality = Self::filter_quality_for_image(style);

      let transform = Transform::from_row(
        scale_x,
        0.0,
        0.0,
        scale_y,
        self.device_x(x) + dest_x,
        self.device_y(y) + dest_y,
      );
      self
        .pixmap
        .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
      return true;
    }

    let dest_x = self.device_length(dest_x);
    let dest_y = self.device_length(dest_y);
    let dest_w = self.device_length(dest_w);
    let dest_h = self.device_length(dest_h);

    let scale_x = dest_w / pixmap.width() as f32;
    let scale_y = dest_h / pixmap.height() as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() {
      return false;
    }

    let mut paint = PixmapPaint::default();
    paint.quality = quality;

    let transform = Transform::from_row(
      scale_x,
      0.0,
      0.0,
      scale_y,
      self.device_x(x) + dest_x,
      self.device_y(y) + dest_y,
    );
    self
      .pixmap
      .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
    true
  }

  fn paint_solid_rect_simple(&mut self, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return;
    }
    let device_rect = self.device_rect(rect);
    let Some(sk_rect) = SkiaRect::from_xywh(
      device_rect.x(),
      device_rect.y(),
      device_rect.width(),
      device_rect.height(),
    ) else {
      return;
    };
    let path = PathBuilder::from_rect(sk_rect);
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
    paint.anti_alias = true;
    self.pixmap.fill_path(
      &path,
      &paint,
      tiny_skia::FillRule::Winding,
      Transform::identity(),
      clip_mask,
    );
  }

  fn paint_solid_rect_simple_no_aa(&mut self, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return;
    }
    let device_rect = self.device_rect(rect);
    let Some(sk_rect) = SkiaRect::from_xywh(
      device_rect.x(),
      device_rect.y(),
      device_rect.width(),
      device_rect.height(),
    ) else {
      return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
    paint.anti_alias = false;
    self
      .pixmap
      .fill_rect(sk_rect, &paint, Transform::identity(), clip_mask);
  }

  fn paint_play_triangle_icon(&mut self, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
    let w = rect.width().max(0.0);
    let h = rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return;
    }
    let cols = 6usize;
    let col_w = (w / cols as f32).max(0.0);
    if col_w <= 0.0 {
      return;
    }
    for i in 0..cols {
      let t = (i + 1) as f32 / cols as f32;
      let col_h = (h * t).max(0.0);
      let x = rect.x() + col_w * i as f32;
      let y = rect.y() + (h - col_h) * 0.5;
      self.paint_solid_rect_simple(Rect::from_xywh(x, y, col_w, col_h), color, clip_mask);
    }
  }

  fn paint_ui_svg_icon(
    &mut self,
    svg: &str,
    rect: Rect,
    opacity: f32,
    clip_mask: Option<&Mask>,
  ) -> bool {
    let w = rect.width().max(0.0);
    let h = rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 || svg.trim().is_empty() {
      return false;
    }
    let dest_w_device = self.device_length(w);
    let dest_h_device = self.device_length(h);
    if dest_w_device <= 0.0 || dest_h_device <= 0.0 {
      return false;
    }
    let render_w = dest_w_device.ceil().max(1.0) as u32;
    let render_h = dest_h_device.ceil().max(1.0) as u32;
    let pixmap = match self.image_cache.render_svg_pixmap_at_size(
      svg,
      render_w,
      render_h,
      "browser-ui-icon",
      self.scale,
    ) {
      Ok(pixmap) => pixmap,
      Err(_) => return false,
    };
    let scale_x = dest_w_device / render_w as f32;
    let scale_y = dest_h_device / render_h as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() {
      return false;
    }
    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bilinear;
    paint.opacity = opacity.clamp(0.0, 1.0);
    let transform = Transform::from_row(
      scale_x,
      0.0,
      0.0,
      scale_y,
      self.device_x(rect.x()),
      self.device_y(rect.y()),
    );
    self
      .pixmap
      .draw_pixmap(0, 0, pixmap.as_ref().as_ref(), &paint, transform, clip_mask);
    true
  }

  fn paint_video_controls_placeholder_ui(&mut self, content_rect: Rect, clip_mask: Option<&Mask>) {
    let w = content_rect.width().max(0.0);
    let h = content_rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return;
    }

    let bar_h = 32.0_f32.min(h);
    let overlay_bottom = content_rect.max_y() - bar_h;

    let inset = 8.0;
    let icon_size = 12.0;
    let icon_gap = 8.0;
    let progress_h = 4.0;
    let progress_gap = 4.0;
    let knob_r = 4.0;

    let icon_y = overlay_bottom - inset - icon_size;
    let progress_y = icon_y - progress_gap - progress_h;
    if progress_y < content_rect.y() + inset {
      return;
    }

    let track_x = content_rect.x() + inset;
    let track_w = (w - inset * 2.0).max(0.0);
    if track_w <= 0.0 {
      return;
    }

    self.paint_solid_rect_simple(
      Rect::from_xywh(track_x, progress_y, track_w, progress_h),
      Rgba::WHITE.with_alpha(0.35),
      clip_mask,
    );

    // 0% progress knob.
    let knob_center_x = track_x;
    let knob_center_y = progress_y + progress_h * 0.5;
    let radius = self.device_length(knob_r);
    if radius > 0.0 {
      if let Some(path) = PathBuilder::from_circle(
        self.device_x(knob_center_x),
        self.device_y(knob_center_y),
        radius,
      ) {
        let mut paint = Paint::default();
        let color = Rgba::WHITE.with_alpha(0.9);
        paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
        paint.anti_alias = true;
        self.pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          clip_mask,
        );
      }
    }

    const PLAY_SVG: &str = include_str!("../../assets/browser_icons/play.svg");
    const VOLUME_SVG: &str = include_str!("../../assets/browser_icons/volume.svg");
    const MUTE_SVG: &str = include_str!("../../assets/browser_icons/mute.svg");
    const FULLSCREEN_SVG: &str = include_str!("../../assets/browser_icons/fullscreen.svg");

    let play_rect = Rect::from_xywh(track_x, icon_y, icon_size, icon_size);
    if !self.paint_ui_svg_icon(PLAY_SVG, play_rect, 0.85, clip_mask) {
      self.paint_play_triangle_icon(play_rect, Rgba::WHITE.with_alpha(0.85), clip_mask);
    }

    let icon_count = 3;
    let total_w = icon_count as f32 * icon_size + (icon_count as f32 - 1.0) * icon_gap;
    let start_x = content_rect.max_x() - inset - total_w;
    if start_x.is_finite() && start_x >= play_rect.max_x() + icon_gap {
      let icons = [VOLUME_SVG, MUTE_SVG, FULLSCREEN_SVG];
      for (i, svg) in icons.iter().enumerate() {
        let x = start_x + i as f32 * (icon_size + icon_gap);
        let rect = Rect::from_xywh(x, icon_y, icon_size, icon_size);
        if !self.paint_ui_svg_icon(svg, rect, 0.6, clip_mask) {
          self.paint_solid_rect_simple(rect, Rgba::WHITE.with_alpha(0.6), clip_mask);
        }
      }
    }
  }

  fn paint_replaced_placeholder(
    &mut self,
    replaced_type: &ReplacedType,
    style: Option<&ComputedStyle>,
    rect: Rect,
    clip_mask: Option<&Mask>,
  ) {
    // `replaced_type` placeholders are UA-defined and vary between browsers.
    //
    // `<canvas>` is transparent when nothing has been drawn (and we don't execute JS), so a
    // placeholder would incorrectly obscure author backgrounds.
    if matches!(replaced_type, ReplacedType::Canvas) {
      return;
    }
    // `<video>` without a poster often sits above a thumbnail image that should remain visible
    // until the video loads/paints. Keep that pattern working when controls are not shown.
    if matches!(
      replaced_type,
      ReplacedType::Video {
        poster: None,
        controls: false,
        ..
      }
    ) {
      return;
    }
    // For `<video controls>` without a poster/frame, browsers still paint a dark video surface and
    // chrome for the native controls. Emit a stable approximation rather than leaving the element
    // transparent.
    if let ReplacedType::Video {
      poster: None,
      controls: true,
      ..
    } = replaced_type
    {
      if rect.width() > 0.0 && rect.height() > 0.0 {
        let device_rect = self.device_rect(rect);
        if let Some(sk_rect) = SkiaRect::from_xywh(
          device_rect.x(),
          device_rect.y(),
          device_rect.width(),
          device_rect.height(),
        ) {
          let path = PathBuilder::from_rect(sk_rect);
          let mut paint = Paint::default();
          paint.set_color_rgba8(51, 51, 51, 255);
          paint.anti_alias = true;
          self.pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            clip_mask,
          );

          // Approximate Chrome's control-bar shadow: a black overlay that ramps up near the bottom
          // of the element.
          let h = rect.height().max(0.0);
          let bar_h = 32.0_f32.min(h);
          let shadow_h = 72.0_f32.min((h - bar_h).max(0.0));
          if shadow_h > 0.0 {
            let start = if h > 0.0 {
              (h - bar_h - shadow_h) / h
            } else {
              0.0
            };
            let end = if h > 0.0 { (h - bar_h) / h } else { 1.0 };
            let end_alpha = 0.85;
            let black = |alpha: f32| Rgba::rgb(0, 0, 0).with_alpha(alpha.clamp(0.0, 1.0));
            // Piecewise-linear approximation of an ease-in curve (t^1.5) so the top stays light.
            let mut stops: Vec<(f32, Rgba)> = Vec::new();
            stops.push((0.0, black(0.0)));
            stops.push((start, black(0.0)));
            for t in [0.25_f32, 0.5, 0.75] {
              let pos = start + (end - start) * t;
              let alpha = end_alpha * t.powf(1.5);
              stops.push((pos, black(alpha)));
            }
            stops.push((end, black(end_alpha)));
            stops.push((1.0, black(end_alpha)));
            self.paint_linear_gradient(
              rect,
              rect,
              clip_mask,
              180.0,
              &stops,
              SpreadMode::Pad,
              MixBlendMode::Normal,
            );
          }

          self.paint_video_controls_placeholder_ui(rect, clip_mask);
        }
      }
      return;
    }

    let log_placeholder = runtime::runtime_toggles().truthy("FASTR_LOG_IMAGE_FAIL");
    if log_placeholder {
      if let ReplacedType::Image { src, .. } = replaced_type {
        eprintln!("[image-placeholder] src={}", src);
      }
    }

    if matches!(replaced_type, ReplacedType::Image { .. }) {
      self.paint_missing_image_placeholder(rect, clip_mask);
      return;
    }

    let mut paint = Paint::default();
    paint.set_color_rgba8(200, 200, 200, 255); // Light gray
    paint.anti_alias = true;

    let device_rect = self.device_rect(rect);

    if let Some(sk_rect) = SkiaRect::from_xywh(
      device_rect.x(),
      device_rect.y(),
      device_rect.width(),
      device_rect.height(),
    ) {
      let path = PathBuilder::from_rect(sk_rect);
      self.pixmap.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip_mask,
      );

      // Border around the placeholder
      let mut stroke_paint = Paint::default();
      stroke_paint.set_color_rgba8(150, 150, 150, 255);
      stroke_paint.anti_alias = true;

      let stroke = tiny_skia::Stroke {
        width: 1.0 * self.scale,
        ..Default::default()
      };
      self.pixmap.stroke_path(
        &path,
        &stroke_paint,
        &stroke,
        Transform::identity(),
        clip_mask,
      );
    }

    // Optional label to hint the missing resource type
    let label = replaced_type.placeholder_label();

    if let (Some(style), Some(label_text)) = (style, label) {
      let mut label_style = style.clone();
      label_style.color = Rgba::rgb(120, 120, 120);
      // Use a small inset to avoid clipping against the placeholder edges
      let inset = 2.0;
      let label_rect = Rect::from_xywh(
        rect.x() + inset,
        rect.y() + inset,
        (rect.width() - 2.0 * inset).max(0.0),
        (rect.height() - 2.0 * inset).max(0.0),
      );
      let _ = self.paint_alt_text(label_text, &label_style, label_rect, clip_mask);
    }
  }

  fn paint_fill_rect_crisp(&mut self, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
    if color.a <= 0.0
      || !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
      || rect.width() <= 0.0
      || rect.height() <= 0.0
    {
      return;
    }
    self.paint_solid_rect_crisp(rect, color, clip_mask);
  }

  fn paint_solid_rect_crisp(&mut self, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
    if color.a <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
      return;
    }

    let device_rect = self.device_rect(rect);
    if device_rect.width() <= 0.0 || device_rect.height() <= 0.0 {
      return;
    }
    let Some(sk_rect) = SkiaRect::from_xywh(
      device_rect.x(),
      device_rect.y(),
      device_rect.width(),
      device_rect.height(),
    ) else {
      return;
    };
    let mut paint = Paint::default();
    paint.set_color(tiny_skia::Color::from_rgba8(
      color.r,
      color.g,
      color.b,
      color.alpha_u8(),
    ));
    paint.anti_alias = false;
    self
      .pixmap
      .fill_rect(sk_rect, &paint, Transform::identity(), clip_mask);
  }

  fn missing_image_icon_size(content_rect: Rect) -> f32 {
    // Keep behaviour in sync with `DisplayListBuilder::missing_image_icon_size` so the immediate
    // paint path matches the display-list pipeline.
    if !content_rect.width().is_finite() || !content_rect.height().is_finite() {
      return 0.0;
    }
    let icon_inset = 2.0;
    let max_icon_size = 16.0;
    let available_w = (content_rect.width() - icon_inset * 2.0).max(0.0);
    let available_h = (content_rect.height() - icon_inset * 2.0).max(0.0);
    let icon_size = (available_w.min(available_h) * 0.5)
      .floor()
      .clamp(0.0, max_icon_size);
    if icon_size >= 8.0 { icon_size } else { 0.0 }
  }

  fn paint_inside_border_rect(&mut self, rect: Rect, color: Rgba, clip_mask: Option<&Mask>) {
    // Keep behaviour in sync with `DisplayListBuilder::emit_inside_border_rect` so broken-image
    // placeholders render consistently across the display-list and immediate paint paths.
    if !rect.width().is_finite() || !rect.height().is_finite() {
      return;
    }
    let w = rect.width().max(0.0);
    let h = rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return;
    }

    // Chrome's 1px border sits inside the image box. Using `stroke_path` would center the stroke
    // on the edge, which gets clipped/anti-aliased and ends up darker than browsers.
    let thickness: f32 = 1.0;
    let th = thickness.min(h);
    let tw = thickness.min(w);
    let x = rect.x();
    let y = rect.y();
    let bottom_y = y + h - th;
    let right_x = x + w - tw;

    self.paint_fill_rect_crisp(Rect::from_xywh(x, y, w, th), color, clip_mask);
    if bottom_y > y {
      self.paint_fill_rect_crisp(Rect::from_xywh(x, bottom_y, w, th), color, clip_mask);
    }

    self.paint_fill_rect_crisp(Rect::from_xywh(x, y, tw, h), color, clip_mask);
    if right_x > x {
      self.paint_fill_rect_crisp(Rect::from_xywh(right_x, y, tw, h), color, clip_mask);
    }
  }

  fn paint_broken_image_icon(&mut self, icon_rect: Rect, clip_mask: Option<&Mask>) {
    // Keep behaviour in sync with `DisplayListBuilder::emit_broken_image_icon` so broken-image
    // placeholders render consistently across the display-list and immediate paint paths.
    let inner_rect = Rect::from_xywh(
      icon_rect.x() + 1.0,
      icon_rect.y() + 1.0,
      (icon_rect.width() - 2.0).max(0.0),
      (icon_rect.height() - 2.0).max(0.0),
    );

    if inner_rect.width() > 0.0 && inner_rect.height() > 0.0 {
      self.paint_fill_rect_crisp(inner_rect, Rgba::WHITE, clip_mask);

      // Sky.
      let sky_h = (inner_rect.height() * 0.62)
        .floor()
        .clamp(0.0, inner_rect.height());
      if sky_h > 0.0 {
        self.paint_fill_rect_crisp(
          Rect::from_xywh(inner_rect.x(), inner_rect.y(), inner_rect.width(), sky_h),
          Rgba::rgb(198, 216, 244),
          clip_mask,
        );
      }

      // Ground.
      let ground_h = (inner_rect.height() * 0.3)
        .floor()
        .clamp(0.0, inner_rect.height());
      if ground_h > 0.0 {
        self.paint_fill_rect_crisp(
          Rect::from_xywh(
            inner_rect.x(),
            inner_rect.y() + inner_rect.height() - ground_h,
            inner_rect.width(),
            ground_h,
          ),
          Rgba::rgb(88, 174, 57),
          clip_mask,
        );
      }

      // "Sun" highlight.
      self.paint_fill_rect_crisp(
        Rect::from_xywh(inner_rect.x() + 2.0, inner_rect.y() + 2.0, 3.0, 3.0),
        Rgba::WHITE,
        clip_mask,
      );
    }

    self.paint_inside_border_rect(icon_rect, Rgba::rgb(192, 192, 192), clip_mask);
  }

  fn paint_missing_image_placeholder(&mut self, content_rect: Rect, clip_mask: Option<&Mask>) {
    self.paint_inside_border_rect(content_rect, Rgba::rgb(192, 192, 192), clip_mask);
    let icon_inset = 2.0;
    let icon_size = Self::missing_image_icon_size(content_rect);
    if icon_size > 0.0 {
      self.paint_broken_image_icon(
        Rect::from_xywh(
          content_rect.x() + icon_inset,
          content_rect.y() + icon_inset,
          icon_size,
          icon_size,
        ),
        clip_mask,
      );
    }
  }

  fn effective_text_align(style: &ComputedStyle) -> crate::style::types::TextAlign {
    use crate::style::types::{Direction, TextAlign};
    match style.text_align {
      TextAlign::Start | TextAlign::MatchParent | TextAlign::Justify | TextAlign::JustifyAll => {
        if style.direction == Direction::Rtl {
          TextAlign::Right
        } else {
          TextAlign::Left
        }
      }
      TextAlign::End => {
        if style.direction == Direction::Rtl {
          TextAlign::Left
        } else {
          TextAlign::Right
        }
      }
      other => other,
    }
  }

  fn aligned_text_start_x(style: &ComputedStyle, rect: Rect, advance_width: f32) -> f32 {
    use crate::style::types::TextAlign;
    let advance_width = if advance_width.is_finite() {
      advance_width.max(0.0)
    } else {
      0.0
    };
    match Self::effective_text_align(style) {
      TextAlign::Center => rect.x() + ((rect.width() - advance_width).max(0.0) / 2.0),
      TextAlign::Right => rect.x() + (rect.width() - advance_width).max(0.0),
      _ => rect.x(),
    }
  }

  fn paint_alt_text(
    &mut self,
    alt: &str,
    style: &ComputedStyle,
    rect: Rect,
    clip_mask: Option<&Mask>,
  ) -> bool {
    self.paint_alt_text_impl(alt, style, rect, clip_mask, true)
  }

  fn paint_alt_text_raw(
    &mut self,
    alt: &str,
    style: &ComputedStyle,
    rect: Rect,
    clip_mask: Option<&Mask>,
  ) -> bool {
    self.paint_alt_text_impl(alt, style, rect, clip_mask, false)
  }

  fn paint_alt_text_impl(
    &mut self,
    alt: &str,
    style: &ComputedStyle,
    rect: Rect,
    clip_mask: Option<&Mask>,
    trim: bool,
  ) -> bool {
    let text = if trim {
      trim_ascii_whitespace_html_css(alt)
    } else {
      alt
    };
    if text.is_empty() {
      return false;
    }

    let mut runs = match self.shaper.shape(text, style, &self.font_ctx) {
      Ok(runs) => runs,
      Err(_) => return false,
    };
    if runs.is_empty() {
      return false;
    }

    TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);

    let metrics_scaled = self.resolve_scaled_metrics(style);
    let mut line_height = compute_line_height_with_metrics_viewport(
      style,
      metrics_scaled.as_ref(),
      Some(Size::new(self.css_width, self.css_height)),
      self.font_ctx.root_font_metrics(),
    );
    // Fall back to the legacy single-line behaviour when line-height is unusable.
    if !line_height.is_finite() || line_height <= 0.0 {
      line_height = style.font_size.max(1.0);
    }

    let metrics = TextItem::metrics_from_runs(&self.font_ctx, &runs, line_height, style.font_size);

    // Match browser behaviour for missing-image alt text: wrap to multiple lines when the replaced
    // box is narrow. Reuse the inline layout engine's line-breaking primitives so wrapping follows
    // CSS `white-space` / `word-break` / `overflow-wrap` rules.
    let text_item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      crate::text::line_break::find_break_opportunities(text),
      Vec::new(),
      Arc::new(style.clone()),
      style.direction,
    );

    let max_lines = if rect.height().is_finite() && rect.height() > 0.0 {
      (rect.height() / line_height).floor() as usize
    } else {
      1
    }
    .max(1);

    let allows_soft_wrap = !matches!(
      style.white_space,
      crate::style::types::WhiteSpace::Nowrap | crate::style::types::WhiteSpace::Pre
    ) && !matches!(style.text_wrap, crate::style::types::TextWrap::NoWrap);
    let preserves_newlines = matches!(
      style.white_space,
      crate::style::types::WhiteSpace::Pre
        | crate::style::types::WhiteSpace::PreWrap
        | crate::style::types::WhiteSpace::PreLine
        | crate::style::types::WhiteSpace::BreakSpaces
    );

    let mut remaining = text_item;
    let mut reshape_cache = crate::layout::contexts::inline::line_builder::ReshapeCache::default();
    let mut y = rect.y();
    let mut painted_any = false;

    let mut line_idx = 0usize;
    while line_idx < max_lines {
      if remaining.text.is_empty() {
        break;
      }

      let line_rect = Rect::from_xywh(rect.x(), y, rect.width(), line_height);

      // Break at a mandatory break (e.g. newline) regardless of width, and at allowed breaks only
      // when overflowing.
      let mut break_opportunity = remaining.find_break_point(line_rect.width());
      if break_opportunity.is_none() && allows_soft_wrap && remaining.advance > line_rect.width() {
        break_opportunity = remaining
          .break_opportunities
          .iter()
          .copied()
          .find(|b| b.byte_offset > 0 && b.byte_offset < remaining.text.len());
      }

      let should_wrap = break_opportunity.is_some_and(|brk| {
        if matches!(
          brk.break_type,
          crate::text::line_break::BreakType::Mandatory
        ) {
          preserves_newlines
        } else {
          allows_soft_wrap && remaining.advance > line_rect.width()
        }
      });

      if should_wrap {
        if let Some(brk) = break_opportunity {
          if let Some((mut before, after)) = remaining.split_at(
            brk.byte_offset,
            brk.adds_hyphen,
            &self.shaper,
            &self.font_ctx,
            &mut reshape_cache,
          ) {
            // CSS trims collapsible trailing spaces at soft wrap opportunities. Mirror the inline
            // formatting context so width calculations don't include those spaces.
            let mut drop_before = false;
            if matches!(brk.break_type, crate::text::line_break::BreakType::Allowed)
              && matches!(
                before.style.white_space,
                crate::style::types::WhiteSpace::Normal
                  | crate::style::types::WhiteSpace::Nowrap
                  | crate::style::types::WhiteSpace::PreLine
              )
            {
              let trimmed_len = before.text.trim_end_matches(' ').len();
              if trimmed_len < before.text.len() {
                if trimmed_len == 0 {
                  drop_before = true;
                } else if let Some((trimmed, _)) = before.split_at(
                  trimmed_len,
                  false,
                  &self.shaper,
                  &self.font_ctx,
                  &mut reshape_cache,
                ) {
                  before = trimmed;
                }
              }
            }

            if !drop_before && before.advance > 0.0 {
              let start_x = Self::aligned_text_start_x(style, line_rect, before.advance);
              let half_leading = (before.metrics.line_height
                - (before.metrics.ascent + before.metrics.descent))
                / 2.0;
              let baseline_y = y + half_leading + before.metrics.baseline_offset;
              self.paint_shaped_runs(
                &before.runs,
                start_x,
                baseline_y,
                style.color,
                Some(style),
                clip_mask,
              );
              painted_any = true;
            }

            // If the wrapped segment contained only collapsible whitespace, keep consuming from the
            // remainder without advancing the line so we don't output empty lines for leading spaces.
            if drop_before && matches!(brk.break_type, crate::text::line_break::BreakType::Allowed) {
              remaining = after;
              continue;
            }

            remaining = after;
            y += line_height;
            line_idx += 1;
            continue;
          }
        }
      }

      // Either the remaining text fits, wrapping is disabled, or we couldn't split cleanly.
      let start_x = Self::aligned_text_start_x(style, line_rect, remaining.advance);
      let half_leading = (remaining.metrics.line_height
        - (remaining.metrics.ascent + remaining.metrics.descent))
        / 2.0;
      let baseline_y = y + half_leading + remaining.metrics.baseline_offset;

      self.paint_shaped_runs(
        &remaining.runs,
        start_x,
        baseline_y,
        style.color,
        Some(style),
        clip_mask,
      );
      painted_any = true;
      break;
    }

    painted_any
  }

  fn measure_alt_text(&self, alt: &str, style: &ComputedStyle) -> Option<Size> {
    self.measure_alt_text_impl(alt, style, true)
  }

  fn measure_alt_text_raw(&self, alt: &str, style: &ComputedStyle) -> Option<Size> {
    self.measure_alt_text_impl(alt, style, false)
  }

  fn measure_alt_text_impl(&self, alt: &str, style: &ComputedStyle, trim: bool) -> Option<Size> {
    let text = if trim {
      trim_ascii_whitespace_html_css(alt)
    } else {
      alt
    };
    if text.is_empty() {
      return None;
    }
    let width: f32 = if style.letter_spacing == 0.0 && style.word_spacing == 0.0 {
      let runs = self.shaper.shape_arc(text, style, &self.font_ctx).ok()?;
      runs.iter().map(|r| r.advance).sum()
    } else {
      let mut runs = self.shaper.shape(text, style, &self.font_ctx).ok()?;
      TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
      runs.iter().map(|r| r.advance).sum()
    };
    let metrics_scaled = Self::resolve_scaled_metrics_static(style, &self.font_ctx);
    let line_height = compute_line_height_with_metrics_viewport(
      style,
      metrics_scaled.as_ref(),
      Some(Size::new(self.css_width, self.css_height)),
      self.font_ctx.root_font_metrics(),
    );
    Some(Size::new(width, line_height))
  }

  fn decoration_metrics<'a>(
    &self,
    runs: Option<&'a [ShapedRun]>,
    style: &ComputedStyle,
  ) -> Option<DecorationMetrics> {
    let mut metrics_source = runs.and_then(|rs| {
      rs.iter().find_map(|run| {
        let coords: Vec<_> = run.variations.iter().map(|v| (v.tag, v.value)).collect();
        let metrics = if coords.is_empty() {
          run.font.metrics()
        } else {
          run
            .font
            .metrics_with_variations(&coords)
            .or_else(|_| run.font.metrics())
        }
        .ok()?;
        Some((metrics, run.font_size * run.scale))
      })
    });

    if metrics_source.is_none() {
      let italic = matches!(style.font_style, crate::style::types::FontStyle::Italic);
      let oblique = matches!(style.font_style, crate::style::types::FontStyle::Oblique(_));
      let stretch =
        crate::text::font_db::FontStretch::from_percentage(style.font_stretch.to_percentage());
      let preferred_aspect = crate::text::pipeline::preferred_font_aspect(style, &self.font_ctx);
      metrics_source = self
        .font_ctx
        .get_font_full(
          &style.font_family,
          style.font_weight.to_u16(),
          if italic {
            crate::text::font_db::FontStyle::Italic
          } else if oblique {
            crate::text::font_db::FontStyle::Oblique
          } else {
            crate::text::font_db::FontStyle::Normal
          },
          stretch,
        )
        .or_else(|| self.font_ctx.get_sans_serif())
        .and_then(|font| {
          let used_font_size =
            crate::text::pipeline::compute_adjusted_font_size(style, &font, preferred_aspect);
          let authored = crate::text::variations::authored_variations_from_style(style);
          let variations = crate::text::face_cache::with_face(&font, |face| {
            crate::text::variations::collect_variations_for_face(
              face,
              style,
              &font,
              used_font_size,
              &authored,
            )
          })
          .unwrap_or_else(|| authored.clone());
          let coords: Vec<_> = variations.iter().map(|v| (v.tag, v.value)).collect();
          let metrics = if coords.is_empty() {
            font.metrics().ok()?
          } else {
            font
              .metrics_with_variations(&coords)
              .or_else(|_| font.metrics())
              .ok()?
          };
          let size_adjust = if font.face_metrics_overrides.size_adjust.is_finite()
            && font.face_metrics_overrides.size_adjust > 0.0
          {
            font.face_metrics_overrides.size_adjust
          } else {
            1.0
          };
          Some((metrics, used_font_size * size_adjust))
        });
    }

    if let Some((metrics, size)) = metrics_source {
      let scale = size / (metrics.units_per_em as f32);

      let underline_pos = metrics.underline_position as f32 * scale;
      let underline_thickness = (metrics.underline_thickness as f32 * scale).max(1.0);
      let descent = (metrics.descent as f32 * scale).abs();
      let strike_pos = metrics
        .strikeout_position
        .map(|p| p as f32 * scale)
        .unwrap_or_else(|| metrics.ascent as f32 * scale * 0.3);
      let strike_thickness = metrics
        .strikeout_thickness
        .map(|t| t as f32 * scale)
        .unwrap_or(underline_thickness);
      let ascent = metrics.ascent as f32 * scale;

      Some(DecorationMetrics {
        underline_pos,
        underline_thickness,
        strike_pos,
        strike_thickness,
        ascent,
        descent,
        has_font_underline_metrics: true,
      })
    } else {
      // Fallback heuristic metrics when we cannot obtain font metrics.
      let size = style.font_size.max(1.0);
      let ascent = size * 0.8;
      let descent = size - ascent;
      let underline_thickness = (size * 0.1).max(1.0);
      let underline_pos = descent * 0.5;
      let strike_pos = ascent * 0.4;

      Some(DecorationMetrics {
        underline_pos,
        underline_thickness,
        strike_pos,
        strike_thickness: underline_thickness,
        ascent,
        descent,
        has_font_underline_metrics: false,
      })
    }
  }

  fn resolve_underline_offset_length(
    &self,
    offset: crate::style::types::TextUnderlineOffset,
    style: &ComputedStyle,
  ) -> Option<f32> {
    match offset {
      crate::style::types::TextUnderlineOffset::Auto => None,
      crate::style::types::TextUnderlineOffset::Length(l) => {
        let resolved = if l.unit == LengthUnit::Percent {
          l.resolve_against(style.font_size).unwrap_or(0.0)
        } else if l.unit.is_viewport_relative() {
          l.resolve_with_viewport_for_writing_mode(
            self.css_width,
            self.css_height,
            style.writing_mode,
          )
          .unwrap_or_else(|| l.to_px())
        } else {
          resolve_font_relative_length(l, style, &self.font_ctx)
        };
        Some(resolved * self.scale)
      }
    }
  }

  fn underline_auto_offset_value(
    &self,
    metrics: &DecorationMetrics,
    position: crate::style::types::TextUnderlinePosition,
    inline_vertical: bool,
  ) -> f32 {
    // Font underline metrics (`post` table / FreeType / Skia) typically describe the *center* of
    // the underline bar. CSS underline positioning is defined in terms of an "edge" that is then
    // offset away from the text, so adjust the metric to refer to the bar's edge closest to the
    // text.
    //
    // Without this adjustment, underlines end up consistently shifted by ~½ the underline
    // thickness (often a full device pixel after snapping), which shows up prominently in
    // Chrome-vs-FastRender diffs on pages with many underlined links (e.g. `weibo.cn`).
    let underline_pos =
      if metrics.underline_pos.is_finite() && metrics.underline_thickness.is_finite() {
        metrics.underline_pos + metrics.underline_thickness * 0.5
      } else {
        metrics.underline_pos
      };

    // `text-underline-offset: auto` is UA-defined. We preserve existing underline placement
    // behavior by defaulting to the font-provided underline position (or clamping to the
    // text-under edge for `text-underline-position: under`). The CSS Text Decoration Level 4 spec
    // requires this offset to be zero when `text-underline-position: from-font` and font metrics
    // are available.
    match position {
      crate::style::types::TextUnderlinePosition::FromFont
        if metrics.has_font_underline_metrics =>
      {
        0.0
      }
      crate::style::types::TextUnderlinePosition::Under
      | crate::style::types::TextUnderlinePosition::UnderLeft
      | crate::style::types::TextUnderlinePosition::UnderRight => {
        (-metrics.descent - underline_pos).max(0.0)
      }
      crate::style::types::TextUnderlinePosition::Left if inline_vertical => {
        (-metrics.descent - underline_pos).max(0.0)
      }
      crate::style::types::TextUnderlinePosition::Right if inline_vertical => 0.0,
      _ => (-underline_pos).max(0.0),
    }
  }

  fn underline_center(
    &self,
    metrics: &DecorationMetrics,
    position: crate::style::types::TextUnderlinePosition,
    offset: crate::style::types::TextUnderlineOffset,
    thickness: f32,
    baseline: f32,
    inline_vertical: bool,
    style: &ComputedStyle,
  ) -> f32 {
    // In horizontal typographic modes, `left` and `right` behave as `auto`. In vertical
    // typographic modes, `under left` / `under right` collapse to `left` / `right` since `under`
    // applies only to horizontal positioning. (See CSS Text Decoration Level 4 §7.4.)
    let position = if inline_vertical {
      match position {
        crate::style::types::TextUnderlinePosition::UnderLeft => {
          crate::style::types::TextUnderlinePosition::Left
        }
        crate::style::types::TextUnderlinePosition::UnderRight => {
          crate::style::types::TextUnderlinePosition::Right
        }
        _ => position,
      }
    } else {
      match position {
        crate::style::types::TextUnderlinePosition::Left
        | crate::style::types::TextUnderlinePosition::Right => {
          crate::style::types::TextUnderlinePosition::Auto
        }
        _ => position,
      }
    };

    let over_positioned =
      inline_vertical && matches!(position, crate::style::types::TextUnderlinePosition::Right);
    let dir = if over_positioned { 1.0 } else { -1.0 };
    let zero_pos = match position {
      crate::style::types::TextUnderlinePosition::Auto => 0.0,
      crate::style::types::TextUnderlinePosition::FromFont => {
        if metrics.has_font_underline_metrics {
          if metrics.underline_pos.is_finite() && metrics.underline_thickness.is_finite() {
            metrics.underline_pos + metrics.underline_thickness * 0.5
          } else {
            metrics.underline_pos
          }
        } else {
          0.0
        }
      }
      crate::style::types::TextUnderlinePosition::Under
      | crate::style::types::TextUnderlinePosition::UnderLeft
      | crate::style::types::TextUnderlinePosition::UnderRight
      | crate::style::types::TextUnderlinePosition::Left => -metrics.descent,
      crate::style::types::TextUnderlinePosition::Right => {
        if inline_vertical {
          metrics.ascent
        } else {
          0.0
        }
      }
    };

    let used_offset = self
      .resolve_underline_offset_length(offset, style)
      .unwrap_or_else(|| self.underline_auto_offset_value(metrics, position, inline_vertical));
    let center_pos = zero_pos + dir * (used_offset + thickness * 0.5);

    if inline_vertical {
      let underline_side = resolve_underline_side(style.writing_mode, position);
      let over_side = if over_positioned {
        underline_side
      } else {
        match underline_side {
          UnderlineSide::Left => UnderlineSide::Right,
          UnderlineSide::Right => UnderlineSide::Left,
        }
      };
      let mapping = match over_side {
        UnderlineSide::Right => 1.0,
        UnderlineSide::Left => -1.0,
      };
      baseline + mapping * center_pos
    } else {
      baseline - center_pos
    }
  }

  fn resolve_decoration_thickness_value(
    &self,
    thickness: TextDecorationThickness,
    style: &ComputedStyle,
  ) -> Option<f32> {
    match thickness {
      TextDecorationThickness::Auto => Some((style.font_size * 0.1).max(1.0) * self.scale),
      TextDecorationThickness::FromFont => None,
      TextDecorationThickness::Length(l) => {
        if l.unit == LengthUnit::Percent {
          Some(l.resolve_against(style.font_size).unwrap_or(0.0) * self.scale)
        } else if l.unit.is_viewport_relative() {
          l.resolve_with_viewport_for_writing_mode(
            self.css_width,
            self.css_height,
            style.writing_mode,
          )
          .map(|v| v * self.scale)
        } else {
          Some(resolve_font_relative_length(l, style, &self.font_ctx) * self.scale)
        }
      }
    }
  }

  fn render_generated_image(
    &mut self,
    bg: &BackgroundImage,
    style: &ComputedStyle,
    width: u32,
    height: u32,
  ) -> Option<Pixmap> {
    if width == 0 || height == 0 {
      return None;
    }

    let rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
    match bg {
      BackgroundImage::LinearGradient { angle, stops } => {
        let gradient_rect = rect;
        let rad = angle.to_radians();
        let dx = rad.sin();
        let dy = -rad.cos();
        let gradient_length = gradient_rect.width() * dx.abs() + gradient_rect.height() * dy.abs();
        let resolved = normalize_color_stops(
          stops,
          style.color,
          gradient_length,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let len = 0.5 * (gradient_rect.width() * dx.abs() + gradient_rect.height() * dy.abs());
        let cx = gradient_rect.x() + gradient_rect.width() / 2.0;
        let cy = gradient_rect.y() + gradient_rect.height() / 2.0;

        let start = Point::new(cx - dx * len, cy - dy * len);
        let end = Point::new(cx + dx * len, cy + dy * len);
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_linear_gradient(
          width,
          height,
          start,
          end,
          SpreadMode::Pad,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(width.max(height)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RepeatingLinearGradient { angle, stops } => {
        let gradient_rect = rect;
        let rad = angle.to_radians();
        let dx = rad.sin();
        let dy = -rad.cos();
        let gradient_length = gradient_rect.width() * dx.abs() + gradient_rect.height() * dy.abs();
        let resolved = normalize_color_stops(
          stops,
          style.color,
          gradient_length,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let len = 0.5 * (gradient_rect.width() * dx.abs() + gradient_rect.height() * dy.abs());
        let cx = gradient_rect.x() + gradient_rect.width() / 2.0;
        let cy = gradient_rect.y() + gradient_rect.height() / 2.0;

        let start = Point::new(cx - dx * len, cy - dy * len);
        let end = Point::new(cx + dx * len, cy + dy * len);
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_linear_gradient(
          width,
          height,
          start,
          end,
          SpreadMode::Repeat,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(width.max(height)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RadialGradient {
        shape,
        size,
        position,
        stops,
      } => {
        let (cx, cy, radius_x, radius_y) = radial_geometry(
          rect,
          position,
          size,
          *shape,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let resolved = normalize_color_stops(
          stops,
          style.color,
          radius_x.max(radius_y),
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let skia_stops = gradient_stops(&resolved);
        let transform = Transform::from_translate(cx, cy).pre_scale(radius_x, radius_y);
        let shader = RadialGradient::new(
          tiny_skia::Point::from_xy(0.0, 0.0),
          tiny_skia::Point::from_xy(0.0, 0.0),
          1.0,
          skia_stops,
          SpreadMode::Pad,
          transform,
        )?;

        let timer = self.diagnostics_enabled.then(Instant::now);
        let mut pixmap = new_pixmap(width, height)?;
        let skia_rect = SkiaRect::from_xywh(0.0, 0.0, width as f32, height as f32)?;
        let path = PathBuilder::from_rect(skia_rect);
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          None,
        );
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RepeatingRadialGradient {
        shape,
        size,
        position,
        stops,
      } => {
        let (cx, cy, radius_x, radius_y) = radial_geometry(
          rect,
          position,
          size,
          *shape,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let resolved = normalize_color_stops(
          stops,
          style.color,
          radius_x.max(radius_y),
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let skia_stops = gradient_stops(&resolved);
        let transform = Transform::from_translate(cx, cy).pre_scale(radius_x, radius_y);
        let shader = RadialGradient::new(
          tiny_skia::Point::from_xy(0.0, 0.0),
          tiny_skia::Point::from_xy(0.0, 0.0),
          1.0,
          skia_stops,
          SpreadMode::Repeat,
          transform,
        )?;

        let timer = self.diagnostics_enabled.then(Instant::now);
        let mut pixmap = new_pixmap(width, height)?;
        let skia_rect = SkiaRect::from_xywh(0.0, 0.0, width as f32, height as f32)?;
        let path = PathBuilder::from_rect(skia_rect);
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          None,
        );
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::ConicGradient {
        from_angle,
        position,
        stops,
      } => {
        let resolved = normalize_color_stops_unclamped(
          stops,
          style.color,
          1.0,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let center = resolve_gradient_center(
          rect,
          position,
          rect,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_conic_gradient(
          width,
          height,
          center,
          from_angle.to_radians(),
          SpreadMode::Pad,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(width.max(height).saturating_mul(2)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RepeatingConicGradient {
        from_angle,
        position,
        stops,
      } => {
        let resolved = normalize_color_stops_unclamped(
          stops,
          style.color,
          1.0,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let center = resolve_gradient_center(
          rect,
          position,
          rect,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_conic_gradient(
          width,
          height,
          center,
          from_angle.to_radians(),
          SpreadMode::Repeat,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(width.max(height).saturating_mul(2)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::Url(_) | BackgroundImage::None => None,
    }
  }

  fn render_generated_image_region(
    &mut self,
    bg: &BackgroundImage,
    style: &ComputedStyle,
    full_w: u32,
    full_h: u32,
    region_x: u32,
    region_y: u32,
    width: u32,
    height: u32,
  ) -> Option<Pixmap> {
    if width == 0 || height == 0 || full_w == 0 || full_h == 0 {
      return None;
    }

    let full_rect = Rect::from_xywh(0.0, 0.0, full_w as f32, full_h as f32);
    let paint_rect = Rect::from_xywh(
      region_x as f32,
      region_y as f32,
      width as f32,
      height as f32,
    );
    let offset = Point::new(region_x as f32, region_y as f32);

    match bg {
      BackgroundImage::LinearGradient { angle, stops } => {
        let rad = angle.to_radians();
        let dx = rad.sin();
        let dy = -rad.cos();
        let gradient_length = full_rect.width() * dx.abs() + full_rect.height() * dy.abs();
        let resolved = normalize_color_stops(
          stops,
          style.color,
          gradient_length,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let len = 0.5 * (full_rect.width() * dx.abs() + full_rect.height() * dy.abs());
        let cx = full_rect.width() / 2.0;
        let cy = full_rect.height() / 2.0;
        let start = Point::new(cx - dx * len - offset.x, cy - dy * len - offset.y);
        let end = Point::new(cx + dx * len - offset.x, cy + dy * len - offset.y);
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_linear_gradient(
          width,
          height,
          start,
          end,
          SpreadMode::Pad,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(full_w.max(full_h)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RepeatingLinearGradient { angle, stops } => {
        let rad = angle.to_radians();
        let dx = rad.sin();
        let dy = -rad.cos();
        let gradient_length = full_rect.width() * dx.abs() + full_rect.height() * dy.abs();
        let resolved = normalize_color_stops(
          stops,
          style.color,
          gradient_length,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let len = 0.5 * (full_rect.width() * dx.abs() + full_rect.height() * dy.abs());
        let cx = full_rect.width() / 2.0;
        let cy = full_rect.height() / 2.0;
        let start = Point::new(cx - dx * len - offset.x, cy - dy * len - offset.y);
        let end = Point::new(cx + dx * len - offset.x, cy + dy * len - offset.y);
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_linear_gradient(
          width,
          height,
          start,
          end,
          SpreadMode::Repeat,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(full_w.max(full_h)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RadialGradient {
        shape,
        size,
        position,
        stops,
      } => {
        let (cx, cy, radius_x, radius_y) = radial_geometry(
          full_rect,
          position,
          size,
          *shape,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let resolved = normalize_color_stops(
          stops,
          style.color,
          radius_x.max(radius_y),
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let skia_stops = gradient_stops(&resolved);
        let cx = cx - offset.x;
        let cy = cy - offset.y;
        let transform = Transform::from_translate(cx, cy).pre_scale(radius_x, radius_y);
        let shader = RadialGradient::new(
          tiny_skia::Point::from_xy(0.0, 0.0),
          tiny_skia::Point::from_xy(0.0, 0.0),
          1.0,
          skia_stops,
          SpreadMode::Pad,
          transform,
        )?;

        let timer = self.diagnostics_enabled.then(Instant::now);
        let mut pixmap = new_pixmap(width, height)?;
        let skia_rect = SkiaRect::from_xywh(0.0, 0.0, width as f32, height as f32)?;
        let path = PathBuilder::from_rect(skia_rect);
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          None,
        );
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RepeatingRadialGradient {
        shape,
        size,
        position,
        stops,
      } => {
        let (cx, cy, radius_x, radius_y) = radial_geometry(
          full_rect,
          position,
          size,
          *shape,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let resolved = normalize_color_stops(
          stops,
          style.color,
          radius_x.max(radius_y),
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let skia_stops = gradient_stops(&resolved);
        let cx = cx - offset.x;
        let cy = cy - offset.y;
        let transform = Transform::from_translate(cx, cy).pre_scale(radius_x, radius_y);
        let shader = RadialGradient::new(
          tiny_skia::Point::from_xy(0.0, 0.0),
          tiny_skia::Point::from_xy(0.0, 0.0),
          1.0,
          skia_stops,
          SpreadMode::Repeat,
          transform,
        )?;

        let timer = self.diagnostics_enabled.then(Instant::now);
        let mut pixmap = new_pixmap(width, height)?;
        let skia_rect = SkiaRect::from_xywh(0.0, 0.0, width as f32, height as f32)?;
        let path = PathBuilder::from_rect(skia_rect);
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(
          &path,
          &paint,
          tiny_skia::FillRule::Winding,
          Transform::identity(),
          None,
        );
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::ConicGradient {
        from_angle,
        position,
        stops,
      } => {
        let resolved = normalize_color_stops_unclamped(
          stops,
          style.color,
          1.0,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let center = resolve_gradient_center(
          full_rect,
          position,
          paint_rect,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_conic_gradient(
          width,
          height,
          center,
          from_angle.to_radians(),
          SpreadMode::Pad,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(full_w.max(full_h).saturating_mul(2)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::RepeatingConicGradient {
        from_angle,
        position,
        stops,
      } => {
        let resolved = normalize_color_stops_unclamped(
          stops,
          style.color,
          1.0,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if resolved.is_empty() {
          return None;
        }
        let center = resolve_gradient_center(
          full_rect,
          position,
          paint_rect,
          style.font_size,
          style.root_font_size,
          (self.css_width, self.css_height),
        );
        let timer = self.diagnostics_enabled.then(Instant::now);
        let pixmap = rasterize_conic_gradient(
          width,
          height,
          center,
          from_angle.to_radians(),
          SpreadMode::Repeat,
          &resolved,
          &self.gradient_cache,
          gradient_bucket(full_w.max(full_h).saturating_mul(2)),
        )
        .ok()
        .flatten()?;
        if let Some(start) = timer {
          self.record_gradient_usage((width * height) as u64, start);
        }
        Some(pixmap)
      }
      BackgroundImage::Url(_) | BackgroundImage::None => None,
    }
  }

  fn paint_generated_image_intersection(
    &mut self,
    bg: &BackgroundImage,
    style: &ComputedStyle,
    tile_rect_css: Rect,
    intersection_css: Rect,
    full_w: u32,
    full_h: u32,
    clip_mask: Option<&Mask>,
    blend_mode: MixBlendMode,
    quality: FilterQuality,
  ) {
    if full_w == 0
      || full_h == 0
      || intersection_css.width() <= 0.0
      || intersection_css.height() <= 0.0
    {
      return;
    }

    let tile_rect_device_original = self.device_rect(tile_rect_css);
    let intersection_device = self.device_rect(intersection_css);
    if intersection_device.width() <= 0.0 || intersection_device.height() <= 0.0 {
      return;
    }

    let mut tile_rect_device = tile_rect_device_original;
    if quality == FilterQuality::Nearest
      && (tile_rect_device.width() > full_w as f32 || tile_rect_device.height() > full_h as f32)
    {
      let (snapped_w, offset_x) = snap_upscale(tile_rect_device.width(), full_w as f32)
        .unwrap_or_else(|| (tile_rect_device.width(), 0.0));
      let (snapped_h, offset_y) = snap_upscale(tile_rect_device.height(), full_h as f32)
        .unwrap_or_else(|| (tile_rect_device.height(), 0.0));
      tile_rect_device = Rect::from_xywh(
        tile_rect_device.x() + offset_x,
        tile_rect_device.y() + offset_y,
        snapped_w,
        snapped_h,
      );
    }

    let scale_x = tile_rect_device.width() / full_w as f32;
    let scale_y = tile_rect_device.height() / full_h as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
      return;
    }

    let src_min_x = (intersection_device.x() - tile_rect_device.x()) / scale_x;
    let src_max_x = (intersection_device.max_x() - tile_rect_device.x()) / scale_x;
    let src_min_y = (intersection_device.y() - tile_rect_device.y()) / scale_y;
    let src_max_y = (intersection_device.max_y() - tile_rect_device.y()) / scale_y;

    let pad: i64 = 1;
    let full_w_i = i64::from(full_w);
    let full_h_i = i64::from(full_h);
    let mut src_x0 = src_min_x.floor() as i64 - pad;
    let mut src_y0 = src_min_y.floor() as i64 - pad;
    let mut src_x1 = src_max_x.ceil() as i64 + pad;
    let mut src_y1 = src_max_y.ceil() as i64 + pad;
    src_x0 = src_x0.clamp(0, full_w_i.saturating_sub(1));
    src_y0 = src_y0.clamp(0, full_h_i.saturating_sub(1));
    src_x1 = src_x1.clamp(0, full_w_i);
    src_y1 = src_y1.clamp(0, full_h_i);
    if src_x1 <= src_x0 || src_y1 <= src_y0 {
      return;
    }

    let region_w = (src_x1 - src_x0) as u32;
    let region_h = (src_y1 - src_y0) as u32;
    if region_w == 0 || region_h == 0 {
      return;
    }
    if pixmap_allocation_bytes(region_w, region_h)
      .map(|bytes| bytes > MAX_PIXMAP_BYTES)
      .unwrap_or(true)
    {
      return;
    }

    let Some(pixmap) = self.render_generated_image_region(
      bg,
      style,
      full_w,
      full_h,
      src_x0 as u32,
      src_y0 as u32,
      region_w,
      region_h,
    ) else {
      return;
    };

    let translate_x = tile_rect_device.x() + (src_x0 as f32) * scale_x;
    let translate_y = tile_rect_device.y() + (src_y0 as f32) * scale_y;
    let mut paint = Paint::default();
    paint.shader = Pattern::new(
      pixmap.as_ref(),
      SpreadMode::Pad,
      quality,
      1.0,
      Transform::from_row(scale_x, 0.0, 0.0, scale_y, translate_x, translate_y),
    );
    paint.anti_alias = false;

    let Some(rect) = SkiaRect::from_xywh(
      intersection_device.x(),
      intersection_device.y(),
      intersection_device.width(),
      intersection_device.height(),
    ) else {
      return;
    };

    if is_hsl_blend(blend_mode) {
      if let Some(mut layer) = new_pixmap(self.pixmap.width(), self.pixmap.height()) {
        paint.blend_mode = SkiaBlendMode::SourceOver;
        layer.fill_rect(rect, &paint, Transform::identity(), clip_mask);
        composite_hsl_layer(
          &mut self.pixmap,
          &layer,
          1.0,
          blend_mode,
          Some(intersection_device),
        );
      } else {
        paint.blend_mode = map_blend_mode(blend_mode);
        self
          .pixmap
          .fill_rect(rect, &paint, Transform::identity(), clip_mask);
      }
    } else {
      paint.blend_mode = map_blend_mode(blend_mode);
      self
        .pixmap
        .fill_rect(rect, &paint, Transform::identity(), clip_mask);
    }
  }

  fn paint_text_decoration(
    &mut self,
    style: &ComputedStyle,
    runs: Option<&[ShapedRun]>,
    inline_start: f32,
    block_baseline: f32,
    inline_len: f32,
    inline_vertical: bool,
  ) {
    let has_any_decoration =
      !style.applied_text_decorations.is_empty() || !style.text_decoration.lines.is_empty();
    if !has_any_decoration || inline_len <= 0.0 {
      return;
    }

    let inline_start = if inline_vertical {
      self.device_y(inline_start)
    } else {
      self.device_x(inline_start)
    };
    let block_baseline = if inline_vertical {
      self.device_x(block_baseline)
    } else {
      self.device_y(block_baseline)
    };
    let inline_len = self.device_length(inline_len);

    let Some(metrics) = self.decoration_metrics(runs, style) else {
      return;
    };
    let metrics = metrics.scaled(self.scale);

    // CSS Text Decoration Level 4: spelling/grammar error decorations are UA-defined, and the UA
    // must disregard other `text-decoration-*` sub-properties and paint-affecting properties
    // (text-decoration-color/style/thickness, underline-position/offset, skip-ink, etc).
    const SPELLING_ERROR_COLOR: Rgba = Rgba::RED;
    // CSS `green` is #008000; keep the grammar underline closer to typical browser output than
    // `Rgba::GREEN` (which is #00FF00 / `lime`).
    const GRAMMAR_ERROR_COLOR: Rgba = Rgba::rgb(0, 128, 0);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum UnderlineDecorationKind {
      Underline,
      SpellingError,
      GrammarError,
    }

    impl UnderlineDecorationKind {
      fn priority(self) -> u8 {
        match self {
          Self::Underline => 0,
          Self::SpellingError => 1,
          Self::GrammarError => 2,
        }
      }
    }

    #[derive(Debug, Clone)]
    struct UnderlineDecoration {
      kind: UnderlineDecorationKind,
      style: TextDecorationStyle,
      color: Rgba,
      center: f32,
      thickness: f32,
      // `Some` for skip-ink segmented standard underlines; offsets are in device px and absolute
      // inline coordinates (start/end), matching `build_underline_segments` output.
      segments: Option<Vec<(f32, f32)>>,
    }

    let draw_solid_line =
      |pixmap: &mut Pixmap, paint: &Paint, start: f32, len: f32, center: f32, thickness: f32| {
        if thickness <= 0.0 || len <= 0.0 {
          return;
        }

        let rect = if inline_vertical {
          SkiaRect::from_xywh(center - thickness * 0.5, start, thickness, len)
        } else {
          SkiaRect::from_xywh(start, center - thickness * 0.5, len, thickness)
        };
        if let Some(rect) = rect {
          let path = PathBuilder::from_rect(rect);
          pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
          );
        }
      };

    let draw_stroked_line = |pixmap: &mut Pixmap,
                             paint: &Paint,
                             start: f32,
                             len: f32,
                             center: f32,
                             thickness: f32,
                             dash: Option<Vec<f32>>,
                             round: bool| {
      let mut path = PathBuilder::new();
      if inline_vertical {
        path.move_to(center, start);
        path.line_to(center, start + len);
      } else {
        path.move_to(start, center);
        path.line_to(start + len, center);
      }
      let Some(path) = path.finish() else { return };

      let mut stroke = Stroke::default();
      stroke.width = thickness;
      stroke.line_cap = if round {
        tiny_skia::LineCap::Round
      } else {
        tiny_skia::LineCap::Butt
      };
      if let Some(arr) = dash {
        stroke.dash = tiny_skia::StrokeDash::new(arr, 0.0);
      }

      pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    };

    let draw_wavy_line =
      |pixmap: &mut Pixmap, paint: &Paint, start: f32, len: f32, center: f32, thickness: f32| {
        if thickness <= 0.0 || len <= 0.0 {
          return;
        }
        let wavelength = (thickness * 4.0).max(6.0);
        let amplitude = (thickness * 0.75).max(thickness * 0.5);

        let mut path = PathBuilder::new();
        if inline_vertical {
          path.move_to(center, start);
        } else {
          path.move_to(start, center);
        }
        let mut cursor = start;
        let mut up = true;
        while cursor < start + len {
          let end = (cursor + wavelength).min(start + len);
          let mid = cursor + (end - cursor) * 0.5;
          if inline_vertical {
            let control_x = if up {
              center - amplitude
            } else {
              center + amplitude
            };
            path.quad_to(control_x, mid, center, end);
          } else {
            let control_y = if up {
              center - amplitude
            } else {
              center + amplitude
            };
            path.quad_to(mid, control_y, end, center);
          }
          cursor = end;
          up = !up;
        }

        if let Some(path) = path.finish() {
          let mut stroke = Stroke::default();
          stroke.width = thickness.max(0.5);
          stroke.line_cap = tiny_skia::LineCap::Round;
          pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
      };

    let stroke_half_extent = |style: TextDecorationStyle, thickness: f32| -> f32 {
      match style {
        TextDecorationStyle::Double => thickness * 2.5,
        TextDecorationStyle::Wavy => thickness * 2.0,
        _ => thickness * 0.5,
      }
    };

    let decorations = if !style.applied_text_decorations.is_empty() {
      Cow::Borrowed(style.applied_text_decorations.as_slice())
    } else {
      Cow::Owned(vec![crate::style::types::ResolvedTextDecoration {
        origin_id: style as *const ComputedStyle as usize,
        decoration: style.text_decoration.clone(),
        skip_ink: style.text_decoration_skip_ink,
        underline_offset: style.text_underline_offset,
        underline_position: style.text_underline_position,
        inset: style.text_decoration_inset,
      }])
    };

    for deco in decorations.iter() {
      let lines = deco.decoration.lines;

      let decoration_color = deco.decoration.color.unwrap_or(style.color);
      let standard_visible = decoration_color.alpha_u8() != 0;

      let used_thickness =
        self.resolve_decoration_thickness_value(deco.decoration.thickness, style);

      let render_line = |pixmap: &mut Pixmap,
                         paint: &Paint,
                         deco_style: TextDecorationStyle,
                         start: f32,
                         len: f32,
                         center: f32,
                         thickness: f32| match deco_style
      {
        TextDecorationStyle::Solid => {
          draw_solid_line(pixmap, paint, start, len, center, thickness);
        }
        TextDecorationStyle::Double => {
          let line_thickness = (thickness * 0.7).max(0.5);
          let gap = line_thickness.max(thickness * 0.6);
          draw_solid_line(
            pixmap,
            paint,
            start,
            len,
            center - (gap * 0.5),
            line_thickness,
          );
          draw_solid_line(
            pixmap,
            paint,
            start,
            len,
            center + (gap * 0.5),
            line_thickness,
          );
        }
        TextDecorationStyle::Dotted => {
          draw_stroked_line(
            pixmap,
            paint,
            start,
            len,
            center,
            thickness,
            Some(vec![thickness, thickness]),
            true,
          );
        }
        TextDecorationStyle::Dashed => {
          draw_stroked_line(
            pixmap,
            paint,
            start,
            len,
            center,
            thickness,
            Some(vec![3.0 * thickness, thickness]),
            false,
          );
        }
        TextDecorationStyle::Wavy => {
          draw_wavy_line(pixmap, paint, start, len, center, thickness);
        }
      };

      let mut underline_like: Vec<UnderlineDecoration> = Vec::new();

      if standard_visible && lines.contains(TextDecorationLine::UNDERLINE) {
        let thickness = used_thickness.unwrap_or(metrics.underline_thickness);
        let center = self.underline_center(
          &metrics,
          deco.underline_position,
          deco.underline_offset,
          thickness,
          block_baseline,
          inline_vertical,
          style,
        );
        let segments = if matches!(
          deco.skip_ink,
          crate::style::types::TextDecorationSkipInk::Auto
            | crate::style::types::TextDecorationSkipInk::All
        ) {
          runs.map(|runs| {
            let viewport = (self.css_width, self.css_height);
            let stroke_width = runs
              .iter()
              .map(|run| {
                style
                  .webkit_text_stroke_width
                  .resolve_with_context(
                    None,
                    viewport.0,
                    viewport.1,
                    run.font_size,
                    style.root_font_size,
                  )
                  .unwrap_or(0.0)
              })
              .map(|w| if w.is_finite() { w.max(0.0) } else { 0.0 })
              .fold(0.0_f32, f32::max)
              * self.scale;
            self.build_underline_segments(
              runs,
              inline_start,
              inline_len,
              center,
              thickness,
              block_baseline,
              inline_vertical,
              deco.skip_ink,
              stroke_width,
            )
          })
        } else {
          None
        };
        underline_like.push(UnderlineDecoration {
          kind: UnderlineDecorationKind::Underline,
          style: deco.decoration.style,
          color: decoration_color,
          center,
          thickness,
          segments,
        });
      }

      if lines.contains(TextDecorationLine::SPELLING_ERROR) {
        let thickness = metrics.underline_thickness;
        let center = self.underline_center(
          &metrics,
          crate::style::types::TextUnderlinePosition::Auto,
          crate::style::types::TextUnderlineOffset::Auto,
          thickness,
          block_baseline,
          inline_vertical,
          style,
        );
        underline_like.push(UnderlineDecoration {
          kind: UnderlineDecorationKind::SpellingError,
          style: TextDecorationStyle::Wavy,
          color: SPELLING_ERROR_COLOR,
          center,
          thickness,
          segments: None,
        });
      }

      if lines.contains(TextDecorationLine::GRAMMAR_ERROR) {
        let thickness = metrics.underline_thickness;
        let center = self.underline_center(
          &metrics,
          crate::style::types::TextUnderlinePosition::Auto,
          crate::style::types::TextUnderlineOffset::Auto,
          thickness,
          block_baseline,
          inline_vertical,
          style,
        );
        underline_like.push(UnderlineDecoration {
          kind: UnderlineDecorationKind::GrammarError,
          style: TextDecorationStyle::Wavy,
          color: GRAMMAR_ERROR_COLOR,
          center,
          thickness,
          segments: None,
        });
      }

      // If multiple underline-like decorations are present on the same text run (e.g.
      // `underline spelling-error`), bump the later decorations outwards so all strokes remain
      // visible.
      if underline_like.len() > 1 {
        let baseline = block_baseline;
        let mut neg: Vec<usize> = Vec::new();
        let mut pos: Vec<usize> = Vec::new();
        for idx in 0..underline_like.len() {
          let delta = underline_like[idx].center - baseline;
          let sign = if delta.abs() <= 1e-3 { 1.0 } else { delta.signum() };
          if sign < 0.0 {
            neg.push(idx);
          } else {
            pos.push(idx);
          }
        }

        let mut adjust_group = |indices: &mut Vec<usize>, sign: f32| {
          if indices.len() <= 1 {
            return;
          }
          indices.sort_by(|&a, &b| {
            let da = (underline_like[a].center - baseline).abs();
            let db = (underline_like[b].center - baseline).abs();
            da.partial_cmp(&db)
              .unwrap_or(std::cmp::Ordering::Equal)
              .then_with(|| underline_like[a].kind.priority().cmp(&underline_like[b].kind.priority()))
          });

          let mut prev_pos: Option<f32> = None;
          let mut prev_half: f32 = 0.0;
          let mut prev_thickness: f32 = 0.0;
          for &idx in indices.iter() {
            let delta = underline_like[idx].center - baseline;
            let pos = delta.abs();
            let thickness = underline_like[idx].thickness;
            let half_extent = stroke_half_extent(underline_like[idx].style, thickness);
            let adjusted = match prev_pos {
              Some(prev) => {
                let gap = prev_thickness.max(thickness) * 0.5;
                let min_pos = prev + prev_half + half_extent + gap;
                pos.max(min_pos)
              }
              None => pos,
            };
            underline_like[idx].center = baseline + sign * adjusted;
            prev_pos = Some(adjusted);
            prev_half = half_extent;
            prev_thickness = thickness;
          }
        };

        adjust_group(&mut pos, 1.0);
        adjust_group(&mut neg, -1.0);
      }

      // Paint standard decorations (respecting authored style/thickness/etc). Keep legacy ordering
      // (underline -> overline -> line-through).
      if standard_visible {
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(color_to_skia(decoration_color));

        let painter_style = deco.decoration.style;

        if let Some(underline) = underline_like
          .iter()
          .find(|u| u.kind == UnderlineDecorationKind::Underline)
        {
          if let Some(segments) = &underline.segments {
            for &(seg_start, seg_end) in segments {
              let seg_width = seg_end - seg_start;
              if seg_width <= 0.0 {
                continue;
              }
              render_line(
                &mut self.pixmap,
                &paint,
                painter_style,
                seg_start,
                seg_width,
                underline.center,
                underline.thickness,
              );
            }
          } else {
            render_line(
              &mut self.pixmap,
              &paint,
              painter_style,
              inline_start,
              inline_len,
              underline.center,
              underline.thickness,
            );
          }
        }

        if lines.contains(TextDecorationLine::OVERLINE) {
          render_line(
            &mut self.pixmap,
            &paint,
            painter_style,
            inline_start,
            inline_len,
            block_baseline - metrics.ascent,
            used_thickness.unwrap_or(metrics.underline_thickness),
          );
        }
        if lines.contains(TextDecorationLine::LINE_THROUGH) {
          render_line(
            &mut self.pixmap,
            &paint,
            painter_style,
            inline_start,
            inline_len,
            block_baseline - metrics.strike_pos,
            used_thickness.unwrap_or(metrics.strike_thickness),
          );
        }
      }

      // Paint spelling/grammar error decorations as additional underline strokes. They must ignore
      // authored text-decoration paint-affecting properties (color/style/thickness/skip-ink, etc),
      // so treat them as separate paints.
      for underline in &underline_like {
        let kind = underline.kind;
        if !matches!(
          kind,
          UnderlineDecorationKind::SpellingError | UnderlineDecorationKind::GrammarError
        ) {
          continue;
        }
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(color_to_skia(underline.color));
        render_line(
          &mut self.pixmap,
          &paint,
          underline.style,
          inline_start,
          inline_len,
          underline.center,
          underline.thickness,
        );
      }
    }
  }

  fn build_underline_segments(
    &self,
    runs: &[ShapedRun],
    line_start: f32,
    line_width: f32,
    center: f32,
    thickness: f32,
    baseline_y: f32,
    inline_vertical: bool,
    skip_ink: TextDecorationSkipInk,
    stroke_width_px: f32,
  ) -> Vec<(f32, f32)> {
    if line_width <= 0.0 {
      return Vec::new();
    }

    let band_half = (thickness * 0.5).abs();
    let extra_pad = if stroke_width_px.is_finite() {
      (stroke_width_px * 0.5).max(0.0)
    } else {
      0.0
    };
    let mut exclusions = if inline_vertical {
      let band_left = center - band_half;
      let band_right = center + band_half;
      collect_underline_exclusions_vertical(
        runs,
        line_start,
        baseline_y,
        band_left,
        band_right,
        skip_ink == TextDecorationSkipInk::All,
        self.scale,
        extra_pad,
      )
    } else {
      let band_top = center - band_half;
      let band_bottom = center + band_half;
      collect_underline_exclusions(
        runs,
        line_start,
        baseline_y,
        band_top,
        band_bottom,
        skip_ink == TextDecorationSkipInk::All,
        self.scale,
        extra_pad,
      )
    };

    let mut segments = subtract_intervals(
      (line_start, line_start + line_width),
      exclusions.as_mut_slice(),
    );
    if segments.is_empty() && skip_ink != TextDecorationSkipInk::All {
      // Never drop the underline entirely when skipping ink; fall back to a full span.
      segments.push((line_start, line_start + line_width));
    }
    segments
  }

  fn paint_text_emphasis(
    &mut self,
    style: &ComputedStyle,
    runs: Option<&[ShapedRun]>,
    inline_origin: f32,
    block_baseline: f32,
    inline_vertical: bool,
  ) {
    let runs = match runs {
      Some(r) => r,
      None => return,
    };
    if style.text_emphasis_style.is_none() {
      return;
    }

    let inline_origin = if inline_vertical {
      self.device_y(inline_origin)
    } else {
      self.device_x(inline_origin)
    };
    let block_baseline = if inline_vertical {
      self.device_x(block_baseline)
    } else {
      self.device_y(block_baseline)
    };

    let Some(metrics) = self.decoration_metrics(Some(runs), style) else {
      return;
    };
    let metrics = metrics.scaled(self.scale);

    let resolved_position = match style.text_emphasis_position {
      crate::style::types::TextEmphasisPosition::Auto => {
        crate::style::types::TextEmphasisPosition::Over
      }
      other => other,
    };
    let emphasis_color = style.text_emphasis_color.unwrap_or(style.color);
    let mark_size = (style.font_size * 0.5 * self.scale).max(1.0);
    let gap = mark_size * 0.3;

    let block_center = if inline_vertical {
      let offset = gap + mark_size * 0.5;
      if crate::style::is_vertical_typographic_mode(style.writing_mode) {
        // In vertical typographic modes, emphasis placement is controlled by `right`/`left`.
        let mark_on_left = matches!(
          resolved_position,
          crate::style::types::TextEmphasisPosition::OverLeft
            | crate::style::types::TextEmphasisPosition::UnderLeft
        );
        if mark_on_left {
          block_baseline - offset
        } else {
          block_baseline + offset
        }
      } else {
        match resolved_position {
          crate::style::types::TextEmphasisPosition::Over
          | crate::style::types::TextEmphasisPosition::OverLeft
          | crate::style::types::TextEmphasisPosition::OverRight => block_baseline + offset,
          crate::style::types::TextEmphasisPosition::Under
          | crate::style::types::TextEmphasisPosition::UnderLeft
          | crate::style::types::TextEmphasisPosition::UnderRight => block_baseline - offset,
          crate::style::types::TextEmphasisPosition::Auto => block_baseline + offset,
        }
      }
    } else {
      match resolved_position {
        crate::style::types::TextEmphasisPosition::Over
        | crate::style::types::TextEmphasisPosition::OverLeft
        | crate::style::types::TextEmphasisPosition::OverRight => {
          block_baseline - metrics.ascent - gap - mark_size * 0.5
        }
        crate::style::types::TextEmphasisPosition::Under
        | crate::style::types::TextEmphasisPosition::UnderLeft
        | crate::style::types::TextEmphasisPosition::UnderRight => {
          block_baseline + metrics.descent + gap + mark_size * 0.5
        }
        crate::style::types::TextEmphasisPosition::Auto => {
          block_baseline - metrics.ascent - gap - mark_size * 0.5
        }
      }
    };

    // Precompute mark painter for string emphasis.
    let mut string_mark: Option<(Vec<tiny_skia::Path>, f32, f32)> = None;
    if let crate::style::types::TextEmphasisStyle::String(ref s) = style.text_emphasis_style {
      use unicode_segmentation::UnicodeSegmentation;
      let mark_str = s.graphemes(true).next().unwrap_or("");
      if !mark_str.is_empty() {
        let mut mark_style = style.clone();
        mark_style.font_size = style.font_size * 0.5;
        mark_style.font_variant_east_asian.ruby = true;
        if crate::style::is_vertical_typographic_mode(style.writing_mode) {
          // CSS Text Decoration 4: emphasis marks must remain upright in vertical typographic modes,
          // regardless of the element's authored `text-orientation`.
          mark_style.text_orientation = crate::style::types::TextOrientation::Upright;
        }
        let Ok(mark_runs) = self.shaper.shape_arc(mark_str, &mark_style, &self.font_ctx) else {
          return;
        };
        let mark_width: f32 = mark_runs.iter().map(|r| r.advance * self.scale).sum();
        let mark_metrics =
          crate::layout::contexts::inline::line_builder::TextItem::metrics_from_runs(
            &self.font_ctx,
            mark_runs.as_ref(),
            mark_style.font_size,
            mark_style.font_size,
          );
        let mut paths = Vec::new();
        let mut run_pen_inline = 0.0;
        for run in mark_runs.iter() {
          let advance = run.advance * self.scale;
          let run_origin = run_pen_inline;
          let Some(instance) = FontInstance::new(&run.font, &run.variations) else {
            continue;
          };
          let units_per_em = instance.units_per_em();
          if units_per_em == 0.0 {
            continue;
          }
          let scale = (run.font_size / units_per_em) * run.scale * self.scale;
          let mut pen_inline = 0.0_f32;
          let mut pen_block = 0.0_f32;
          for glyph in &run.glyphs {
            let glyph_x = run_origin + pen_inline + glyph.x_offset * self.scale;
            let glyph_y =
              mark_metrics.baseline_offset * self.scale + pen_block - glyph.y_offset * self.scale;
            if let Some(path) = Self::build_glyph_path(
              &instance,
              glyph.glyph_id,
              glyph_x,
              glyph_y,
              scale,
              run.synthetic_oblique,
            ) {
              paths.push(path);
            }
            pen_inline += glyph.x_advance * self.scale;
            pen_block += glyph.y_advance * self.scale;
          }
          run_pen_inline += advance;
        }
        if !paths.is_empty() {
          string_mark = Some((
            paths,
            mark_width,
            (mark_metrics.ascent + mark_metrics.descent) * self.scale,
          ));
        }
      }
    }

    let mut pen_inline = inline_origin;
    for run in runs {
      let advance = run.advance * self.scale;
      let run_origin_inline = pen_inline;

      use unicode_segmentation::UnicodeSegmentation;

      // `text-emphasis` marks are drawn once per typographic character unit (extended grapheme
      // cluster). HarfBuzz cluster values can span multiple grapheme clusters (e.g. ligatures), so
      // distribute each HarfBuzz cluster's advance across the graphemes it covers.
      let graphemes: Vec<(usize, &str)> = run.text.grapheme_indices(true).collect();
      let mut cluster_starts: Vec<usize> = run.glyphs.iter().map(|g| g.cluster as usize).collect();
      cluster_starts.sort_unstable();
      cluster_starts.dedup();
      let mut cluster_end_for: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
      for (idx, start) in cluster_starts.iter().enumerate() {
        let end = cluster_starts
          .get(idx + 1)
          .copied()
          .unwrap_or_else(|| run.text.len());
        cluster_end_for.insert(*start, end);
      }

      let mut cursor_inline = run_origin_inline;
      let mut glyph_idx = 0;
      while glyph_idx < run.glyphs.len() {
        let cluster_start = run.glyphs[glyph_idx].cluster as usize;
        let cluster_end = cluster_end_for
          .get(&cluster_start)
          .copied()
          .unwrap_or_else(|| run.text.len());

        let first_glyph = &run.glyphs[glyph_idx];
        let cluster_offset = if inline_vertical {
          -first_glyph.y_offset
        } else {
          first_glyph.x_offset
        };

        let mut cluster_advance = 0.0_f32;
        while glyph_idx < run.glyphs.len()
          && run.glyphs[glyph_idx].cluster as usize == cluster_start
        {
          let glyph = &run.glyphs[glyph_idx];
          let inline_advance = if inline_vertical {
            if glyph.y_advance.abs() > f32::EPSILON {
              glyph.y_advance
            } else {
              glyph.x_advance
            }
          } else {
            glyph.x_advance
          };
          cluster_advance += inline_advance;
          glyph_idx += 1;
        }

        let start_idx = graphemes.partition_point(|(start, _)| *start < cluster_start);
        let end_idx = graphemes.partition_point(|(start, _)| *start < cluster_end);
        let count = end_idx.saturating_sub(start_idx);
        if count == 0 {
          cursor_inline += cluster_advance * self.scale;
          continue;
        }

        let per_unit_advance = cluster_advance / (count as f32);
        for (idx, (_, grapheme)) in graphemes[start_idx..end_idx].iter().enumerate() {
          let inline_center = cursor_inline
            + cluster_offset * self.scale
            + (idx as f32 + 0.5) * per_unit_advance * self.scale;

          let mut chars = grapheme.chars();
          if let Some(ch) = chars.next() {
            let mut skip_mark =
              crate::style::should_skip_text_emphasis_mark(ch, style.text_emphasis_skip);
            if skip_mark
              && !style
                .text_emphasis_skip
                .contains(crate::style::types::TextEmphasisSkip::NARROW)
              && ch == ' '
              && chars.next().is_some_and(crate::style::is_combining_mark)
            {
              skip_mark = false;
            }
            if skip_mark {
              continue;
            }
          }

          let block_center_with_pen = block_center;
          let (mark_center_x, mark_center_y) = if inline_vertical {
            (block_center_with_pen, inline_center)
          } else {
            (inline_center, block_center_with_pen)
          };

          match style.text_emphasis_style {
            crate::style::types::TextEmphasisStyle::Mark { fill, shape } => {
              let default_shape = if crate::style::is_vertical_typographic_mode(style.writing_mode)
              {
                crate::style::types::TextEmphasisShape::Sesame
              } else {
                crate::style::types::TextEmphasisShape::Circle
              };
              let shape = shape.unwrap_or(default_shape);
              self.draw_emphasis_mark(
                mark_center_x,
                mark_center_y,
                mark_size,
                fill,
                shape,
                emphasis_color,
                resolved_position,
                inline_vertical,
              );
            }
            crate::style::types::TextEmphasisStyle::String(_) => {
              if let Some((ref paths, width, height)) = string_mark {
                let mut paint = Paint::default();
                paint.anti_alias = true;
                paint.set_color(color_to_skia(emphasis_color));

                let (draw_w, draw_h) = if inline_vertical {
                  (height, width)
                } else {
                  (width, height)
                };
                let offset_x = mark_center_x - draw_w * 0.5;
                let offset_y = mark_center_y - draw_h * 0.5;
                for path in paths {
                  let translated = path
                    .clone()
                    .transform(Transform::from_translate(offset_x, offset_y));
                  if let Some(p) = translated {
                    self.pixmap.fill_path(
                      &p,
                      &paint,
                      tiny_skia::FillRule::EvenOdd,
                      Transform::identity(),
                      None,
                    );
                  }
                }
              }
            }
            crate::style::types::TextEmphasisStyle::None => {}
          }
        }

        cursor_inline += cluster_advance * self.scale;
      }

      pen_inline += advance;
    }
  }

  fn draw_emphasis_mark(
    &mut self,
    center_x: f32,
    center_y: f32,
    size: f32,
    fill: crate::style::types::TextEmphasisFill,
    shape: crate::style::types::TextEmphasisShape,
    color: Rgba,
    _position: crate::style::types::TextEmphasisPosition,
    _inline_vertical: bool,
  ) {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(color_to_skia(color));

    match shape {
      crate::style::types::TextEmphasisShape::Dot => {
        let radius = size * 0.5;
        if let Some(path) = PathBuilder::from_circle(center_x, center_y, radius) {
          match fill {
            crate::style::types::TextEmphasisFill::Filled => self.pixmap.fill_path(
              &path,
              &paint,
              tiny_skia::FillRule::EvenOdd,
              Transform::identity(),
              None,
            ),
            crate::style::types::TextEmphasisFill::Open => {
              let mut stroke = Stroke::default();
              stroke.width = (size * 0.18).max(0.5);
              self
                .pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
          }
        }
      }
      crate::style::types::TextEmphasisShape::Circle => {
        let radius = size * 0.5;
        if let Some(path) = PathBuilder::from_circle(center_x, center_y, radius) {
          match fill {
            crate::style::types::TextEmphasisFill::Filled => self.pixmap.fill_path(
              &path,
              &paint,
              tiny_skia::FillRule::EvenOdd,
              Transform::identity(),
              None,
            ),
            crate::style::types::TextEmphasisFill::Open => {
              let mut stroke = Stroke::default();
              stroke.width = (size * 0.18).max(0.5);
              self
                .pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
          }
        }
      }
      crate::style::types::TextEmphasisShape::DoubleCircle => {
        let mut stroke = Stroke::default();
        stroke.width = (size * 0.14).max(0.5);
        let radii = [size * 0.5, size * 0.33];
        for radius in radii {
          if let Some(path) = PathBuilder::from_circle(center_x, center_y, radius) {
            self
              .pixmap
              .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
          }
        }
      }
      crate::style::types::TextEmphasisShape::Triangle => {
        let half = size * 0.5;
        let height = size * 0.9;
        let mut builder = PathBuilder::new();
        let apex_y = center_y - height * 0.5;
        let base_y = center_y + height * 0.5;
        builder.move_to(center_x, apex_y);
        builder.line_to(center_x - half, base_y);
        builder.line_to(center_x + half, base_y);
        builder.close();
        if let Some(path) = builder.finish() {
          match fill {
            crate::style::types::TextEmphasisFill::Filled => {
              self.pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::EvenOdd,
                Transform::identity(),
                None,
              );
            }
            crate::style::types::TextEmphasisFill::Open => {
              let mut stroke = Stroke::default();
              stroke.width = (size * 0.18).max(0.5);
              self
                .pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
          }
        }
      }
      crate::style::types::TextEmphasisShape::Sesame => {
        let len = size * 0.75;
        let angle = 20.0_f32.to_radians();
        let dx = (angle.cos() * len * 0.5, angle.sin() * len * 0.5);
        let mut builder = PathBuilder::new();
        builder.move_to(center_x - dx.0, center_y - dx.1);
        builder.line_to(center_x + dx.0, center_y + dx.1);
        if let Some(path) = builder.finish() {
          let mut stroke = Stroke::default();
          stroke.width = (size * 0.2).max(0.6);
          stroke.line_cap = tiny_skia::LineCap::Round;
          self
            .pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
      }
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct DecorationMetrics {
  underline_pos: f32,
  underline_thickness: f32,
  strike_pos: f32,
  strike_thickness: f32,
  ascent: f32,
  descent: f32,
  has_font_underline_metrics: bool,
}

impl DecorationMetrics {
  fn scaled(self, scale: f32) -> Self {
    Self {
      underline_pos: self.underline_pos * scale,
      underline_thickness: self.underline_thickness * scale,
      strike_pos: self.strike_pos * scale,
      strike_thickness: self.strike_thickness * scale,
      ascent: self.ascent * scale,
      descent: self.descent * scale,
      has_font_underline_metrics: self.has_font_underline_metrics,
    }
  }
}

fn collect_underline_exclusions(
  runs: &[ShapedRun],
  line_start: f32,
  baseline_y: f32,
  band_top: f32,
  band_bottom: f32,
  skip_all: bool,
  device_scale: f32,
  extra_pad_px: f32,
) -> Vec<(f32, f32)> {
  crate::paint::text_decoration_skip_ink::collect_underline_exclusions(
    runs,
    line_start,
    baseline_y,
    band_top,
    band_bottom,
    skip_all,
    device_scale,
    extra_pad_px,
  )
}

fn collect_underline_exclusions_vertical(
  runs: &[ShapedRun],
  inline_start: f32,
  block_baseline: f32,
  band_left: f32,
  band_right: f32,
  skip_all: bool,
  device_scale: f32,
  extra_pad_px: f32,
) -> Vec<(f32, f32)> {
  crate::paint::text_decoration_skip_ink::collect_underline_exclusions_vertical(
    runs,
    inline_start,
    block_baseline,
    band_left,
    band_right,
    skip_all,
    device_scale,
    extra_pad_px,
  )
}

fn subtract_intervals(total: (f32, f32), exclusions: &mut [(f32, f32)]) -> Vec<(f32, f32)> {
  exclusions.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
  let mut start = total.0;
  let mut allowed = Vec::new();

  for &(ex_start, ex_end) in exclusions.iter() {
    if ex_end <= start {
      continue;
    }
    if ex_start > total.1 {
      break;
    }
    let seg_end = ex_start.min(total.1);
    if seg_end > start {
      allowed.push((start, seg_end));
    }
    start = ex_end.max(start);
    if start >= total.1 {
      break;
    }
  }

  if start < total.1 {
    allowed.push((start, total.1));
  }

  allowed
}

fn color_to_skia(color: Rgba) -> tiny_skia::Color {
  let alpha = (color.a * 255.0).clamp(0.0, 255.0).round() as u8;
  tiny_skia::Color::from_rgba8(color.r, color.g, color.b, alpha)
}

fn build_transform_3d(
  style: Option<&ComputedStyle>,
  bounds: Rect,
  viewport: Option<(f32, f32)>,
) -> Option<Transform3D> {
  style.and_then(|style| DisplayListBuilder::debug_resolve_transform(style, bounds, viewport))
}

fn transform2d_to_skia(transform: Transform2D) -> Transform {
  Transform::from_row(
    transform.a,
    transform.b,
    transform.c,
    transform.d,
    transform.e,
    transform.f,
  )
}

fn transform_rect(rect: Rect, ts: &Transform) -> Rect {
  let corners = [
    (rect.min_x(), rect.min_y()),
    (rect.max_x(), rect.min_y()),
    (rect.max_x(), rect.max_y()),
    (rect.min_x(), rect.max_y()),
  ];
  let mut min_x = f32::INFINITY;
  let mut min_y = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  let mut max_y = f32::NEG_INFINITY;

  for (x, y) in corners {
    let tx = x * ts.sx + y * ts.kx + ts.tx;
    let ty = x * ts.ky + y * ts.sy + ts.ty;
    min_x = min_x.min(tx);
    min_y = min_y.min(ty);
    max_x = max_x.max(tx);
    max_y = max_y.max(ty);
  }

  Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y)
}

fn approx_same_rect(a: Rect, b: Rect) -> bool {
  let eps = 0.001;
  (a.min_x() - b.min_x()).abs() <= eps
    && (a.min_y() - b.min_y()).abs() <= eps
    && (a.width() - b.width()).abs() <= eps
    && (a.height() - b.height()).abs() <= eps
}

#[cfg(test)]
const MAX_IMPORTED_CSS_BYTES: usize = 2 * 1024 * 1024;

#[cfg(test)]
fn decode_data_url_to_string(data_url: &str) -> Result<String> {
  let decoded = crate::resource::data_url::decode_data_url_prefix(
    data_url,
    MAX_IMPORTED_CSS_BYTES.saturating_add(1),
  )
  .map(|resource| resource.bytes)
  .map_err(|err| RenderError::InvalidParameters {
    message: format!("Invalid data URL: {err}"),
  })?;

  if decoded.len() > MAX_IMPORTED_CSS_BYTES {
    return Err(
      RenderError::InvalidParameters {
        message: format!(
          "Inline @import stylesheet exceeds {} bytes",
          MAX_IMPORTED_CSS_BYTES
        ),
      }
      .into(),
    );
  }

  if let Some((enc, bom_len)) = Encoding::for_bom(&decoded) {
    return Ok(
      enc
        .decode_without_bom_handling(&decoded[bom_len..])
        .0
        .into_owned(),
    );
  }

  Ok(String::from_utf8_lossy(&decoded).into_owned())
}

#[cfg(test)]
struct EmbeddedImportFetcher {
  base_url: Option<String>,
  fetcher: Arc<dyn crate::resource::ResourceFetcher>,
  referrer_policy: crate::resource::ReferrerPolicy,
  imported_stylesheet_policies: RefCell<HashMap<String, crate::resource::ReferrerPolicy>>,
}

#[cfg(test)]
impl EmbeddedImportFetcher {
  fn new(
    base_url: Option<String>,
    fetcher: Arc<dyn crate::resource::ResourceFetcher>,
    referrer_policy: crate::resource::ReferrerPolicy,
  ) -> Self {
    Self {
      base_url,
      fetcher,
      referrer_policy,
      imported_stylesheet_policies: RefCell::new(HashMap::new()),
    }
  }

  fn resolve_url(&self, href: &str) -> Option<Url> {
    if crate::resource::is_data_url(href) {
      return None;
    }

    if let Ok(abs) = Url::parse(href) {
      return Some(abs);
    }

    let base = self.base_url.as_ref()?;
    let mut base_candidate = base.clone();
    if base_candidate.starts_with("file://") {
      let path = &base_candidate["file://".len()..];
      if std::path::Path::new(path).is_dir() && !base_candidate.ends_with('/') {
        base_candidate.push('/');
      }
    }

    Url::parse(&base_candidate)
      .or_else(|_| {
        Url::from_file_path(&base_candidate).map_err(|()| url::ParseError::RelativeUrlWithoutBase)
      })
      .ok()
      .and_then(|base_url| base_url.join(href).ok())
  }

  fn referrer_policy_for_importer(
    &self,
    importer_url: Option<&str>,
  ) -> crate::resource::ReferrerPolicy {
    if let Some(importer_url) = importer_url {
      if let Some(policy) = self
        .imported_stylesheet_policies
        .borrow()
        .get(importer_url)
        .copied()
      {
        return policy;
      }
    }

    self.referrer_policy
  }
}

#[cfg(test)]
impl css::types::CssImportLoader for EmbeddedImportFetcher {
  fn load(&self, url: &str) -> Result<String> {
    Ok(self.load_with_importer(url, self.base_url.as_deref())?.css)
  }

  fn referrer_policy_for_stylesheet(&self, url: &str) -> Option<crate::resource::ReferrerPolicy> {
    self.imported_stylesheet_policies.borrow().get(url).copied()
  }

  fn load_with_importer(
    &self,
    url: &str,
    importer_url: Option<&str>,
  ) -> Result<crate::css::loader::FetchedStylesheet> {
    if crate::resource::is_data_url(url) {
      return Ok(crate::css::loader::FetchedStylesheet::new(
        decode_data_url_to_string(url)?,
        None,
      ));
    }

    let resolved = self
      .resolve_url(url)
      .or_else(|| Url::parse(url).ok())
      .ok_or_else(|| RenderError::InvalidParameters {
        message: format!("Cannot resolve @import URL '{}'", url),
      })?;

    match resolved.scheme() {
      "file" | "http" | "https" => {}
      _ => {
        return Err(
          RenderError::InvalidParameters {
            message: format!("Unsupported URL scheme for @import: {}", resolved),
          }
          .into(),
        );
      }
    };

    let referrer_policy = self.referrer_policy_for_importer(importer_url);
    let mut req =
      crate::resource::FetchRequest::new(resolved.as_str(), crate::resource::FetchDestination::Style)
        .with_referrer_policy(referrer_policy);
    if let Some(importer_url) = importer_url {
      req = req.with_referrer_url(importer_url);
    }

    let resource = self.fetcher.fetch_partial_with_request(
      req,
      MAX_IMPORTED_CSS_BYTES.saturating_add(1),
    )?;
    if resource.bytes.len() > MAX_IMPORTED_CSS_BYTES {
      return Err(
        RenderError::InvalidParameters {
          message: format!(
            "@import stylesheet exceeds {} bytes: {}",
            MAX_IMPORTED_CSS_BYTES, resolved
          ),
        }
        .into(),
      );
    }

    let decoded = css::encoding::decode_css_bytes(&resource.bytes, resource.content_type.as_deref());
    let sheet_base = resource
      .final_url
      .clone()
      .unwrap_or_else(|| resolved.to_string());

    let effective_policy = resource.response_referrer_policy.unwrap_or(referrer_policy);
    {
      let mut policies = self.imported_stylesheet_policies.borrow_mut();
      policies.insert(sheet_base.clone(), effective_policy);
      if sheet_base != resolved.as_str() {
        policies.insert(resolved.to_string(), effective_policy);
      }
    }

    let rewritten = match css::loader::absolutize_css_urls_cow(&decoded, &sheet_base)? {
      Cow::Borrowed(_) => decoded,
      Cow::Owned(rewritten) => rewritten,
    };

    Ok(crate::css::loader::FetchedStylesheet::new(
      rewritten,
      resource.final_url.clone(),
    ))
  }
}

fn clip_rect_axes(mut bounds: Rect, clip: Rect, clip_x: bool, clip_y: bool) -> Rect {
  if clip_x {
    let min_x = bounds.min_x().max(clip.min_x());
    let max_x = bounds.max_x().min(clip.max_x());
    bounds.origin.x = min_x;
    bounds.size.width = (max_x - min_x).max(0.0);
  }
  if clip_y {
    let min_y = bounds.min_y().max(clip.min_y());
    let max_y = bounds.max_y().min(clip.max_y());
    bounds.origin.y = min_y;
    bounds.size.height = (max_y - min_y).max(0.0);
  }
  bounds
}

fn compute_descendant_bounds(
  commands: &[DisplayCommand],
  root_rect: Rect,
  viewport: (f32, f32),
  clip_root: bool,
) -> Option<Rect> {
  let mut current: Option<Rect> = None;
  for cmd in commands {
    if !clip_root
      && matches!(
        cmd,
        DisplayCommand::Background { rect, .. } | DisplayCommand::Border { rect, .. }
          if approx_same_rect(*rect, root_rect)
      )
    {
      continue;
    }
    if let Some(r) = command_bounds(cmd, viewport) {
      current = Some(match current {
        Some(acc) => acc.union(r),
        None => r,
      });
    }
  }
  current
}

fn compute_outline_bounds(commands: &[DisplayCommand]) -> Option<Rect> {
  let mut current: Option<Rect> = None;

  for cmd in commands {
    match cmd {
      DisplayCommand::Outline { rect, .. } => {
        current = Some(current.map(|acc| acc.union(*rect)).unwrap_or(*rect));
      }
      DisplayCommand::StackingContext { commands, .. } => {
        if let Some(r) = compute_outline_bounds(commands) {
          current = Some(current.map(|acc| acc.union(r)).unwrap_or(r));
        }
      }
      _ => {}
    }
  }

  current
}

fn root_border_image_bounds(
  commands: &[DisplayCommand],
  root_rect: Rect,
  viewport: (f32, f32),
) -> Option<Rect> {
  for cmd in commands {
    let DisplayCommand::Border { rect, style, .. } = cmd else {
      continue;
    };
    if !approx_same_rect(*rect, root_rect) {
      continue;
    }
    return crate::paint::paint_bounds::border_image_paint_bounds(*rect, style, Some(viewport));
  }
  None
}

fn stacking_context_bounds(
  commands: &[DisplayCommand],
  filters: &[ResolvedFilter],
  backdrop_filters: &[ResolvedFilter],
  rect: Rect,
  transform: Option<&Transform3D>,
  clip: Option<&StackingClip>,
  clip_path: Option<&StackingClipPath>,
  viewport: (f32, f32),
) -> Option<Rect> {
  let project_rect_bounds = |rect: Rect, transform: &Transform3D| -> Option<Rect> {
    let corners = rect_corners(rect);
    let mut projected = [Point::ZERO; 4];
    for (idx, corner) in corners.iter().enumerate() {
      let (tx, ty, _tz, tw) = transform.transform_point(corner.x, corner.y, 0.0);
      if !tx.is_finite() || !ty.is_finite() || !tw.is_finite() || tw.abs() < 1e-6 || tw < 0.0 {
        return None;
      }
      projected[idx] = Point::new(tx / tw, ty / tw);
    }
    Some(quad_bounds(&projected))
  };

  let mut base = rect;
  let clip_root = clip.map(|c| c.clip_root).unwrap_or(false);
  if !clip_root {
    if let Some(bounds) = root_border_image_bounds(commands, rect, viewport) {
      base = base.union(bounds);
    }
  }
  let outline_bounds = compute_outline_bounds(commands);
  if let Some(desc) = compute_descendant_bounds(commands, rect, viewport, clip_root) {
    let clipped = if let Some(clip) = clip {
      clip_non_outline(
        commands,
        desc,
        clip.rect,
        clip.clip_x,
        clip.clip_y,
        viewport,
      )
    } else {
      desc
    };
    base = base.union(clipped);
  }
  if let Some(path) = clip_path {
    match path {
      StackingClipPath::Shape(path) => {
        base = base
          .intersection(path.bounds())
          .unwrap_or_else(|| path.bounds());
      }
      StackingClipPath::SvgFragment { .. } | StackingClipPath::SvgExternal { .. } => {
        // Unlike basic shapes, SVG `clip-path: url(#id)` can legitimately expose pixels outside the
        // element's reference box (e.g. `clipPathUnits="userSpaceOnUse"` with negative
        // coordinates). We don't currently compute tight bounds for SVG clip paths, so avoid
        // intersecting the stacking-context bounds with the reference rect here (which would
        // incorrectly cull overflow-visible content).
      }
    }
  }
  if let Some(outline_bounds) = outline_bounds {
    base = base.union(outline_bounds);
  }
  let (l, t, r, b) = compute_filter_outset(filters, base, 1.0);
  let (bl, bt, br, bb) = compute_filter_outset(backdrop_filters, base, 1.0);
  let total_l = l.max(bl);
  let total_t = t.max(bt);
  let total_r = r.max(br);
  let total_b = b.max(bb);
  if total_l > 0.0 || total_t > 0.0 || total_r > 0.0 || total_b > 0.0 {
    base = Rect::from_xywh(
      base.min_x() - total_l,
      base.min_y() - total_t,
      base.width() + total_l + total_r,
      base.height() + total_t + total_b,
    );
  }
  if let Some(ts) = transform {
    if let Some(transformed) = project_rect_bounds(base, ts) {
      base = base.union(transformed);
    }
  }
  Some(base)
}

fn command_bounds(cmd: &DisplayCommand, viewport: (f32, f32)) -> Option<Rect> {
  match cmd {
    DisplayCommand::Background { rect, .. }
    | DisplayCommand::Outline { rect, .. }
    | DisplayCommand::Text { rect, .. }
    | DisplayCommand::Replaced { rect, .. } => Some(*rect),
    DisplayCommand::Border { rect, style, .. } => {
      if let Some(bounds) =
        crate::paint::paint_bounds::border_image_paint_bounds(*rect, style, Some(viewport))
      {
        Some(bounds)
      } else {
        Some(*rect)
      }
    }
    DisplayCommand::StackingContext {
      commands,
      filters,
      backdrop_filters,
      clip,
      clip_path,
      rect,
      transform_3d,
      ..
    } => stacking_context_bounds(
      commands,
      filters,
      backdrop_filters,
      *rect,
      transform_3d.as_ref(),
      clip.as_ref(),
      clip_path.as_ref(),
      viewport,
    ),
  }
}

#[allow(dead_code)]
fn compute_commands_bounds(commands: &[DisplayCommand], viewport: (f32, f32)) -> Option<Rect> {
  let mut current: Option<Rect> = None;
  for cmd in commands {
    if let Some(r) = command_bounds(cmd, viewport) {
      current = Some(match current {
        Some(acc) => acc.union(r),
        None => r,
      });
    }
  }
  current
}

fn clip_non_outline(
  commands: &[DisplayCommand],
  mut bounds: Rect,
  clip_rect: Rect,
  clip_x: bool,
  clip_y: bool,
  viewport: (f32, f32),
) -> Rect {
  let mut current: Option<Rect> = None;
  for cmd in commands {
    if matches!(cmd, DisplayCommand::Outline { .. }) {
      continue;
    }
    if let Some(r) = command_bounds(cmd, viewport) {
      let clipped = clip_rect_axes(r, clip_rect, clip_x, clip_y);
      current = Some(current.map(|c| c.union(clipped)).unwrap_or(clipped));
    }
  }
  if let Some(c) = current {
    bounds = bounds.union(c);
  }
  bounds
}

fn describe_content(content: &FragmentContent) -> &'static str {
  match content {
    FragmentContent::Block { .. } => "block",
    FragmentContent::Inline { .. } => "inline",
    FragmentContent::Text { is_marker, .. } => {
      if *is_marker {
        "marker-text"
      } else {
        "text"
      }
    }
    FragmentContent::Line { .. } => "line",
    FragmentContent::Replaced { .. } => "replaced",
    FragmentContent::RunningAnchor { .. } => "running-anchor",
    FragmentContent::FootnoteAnchor { .. } => "footnote-anchor",
  }
}

fn fragment_counts(node: &FragmentNode) -> (usize, usize, usize, usize, usize) {
  let mut total = 1;
  let mut text = 0;
  let mut replaced = 0;
  let mut lines = 0;
  let mut inline = 0;
  match node.content {
    FragmentContent::Text { .. } => text += 1,
    FragmentContent::Replaced { .. } => replaced += 1,
    FragmentContent::Line { .. } => lines += 1,
    FragmentContent::Inline { .. } => inline += 1,
    FragmentContent::Block { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => {}
  }
  for child in node.children.iter() {
    let (t, tx, r, l, i) = fragment_counts(child);
    total += t;
    text += tx;
    replaced += r;
    lines += l;
    inline += i;
  }
  (total, text, replaced, lines, inline)
}

fn fragment_tree_counts(tree: &FragmentTree) -> (usize, usize, usize, usize, usize) {
  std::iter::once(&tree.root)
    .chain(tree.additional_fragments.iter())
    .map(fragment_counts)
    .fold(
      (0, 0, 0, 0, 0),
      |(t0, tx0, r0, l0, i0), (t, tx, r, l, i)| (t0 + t, tx0 + tx, r0 + r, l0 + l, i0 + i),
    )
}

fn resolve_filters(
  filters: &[FilterFunction],
  style: &ComputedStyle,
  viewport: (f32, f32),
  font_ctx: &FontContext,
  svg_filters: &mut SvgFilterResolver,
) -> Vec<ResolvedFilter> {
  filters
    .iter()
    .filter_map(|f| match f {
      FilterFunction::Blur(len) => {
        let radius = resolve_filter_length(len, style, viewport, font_ctx)?;
        (radius >= 0.0).then_some(ResolvedFilter::Blur(radius))
      }
      FilterFunction::Brightness(v) => Some(ResolvedFilter::Brightness((*v).max(0.0))),
      FilterFunction::Contrast(v) => Some(ResolvedFilter::Contrast((*v).max(0.0))),
      FilterFunction::Grayscale(v) => Some(ResolvedFilter::Grayscale(v.clamp(0.0, 1.0))),
      FilterFunction::Sepia(v) => Some(ResolvedFilter::Sepia(v.clamp(0.0, 1.0))),
      FilterFunction::Saturate(v) => Some(ResolvedFilter::Saturate((*v).max(0.0))),
      FilterFunction::HueRotate(deg) => Some(ResolvedFilter::HueRotate(*deg)),
      FilterFunction::Invert(v) => Some(ResolvedFilter::Invert(v.clamp(0.0, 1.0))),
      FilterFunction::Opacity(v) => Some(ResolvedFilter::Opacity(v.clamp(0.0, 1.0))),
      FilterFunction::DropShadow(shadow) => {
        let color = match shadow.color {
          FilterColor::CurrentColor => style.color,
          FilterColor::Color(c) => c,
        };
        let offset_x = resolve_filter_length(&shadow.offset_x, style, viewport, font_ctx)?;
        let offset_y = resolve_filter_length(&shadow.offset_y, style, viewport, font_ctx)?;
        let blur_radius = resolve_filter_length(&shadow.blur_radius, style, viewport, font_ctx)?;
        if blur_radius < 0.0 {
          return None;
        }
        let spread = resolve_filter_length(&shadow.spread, style, viewport, font_ctx)?;
        Some(ResolvedFilter::DropShadow {
          offset_x,
          offset_y,
          blur_radius,
          spread,
          color,
        })
      }
      FilterFunction::Url(url) => svg_filters.resolve(url).map(ResolvedFilter::SvgFilter),
    })
    .collect()
}

fn resolve_filter_length(
  len: &Length,
  style: &ComputedStyle,
  viewport: (f32, f32),
  font_ctx: &FontContext,
) -> Option<f32> {
  if len.has_percentage() {
    return None;
  }

  if len.unit.is_container_query_relative()
    || len
      .calc
      .is_some_and(|calc| calc.has_container_query_relative())
  {
    return None;
  }

  let resolved = resolve_length_with_percentage_metrics(
    *len,
    None,
    Size::new(viewport.0, viewport.1),
    style.font_size,
    style.root_font_size,
    Some(style),
    Some(font_ctx),
  )?;

  resolved.is_finite().then_some(resolved)
}

fn apply_filters(
  pixmap: &mut Pixmap,
  filters: &[ResolvedFilter],
  scale: f32,
  bbox: Rect,
) -> RenderResult<()> {
  for filter in filters {
    match filter {
      ResolvedFilter::Blur(radius) => apply_gaussian_blur(pixmap, *radius * scale)?,
      ResolvedFilter::Brightness(amount) => {
        apply_color_filter(pixmap, |c, a| (scale_color(c, *amount), a))?
      }
      ResolvedFilter::Contrast(amount) => {
        apply_color_filter(pixmap, |c, a| (apply_contrast(c, *amount), a))?
      }
      ResolvedFilter::Grayscale(amount) => {
        apply_color_filter(pixmap, |c, a| (grayscale(c, *amount), a))?
      }
      ResolvedFilter::Sepia(amount) => apply_color_filter(pixmap, |c, a| (sepia(c, *amount), a))?,
      ResolvedFilter::Saturate(amount) => {
        apply_color_filter(pixmap, |c, a| (saturate(c, *amount), a))?
      }
      ResolvedFilter::HueRotate(deg) => {
        apply_color_filter(pixmap, |c, a| (hue_rotate(c, *deg), a))?
      }
      ResolvedFilter::Invert(amount) => apply_color_filter(pixmap, |c, a| (invert(c, *amount), a))?,
      ResolvedFilter::Opacity(amount) => apply_color_filter(pixmap, |c, a| (c, a * *amount))?,
      ResolvedFilter::DropShadow {
        offset_x,
        offset_y,
        blur_radius,
        spread,
        color,
      } => apply_drop_shadow(
        pixmap,
        *offset_x * scale,
        *offset_y * scale,
        *blur_radius * scale,
        *spread * scale,
        *color,
      )?,
      ResolvedFilter::SvgFilter(filter) => {
        crate::paint::svg_filter::apply_svg_filter(filter.as_ref(), pixmap, scale, bbox)?;
      }
    }
  }
  Ok(())
}

fn apply_backdrop_filters(
  pixmap: &mut Pixmap,
  bounds: &Rect,
  filters: &[ResolvedFilter],
  radii: BorderRadii,
  scale: f32,
  filter_bounds: Rect,
) -> RenderResult<()> {
  if filters.is_empty() {
    return Ok(());
  }
  check_active(RenderStage::Paint)?;
  let (out_l, out_t, out_r, out_b) = compute_filter_outset(filters, filter_bounds, scale);

  let x = (bounds.min_x() - out_l).floor() as i32;
  let y = (bounds.min_y() - out_t).floor() as i32;
  let width = (bounds.width() + out_l + out_r).ceil() as u32;
  let height = (bounds.height() + out_t + out_b).ceil() as u32;
  if width == 0 || height == 0 {
    return Ok(());
  }

  let pix_w = pixmap.width() as i32;
  let pix_h = pixmap.height() as i32;
  if x >= pix_w || y >= pix_h {
    return Ok(());
  }

  let clamped_x = x.max(0) as u32;
  let clamped_y = y.max(0) as u32;
  let max_w = pix_w.saturating_sub(clamped_x as i32).max(0) as u32;
  let max_h = pix_h.saturating_sub(clamped_y as i32).max(0) as u32;
  let region_w = width.min(max_w);
  let region_h = height.min(max_h);
  if region_w == 0 || region_h == 0 {
    return Ok(());
  }

  let mut scratch = BACKDROP_FILTER_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
  let mut region = match scratch.region.take() {
    Some(existing) if existing.width() == region_w && existing.height() == region_h => existing,
    _ => match new_pixmap(region_w, region_h) {
      Some(p) => {
        record_layer_allocation(region_w, region_h);
        p
      }
      None => {
        BACKDROP_FILTER_SCRATCH.with(|cell| {
          *cell.borrow_mut() = scratch;
        });
        return Ok(());
      }
    },
  };

  // Copy region
  let bytes_per_row = pixmap.width() as usize * 4;
  let region_row_bytes = region_w as usize * 4;
  let start = (clamped_y as usize * bytes_per_row) + clamped_x as usize * 4;
  let copy_in = (|| -> RenderResult<()> {
    let data = pixmap.data();
    let dest = region.data_mut();
    let mut deadline_counter = 0usize;
    for row in 0..region_h as usize {
      check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
      let src_offset = start + row * bytes_per_row;
      let dst_offset = row * region_row_bytes;
      let src_slice = &data[src_offset..src_offset + region_row_bytes];
      let dst_slice = &mut dest[dst_offset..dst_offset + region_row_bytes];
      dst_slice.copy_from_slice(src_slice);
    }
    Ok(())
  })();
  if let Err(err) = copy_in {
    scratch.region = Some(region);
    BACKDROP_FILTER_SCRATCH.with(|cell| {
      *cell.borrow_mut() = scratch;
    });
    return Err(err);
  }

  let local_bbox = Rect::from_xywh(
    bounds.x() - clamped_x as f32,
    bounds.y() - clamped_y as f32,
    bounds.width(),
    bounds.height(),
  );
  if let Err(err) = apply_filters(&mut region, filters, scale, local_bbox) {
    scratch.region = Some(region);
    BACKDROP_FILTER_SCRATCH.with(|cell| {
      *cell.borrow_mut() = scratch;
    });
    return Err(err);
  }
  if !radii.is_zero() {
    if let Err(err) = apply_clip_mask_rect(&mut region, local_bbox, radii) {
      scratch.region = Some(region);
      BACKDROP_FILTER_SCRATCH.with(|cell| {
        *cell.borrow_mut() = scratch;
      });
      return Err(err);
    }
  }

  check_active(RenderStage::Paint)?;
  let mut paint = PixmapPaint::default();
  paint.blend_mode = SkiaBlendMode::SourceOver;
  pixmap.draw_pixmap(
    clamped_x as i32,
    clamped_y as i32,
    region.as_ref(),
    &paint,
    Transform::identity(),
    None,
  );

  scratch.region = Some(region);
  BACKDROP_FILTER_SCRATCH.with(|cell| {
    *cell.borrow_mut() = scratch;
  });
  Ok(())
}

fn apply_color_filter<F>(pixmap: &mut Pixmap, mut f: F) -> RenderResult<()>
where
  F: FnMut([f32; 3], f32) -> ([f32; 3], f32),
{
  const FILTER_DEADLINE_STRIDE: usize = 1024;
  for (idx, px) in pixmap.pixels_mut().iter_mut().enumerate() {
    if idx % FILTER_DEADLINE_STRIDE == 0 {
      check_active(RenderStage::Paint)?;
    }
    let alpha = px.alpha() as f32 / 255.0;
    let base = if alpha > 0.0 {
      [
        (px.red() as f32 / 255.0) / alpha,
        (px.green() as f32 / 255.0) / alpha,
        (px.blue() as f32 / 255.0) / alpha,
      ]
    } else {
      [0.0, 0.0, 0.0]
    };
    let (mut color, mut new_alpha) = f(base, alpha);
    new_alpha = new_alpha.clamp(0.0, 1.0);
    color[0] = color[0].clamp(0.0, 1.0);
    color[1] = color[1].clamp(0.0, 1.0);
    color[2] = color[2].clamp(0.0, 1.0);

    let r = (color[0] * new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    let g = (color[1] * new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    let b = (color[2] * new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    let a = (new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;

    *px = PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(PremultipliedColorU8::TRANSPARENT);
  }
  Ok(())
}

fn scale_color(color: [f32; 3], factor: f32) -> [f32; 3] {
  [color[0] * factor, color[1] * factor, color[2] * factor]
}

fn apply_contrast(color: [f32; 3], factor: f32) -> [f32; 3] {
  [
    ((color[0] - 0.5) * factor + 0.5),
    ((color[1] - 0.5) * factor + 0.5),
    ((color[2] - 0.5) * factor + 0.5),
  ]
}

fn grayscale(color: [f32; 3], amount: f32) -> [f32; 3] {
  let gray = color[0] * 0.2126 + color[1] * 0.7152 + color[2] * 0.0722;
  [
    color[0] + (gray - color[0]) * amount,
    color[1] + (gray - color[1]) * amount,
    color[2] + (gray - color[2]) * amount,
  ]
}

fn sepia(color: [f32; 3], amount: f32) -> [f32; 3] {
  let sepia_r = color[0] * 0.393 + color[1] * 0.769 + color[2] * 0.189;
  let sepia_g = color[0] * 0.349 + color[1] * 0.686 + color[2] * 0.168;
  let sepia_b = color[0] * 0.272 + color[1] * 0.534 + color[2] * 0.131;
  [
    color[0] + (sepia_r - color[0]) * amount,
    color[1] + (sepia_g - color[1]) * amount,
    color[2] + (sepia_b - color[2]) * amount,
  ]
}

fn saturate(color: [f32; 3], factor: f32) -> [f32; 3] {
  let rw = 0.213;
  let gw = 0.715;
  let bw = 0.072;
  [
    (rw + (1.0 - rw) * factor) * color[0]
      + (gw - gw * factor) * color[1]
      + (bw - bw * factor) * color[2],
    (rw - rw * factor) * color[0]
      + (gw + (1.0 - gw) * factor) * color[1]
      + (bw - bw * factor) * color[2],
    (rw - rw * factor) * color[0]
      + (gw - gw * factor) * color[1]
      + (bw + (1.0 - bw) * factor) * color[2],
  ]
}

fn hue_rotate(color: [f32; 3], degrees: f32) -> [f32; 3] {
  let angle = degrees.to_radians();
  let cos = angle.cos();
  let sin = angle.sin();

  let r = color[0];
  let g = color[1];
  let b = color[2];

  [
    r * (0.213 + cos * 0.787 - sin * 0.213)
      + g * (0.715 - 0.715 * cos - 0.715 * sin)
      + b * (0.072 - 0.072 * cos + 0.928 * sin),
    r * (0.213 - 0.213 * cos + 0.143 * sin)
      + g * (0.715 + 0.285 * cos + 0.140 * sin)
      + b * (0.072 - 0.072 * cos - 0.283 * sin),
    r * (0.213 - 0.213 * cos - 0.787 * sin)
      + g * (0.715 - 0.715 * cos + 0.715 * sin)
      + b * (0.072 + 0.928 * cos + 0.072 * sin),
  ]
}

fn invert(color: [f32; 3], amount: f32) -> [f32; 3] {
  [
    color[0] + (1.0 - color[0] - color[0]) * amount,
    color[1] + (1.0 - color[1] - color[1]) * amount,
    color[2] + (1.0 - color[2] - color[2]) * amount,
  ]
}

const LEGACY_FILTER_DEADLINE_STRIDE: usize = 4096;

fn apply_drop_shadow(
  pixmap: &mut Pixmap,
  offset_x: f32,
  offset_y: f32,
  blur_radius: f32,
  spread: f32,
  color: Rgba,
) -> RenderResult<()> {
  let width = pixmap.width();
  let height = pixmap.height();
  if width == 0 || height == 0 {
    return Ok(());
  }

  let mut shadow = match new_pixmap(width, height) {
    Some(p) => {
      record_layer_allocation(width, height);
      p
    }
    None => return Ok(()),
  };

  {
    let src = pixmap.pixels();
    let dst = shadow.pixels_mut();
    for (idx, (src_px, dst_px)) in src.iter().zip(dst.iter_mut()).enumerate() {
      if idx % LEGACY_FILTER_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
      let alpha = src_px.alpha() as f32 / 255.0;
      if alpha == 0.0 {
        *dst_px = PremultipliedColorU8::TRANSPARENT;
        continue;
      }
      let total_alpha = (color.a * alpha).clamp(0.0, 1.0);
      let r = (color.r as f32 / 255.0) * total_alpha;
      let g = (color.g as f32 / 255.0) * total_alpha;
      let b = (color.b as f32 / 255.0) * total_alpha;
      let a = total_alpha * 255.0;
      *dst_px = PremultipliedColorU8::from_rgba(
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
        a.round().clamp(0.0, 255.0) as u8,
      )
      .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
  }

  if spread != 0.0 {
    apply_spread(&mut shadow, spread)?;
  }

  if blur_radius > 0.0 {
    apply_gaussian_blur(&mut shadow, blur_radius)?;
  }

  let mut paint = PixmapPaint::default();
  paint.blend_mode = SkiaBlendMode::DestinationOver;
  pixmap.draw_pixmap(
    0,
    0,
    shadow.as_ref(),
    &paint,
    Transform::from_translate(offset_x, offset_y),
    None,
  );
  Ok(())
}

#[derive(Default)]
struct DropShadowSpreadScratch {
  alpha0: Vec<u8>,
  alpha1: Vec<u8>,
}

thread_local! {
  static DROP_SHADOW_SPREAD_SCRATCH: RefCell<DropShadowSpreadScratch> =
    RefCell::new(DropShadowSpreadScratch::default());
}

fn apply_spread(pixmap: &mut Pixmap, spread: f32) -> RenderResult<()> {
  // Run a separable square dilation/erosion with sliding-window extrema to avoid
  // the quadratic neighborhood scan.
  let radius = spread.abs().ceil() as i32;
  if radius <= 0 || spread == 0.0 {
    return Ok(());
  }
  let expand = spread > 0.0;
  let width = pixmap.width() as usize;
  let height = pixmap.height() as usize;
  if width == 0 || height == 0 {
    return Ok(());
  }
  let radius = radius as usize;
  // Clamp-to-edge addressing saturates once the radius exceeds the image dimensions. Clamp here
  // to avoid pathological window sizes and extremely long sliding-window loops when CSS supplies
  // a very large `spread`.
  let radius_x = radius.min(width.saturating_sub(1));
  let radius_y = radius.min(height.saturating_sub(1));
  let len = width
    .checked_mul(height)
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: buffer size overflow ({width}x{height})"),
    })?;

  let mut base_ratio = (0.0, 0.0, 0.0);
  for px in pixmap.pixels().iter() {
    let alpha = px.alpha();
    if alpha > 0 {
      let a = alpha as f32;
      base_ratio = (
        px.red() as f32 / a,
        px.green() as f32 / a,
        px.blue() as f32 / a,
      );
      break;
    }
  }

  let mut scratch = DROP_SHADOW_SPREAD_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
  let result = (|| -> RenderResult<()> {
    scratch
      .alpha0
      .try_reserve_exact(len.saturating_sub(scratch.alpha0.len()))
      .map_err(|err| RenderError::InvalidParameters {
        message: format!("drop shadow spread: alpha scratch allocation failed: {err}"),
      })?;
    scratch.alpha0.resize(len, 0);

    scratch
      .alpha1
      .try_reserve_exact(len.saturating_sub(scratch.alpha1.len()))
      .map_err(|err| RenderError::InvalidParameters {
        message: format!("drop shadow spread: alpha scratch allocation failed: {err}"),
      })?;
    scratch.alpha1.resize(len, 0);

    {
      let src_pixels = pixmap.pixels();
      for (idx, (src, dst)) in src_pixels.iter().zip(scratch.alpha0.iter_mut()).enumerate() {
        if idx % LEGACY_FILTER_DEADLINE_STRIDE == 0 {
          check_active(RenderStage::Paint)?;
        }
        *dst = src.alpha();
      }
    }

    apply_spread_alpha_horizontal(
      &scratch.alpha0,
      &mut scratch.alpha1,
      width,
      height,
      radius_x,
      expand,
    )?;

    apply_spread_alpha_vertical(
      &scratch.alpha1,
      &mut scratch.alpha0,
      width,
      height,
      radius_y,
      expand,
    )?;

    // Apply the updated alpha back onto the pixmap while preserving the per-pixel premultiplied
    // color ratios used by the legacy spread implementation.
    let dst_pixels = pixmap.pixels_mut();
    for (idx, px) in dst_pixels.iter_mut().enumerate() {
      if idx % LEGACY_FILTER_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
      let agg_alpha = scratch.alpha0[idx];
      if agg_alpha == 0 {
        *px = PremultipliedColorU8::TRANSPARENT;
        continue;
      }

      let orig = *px;
      let orig_alpha = orig.alpha();
      if orig_alpha > 0 {
        let factor = (agg_alpha as f32) / (orig_alpha as f32);
        let r = (orig.red() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let g = (orig.green() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let b = (orig.blue() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        *px = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      } else {
        let r = (base_ratio.0 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let g = (base_ratio.1 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let b = (base_ratio.2 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        *px = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      }
    }

    Ok(())
  })();

  DROP_SHADOW_SPREAD_SCRATCH.with(|cell| {
    *cell.borrow_mut() = scratch;
  });

  result
}

fn apply_spread_alpha_horizontal(
  src: &[u8],
  dst: &mut [u8],
  width: usize,
  height: usize,
  radius: usize,
  expand: bool,
) -> RenderResult<()> {
  debug_assert_eq!(src.len(), width * height);
  debug_assert_eq!(dst.len(), width * height);
  if radius == 0 {
    dst.copy_from_slice(src);
    return Ok(());
  }
  let window_size = radius
    .checked_mul(2)
    .and_then(|size| size.checked_add(1))
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let extended_len = width
    .checked_add(
      radius
        .checked_mul(2)
        .ok_or(RenderError::InvalidParameters {
          message: format!("drop shadow spread: buffer size overflow (radius={radius})"),
        })?,
    )
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: buffer size overflow (width={width}, radius={radius})"),
    })?;

  let queue_capacity = window_size
    .checked_add(1)
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let mut queue: VecDeque<(usize, u8)> = VecDeque::new();
  queue
    .try_reserve_exact(queue_capacity)
    .map_err(|err| RenderError::InvalidParameters {
      message: format!("drop shadow spread: window buffer allocation failed: {err}"),
    })?;
  let mut deadline_counter = 0usize;
  for y in 0..height {
    queue.clear();
    let row_start = y * width;
    for j in 0..extended_len {
      check_active_periodic(
        &mut deadline_counter,
        LEGACY_FILTER_DEADLINE_STRIDE,
        RenderStage::Paint,
      )?;
      let src_x = if j < radius {
        0
      } else if j >= radius + width {
        width - 1
      } else {
        j - radius
      };
      let value = src[row_start + src_x];

      if expand {
        while let Some(&(_, v)) = queue.back() {
          if v >= value {
            break;
          }
          queue.pop_back();
        }
      } else {
        while let Some(&(_, v)) = queue.back() {
          if v <= value {
            break;
          }
          queue.pop_back();
        }
      }
      queue.push_back((j, value));

      if j >= window_size {
        let expire = j - window_size;
        while let Some(&(idx, _)) = queue.front() {
          if idx <= expire {
            queue.pop_front();
          } else {
            break;
          }
        }
      }

      if j + 1 >= window_size {
        let out_x = j + 1 - window_size;
        dst[row_start + out_x] = queue.front().map(|(_, v)| *v).unwrap_or(0);
      }
    }
  }
  Ok(())
}

fn apply_spread_alpha_vertical(
  src: &[u8],
  dst: &mut [u8],
  width: usize,
  height: usize,
  radius: usize,
  expand: bool,
) -> RenderResult<()> {
  debug_assert_eq!(src.len(), width * height);
  debug_assert_eq!(dst.len(), width * height);
  if radius == 0 {
    dst.copy_from_slice(src);
    return Ok(());
  }
  let window_size = radius
    .checked_mul(2)
    .and_then(|size| size.checked_add(1))
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let extended_len = height
    .checked_add(
      radius
        .checked_mul(2)
        .ok_or(RenderError::InvalidParameters {
          message: format!("drop shadow spread: buffer size overflow (radius={radius})"),
        })?,
    )
    .ok_or(RenderError::InvalidParameters {
      message: format!(
        "drop shadow spread: buffer size overflow (height={height}, radius={radius})"
      ),
    })?;

  let queue_capacity = window_size
    .checked_add(1)
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let mut queue: VecDeque<(usize, u8)> = VecDeque::new();
  queue
    .try_reserve_exact(queue_capacity)
    .map_err(|err| RenderError::InvalidParameters {
      message: format!("drop shadow spread: window buffer allocation failed: {err}"),
    })?;
  let mut deadline_counter = 0usize;
  for x in 0..width {
    queue.clear();
    for j in 0..extended_len {
      check_active_periodic(
        &mut deadline_counter,
        LEGACY_FILTER_DEADLINE_STRIDE,
        RenderStage::Paint,
      )?;
      let src_y = if j < radius {
        0
      } else if j >= radius + height {
        height - 1
      } else {
        j - radius
      };
      let value = src[src_y * width + x];

      if expand {
        while let Some(&(_, v)) = queue.back() {
          if v >= value {
            break;
          }
          queue.pop_back();
        }
      } else {
        while let Some(&(_, v)) = queue.back() {
          if v <= value {
            break;
          }
          queue.pop_back();
        }
      }
      queue.push_back((j, value));

      if j >= window_size {
        let expire = j - window_size;
        while let Some(&(idx, _)) = queue.front() {
          if idx <= expire {
            queue.pop_front();
          } else {
            break;
          }
        }
      }

      if j + 1 >= window_size {
        let out_y = j + 1 - window_size;
        dst[out_y * width + x] = queue.front().map(|(_, v)| *v).unwrap_or(0);
      }
    }
  }
  Ok(())
}

#[cfg(test)]
fn apply_spread_slow_reference(pixmap: &mut Pixmap, spread: f32) {
  let radius = spread.abs().ceil() as i32;
  if radius <= 0 || spread == 0.0 {
    return;
  }
  let expand = spread > 0.0;
  let width = pixmap.width() as i32;
  let height = pixmap.height() as i32;
  let original = pixmap.clone();
  let src = original.pixels();
  let dst = pixmap.pixels_mut();

  let mut base_ratio = (0.0, 0.0, 0.0);
  for px in src.iter() {
    let alpha = px.alpha();
    if alpha > 0 {
      let a = alpha as f32;
      base_ratio = (
        px.red() as f32 / a,
        px.green() as f32 / a,
        px.blue() as f32 / a,
      );
      break;
    }
  }

  for y in 0..height {
    for x in 0..width {
      let mut agg_alpha = if expand { 0u8 } else { 255u8 };
      for dy in -radius..=radius {
        for dx in -radius..=radius {
          let ny = (y + dy).clamp(0, height - 1);
          let nx = (x + dx).clamp(0, width - 1);
          let idx = (ny as usize) * (width as usize) + nx as usize;
          let px = src[idx];
          if expand {
            agg_alpha = agg_alpha.max(px.alpha());
          } else {
            agg_alpha = agg_alpha.min(px.alpha());
          }
        }
      }
      let idx = (y as usize) * (width as usize) + x as usize;
      if agg_alpha == 0 {
        dst[idx] = PremultipliedColorU8::TRANSPARENT;
        continue;
      }

      let orig = src[idx];
      let orig_alpha = orig.alpha();
      if orig_alpha > 0 {
        let factor = (agg_alpha as f32) / (orig_alpha as f32);
        let r = (orig.red() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let g = (orig.green() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let b = (orig.blue() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        dst[idx] = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      } else {
        let r = (base_ratio.0 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let g = (base_ratio.1 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let b = (base_ratio.2 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        dst[idx] = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      }
    }
  }
}

fn is_hsl_blend(mode: MixBlendMode) -> bool {
  matches!(
    mode,
    MixBlendMode::Hue | MixBlendMode::Saturation | MixBlendMode::Color | MixBlendMode::Luminosity
  )
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
  let max = r.max(g).max(b);
  let min = r.min(g).min(b);
  let l = (max + min) / 2.0;
  if (max - min).abs() < f32::EPSILON {
    return (0.0, 0.0, l);
  }

  let d = max - min;
  let s = if l > 0.5 {
    d / (2.0 - max - min)
  } else {
    d / (max + min)
  };
  let h = if (max - r).abs() < f32::EPSILON {
    (g - b) / d + if g < b { 6.0 } else { 0.0 }
  } else if (max - g).abs() < f32::EPSILON {
    (b - r) / d + 2.0
  } else {
    (r - g) / d + 4.0
  } / 6.0;
  (h, s, l)
}

fn hue_to_rgb(p: f32, q: f32, t: f32) -> f32 {
  let mut t = t;
  if t < 0.0 {
    t += 1.0;
  }
  if t > 1.0 {
    t -= 1.0;
  }
  if t < 1.0 / 6.0 {
    p + (q - p) * 6.0 * t
  } else if t < 0.5 {
    q
  } else if t < 2.0 / 3.0 {
    p + (q - p) * (2.0 / 3.0 - t) * 6.0
  } else {
    p
  }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
  if s <= 0.0 {
    return (l, l, l);
  }
  let q = if l < 0.5 {
    l * (1.0 + s)
  } else {
    l + s - l * s
  };
  let p = 2.0 * l - q;
  let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
  let g = hue_to_rgb(p, q, h);
  let b = hue_to_rgb(p, q, h - 1.0 / 3.0);
  (r, g, b)
}

fn apply_hsl_blend(
  mode: MixBlendMode,
  src: (f32, f32, f32),
  dst: (f32, f32, f32),
) -> (f32, f32, f32) {
  let (sh, ss, sl) = rgb_to_hsl(src.0, src.1, src.2);
  let (dh, ds, dl) = rgb_to_hsl(dst.0, dst.1, dst.2);
  match mode {
    MixBlendMode::Hue => hsl_to_rgb(sh, ds, dl),
    MixBlendMode::Saturation => hsl_to_rgb(dh, ss, dl),
    MixBlendMode::Color => hsl_to_rgb(sh, ss, dl),
    MixBlendMode::Luminosity => hsl_to_rgb(dh, ds, sl),
    _ => dst,
  }
}

fn composite_hsl_layer(
  dest: &mut Pixmap,
  layer: &Pixmap,
  opacity: f32,
  mode: MixBlendMode,
  area: Option<Rect>,
) {
  if !is_hsl_blend(mode) {
    return;
  }
  let dest_width = dest.width() as usize;
  let dest_height = dest.height() as usize;
  let layer_width = layer.width() as usize;
  let layer_height = layer.height() as usize;
  if dest_width == 0 || dest_height == 0 || layer_width == 0 || layer_height == 0 {
    return;
  }

  let max_width = dest_width.min(layer_width);
  let max_height = dest_height.min(layer_height);
  let (mut x0, mut y0, mut x1, mut y1) = if let Some(rect) = area {
    let x0 = rect.min_x().floor() as i32;
    let y0 = rect.min_y().floor() as i32;
    let x1 = rect.max_x().ceil() as i32;
    let y1 = rect.max_y().ceil() as i32;
    (x0, y0, x1, y1)
  } else {
    (0, 0, max_width as i32, max_height as i32)
  };
  x0 = x0.clamp(0, max_width as i32);
  y0 = y0.clamp(0, max_height as i32);
  x1 = x1.clamp(0, max_width as i32);
  y1 = y1.clamp(0, max_height as i32);
  if x0 >= x1 || y0 >= y1 {
    return;
  }

  let src_pixels = layer.pixels();
  let dst_pixels = dest.pixels_mut();
  let src_stride = layer_width;
  let dst_stride = dest_width;
  let opacity = opacity.clamp(0.0, 1.0);

  for y in y0..y1 {
    let yi = y as usize;
    for x in x0..x1 {
      let xi = x as usize;
      let src_px = src_pixels[yi * src_stride + xi];
      let raw_sa = src_px.alpha() as f32 / 255.0;
      if raw_sa == 0.0 || opacity == 0.0 {
        continue;
      }
      let dst_px = &mut dst_pixels[yi * dst_stride + xi];
      let sa = (raw_sa * opacity).clamp(0.0, 1.0);
      let da = dst_px.alpha() as f32 / 255.0;

      let src_rgb = if raw_sa > 0.0 {
        (
          (src_px.red() as f32 / 255.0) / raw_sa,
          (src_px.green() as f32 / 255.0) / raw_sa,
          (src_px.blue() as f32 / 255.0) / raw_sa,
        )
      } else {
        (0.0, 0.0, 0.0)
      };
      let dst_rgb = if da > 0.0 {
        (
          (dst_px.red() as f32 / 255.0) / da,
          (dst_px.green() as f32 / 255.0) / da,
          (dst_px.blue() as f32 / 255.0) / da,
        )
      } else {
        (0.0, 0.0, 0.0)
      };

      let blended_rgb = apply_hsl_blend(mode, src_rgb, dst_rgb);

      let out_a = sa + da * (1.0 - sa);
      let out_rgb = if out_a > 0.0 {
        (
          (blended_rgb.0 * sa + dst_rgb.0 * da * (1.0 - sa)) / out_a,
          (blended_rgb.1 * sa + dst_rgb.1 * da * (1.0 - sa)) / out_a,
          (blended_rgb.2 * sa + dst_rgb.2 * da * (1.0 - sa)) / out_a,
        )
      } else {
        (0.0, 0.0, 0.0)
      };

      let out_a_u8 = (out_a * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
      let scale = out_a;
      let r = ((out_rgb.0 * scale) * 255.0 + 0.5).clamp(0.0, out_a_u8 as f32) as u8;
      let g = ((out_rgb.1 * scale) * 255.0 + 0.5).clamp(0.0, out_a_u8 as f32) as u8;
      let b = ((out_rgb.2 * scale) * 255.0 + 0.5).clamp(0.0, out_a_u8 as f32) as u8;
      *dst_px = PremultipliedColorU8::from_rgba(r, g, b, out_a_u8)
        .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
  }
}

fn composite_hsl_layer_offset(
  dest: &mut Pixmap,
  layer: &Pixmap,
  opacity: f32,
  mode: MixBlendMode,
  offset_x: i32,
  offset_y: i32,
) {
  if !is_hsl_blend(mode) {
    return;
  }
  let opacity = opacity.clamp(0.0, 1.0);
  if opacity <= 0.0 {
    return;
  }

  let dest_w = dest.width() as i32;
  let dest_h = dest.height() as i32;
  let src_w = layer.width() as i32;
  let src_h = layer.height() as i32;
  if dest_w <= 0 || dest_h <= 0 || src_w <= 0 || src_h <= 0 {
    return;
  }

  let dst_x0 = offset_x.max(0);
  let dst_y0 = offset_y.max(0);
  let dst_x1 = (offset_x + src_w).min(dest_w);
  let dst_y1 = (offset_y + src_h).min(dest_h);
  if dst_x0 >= dst_x1 || dst_y0 >= dst_y1 {
    return;
  }

  let src_pixels = layer.pixels();
  let src_stride = src_w as usize;
  let dst_stride = dest_w as usize;
  let dst_pixels = dest.pixels_mut();

  for dy in dst_y0..dst_y1 {
    let sy = (dy - offset_y) as usize;
    let dst_row = dy as usize;
    for dx in dst_x0..dst_x1 {
      let sx = (dx - offset_x) as usize;
      let src_px = src_pixels[sy * src_stride + sx];
      let raw_sa = src_px.alpha() as f32 / 255.0;
      if raw_sa == 0.0 {
        continue;
      }

      let sa = (raw_sa * opacity).clamp(0.0, 1.0);
      if sa == 0.0 {
        continue;
      }

      let dst_px = &mut dst_pixels[dst_row * dst_stride + dx as usize];
      let da = dst_px.alpha() as f32 / 255.0;

      let src_rgb = (
        (src_px.red() as f32 / 255.0) / raw_sa,
        (src_px.green() as f32 / 255.0) / raw_sa,
        (src_px.blue() as f32 / 255.0) / raw_sa,
      );
      let dst_rgb = if da > 0.0 {
        (
          (dst_px.red() as f32 / 255.0) / da,
          (dst_px.green() as f32 / 255.0) / da,
          (dst_px.blue() as f32 / 255.0) / da,
        )
      } else {
        (0.0, 0.0, 0.0)
      };

      let blended_rgb = apply_hsl_blend(mode, src_rgb, dst_rgb);

      let out_a = sa + da * (1.0 - sa);
      let out_rgb = if out_a > 0.0 {
        (
          (blended_rgb.0 * sa + dst_rgb.0 * da * (1.0 - sa)) / out_a,
          (blended_rgb.1 * sa + dst_rgb.1 * da * (1.0 - sa)) / out_a,
          (blended_rgb.2 * sa + dst_rgb.2 * da * (1.0 - sa)) / out_a,
        )
      } else {
        (0.0, 0.0, 0.0)
      };

      let out_a_u8 = (out_a * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
      let scale = out_a;
      let r = ((out_rgb.0 * scale) * 255.0 + 0.5).clamp(0.0, out_a_u8 as f32) as u8;
      let g = ((out_rgb.1 * scale) * 255.0 + 0.5).clamp(0.0, out_a_u8 as f32) as u8;
      let b = ((out_rgb.2 * scale) * 255.0 + 0.5).clamp(0.0, out_a_u8 as f32) as u8;
      *dst_px = PremultipliedColorU8::from_rgba(r, g, b, out_a_u8)
        .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
  }
}

fn map_blend_mode(mode: MixBlendMode) -> SkiaBlendMode {
  match mode {
    MixBlendMode::Normal => SkiaBlendMode::SourceOver,
    MixBlendMode::Multiply => SkiaBlendMode::Multiply,
    MixBlendMode::Screen => SkiaBlendMode::Screen,
    MixBlendMode::Overlay => SkiaBlendMode::Overlay,
    MixBlendMode::Darken => SkiaBlendMode::Darken,
    MixBlendMode::Lighten => SkiaBlendMode::Lighten,
    MixBlendMode::ColorDodge => SkiaBlendMode::ColorDodge,
    MixBlendMode::ColorBurn => SkiaBlendMode::ColorBurn,
    MixBlendMode::HardLight => SkiaBlendMode::HardLight,
    MixBlendMode::SoftLight => SkiaBlendMode::SoftLight,
    MixBlendMode::Difference => SkiaBlendMode::Difference,
    MixBlendMode::Exclusion => SkiaBlendMode::Exclusion,
    MixBlendMode::Hue => SkiaBlendMode::Hue,
    MixBlendMode::Saturation => SkiaBlendMode::Saturation,
    MixBlendMode::Color => SkiaBlendMode::Color,
    MixBlendMode::Luminosity => SkiaBlendMode::Luminosity,
    MixBlendMode::PlusLighter => SkiaBlendMode::Plus,
    MixBlendMode::PlusDarker
    | MixBlendMode::HueHsv
    | MixBlendMode::SaturationHsv
    | MixBlendMode::ColorHsv
    | MixBlendMode::LuminosityHsv
    | MixBlendMode::HueOklch
    | MixBlendMode::ChromaOklch
    | MixBlendMode::ColorOklch
    | MixBlendMode::LuminosityOklch => SkiaBlendMode::SourceOver,
  }
}

fn resolve_border_radii(style: Option<&ComputedStyle>, bounds: Rect) -> BorderRadii {
  let Some(style) = style else {
    return BorderRadii::ZERO;
  };
  let w = bounds.width().max(0.0);
  let h = bounds.height().max(0.0);
  if w <= 0.0 || h <= 0.0 {
    return BorderRadii::ZERO;
  }

  let resolve_radius = |len: &Length, reference: f32| -> f32 {
    resolve_length_for_paint(
      len,
      style.font_size,
      style.root_font_size,
      reference,
      (w, h),
    )
  };

  let radii = BorderRadii {
    top_left: crate::paint::display_list::BorderRadius {
      x: resolve_radius(&style.border_top_left_radius.x, w).max(0.0),
      y: resolve_radius(&style.border_top_left_radius.y, h).max(0.0),
    },
    top_right: crate::paint::display_list::BorderRadius {
      x: resolve_radius(&style.border_top_right_radius.x, w).max(0.0),
      y: resolve_radius(&style.border_top_right_radius.y, h).max(0.0),
    },
    bottom_right: crate::paint::display_list::BorderRadius {
      x: resolve_radius(&style.border_bottom_right_radius.x, w).max(0.0),
      y: resolve_radius(&style.border_bottom_right_radius.y, h).max(0.0),
    },
    bottom_left: crate::paint::display_list::BorderRadius {
      x: resolve_radius(&style.border_bottom_left_radius.x, w).max(0.0),
      y: resolve_radius(&style.border_bottom_left_radius.y, h).max(0.0),
    },
  };
  radii.clamped(w, h)
}

fn resolve_clip_radii(
  style: &ComputedStyle,
  rects: &BackgroundRects,
  clip: crate::style::types::BackgroundBox,
  viewport: Option<(f32, f32)>,
) -> BorderRadii {
  let base = resolve_border_radii(Some(style), rects.border);
  if base.is_zero() {
    return base;
  }

  let percentage_base = rects.border.width().max(0.0);
  let font_size = style.font_size;
  let vp = viewport.unwrap_or((percentage_base, percentage_base));
  let border_left = resolve_length_for_paint(
    &style.used_border_left_width(),
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );
  let border_right = resolve_length_for_paint(
    &style.used_border_right_width(),
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );
  let border_top = resolve_length_for_paint(
    &style.used_border_top_width(),
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );
  let border_bottom = resolve_length_for_paint(
    &style.used_border_bottom_width(),
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );

  let padding_left = resolve_length_for_paint(
    &style.padding_left,
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );
  let padding_right = resolve_length_for_paint(
    &style.padding_right,
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );
  let padding_top = resolve_length_for_paint(
    &style.padding_top,
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );
  let padding_bottom = resolve_length_for_paint(
    &style.padding_bottom,
    font_size,
    style.root_font_size,
    percentage_base,
    vp,
  );

  match clip {
    crate::style::types::BackgroundBox::BorderBox => base,
    crate::style::types::BackgroundBox::PaddingBox => {
      let shrunk = BorderRadii {
        top_left: crate::paint::display_list::BorderRadius {
          x: (base.top_left.x - border_left).max(0.0),
          y: (base.top_left.y - border_top).max(0.0),
        },
        top_right: crate::paint::display_list::BorderRadius {
          x: (base.top_right.x - border_right).max(0.0),
          y: (base.top_right.y - border_top).max(0.0),
        },
        bottom_right: crate::paint::display_list::BorderRadius {
          x: (base.bottom_right.x - border_right).max(0.0),
          y: (base.bottom_right.y - border_bottom).max(0.0),
        },
        bottom_left: crate::paint::display_list::BorderRadius {
          x: (base.bottom_left.x - border_left).max(0.0),
          y: (base.bottom_left.y - border_bottom).max(0.0),
        },
      };
      shrunk.clamped(rects.padding.width(), rects.padding.height())
    }
    crate::style::types::BackgroundBox::ContentBox | crate::style::types::BackgroundBox::Text => {
      let shrink_left = border_left + padding_left;
      let shrink_right = border_right + padding_right;
      let shrink_top = border_top + padding_top;
      let shrink_bottom = border_bottom + padding_bottom;
      let shrunk = BorderRadii {
        top_left: crate::paint::display_list::BorderRadius {
          x: (base.top_left.x - shrink_left).max(0.0),
          y: (base.top_left.y - shrink_top).max(0.0),
        },
        top_right: crate::paint::display_list::BorderRadius {
          x: (base.top_right.x - shrink_right).max(0.0),
          y: (base.top_right.y - shrink_top).max(0.0),
        },
        bottom_right: crate::paint::display_list::BorderRadius {
          x: (base.bottom_right.x - shrink_right).max(0.0),
          y: (base.bottom_right.y - shrink_bottom).max(0.0),
        },
        bottom_left: crate::paint::display_list::BorderRadius {
          x: (base.bottom_left.x - shrink_left).max(0.0),
          y: (base.bottom_left.y - shrink_bottom).max(0.0),
        },
      };
      shrunk.clamped(rects.content.width(), rects.content.height())
    }
  }
}

fn build_rounded_rect_mask(
  rect: Rect,
  radii: BorderRadii,
  canvas_w: u32,
  canvas_h: u32,
) -> Option<Mask> {
  if canvas_w == 0 || canvas_h == 0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
    return None;
  }

  let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
    rect.x(),
    rect.y(),
    rect.width(),
    rect.height(),
    &radii,
  ) else {
    return None;
  };

  let mut mask = Mask::new(canvas_w, canvas_h)?;
  mask.fill_path(
    &path,
    tiny_skia::FillRule::Winding,
    true,
    Transform::identity(),
  );
  Some(mask)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClipMaskDirtyRect {
  x0: u32,
  y0: u32,
  x1: u32,
  y1: u32,
}

const CLIP_MASK_DEADLINE_STRIDE: usize = 16 * 1024;

#[derive(Default)]
struct CssMaskScratch {
  mask: Option<Mask>,
  last_dirty: Option<ClipMaskDirtyRect>,
}

thread_local! {
  static CSS_MASK_SCRATCH: RefCell<CssMaskScratch> = RefCell::new(CssMaskScratch::default());
}

#[derive(Debug)]
struct RenderedMask {
  mask: Option<Mask>,
  dirty: Option<ClipMaskDirtyRect>,
}

impl RenderedMask {
  #[inline]
  fn mask(&self) -> &Mask {
    self
      .mask
      .as_ref()
      .expect("RenderedMask must contain a mask until dropped")
  }
}

impl Drop for RenderedMask {
  fn drop(&mut self) {
    let Some(mask) = self.mask.take() else { return };
    CSS_MASK_SCRATCH.with(|cell| {
      let mut scratch = cell.borrow_mut();
      scratch.mask = Some(mask);
      scratch.last_dirty = self.dirty;
    });
  }
}

#[derive(Default)]
struct ClipMaskScratch {
  mask: Option<Mask>,
  last_rect: Option<Rect>,
  last_radii: Option<BorderRadii>,
}

thread_local! {
  static CLIP_MASK_SCRATCH: RefCell<ClipMaskScratch> = RefCell::new(ClipMaskScratch::default());
}

#[derive(Default)]
struct MaskLayerPixmapScratch {
  pixmap: Option<Pixmap>,
  last_dirty: Option<ClipMaskDirtyRect>,
}

thread_local! {
  static MASK_LAYER_PIXMAP_SCRATCH: RefCell<MaskLayerPixmapScratch> =
    RefCell::new(MaskLayerPixmapScratch::default());
}

#[derive(Default)]
struct BackdropFilterScratch {
  region: Option<Pixmap>,
}

thread_local! {
  static BACKDROP_FILTER_SCRATCH: RefCell<BackdropFilterScratch> =
    RefCell::new(BackdropFilterScratch::default());
}

#[derive(Default)]
struct BackgroundClipMaskScratch {
  mask: Option<Mask>,
  last_rect: Option<Rect>,
  last_radii: Option<BorderRadii>,
}

thread_local! {
  static BACKGROUND_CLIP_MASK_SCRATCH: RefCell<BackgroundClipMaskScratch> =
    RefCell::new(BackgroundClipMaskScratch::default());
}

pub(crate) fn reset_thread_local_scratch_for_tests() {
  DROP_SHADOW_SPREAD_SCRATCH.with(|cell| *cell.borrow_mut() = DropShadowSpreadScratch::default());
  CSS_MASK_SCRATCH.with(|cell| *cell.borrow_mut() = CssMaskScratch::default());
  CLIP_MASK_SCRATCH.with(|cell| *cell.borrow_mut() = ClipMaskScratch::default());
  MASK_LAYER_PIXMAP_SCRATCH.with(|cell| *cell.borrow_mut() = MaskLayerPixmapScratch::default());
  BACKDROP_FILTER_SCRATCH.with(|cell| *cell.borrow_mut() = BackdropFilterScratch::default());
  BACKGROUND_CLIP_MASK_SCRATCH
    .with(|cell| *cell.borrow_mut() = BackgroundClipMaskScratch::default());
}

struct BackgroundClipMaskGuard {
  scratch: BackgroundClipMaskScratch,
}

impl BackgroundClipMaskGuard {
  fn take() -> Self {
    let scratch = BACKGROUND_CLIP_MASK_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    Self { scratch }
  }

  fn mask(
    &mut self,
    rect: Rect,
    radii: BorderRadii,
    canvas_w: u32,
    canvas_h: u32,
  ) -> Option<&Mask> {
    if canvas_w == 0 || canvas_h == 0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
      return None;
    }

    let replace = match self.scratch.mask.as_ref() {
      Some(existing) => existing.width() != canvas_w || existing.height() != canvas_h,
      None => true,
    };
    if replace {
      self.scratch.mask = Mask::new(canvas_w, canvas_h);
      self.scratch.last_rect = None;
      self.scratch.last_radii = None;
    }

    let clamped = radii.clamped(rect.width(), rect.height());
    let needs_rebuild =
      self.scratch.last_rect != Some(rect) || self.scratch.last_radii != Some(clamped);

    let Some(mask) = self.scratch.mask.as_mut() else {
      return None;
    };

    if needs_rebuild {
      if let Some(dirty) = clip_mask_dirty_bounds(rect, canvas_w, canvas_h) {
        if clear_mask_rect(mask, dirty).is_err() {
          return None;
        }
      } else {
        mask.data_mut().fill(0);
      }

      let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        &clamped,
      ) else {
        return None;
      };

      mask.fill_path(
        &path,
        tiny_skia::FillRule::Winding,
        true,
        Transform::identity(),
      );

      self.scratch.last_rect = Some(rect);
      self.scratch.last_radii = Some(clamped);
    }

    Some(mask)
  }
}

impl Drop for BackgroundClipMaskGuard {
  fn drop(&mut self) {
    BACKGROUND_CLIP_MASK_SCRATCH.with(|cell| {
      *cell.borrow_mut() = std::mem::take(&mut self.scratch);
    });
  }
}

fn clear_mask_rect(mask: &mut Mask, rect: ClipMaskDirtyRect) -> RenderResult<()> {
  if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
    return Ok(());
  }
  let width = mask.width() as usize;
  let stride = width;
  let data = mask.data_mut();
  let x0 = rect.x0 as usize;
  let x1 = rect.x1 as usize;
  let y0 = rect.y0 as usize;
  let y1 = rect.y1 as usize;
  let mut deadline_counter = 0usize;
  if x0 == 0 && x1 == stride {
    for chunk in data[y0 * stride..y1 * stride].chunks_mut(CLIP_MASK_DEADLINE_STRIDE) {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      chunk.fill(0);
    }
    return Ok(());
  }
  for row in y0..y1 {
    let offset = row * stride;
    for chunk in data[offset + x0..offset + x1].chunks_mut(CLIP_MASK_DEADLINE_STRIDE) {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      chunk.fill(0);
    }
  }
  Ok(())
}

#[inline]
fn dirty_union(a: ClipMaskDirtyRect, b: ClipMaskDirtyRect) -> ClipMaskDirtyRect {
  ClipMaskDirtyRect {
    x0: a.x0.min(b.x0),
    y0: a.y0.min(b.y0),
    x1: a.x1.max(b.x1),
    y1: a.y1.max(b.y1),
  }
}

#[inline]
fn dirty_intersection(a: ClipMaskDirtyRect, b: ClipMaskDirtyRect) -> Option<ClipMaskDirtyRect> {
  let x0 = a.x0.max(b.x0);
  let y0 = a.y0.max(b.y0);
  let x1 = a.x1.min(b.x1);
  let y1 = a.y1.min(b.y1);
  if x0 >= x1 || y0 >= y1 {
    None
  } else {
    Some(ClipMaskDirtyRect { x0, y0, x1, y1 })
  }
}

fn hard_clip_mask_outside_rect_within_bounds(
  mask: &mut Mask,
  keep: ClipMaskDirtyRect,
  bounds: ClipMaskDirtyRect,
) -> RenderResult<()> {
  if bounds.x0 >= bounds.x1 || bounds.y0 >= bounds.y1 {
    return Ok(());
  }

  let keep_x0 = keep.x0.max(bounds.x0);
  let keep_y0 = keep.y0.max(bounds.y0);
  let keep_x1 = keep.x1.min(bounds.x1);
  let keep_y1 = keep.y1.min(bounds.y1);
  if keep_x0 >= keep_x1 || keep_y0 >= keep_y1 {
    clear_mask_rect(mask, bounds)?;
    return Ok(());
  }

  if bounds.y0 < keep_y0 {
    clear_mask_rect(
      mask,
      ClipMaskDirtyRect {
        x0: bounds.x0,
        y0: bounds.y0,
        x1: bounds.x1,
        y1: keep_y0,
      },
    )?;
  }
  if keep_y1 < bounds.y1 {
    clear_mask_rect(
      mask,
      ClipMaskDirtyRect {
        x0: bounds.x0,
        y0: keep_y1,
        x1: bounds.x1,
        y1: bounds.y1,
      },
    )?;
  }
  if bounds.x0 < keep_x0 {
    clear_mask_rect(
      mask,
      ClipMaskDirtyRect {
        x0: bounds.x0,
        y0: keep_y0,
        x1: keep_x0,
        y1: keep_y1,
      },
    )?;
  }
  if keep_x1 < bounds.x1 {
    clear_mask_rect(
      mask,
      ClipMaskDirtyRect {
        x0: keep_x1,
        y0: keep_y0,
        x1: bounds.x1,
        y1: keep_y1,
      },
    )?;
  }
  Ok(())
}

#[inline]
fn div_255(value: u16) -> u16 {
  // Exact floor division by 255 for values in the 0..=65025 range (the max product of two u8s).
  (value + 1 + (value >> 8)) >> 8
}

fn copy_pixmap_alpha_to_mask(
  mask: &mut Mask,
  pixmap: &Pixmap,
  rect: ClipMaskDirtyRect,
) -> RenderResult<()> {
  if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
    return Ok(());
  }
  if mask.width() != pixmap.width() || mask.height() != pixmap.height() {
    return Ok(());
  }

  let width = mask.width() as usize;
  let mask_stride = width;
  let pixmap_stride = width * 4;
  let mask_data = mask.data_mut();
  let pixmap_data = pixmap.data();

  let x0 = rect.x0 as usize;
  let x1 = rect.x1 as usize;
  let mut deadline_counter = 0usize;
  for y in rect.y0 as usize..rect.y1 as usize {
    let dst_start = y * mask_stride + x0;
    let dst_end = y * mask_stride + x1;
    let dst = &mut mask_data[dst_start..dst_end];
    let mut src_idx = y * pixmap_stride + x0 * 4 + 3;
    let mut x = 0usize;
    while x < dst.len() {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      let x_end = (x + CLIP_MASK_DEADLINE_STRIDE).min(dst.len());
      for px in dst[x..x_end].iter_mut() {
        *px = pixmap_data[src_idx];
        src_idx += 4;
      }
      x = x_end;
    }
  }
  Ok(())
}

fn apply_text_clip_mask_to_pixmap_alpha(
  pixmap: &mut Pixmap,
  text_mask: &Mask,
  rect: ClipMaskDirtyRect,
) -> RenderResult<()> {
  if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
    return Ok(());
  }
  if pixmap.width() != text_mask.width() || pixmap.height() != text_mask.height() {
    return Ok(());
  }

  let width = pixmap.width() as usize;
  let pixmap_stride = width * 4;
  let text_stride = width;
  let data = pixmap.data_mut();
  let text_data = text_mask.data();

  let x0 = rect.x0 as usize;
  let x1 = rect.x1 as usize;
  let row_len = x1.saturating_sub(x0);
  if row_len == 0 {
    return Ok(());
  }

  let mut deadline_counter = 0usize;
  for y in rect.y0 as usize..rect.y1 as usize {
    let mut pix_idx = y * pixmap_stride + x0 * 4 + 3;
    let mut text_idx = y * text_stride + x0;
    let mut x = 0usize;
    while x < row_len {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      let x_end = (x + CLIP_MASK_DEADLINE_STRIDE).min(row_len);
      for _ in x..x_end {
        let clip = text_data[text_idx] as u16;
        let r = data[pix_idx - 3] as u16;
        let g = data[pix_idx - 2] as u16;
        let b = data[pix_idx - 1] as u16;
        let a = data[pix_idx] as u16;
        data[pix_idx - 3] = div_255(r * clip) as u8;
        data[pix_idx - 2] = div_255(g * clip) as u8;
        data[pix_idx - 1] = div_255(b * clip) as u8;
        data[pix_idx] = div_255(a * clip) as u8;
        pix_idx += 4;
        text_idx += 1;
      }
      x = x_end;
    }
  }
  Ok(())
}

fn apply_mask_composite_from_pixmap_alpha(
  dest: &mut Mask,
  src: &Pixmap,
  rect: ClipMaskDirtyRect,
  op: MaskComposite,
) -> RenderResult<()> {
  if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
    return Ok(());
  }
  if dest.width() != src.width() || dest.height() != src.height() {
    return Ok(());
  }

  let width = dest.width() as usize;
  let mask_stride = width;
  let pixmap_stride = width * 4;
  let dest_data = dest.data_mut();
  let src_data = src.data();

  let x0 = rect.x0 as usize;
  let x1 = rect.x1 as usize;
  let mut deadline_counter = 0usize;
  match op {
    MaskComposite::Add => {
      for y in rect.y0 as usize..rect.y1 as usize {
        let dst_start = y * mask_stride + x0;
        let dst_end = y * mask_stride + x1;
        let dst = &mut dest_data[dst_start..dst_end];
        let mut src_idx = y * pixmap_stride + x0 * 4 + 3;
        let mut x = 0usize;
        while x < dst.len() {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let x_end = (x + CLIP_MASK_DEADLINE_STRIDE).min(dst.len());
          for px in dst[x..x_end].iter_mut() {
            let s = src_data[src_idx] as u16;
            let d = *px as u16;
            let out = s + div_255(d * (255 - s));
            *px = out.min(255) as u8;
            src_idx += 4;
          }
          x = x_end;
        }
      }
    }
    MaskComposite::Subtract => {
      for y in rect.y0 as usize..rect.y1 as usize {
        let dst_start = y * mask_stride + x0;
        let dst_end = y * mask_stride + x1;
        let dst = &mut dest_data[dst_start..dst_end];
        let mut src_idx = y * pixmap_stride + x0 * 4 + 3;
        let mut x = 0usize;
        while x < dst.len() {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let x_end = (x + CLIP_MASK_DEADLINE_STRIDE).min(dst.len());
          for px in dst[x..x_end].iter_mut() {
            let s = src_data[src_idx] as u16;
            let d = *px as u16;
            *px = div_255(s * (255 - d)) as u8;
            src_idx += 4;
          }
          x = x_end;
        }
      }
    }
    MaskComposite::Intersect => {
      for y in rect.y0 as usize..rect.y1 as usize {
        let dst_start = y * mask_stride + x0;
        let dst_end = y * mask_stride + x1;
        let dst = &mut dest_data[dst_start..dst_end];
        let mut src_idx = y * pixmap_stride + x0 * 4 + 3;
        let mut x = 0usize;
        while x < dst.len() {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let x_end = (x + CLIP_MASK_DEADLINE_STRIDE).min(dst.len());
          for px in dst[x..x_end].iter_mut() {
            let s = src_data[src_idx] as u16;
            let d = *px as u16;
            *px = div_255(s * d) as u8;
            src_idx += 4;
          }
          x = x_end;
        }
      }
    }
    MaskComposite::Exclude => {
      for y in rect.y0 as usize..rect.y1 as usize {
        let dst_start = y * mask_stride + x0;
        let dst_end = y * mask_stride + x1;
        let dst = &mut dest_data[dst_start..dst_end];
        let mut src_idx = y * pixmap_stride + x0 * 4 + 3;
        let mut x = 0usize;
        while x < dst.len() {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let x_end = (x + CLIP_MASK_DEADLINE_STRIDE).min(dst.len());
          for px in dst[x..x_end].iter_mut() {
            let s = src_data[src_idx] as u16;
            let d = *px as u16;
            let src_out = div_255(s * (255 - d));
            let dst_out = div_255(d * (255 - s));
            *px = (src_out + dst_out).min(255) as u8;
            src_idx += 4;
          }
          x = x_end;
        }
      }
    }
  }
  Ok(())
}

fn clear_pixmap_rect_rgba(pixmap: &mut Pixmap, rect: ClipMaskDirtyRect) -> RenderResult<()> {
  if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
    return Ok(());
  }

  let width = pixmap.width() as usize;
  let stride = width * 4;
  let data = pixmap.data_mut();
  let x0 = rect.x0 as usize * 4;
  let x1 = rect.x1 as usize * 4;
  let y0 = rect.y0 as usize;
  let y1 = rect.y1 as usize;
  let chunk_bytes = CLIP_MASK_DEADLINE_STRIDE.saturating_mul(4);
  let mut deadline_counter = 0usize;

  if x0 == 0 && x1 == stride {
    for chunk in data[y0 * stride..y1 * stride].chunks_mut(chunk_bytes) {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      chunk.fill(0);
    }
    return Ok(());
  }

  for row in y0..y1 {
    let offset = row * stride;
    for chunk in data[offset + x0..offset + x1].chunks_mut(chunk_bytes) {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      chunk.fill(0);
    }
  }
  Ok(())
}

#[inline]
fn mul_div_255_round(value: u8, alpha: u8) -> u8 {
  // Match `tiny_skia::Pixmap::apply_mask` rounding behavior.
  let prod = value as u16 * alpha as u16;
  ((prod + 255) >> 8) as u8
}

fn apply_mask_rect_rgba(
  pixmap: &mut Pixmap,
  mask: &Mask,
  rect: ClipMaskDirtyRect,
) -> RenderResult<()> {
  if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
    return Ok(());
  }
  if pixmap.width() != mask.width() || pixmap.height() != mask.height() {
    return Ok(());
  }

  check_active(RenderStage::Paint)?;
  let width = pixmap.width() as usize;
  let pixel_stride = width * 4;
  let mask_stride = width;
  let pixmap_data = pixmap.data_mut();
  let mask_data = mask.data();

  let x0 = rect.x0 as usize;
  let x1 = rect.x1 as usize;
  let chunk_bytes = CLIP_MASK_DEADLINE_STRIDE.saturating_mul(4);
  for y in rect.y0 as usize..rect.y1 as usize {
    let pixel_row_start = y * pixel_stride;
    let mask_row_start = y * mask_stride;
    let row_pixels = &mut pixmap_data[pixel_row_start + x0 * 4..pixel_row_start + x1 * 4];
    let row_mask = &mask_data[mask_row_start + x0..mask_row_start + x1];
    for (pixel_chunk, mask_chunk) in row_pixels
      .chunks_mut(chunk_bytes)
      .zip(row_mask.chunks(CLIP_MASK_DEADLINE_STRIDE))
    {
      check_active(RenderStage::Paint)?;
      for (px, m) in pixel_chunk.chunks_exact_mut(4).zip(mask_chunk.iter()) {
        let m = *m;
        if m == 255 {
          continue;
        }
        if m == 0 {
          px.fill(0);
          continue;
        }
        px[0] = mul_div_255_round(px[0], m);
        px[1] = mul_div_255_round(px[1], m);
        px[2] = mul_div_255_round(px[2], m);
        px[3] = mul_div_255_round(px[3], m);
      }
    }
  }
  Ok(())
}

fn apply_mask_with_dirty_bounds_rgba(
  pixmap: &mut Pixmap,
  mask: &Mask,
  dirty: Option<ClipMaskDirtyRect>,
) -> RenderResult<()> {
  if pixmap.width() != mask.width() || pixmap.height() != mask.height() {
    return Ok(());
  }

  let Some(dirty) = dirty else {
    check_active(RenderStage::Paint)?;
    pixmap.data_mut().fill(0);
    return Ok(());
  };

  let w = dirty.x1.saturating_sub(dirty.x0);
  let h = dirty.y1.saturating_sub(dirty.y0);
  if w == 0 || h == 0 {
    check_active(RenderStage::Paint)?;
    pixmap.data_mut().fill(0);
    return Ok(());
  }

  hard_clip_pixmap_outside_rect_rgba(
    pixmap,
    Rect::from_xywh(dirty.x0 as f32, dirty.y0 as f32, w as f32, h as f32),
  )?;
  apply_mask_rect_rgba(pixmap, mask, dirty)?;
  Ok(())
}

fn clip_mask_dirty_bounds(rect: Rect, width: u32, height: u32) -> Option<ClipMaskDirtyRect> {
  let x0 = rect.x().floor().max(0.0) as u32;
  let y0 = rect.y().floor().max(0.0) as u32;
  let x1 = (rect.x() + rect.width()).ceil().max(0.0) as u32;
  let y1 = (rect.y() + rect.height()).ceil().max(0.0) as u32;
  let x0 = x0.saturating_sub(1).min(width);
  let y0 = y0.saturating_sub(1).min(height);
  let x1 = x1.saturating_add(1).min(width);
  let y1 = y1.saturating_add(1).min(height);
  if x0 >= x1 || y0 >= y1 {
    None
  } else {
    Some(ClipMaskDirtyRect { x0, y0, x1, y1 })
  }
}

fn hard_clip_pixmap_outside_rect_rgba(pixmap: &mut Pixmap, rect: Rect) -> RenderResult<()> {
  check_active(RenderStage::Paint)?;
  let width = pixmap.width();
  let height = pixmap.height();

  // Hard-clip pixels outside the rectangle to avoid filter bleed.
  //
  // This is intentionally done using integer bounds (floor/ceil) so that any pixels outside the
  // clip rect are *fully* cleared even when the clip coordinates are fractional. Filters can
  // otherwise sample premultiplied RGB values outside the clip and bleed them back in.
  let x0 = (rect.x().floor().max(0.0) as u32).min(width);
  let y0 = (rect.y().floor().max(0.0) as u32).min(height);
  let x1 = (rect.x() + rect.width()).ceil().min(width as f32) as u32;
  let y1 = (rect.y() + rect.height()).ceil().min(height as f32) as u32;
  if x0 == 0 && y0 == 0 && x1 == width && y1 == height {
    return Ok(());
  }

  let stride = width as usize * 4;
  let data = pixmap.data_mut();

  // Clear fully outside rows in one contiguous fill.
  data[..y0 as usize * stride].fill(0);
  check_active(RenderStage::Paint)?;
  data[y1 as usize * stride..].fill(0);
  check_active(RenderStage::Paint)?;

  let left_bytes = x0 as usize * 4;
  let right_start = x1 as usize * 4;
  if left_bytes != 0 || right_start != stride {
    let row_pixels = width as usize;
    let deadline_row_stride = (CLIP_MASK_DEADLINE_STRIDE / row_pixels.max(1)).max(1);
    let mut deadline_counter = 0usize;
    for row in y0..y1 {
      if deadline_counter % deadline_row_stride == 0 {
        check_active(RenderStage::Paint)?;
      }
      deadline_counter = deadline_counter.wrapping_add(1);
      let offset = row as usize * stride;
      let row = &mut data[offset..offset + stride];
      row[..left_bytes].fill(0);
      row[right_start..].fill(0);
    }
  }
  Ok(())
}

fn apply_clip_mask_rect(pixmap: &mut Pixmap, rect: Rect, radii: BorderRadii) -> RenderResult<()> {
  check_active(RenderStage::Paint)?;
  if rect.width() <= 0.0 || rect.height() <= 0.0 {
    return Ok(());
  }
  let width = pixmap.width();
  let height = pixmap.height();
  if width == 0 || height == 0 {
    return Ok(());
  }
  let diag_enabled = paint_diagnostics_enabled();
  let clip_timer = diag_enabled.then(Instant::now);
  if diag_enabled {
    with_paint_diagnostics(|stats| {
      stats.clip_mask_calls += 1;
    });
  }

  // No-op fast path: clipping to a full-coverage axis-aligned rect without radii.
  if radii.is_zero() {
    let right = rect.x() + rect.width();
    let bottom = rect.y() + rect.height();
    if rect.x() <= 0.5
      && rect.y() <= 0.5
      && right >= width as f32 - 0.5
      && bottom >= height as f32 - 0.5
    {
      if diag_enabled {
        with_paint_diagnostics(|stats| {
          if let Some(start) = clip_timer {
            stats.clip_mask_ms += start.elapsed().as_secs_f64() * 1000.0;
          }
        });
      }
      return Ok(());
    }
  }

  // Fast path: fully outside the pixmap bounds.
  let right = rect.x() + rect.width();
  let bottom = rect.y() + rect.height();
  if right <= 0.0 || bottom <= 0.0 || rect.x() >= width as f32 || rect.y() >= height as f32 {
    check_active(RenderStage::Paint)?;
    pixmap.data_mut().fill(0);
    if diag_enabled {
      with_paint_diagnostics(|stats| {
        stats.clip_mask_pixels += u64::from(width) * u64::from(height);
        if let Some(start) = clip_timer {
          stats.clip_mask_ms += start.elapsed().as_secs_f64() * 1000.0;
        }
      });
    }
    return Ok(());
  }

  // Fast path: pure hard clip for pixel-aligned axis-aligned rects with no radii. For these
  // rectangles the rounded-rect mask would be all-0/255 and therefore equivalent to a hard clip,
  // so we can avoid generating and applying the mask entirely.
  if radii.is_zero() {
    let is_int = |v: f32| v.is_finite() && (v - v.round()).abs() <= 1e-6;
    if is_int(rect.x()) && is_int(rect.y()) && is_int(rect.width()) && is_int(rect.height()) {
      hard_clip_pixmap_outside_rect_rgba(pixmap, rect)?;
      if diag_enabled {
        with_paint_diagnostics(|stats| {
          stats.clip_mask_pixels += u64::from(width) * u64::from(height);
          if let Some(start) = clip_timer {
            stats.clip_mask_ms += start.elapsed().as_secs_f64() * 1000.0;
          }
        });
      }
      return Ok(());
    }
  }

  // Generate a rounded-rect alpha mask, reusing a thread-local scratch mask to avoid per-call
  // allocations.
  let applied_mask = CLIP_MASK_SCRATCH.with(|cell| -> RenderResult<bool> {
    let mut scratch = cell.borrow_mut();
    let replace = match scratch.mask.as_ref() {
      Some(existing) => existing.width() != width || existing.height() != height,
      None => true,
    };
    if replace {
      scratch.mask = Mask::new(width, height);
      scratch.last_rect = None;
      scratch.last_radii = None;
    }

    let clamped = radii.clamped(rect.width(), rect.height());
    let needs_rebuild = scratch.last_rect != Some(rect) || scratch.last_radii != Some(clamped);
    let dirty = clip_mask_dirty_bounds(rect, width, height);

    {
      let Some(mask) = scratch.mask.as_mut() else {
        return Ok(false);
      };

      if needs_rebuild {
        if let Some(dirty) = dirty {
          clear_mask_rect(mask, dirty)?;
        } else {
          // If the dirty bounds are empty (e.g. due to weird floating point inputs), clear the
          // entire mask so we don't reuse stale data.
          mask.data_mut().fill(0);
        }

        let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
          rect.x(),
          rect.y(),
          rect.width(),
          rect.height(),
          &clamped,
        ) else {
          return Ok(false);
        };

        check_active(RenderStage::Paint)?;
        // Match `fill_rounded_rect` semantics (anti-aliased) without allocating an intermediate
        // RGBA pixmap or converting it to a mask.
        mask.fill_path(
          &path,
          tiny_skia::FillRule::Winding,
          true,
          Transform::identity(),
        );
      }

      if let Some(dirty) = dirty {
        let right = rect.x() + rect.width();
        let bottom = rect.y() + rect.height();
        let left_radius = clamped.top_left.x.max(clamped.bottom_left.x);
        let right_radius = clamped.top_right.x.max(clamped.bottom_right.x);
        let top_radius = clamped.top_left.y.max(clamped.top_right.y);
        let bottom_radius = clamped.bottom_left.y.max(clamped.bottom_right.y);
        let skip_x0 = (rect.x() + left_radius).ceil().clamp(0.0, width as f32) as u32;
        let skip_x1 = (right - right_radius).floor().clamp(0.0, width as f32) as u32;
        let skip_y0 = (rect.y() + top_radius).ceil().clamp(0.0, height as f32) as u32;
        let skip_y1 = (bottom - bottom_radius).floor().clamp(0.0, height as f32) as u32;

        let skip_x0 = skip_x0.clamp(dirty.x0, dirty.x1);
        let skip_x1 = skip_x1.clamp(dirty.x0, dirty.x1);
        let skip_y0 = skip_y0.clamp(dirty.y0, dirty.y1);
        let skip_y1 = skip_y1.clamp(dirty.y0, dirty.y1);

        if skip_x0 >= skip_x1 || skip_y0 >= skip_y1 {
          apply_mask_rect_rgba(pixmap, mask, dirty)?;
        } else {
          if dirty.y0 < skip_y0 {
            apply_mask_rect_rgba(
              pixmap,
              mask,
              ClipMaskDirtyRect {
                x0: dirty.x0,
                y0: dirty.y0,
                x1: dirty.x1,
                y1: skip_y0,
              },
            )?;
          }
          if skip_y0 < skip_y1 {
            if dirty.x0 < skip_x0 {
              apply_mask_rect_rgba(
                pixmap,
                mask,
                ClipMaskDirtyRect {
                  x0: dirty.x0,
                  y0: skip_y0,
                  x1: skip_x0,
                  y1: skip_y1,
                },
              )?;
            }
            if skip_x1 < dirty.x1 {
              apply_mask_rect_rgba(
                pixmap,
                mask,
                ClipMaskDirtyRect {
                  x0: skip_x1,
                  y0: skip_y0,
                  x1: dirty.x1,
                  y1: skip_y1,
                },
              )?;
            }
          }
          if skip_y1 < dirty.y1 {
            apply_mask_rect_rgba(
              pixmap,
              mask,
              ClipMaskDirtyRect {
                x0: dirty.x0,
                y0: skip_y1,
                x1: dirty.x1,
                y1: dirty.y1,
              },
            )?;
          }
        }
      } else {
        apply_mask_rect_rgba(
          pixmap,
          mask,
          ClipMaskDirtyRect {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
          },
        )?;
      }
    }

    if needs_rebuild {
      scratch.last_rect = Some(rect);
      scratch.last_radii = Some(clamped);
    }
    Ok(true)
  })?;
  if !applied_mask {
    if diag_enabled {
      with_paint_diagnostics(|stats| {
        if let Some(start) = clip_timer {
          stats.clip_mask_ms += start.elapsed().as_secs_f64() * 1000.0;
        }
      });
    }
    return Ok(());
  }

  hard_clip_pixmap_outside_rect_rgba(pixmap, rect)?;

  if diag_enabled {
    with_paint_diagnostics(|stats| {
      stats.clip_mask_pixels += u64::from(width) * u64::from(height);
      if let Some(start) = clip_timer {
        stats.clip_mask_ms += start.elapsed().as_secs_f64() * 1000.0;
      }
    });
  }
  Ok(())
}

fn mask_value_from_pixel(pixel: &[u8], mode: MaskMode) -> u8 {
  let a = pixel.get(3).copied().unwrap_or(0) as f32 / 255.0;
  let value = match mode {
    MaskMode::Alpha | MaskMode::MatchSource => a,
    MaskMode::Luminance => {
      if a <= 0.0 {
        0.0
      } else {
        let r = pixel.get(0).copied().unwrap_or(0) as f32 / 255.0 / a;
        let g = pixel.get(1).copied().unwrap_or(0) as f32 / 255.0 / a;
        let b = pixel.get(2).copied().unwrap_or(0) as f32 / 255.0 / a;
        (0.2126 * r + 0.7152 * g + 0.0722 * b) * a
      }
    }
  };
  (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn mask_tile_from_image(tile: &Pixmap, mode: MaskMode) -> RenderResult<Option<Pixmap>> {
  let Some(size) = IntSize::from_wh(tile.width(), tile.height()) else {
    return Ok(None);
  };

  let mut data = Vec::new();
  if data.try_reserve_exact(tile.data().len()).is_err() {
    return Ok(None);
  }
  let mut deadline_counter = 0usize;
  let chunk_bytes = CLIP_MASK_DEADLINE_STRIDE.saturating_mul(4);
  for pixel_chunk in tile.data().chunks(chunk_bytes) {
    check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
    for chunk in pixel_chunk.chunks_exact(4) {
      let v = mask_value_from_pixel(chunk, mode);
      data.extend_from_slice(&[v, v, v, v]);
    }
  }
  Ok(Pixmap::from_vec(data, size))
}

fn paint_mask_tile(
  dest: &mut Pixmap,
  tile: &Pixmap,
  tx: f32,
  ty: f32,
  tile_w: f32,
  tile_h: f32,
  clip_rect: Rect,
  origin_offset: Point,
  scale: f32,
) {
  if tile_w <= 0.0 || tile_h <= 0.0 {
    return;
  }
  let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
  let Some(intersection) = tile_rect.intersection(clip_rect) else {
    return;
  };
  if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
    return;
  }

  let device_clip = Rect::from_xywh(
    (intersection.x() - origin_offset.x) * scale,
    (intersection.y() - origin_offset.y) * scale,
    intersection.width() * scale,
    intersection.height() * scale,
  );
  if device_clip.width() <= 0.0 || device_clip.height() <= 0.0 {
    return;
  }
  let device_bounds = Rect::from_xywh(0.0, 0.0, dest.width() as f32, dest.height() as f32);
  let Some(device_clip) = device_clip.intersection(device_bounds) else {
    return;
  };
  let Some(src_rect) = SkiaRect::from_xywh(
    device_clip.x(),
    device_clip.y(),
    device_clip.width(),
    device_clip.height(),
  ) else {
    return;
  };

  let scale_x = tile_w / tile.width() as f32;
  let scale_y = tile_h / tile.height() as f32;

  let mut paint = Paint::default();
  paint.shader = Pattern::new(
    tile.as_ref(),
    SpreadMode::Pad,
    FilterQuality::Bilinear,
    1.0,
    Transform::from_row(
      scale_x * scale,
      0.0,
      0.0,
      scale_y * scale,
      (tx - origin_offset.x) * scale,
      (ty - origin_offset.y) * scale,
    ),
  );
  paint.anti_alias = false;
  dest.fill_rect(src_rect, &paint, Transform::identity(), None);
}

fn paint_mask_border_patch(
  dest: &mut Pixmap,
  source: &Pixmap,
  src_rect: Rect,
  dest_rect: Rect,
  repeat_x: BorderImageRepeat,
  repeat_y: BorderImageRepeat,
  clip_rect: Rect,
) {
  if src_rect.width() <= 0.0
    || src_rect.height() <= 0.0
    || dest_rect.width() <= 0.0
    || dest_rect.height() <= 0.0
  {
    return;
  }

  let Some(clip_rect) = dest_rect.intersection(clip_rect) else {
    return;
  };
  if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
    return;
  }

  let sx0 = src_rect.x().max(0.0).floor() as u32;
  let sy0 = src_rect.y().max(0.0).floor() as u32;
  let sx1 = (src_rect.x() + src_rect.width())
    .ceil()
    .min(source.width() as f32)
    .max(0.0) as u32;
  let sy1 = (src_rect.y() + src_rect.height())
    .ceil()
    .min(source.height() as f32)
    .max(0.0) as u32;
  if sx1 <= sx0 || sy1 <= sy0 {
    return;
  }
  let width = sx1 - sx0;
  let height = sy1 - sy0;

  let Some(bytes) = u64::from(width)
    .checked_mul(u64::from(height))
    .and_then(|px| px.checked_mul(4))
  else {
    return;
  };
  let mut patch = match reserve_buffer(bytes, "mask-border patch") {
    Ok(buf) => buf,
    Err(_) => return,
  };
  let Some(size) = IntSize::from_wh(width, height) else {
    return;
  };

  let data = source.data();
  let source_width = source.width();
  let Some(row_bytes) = u64::from(width).checked_mul(4) else {
    return;
  };
  for row in sy0..sy1 {
    let Some(start) = u64::from(row)
      .checked_mul(u64::from(source_width))
      .and_then(|v| v.checked_add(u64::from(sx0)))
      .and_then(|v| v.checked_mul(4))
    else {
      return;
    };
    let Some(end) = start.checked_add(row_bytes) else {
      return;
    };
    let Ok(start) = usize::try_from(start) else {
      return;
    };
    let Ok(end) = usize::try_from(end) else {
      return;
    };
    if end > data.len() || start > end {
      return;
    }
    patch.extend_from_slice(&data[start..end]);
  }
  let Some(patch_pixmap) = Pixmap::from_vec(patch, size) else {
    return;
  };

  let mut tile_w = dest_rect.width();
  let mut tile_h = dest_rect.height();

  let mut scale_x = tile_w / width as f32;
  let mut scale_y = tile_h / height as f32;

  if repeat_x != BorderImageRepeat::Stretch {
    scale_x = scale_y;
    tile_w = width as f32 * scale_x;
  }
  if repeat_y != BorderImageRepeat::Stretch {
    scale_y = scale_x;
    tile_h = height as f32 * scale_y;
  }

  if repeat_x == BorderImageRepeat::Round && tile_w > 0.0 {
    let count = (dest_rect.width() / tile_w).round().max(1.0);
    tile_w = dest_rect.width() / count;
    scale_x = tile_w / width as f32;
  }
  if repeat_y == BorderImageRepeat::Round && tile_h > 0.0 {
    let count = (dest_rect.height() / tile_h).round().max(1.0);
    tile_h = dest_rect.height() / count;
    scale_y = tile_h / height as f32;
  }

  // Avoid panics/aborts when the destination is extremely thin and would require an unbounded
  // number of tiles (e.g. a huge width with a near-zero height and repeat-x=space).
  const MAX_BORDER_IMAGE_TILES_PER_AXIS: f32 = 4096.0;
  let paint_repeated_patch = |dest: &mut Pixmap| {
    let Some(dest_sk_rect) = SkiaRect::from_xywh(
      clip_rect.x(),
      clip_rect.y(),
      clip_rect.width(),
      clip_rect.height(),
    ) else {
      return;
    };
    let mut paint = Paint::default();
    paint.shader = Pattern::new(
      patch_pixmap.as_ref(),
      SpreadMode::Repeat,
      FilterQuality::Bilinear,
      1.0,
      Transform::from_row(scale_x, 0.0, 0.0, scale_y, dest_rect.x(), dest_rect.y()),
    );
    paint.anti_alias = false;
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
    dest.fill_rect(dest_sk_rect, &paint, Transform::identity(), None);
  };

  let tiles_x = dest_rect.width() / tile_w;
  let tiles_y = dest_rect.height() / tile_h;
  let too_many_x = repeat_x != BorderImageRepeat::Stretch
    && (!tiles_x.is_finite() || tiles_x > MAX_BORDER_IMAGE_TILES_PER_AXIS);
  let too_many_y = repeat_y != BorderImageRepeat::Stretch
    && (!tiles_y.is_finite() || tiles_y > MAX_BORDER_IMAGE_TILES_PER_AXIS);
  if too_many_x || too_many_y {
    paint_repeated_patch(dest);
    return;
  }

  let positions_x = match repeat_x {
    BorderImageRepeat::Stretch => vec![dest_rect.x()],
    BorderImageRepeat::Round => {
      let end = dest_rect.x() + dest_rect.width();
      if tile_w <= 0.0 {
        return;
      }
      let count = (tiles_x.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
      let mut pos = Vec::new();
      if pos.try_reserve_exact(count).is_err() {
        paint_repeated_patch(dest);
        return;
      }
      let mut cursor = dest_rect.x();
      for _ in 0..count {
        if cursor >= end - 1e-3 {
          break;
        }
        pos.push(cursor);
        cursor += tile_w;
      }
      pos
    }
    BorderImageRepeat::Space => {
      if tile_w <= 0.0 {
        return;
      }
      let count = tiles_x.floor();
      if count < 2.0 {
        vec![dest_rect.x() + (dest_rect.width() - tile_w) * 0.5]
      } else {
        let spacing = (dest_rect.width() - tile_w * count) / (count - 1.0);
        let count = count as usize;
        let mut pos = Vec::new();
        if pos.try_reserve_exact(count).is_err() {
          paint_repeated_patch(dest);
          return;
        }
        let mut cursor = dest_rect.x();
        for _ in 0..count {
          pos.push(cursor);
          cursor += tile_w + spacing;
        }
        pos
      }
    }
    BorderImageRepeat::Repeat => {
      let end = dest_rect.x() + dest_rect.width();
      if tile_w <= 0.0 {
        return;
      }
      let count = (tiles_x.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
      let mut pos = Vec::new();
      if pos.try_reserve_exact(count).is_err() {
        paint_repeated_patch(dest);
        return;
      }
      let mut cursor = dest_rect.x();
      for _ in 0..count {
        if cursor >= end - 1e-3 {
          break;
        }
        pos.push(cursor);
        cursor += tile_w;
      }
      pos
    }
  };

  let positions_y = match repeat_y {
    BorderImageRepeat::Stretch => vec![dest_rect.y()],
    BorderImageRepeat::Round => {
      let end = dest_rect.y() + dest_rect.height();
      if tile_h <= 0.0 {
        return;
      }
      let count = (tiles_y.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
      let mut pos = Vec::new();
      if pos.try_reserve_exact(count).is_err() {
        paint_repeated_patch(dest);
        return;
      }
      let mut cursor = dest_rect.y();
      for _ in 0..count {
        if cursor >= end - 1e-3 {
          break;
        }
        pos.push(cursor);
        cursor += tile_h;
      }
      pos
    }
    BorderImageRepeat::Space => {
      if tile_h <= 0.0 {
        return;
      }
      let count = tiles_y.floor();
      if count < 2.0 {
        vec![dest_rect.y() + (dest_rect.height() - tile_h) * 0.5]
      } else {
        let spacing = (dest_rect.height() - tile_h * count) / (count - 1.0);
        let count = count as usize;
        let mut pos = Vec::new();
        if pos.try_reserve_exact(count).is_err() {
          paint_repeated_patch(dest);
          return;
        }
        let mut cursor = dest_rect.y();
        for _ in 0..count {
          pos.push(cursor);
          cursor += tile_h + spacing;
        }
        pos
      }
    }
    BorderImageRepeat::Repeat => {
      let end = dest_rect.y() + dest_rect.height();
      if tile_h <= 0.0 {
        return;
      }
      let count = (tiles_y.ceil().max(1.0) as usize).min(MAX_BORDER_IMAGE_TILES_PER_AXIS as usize);
      let mut pos = Vec::new();
      if pos.try_reserve_exact(count).is_err() {
        paint_repeated_patch(dest);
        return;
      }
      let mut cursor = dest_rect.y();
      for _ in 0..count {
        if cursor >= end - 1e-3 {
          break;
        }
        pos.push(cursor);
        cursor += tile_h;
      }
      pos
    }
  };

  for ty in positions_y.iter().copied() {
    for tx in positions_x.iter().copied() {
      let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
      let Some(intersection) = tile_rect.intersection(clip_rect) else {
        continue;
      };
      if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
        continue;
      }
      let Some(dst_rect) = SkiaRect::from_xywh(
        intersection.x(),
        intersection.y(),
        intersection.width(),
        intersection.height(),
      ) else {
        continue;
      };
      let mut paint = Paint::default();
      paint.shader = Pattern::new(
        patch_pixmap.as_ref(),
        SpreadMode::Pad,
        FilterQuality::Bilinear,
        1.0,
        Transform::from_row(scale_x, 0.0, 0.0, scale_y, tx, ty),
      );
      paint.anti_alias = false;
      paint.blend_mode = tiny_skia::BlendMode::SourceOver;
      dest.fill_rect(dst_rect, &paint, Transform::identity(), None);
    }
  }
}

#[cfg(test)]
fn apply_mask_composite(dest: &mut Mask, src: &Mask, op: MaskComposite) {
  if dest.width() != src.width() || dest.height() != src.height() {
    return;
  }

  let dest_data = dest.data_mut();
  let src_data = src.data();
  for (d, s) in dest_data.iter_mut().zip(src_data.iter()) {
    let src = *s as u16;
    let dst = *d as u16;
    let out = match op {
      MaskComposite::Add => src + dst.saturating_mul(255 - src) / 255,
      MaskComposite::Subtract => src.saturating_mul(255 - dst) / 255,
      MaskComposite::Intersect => src.saturating_mul(dst) / 255,
      MaskComposite::Exclude => {
        let src_out = src.saturating_mul(255 - dst) / 255;
        let dst_out = dst.saturating_mul(255 - src) / 255;
        src_out + dst_out
      }
    };
    *d = out.min(255) as u8;
  }
}

#[derive(Clone, Copy)]
struct BackgroundRects {
  border: Rect,
  padding: Rect,
  content: Rect,
}

fn background_rects(
  x: f32,
  y: f32,
  width: f32,
  height: f32,
  style: &ComputedStyle,
  viewport: Option<(f32, f32)>,
) -> BackgroundRects {
  let base = width.max(0.0);
  let font_size = style.font_size;
  let vp = viewport.unwrap_or((base, base));

  let border_left = resolve_length_for_paint(
    &style.used_border_left_width(),
    font_size,
    style.root_font_size,
    base,
    vp,
  );
  let border_right = resolve_length_for_paint(
    &style.used_border_right_width(),
    font_size,
    style.root_font_size,
    base,
    vp,
  );
  let border_top = resolve_length_for_paint(
    &style.used_border_top_width(),
    font_size,
    style.root_font_size,
    base,
    vp,
  );
  let border_bottom = resolve_length_for_paint(
    &style.used_border_bottom_width(),
    font_size,
    style.root_font_size,
    base,
    vp,
  );

  let padding_left = resolve_length_for_paint(
    &style.padding_left,
    font_size,
    style.root_font_size,
    base,
    vp,
  );
  let padding_right = resolve_length_for_paint(
    &style.padding_right,
    font_size,
    style.root_font_size,
    base,
    vp,
  );
  let padding_top = resolve_length_for_paint(
    &style.padding_top,
    font_size,
    style.root_font_size,
    base,
    vp,
  );
  let padding_bottom = resolve_length_for_paint(
    &style.padding_bottom,
    font_size,
    style.root_font_size,
    base,
    vp,
  );

  let border_rect = Rect::from_xywh(x, y, width, height);
  let padding_rect = inset_rect(
    border_rect,
    border_left,
    border_top,
    border_right,
    border_bottom,
  );
  let content_rect = inset_rect(
    padding_rect,
    padding_left,
    padding_top,
    padding_right,
    padding_bottom,
  );

  BackgroundRects {
    border: border_rect,
    padding: padding_rect,
    content: content_rect,
  }
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  let new_x = rect.x() + left;
  let new_y = rect.y() + top;
  let new_w = (rect.width() - left - right).max(0.0);
  let new_h = (rect.height() - top - bottom).max(0.0);
  Rect::from_xywh(new_x, new_y, new_w, new_h)
}

fn resolve_length_for_paint(
  len: &Length,
  font_size: f32,
  root_font_size: f32,
  percentage_base: f32,
  viewport: (f32, f32),
) -> f32 {
  crate::paint::paint_bounds::resolve_length_for_paint(
    len,
    font_size,
    root_font_size,
    percentage_base,
    Some(viewport),
  )
}

fn compute_background_size(
  layer: &BackgroundLayer,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
  area_w: f32,
  area_h: f32,
  img_w: f32,
  img_h: f32,
  intrinsic_ratio: Option<f32>,
) -> (f32, f32) {
  let natural_w = if img_w > 0.0 { Some(img_w) } else { None };
  let natural_h = if img_h > 0.0 { Some(img_h) } else { None };
  let ratio = intrinsic_ratio.filter(|r| r.is_finite() && *r > 0.0);

  match layer.size {
    BackgroundSize::Keyword(BackgroundSizeKeyword::Cover) => {
      let area_w = area_w.max(0.0);
      let area_h = area_h.max(0.0);
      if let Some(ratio) = ratio {
        if area_w <= 0.0 || area_h <= 0.0 {
          return (area_w, area_h);
        }
        let area_ratio = if area_h != 0.0 {
          area_w / area_h
        } else {
          f32::INFINITY
        };
        if area_ratio > ratio {
          (area_w, area_w / ratio)
        } else {
          (area_h * ratio, area_h)
        }
      } else {
        (area_w, area_h)
      }
    }
    BackgroundSize::Keyword(BackgroundSizeKeyword::Contain) => {
      let area_w = area_w.max(0.0);
      let area_h = area_h.max(0.0);
      if let Some(ratio) = ratio {
        if area_w <= 0.0 || area_h <= 0.0 {
          return (area_w, area_h);
        }
        let area_ratio = if area_h != 0.0 {
          area_w / area_h
        } else {
          f32::INFINITY
        };
        if area_ratio > ratio {
          (area_h * ratio, area_h)
        } else {
          (area_w, area_w / ratio)
        }
      } else {
        (area_w, area_h)
      }
    }
    BackgroundSize::Explicit(x, y) => {
      let resolve = |component: BackgroundSizeComponent, area: f32| -> Option<f32> {
        match component {
          BackgroundSizeComponent::Auto => None,
          BackgroundSizeComponent::Length(len) => {
            Some(resolve_length_for_paint(&len, font_size, root_font_size, area, viewport).max(0.0))
          }
        }
      };

      let resolved_x = resolve(x, area_w);
      let resolved_y = resolve(y, area_h);

      match (resolved_x, resolved_y) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
          if let Some(r) = ratio {
            (w, (w / r).max(0.0))
          } else if let Some(h) = natural_h {
            (w, h)
          } else {
            (w, area_h.max(0.0))
          }
        }
        (None, Some(h)) => {
          if let Some(r) = ratio {
            ((h * r).max(0.0), h)
          } else if let Some(w) = natural_w {
            (w, h)
          } else {
            (area_w.max(0.0), h)
          }
        }
        (None, None) => {
          if let (Some(w), Some(h)) = (natural_w, natural_h) {
            (w, h)
          } else if let Some(ratio) = ratio {
            // CSS Backgrounds 3: if an image has an intrinsic ratio but no intrinsic dimensions,
            // `background-size: auto` behaves like `contain`.
            let area_w = area_w.max(0.0);
            let area_h = area_h.max(0.0);
            if area_w <= 0.0 || area_h <= 0.0 {
              (area_w, area_h)
            } else {
              let area_ratio = if area_h != 0.0 {
                area_w / area_h
              } else {
                f32::INFINITY
              };
              if area_ratio > ratio {
                (area_h * ratio, area_h)
              } else {
                (area_w, area_w / ratio)
              }
            }
          } else {
            (area_w.max(0.0), area_h.max(0.0))
          }
        }
      }
    }
  }
}

fn resolve_background_offset(
  pos: BackgroundPosition,
  area_w: f32,
  area_h: f32,
  tile_w: f32,
  tile_h: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
) -> (f32, f32) {
  let resolve_axis =
    |comp: crate::style::types::BackgroundPositionComponent, area: f32, tile: f32| -> f32 {
      let available = area - tile;
      let offset =
        resolve_length_for_paint(&comp.offset, font_size, root_font_size, available, viewport);
      comp.alignment * available + offset
    };

  match pos {
    BackgroundPosition::Position { x, y } => {
      let x = resolve_axis(x, area_w, tile_w);
      let y = resolve_axis(y, area_h, tile_h);
      (x, y)
    }
  }
}

fn normalize_color_stops(
  stops: &[ColorStop],
  current_color: Rgba,
  gradient_length: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
  is_dark: bool,
  forced_colors: bool,
) -> Vec<(f32, Rgba)> {
  // CSS Images 3: Gradient color stop “fixup”.
  //
  // We implement the algorithm from:
  // https://www.w3.org/TR/css-images-3/#color-stop-fixup
  //
  // Notably:
  // - Stop positions can be outside the [0%, 100%] range (e.g. -50%, 150%).
  // - Unspecified stop positions are filled in by evenly spacing them between the nearest
  //   positioned stops.
  if stops.is_empty() {
    return Vec::new();
  }

  let gradient_length = if gradient_length.is_finite() && gradient_length > 0.0 {
    gradient_length
  } else {
    0.0
  };

  let mut positions: Vec<Option<f32>> = stops
    .iter()
    .map(|stop| match stop.position {
      Some(crate::css::types::ColorStopPosition::Fraction(v)) => Some(v),
      Some(crate::css::types::ColorStopPosition::Length(len)) => {
        if gradient_length <= 0.0 {
          None
        } else {
          len
            .resolve_with_context(
              Some(gradient_length),
              viewport.0,
              viewport.1,
              font_size,
              root_font_size,
            )
            .map(|px| px / gradient_length)
        }
      }
      None => None,
    })
    .collect();

  // If no stops had positions at all, the fixup algorithm reduces to evenly distributing them
  // from 0%..100%.
  if positions.iter().all(|p| p.is_none()) {
    if stops.len() == 1 {
      return vec![(
        0.0,
        stops[0]
          .color
          .to_rgba_with_scheme_and_forced_colors(current_color, is_dark, forced_colors),
      )];
    }
    let denom = (stops.len() - 1) as f32;
    return stops
      .iter()
      .enumerate()
      .map(|(i, stop)| {
        (
          i as f32 / denom,
          stop
            .color
            .to_rgba_with_scheme_and_forced_colors(current_color, is_dark, forced_colors),
        )
      })
      .collect();
  }

  // Step 1: If the first stop has no position, set it to 0%.
  if positions.first().and_then(|p| *p).is_none() {
    positions[0] = Some(0.0);
  }
  // Step 2: If the last stop has no position, set it to 100%.
  if positions.last().and_then(|p| *p).is_none() {
    if let Some(last) = positions.last_mut() {
      *last = Some(1.0);
    }
  }

  // Step 3: Ensure positioned stops are non-decreasing.
  let mut max_specified = positions[0].unwrap_or(0.0);
  for pos in positions.iter_mut().skip(1) {
    if let Some(value) = *pos {
      if value < max_specified {
        *pos = Some(max_specified);
      } else {
        max_specified = value;
      }
    }
    if let Some(value) = *pos {
      max_specified = value;
    }
  }

  // Step 4: Distribute runs of missing stops between the nearest positioned stops.
  let mut idx = 0usize;
  while idx < positions.len() {
    if positions[idx].is_some() {
      idx += 1;
      continue;
    }

    // Safe because step 1 guarantees the first entry is positioned.
    let start_idx = idx.saturating_sub(1);
    let start_pos = positions[start_idx].unwrap_or(0.0);

    let mut end_idx = idx;
    while end_idx < positions.len() && positions[end_idx].is_none() {
      end_idx += 1;
    }

    // Safe because step 2 guarantees the last entry is positioned.
    if end_idx >= positions.len() {
      break;
    }
    let end_pos = positions[end_idx].unwrap_or(start_pos);

    let span = (end_idx - start_idx) as f32;
    for offset in 1..(end_idx - start_idx) {
      let t = offset as f32 / span;
      positions[start_idx + offset] = Some(start_pos + (end_pos - start_pos) * t);
    }

    idx = end_idx + 1;
  }

  // Pair the resolved stop positions with colors.
  // (Any remaining `None`s are treated as repeating the previous used position, though the
  // algorithm above should have resolved them all.)
  let mut output = Vec::with_capacity(stops.len());
  let mut prev_pos = f32::NEG_INFINITY;
  for (stop, pos_opt) in stops.iter().zip(positions.iter()) {
    let pos = pos_opt.unwrap_or(prev_pos);
    let used_pos = if pos < prev_pos { prev_pos } else { pos };
    prev_pos = used_pos;
    output.push((
      used_pos,
      stop
        .color
        .to_rgba_with_scheme_and_forced_colors(current_color, is_dark, forced_colors),
    ));
  }

  output
}

fn normalize_color_stops_unclamped(
  stops: &[ColorStop],
  current_color: Rgba,
  gradient_length: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
  is_dark: bool,
  forced_colors: bool,
) -> Vec<(f32, Rgba)> {
  normalize_color_stops(
    stops,
    current_color,
    gradient_length,
    font_size,
    root_font_size,
    viewport,
    is_dark,
    forced_colors,
  )
}

fn radial_geometry(
  rect: Rect,
  position: &BackgroundPosition,
  size: &RadialGradientSize,
  shape: RadialGradientShape,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
) -> (f32, f32, f32, f32) {
  let (align_x, off_x, align_y, off_y) = match position {
    BackgroundPosition::Position { x, y } => {
      let ox =
        resolve_length_for_paint(&x.offset, font_size, root_font_size, rect.width(), viewport);
      let oy = resolve_length_for_paint(
        &y.offset,
        font_size,
        root_font_size,
        rect.height(),
        viewport,
      );
      (x.alignment, ox, y.alignment, oy)
    }
  };
  let cx = rect.x() + align_x * rect.width() + off_x;
  let cy = rect.y() + align_y * rect.height() + off_y;

  let dx_left = (cx - rect.x()).max(0.0);
  let dx_right = (rect.x() + rect.width() - cx).max(0.0);
  let dy_top = (cy - rect.y()).max(0.0);
  let dy_bottom = (rect.y() + rect.height() - cy).max(0.0);

  let (mut radius_x, mut radius_y) = match size {
    RadialGradientSize::ClosestSide => (dx_left.min(dx_right), dy_top.min(dy_bottom)),
    RadialGradientSize::FarthestSide => (dx_left.max(dx_right), dy_top.max(dy_bottom)),
    RadialGradientSize::ClosestCorner => {
      let corners = [
        (dx_left, dy_top),
        (dx_left, dy_bottom),
        (dx_right, dy_top),
        (dx_right, dy_bottom),
      ];
      let mut best = f32::INFINITY;
      let mut best_pair = (0.0, 0.0);
      for (dx, dy) in corners {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < best {
          best = dist;
          best_pair = (dx, dy);
        }
      }
      (
        best_pair.0 * std::f32::consts::SQRT_2,
        best_pair.1 * std::f32::consts::SQRT_2,
      )
    }
    RadialGradientSize::FarthestCorner => {
      let corners = [
        (dx_left, dy_top),
        (dx_left, dy_bottom),
        (dx_right, dy_top),
        (dx_right, dy_bottom),
      ];
      let mut best = -f32::INFINITY;
      let mut best_pair = (0.0, 0.0);
      for (dx, dy) in corners {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist > best {
          best = dist;
          best_pair = (dx, dy);
        }
      }
      (
        best_pair.0 * std::f32::consts::SQRT_2,
        best_pair.1 * std::f32::consts::SQRT_2,
      )
    }
    RadialGradientSize::Explicit { x, y } => {
      let rx =
        resolve_length_for_paint(x, font_size, root_font_size, rect.width(), viewport).max(0.0);
      let ry = y
        .as_ref()
        .map(|yy| {
          resolve_length_for_paint(yy, font_size, root_font_size, rect.height(), viewport).max(0.0)
        })
        .unwrap_or(rx);
      (rx, ry)
    }
  };

  if matches!(shape, RadialGradientShape::Circle) {
    let r = if matches!(
      size,
      RadialGradientSize::ClosestCorner | RadialGradientSize::FarthestCorner
    ) {
      // Corner-based sizes already yield isotropic radii; use hypotenuse distance instead.
      let r_corner = ((radius_x * radius_x + radius_y * radius_y) / 2.0).sqrt();
      r_corner
    } else {
      radius_x.min(radius_y)
    };
    radius_x = r;
    radius_y = r;
  }

  (cx, cy, radius_x.max(0.0), radius_y.max(0.0))
}

fn resolve_gradient_center(
  rect: Rect,
  position: &BackgroundPosition,
  paint_rect: Rect,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
) -> Point {
  let (align_x, off_x, align_y, off_y) = match position {
    BackgroundPosition::Position { x, y } => {
      let ox =
        resolve_length_for_paint(&x.offset, font_size, root_font_size, rect.width(), viewport);
      let oy = resolve_length_for_paint(
        &y.offset,
        font_size,
        root_font_size,
        rect.height(),
        viewport,
      );
      (x.alignment, ox, y.alignment, oy)
    }
  };
  let cx = rect.x() + align_x * rect.width() + off_x - paint_rect.x();
  let cy = rect.y() + align_y * rect.height() + off_y - paint_rect.y();
  Point::new(cx, cy)
}

fn gradient_stops(stops: &[(f32, Rgba)]) -> Vec<tiny_skia::GradientStop> {
  stops
    .iter()
    .map(|(pos, color)| {
      let alpha = (color.a * 255.0).round().clamp(0.0, 255.0) as u8;
      tiny_skia::GradientStop::new(
        pos.clamp(0.0, 1.0),
        tiny_skia::Color::from_rgba8(color.r, color.g, color.b, alpha),
      )
    })
    .collect()
}

fn aligned_start(origin: f32, tile: f32, clip_min: f32) -> f32 {
  if tile == 0.0 {
    return origin;
  }
  let steps = ((clip_min - origin) / tile).floor();
  origin + steps * tile
}

fn round_tile_length(area_len: f32, tile_len: f32) -> f32 {
  if tile_len == 0.0 {
    return 0.0;
  }
  let count = (area_len / tile_len).round().max(1.0);
  area_len / count
}

fn tile_positions(
  repeat: BackgroundRepeatKeyword,
  area_start: f32,
  area_len: f32,
  tile_len: f32,
  offset: f32,
  clip_min: f32,
  clip_max: f32,
) -> Vec<f32> {
  if tile_len <= 0.0 {
    return Vec::new();
  }

  match repeat {
    BackgroundRepeatKeyword::NoRepeat => vec![area_start + offset],
    BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round => {
      let start = aligned_start(area_start + offset, tile_len, clip_min);
      let mut positions = Vec::new();
      let mut pos = start;
      while pos < clip_max {
        positions.push(pos);
        pos += tile_len;
      }
      positions
    }
    BackgroundRepeatKeyword::Space => {
      let count = (area_len / tile_len).floor() as i32;
      if count >= 2 {
        let spacing = (area_len - tile_len * count as f32) / (count as f32 - 1.0);
        let step = tile_len + spacing;
        let anchor = area_start;
        let mut positions = Vec::new();
        let k = ((clip_min - anchor) / step).floor();
        let mut pos = anchor + k * step;
        while pos < clip_max {
          positions.push(pos);
          pos += step;
        }
        positions
      } else {
        // When fewer than two tiles fit, center the single tile per CSS Backgrounds 3.
        let centered = area_start + offset + (area_len - tile_len) * 0.5;
        vec![centered]
      }
    }
  }
}

/// Painting backends supported by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintBackend {
  Legacy,
  DisplayList,
}

/// Maximum amount of time the display-list builder is allowed to run before we fall back to the
/// legacy painter when operating under an overall render deadline.
///
/// The display-list pipeline is the default paint backend and is required for correctness on many
/// pages (e.g. stacking contexts for `position: fixed`). The legacy painter is maintained only as a
/// best-effort fallback when a render is about to time out, so the budget must be generous enough
/// that typical pages do not silently fall back and lose fidelity.
const DISPLAY_LIST_BUILD_BUDGET_CAP: Duration = Duration::from_secs(10);

/// Maximum amount of time to spend optimizing the display list under a render deadline.
///
/// Optimization is optional; when the budget is exceeded we rasterize the unoptimized list.
const DISPLAY_LIST_OPTIMIZE_BUDGET_CAP: Duration = Duration::from_millis(200);

/// Runtime toggle for overriding [`DISPLAY_LIST_OPTIMIZE_BUDGET_CAP`] in milliseconds.
///
/// This is primarily intended for tests that need deterministic optimizer budget timeouts without
/// using extremely tight overall paint deadlines.
const ENV_DISPLAY_LIST_OPTIMIZE_BUDGET_CAP_MS: &str = "FASTR_DISPLAY_LIST_OPTIMIZE_BUDGET_CAP_MS";

#[inline]
fn display_list_optimize_budget_cap() -> Duration {
  crate::debug::runtime::runtime_toggles()
    .u64(ENV_DISPLAY_LIST_OPTIMIZE_BUDGET_CAP_MS)
    .map(Duration::from_millis)
    .unwrap_or(DISPLAY_LIST_OPTIMIZE_BUDGET_CAP)
}

#[inline]
fn display_list_build_budget_from_remaining(remaining: Duration) -> Duration {
  (remaining / 2).min(DISPLAY_LIST_BUILD_BUDGET_CAP)
}

#[inline]
fn display_list_optimize_budget_from_remaining(remaining: Duration) -> Duration {
  (remaining / 4).min(display_list_optimize_budget_cap())
}

pub(crate) fn paint_backend_from_env() -> PaintBackend {
  // Prefer the active runtime toggles so library users (and tests) can override env-derived
  // behavior without mutating the process environment.
  let toggles = crate::debug::runtime::runtime_toggles();
  let Some(raw) = toggles.get("FASTR_PAINT_BACKEND") else {
    return PaintBackend::DisplayList;
  };
  match raw.trim().to_ascii_lowercase().as_str() {
    "display_list" | "display-list" | "displaylist" => PaintBackend::DisplayList,
    "legacy" | "immediate" => PaintBackend::Legacy,
    _ => PaintBackend::DisplayList,
  }
}

fn legacy_paint_tree_with_resources_scaled_offset(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  scale: f32,
  offset: Point,
  scroll_state: &ScrollState,
  max_iframe_depth: usize,
  trace: TraceHandle,
) -> Result<Pixmap> {
  let painter =
    Painter::with_resources_scaled(width, height, background, font_ctx, image_cache, scale)?
      .with_max_iframe_depth(max_iframe_depth)
      .with_scroll_state(scroll_state.clone())
      .with_media_provider(media_provider)
      .with_trace(trace);
  record_stage(StageHeartbeat::PaintRasterize);
  painter.paint_with_offset(tree, offset)
}

/// Paints a fragment tree via the display-list pipeline (builder → optimize → renderer).
pub fn paint_tree_display_list_with_resources_scaled_offset(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
) -> Result<Pixmap> {
  paint_tree_display_list_with_resources_scaled_offset_depth(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    scale,
    offset,
    paint_parallelism,
    scroll_state,
    crate::api::DEFAULT_MAX_IFRAME_DEPTH,
  )
}

/// Paints a fragment tree via the display-list pipeline (builder → optimize → renderer) with an
/// explicit iframe depth limit.
pub fn paint_tree_display_list_with_resources_scaled_offset_depth(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  max_iframe_depth: usize,
) -> Result<Pixmap> {
  paint_tree_display_list_with_resources_scaled_offset_depth_with_trace(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    None,
    scale,
    offset,
    paint_parallelism,
    scroll_state,
    max_iframe_depth,
    TraceHandle::disabled(),
  )
}

pub(crate) fn paint_tree_display_list_with_resources_scaled_offset_depth_with_trace(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  max_iframe_depth: usize,
  trace: TraceHandle,
) -> Result<Pixmap> {
  let _paint_span = trace.span("paint", "paint");
  let _display_list_span = trace.span("display_list_build", "paint");
  record_stage(StageHeartbeat::PaintBuild);
  check_active(RenderStage::Paint).map_err(Error::Render)?;
  let diagnostics_enabled = paint_diagnostics_enabled();
  let build_start = diagnostics_enabled.then(Instant::now);
  // Use the layout viewport for resolving viewport-relative paint units (vw/vh, etc), but cull
  // against the actual paint surface size so paged media stacked into a single pixmap (or
  // fit-to-content renders) don't drop fragments outside the first viewport.
  let viewport = tree.viewport_size();
  let culling_viewport = Size::new(width as f32, height as f32);
  let build_budget = active_deadline()
    .and_then(|deadline| deadline.remaining_timeout())
    .map(display_list_build_budget_from_remaining);
  let build_display_list_for_root =
    |root: &FragmentNode| -> Result<crate::paint::display_list::DisplayList> {
      DisplayListBuilder::with_image_cache(image_cache.clone())
        .with_font_context(font_ctx.clone())
        .with_svg_filter_defs(tree.svg_filter_defs.clone())
        .with_svg_id_defs(tree.svg_id_defs.clone())
        .with_svg_id_defs_raw(tree.svg_id_defs_raw.clone())
        .with_appearance_none_form_controls(tree.appearance_none_form_controls.clone())
        .with_scroll_state(scroll_state.clone())
        .with_media_provider(media_provider.clone())
        .with_device_pixel_ratio(scale)
        .with_parallelism(&paint_parallelism)
        .with_max_iframe_depth(max_iframe_depth)
        .with_viewport_size(viewport.width, viewport.height)
        .with_culling_viewport_size(culling_viewport.width, culling_viewport.height)
        .build_with_stacking_tree_offset_checked(root, offset)
    };
  let display_list_result = match build_budget {
    Some(budget) => {
      let cancel = active_deadline().and_then(|deadline| deadline.cancel_callback());
      let deadline = RenderDeadline::new(Some(budget), cancel);
      with_deadline(Some(&deadline), || {
        let mut display_list = build_display_list_for_root(&tree.root)?;
        for extra in &tree.additional_fragments {
          display_list.append(build_display_list_for_root(extra)?);
        }
        Ok(display_list)
      })
    }
    None => {
      let mut display_list = build_display_list_for_root(&tree.root)?;
      for extra in &tree.additional_fragments {
        display_list.append(build_display_list_for_root(extra)?);
      }
      Ok(display_list)
    }
  };
  let mut display_list = match display_list_result {
    Ok(list) => list,
    Err(err) => {
      // The display-list pipeline is faster for most pages, but builder/stacking-tree construction
      // can still be slower than legacy in pathological cases. When we're running under a render
      // deadline, fall back to the legacy painter if the builder can't complete within a small
      // slice of the remaining budget.
      if build_budget.is_some() && matches!(err, Error::Render(RenderError::Timeout { .. })) {
        // If the outer render deadline has expired/canceled, surface it instead of treating this
        // as a display-list build budget timeout.
        if let Err(err) = check_active(RenderStage::Paint) {
          return Err(Error::Render(err));
        }
        return legacy_paint_tree_with_resources_scaled_offset(
          tree,
          width,
          height,
          background,
          font_ctx,
          image_cache,
          media_provider.clone(),
          scale,
          offset,
          scroll_state,
          max_iframe_depth,
          TraceHandle::disabled(),
        );
      }
      return Err(err);
    }
  };
  if let (true, Some(start)) = (diagnostics_enabled, build_start) {
    let build_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_paint_diagnostics(|diag| {
      diag.build_ms = build_ms;
      diag.serial_ms += build_ms;
      diag.parallel_threads = diag.parallel_threads.max(1);
    });
  }

  drop(_display_list_span);
  paint_display_list_with_resources_scaled_with_trace(
    &display_list,
    width,
    height,
    background,
    font_ctx,
    scale,
    paint_parallelism,
    &trace,
  )
}

pub(crate) fn paint_tree_display_list_into_rgba_with_resources_scaled_offset_depth_with_trace(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  max_iframe_depth: usize,
  trace: TraceHandle,
  out: &mut [u8],
  stride_bytes: usize,
) -> Result<()> {
  let _paint_span = trace.span("paint", "paint");
  let _display_list_span = trace.span("display_list_build", "paint");
  record_stage(StageHeartbeat::PaintBuild);
  check_active(RenderStage::Paint).map_err(Error::Render)?;
  let diagnostics_enabled = paint_diagnostics_enabled();
  let build_start = diagnostics_enabled.then(Instant::now);
  let viewport = tree.viewport_size();
  let culling_viewport = Size::new(width as f32, height as f32);
  let build_budget = active_deadline()
    .and_then(|deadline| deadline.remaining_timeout())
    .map(display_list_build_budget_from_remaining);
  let build_display_list_for_root =
    |root: &FragmentNode| -> Result<crate::paint::display_list::DisplayList> {
      DisplayListBuilder::with_image_cache(image_cache.clone())
        .with_font_context(font_ctx.clone())
        .with_svg_filter_defs(tree.svg_filter_defs.clone())
        .with_svg_id_defs(tree.svg_id_defs.clone())
        .with_svg_id_defs_raw(tree.svg_id_defs_raw.clone())
        .with_appearance_none_form_controls(tree.appearance_none_form_controls.clone())
        .with_scroll_state(scroll_state.clone())
        .with_media_provider(media_provider.clone())
        .with_device_pixel_ratio(scale)
        .with_parallelism(&paint_parallelism)
        .with_max_iframe_depth(max_iframe_depth)
        .with_viewport_size(viewport.width, viewport.height)
        .with_culling_viewport_size(culling_viewport.width, culling_viewport.height)
        .build_with_stacking_tree_offset_checked(root, offset)
    };
  let display_list_result = match build_budget {
    Some(budget) => {
      let cancel = active_deadline().and_then(|deadline| deadline.cancel_callback());
      let deadline = RenderDeadline::new(Some(budget), cancel);
      with_deadline(Some(&deadline), || {
        let mut display_list = build_display_list_for_root(&tree.root)?;
        for extra in &tree.additional_fragments {
          display_list.append(build_display_list_for_root(extra)?);
        }
        Ok(display_list)
      })
    }
    None => {
      let mut display_list = build_display_list_for_root(&tree.root)?;
      for extra in &tree.additional_fragments {
        display_list.append(build_display_list_for_root(extra)?);
      }
      Ok(display_list)
    }
  };
  let display_list = match display_list_result {
    Ok(list) => list,
    Err(err) => {
      if build_budget.is_some() && matches!(err, Error::Render(RenderError::Timeout { .. })) {
        if let Err(err) = check_active(RenderStage::Paint) {
          return Err(Error::Render(err));
        }
        let pixmap = legacy_paint_tree_with_resources_scaled_offset(
          tree,
          width,
          height,
          background,
          font_ctx,
          image_cache,
          media_provider.clone(),
          scale,
          offset,
          scroll_state,
          max_iframe_depth,
          TraceHandle::disabled(),
        )?;
        copy_pixmap_rgba_into_strided_buffer(&pixmap, out, stride_bytes).map_err(Error::Render)?;
        return Ok(());
      }
      return Err(err);
    }
  };
  if let (true, Some(start)) = (diagnostics_enabled, build_start) {
    let build_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_paint_diagnostics(|diag| {
      diag.build_ms = build_ms;
      diag.serial_ms += build_ms;
      diag.parallel_threads = diag.parallel_threads.max(1);
    });
  }

  drop(_display_list_span);
  paint_display_list_into_rgba_with_resources_scaled_with_trace(
    &display_list,
    width,
    height,
    background,
    font_ctx,
    scale,
    paint_parallelism,
    &trace,
    out,
    stride_bytes,
  )
}

/// Optimizes and rasterizes an already-built display list.
///
/// This lets callers reuse a display list for both debugging output (e.g. pipeline snapshots) and
/// the paint stage without rebuilding it twice.
pub(crate) fn paint_display_list_with_resources_scaled_with_trace(
  display_list: &crate::paint::display_list::DisplayList,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  scale: f32,
  paint_parallelism: PaintParallelism,
  trace: &TraceHandle,
) -> Result<Pixmap> {
  Ok(
    paint_display_list_with_resources_scaled_with_trace_report(
      display_list,
      width,
      height,
      background,
      font_ctx,
      scale,
      paint_parallelism,
      trace,
    )?
    .pixmap,
  )
}

pub(crate) struct DisplayListPaintReport {
  pub pixmap: Pixmap,
  pub used_optimized_list: bool,
}

fn optimize_display_list_for_paint(
  display_list: &crate::paint::display_list::DisplayList,
  viewport_rect: Rect,
) -> Result<(Option<crate::paint::display_list::DisplayList>, crate::paint::optimize::OptimizationStats)>
{
  let optimizer = DisplayListOptimizer::new();
  let optimize_budget = active_deadline()
    .and_then(|deadline| deadline.remaining_timeout())
    .map(display_list_optimize_budget_from_remaining);
  let original_items = display_list.len();
  let optimize_result = match optimize_budget {
    Some(budget) => {
      let cancel = active_deadline().and_then(|deadline| deadline.cancel_callback());
      let deadline = RenderDeadline::new(Some(budget), cancel);
      with_deadline(Some(&deadline), || {
        optimizer.optimize_checked(display_list, viewport_rect)
      })
    }
    None => optimizer.optimize_checked(display_list, viewport_rect),
  };

  match optimize_result {
    Ok((optimized, stats)) => Ok((Some(optimized), stats)),
    Err(err) => {
      // Optimization is optional; if we hit the optimization budget, rasterize the original display
      // list (still benefiting from display-list renderer caching + tiling).
      if optimize_budget.is_some() && matches!(err, Error::Render(RenderError::Timeout { .. })) {
        if let Err(err) = check_active(RenderStage::Paint) {
          return Err(Error::Render(err));
        }
        Ok((
          None,
          crate::paint::optimize::OptimizationStats {
            original_count: original_items,
            final_count: original_items,
            ..Default::default()
          },
        ))
      } else {
        Err(err)
      }
    }
  }
}

/// Like [`paint_display_list_with_resources_scaled_with_trace`], but also reports which display
/// list variant was rasterized (optimized vs original).
pub(crate) fn paint_display_list_with_resources_scaled_with_trace_report(
  display_list: &crate::paint::display_list::DisplayList,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  scale: f32,
  paint_parallelism: PaintParallelism,
  trace: &TraceHandle,
) -> Result<DisplayListPaintReport> {
  let diagnostics_enabled = paint_diagnostics_enabled();

  let _optimize_span = trace.span("display_list_optimize", "paint");
  let viewport_rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let optimize_start = diagnostics_enabled.then(Instant::now);
  let (optimized, stats) = optimize_display_list_for_paint(display_list, viewport_rect)?;
  let used_optimized_list = optimized.is_some();

  let list_to_render = optimized.as_ref().unwrap_or(display_list);

  if let (true, Some(start)) = (diagnostics_enabled, optimize_start) {
    let optimize_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_paint_diagnostics(|diag| {
      diag.optimize_ms = optimize_ms;
      diag.optimize_original_items = stats.original_count;
      diag.optimize_final_items = stats.final_count;
      diag.optimize_culled = stats.culled_count;
      diag.optimize_transparent_removed = stats.transparent_removed;
      diag.optimize_noop_removed = stats.noop_removed;
      diag.optimize_merged = stats.merged_count;
      diag.serial_ms += optimize_ms;
      diag.parallel_threads = diag.parallel_threads.max(1);
    });
  }

  drop(_optimize_span);
  let _raster_span = trace.span("rasterize", "paint");
  let mut renderer = DisplayListRenderer::new_scaled(width, height, background, font_ctx, scale)?;
  renderer.set_parallelism(paint_parallelism);
  record_stage(StageHeartbeat::PaintRasterize);
  let report = renderer.render_with_report(list_to_render)?;

  if paint_diagnostics_enabled() {
    with_paint_diagnostics(|diag| {
      diag.raster_ms = report.duration.as_secs_f64() * 1000.0;
      diag.command_count = list_to_render.len();
      diag.gradient_ms = report.gradient_stats.millis();
      diag.gradient_pixels = report.gradient_stats.pixels;
      diag.gradient_pixmap_cache_hits = diag
        .gradient_pixmap_cache_hits
        .saturating_add(report.gradient_pixmap_cache_hits);
      diag.gradient_pixmap_cache_misses = diag
        .gradient_pixmap_cache_misses
        .saturating_add(report.gradient_pixmap_cache_misses);
      diag.gradient_pixmap_cache_bytes = diag
        .gradient_pixmap_cache_bytes
        .max(report.gradient_pixmap_cache_bytes);
      diag.image_pixmap_cache_hits += report.image_pixmap_cache_hits;
      diag.image_pixmap_cache_misses += report.image_pixmap_cache_misses;
      diag.image_pixmap_ms += report.image_pixmap_ms;
      diag.background_ms += report.background_ms;
      diag.clip_mask_calls += report.clip_mask_calls;
      diag.clip_mask_ms += report.clip_mask_ms;
      diag.clip_mask_pixels += report.clip_mask_pixels;
      diag.layer_allocations += report.layer_allocations;
      diag.layer_alloc_bytes += report.layer_alloc_bytes;
      diag.backdrop_composite_allocations += report.backdrop_composite_allocations;
      diag.backdrop_composite_bytes += report.backdrop_composite_bytes;
      diag.backdrop_composite_cache_hits += report.backdrop_composite_cache_hits;
      diag.backdrop_composite_cache_misses += report.backdrop_composite_cache_misses;
      diag.parallel_tasks += report.parallel_tasks;
      diag.parallel_threads = diag.parallel_threads.max(report.parallel_threads);
      diag.parallel_fallback_reason = report.fallback_reason.clone();
      diag.parallel_ms += report.parallel_duration.as_secs_f64() * 1000.0;
      diag.serial_ms += report.serial_duration.as_secs_f64() * 1000.0;
    });
  }

  Ok(DisplayListPaintReport {
    pixmap: report.pixmap,
    used_optimized_list,
  })
}

/// Display-list input variants accepted by partial repaint helpers.
pub(crate) enum PartialRepaintDisplayList<'a> {
  /// A single list reference, with an explicit marker describing whether it has already been
  /// optimized.
  Single {
    display_list: &'a crate::paint::display_list::DisplayList,
    already_optimized: bool,
  },
  /// Both the original list and an optional optimized list produced by the standard pipeline.
  ///
  /// When `optimized` is `None`, the caller indicates that optimization was skipped (e.g. due to
  /// deadline budgeting) and the original list must be used for rasterization.
  OriginalAndOptimized {
    original: &'a crate::paint::display_list::DisplayList,
    optimized: Option<&'a crate::paint::display_list::DisplayList>,
  },
}

pub(crate) struct PartialRepaintReport {
  pub used_optimized_list: bool,
}

/// Repaints a sub-rectangle of an existing pixmap using a display list slice derived from the same
/// optimize-stage semantics as full paint.
///
/// This is a low-level helper for incremental rendering. It overwrites only the pixels covered by
/// `dirty_rect_css` (in CSS coordinates), leaving the rest of `pixmap` untouched.
pub(crate) fn repaint_display_list_region_with_resources_scaled_with_trace(
  lists: PartialRepaintDisplayList<'_>,
  pixmap: &mut Pixmap,
  dirty_rect_css: Rect,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  scale: f32,
  trace: &TraceHandle,
) -> Result<PartialRepaintReport> {
  let viewport_rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let mut optimized_owned: Option<crate::paint::display_list::DisplayList> = None;
  let (list_to_render, used_optimized_list) = match lists {
    PartialRepaintDisplayList::Single {
      display_list,
      already_optimized,
    } => {
      if already_optimized {
        (display_list, true)
      } else {
        let (optimized, _stats) = optimize_display_list_for_paint(display_list, viewport_rect)?;
        optimized_owned = optimized;
        let list = optimized_owned.as_ref().unwrap_or(display_list);
        (list, optimized_owned.is_some())
      }
    }
    PartialRepaintDisplayList::OriginalAndOptimized { original, optimized } => {
      (optimized.unwrap_or(original), optimized.is_some())
    }
  };

  // Convert the dirty rectangle to device pixel bounds and clamp to the destination pixmap.
  let device_w = pixmap.width() as i64;
  let device_h = pixmap.height() as i64;
  if device_w <= 0 || device_h <= 0 {
    return Ok(PartialRepaintReport { used_optimized_list });
  }
  if dirty_rect_css.width() <= 0.0 || dirty_rect_css.height() <= 0.0 {
    return Ok(PartialRepaintReport { used_optimized_list });
  }
  let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };

  let x0 = (dirty_rect_css.min_x() * scale).floor() as i64;
  let y0 = (dirty_rect_css.min_y() * scale).floor() as i64;
  let x1 = (dirty_rect_css.max_x() * scale).ceil() as i64;
  let y1 = (dirty_rect_css.max_y() * scale).ceil() as i64;

  let clamp_i64 = |v: i64, min: i64, max: i64| v.max(min).min(max);
  let x0 = clamp_i64(x0, 0, device_w);
  let y0 = clamp_i64(y0, 0, device_h);
  let x1 = clamp_i64(x1, 0, device_w);
  let y1 = clamp_i64(y1, 0, device_h);
  let region_w = (x1 - x0).max(0) as u32;
  let region_h = (y1 - y0).max(0) as u32;
  if region_w == 0 || region_h == 0 {
    return Ok(PartialRepaintReport { used_optimized_list });
  }

  // Render a tile covering the dirty region. We currently use a zero halo; callers that need
  // filter/AA padding should inflate `dirty_rect_css` before calling this helper.
  let render_x = x0 as u32;
  let render_y = y0 as u32;
  let render_w = region_w;
  let render_h = region_h;

  let _partial_span = trace.span("display_list_partial_repaint", "paint");
  let tile = crate::paint::display_list_renderer::render_display_list_tile(
    list_to_render,
    render_x,
    render_y,
    render_w,
    render_h,
    background,
    font_ctx,
    scale,
  )?;

  // Blit the rendered region back into the destination pixmap.
  let dest_stride = pixmap.width() as usize * 4;
  let src_stride = tile.width() as usize * 4;
  let copy_bytes = region_w as usize * 4;
  const PARTIAL_REPAINT_DEADLINE_STRIDE: usize = 256;
  let mut deadline_counter = 0usize;
  for row in 0..region_h as usize {
    check_active_periodic(
      &mut deadline_counter,
      PARTIAL_REPAINT_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
      .map_err(Error::Render)?;
    let dest_offset = (render_y as usize + row) * dest_stride + render_x as usize * 4;
    let src_offset = row * src_stride;
    pixmap.data_mut()[dest_offset..dest_offset + copy_bytes]
      .copy_from_slice(&tile.data()[src_offset..src_offset + copy_bytes]);
  }

  Ok(PartialRepaintReport { used_optimized_list })
}

/// Result of painting a frame into multiple ordered layers interleaved with remote iframe slots.
///
/// Composition order is:
/// `layers[0] -> slot[0] -> layers[1] -> slot[1] -> ... -> layers[N]`
#[derive(Debug)]
pub struct LayeredPaintResult {
  /// Parent frame layers in paint order. Each pixmap is an RGBA (premultiplied) surface with a
  /// transparent initial backdrop.
  pub layers: Vec<Pixmap>,
  /// Remote iframe slots encountered in paint order.
  pub slots: Vec<crate::paint::display_list::RemoteFrameSlotItem>,
}

#[derive(Debug)]
struct LayeredDisplayListPlan {
  layers: Vec<crate::paint::display_list::DisplayList>,
  slots: Vec<crate::paint::display_list::RemoteFrameSlotItem>,
}

fn split_display_list_for_remote_iframe_slots(
  display_list: &crate::paint::display_list::DisplayList,
) -> Result<LayeredDisplayListPlan> {
  use crate::paint::display_list::DisplayItem;

  fn is_push(item: &DisplayItem) -> bool {
    matches!(
      item,
      DisplayItem::PushClip(_)
        | DisplayItem::PushOpacity(_)
        | DisplayItem::PushTransform(_)
        | DisplayItem::PushBlendMode(_)
        | DisplayItem::PushStackingContext(_)
        | DisplayItem::PushBackfaceVisibility(_)
    )
  }

  fn pop_for_push(push: &DisplayItem) -> DisplayItem {
    match push {
      DisplayItem::PushClip(_) => DisplayItem::PopClip,
      DisplayItem::PushOpacity(_) => DisplayItem::PopOpacity,
      DisplayItem::PushTransform(_) => DisplayItem::PopTransform,
      DisplayItem::PushBlendMode(_) => DisplayItem::PopBlendMode,
      DisplayItem::PushStackingContext(_) => DisplayItem::PopStackingContext,
      DisplayItem::PushBackfaceVisibility(_) => DisplayItem::PopBackfaceVisibility,
      _ => {
        debug_assert!(false, "unexpected non-push item in stack");
        DisplayItem::PopClip
      }
    }
  }

  fn pop_matches_push(pop: &DisplayItem, push: &DisplayItem) -> bool {
    matches!(
      (pop, push),
      (DisplayItem::PopClip, DisplayItem::PushClip(_))
        | (DisplayItem::PopOpacity, DisplayItem::PushOpacity(_))
        | (DisplayItem::PopTransform, DisplayItem::PushTransform(_))
        | (DisplayItem::PopBlendMode, DisplayItem::PushBlendMode(_))
        | (DisplayItem::PopStackingContext, DisplayItem::PushStackingContext(_))
        | (DisplayItem::PopBackfaceVisibility, DisplayItem::PushBackfaceVisibility(_))
    )
  }

  let mut layers: Vec<crate::paint::display_list::DisplayList> = Vec::new();
  let mut slots: Vec<crate::paint::display_list::RemoteFrameSlotItem> = Vec::new();

  // Stack of currently-active push items at the current scan position.
  let mut active_stack: Vec<DisplayItem> = Vec::new();
  let mut current_items: Vec<DisplayItem> = Vec::new();
  let mut next_slot_index: u32 = 0;

  fn append_stack_pops(items: &mut Vec<DisplayItem>, active_stack: &[DisplayItem]) {
    for push in active_stack.iter().rev() {
      items.push(pop_for_push(push));
    }
  }

  fn start_layer_with_prefix(items: &mut Vec<DisplayItem>, active_stack: &[DisplayItem]) {
    items.extend(active_stack.iter().cloned());
  }

  start_layer_with_prefix(&mut current_items, &active_stack);

  for item in display_list.items().iter() {
    match item {
      DisplayItem::RemoteFrameSlot(slot) => {
        append_stack_pops(&mut current_items, &active_stack);
        layers.push(display_list.with_items(std::mem::take(&mut current_items)));

        let mut slot = slot.clone();
        slot.slot_index = next_slot_index;
        next_slot_index = next_slot_index.saturating_add(1);
        slots.push(slot);

        // Begin a new layer after the slot.
        start_layer_with_prefix(&mut current_items, &active_stack);
      }
      DisplayItem::PopClip
      | DisplayItem::PopOpacity
      | DisplayItem::PopTransform
      | DisplayItem::PopBlendMode
      | DisplayItem::PopStackingContext
      | DisplayItem::PopBackfaceVisibility => {
        current_items.push(item.clone());
        let Some(push) = active_stack.pop() else {
          return Err(Error::Render(RenderError::PaintFailed {
            operation: "remote iframe layer split: stack underflow".to_string(),
          }));
        };
        if !pop_matches_push(item, &push) {
          return Err(Error::Render(RenderError::PaintFailed {
            operation: "remote iframe layer split: stack mismatch".to_string(),
          }));
        }
      }
      other => {
        current_items.push(other.clone());
        if is_push(other) {
          active_stack.push(other.clone());
        }
      }
    }
  }

  append_stack_pops(&mut current_items, &active_stack);
  layers.push(display_list.with_items(current_items));

  Ok(LayeredDisplayListPlan { layers, slots })
}

/// Paints a fragment tree via the display-list pipeline, returning ordered layers split around
/// out-of-process iframe slots.
///
/// Each returned layer pixmap is cleared to transparent before rendering so the browser compositor
/// can blend them together (interleaving remote child frame surfaces) while preserving paint order.
pub fn paint_tree_display_list_layered_with_resources_scaled_offset_depth(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  max_iframe_depth: usize,
) -> Result<LayeredPaintResult> {
  record_stage(StageHeartbeat::PaintBuild);
  check_active(RenderStage::Paint).map_err(Error::Render)?;

  let viewport = tree.viewport_size();
  let culling_viewport = Size::new(width as f32, height as f32);

  let mut display_list = DisplayListBuilder::with_image_cache(image_cache.clone())
    .with_font_context(font_ctx.clone())
    .with_svg_filter_defs(tree.svg_filter_defs.clone())
    .with_svg_id_defs(tree.svg_id_defs.clone())
    .with_svg_id_defs_raw(tree.svg_id_defs_raw.clone())
    .with_appearance_none_form_controls(tree.appearance_none_form_controls.clone())
    .with_scroll_state(scroll_state.clone())
    .with_device_pixel_ratio(scale)
    .with_parallelism(&paint_parallelism)
    .with_max_iframe_depth(max_iframe_depth)
    .with_viewport_size(viewport.width, viewport.height)
    .with_culling_viewport_size(culling_viewport.width, culling_viewport.height)
    .build_with_stacking_tree_offset_checked(&tree.root, offset)?;
  for extra in &tree.additional_fragments {
    let extra_list = DisplayListBuilder::with_image_cache(image_cache.clone())
      .with_font_context(font_ctx.clone())
      .with_svg_filter_defs(tree.svg_filter_defs.clone())
      .with_svg_id_defs(tree.svg_id_defs.clone())
      .with_svg_id_defs_raw(tree.svg_id_defs_raw.clone())
      .with_appearance_none_form_controls(tree.appearance_none_form_controls.clone())
      .with_scroll_state(scroll_state.clone())
      .with_device_pixel_ratio(scale)
      .with_parallelism(&paint_parallelism)
      .with_max_iframe_depth(max_iframe_depth)
      .with_viewport_size(viewport.width, viewport.height)
      .with_culling_viewport_size(culling_viewport.width, culling_viewport.height)
      .build_with_stacking_tree_offset_checked(extra, offset)?;
    display_list.append(extra_list);
  }

  // Optimize the full list once, then split into layers.
  let optimizer = DisplayListOptimizer::new();
  let viewport_rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let list_to_render = match optimizer.optimize_checked(&display_list, viewport_rect) {
    Ok((optimized, _stats)) => optimized,
    Err(_) => display_list,
  };

  let plan = split_display_list_for_remote_iframe_slots(&list_to_render)?;

  record_stage(StageHeartbeat::PaintRasterize);
  let mut layers = Vec::with_capacity(plan.layers.len());
  for layer_list in plan.layers {
    let mut renderer =
      DisplayListRenderer::new_scaled(width, height, Rgba::TRANSPARENT, font_ctx.clone(), scale)?;
    renderer.set_parallelism(paint_parallelism);
    layers.push(renderer.render_with_report(&layer_list)?.pixmap);
  }

  Ok(LayeredPaintResult {
    layers,
    slots: plan.slots,
  })
}

/// Optimizes and rasterizes an already-built display list directly into an externally-owned RGBA8
/// buffer.
pub(crate) fn paint_display_list_into_rgba_with_resources_scaled_with_trace(
  display_list: &crate::paint::display_list::DisplayList,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  scale: f32,
  paint_parallelism: PaintParallelism,
  trace: &TraceHandle,
  out: &mut [u8],
  stride_bytes: usize,
) -> Result<()> {
  let diagnostics_enabled = paint_diagnostics_enabled();

  let _optimize_span = trace.span("display_list_optimize", "paint");
  let optimizer = DisplayListOptimizer::new();
  let viewport_rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let optimize_start = diagnostics_enabled.then(Instant::now);
  let optimize_budget = active_deadline()
    .and_then(|deadline| deadline.remaining_timeout())
    .map(display_list_optimize_budget_from_remaining);
  let original_items = display_list.len();
  let optimize_result = match optimize_budget {
    Some(budget) => {
      let cancel = active_deadline().and_then(|deadline| deadline.cancel_callback());
      let deadline = RenderDeadline::new(Some(budget), cancel);
      with_deadline(Some(&deadline), || optimizer.optimize_checked(display_list, viewport_rect))
    }
    None => optimizer.optimize_checked(display_list, viewport_rect),
  };

  let (optimized, stats) = match optimize_result {
    Ok((optimized, stats)) => (Some(optimized), stats),
    Err(err) => {
      // Optimization is optional; if we hit the optimization budget, rasterize the original display
      // list.
      if optimize_budget.is_some() && matches!(err, Error::Render(RenderError::Timeout { .. })) {
        if let Err(err) = check_active(RenderStage::Paint) {
          return Err(Error::Render(err));
        }
        (
          None,
          crate::paint::optimize::OptimizationStats {
            original_count: original_items,
            final_count: original_items,
            ..Default::default()
          },
        )
      } else {
        return Err(err);
      }
    }
  };

  let list_to_render = optimized.as_ref().unwrap_or(display_list);

  if let (true, Some(start)) = (diagnostics_enabled, optimize_start) {
    let optimize_ms = start.elapsed().as_secs_f64() * 1000.0;
    with_paint_diagnostics(|diag| {
      diag.optimize_ms = optimize_ms;
      diag.optimize_original_items = stats.original_count;
      diag.optimize_final_items = stats.final_count;
      diag.optimize_culled = stats.culled_count;
      diag.optimize_transparent_removed = stats.transparent_removed;
      diag.optimize_noop_removed = stats.noop_removed;
      diag.optimize_merged = stats.merged_count;
      diag.serial_ms += optimize_ms;
      diag.parallel_threads = diag.parallel_threads.max(1);
    });
  }

  drop(_optimize_span);
  let _raster_span = trace.span("rasterize", "paint");

  let mut renderer = DisplayListRenderer::new_scaled_into_rgba_buffer(
    width,
    height,
    background,
    font_ctx,
    scale,
    out,
    stride_bytes,
  )?;
  // `DisplayListRenderer::render_into` is currently a serial path; disable tile-parallelism to keep
  // behavior aligned with what it executes today.
  renderer.set_parallelism(PaintParallelism::disabled());
  record_stage(StageHeartbeat::PaintRasterize);
  let start = Instant::now();
  renderer.render_into(list_to_render)?;

  if paint_diagnostics_enabled() {
    with_paint_diagnostics(|diag| {
      diag.raster_ms += start.elapsed().as_secs_f64() * 1000.0;
      diag.command_count = list_to_render.len();
    });
  }

  // Preserve the signature of `paint_display_list_with_resources_scaled_with_trace` by leaving
  // diagnostics accumulation to the caller; for external buffers we do not currently return a full
  // `RenderReport`.
  let _ = paint_parallelism;
  Ok(())
}

/// Paints a fragment tree via the display-list pipeline using explicit resources.
pub fn paint_tree_display_list_with_resources(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
) -> Result<Pixmap> {
  paint_tree_display_list_with_resources_scaled_offset(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    paint_parallelism,
    scroll_state,
  )
}

/// Paints a fragment tree via the display-list pipeline with default resources.
pub fn paint_tree_display_list(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
) -> Result<Pixmap> {
  paint_tree_display_list_with_resources_scaled_offset(
    tree,
    width,
    height,
    background,
    FontContext::new(),
    ImageCache::new(),
    1.0,
    Point::ZERO,
    paint_parallelism,
    scroll_state,
  )
}

/// Paints a fragment tree at a device scale with an additional translation applied.
pub fn paint_tree_with_resources_scaled_offset(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
) -> Result<Pixmap> {
  paint_tree_with_resources_scaled_offset_backend(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    scale,
    offset,
    paint_parallelism,
    scroll_state,
    paint_backend_from_env(),
  )
}

/// Paints a fragment tree at a device scale with an additional translation applied using the
/// selected backend.
pub fn paint_tree_with_resources_scaled_offset_backend(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  backend: PaintBackend,
) -> Result<Pixmap> {
  paint_tree_with_resources_scaled_offset_backend_with_iframe_depth(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    None,
    scale,
    offset,
    paint_parallelism,
    scroll_state,
    backend,
    crate::api::DEFAULT_MAX_IFRAME_DEPTH,
  )
}

pub(crate) fn paint_tree_with_resources_scaled_offset_backend_with_iframe_depth(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  scale: f32,
  offset: Point,
  paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  backend: PaintBackend,
  max_iframe_depth: usize,
) -> Result<Pixmap> {
  match backend {
    PaintBackend::Legacy => legacy_paint_tree_with_resources_scaled_offset(
      tree,
      width,
      height,
      background,
      font_ctx,
      image_cache,
      media_provider,
      scale,
      offset,
      scroll_state,
      max_iframe_depth,
      TraceHandle::disabled(),
    ),
    PaintBackend::DisplayList => paint_tree_display_list_with_resources_scaled_offset_depth_with_trace(
      tree,
      width,
      height,
      background,
      font_ctx,
      image_cache,
      media_provider,
      scale,
      offset,
      paint_parallelism,
      scroll_state,
      max_iframe_depth,
      TraceHandle::disabled(),
    ),
  }
}

/// Paints a fragment tree using explicit resources at the provided device scale.
pub fn paint_tree_with_resources_scaled(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
) -> Result<Pixmap> {
  paint_tree_with_resources_scaled_offset(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    scale,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
  )
}

/// Paints a fragment tree using provided font and image resources.
pub fn paint_tree_with_resources(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
) -> Result<Pixmap> {
  paint_tree_with_resources_scaled(tree, width, height, background, font_ctx, image_cache, 1.0)
}

/// Paints a fragment tree at the given device scale.
pub fn paint_tree_scaled(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  scale: f32,
) -> Result<Pixmap> {
  paint_tree_with_resources_scaled(
    tree,
    width,
    height,
    background,
    FontContext::new(),
    ImageCache::new(),
    scale,
  )
}

/// Paints a fragment tree to a pixmap
///
/// This is the main entry point for painting.
pub fn paint_tree(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
) -> Result<Pixmap> {
  paint_tree_scaled(tree, width, height, background, 1.0)
}

/// Paints a fragment tree with tracing enabled.
pub(crate) fn paint_tree_with_resources_scaled_offset_with_trace(
  tree: &FragmentTree,
  width: u32,
  height: u32,
  background: Rgba,
  font_ctx: FontContext,
  image_cache: ImageCache,
  scale: f32,
  offset: Point,
  _paint_parallelism: PaintParallelism,
  scroll_state: &ScrollState,
  max_iframe_depth: usize,
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  trace: TraceHandle,
) -> Result<Pixmap> {
  legacy_paint_tree_with_resources_scaled_offset(
    tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    media_provider,
    scale,
    offset,
    scroll_state,
    max_iframe_depth,
    trace,
  )
}

/// Scales a pixmap by the given device pixel ratio, returning a new pixmap.
/// This is a coarse fallback for high-DPI outputs; painting should ideally
/// happen directly at device resolution.
pub fn scale_pixmap_for_dpr(pixmap: Pixmap, dpr: f32) -> Result<Pixmap> {
  let dpr = if dpr.is_finite() && dpr > 0.0 {
    dpr
  } else {
    1.0
  };
  if (dpr - 1.0).abs() < f32::EPSILON {
    return Ok(pixmap);
  }

  let new_w = (((pixmap.width() as f32) * dpr).round()).max(1.0) as u32;
  let new_h = (((pixmap.height() as f32) * dpr).round()).max(1.0) as u32;
  let mut target = new_pixmap(new_w, new_h).ok_or_else(|| RenderError::InvalidParameters {
    message: "Failed to allocate scaled pixmap".to_string(),
  })?;

  let mut paint = PixmapPaint::default();
  paint.quality = FilterQuality::Bilinear;
  let transform = Transform::from_scale(dpr, dpr);
  target.draw_pixmap(0, 0, pixmap.as_ref(), &paint, transform, None);
  Ok(target)
}

#[derive(Copy, Clone)]
struct BorderWidths {
  top: f32,
  right: f32,
  bottom: f32,
  left: f32,
}

fn resolve_slice_value(value: crate::style::types::BorderImageSliceValue, axis_len: u32) -> f32 {
  match value {
    crate::style::types::BorderImageSliceValue::Number(n) => n.max(0.0),
    crate::style::types::BorderImageSliceValue::Percentage(p) => (p / 100.0) * axis_len as f32,
  }
}

fn resolve_border_image_widths(
  widths: &crate::style::types::BorderImageWidth,
  border: BorderWidths,
  box_width: f32,
  box_height: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
) -> BorderWidths {
  let resolve_single = |value: BorderImageWidthValue, border: f32, axis: f32| -> f32 {
    match value {
      BorderImageWidthValue::Auto => border,
      BorderImageWidthValue::Number(n) => (n * border).max(0.0),
      BorderImageWidthValue::Length(len) => {
        resolve_length_for_paint(&len, font_size, root_font_size, axis, viewport).max(0.0)
      }
      BorderImageWidthValue::Percentage(p) => ((p / 100.0) * axis).max(0.0),
    }
  };

  BorderWidths {
    top: resolve_single(widths.top, border.top, box_height),
    right: resolve_single(widths.right, border.right, box_width),
    bottom: resolve_single(widths.bottom, border.bottom, box_height),
    left: resolve_single(widths.left, border.left, box_width),
  }
}

fn resolve_border_image_outset(
  outset: &crate::style::types::BorderImageOutset,
  border: BorderWidths,
  font_size: f32,
  root_font_size: f32,
  viewport: (f32, f32),
) -> BorderWidths {
  fn resolve_single(
    value: BorderImageOutsetValue,
    border: f32,
    font_size: f32,
    root_font_size: f32,
    viewport: (f32, f32),
  ) -> f32 {
    match value {
      BorderImageOutsetValue::Number(n) => (n * border).max(0.0),
      BorderImageOutsetValue::Length(len) => {
        resolve_length_for_paint(&len, font_size, root_font_size, border.max(1.0), viewport)
          .max(0.0)
      }
    }
  }

  BorderWidths {
    top: resolve_single(outset.top, border.top, font_size, root_font_size, viewport),
    right: resolve_single(
      outset.right,
      border.right,
      font_size,
      root_font_size,
      viewport,
    ),
    bottom: resolve_single(
      outset.bottom,
      border.bottom,
      font_size,
      root_font_size,
      viewport,
    ),
    left: resolve_single(
      outset.left,
      border.left,
      font_size,
      root_font_size,
      viewport,
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::ColorStop;
  use crate::css::types::ColorStopPosition;
  use crate::css::types::TextShadow;
  use crate::geometry::Rect;
  use crate::image_loader::ImageCache;
  use crate::paint::display_list::BorderRadii;
  use crate::paint::pixmap::NewPixmapAllocRecorder;
  use crate::style::types::BackgroundAttachment;
  use crate::style::types::BackgroundBox;
  use crate::style::types::BackgroundImage;
  use crate::style::types::BackgroundImageUrl;
  use crate::style::types::BackgroundPosition;
  use crate::style::types::BackgroundPositionComponent;
  use crate::style::types::BackgroundRepeat;
  use crate::style::types::BackgroundSize;
  use crate::style::types::BackgroundSizeComponent;
  use crate::style::types::BorderImage;
  use crate::style::types::BorderImageRepeat;
  use crate::style::types::BorderImageSlice;
  use crate::style::types::BorderImageSliceValue;
  use crate::style::types::BorderImageSource;
  use crate::style::types::ClipPath;
  use crate::style::types::FilterShadow;
  use crate::style::types::ImageRendering;
  use crate::style::types::Isolation;
  use crate::style::types::MaskClip;
  use crate::style::types::MaskComposite;
  use crate::style::types::MaskLayer;
  use crate::style::types::MaskMode;
  use crate::style::types::MaskOrigin;
  use crate::style::types::MixBlendMode;
  use crate::style::types::OutlineColor;
  use crate::style::types::OutlineStyle;
  use crate::style::types::Overflow;
  use crate::style::types::ShapeRadius;
  use crate::style::types::TextDecorationThickness;
  use crate::style::types::TransformBox;
  use crate::style::values::Length;
  use crate::style::ComputedStyle;
  use crate::text::font_loader::FontContext;
  use crate::tree::box_tree::CrossOriginAttribute;
  use crate::tree::box_tree::ForeignObjectInfo;
  use crate::tree::box_tree::ImageDecodingAttribute;
  use crate::tree::box_tree::SrcsetCandidate;
  use crate::tree::box_tree::SrcsetDescriptor;
  use crate::Position;
  use base64::Engine;
  use image::codecs::png::PngEncoder;
  use image::ExtendedColorType;
  use image::ImageEncoder;
  use image::RgbaImage;
  use std::sync::Arc;

  #[test]
  fn text_cache_key_canonicalizes_negative_zero_font_size() {
    let key = TextCacheKey::new(1, 0.0, "hello");
    let key_neg = TextCacheKey::new(1, -0.0, "hello");
    assert_eq!(key, key_neg);
  }

  #[test]
  fn remote_iframe_layer_split_preserves_display_list_metadata() {
    let mut list = crate::paint::display_list::DisplayList::from_items(vec![
      crate::paint::display_list::DisplayItem::FillRect(crate::paint::display_list::FillRectItem {
        rect: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
        color: Rgba::WHITE,
      }),
      crate::paint::display_list::DisplayItem::RemoteFrameSlot(
        crate::paint::display_list::RemoteFrameSlotItem {
          slot_index: 123,
          src: "https://example.com".to_string(),
          rect: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
          clip: None,
        },
      ),
      crate::paint::display_list::DisplayItem::FillRect(crate::paint::display_list::FillRectItem {
        rect: Rect::from_xywh(10.0, 10.0, 5.0, 5.0),
        color: Rgba::WHITE,
      }),
    ]);
    list.mark_has_scroll_linked_animations();
    list.set_has_gif_images(true);
    list.set_has_animation_time_dependent_images(true);

    let plan = split_display_list_for_remote_iframe_slots(&list).expect("split plan");
    assert_eq!(plan.slots.len(), 1);
    assert_eq!(plan.slots[0].slot_index, 0);
    assert_eq!(plan.layers.len(), 2);

    for layer in plan.layers {
      assert!(layer.has_scroll_linked_animations());
      assert!(layer.has_gif_images());
      assert!(layer.has_animation_time_dependent_images());
      assert!(!layer.items().iter().any(|item| matches!(
        item,
        crate::paint::display_list::DisplayItem::RemoteFrameSlot(_)
      )));
    }
  }

  #[test]
  fn normalize_color_stops_preserves_out_of_range_positions() {
    let stops = vec![
      ColorStop {
        color: Color::parse("red").expect("color"),
        position: Some(ColorStopPosition::Fraction(-0.5)),
      },
      ColorStop {
        color: Color::parse("blue").expect("color"),
        position: Some(ColorStopPosition::Fraction(1.5)),
      },
    ];

    let normalized = normalize_color_stops(
      &stops,
      Rgba::BLACK,
      100.0,
      16.0,
      16.0,
      (100.0, 100.0),
      false,
      false,
    );

    assert_eq!(normalized.len(), 2);
    assert!((normalized[0].0 + 0.5).abs() < 1e-6);
    assert!((normalized[1].0 - 1.5).abs() < 1e-6);
  }

  #[test]
  fn normalize_color_stops_fills_missing_positions_between_specified_stops() {
    let stops = vec![
      ColorStop {
        color: Color::parse("red").expect("color"),
        position: Some(ColorStopPosition::Fraction(0.0)),
      },
      ColorStop {
        color: Color::parse("green").expect("color"),
        position: Some(ColorStopPosition::Fraction(0.5)),
      },
      ColorStop {
        color: Color::parse("blue").expect("color"),
        position: None,
      },
      ColorStop {
        color: Color::parse("black").expect("color"),
        position: Some(ColorStopPosition::Fraction(0.75)),
      },
      ColorStop {
        color: Color::parse("white").expect("color"),
        position: None,
      },
    ];

    let normalized = normalize_color_stops(
      &stops,
      Rgba::BLACK,
      100.0,
      16.0,
      16.0,
      (100.0, 100.0),
      false,
      false,
    );

    let positions: Vec<f32> = normalized.iter().map(|(pos, _)| *pos).collect();
    assert_eq!(positions.len(), 5);
    assert!((positions[0] - 0.0).abs() < 1e-6);
    assert!((positions[1] - 0.5).abs() < 1e-6);
    assert!((positions[2] - 0.625).abs() < 1e-6);
    assert!((positions[3] - 0.75).abs() < 1e-6);
    assert!((positions[4] - 1.0).abs() < 1e-6);
  }

  #[test]
  fn non_ascii_whitespace_measure_alt_text_does_not_trim_nbsp() {
    let font_ctx = FontContext::with_config(crate::text::font_db::FontConfig::bundled_only());
    let painter =
      Painter::with_resources_scaled(10, 10, Rgba::TRANSPARENT, font_ctx, ImageCache::new(), 1.0)
        .expect("painter");
    let style = ComputedStyle::default();
    assert!(painter.measure_alt_text(" ", &style).is_none());
    assert!(painter.measure_alt_text("\u{00A0}", &style).is_some());
  }

  #[test]
  fn non_ascii_whitespace_paint_svg_does_not_trim_nbsp_prefix() {
    #[derive(Clone)]
    struct CountingFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl crate::resource::ResourceFetcher for CountingFetcher {
      fn fetch(&self, url: &str) -> crate::error::Result<crate::resource::FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(crate::error::Error::Other(format!(
          "unexpected fetch attempt for {url}"
        )))
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CountingFetcher {
      calls: Arc::clone(&calls),
    };
    let image_cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let font_ctx = FontContext::with_config(crate::text::font_db::FontConfig::bundled_only());
    let mut painter =
      Painter::with_resources_scaled(10, 10, Rgba::TRANSPARENT, font_ctx, image_cache, 1.0)
        .expect("painter");

    let svg = "\u{00A0}<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"></svg>";
    let painted = painter.paint_svg(svg, None, 0.0, 0.0, 10.0, 10.0, None);
    assert!(
      !painted,
      "expected SVG paint to treat NBSP-prefixed markup as a URL (and fail without a fetch)"
    );
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "NBSP must not be treated as whitespace when detecting inline SVG markup"
    );
  }

  #[test]
  fn painter_resolve_length_for_paint_resolves_rem_against_root_font_size() {
    let len = Length::rem(1.0);
    let resolved = resolve_length_for_paint(&len, 10.0, 20.0, 0.0, (100.0, 100.0));
    assert!((resolved - 20.0).abs() < 1e-6);
  }

  #[test]
  fn painter_resolve_length_for_paint_does_not_fallback_to_element_font_size_for_rem() {
    let len = Length::rem(1.0);
    let resolved = resolve_length_for_paint(&len, 10.0, f32::NAN, 0.0, (100.0, 100.0));
    assert_eq!(resolved, 0.0);
  }

  #[test]
  fn opacity_layer_applies_clip_mask_inside_layer() {
    let mut painter = Painter::new(4, 4, Rgba::TRANSPARENT).expect("painter");
    painter
      .pixmap
      .fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));

    let mut mask = Mask::new(4, 4).expect("mask");
    mask.data_mut().fill(0);
    for y in 1..=2u32 {
      for x in 1..=2u32 {
        mask.data_mut()[(y * 4 + x) as usize] = 128;
      }
    }

    let bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);
    painter.paint_with_opacity_layer(0.5, bounds, Some(&mask), |layer, clip| {
      let rect = layer.device_rect(bounds);
      let sk_rect =
        SkiaRect::from_xywh(rect.x(), rect.y(), rect.width(), rect.height()).expect("rect");
      let path = PathBuilder::from_rect(sk_rect);

      let mut blue = Paint::default();
      blue.set_color_rgba8(0, 0, 255, 255);
      blue.anti_alias = false;
      layer.pixmap.fill_path(
        &path,
        &blue,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip,
      );

      let mut red = Paint::default();
      red.set_color_rgba8(255, 0, 0, 255);
      red.anti_alias = false;
      layer.pixmap.fill_path(
        &path,
        &red,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        clip,
      );
    });

    let px = painter.pixmap.pixel(1, 1).expect("pixel");
    assert_eq!(px.green(), 0);
    assert!(
      px.blue() > 0,
      "expected blue channel to remain visible when clipping occurs within the opacity layer; got rgba=({},{},{},{})",
      px.red(),
      px.green(),
      px.blue(),
      px.alpha()
    );
    assert!(
      px.alpha() > 80,
      "expected alpha to reflect single clip application (not squared); got rgba=({},{},{},{})",
      px.red(),
      px.green(),
      px.blue(),
      px.alpha()
    );
  }

  #[test]
  fn foreign_object_image_tag_dedupes_opacity_attribute() {
    let foreign = ForeignObjectInfo {
      placeholder: String::new(),
      attributes: vec![
        ("opacity".to_string(), "0.9".to_string()),
        ("id".to_string(), "foo".to_string()),
      ],
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 0.5,
      background: None,
      html: String::new(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Visible,
    };

    let output = crate::paint::svg_foreign_object::foreign_object_image_tag(
      &foreign,
      "data:image/png;base64,abc",
      0,
      Rect::from_xywh(foreign.x, foreign.y, foreign.width, foreign.height),
    )
    .expect("foreignObject image tag");
    assert_eq!(output.match_indices("opacity=").count(), 1);
  }

  #[test]
  fn foreign_object_image_tag_preserves_opacity_attribute_when_computed_is_default() {
    let foreign = ForeignObjectInfo {
      placeholder: String::new(),
      attributes: vec![
        ("opacity".to_string(), "0.9".to_string()),
        ("id".to_string(), "foo".to_string()),
      ],
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: String::new(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Visible,
    };

    let output = crate::paint::svg_foreign_object::foreign_object_image_tag(
      &foreign,
      "data:image/png;base64,abc",
      0,
      Rect::from_xywh(foreign.x, foreign.y, foreign.width, foreign.height),
    )
    .expect("foreignObject image tag");
    assert_eq!(output.match_indices("opacity=").count(), 1);
    assert!(
      output.contains("opacity=\"0.9\""),
      "expected output to forward the original opacity attribute, got {output:?}"
    );
    assert!(output.contains("id=\"foo\""));
  }

  #[test]
  fn foreign_object_image_tag_preserves_clip_path_attribute_when_overflow_visible() {
    let foreign = ForeignObjectInfo {
      placeholder: String::new(),
      attributes: vec![
        ("clip-path".to_string(), "url(#foo)".to_string()),
        ("id".to_string(), "bar".to_string()),
      ],
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: String::new(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Visible,
    };

    let output = crate::paint::svg_foreign_object::foreign_object_image_tag(
      &foreign,
      "data:image/png;base64,abc",
      0,
      Rect::from_xywh(foreign.x, foreign.y, foreign.width, foreign.height),
    )
    .expect("foreignObject image tag");
    assert!(
      !output.contains("<g"),
      "visible overflow should not introduce wrapper groups"
    );
    assert_eq!(output.match_indices("clip-path=").count(), 1);
    assert!(output.contains("clip-path=\"url(#foo)\""));
  }

  #[test]
  fn foreign_object_image_tag_applies_clip_path_attribute_on_wrapper_group_when_overflow_clips() {
    let foreign = ForeignObjectInfo {
      placeholder: String::new(),
      attributes: vec![
        ("clip-path".to_string(), "url(#foo)".to_string()),
        ("id".to_string(), "bar".to_string()),
      ],
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: String::new(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Hidden,
      overflow_y: Overflow::Hidden,
    };

    let output = crate::paint::svg_foreign_object::foreign_object_image_tag(
      &foreign,
      "data:image/png;base64,abc",
      0,
      Rect::from_xywh(foreign.x, foreign.y, foreign.width, foreign.height),
    )
    .expect("foreignObject image tag");
    assert!(output.contains("<g clip-path=\"url(#foo)\">"));
    let clip_prefix = "<clipPath id=\"";
    let clip_start = output.find(clip_prefix).expect("clipPath start") + clip_prefix.len();
    let clip_end = clip_start + output[clip_start..].find('"').expect("clipPath id end");
    let clip_id = &output[clip_start..clip_end];
    assert!(
      output.contains(&format!("<image clip-path=\"url(#{clip_id})\"")),
      "expected injected <image> to reference generated clipPath id {clip_id:?}, got {output:?}"
    );
    assert_eq!(output.match_indices("url(#foo)").count(), 1);
    assert_eq!(output.match_indices("clip-path=").count(), 2);
    let image_tag = output
      .split("<image")
      .nth(1)
      .and_then(|rest| rest.split('>').next())
      .expect("image tag present");
    assert_eq!(image_tag.match_indices("clip-path=").count(), 1);
  }

  #[test]
  fn foreign_object_image_tag_extends_clip_rect_for_visible_x_with_filter() {
    let foreign = ForeignObjectInfo {
      placeholder: String::new(),
      attributes: vec![("filter".to_string(), "url(#blur)".to_string())],
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: String::new(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Clip,
    };

    let output = crate::paint::svg_foreign_object::foreign_object_image_tag(
      &foreign,
      "data:image/png;base64,abc",
      0,
      Rect::from_xywh(foreign.x, foreign.y, foreign.width, foreign.height),
    )
    .expect("foreignObject image tag");
    assert!(
      output
        .contains("<rect x=\"-1.000000\" y=\"0.000000\" width=\"3.000000\" height=\"1.000000\""),
      "expected clip rect to extend in x for visible overflow, got {output:?}"
    );
  }

  #[test]
  fn foreign_object_image_tag_extends_clip_rect_for_visible_y_with_filter() {
    let foreign = ForeignObjectInfo {
      placeholder: String::new(),
      attributes: vec![("filter".to_string(), "url(#blur)".to_string())],
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: String::new(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Clip,
      overflow_y: Overflow::Visible,
    };

    let output = crate::paint::svg_foreign_object::foreign_object_image_tag(
      &foreign,
      "data:image/png;base64,abc",
      0,
      Rect::from_xywh(foreign.x, foreign.y, foreign.width, foreign.height),
    )
    .expect("foreignObject image tag");
    assert!(
      output
        .contains("<rect x=\"0.000000\" y=\"-1.000000\" width=\"1.000000\" height=\"3.000000\""),
      "expected clip rect to extend in y for visible overflow, got {output:?}"
    );
  }

  fn make_empty_tree() -> FragmentTree {
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);
    FragmentTree::new(root)
  }

  fn red_svg() -> String {
    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"><rect width=\"1\" height=\"1\" fill=\"red\"/></svg>"
            .to_string()
  }

  fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let px = pixmap.pixel(x, y).expect("pixel in bounds");
    let a = px.alpha();
    if a == 0 {
      return (0, 0, 0, 0);
    }
    let r = ((px.red() as u16 * 255) / a as u16) as u8;
    let g = ((px.green() as u16 * 255) / a as u16) as u8;
    let b = ((px.blue() as u16 * 255) / a as u16) as u8;
    (r, g, b, a)
  }

  fn hue_delta(a: f32, b: f32) -> f32 {
    let d = (a - b).abs();
    d.min(1.0 - d)
  }

  fn assert_hsl_components(
    actual: (u8, u8, u8),
    expected_hsl: (f32, f32, f32),
    tol_h: f32,
    tol_s: f32,
    tol_l: f32,
    context: &str,
  ) {
    let (h, s, l) = rgb_to_hsl(
      actual.0 as f32 / 255.0,
      actual.1 as f32 / 255.0,
      actual.2 as f32 / 255.0,
    );
    assert!(
      hue_delta(h, expected_hsl.0) <= tol_h
        && (s - expected_hsl.1).abs() <= tol_s
        && (l - expected_hsl.2).abs() <= tol_l,
      "{context}: expected hsl {:?}, got hsl ({h:.3},{s:.3},{l:.3})",
      expected_hsl
    );
  }

  fn bounding_box_for_color(
    pixmap: &Pixmap,
    predicate: impl Fn((u8, u8, u8, u8)) -> bool,
  ) -> Option<(u32, u32, u32, u32)> {
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;

    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        let color = color_at(pixmap, x, y);
        if predicate(color) {
          min_x = min_x.min(x);
          min_y = min_y.min(y);
          max_x = max_x.max(x);
          max_y = max_y.max(y);
        }
      }
    }

    if min_x == u32::MAX {
      None
    } else {
      Some((min_x, min_y, max_x, max_y))
    }
  }

  fn downsample_half(pixmap: Pixmap) -> Pixmap {
    scale_pixmap_for_dpr(pixmap, 0.5).expect("downsample pixmap")
  }

  fn mean_abs_diff(a: &Pixmap, b: &Pixmap) -> f32 {
    assert_eq!(a.width(), b.width());
    assert_eq!(a.height(), b.height());

    let mut total: u64 = 0;
    let mut count: u64 = 0;
    for (pa, pb) in a.data().chunks_exact(4).zip(b.data().chunks_exact(4)) {
      for i in 0..4 {
        total += pa[i].abs_diff(pb[i]) as u64;
        count += 1;
      }
    }
    total as f32 / count as f32
  }

  fn two_color_data_url() -> String {
    let pixels = vec![
      255, 0, 0, 255, // red
      0, 0, 255, 255, // blue
    ];
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
      .write_image(&pixels, 2, 1, ExtendedColorType::Rgba8)
      .expect("encode png");
    format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(buf)
    )
  }

  #[test]
  fn embedded_import_fetcher_decodes_unpadded_base64_data_url() {
    use crate::css::types::CssImportLoader;

    #[derive(Clone)]
    struct PanicFetcher;
    impl crate::resource::ResourceFetcher for PanicFetcher {
      fn fetch(&self, url: &str) -> crate::error::Result<crate::resource::FetchedResource> {
        panic!("unexpected fetch attempt for {url}");
      }
    }

    let fetcher = EmbeddedImportFetcher::new(
      None,
      Arc::new(PanicFetcher),
      crate::resource::ReferrerPolicy::default(),
    );
    let url = "data:text/css;base64,Ym9k eXtjb2xv\ncjpyZWQ7fQ";
    let css = fetcher.load(url).expect("load data url");
    assert_eq!(css, "body{color:red;}");
  }

  #[test]
  fn embedded_import_fetcher_rejects_oversized_file_urls() {
    use crate::css::types::CssImportLoader;

    #[derive(Clone)]
    struct LargeCssFetcher;
    impl crate::resource::ResourceFetcher for LargeCssFetcher {
      fn fetch(&self, _url: &str) -> crate::error::Result<crate::resource::FetchedResource> {
        Ok(crate::resource::FetchedResource::new(
          vec![b'a'; MAX_IMPORTED_CSS_BYTES + 1],
          Some("text/css".to_string()),
        ))
      }
    }

    let fetcher = EmbeddedImportFetcher::new(
      None,
      Arc::new(LargeCssFetcher),
      crate::resource::ReferrerPolicy::default(),
    );
    assert!(fetcher.load("file:///tmp/large.css").is_err());
  }

  #[test]
  fn embedded_import_fetcher_imports_go_through_resource_fetcher() {
    #[derive(Clone)]
    struct RecordingFetcher {
      requests: Arc<Mutex<Vec<(String, Option<String>)>>>,
    }

    impl crate::resource::ResourceFetcher for RecordingFetcher {
      fn fetch(&self, url: &str) -> crate::error::Result<crate::resource::FetchedResource> {
        self.fetch_with_request(crate::resource::FetchRequest::new(
          url,
          crate::resource::FetchDestination::Style,
        ))
      }

      fn fetch_with_request(
        &self,
        req: crate::resource::FetchRequest<'_>,
      ) -> crate::error::Result<crate::resource::FetchedResource> {
        self
          .requests
          .lock()
          .unwrap()
          .push((req.url.to_string(), req.referrer_url.map(|s| s.to_string())));
        Ok(crate::resource::FetchedResource::new(
          b"p { color: blue; }".to_vec(),
          Some("text/css".to_string()),
        ))
      }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let loader = EmbeddedImportFetcher::new(
      None,
      Arc::new(RecordingFetcher {
        requests: Arc::clone(&requests),
      }),
      crate::resource::ReferrerPolicy::default(),
    );

    let sheet = crate::css::parser::parse_stylesheet("@import \"imported.css\";")
      .expect("parse stylesheet");
    let media = crate::style::media::MediaContext::screen(800.0, 600.0);
    let _resolved = sheet
      .resolve_imports(&loader, Some("https://example.com/base.css"), &media)
      .expect("resolve imports");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "https://example.com/imported.css");
    assert_eq!(requests[0].1.as_deref(), Some("https://example.com/base.css"));
  }

  #[test]
  fn image_rendering_crisp_edges_uses_nearest_filter_quality() {
    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::CrispEdges;
    assert_eq!(
      Painter::filter_quality_for_image(Some(&style)),
      FilterQuality::Nearest
    );
  }

  #[test]
  fn background_image_rendering_pixelated_uses_nearest_sampling() {
    let url = two_color_data_url();

    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::Pixelated;
    style.background_color = Rgba::WHITE;
    style.background_layers = smallvec::smallvec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(5.0)),
        BackgroundSizeComponent::Length(Length::px(1.0)),
      ),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }];

    let fragment =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 5.0, 1.0), vec![], Arc::new(style));
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 5, 1, Rgba::WHITE).expect("paint");

    // Pixelated sampling should keep the left half fully red and the right half blue without purple blending.
    assert_eq!(color_at(&pixmap, 1, 0), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 3, 0), (0, 0, 255, 255));
  }

  #[test]
  fn background_image_rendering_crisp_edges_uses_nearest_sampling() {
    let url = two_color_data_url();

    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::CrispEdges;
    style.background_color = Rgba::WHITE;
    style.background_layers = smallvec::smallvec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(5.0)),
        BackgroundSizeComponent::Length(Length::px(1.0)),
      ),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }];

    let fragment =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 5.0, 1.0), vec![], Arc::new(style));
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 5, 1, Rgba::WHITE).expect("paint");

    // Crisp edges should also use nearest-neighbor sampling when upscaling backgrounds.
    assert_eq!(color_at(&pixmap, 1, 0), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 3, 0), (0, 0, 255, 255));
  }

  #[test]
  fn stacking_order_places_floats_between_blocks_and_inlines() {
    // Build four children that should paint in block → float → inline → positioned order,
    // regardless of tree order between blocks and floats.
    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    block_style.background_color = Rgba::RED;
    let mut block = FragmentNode::new_block(Rect::from_xywh(10.0, 0.0, 5.0, 5.0), vec![]);
    block.style = Some(Arc::new(block_style));

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = crate::style::float::Float::Left;
    float_style.background_color = Rgba::GREEN;
    let mut float_frag = FragmentNode::new_block(Rect::from_xywh(20.0, 0.0, 5.0, 5.0), vec![]);
    float_frag.style = Some(Arc::new(float_style));

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    inline_style.background_color = Rgba::BLUE;
    let mut inline =
      FragmentNode::new_text(Rect::from_xywh(30.0, 0.0, 5.0, 5.0), "x".to_string(), 12.0);
    inline.style = Some(Arc::new(inline_style));

    let mut positioned_style = ComputedStyle::default();
    positioned_style.display = Display::Block;
    positioned_style.position = Position::Relative;
    positioned_style.background_color = Rgba::WHITE;
    let mut positioned = FragmentNode::new_block(Rect::from_xywh(40.0, 0.0, 5.0, 5.0), vec![]);
    positioned.style = Some(Arc::new(positioned_style));

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
      vec![float_frag, block, inline, positioned],
    );

    let painter = Painter::new(100, 10, Rgba::WHITE).expect("painter");
    let mut commands = Vec::new();
    let mut svg_filters = SvgFilterResolver::new(None, vec![&root], None);
    painter
      .collect_stacking_context(
        &root,
        Point::ZERO,
        None,
        true,
        false,
        Point::ZERO,
        false,
        RootPaintOptions {
          use_root_background: false,
          extend_background_to_viewport: false,
        },
        &mut commands,
        &mut svg_filters,
      )
      .expect("stacking context collection should succeed");

    // Background commands occur in paint order; filter child backgrounds to check ordering.
    let xs: Vec<f32> = commands
      .iter()
      .filter_map(|cmd| match cmd {
        DisplayCommand::Background { rect, .. } if rect.width() == 5.0 => Some(rect.x()),
        _ => None,
      })
      .collect();

    assert_eq!(xs, vec![10.0, 20.0, 30.0, 40.0]);
  }

  #[test]
  fn test_painter_creation() {
    let painter = Painter::new(100, 100, Rgba::WHITE);
    assert!(painter.is_ok());
  }

  #[test]
  fn test_paint_empty_tree() {
    let tree = make_empty_tree();
    let result = paint_tree(&tree, 100, 100, Rgba::WHITE);
    assert!(result.is_ok());

    let pixmap = result.unwrap();
    assert_eq!(pixmap.width(), 100);
    assert_eq!(pixmap.height(), 100);
  }

  #[test]
  fn test_paint_with_text() {
    let text_fragment = FragmentNode::new_text(
      Rect::from_xywh(10.0, 10.0, 50.0, 16.0),
      "Hello".to_string(),
      12.0,
    );
    let root =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![text_fragment]);
    let tree = FragmentTree::new(root);

    let result = paint_tree(&tree, 100, 100, Rgba::WHITE);
    assert!(result.is_ok());
  }

  #[test]
  fn test_paint_nested_fragments() {
    let inner = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 30.0, 30.0), vec![]);
    let outer = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), vec![inner]);
    let tree = FragmentTree::new(outer);

    let result = paint_tree(&tree, 100, 100, Rgba::WHITE);
    assert!(result.is_ok());
  }

  #[test]
  fn test_background_white() {
    let tree = make_empty_tree();
    let result = paint_tree(&tree, 10, 10, Rgba::WHITE);
    assert!(result.is_ok());

    let pixmap = result.unwrap();
    let data = pixmap.data();
    // tiny-skia stores premultiplied RGBA bytes
    // WHITE in RGBA is (255, 255, 255, 255)
    assert_eq!(data[0], 255); // R
    assert_eq!(data[1], 255); // G
    assert_eq!(data[2], 255); // B
    assert_eq!(data[3], 255); // A
  }

  #[test]
  fn text_decoration_metrics_available() {
    let painter = Painter::new(10, 10, Rgba::WHITE).expect("painter");
    let style = ComputedStyle::default();
    let metrics = painter.decoration_metrics(None, &style);
    assert!(metrics.is_some());
  }

  #[test]
  fn decoration_metrics_handle_variable_font_variations() {
    let painter = Painter::new(10, 10, Rgba::WHITE).expect("painter");
    let mut style = ComputedStyle::default();
    style.font_size = 14.0;

    let font_bytes = Arc::new(include_bytes!("../../tests/fonts/RobotoFlex-VF.ttf").to_vec());
    let font = Arc::new(crate::text::font_db::LoadedFont {
      id: None,
      data: font_bytes.clone(),
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "Roboto Flex".to_string(),
      weight: crate::text::font_db::FontWeight::NORMAL,
      style: crate::text::font_db::FontStyle::Normal,
      stretch: crate::text::font_db::FontStretch::Normal,
    });

    let variations = vec![
      rustybuzz::Variation {
        tag: ttf_parser::Tag::from_bytes(b"wght"),
        value: 720.0,
      },
      rustybuzz::Variation {
        tag: ttf_parser::Tag::from_bytes(b"wdth"),
        value: 95.0,
      },
    ];

    let run = ShapedRun {
      text: "x".to_string(),
      start: 0,
      end: 1,
      glyphs: Vec::new(),
      direction: TextDirection::LeftToRight,
      level: 0,
      advance: 0.0,
      font: font.clone(),
      font_size: style.font_size,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: variations.clone(),
      scale: 1.0,
    };

    let metrics = painter
      .decoration_metrics(Some(&[run]), &style)
      .expect("metrics");

    let coords: Vec<_> = variations.iter().map(|v| (v.tag, v.value)).collect();
    let font_metrics =
      crate::text::font_db::FontMetrics::from_data_with_variations(&font.data, font.index, &coords)
        .expect("font metrics");
    let scale = style.font_size / font_metrics.units_per_em as f32;
    let expected_underline_pos = font_metrics.underline_position as f32 * scale;
    let expected_underline_thickness = (font_metrics.underline_thickness as f32 * scale).max(1.0);

    assert!((metrics.underline_pos - expected_underline_pos).abs() < 0.0001);
    assert!((metrics.underline_thickness - expected_underline_thickness).abs() < 0.0001);
  }

  #[test]
  fn text_shadow_offsets_are_painted() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 16.0;
    style.text_shadow = vec![TextShadow {
      offset_x: Length::px(4.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      color: Some(Rgba::from_rgba8(255, 0, 0, 255)),
    }]
    .into();
    let style = Arc::new(style);

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(10.0, 10.0, 80.0, 30.0),
      "Hi".to_string(),
      16.0,
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 60.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 120, 60, Rgba::WHITE).expect("paint");

    let black_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r < 32 && g < 32 && b < 32)
        .expect("black text");
    let red_bbox = bounding_box_for_color(&pixmap, |(r, g, b, _)| {
      let (r, g, b) = (r as u16, g as u16, b as u16);
      r > g + 20 && r > b + 20 && !(r < 40 && g < 40 && b < 40) && (g < 250 || b < 250)
    })
    .expect("shadow");

    assert!(
      red_bbox.0 > black_bbox.0 + 2,
      "shadow should be offset to the right of the glyphs"
    );
    assert!(
      red_bbox.1.abs_diff(black_bbox.1) <= 2,
      "shadow should align vertically when no y offset is set"
    );
  }

  #[test]
  fn text_shadow_offset_scales_with_device_pixel_ratio() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 16.0;
    style.text_shadow = vec![TextShadow {
      offset_x: Length::px(2.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      color: Some(Rgba::from_rgba8(255, 0, 0, 255)),
    }]
    .into();
    let style = Arc::new(style);

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(10.0, 10.0, 80.0, 30.0),
      "Hi".to_string(),
      16.0,
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 60.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree_scaled(&tree, 120, 60, Rgba::WHITE, 2.0).expect("paint");

    let black_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r < 32 && g < 32 && b < 32)
        .expect("black text");
    let shadow_pixels = pixmap
      .data()
      .chunks_exact(4)
      .filter(|px| px[3] > 0 && px[0] > px[1] && px[0] > px[2])
      .count();
    assert!(shadow_pixels > 0, "expected shadow pixels to be present");

    let red_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > g && r > b).expect("shadow");

    let dx = red_bbox.0.saturating_sub(black_bbox.0);
    assert!(
      (3..=5).contains(&dx),
      "shadow should shift ~4 device px (2 CSS px at 2x), got {dx}"
    );
    assert!(
      red_bbox.1.abs_diff(black_bbox.1) <= 2,
      "shadow should align vertically when no y offset is set"
    );
  }

  #[test]
  fn text_shadow_blur_scales_with_device_pixel_ratio() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 16.0;
    style.text_shadow = vec![TextShadow {
      offset_x: Length::px(0.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(4.0),
      color: Some(Rgba::from_rgba8(255, 0, 0, 255)),
    }]
    .into();
    let style = Arc::new(style);

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(10.0, 10.0, 80.0, 30.0),
      "Hi".to_string(),
      16.0,
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 60.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree_scaled(&tree, 120, 60, Rgba::TRANSPARENT, 2.0).expect("paint");

    let black_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r < 32 && g < 32 && b < 32)
        .expect("black text");
    let shadow_pixels = pixmap
      .data()
      .chunks_exact(4)
      .filter(|px| px[3] > 0 && px[0] > px[1] && px[0] > px[2])
      .count();
    assert!(shadow_pixels > 0, "expected shadow pixels to be present");

    let red_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > g && r > b).expect("shadow");

    let mut outside = 0;
    let width = pixmap.width();
    for y in 0..pixmap.height() {
      for x in 0..width {
        if x >= black_bbox.0 && x <= black_bbox.2 && y >= black_bbox.1 && y <= black_bbox.3 {
          continue;
        }
        let (r, g, b, a) = color_at(&pixmap, x, y);
        if a > 0 && r > g && r > b {
          outside += 1;
        }
      }
    }
    assert!(
            outside > 0,
            "blur should paint shadow pixels outside the glyph bounds (black bbox {:?}, red bbox {:?}, shadow pixels {})",
            black_bbox,
            red_bbox,
            shadow_pixels
        );
  }

  #[test]
  fn perspective_transform_is_consistent_across_device_scale() {
    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    child_style.perspective = Some(Length::px(320.0));
    child_style.transform = vec![
      crate::css::types::Transform::RotateY(30.0),
      crate::css::types::Transform::Translate(Length::px(8.0), Length::px(0.0)),
    ];
    let child_style = Arc::new(child_style);

    let mut root_style = ComputedStyle::default();
    root_style.background_color = Rgba::WHITE;
    let root_style = Arc::new(root_style);

    let child =
      FragmentNode::new_block_styled(Rect::from_xywh(20.0, 12.0, 36.0, 28.0), vec![], child_style);
    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 80.0, 60.0),
      vec![child],
      root_style,
    );
    let tree = FragmentTree::new(root);

    let baseline = paint_tree_scaled(&tree, 80, 60, Rgba::WHITE, 1.0).expect("paint 1x");
    let hidpi = paint_tree_scaled(&tree, 80, 60, Rgba::WHITE, 2.0).expect("paint 2x");
    let hidpi_down = downsample_half(hidpi);

    let red_predicate = |(r, g, b, a): (u8, u8, u8, u8)| {
      a > 0 && (r as u16) > (g as u16 + 10) && (r as u16) > (b as u16 + 10)
    };
    let bbox_base = bounding_box_for_color(&baseline, red_predicate).expect("baseline bbox");
    let bbox_down = bounding_box_for_color(&hidpi_down, red_predicate).expect("hidpi bbox");

    assert!(
      bbox_base.0.abs_diff(bbox_down.0) <= 1
        && bbox_base.1.abs_diff(bbox_down.1) <= 1
        && bbox_base.2.abs_diff(bbox_down.2) <= 1
        && bbox_base.3.abs_diff(bbox_down.3) <= 1,
      "expected similar projected bounds: {:?} vs {:?}",
      bbox_base,
      bbox_down
    );

    let center_x = (bbox_base.0 + bbox_base.2) / 2;
    let center_y = (bbox_base.1 + bbox_base.3) / 2;
    let center_base = color_at(&baseline, center_x, center_y);
    let center_down = color_at(&hidpi_down, center_x, center_y);
    let max_center_delta = center_base
      .0
      .abs_diff(center_down.0)
      .max(center_base.1.abs_diff(center_down.1))
      .max(center_base.2.abs_diff(center_down.2));
    assert!(
      max_center_delta <= 4,
      "center color should remain consistent after downsampling (base {:?}, hidpi {:?})",
      center_base,
      center_down
    );

    let avg_diff = mean_abs_diff(&baseline, &hidpi_down);
    assert!(
      avg_diff <= 3.0,
      "overall difference should stay small after downsampling, got {avg_diff}"
    );
  }

  #[test]
  fn text_shadow_resolves_percent_and_em_units() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 20.0;
    style.text_shadow = vec![TextShadow {
      offset_x: Length::percent(50.0), // 10px
      offset_y: Length::em(1.0),       // 20px
      blur_radius: Length::px(0.0),
      color: Some(Rgba::from_rgba8(255, 0, 0, 255)),
    }]
    .into();
    let style = Arc::new(style);

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 80.0, 60.0),
      "Hi".to_string(),
      20.0,
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 80.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 120, 100, Rgba::WHITE).expect("paint");

    let black_bbox = bounding_box_for_color(&pixmap, |(r, g, b, a)| {
      a > 0 && r.abs_diff(g) <= 10 && r.abs_diff(b) <= 10 && r < 96 && g < 96 && b < 96
    })
    .expect("black text");
    let red_bbox = bounding_box_for_color(&pixmap, |(r, g, b, a)| {
      a > 0 && r > 32 && r > g.saturating_add(20) && r > b.saturating_add(20)
    })
    .expect("shadow");

    let dx = red_bbox.0 as i32 - black_bbox.0 as i32;
    let dy = red_bbox.1 as i32 - black_bbox.1 as i32;
    assert!(
      (9..=11).contains(&dx),
      "percent offset_x should resolve to ~10px (got {dx})"
    );
    assert!(
      (19..=21).contains(&dy),
      "1em offset_y should resolve to ~20px (got {dy})"
    );
  }

  #[test]
  fn marker_text_shadow_is_painted() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.color = Rgba::BLACK;
    style.font_size = 16.0;
    style.text_shadow = vec![TextShadow {
      offset_x: Length::px(3.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      color: Some(Rgba::from_rgba8(255, 0, 0, 255)),
    }]
    .into();
    let style = Arc::new(style);

    let marker = FragmentNode::new_with_style(
      Rect::from_xywh(10.0, 10.0, 20.0, 20.0),
      FragmentContent::Text {
        text: "•".to_string().into(),
        box_id: None,
        source_range: None,
        baseline_offset: 16.0,
        shaped: None,
        is_marker: true,
        emphasis_offset: Default::default(),
        document_selection: None,
      },
      vec![],
      style,
    );

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 40.0, 30.0), vec![marker]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 60, 40, Rgba::WHITE).expect("paint");

    let glyph_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r < 32 && g < 32 && b < 32)
        .expect("marker glyph");
    let shadow_bbox = bounding_box_for_color(&pixmap, |(r, g, b, _)| {
      let (r, g, b) = (r as u16, g as u16, b as u16);
      r > g + 20 && r > b + 20
    })
    .expect("marker shadow");

    assert!(
      shadow_bbox.0 > glyph_bbox.0,
      "shadow should render to the inline end of the marker glyph"
    );
    assert!(
      shadow_bbox.1.abs_diff(glyph_bbox.1) <= 2,
      "shadow should stay vertically aligned with the marker glyph"
    );
  }

  #[test]
  fn filter_lengths_resolve_viewport_units() {
    let mut style = ComputedStyle::default();
    style.filter = vec![FilterFunction::Blur(Length::new(10.0, LengthUnit::Vw))];
    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = resolve_filters(
      &style.filter,
      &style,
      (200.0, 100.0),
      &FontContext::new(),
      &mut resolver,
    );
    match filters.first() {
      Some(ResolvedFilter::Blur(radius)) => assert!((radius - 20.0).abs() < 0.001),
      other => panic!("expected blur filter, got {:?}", other),
    }
  }

  #[test]
  fn filter_lengths_ignore_percentages() {
    let mut style = ComputedStyle::default();
    style.filter = vec![FilterFunction::Blur(Length::percent(50.0))];
    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = resolve_filters(
      &style.filter,
      &style,
      (200.0, 100.0),
      &FontContext::new(),
      &mut resolver,
    );
    assert!(filters.is_empty(), "percentage blur should be discarded");
  }

  #[test]
  fn filter_lengths_resolve_ex_units() {
    let mut style = ComputedStyle::default();
    style.font_size = 20.0;
    style.filter = vec![FilterFunction::Blur(Length::new(1.0, LengthUnit::Ex))];
    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = resolve_filters(
      &style.filter,
      &style,
      (200.0, 100.0),
      &FontContext::new(),
      &mut resolver,
    );
    match filters.first() {
      Some(ResolvedFilter::Blur(radius)) => assert!(
        (radius - 10.0).abs() < 2.0,
        "expected ex to resolve near half the font size (got {radius})"
      ),
      other => panic!("expected blur filter, got {:?}", other),
    }
  }

  #[test]
  fn negative_blur_lengths_resolve_to_no_filter() {
    let mut style = ComputedStyle::default();
    style.filter = vec![FilterFunction::Blur(Length::px(-2.0))];
    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = resolve_filters(
      &style.filter,
      &style,
      (200.0, 100.0),
      &FontContext::new(),
      &mut resolver,
    );
    assert!(filters.is_empty(), "negative blur should drop the filter");
  }

  #[test]
  fn drop_shadow_negative_spread_reduces_outset() {
    let filters = vec![ResolvedFilter::DropShadow {
      offset_x: 0.0,
      offset_y: 0.0,
      blur_radius: 4.0,
      spread: -2.0,
      color: Rgba::BLACK,
    }];
    let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    let with_zero_spread = vec![ResolvedFilter::DropShadow {
      offset_x: 0.0,
      offset_y: 0.0,
      blur_radius: 4.0,
      spread: 0.0,
      color: Rgba::BLACK,
    }];
    let (l0, t0, r0, b0) = compute_filter_outset(&with_zero_spread, bbox, 1.0);
    assert!(
      (l - 10.0).abs() < 0.01
        && (t - 10.0).abs() < 0.01
        && (r - 10.0).abs() < 0.01
        && (b - 10.0).abs() < 0.01,
      "negative spread should reduce blur outset (got {l},{t},{r},{b})"
    );
    assert!(
      l < l0 && t < t0 && r < r0 && b < b0,
      "reduced spread should shrink outsets"
    );
  }

  #[test]
  fn drop_shadow_avoids_extra_result_pixmap_allocation() {
    let mut pixmap = new_pixmap(8, 8).expect("pixmap");
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        let idx = ((y * pixmap.width() + x) * 4) as usize;
        pixmap.data_mut()[idx] = 200;
        pixmap.data_mut()[idx + 3] = 255;
      }
    }

    let recorder = NewPixmapAllocRecorder::start();
    apply_drop_shadow(&mut pixmap, 0.0, 0.0, 0.0, 0.0, Rgba::BLACK).expect("drop shadow");
    let allocations = recorder.take();

    assert_eq!(
      allocations.len(),
      1,
      "expected only the shadow pixmap allocation, got {allocations:?}"
    );
    assert_eq!((allocations[0].width, allocations[0].height), (8, 8));
  }

  #[test]
  fn drop_shadow_spread_is_cancelable() {
    let width = 2048;
    let height = 64;
    let mut pixmap = new_pixmap(width, height).expect("pixmap");
    for px in pixmap.pixels_mut() {
      *px = PremultipliedColorU8::from_rgba(0, 0, 0, 255).expect("premultiplied");
    }

    let total_pixels = width as usize * height as usize;
    let conversion_checks =
      (total_pixels + LEGACY_FILTER_DEADLINE_STRIDE - 1) / LEGACY_FILTER_DEADLINE_STRIDE;
    // Allow the conversion loop to finish before triggering cancellation so we prove the spread
    // path checks deadlines as well.
    let cancel_after = conversion_checks + 8;

    let cancel_calls = Arc::new(AtomicUsize::new(0));
    let cancel_calls_cb = cancel_calls.clone();
    let cancel = Arc::new(move || {
      let call_num = cancel_calls_cb.fetch_add(1, Ordering::SeqCst) + 1;
      call_num > cancel_after
    });
    let deadline = RenderDeadline::new(None, Some(cancel));

    let result = with_deadline(Some(&deadline), || {
      apply_drop_shadow(&mut pixmap, 0.0, 0.0, 0.0, 8.0, Rgba::BLACK)
    });
    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout from cooperative cancellation, got {result:?}"
    );
    let calls = cancel_calls.load(Ordering::SeqCst);
    assert!(
      calls >= cancel_after + 1,
      "expected cancel callback to be invoked multiple times (got {calls})"
    );
  }

  #[test]
  fn spread_matches_reference_implementation() {
    let mut source = new_pixmap(17, 9).expect("pixmap");
    let width = source.width() as usize;
    for (idx, px) in source.pixels_mut().iter_mut().enumerate() {
      let x = idx % width;
      let y = idx / width;
      let alpha = if (x + y) % 4 == 0 {
        0
      } else {
        ((x * 17 + y * 31) % 200 + 30) as u8
      };
      let ru = ((x * 53 + y * 19) % 256) as u8;
      let gu = ((x * 11 + y * 73) % 256) as u8;
      let bu = ((x * 97 + y * 7) % 256) as u8;
      let r = ((ru as u16 * alpha as u16 + 127) / 255) as u8;
      let g = ((gu as u16 * alpha as u16 + 127) / 255) as u8;
      let b = ((bu as u16 * alpha as u16 + 127) / 255) as u8;
      *px = PremultipliedColorU8::from_rgba(r, g, b, alpha).expect("premultiplied");
    }

    for spread in [3.0, -3.0] {
      let mut fast = source.clone();
      let mut slow = source.clone();
      apply_spread(&mut fast, spread).expect("fast spread");
      apply_spread_slow_reference(&mut slow, spread);
      assert_eq!(
        fast.data(),
        slow.data(),
        "spread {spread} should match legacy reference implementation"
      );
    }
  }

  #[test]
  fn filter_outset_accumulates_blurs() {
    let filters = vec![ResolvedFilter::Blur(2.0), ResolvedFilter::Blur(3.0)];
    let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      (l - 15.0).abs() < 0.01
        && (t - 15.0).abs() < 0.01
        && (r - 15.0).abs() < 0.01
        && (b - 15.0).abs() < 0.01,
      "blur outsets should add up across filter chain"
    );
  }

  #[test]
  fn filter_outset_accumulates_drop_shadow_offsets() {
    let filters = vec![
      ResolvedFilter::Blur(2.0),
      ResolvedFilter::DropShadow {
        offset_x: -4.0,
        offset_y: 3.0,
        blur_radius: 1.0,
        spread: 0.0,
        color: Rgba::BLACK,
      },
    ];
    let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    // Blur contributes 6px first; drop shadow adds another 3px blur and shifts left/up by offsets.
    assert!(
      (l - 13.0).abs() < 0.01
        && (t - 6.0).abs() < 0.01
        && (r - 6.0).abs() < 0.01
        && (b - 12.0).abs() < 0.01,
      "expected accumulated outsets to be l=13,t=6,r=6,b=12 but got {l},{t},{r},{b}"
    );
  }

  #[test]
  fn blur_filter_outset_scales_with_device_pixel_ratio() {
    let filters = vec![ResolvedFilter::Blur(4.0)];
    let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    // Blur outset is radius * 3 per side.
    assert!(
      (l - 12.0).abs() < 0.01
        && (t - 12.0).abs() < 0.01
        && (r - 12.0).abs() < 0.01
        && (b - 12.0).abs() < 0.01
    );

    let filters = vec![ResolvedFilter::Blur(2.0)];
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 2.0);
    // Device pixel ratio doubles the blur radius before computing outsets.
    assert!(
      (l - 12.0).abs() < 0.01
        && (t - 12.0).abs() < 0.01
        && (r - 12.0).abs() < 0.01
        && (b - 12.0).abs() < 0.01
    );
  }

  #[test]
  fn drop_shadow_negative_spread_erodes_shadow() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::BLACK;
    style.filter = vec![FilterFunction::DropShadow(Box::new(FilterShadow {
      offset_x: Length::px(6.0),
      offset_y: Length::px(6.0),
      blur_radius: Length::px(0.0),
      spread: Length::px(-2.0),
      color: FilterColor::Color(Rgba::from_rgba8(255, 0, 0, 255)),
    }))];
    let mut root = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 20.0, 10.0), Vec::new());
    root.style = Some(Arc::new(style));

    let pixmap = paint_tree(&FragmentTree::new(root), 60, 40, Rgba::WHITE).expect("paint");
    let shadow_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > g && r > b).expect("shadow");
    let width = shadow_bbox.2 - shadow_bbox.0 + 1;
    assert!(
      width < 20,
      "negative spread should shrink shadow width (got width {width})"
    );
  }

  #[test]
  fn unit_interval_filters_clamp_to_one() {
    let mut style = ComputedStyle::default();
    style.filter = vec![
      FilterFunction::Grayscale(2.0),
      FilterFunction::Sepia(1.5),
      FilterFunction::Invert(1.3),
      FilterFunction::Opacity(1.7),
    ];
    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = resolve_filters(
      &style.filter,
      &style,
      (200.0, 100.0),
      &FontContext::new(),
      &mut resolver,
    );
    assert_eq!(filters.len(), 4);
    assert!(filters.iter().all(|f| match f {
      ResolvedFilter::Grayscale(v) | ResolvedFilter::Sepia(v) | ResolvedFilter::Invert(v) =>
        (*v - 1.0).abs() < 0.001,
      ResolvedFilter::Opacity(v) => (*v - 1.0).abs() < 0.001,
      _ => false,
    }));
  }

  #[test]
  fn multiplicative_filters_keep_values_above_one() {
    let mut style = ComputedStyle::default();
    style.filter = vec![
      FilterFunction::Brightness(2.5),
      FilterFunction::Contrast(1.7),
      FilterFunction::Saturate(3.2),
    ];
    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = resolve_filters(
      &style.filter,
      &style,
      (200.0, 100.0),
      &FontContext::new(),
      &mut resolver,
    );
    assert_eq!(filters.len(), 3);
    assert!(filters
      .iter()
      .any(|f| matches!(f, ResolvedFilter::Brightness(v) if (*v - 2.5).abs() < 0.001)));
    assert!(filters
      .iter()
      .any(|f| matches!(f, ResolvedFilter::Contrast(v) if (*v - 1.7).abs() < 0.001)));
    assert!(filters
      .iter()
      .any(|f| matches!(f, ResolvedFilter::Saturate(v) if (*v - 3.2).abs() < 0.001)));
  }

  #[test]
  fn grayscale_filter_converts_pixel_values() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::BLUE;
    style.filter = vec![FilterFunction::Grayscale(1.0)];

    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), Vec::new());
    root.style = Some(Arc::new(style));

    let pixmap = paint_tree(&FragmentTree::new(root), 30, 30, Rgba::WHITE).expect("paint");
    let pixel = pixmap.pixel(10, 10).expect("sample");
    // Blue (0,0,1) converted to grayscale yields ~0.0722 in each channel.
    assert!(
      pixel.red().abs_diff(18) <= 1,
      "expected ~18, got {}",
      pixel.red()
    );
    assert!(
      pixel.green().abs_diff(18) <= 1,
      "expected ~18, got {}",
      pixel.green()
    );
    assert!(
      pixel.blue().abs_diff(18) <= 1,
      "expected ~18, got {}",
      pixel.blue()
    );
  }

  #[test]
  fn mix_blend_mode_multiply_combines_colors() {
    // Background: red, child: semi-opaque blue with multiply blend → purple-ish with reduced alpha.
    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::from_rgba8(0, 0, 255, 128);
    child_style.mix_blend_mode = MixBlendMode::Multiply;

    let child = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      FragmentContent::Block { box_id: None },
      vec![],
      child_style.into(),
    );

    let mut root_style = ComputedStyle::default();
    root_style.background_color = Rgba::RED;
    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![child]);
    root.style = Some(Arc::new(root_style));

    let pixmap = paint_tree(&FragmentTree::new(root), 30, 30, Rgba::WHITE).expect("paint");
    let pixel = pixmap.pixel(10, 10).expect("sample");

    // Multiply red (1,0,0) by blue (0,0,1) → (0,0,0); with 50% alpha over red bg we expect
    // the result to stay dark and keep a nonzero blue component but low green.
    assert!(
      pixel.red() < 200,
      "red should darken under multiply (got {})",
      pixel.red()
    );
    assert!(
      pixel.blue() < 130,
      "blue should darken under multiply (got {})",
      pixel.blue()
    );
    // Green should remain near zero for red*blue.
    assert!(
      pixel.green() < 30,
      "green should stay low (got {})",
      pixel.green()
    );
  }

  #[test]
  fn text_decoration_thickness_auto_uses_ua_default_in_legacy_painter() {
    let painter = Painter::with_resources_scaled(
      10,
      10,
      Rgba::WHITE,
      FontContext::new(),
      ImageCache::new(),
      2.0,
    )
    .expect("painter");
    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    let auto_thickness = painter
      .resolve_decoration_thickness_value(TextDecorationThickness::Auto, &style)
      .expect("auto thickness");
    assert!((auto_thickness - 2.0).abs() < 0.0001);

    style.font_size = 200.0;
    let auto_thickness_large = painter
      .resolve_decoration_thickness_value(TextDecorationThickness::Auto, &style)
      .expect("auto thickness");
    assert!((auto_thickness_large - 40.0).abs() < 0.0001);

    assert!(painter
      .resolve_decoration_thickness_value(TextDecorationThickness::FromFont, &style)
      .is_none());
  }

  #[test]
  fn underline_offset_moves_line() {
    let mut style = ComputedStyle::default();
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    style.font_size = 20.0;

    let painter = Painter::new(10, 10, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("Hi", &style, &painter.font_ctx)
      .expect("shape");
    let metrics = painter
      .decoration_metrics(Some(&runs), &style)
      .expect("metrics");
    let metrics = metrics.scaled(painter.scale);
    let baseline = 20.0 * painter.scale;
    let thickness = metrics.underline_thickness;
    let base_center = painter.underline_center(
      &metrics,
      style.text_underline_position,
      crate::style::types::TextUnderlineOffset::Length(Length::px(0.0)),
      thickness,
      baseline,
      false,
      &style,
    );

    style.text_underline_offset = crate::style::types::TextUnderlineOffset::Length(Length::px(4.0));
    let shifted_center = painter.underline_center(
      &metrics,
      style.text_underline_position,
      style.text_underline_offset,
      thickness,
      baseline,
      false,
      &style,
    );

    assert!(
      shifted_center > base_center,
      "positive offset should move underline further from the baseline"
    );
    assert!(
      (shifted_center - base_center - 4.0 * painter.scale).abs() < 0.01,
      "underline offset should roughly follow the authored length"
    );
  }

  #[test]
  fn underline_position_under_moves_line_downward() {
    let mut style = ComputedStyle::default();
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    style.font_size = 20.0;

    let painter = Painter::new(20, 20, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("Hg", &style, &painter.font_ctx)
      .expect("shape");
    let metrics = painter
      .decoration_metrics(Some(&runs), &style)
      .expect("metrics");
    let metrics = metrics.scaled(painter.scale);
    let thickness = metrics.underline_thickness;
    let baseline = 20.0 * painter.scale;
    let offset = crate::style::types::TextUnderlineOffset::Length(Length::px(0.0));

    let auto_center = painter.underline_center(
      &metrics,
      style.text_underline_position,
      offset,
      thickness,
      baseline,
      false,
      &style,
    );
    style.text_underline_position = crate::style::types::TextUnderlinePosition::Under;
    let under_center = painter.underline_center(
      &metrics,
      style.text_underline_position,
      offset,
      thickness,
      baseline,
      false,
      &style,
    );

    assert!(
      under_center > auto_center,
      "requesting under position should place the underline below the auto position"
    );
    assert!(
      under_center - auto_center > 0.5,
      "under position should move the line a noticeable distance below the baseline"
    );
  }

  #[test]
  fn text_decoration_thickness_uses_font_relative_units() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 20.0;
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::from_rgba8(255, 0, 0, 255));
    style.text_decoration.thickness =
      crate::style::types::TextDecorationThickness::Length(Length::percent(50.0));
    let style = Arc::new(style);

    let painter = Painter::new(100, 60, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("Hi", &style, &painter.font_ctx)
      .expect("shape");
    let width: f32 = runs.iter().map(|r| r.advance).sum();
    let baseline = 32.0;
    let rect = Rect::from_xywh(10.0, 10.0, width + 2.0, 40.0);
    let fragment =
      FragmentNode::new_text_shaped(rect, "Hi".to_string(), baseline - rect.y(), runs, style);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 60.0), vec![fragment]);
    let pixmap = paint_tree(&FragmentTree::new(root), 100, 60, Rgba::WHITE).expect("paint");

    let red_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > 200 && g < 80 && b < 80)
        .expect("underline");
    let height = red_bbox.3 - red_bbox.1 + 1;
    assert!(
      height >= 9 && height <= 11,
      "expected underline thickness around 10px (50% of 20px font), got {height}"
    );
  }

  #[test]
  fn underline_offset_accepts_ex_units() {
    let mut style = ComputedStyle::default();
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    style.font_size = 20.0;

    let painter = Painter::new(60, 40, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("Hi", &style, &painter.font_ctx)
      .expect("shape");
    let metrics = painter
      .decoration_metrics(Some(&runs), &style)
      .expect("metrics");
    let metrics = metrics.scaled(painter.scale);
    let thickness = metrics.underline_thickness;
    let baseline = 24.0 * painter.scale;
    let base_center = painter.underline_center(
      &metrics,
      style.text_underline_position,
      crate::style::types::TextUnderlineOffset::Length(Length::px(0.0)),
      thickness,
      baseline,
      false,
      &style,
    );

    let mut ex_style = style;
    ex_style.text_underline_offset =
      crate::style::types::TextUnderlineOffset::Length(Length::ex(1.0));
    let ex_center = painter.underline_center(
      &metrics,
      ex_style.text_underline_position,
      ex_style.text_underline_offset,
      thickness,
      baseline,
      false,
      &ex_style,
    );

    assert!(
      ex_center > base_center,
      "ex-based underline offset should push the line farther from the baseline"
    );
  }

  #[test]
  fn underline_thickness_scales_with_device_pixel_ratio() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 20.0;
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::from_rgba8(255, 0, 0, 255));
    style.text_decoration.thickness =
      crate::style::types::TextDecorationThickness::Length(Length::px(4.0));
    style.text_decoration_skip_ink = crate::style::types::TextDecorationSkipInk::None;
    let style = Arc::new(style);

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(10.0, 10.0, 80.0, 30.0),
      "Hi".to_string(),
      22.0,
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 60.0), vec![fragment]);
    let pixmap =
      paint_tree_scaled(&FragmentTree::new(root), 120, 60, Rgba::WHITE, 2.0).expect("paint");

    let red_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > 200 && g < 80 && b < 80)
        .expect("underline");
    let height = red_bbox.3 - red_bbox.1 + 1;
    assert!(
      (7..=9).contains(&height),
      "expected underline thickness around 8 device px (4 CSS px at 2x), got {height}"
    );
  }

  #[test]
  fn skip_ink_all_forces_exclusions_even_without_overlap() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 24.0;
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::from_rgba8(255, 0, 0, 255));
    style.text_decoration_skip_ink = crate::style::types::TextDecorationSkipInk::All;
    style.text_decoration.thickness =
      crate::style::types::TextDecorationThickness::Length(Length::px(6.0));
    style.text_underline_offset = crate::style::types::TextUnderlineOffset::Length(Length::px(0.0));

    let painter = Painter::new(240, 140, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("HI HI", &style, &painter.font_ctx)
      .expect("shape");
    let metrics = painter
      .decoration_metrics(Some(&runs), &style)
      .expect("metrics");
    let width: f32 = runs.iter().map(|r| r.advance).sum();
    let baseline = 50.0;
    // Place the underline well below the glyph ink so auto skip-ink would keep it continuous.
    let center = baseline + 30.0;
    let thickness = match style.text_decoration.thickness {
      crate::style::types::TextDecorationThickness::Length(l) => l.to_px(),
      _ => metrics.underline_thickness,
    };
    let line_start = 10.0;
    let segments_all = painter.build_underline_segments(
      &runs,
      line_start,
      width,
      center,
      thickness,
      baseline,
      false,
      crate::style::types::TextDecorationSkipInk::All,
      0.0,
    );

    let segments_auto = painter.build_underline_segments(
      &runs,
      line_start,
      width,
      center,
      thickness,
      baseline,
      false,
      crate::style::types::TextDecorationSkipInk::Auto,
      0.0,
    );

    assert!(
      !segments_all.is_empty(),
      "skip-ink all should still paint around glyphs rather than dropping the line entirely"
    );
    assert_eq!(
      segments_auto.len(),
      1,
      "auto skip-ink should keep a continuous line when nothing overlaps the band"
    );
    let full_span = (line_start, line_start + width);
    let auto_span = segments_auto[0];
    assert!(
      (auto_span.0 - full_span.0).abs() < 0.01 && (auto_span.1 - full_span.1).abs() < 0.01,
      "auto span should cover the entire underline"
    );
    let carved_length: f32 = segments_all.iter().map(|(s, e)| e - s).sum();
    assert!(
      carved_length < width - 0.5,
      "skip-ink: all should carve out glyph intervals even when the underline is far from ink"
    );
  }

  #[test]
  fn underline_skip_ink_carves_descenders() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 28.0;
    style.text_decoration.lines = crate::style::types::TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::from_rgba8(255, 0, 0, 255));
    style.text_decoration.thickness =
      crate::style::types::TextDecorationThickness::Length(Length::px(3.0));
    style.text_underline_offset =
      crate::style::types::TextUnderlineOffset::Length(Length::px(-1.0));

    let painter = Painter::new(160, 100, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("gy", &style, &painter.font_ctx)
      .expect("shape");
    assert!(
      !runs.is_empty(),
      "shaping should yield glyphs for skip-ink evaluation"
    );
    let line_start = 10.0;
    let line_width: f32 = runs.iter().map(|r| r.advance).sum();
    let baseline = 50.0;

    let baseline_offset = baseline - 10.0;
    let rect = Rect::from_xywh(line_start, 10.0, line_width, 60.0);
    let fragment_auto = FragmentNode::new_text_shaped(
      rect,
      "gy".to_string(),
      baseline_offset,
      runs.clone(),
      Arc::new(style.clone()),
    );
    let root_auto =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 160.0, 100.0), vec![fragment_auto]);
    let pix_auto =
      paint_tree(&FragmentTree::new(root_auto), 160, 100, Rgba::WHITE).expect("auto paint");

    let mut no_skip_style = style;
    no_skip_style.text_decoration_skip_ink = crate::style::types::TextDecorationSkipInk::None;
    let fragment_none = FragmentNode::new_text_shaped(
      rect,
      "gy".to_string(),
      baseline_offset,
      runs,
      Arc::new(no_skip_style),
    );
    let root_none =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 160.0, 100.0), vec![fragment_none]);
    let pix_none =
      paint_tree(&FragmentTree::new(root_none), 160, 100, Rgba::WHITE).expect("no-skip paint");

    let is_redish = |(r, g, b, a): (u8, u8, u8, u8)| {
      a > 0 && (r as i16 - g as i16) > 80 && (r as i16 - b as i16) > 80
    };
    let mut found = false;
    let mut any_none_red = false;
    for y in 0..pix_none.height() {
      for x in 0..pix_none.width() {
        let auto_px = color_at(&pix_auto, x, y);
        let no_skip_px = color_at(&pix_none, x, y);
        if is_redish(no_skip_px) {
          any_none_red = true;
        }
        if is_redish(no_skip_px) && !is_redish(auto_px) {
          found = true;
          break;
        }
      }
      if found {
        break;
      }
    }
    assert!(
      any_none_red,
      "expected an underline to be painted when skip-ink is none"
    );
    assert!(
      found,
      "expected skip-ink auto to omit underline pixels that are present when skip-ink is none"
    );
  }

  #[test]
  fn paints_text_emphasis_marks_above_text() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 24.0;
    style.text_emphasis_style = crate::style::types::TextEmphasisStyle::Mark {
      fill: crate::style::types::TextEmphasisFill::Filled,
      shape: Some(crate::style::types::TextEmphasisShape::Circle),
    };
    style.text_emphasis_color = Some(Rgba::from_rgba8(255, 0, 0, 255));
    let style = Arc::new(style);

    let painter = Painter::new(80, 60, Rgba::WHITE).expect("painter");
    let runs = painter
      .shaper
      .shape("A", &style, &painter.font_ctx)
      .expect("shape");
    let width: f32 = runs.iter().map(|r| r.advance).sum();
    let baseline = 32.0;
    let rect = Rect::from_xywh(10.0, 8.0, width + 2.0, 40.0);
    let fragment =
      FragmentNode::new_text_shaped(rect, "A".to_string(), baseline - rect.y(), runs, style);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 80.0, 60.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 80, 60, Rgba::WHITE).expect("paint");

    let black_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r < 32 && g < 32 && b < 32)
        .expect("text");
    let red_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > 200 && g < 80 && b < 80)
        .expect("emphasis mark");

    assert!(
      red_bbox.1 < black_bbox.1,
      "emphasis mark should appear above the glyphs when positioned over the text"
    );
  }

  #[test]
  fn emphasis_triangle_mark_remains_upright_for_vertical_runs() {
    let mut painter = Painter::new(120, 120, Rgba::WHITE).expect("painter");
    painter.fill_background();
    painter.draw_emphasis_mark(
      60.0,
      60.0,
      50.0,
      crate::style::types::TextEmphasisFill::Filled,
      crate::style::types::TextEmphasisShape::Triangle,
      Rgba::from_rgba8(255, 0, 0, 255),
      crate::style::types::TextEmphasisPosition::Under,
      true,
    );

    let bbox = bounding_box_for_color(&painter.pixmap, |(r, g, b, a)| {
      a > 0 && r > 200 && g < 80 && b < 80
    })
    .expect("triangle mark");
    let width = bbox.2 - bbox.0;
    let height = bbox.3 - bbox.1;
    assert!(
      width > height,
      "expected upright triangle mark (width {width} > height {height})"
    );
  }

  #[test]
  fn emphasis_sesame_mark_remains_upright_for_vertical_runs() {
    let mut painter = Painter::new(120, 120, Rgba::WHITE).expect("painter");
    painter.fill_background();
    painter.draw_emphasis_mark(
      60.0,
      60.0,
      50.0,
      crate::style::types::TextEmphasisFill::Filled,
      crate::style::types::TextEmphasisShape::Sesame,
      Rgba::from_rgba8(255, 0, 0, 255),
      crate::style::types::TextEmphasisPosition::Under,
      true,
    );

    let bbox = bounding_box_for_color(&painter.pixmap, |(r, g, b, a)| {
      a > 0 && r > 200 && g < 80 && b < 80
    })
    .expect("sesame mark");
    let width = bbox.2 - bbox.0;
    let height = bbox.3 - bbox.1;
    assert!(
      width > height,
      "expected upright sesame mark (width {width} > height {height})"
    );
  }

  #[test]
  fn legacy_range_control_uses_slider_pseudo_styles() {
    let mut style = ComputedStyle::default();
    style.accent_color = AccentColor::Color(Rgba::TRANSPARENT);

    let mut track_style = ComputedStyle::default();
    track_style.height = Some(Length::px(8.0));
    track_style.background_color = Rgba::GREEN;
    track_style.border_top_width = Length::px(0.0);
    track_style.border_right_width = Length::px(0.0);
    track_style.border_bottom_width = Length::px(0.0);
    track_style.border_left_width = Length::px(0.0);

    let mut thumb_style = ComputedStyle::default();
    thumb_style.width = Some(Length::px(20.0));
    thumb_style.height = Some(Length::px(20.0));
    thumb_style.background_color = Rgba::RED;
    thumb_style.border_top_width = Length::px(0.0);
    thumb_style.border_right_width = Length::px(0.0);
    thumb_style.border_bottom_width = Length::px(0.0);
    thumb_style.border_left_width = Length::px(0.0);

    let control = FormControl {
      control: FormControlKind::Range {
        value: 50.0,
        min: 0.0,
        max: 100.0,
      },
      appearance: Appearance::Auto,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
      placeholder_style: None,
      slider_thumb_style: Some(Arc::new(thumb_style)),
      slider_track_style: Some(Arc::new(track_style)),
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
    };

    let mut painter = Painter::new(100, 30, Rgba::WHITE).expect("painter");
    painter.fill_background();
    let content_rect = Rect::from_xywh(0.0, 0.0, 100.0, 30.0);
    painter.paint_form_control(&control, &style, content_rect, content_rect, None, None);

    let pixmap = painter.pixmap;
    let thumb_px = color_at(&pixmap, 50, 15);
    assert!(
      thumb_px.0 > 200 && thumb_px.1 < 80 && thumb_px.2 < 80,
      "expected thumb to use slider pseudo background color, got rgba={thumb_px:?}"
    );

    let track_px = color_at(&pixmap, 10, 15);
    assert!(
      track_px.1 > 200 && track_px.0 < 80 && track_px.2 < 80,
      "expected track to use slider pseudo background color, got rgba={track_px:?}"
    );

    assert_eq!(
      color_at(&pixmap, 10, 5),
      (255, 255, 255, 255),
      "expected background pixels outside the track"
    );
  }

  #[test]
  fn focused_text_control_does_not_paint_internal_tint_overlay_painter() {
    // Regression test: native control painting used to apply a semi-transparent "state tint" fill
    // for focused/focus-visible controls. This is not driven by CSS and should not be painted as an
    // extra overlay.
    let mut style = ComputedStyle::default();
    style.accent_color = AccentColor::Color(Rgba::from_rgba8(26, 115, 232, 255));

    let control = FormControl {
      control: FormControlKind::Text {
        value: String::new(),
        placeholder: None,
        placeholder_style: None,
        size_attr: None,
        kind: TextControlKind::Plain,
        caret: 0,
        caret_affinity: CaretAffinity::Downstream,
        selection: None,
      },
      appearance: Appearance::Auto,
      disabled: false,
      focused: true,
      focus_visible: true,
      required: false,
      invalid: false,
      ime_preedit: None,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
    };

    let mut painter = Painter::new(200, 40, Rgba::WHITE).expect("painter");
    painter.fill_background();
    let content_rect = Rect::from_xywh(10.0, 10.0, 180.0, 20.0);
    painter.paint_form_control(&control, &style, content_rect, content_rect, None, None);

    assert_eq!(
      color_at(&painter.pixmap, 100, 20),
      (255, 255, 255, 255),
      "expected form control painting to not add internal tint overlays"
    );
  }

  #[test]
  fn replaced_content_respects_padding_box() {
    let mut style = ComputedStyle::default();
    style.padding_left = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.padding_top = Length::px(4.0);
    style.padding_bottom = Length::px(4.0);
    style.background_color = Rgba::BLUE;

    let mut painter =
      Painter::with_resources(40, 40, Rgba::WHITE, FontContext::new(), ImageCache::new())
        .expect("painter");
    painter.fill_background();

    let box_x = 10.0;
    let box_y = 10.0;
    let box_size = 20.0;
    let rects = background_rects(box_x, box_y, box_size, box_size, &style, None);
    assert!((rects.content.x() - 14.0).abs() < 0.01);
    assert!((rects.content.y() - 14.0).abs() < 0.01);
    assert!((rects.content.width() - 12.0).abs() < 0.01);
    assert!((rects.content.height() - 12.0).abs() < 0.01);
    painter.paint_background(box_x, box_y, box_size, box_size, &style, Point::ZERO);
    painter.paint_replaced(
      &ReplacedType::Svg {
        content: SvgContent::raw(red_svg()),
      },
      None,
      Some(&style),
      box_x,
      box_y,
      box_size,
      box_size,
    );

    let pixmap = painter.pixmap;
    assert_eq!(color_at(&pixmap, 11, 11), (0, 0, 255, 255));
    assert_eq!(color_at(&pixmap, 15, 15), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 20, 20), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 27, 27), (0, 0, 255, 255));
  }

  #[test]
  fn paints_alt_text_when_image_missing() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Image {
          src: String::new(),
          alt: Some("alt".to_string()),
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
        box_id: None,
      },
      vec![],
      Arc::new(style),
    );

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 60.0, 30.0), vec![fragment]);
    let tree = FragmentTree::new(root);
    let pixmap = paint_tree(&tree, 60, 30, Rgba::WHITE).expect("paint alt");

    let center = color_at(&pixmap, 25, 10);
    assert_ne!(
      center,
      (200, 200, 200, 255),
      "alt text should prevent placeholder rectangles"
    );

    let mut has_ink = false;
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        if color_at(&pixmap, x, y) != (255, 255, 255, 255) {
          has_ink = true;
          break;
        }
      }
      if has_ink {
        break;
      }
    }
    assert!(has_ink, "alt text should paint glyphs");
  }

  #[test]
  fn paints_alt_text_when_image_is_placeholder() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;

    // `about:blank` is treated as a non-fetchable URL and resolves to the internal transparent
    // placeholder image. `<img>` elements should treat that placeholder as a missing image and
    // render alt text rather than painting a transparent 1×1 pixel.
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Image {
          src: "about:blank".to_string(),
          alt: Some("alt".to_string()),
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
        box_id: None,
      },
      vec![],
      Arc::new(style),
    );

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 60.0, 30.0), vec![fragment]);
    let tree = FragmentTree::new(root);
    let pixmap = paint_tree(&tree, 60, 30, Rgba::WHITE).expect("paint alt");

    let center = color_at(&pixmap, 25, 10);
    assert_ne!(
      center,
      (200, 200, 200, 255),
      "alt text should prevent placeholder rectangles"
    );

    let mut has_ink = false;
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        if color_at(&pixmap, x, y) != (255, 255, 255, 255) {
          has_ink = true;
          break;
        }
      }
      if has_ink {
        break;
      }
    }
    assert!(has_ink, "alt text should paint glyphs");
  }

  #[test]
  fn paints_embed_svg_content() {
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"2\" height=\"2\"><rect width=\"2\" height=\"2\" fill=\"red\"/></svg>";
    let style = Arc::new(ComputedStyle::default());
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Embed {
          src: svg.to_string(),
        },
        box_id: None,
      },
      vec![],
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint embed");
    assert_eq!(
      color_at(&pixmap, 10, 10),
      (255, 0, 0, 255),
      "embed should render svg content"
    );
  }

  #[test]
  fn paints_video_poster_image_content() {
    let poster =
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"lime\"/></svg>";
    let style = Arc::new(ComputedStyle::default());
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Video {
          src: String::new(),
          poster: Some(poster.to_string()),
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          controls: false,
        },
        box_id: None,
      },
      vec![],
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 12.0, 12.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 12, 12, Rgba::WHITE).expect("paint video poster");
    assert_eq!(
      color_at(&pixmap, 5, 5),
      (0, 255, 0, 255),
      "poster content should paint instead of placeholder"
    );
  }

  #[test]
  fn opacity_layer_propagates_media_frame_provider() {
    #[derive(Debug)]
    struct MockMediaProvider;

    impl crate::media::MediaFrameProvider for MockMediaProvider {
      fn video_frame(
        &self,
        _box_id: Option<usize>,
        src: &str,
        _size_hint: Option<crate::media::MediaFrameSizeHint>,
      ) -> Option<Arc<ImageData>> {
        if src != "v.mp4" {
          return None;
        }
        let pixels = [0u8, 0, 255, 255].repeat(4);
        Some(Arc::new(ImageData::new_premultiplied(
          2,
          2,
          2.0,
          2.0,
          pixels,
        )))
      }
    }

    let font_ctx = FontContext::with_config(crate::text::font_db::FontConfig::bundled_only());
    let mut painter = Painter::with_resources_scaled(
      2,
      2,
      Rgba::TRANSPARENT,
      font_ctx,
      ImageCache::new(),
      1.0,
    )
    .expect("painter");
    painter.media_provider = Some(Arc::new(MockMediaProvider));

    let style = ComputedStyle::default();
    let replaced = ReplacedType::Video {
      src: "v.mp4".to_string(),
      poster: None,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      controls: false,
    };

    painter.paint_with_opacity_layer(
      0.5,
      Rect::from_xywh(0.0, 0.0, 2.0, 2.0),
      None,
      |layer, _clip| {
        layer.paint_replaced(&replaced, None, Some(&style), 0.0, 0.0, 2.0, 2.0);
      },
    );

    let px = painter.pixmap.pixel(1, 1).expect("pixel");
    assert!(
      px.blue() > 0 && px.alpha() > 0,
      "expected video frame to be painted through opacity layer, got rgba=({}, {}, {}, {})",
      px.red(),
      px.green(),
      px.blue(),
      px.alpha()
    );
    assert!(
      (120..=136).contains(&px.blue()) && (120..=136).contains(&px.alpha()),
      "expected ~50% opacity (blue/alpha near 128), got rgba=({}, {}, {}, {})",
      px.red(),
      px.green(),
      px.blue(),
      px.alpha()
    );
  }

  #[test]
  fn paints_video_controls_placeholder_when_no_poster() {
    let style = Arc::new(ComputedStyle::default());
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Video {
          src: String::new(),
          poster: None,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          controls: true,
        },
        box_id: None,
      },
      vec![],
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![fragment]);
    let tree = FragmentTree::new(root);
    let pixmap = paint_tree(&tree, 200, 200, Rgba::WHITE).expect("paint video placeholder");

    let top = color_at(&pixmap, 100, 20);
    assert_eq!(
      top,
      (51, 51, 51, 255),
      "video surface should paint a stable dark background"
    );

    let bottom = color_at(&pixmap, 100, 190);
    assert!(
      bottom.0 < top.0,
      "expected native control shadow to darken near the bottom (top={top:?} bottom={bottom:?})"
    );
  }

  #[test]
  fn paints_video_controls_placeholder_controls_ui() {
    let style = Arc::new(ComputedStyle::default());
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Video {
          src: String::new(),
          poster: None,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          controls: true,
        },
        box_id: None,
      },
      vec![],
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![fragment]);
    let tree = FragmentTree::new(root);
    let pixmap = paint_tree(&tree, 200, 200, Rgba::WHITE).expect("paint video placeholder");

    let progress = color_at(&pixmap, 100, 142);
    assert!(
      progress.0 > 80,
      "expected a visible progress track in the controls UI (got {progress:?})"
    );
  }

  #[test]
  fn paints_audio_placeholder() {
    let style = Arc::new(ComputedStyle::default());
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 12.0, 8.0),
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Audio {
          src: String::new(),
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
        },
        box_id: None,
      },
      vec![],
      style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 14.0, 10.0), vec![fragment]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 14, 10, Rgba::WHITE).expect("paint audio placeholder");
    let center = color_at(&pixmap, 6, 4);
    assert_eq!(
      center,
      (200, 200, 200, 255),
      "audio should render placeholder fill when no media is available"
    );
  }

  fn svg_data_url(color: &str) -> String {
    format!(
            "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'><rect width='1' height='1' fill='{color}'/></svg>"
        )
  }

  #[test]
  fn srcset_chooses_best_density_for_device_scale() {
    let red = svg_data_url("red");
    let blue = svg_data_url("blue");

    let replaced = ReplacedType::Image {
      src: red.clone(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      sizes: None,
      srcset: vec![
        SrcsetCandidate {
          url: red.clone(),
          descriptor: SrcsetDescriptor::Density(1.0),
        },
        SrcsetCandidate {
          url: blue.clone(),
          descriptor: SrcsetDescriptor::Density(2.0),
        },
      ],
      picture_sources: Vec::new(),
    };

    let style = ComputedStyle::default();
    let mut painter = Painter::with_resources_scaled(
      2,
      2,
      Rgba::WHITE,
      FontContext::new(),
      ImageCache::new(),
      2.0,
    )
    .expect("painter");
    painter.paint_replaced(&replaced, None, Some(&style), 0.0, 0.0, 1.0, 1.0);

    let px = painter.pixmap.pixel(0, 0).unwrap();
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (0, 0, 255),
      "2x density should pick the blue candidate at scale 2.0"
    );
  }

  #[test]
  fn srcset_width_descriptor_uses_slot_width() {
    let red = svg_data_url("red");
    let blue = svg_data_url("blue");

    let replaced = ReplacedType::Image {
      src: red.clone(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      sizes: None,
      srcset: vec![
        SrcsetCandidate {
          url: red.clone(),
          descriptor: SrcsetDescriptor::Width(100),
        },
        SrcsetCandidate {
          url: blue.clone(),
          descriptor: SrcsetDescriptor::Width(300),
        },
      ],
      picture_sources: Vec::new(),
    };

    let style = ComputedStyle::default();
    let mut painter = Painter::with_resources_scaled(
      120,
      20,
      Rgba::WHITE,
      FontContext::new(),
      ImageCache::new(),
      2.0,
    )
    .expect("painter");

    painter.paint_replaced(&replaced, None, Some(&style), 0.0, 0.0, 100.0, 10.0);

    let px = painter.pixmap.pixel(100, 10).unwrap();
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (0, 0, 255),
      "with 100px slot at DPR=2, the 300w candidate (density 3) should be chosen"
    );
  }

  #[test]
  fn paints_linear_gradient_background() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 90.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");
    let left = color_at(&pixmap, 2, 10);
    let right = color_at(&pixmap, 18, 10);
    assert!(left.0 > right.0, "left should be redder than right");
    assert!(right.2 > left.2, "right should be bluer than left");
  }

  #[test]
  fn normalizes_missing_gradient_stops() {
    let stops = vec![
      ColorStop {
        color: Color::Rgba(Rgba::RED),
        position: None,
      },
      ColorStop {
        color: Color::Rgba(Rgba::GREEN),
        position: Some(crate::css::types::ColorStopPosition::Fraction(0.5)),
      },
      ColorStop {
        color: Color::Rgba(Rgba::BLUE),
        position: None,
      },
    ];

    let resolved = super::normalize_color_stops(
      &stops,
      Rgba::WHITE,
      100.0,
      16.0,
      16.0,
      (100.0, 100.0),
      false,
      false,
    );

    assert_eq!(resolved.len(), 3);
    assert!((resolved[0].0 - 0.0).abs() < 1e-6);
    assert!((resolved[1].0 - 0.5).abs() < 1e-6);
    assert!((resolved[2].0 - 1.0).abs() < 1e-6);
  }

  #[test]
  fn normalize_color_stops_resolves_current_color() {
    let stops = vec![
      crate::css::types::ColorStop {
        color: Color::CurrentColor,
        position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
      },
      crate::css::types::ColorStop {
        color: Color::Rgba(Rgba::BLUE),
        position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
      },
    ];
    let resolved = normalize_color_stops(
      &stops,
      Rgba::new(10, 20, 30, 1.0),
      100.0,
      16.0,
      16.0,
      (100.0, 100.0),
      false,
      false,
    );
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].1, Rgba::new(10, 20, 30, 1.0));
    assert_eq!(resolved[1].1, Rgba::BLUE);
  }

  #[test]
  fn paints_repeating_linear_gradient_background() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::RepeatingLinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.5)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");

    let top = color_at(&pixmap, 10, 2);
    let middle = color_at(&pixmap, 10, 10);
    let bottom = color_at(&pixmap, 10, 18);

    // Repeating stripes: samples at different rows should not all match; require at least two distinct colors.
    assert!(top != middle || middle != bottom);
    let mut distinct = std::collections::HashSet::new();
    distinct.insert(top);
    distinct.insert(middle);
    distinct.insert(bottom);
    assert!(
      distinct.len() >= 2,
      "expected at least two colors in repeating gradient"
    );
  }

  #[test]
  fn radial_gradient_uses_farthest_corner_ellipse() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::RadialGradient {
        shape: RadialGradientShape::Ellipse,
        size: RadialGradientSize::FarthestCorner,
        position: BackgroundPosition::Position {
          x: crate::style::types::BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
          y: crate::style::types::BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
        },
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 20, 10, Rgba::WHITE).expect("paint");

    let top_center = color_at(&pixmap, 10, 0);
    let right_center = color_at(&pixmap, 19, 5);
    let diff_r = (top_center.0 as i32 - right_center.0 as i32).abs();
    let diff_b = (top_center.2 as i32 - right_center.2 as i32).abs();
    assert!(
      diff_r < 32 && diff_b < 32,
      "elliptical gradient should make horizontal/vertical edges equally distant (diffs r={} b={})",
      diff_r,
      diff_b
    );

    let corner = color_at(&pixmap, 19, 9);
    assert!(
      corner.2 > corner.0,
      "farthest-corner sizing should leave the corner closest to the final stop"
    );
  }

  #[test]
  fn radial_gradient_honors_position() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::RadialGradient {
        shape: RadialGradientShape::Ellipse,
        size: RadialGradientSize::FarthestCorner,
        position: BackgroundPosition::Position {
          x: crate::style::types::BackgroundPositionComponent {
            alignment: 0.0,
            offset: Length::px(0.0),
          },
          y: crate::style::types::BackgroundPositionComponent {
            alignment: 0.0,
            offset: Length::px(0.0),
          },
        },
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 20, 10, Rgba::WHITE).expect("paint");

    let top_left = color_at(&pixmap, 0, 0);
    assert!(
      top_left.0 > top_left.2,
      "top-left should start at the first stop (more red than blue)"
    );
    let corner = color_at(&pixmap, 19, 9);
    assert!(
      corner.2 > corner.0,
      "gradient centered at top-left should reach final stop toward far corner"
    );
  }

  #[test]
  fn conic_gradient_respects_angles() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::ConicGradient {
        from_angle: 0.0,
        position: BackgroundPosition::Position {
          x: crate::style::types::BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
          y: crate::style::types::BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
        },
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.5)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");

    let top = color_at(&pixmap, 10, 0);
    let bottom = color_at(&pixmap, 10, 19);
    assert!(top.0 > top.2, "top should be dominated by first stop (red)");
    assert!(
      bottom.2 > bottom.0,
      "bottom should reflect halfway stop (blue)"
    );
  }

  #[test]
  fn background_attachment_fixed_anchors_to_viewport() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 90.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      attachment: BackgroundAttachment::Fixed,
      ..BackgroundLayer::default()
    }]);

    // Gradient anchors to viewport: samples at successive x positions diverge even though elements have their own origins.
    let style_arc = Arc::new(style);
    let first = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
      vec![],
      style_arc.clone(),
    );
    let second = FragmentNode::new_block_styled(
      Rect::from_xywh(1.0, 0.0, 1.0, 1.0),
      vec![],
      style_arc.clone(),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 4.0, 2.0), vec![first, second]);
    let tree = FragmentTree::new(root);
    let pixmap = paint_tree(&tree, 4, 2, Rgba::WHITE).expect("paint");

    let left = color_at(&pixmap, 0, 0);
    let right = color_at(&pixmap, 1, 0);
    assert!(
      left.0 > right.0,
      "fixed attachment should keep gradient anchored to viewport"
    );
    assert!(right.2 > left.2);
  }

  #[test]
  fn background_attachment_local_uses_scrollable_overflow_area() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      attachment: BackgroundAttachment::Local,
      ..BackgroundLayer::default()
    }]);
    style.overflow_x = Overflow::Scroll;
    style.overflow_y = Overflow::Scroll;
    style.border_top_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    style.border_bottom_width = Length::px(2.0);
    style.border_left_width = Length::px(2.0);
    style.border_top_style = CssBorderStyle::Solid;
    style.border_right_style = CssBorderStyle::Solid;
    style.border_bottom_style = CssBorderStyle::Solid;
    style.border_left_style = CssBorderStyle::Solid;
    style.border_top_color = Rgba::TRANSPARENT;
    style.border_right_color = Rgba::TRANSPARENT;
    style.border_bottom_color = Rgba::TRANSPARENT;
    style.border_left_color = Rgba::TRANSPARENT;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 12.0, 12.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 12, 12, Rgba::WHITE).expect("paint");

    // Border-box samples should stay transparent because local attachment anchors to the scrollable overflow area
    // (padding box) and border-box clipping collapses to padding-box per CSS Backgrounds 3.
    assert_eq!(color_at(&pixmap, 1, 1), (255, 255, 255, 255));
    // Padding box should still paint the background image.
    assert_eq!(color_at(&pixmap, 6, 6), (255, 0, 0, 255));
  }

  #[test]
  fn background_color_uses_first_layer_clip_value() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    style.background_images = vec![None, None].into();
    style.background_clips = vec![BackgroundBox::PaddingBox, BackgroundBox::BorderBox].into();
    style.rebuild_background_layers();
    style.border_top_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    style.border_bottom_width = Length::px(2.0);
    style.border_left_width = Length::px(2.0);
    style.border_top_style = CssBorderStyle::Solid;
    style.border_right_style = CssBorderStyle::Solid;
    style.border_bottom_style = CssBorderStyle::Solid;
    style.border_left_style = CssBorderStyle::Solid;
    style.border_top_color = Rgba::WHITE;
    style.border_right_color = Rgba::WHITE;
    style.border_bottom_color = Rgba::WHITE;
    style.border_left_color = Rgba::WHITE;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 12.0, 12.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 12, 12, Rgba::WHITE).expect("paint");

    // Color should be clipped to the first layer's clip (padding-box), leaving border white.
    assert_eq!(color_at(&pixmap, 1, 1), (255, 255, 255, 255));
    assert_eq!(color_at(&pixmap, 3, 3), (255, 0, 0, 255));
  }

  #[test]
  fn overflow_hidden_clips_children() {
    let mut parent_style = ComputedStyle::default();
    parent_style.overflow_x = Overflow::Hidden;
    parent_style.overflow_y = Overflow::Visible;
    parent_style.position = Position::Relative;
    parent_style.background_color = Rgba::BLUE;
    let parent_style = Arc::new(parent_style);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(-5.0, -5.0, 30.0, 40.0),
      vec![],
      Arc::new(child_style),
    );
    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![child],
      parent_style,
    );
    let tree = FragmentTree::new(parent);

    let pixmap = paint_tree(&tree, 40, 40, Rgba::WHITE).expect("paint");
    assert_eq!(color_at(&pixmap, 10, 10), (255, 0, 0, 255));
    // Horizontal overflow should clip; vertical overflow should remain visible.
    assert_eq!(color_at(&pixmap, 22, 2), (0, 0, 255, 255));
    assert_eq!(color_at(&pixmap, 10, 25), (255, 0, 0, 255));
  }

  #[test]
  fn overflow_y_hidden_clips_vertical_only() {
    let mut parent_style = ComputedStyle::default();
    parent_style.overflow_x = Overflow::Visible;
    parent_style.overflow_y = Overflow::Hidden;
    parent_style.position = Position::Relative;
    parent_style.background_color = Rgba::BLUE;
    let parent_style = Arc::new(parent_style);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(-5.0, -5.0, 30.0, 40.0),
      vec![],
      Arc::new(child_style),
    );
    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![child],
      parent_style,
    );
    let tree = FragmentTree::new(parent);

    let pixmap = paint_tree(&tree, 40, 40, Rgba::WHITE).expect("paint");
    // Vertical overflow clipped; horizontal overflow visible.
    assert_eq!(color_at(&pixmap, 10, 10), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 10, 25), (0, 0, 255, 255));
    assert_eq!(color_at(&pixmap, 22, 10), (255, 0, 0, 255));
  }

  #[test]
  fn clip_rect_clips_contents() {
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.background_color = Rgba::RED;
    style.clip = Some(crate::style::types::ClipRect {
      top: ClipComponent::Length(Length::px(5.0)),
      right: ClipComponent::Length(Length::px(15.0)),
      bottom: ClipComponent::Length(Length::px(15.0)),
      left: ClipComponent::Length(Length::px(5.0)),
    });
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");

    // Outside the clip rect should stay white; inside should paint red.
    assert_eq!(color_at(&pixmap, 2, 2), (255, 255, 255, 255));
    assert_eq!(color_at(&pixmap, 10, 10), (255, 0, 0, 255));
  }

  #[test]
  fn overflow_clip_limits_layer_bounds() {
    let style = Arc::new(ComputedStyle::default());
    let root_rect = Rect::from_xywh(0.0, 0.0, 20.0, 20.0);
    let commands = vec![
      DisplayCommand::Background {
        rect: root_rect,
        style: style.clone(),
        text_clip: None,
        scroll_delta: Point::ZERO,
      },
      DisplayCommand::Background {
        rect: Rect::from_xywh(1000.0, 0.0, 10.0, 10.0),
        style,
        text_clip: None,
        scroll_delta: Point::ZERO,
      },
    ];
    let clip = Some(StackingClip {
      rect: root_rect,
      radii: BorderRadii::ZERO,
      clip_x: true,
      clip_y: true,
      clip_root: false,
    });
    let bounds = stacking_context_bounds(
      &commands,
      &[],
      &[],
      root_rect,
      None,
      clip.as_ref(),
      None,
      (20.0, 20.0),
    )
    .expect("bounds");
    assert!((bounds.width() - 1010.0).abs() < 0.01);
    assert!(bounds.min_x().abs() < 0.01);
    assert!((bounds.max_x() - 1010.0).abs() < 0.01);
  }

  #[test]
  fn transformed_child_extends_parent_bounds() {
    let mut parent_style = ComputedStyle::default();
    parent_style.isolation = Isolation::Isolate;
    let parent_style = Arc::new(parent_style);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    child_style.transform = vec![crate::css::types::Transform::Translate(
      Length::px(60.0),
      Length::px(0.0),
    )];
    let child_style = Arc::new(child_style);

    let child =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![], child_style);
    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
      vec![child],
      parent_style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 40.0), vec![parent]);
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 120, 40, Rgba::WHITE).expect("paint");
    // The translated child should remain visible even though the parent stacking context bounds
    // are derived from its untransformed content.
    assert_eq!(color_at(&pixmap, 70, 10), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 5, 5), (255, 255, 255, 255));
  }

  #[test]
  fn object_fit_contain_centers_image() {
    let fit = ObjectFit::Contain;
    let position = crate::style::types::ObjectPosition {
      x: crate::style::types::PositionComponent::Keyword(
        crate::style::types::PositionKeyword::Center,
      ),
      y: crate::style::types::PositionComponent::Keyword(
        crate::style::types::PositionKeyword::Center,
      ),
    };

    let (offset_x, offset_y, dest_w, dest_h) = compute_object_fit(
      fit,
      position,
      200.0,
      100.0,
      100.0,
      100.0,
      true,
      16.0,
      16.0,
      Some((200.0, 100.0)),
    )
    .expect("fit computed");
    assert_eq!(dest_h, 100.0);
    assert_eq!(dest_w, 100.0);
    assert!((offset_x - 50.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn background_cover_scales_to_fill() {
    let mut layer = BackgroundLayer::default();
    layer.size = BackgroundSize::Keyword(BackgroundSizeKeyword::Cover);
    let (tw, th) = compute_background_size(
      &layer,
      16.0,
      16.0,
      (200.0, 100.0),
      200.0,
      100.0,
      50.0,
      50.0,
      Some(1.0),
    );
    let (ox, oy) = resolve_background_offset(
      layer.position,
      200.0,
      100.0,
      tw,
      th,
      16.0,
      16.0,
      (200.0, 100.0),
    );
    assert!((tw - 200.0).abs() < 0.01);
    assert!((th - 200.0).abs() < 0.01);
    assert!((ox - 0.0).abs() < 0.01);
    assert!((oy - 0.0).abs() < 0.01);
  }

  #[test]
  fn background_position_alignment_and_offsets_resolve_against_available_space() {
    let layer = BackgroundLayer {
      position: BackgroundPosition::Position {
        x: crate::style::types::BackgroundPositionComponent {
          alignment: 1.0,
          offset: Length::px(-10.0),
        },
        y: crate::style::types::BackgroundPositionComponent {
          alignment: 1.0,
          offset: Length::percent(-20.0),
        },
      },
      ..BackgroundLayer::default()
    };

    let (ox, oy) = resolve_background_offset(
      layer.position,
      100.0,
      60.0,
      20.0,
      10.0,
      16.0,
      16.0,
      (100.0, 60.0),
    );
    // available_x = 80; 1 * 80 - 10 = 70
    // available_y = 50; 1 * 50 - 20%*50 = 40
    assert!((ox - 70.0).abs() < 0.01);
    assert!((oy - 40.0).abs() < 0.01);
  }

  #[test]
  fn background_size_single_dimension_auto_uses_intrinsic_ratio() {
    let mut layer = BackgroundLayer::default();
    layer.size = BackgroundSize::Explicit(
      BackgroundSizeComponent::Auto,
      BackgroundSizeComponent::Length(Length::px(25.0)),
    );
    let (tw, th) = compute_background_size(
      &layer,
      16.0,
      16.0,
      (200.0, 100.0),
      200.0,
      100.0,
      100.0,
      50.0,
      Some(2.0),
    );
    assert!((tw - 50.0).abs() < 0.01);
    assert!((th - 25.0).abs() < 0.01);
  }

  #[test]
  fn background_size_auto_auto_uses_intrinsic_size_or_falls_back() {
    let layer = BackgroundLayer::default();
    let (tw, th) = compute_background_size(
      &layer,
      16.0,
      16.0,
      (120.0, 80.0),
      120.0,
      80.0,
      30.0,
      10.0,
      Some(3.0),
    );
    assert!((tw - 30.0).abs() < 0.01);
    assert!((th - 10.0).abs() < 0.01);

    let (tw, th) =
      compute_background_size(&layer, 16.0, 16.0, (50.0, 60.0), 50.0, 60.0, 0.0, 0.0, None);
    assert!((tw - 50.0).abs() < 0.01);
    assert!((th - 60.0).abs() < 0.01);
  }

  #[test]
  fn background_repeat_space_distributes_evenly() {
    let positions = tile_positions(
      BackgroundRepeatKeyword::Space,
      0.0,
      100.0,
      30.0,
      0.0,
      0.0,
      100.0,
    );
    assert_eq!(positions.len(), 3);
    assert!((positions[0] - 0.0).abs() < 1e-4);
    assert!((positions[1] - 35.0).abs() < 1e-3);
    assert!((positions[2] - 70.0).abs() < 1e-3);
  }

  #[test]
  fn background_repeat_space_centers_single_tile() {
    let positions = tile_positions(
      BackgroundRepeatKeyword::Space,
      10.0,
      40.0,
      30.0,
      0.0,
      0.0,
      100.0,
    );
    assert_eq!(positions, vec![15.0]); // 10 + (40-30)/2

    let with_offset = tile_positions(
      BackgroundRepeatKeyword::Space,
      0.0,
      40.0,
      30.0,
      5.0,
      0.0,
      100.0,
    );
    assert_eq!(with_offset, vec![10.0]); // centered (5) plus offset (5)
  }

  #[test]
  fn background_repeat_space_centers_oversized_tile() {
    // Tile larger than area => still center it.
    let positions = tile_positions(
      BackgroundRepeatKeyword::Space,
      0.0,
      20.0,
      30.0,
      0.0,
      0.0,
      100.0,
    );
    assert_eq!(positions, vec![-5.0]); // (20-30)/2
  }

  #[test]
  fn background_repeat_round_resizes_to_integer_tiles() {
    let rounded = round_tile_length(1099.0, 100.0);
    assert!((rounded - (1099.0 / 11.0)).abs() < 1e-3);
  }

  #[test]
  fn background_blend_mode_multiplies_layers() {
    let make_style = |blend_mode| {
      let mut style = ComputedStyle::default();
      style.background_color = Rgba::BLUE;
      style.set_background_layers(vec![BackgroundLayer {
        image: Some(BackgroundImage::LinearGradient {
          angle: 0.0,
          stops: vec![
            crate::css::types::ColorStop {
              color: Color::Rgba(Rgba::new(255, 255, 0, 1.0)),
              position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
            },
            crate::css::types::ColorStop {
              color: Color::Rgba(Rgba::new(255, 255, 0, 1.0)),
              position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
            },
          ],
        }),
        repeat: BackgroundRepeat::no_repeat(),
        blend_mode,
        ..BackgroundLayer::default()
      }]);
      style
    };

    let normal_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(make_style(MixBlendMode::Normal)),
    );
    let multiply_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(make_style(MixBlendMode::Multiply)),
    );

    let normal =
      paint_tree(&FragmentTree::new(normal_fragment), 10, 10, Rgba::WHITE).expect("paint");
    let multiplied = paint_tree(&FragmentTree::new(multiply_fragment), 10, 10, Rgba::WHITE)
      .expect("paint multiply");

    assert_eq!(color_at(&normal, 5, 5), (255, 255, 0, 255));
    let blended = color_at(&multiplied, 5, 5);
    assert!(
      blended.0 < 5 && blended.1 < 5 && blended.2 < 5,
      "multiply blend should blacken blue + yellow, got {:?}",
      blended
    );
  }

  #[test]
  fn background_blend_mode_plus_lighter_adds_layers() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::from_rgba8(100, 100, 100, 255);
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::from_rgba8(200, 0, 0, 255)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::from_rgba8(200, 0, 0, 255)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      repeat: BackgroundRepeat::no_repeat(),
      blend_mode: MixBlendMode::PlusLighter,
      ..BackgroundLayer::default()
    }]);

    let fragment =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 2.0, 2.0), vec![], Arc::new(style));
    let pixmap = paint_tree(&FragmentTree::new(fragment), 2, 2, Rgba::WHITE).expect("paint");
    let (r, g, b, _) = color_at(&pixmap, 0, 0);
    assert_eq!(
      (r, g, b),
      (255, 100, 100),
      "plus-lighter background blend should add colors"
    );
  }

  #[test]
  fn mix_blend_mode_hue_preserves_hsl_components() {
    let dst = (30u8, 120u8, 220u8);
    let src = (200u8, 30u8, 30u8);

    let mut root_style = ComputedStyle::default();
    root_style.background_color = Rgba::from_rgba8(dst.0, dst.1, dst.2, 255);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::from_rgba8(src.0, src.1, src.2, 255);
    child_style.mix_blend_mode = MixBlendMode::Hue;

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 2.0, 2.0),
      vec![],
      Arc::new(child_style),
    );
    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 2.0, 2.0), vec![child]);
    root.style = Some(Arc::new(root_style));

    let pixmap = paint_tree(&FragmentTree::new(root), 2, 2, Rgba::WHITE).expect("paint");
    let (r, g, b, _) = color_at(&pixmap, 0, 0);

    let expected = apply_hsl_blend(
      MixBlendMode::Hue,
      (
        src.0 as f32 / 255.0,
        src.1 as f32 / 255.0,
        src.2 as f32 / 255.0,
      ),
      (
        dst.0 as f32 / 255.0,
        dst.1 as f32 / 255.0,
        dst.2 as f32 / 255.0,
      ),
    );
    let expected_hsl = rgb_to_hsl(expected.0, expected.1, expected.2);
    assert_hsl_components(
      (r, g, b),
      expected_hsl,
      0.02,
      0.05,
      0.05,
      "hue mix-blend-mode",
    );
  }

  #[test]
  fn mix_blend_mode_plus_lighter_adds_colors() {
    let mut root_style = ComputedStyle::default();
    root_style.background_color = Rgba::from_rgba8(100, 100, 100, 255);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::from_rgba8(200, 0, 0, 255);
    child_style.mix_blend_mode = MixBlendMode::PlusLighter;

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 2.0, 2.0),
      vec![],
      Arc::new(child_style),
    );
    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 2.0, 2.0), vec![child]);
    root.style = Some(Arc::new(root_style));

    let pixmap = paint_tree(&FragmentTree::new(root), 2, 2, Rgba::WHITE).expect("paint");
    let (r, g, b, _) = color_at(&pixmap, 0, 0);
    assert_eq!(
      (r, g, b),
      (255, 100, 100),
      "plus-lighter should add source and destination colors"
    );
  }

  #[test]
  fn clip_path_polygon_masks_painter_output() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    style.clip_path = crate::style::types::ClipPath::BasicShape(
      Box::new(crate::style::types::BasicShape::Polygon {
        fill: crate::style::types::FillRule::NonZero,
        points: vec![
          (Length::px(0.0), Length::px(0.0)),
          (Length::px(0.0), Length::px(10.0)),
          (Length::px(10.0), Length::px(0.0)),
        ],
      }),
      None,
    );
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap =
      paint_tree(&FragmentTree::new(tree.root.clone()), 10, 10, Rgba::WHITE).expect("paint");

    assert_eq!(color_at(&pixmap, 2, 2), (255, 0, 0, 255));
    assert_eq!(color_at(&pixmap, 9, 9), (255, 255, 255, 255));
  }

  #[test]
  fn background_blend_mode_color_preserves_luminance() {
    let dst = (30u8, 120u8, 220u8);
    let src = (200u8, 30u8, 30u8);

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::from_rgba8(dst.0, dst.1, dst.2, 255);
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::from_rgba8(src.0, src.1, src.2, 255)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::from_rgba8(src.0, src.1, src.2, 255)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      repeat: BackgroundRepeat::no_repeat(),
      blend_mode: MixBlendMode::Color,
      ..BackgroundLayer::default()
    }]);

    let fragment =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 2.0, 2.0), vec![], Arc::new(style));
    let pixmap = paint_tree(&FragmentTree::new(fragment), 2, 2, Rgba::WHITE).expect("paint");
    let (r, g, b, _) = color_at(&pixmap, 0, 0);

    let expected = apply_hsl_blend(
      MixBlendMode::Color,
      (
        src.0 as f32 / 255.0,
        src.1 as f32 / 255.0,
        src.2 as f32 / 255.0,
      ),
      (
        dst.0 as f32 / 255.0,
        dst.1 as f32 / 255.0,
        dst.2 as f32 / 255.0,
      ),
    );
    let expected_hsl = rgb_to_hsl(expected.0, expected.1, expected.2);
    assert_hsl_components(
      (r, g, b),
      expected_hsl,
      0.02,
      0.05,
      0.05,
      "background color blend",
    );
  }

  #[test]
  fn background_layers_paint_top_to_bottom() {
    let mut style = ComputedStyle::default();

    let top = BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::from_rgba8(0, 255, 0, 128)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::from_rgba8(0, 255, 0, 128)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    };
    let bottom = BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      ..BackgroundLayer::default()
    };
    style.set_background_layers(vec![top, bottom]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 10, 10, Rgba::WHITE).expect("paint");

    let center = color_at(&pixmap, 5, 5);
    // Top (semi-transparent green) over opaque blue yields roughly 50/50 mix.
    assert!(
      center.1 > 110 && center.1 < 150 && center.2 > 110 && center.2 < 150 && center.0 == 0,
      "expected blended green-over-blue, got {:?}",
      center
    );
  }

  #[test]
  fn background_layers_use_per_layer_clips() {
    let mut style = ComputedStyle::default();
    style.padding_left = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.padding_top = Length::px(4.0);
    style.padding_bottom = Length::px(4.0);

    let top = BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::GREEN),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::GREEN),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      clip: crate::style::types::BackgroundBox::ContentBox,
      ..BackgroundLayer::default()
    };
    let bottom = BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      clip: crate::style::types::BackgroundBox::BorderBox,
      ..BackgroundLayer::default()
    };
    style.set_background_layers(vec![top, bottom]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");

    // Padding area should see only the border-box layer (blue).
    let padding_px = color_at(&pixmap, 1, 1);
    assert!(
      padding_px.2 > 200 && padding_px.1 < 50,
      "expected blue in padding area, got {:?}",
      padding_px
    );

    // Content area should be covered by the content-clipped top layer (green).
    let content_px = color_at(&pixmap, 10, 10);
    assert!(
      content_px.1 > 200 && content_px.2 < 50,
      "expected green in content area, got {:?}",
      content_px
    );
  }

  #[test]
  fn legacy_generated_linear_gradient_clips_huge_tile_to_viewport() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }]);

    // Large background tile that would exceed MAX_PIXMAP_BYTES if rasterized in full.
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(-9900.0, -9900.0, 20000.0, 20000.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree_with_resources_scaled_offset_backend(
      &tree,
      200,
      200,
      Rgba::WHITE,
      FontContext::new(),
      ImageCache::new(),
      1.0,
      Point::ZERO,
      PaintParallelism::default(),
      &ScrollState::default(),
      PaintBackend::Legacy,
    )
    .expect("paint");

    let top = color_at(&pixmap, 0, 0);
    let bottom = color_at(&pixmap, 0, 199);
    assert!(
      top.2 > top.0 && top.0 > 100 && top.2 > 100 && top.1 < 20,
      "expected purple-ish (blue>red) near top of viewport, got {:?}",
      top
    );
    assert!(
      bottom.0 > bottom.2 && bottom.0 > 100 && bottom.2 > 100 && bottom.1 < 20,
      "expected purple-ish (red>blue) near bottom of viewport, got {:?}",
      bottom
    );
  }

  #[test]
  fn legacy_generated_conic_gradient_clips_huge_tile_to_viewport() {
    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::ConicGradient {
        from_angle: 0.0,
        position: BackgroundPosition::Position {
          x: BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
          y: BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
        },
        stops: vec![
          ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          ColorStop {
            color: Color::Rgba(Rgba::GREEN),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.25)),
          },
          ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.5)),
          },
          ColorStop {
            color: Color::Rgba(Rgba::new(255, 255, 0, 1.0)),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.75)),
          },
          ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }]);

    // Offset the tile by half a pixel so the conic center lands exactly on a device pixel center.
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(-9899.5, -9899.5, 20000.0, 20000.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree_with_resources_scaled_offset_backend(
      &tree,
      200,
      200,
      Rgba::WHITE,
      FontContext::new(),
      ImageCache::new(),
      1.0,
      Point::ZERO,
      PaintParallelism::default(),
      &ScrollState::default(),
      PaintBackend::Legacy,
    )
    .expect("paint");

    let top = color_at(&pixmap, 100, 0);
    assert!(
      top.0 > 200 && top.1 < 50 && top.2 < 50,
      "expected red above the conic center, got {:?}",
      top
    );

    let right = color_at(&pixmap, 199, 100);
    assert!(
      right.1 > 200 && right.0 < 50 && right.2 < 50,
      "expected green to the right of the conic center, got {:?}",
      right
    );

    let bottom = color_at(&pixmap, 100, 199);
    assert!(
      bottom.2 > 200 && bottom.0 < 50 && bottom.1 < 50,
      "expected blue below the conic center, got {:?}",
      bottom
    );

    let left = color_at(&pixmap, 0, 100);
    assert!(
      left.0 > 200 && left.1 > 200 && left.2 < 50,
      "expected yellow to the left of the conic center, got {:?}",
      left
    );
  }

  #[test]
  fn outline_draws_outside_box() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::BLUE;
    style.outline_style = OutlineStyle::Solid;
    style.outline_width = Length::px(4.0);
    style.outline_color = OutlineColor::Color(Rgba::RED);
    style.outline_offset = Length::px(2.0);
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(4.0, 4.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 30, 30, Rgba::WHITE).expect("paint");
    assert_eq!(color_at(&pixmap, 5, 5), (0, 0, 255, 255));
    let mut found_outline = false;
    for y in 0..5 {
      for x in 0..10 {
        let px = color_at(&pixmap, x, y);
        if px != (255, 255, 255, 255) {
          found_outline = true;
        }
      }
    }
    assert!(found_outline, "outline stroke should paint outside the box");
    // With `outline-offset: 2px` and `outline-width: 4px`, the outline's outer edge is 6px away
    // from the element border edge, so it should reach the origin for a box placed at (4px,4px).
    assert_eq!(color_at(&pixmap, 0, 0), (255, 0, 0, 255));
  }

  #[test]
  fn filters_expand_clip_under_radii() {
    let mut painter = Painter::with_resources_scaled(
      4,
      4,
      Rgba::TRANSPARENT,
      FontContext::new(),
      ImageCache::new(),
      1.0,
    )
    .unwrap();
    painter.fill_background();

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    let style = Arc::new(style);

    let cmd = DisplayCommand::StackingContext {
      rect: Rect::from_xywh(1.0, 1.0, 2.0, 2.0),
      opacity: 1.0,
      transform: None,
      transform_3d: None,
      blend_mode: MixBlendMode::Normal,
      isolated: true,
      mask: None,
      mask_border: None,
      filters: vec![ResolvedFilter::Blur(1.0)],
      backdrop_filters: Vec::new(),
      radii: BorderRadii::uniform(0.5),
      clip: None,
      has_clip_path: false,
      clip_path: None,
      root_style: None,
      commands: vec![DisplayCommand::Background {
        rect: Rect::from_xywh(1.0, 1.0, 2.0, 2.0),
        style,
        text_clip: None,
        scroll_delta: Point::ZERO,
      }],
    };

    painter.execute_command(cmd).unwrap();
    let alpha_near = painter.pixmap.pixel(0, 1).unwrap().alpha();
    assert!(
      alpha_near > 0,
      "expected blur spill outside rounded rect; alpha at (0,1) was {alpha_near}"
    );
    assert!(
      alpha_near < 255,
      "blurred spill should be softer than solid fill; alpha at (0,1) was {alpha_near}"
    );
  }

  #[test]
  fn backdrop_filters_follow_transforms() {
    let mut painter = Painter::new(24, 16, Rgba::BLUE).expect("painter");
    painter.fill_background();

    let cmd = DisplayCommand::StackingContext {
      rect: Rect::from_xywh(0.0, 0.0, 6.0, 6.0),
      opacity: 1.0,
      transform: Some(Transform::from_translate(12.0, 8.0)),
      transform_3d: Some(Transform3D::from_2d(&Transform2D::translate(12.0, 8.0))),
      blend_mode: MixBlendMode::Normal,
      isolated: false,
      mask: None,
      mask_border: None,
      filters: Vec::new(),
      backdrop_filters: vec![ResolvedFilter::Invert(1.0)],
      radii: BorderRadii::ZERO,
      clip: None,
      has_clip_path: false,
      clip_path: None,
      root_style: None,
      commands: Vec::new(),
    };

    painter.execute_command(cmd).expect("execute");
    let pixmap = painter.pixmap;

    let origin_px = color_at(&pixmap, 1, 1);
    assert_eq!(
      origin_px,
      (0, 0, 255, 255),
      "backdrop filter should not affect the origin"
    );

    let mut inverted = Vec::new();
    let (w, h) = (pixmap.width(), pixmap.height());
    for y in 0..h {
      for x in 0..w {
        let (r, g, b, _) = color_at(&pixmap, x, y);
        if r > 200 && g > 200 && b < 80 {
          inverted.push((x, y));
        }
      }
    }
    let expected: Vec<(u32, u32)> = (8..14)
      .flat_map(|y| (12..18).map(move |x| (x, y)))
      .collect();
    assert_eq!(
      inverted, expected,
      "backdrop filter should track the translated box"
    );
  }

  #[test]
  fn backdrop_filters_cover_bounds() {
    let mut pixmap = new_pixmap(10, 10).expect("pixmap");
    pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 255, 255));

    let bounds = Rect::from_xywh(2.0, 3.0, 4.0, 2.0);
    let filters = vec![ResolvedFilter::Invert(1.0)];
    apply_backdrop_filters(
      &mut pixmap,
      &bounds,
      &filters,
      BorderRadii::ZERO,
      1.0,
      bounds,
    )
    .unwrap();

    let mut inverted = Vec::new();
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        let (r, g, b, _) = color_at(&pixmap, x, y);
        if r > 200 && g > 200 && b < 80 {
          inverted.push((x, y));
        }
      }
    }

    let expected: Vec<(u32, u32)> = (3..5).flat_map(|y| (2..6).map(move |x| (x, y))).collect();
    assert_eq!(
      inverted, expected,
      "backdrop filter should invert the full bounds"
    );
  }

  #[test]
  fn clip_path_masks_stacking_contents() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    style.clip_path = ClipPath::BasicShape(
      Box::new(crate::style::types::BasicShape::Circle {
        radius: ShapeRadius::Length(Length::px(3.0)),
        position: BackgroundPosition::Position {
          x: BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
          y: BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
        },
      }),
      None,
    );

    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
    root.style = Some(Arc::new(style));
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 10, 10, Rgba::WHITE).expect("painted");
    let center = pixmap.pixel(5, 5).expect("center pixel");
    let corner = pixmap.pixel(0, 0).expect("corner pixel");

    assert!(center.red() > 200 && center.green() < 60 && center.blue() < 60);
    assert_eq!(
      (corner.red(), corner.green(), corner.blue(), corner.alpha()),
      (255, 255, 255, 255)
    );
  }

  #[test]
  fn clip_path_polygon_masks_paint_output() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    style.clip_path = ClipPath::BasicShape(
      Box::new(crate::style::types::BasicShape::Polygon {
        fill: crate::style::types::FillRule::NonZero,
        points: vec![
          (Length::px(0.0), Length::px(0.0)),
          (Length::px(0.0), Length::px(10.0)),
          (Length::px(10.0), Length::px(0.0)),
        ],
      }),
      None,
    );

    let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
    root.style = Some(Arc::new(style));
    let tree = FragmentTree::new(root);

    let pixmap = paint_tree(&tree, 10, 10, Rgba::WHITE).expect("painted");
    let inside = pixmap.pixel(2, 2).expect("inside pixel");
    let outside = pixmap.pixel(9, 9).expect("outside pixel");

    assert!(inside.red() > 200 && inside.green() < 60 && inside.blue() < 60);
    assert_eq!(
      (
        outside.red(),
        outside.green(),
        outside.blue(),
        outside.alpha()
      ),
      (255, 255, 255, 255)
    );
  }

  #[test]
  fn outline_not_clipped_by_overflow_hidden() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::WHITE;
    style.outline_style = OutlineStyle::Solid;
    style.outline_width = Length::px(4.0);
    style.outline_color = OutlineColor::Color(Rgba::RED);
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(10.0, 10.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 40, 40, Rgba::WHITE).expect("paint");
    // Outline should extend beyond the 10..20 box even though overflow is hidden.
    assert_eq!(color_at(&pixmap, 8, 15), (255, 0, 0, 255));
  }

  #[test]
  fn outline_not_clipped_by_clip_path() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::WHITE;
    style.outline_style = OutlineStyle::Solid;
    style.outline_width = Length::px(4.0);
    style.outline_color = OutlineColor::Color(Rgba::RED);
    style.clip_path = ClipPath::BasicShape(
      Box::new(crate::style::types::BasicShape::Circle {
        radius: ShapeRadius::Length(Length::px(2.0)),
        position: BackgroundPosition::Position {
          x: BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
          y: BackgroundPositionComponent {
            alignment: 0.5,
            offset: Length::px(0.0),
          },
        },
      }),
      None,
    );

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(5.0, 5.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");
    // Outline should remain visible outside the clip-path bounds.
    assert_eq!(color_at(&pixmap, 4, 10), (255, 0, 0, 255));
  }

  #[test]
  fn visibility_hidden_prevents_painting() {
    let mut style = ComputedStyle::default();
    style.visibility = crate::style::computed::Visibility::Hidden;
    style.background_color = Rgba::RED;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");
    assert_eq!(color_at(&pixmap, 5, 5), (255, 255, 255, 255));
  }

  #[test]
  fn visibility_collapse_prevents_painting() {
    let mut style = ComputedStyle::default();
    style.visibility = crate::style::computed::Visibility::Collapse;
    style.background_color = Rgba::BLUE;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");
    assert_eq!(color_at(&pixmap, 5, 5), (255, 255, 255, 255));
  }

  #[test]
  fn backface_hidden_prevents_painting() {
    let mut style = ComputedStyle::default();
    style.backface_visibility = BackfaceVisibility::Hidden;
    style.transform.push(css::types::Transform::RotateX(180.0));
    style.background_color = Rgba::RED;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");
    assert_eq!(color_at(&pixmap, 5, 5), (255, 255, 255, 255));
  }

  #[test]
  fn perspective_transform_warps_layer() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    style.perspective = Some(Length::px(400.0));
    style
      .transform
      .push(crate::css::types::Transform::RotateY(45.0));
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(10.0, 10.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree_scaled(&tree, 64, 40, Rgba::WHITE, 1.0).expect("paint");

    let mut min_x = pixmap.width();
    let mut min_y = pixmap.height();
    let mut max_x = 0;
    let mut max_y = 0;
    let mut count = 0;
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        let (r, g, b, a) = color_at(&pixmap, x, y);
        if a > 0 && (r != 255 || g != 255 || b != 255) {
          count += 1;
          min_x = min_x.min(x);
          min_y = min_y.min(y);
          max_x = max_x.max(x);
          max_y = max_y.max(y);
        }
      }
    }

    assert!(count > 0, "expected warped content to paint");

    let bounds = Rect::from_xywh(
      min_x as f32,
      min_y as f32,
      (max_x - min_x + 1) as f32,
      (max_y - min_y + 1) as f32,
    );
    let expected = Rect::from_xywh(10.0, 10.0, 20.0, 20.0);
    assert!(
      bounds.width() < expected.width() - 0.5,
      "perspective should shrink projected width; expected < {}, got {}",
      expected.width(),
      bounds.width()
    );
  }

  #[test]
  fn backface_hidden_with_perspective_culls() {
    let mut style = ComputedStyle::default();
    style.backface_visibility = BackfaceVisibility::Hidden;
    style.perspective = Some(Length::px(400.0));
    style.transform.push(css::types::Transform::RotateX(190.0));
    style.background_color = Rgba::RED;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let tree = FragmentTree::new(fragment);

    let pixmap = paint_tree(&tree, 20, 20, Rgba::WHITE).expect("paint");
    assert_eq!(color_at(&pixmap, 5, 5), (255, 255, 255, 255));
  }

  #[test]
  fn exif_orientation_rotates_images_by_default() {
    let mut painter =
      Painter::with_resources(1, 2, Rgba::WHITE, FontContext::new(), ImageCache::new())
        .expect("painter");
    let style = ComputedStyle::default();
    let ok = painter.paint_image_from_src(
      &crate::tree::box_tree::SelectedImageSource {
        url: "tests/fixtures/image_orientation/orientation-6.jpg",
        descriptor: None,
        density: None,
        from_picture: false,
      },
      CrossOriginAttribute::None,
      None,
      Some(&style),
      0.0,
      0.0,
      1.0,
      2.0,
      None,
    );
    assert!(ok, "image should paint");
    let top = color_at(&painter.pixmap, 0, 0);
    let bottom = color_at(&painter.pixmap, 0, 1);
    assert!(
      top.0 > top.1 && top.0 > top.2,
      "expected red-dominant pixel at top after orientation, got {:?}",
      top
    );
    assert!(
      bottom.1 > bottom.0 && bottom.1 > bottom.2,
      "expected green-dominant pixel at bottom after orientation, got {:?}",
      bottom
    );
  }

  #[test]
  fn image_orientation_none_ignores_metadata() {
    let mut painter =
      Painter::with_resources(2, 1, Rgba::WHITE, FontContext::new(), ImageCache::new())
        .expect("painter");
    let mut style = ComputedStyle::default();
    style.image_orientation = ImageOrientation::None;
    let ok = painter.paint_image_from_src(
      &crate::tree::box_tree::SelectedImageSource {
        url: "tests/fixtures/image_orientation/orientation-6.jpg",
        descriptor: None,
        density: None,
        from_picture: false,
      },
      CrossOriginAttribute::None,
      None,
      Some(&style),
      0.0,
      0.0,
      2.0,
      1.0,
      None,
    );
    assert!(ok, "image should paint");
    let left = color_at(&painter.pixmap, 0, 0);
    let right = color_at(&painter.pixmap, 1, 0);
    assert!(
      left.0 > left.1 && left.0 > left.2,
      "expected red-dominant pixel on the left without orientation, got {:?}",
      left
    );
    assert!(
      right.1 > right.0 && right.1 > right.2,
      "expected green-dominant pixel on the right without orientation, got {:?}",
      right
    );
  }

  #[test]
  fn paints_border_image_nine_slice() {
    let mut img = RgbaImage::new(3, 3);
    img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255])); // TL
    img.put_pixel(2, 0, image::Rgba([0, 0, 255, 255])); // TR
    img.put_pixel(0, 2, image::Rgba([0, 255, 0, 255])); // BL
    img.put_pixel(2, 2, image::Rgba([255, 255, 0, 255])); // BR
    let edge = image::Rgba([0, 255, 255, 255]);
    img.put_pixel(1, 0, edge); // top edge
    img.put_pixel(1, 2, edge); // bottom edge
    img.put_pixel(0, 1, edge); // left edge
    img.put_pixel(2, 1, edge); // right edge
    img.put_pixel(1, 1, image::Rgba([255, 255, 255, 255])); // center

    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
      .write_image(img.as_raw(), 3, 3, image::ExtendedColorType::Rgba8)
      .unwrap();
    let data_url = format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&buf)
    );

    let mut style = ComputedStyle::default();
    style.border_top_width = Length::px(4.0);
    style.border_right_width = Length::px(4.0);
    style.border_bottom_width = Length::px(4.0);
    style.border_left_width = Length::px(4.0);
    style.border_top_style = CssBorderStyle::Solid;
    style.border_right_style = CssBorderStyle::Solid;
    style.border_bottom_style = CssBorderStyle::Solid;
    style.border_left_style = CssBorderStyle::Solid;
    style.border_image = BorderImage {
      source: BorderImageSource::Image(Box::new(BackgroundImage::Url(BackgroundImageUrl::new(
        data_url,
      )))),
      slice: BorderImageSlice {
        top: BorderImageSliceValue::Number(1.0),
        right: BorderImageSliceValue::Number(1.0),
        bottom: BorderImageSliceValue::Number(1.0),
        left: BorderImageSliceValue::Number(1.0),
        fill: false,
      },
      ..BorderImage::default()
    };

    let mut painter = Painter::new(16, 16, Rgba::WHITE).expect("painter");
    painter.paint_borders(0.0, 0.0, 16.0, 16.0, &style, None);

    let tl = painter.pixmap.pixel(0, 0).unwrap();
    let tr = painter.pixmap.pixel(15, 0).unwrap();
    let bl = painter.pixmap.pixel(0, 15).unwrap();
    let br = painter.pixmap.pixel(15, 15).unwrap();
    assert_eq!((tl.red(), tl.green(), tl.blue()), (255, 0, 0));
    assert_eq!((tr.red(), tr.green(), tr.blue()), (0, 0, 255));
    assert_eq!((bl.red(), bl.green(), bl.blue()), (0, 255, 0));
    assert_eq!((br.red(), br.green(), br.blue()), (255, 255, 0));
    // Edge samples
    let edge_sample = painter.pixmap.pixel(8, 1).unwrap();
    assert_eq!(
      (edge_sample.red(), edge_sample.green(), edge_sample.blue()),
      (0, 255, 255)
    );
    let edge_sample_left = painter.pixmap.pixel(1, 8).unwrap();
    assert_eq!(
      (
        edge_sample_left.red(),
        edge_sample_left.green(),
        edge_sample_left.blue()
      ),
      (0, 255, 255)
    );
  }

  #[test]
  fn thin_double_border_falls_back_to_solid() {
    let mut style = ComputedStyle::default();
    style.border_top_width = Length::px(1.0);
    style.border_right_width = Length::px(1.0);
    style.border_bottom_width = Length::px(1.0);
    style.border_left_width = Length::px(1.0);
    style.border_top_style = CssBorderStyle::Double;
    style.border_right_style = CssBorderStyle::Double;
    style.border_bottom_style = CssBorderStyle::Double;
    style.border_left_style = CssBorderStyle::Double;
    style.border_top_color = Rgba::from_rgba8(0, 0, 0, 255);
    style.border_right_color = Rgba::from_rgba8(0, 0, 0, 255);
    style.border_bottom_color = Rgba::from_rgba8(0, 0, 0, 255);
    style.border_left_color = Rgba::from_rgba8(0, 0, 0, 255);

    let mut painter = Painter::new(6, 6, Rgba::WHITE).expect("painter");
    painter.paint_borders(0.0, 0.0, 6.0, 6.0, &style, None);

    // The top edge should paint a solid 1px line when double is too thin.
    for x in 0..6 {
      let pixel = painter.pixmap.pixel(x, 0).unwrap();
      assert_eq!((pixel.red(), pixel.green(), pixel.blue()), (0, 0, 0));
      assert!(
        pixel.alpha() >= 180,
        "expected visible solid stroke at ({},0) with alpha >= 180, got {}",
        x,
        pixel.alpha()
      );
    }
  }

  #[test]
  fn border_image_space_distributes_gaps() {
    let mut img = RgbaImage::new(3, 3);
    let magenta = image::Rgba([255, 0, 255, 255]);
    // Fill edges
    for x in 0..3 {
      img.put_pixel(x, 0, magenta);
      img.put_pixel(x, 2, magenta);
    }
    for y in 0..3 {
      img.put_pixel(0, y, magenta);
      img.put_pixel(2, y, magenta);
    }
    img.put_pixel(1, 1, image::Rgba([255, 255, 255, 255])); // center

    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
      .write_image(img.as_raw(), 3, 3, image::ExtendedColorType::Rgba8)
      .unwrap();
    let data_url = format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&buf)
    );

    let mut style = ComputedStyle::default();
    style.border_top_width = Length::px(3.0);
    style.border_right_width = Length::px(3.0);
    style.border_bottom_width = Length::px(3.0);
    style.border_left_width = Length::px(3.0);
    style.border_top_style = CssBorderStyle::Solid;
    style.border_right_style = CssBorderStyle::Solid;
    style.border_bottom_style = CssBorderStyle::Solid;
    style.border_left_style = CssBorderStyle::Solid;
    style.border_image = BorderImage {
      source: BorderImageSource::Image(Box::new(BackgroundImage::Url(BackgroundImageUrl::new(
        data_url,
      )))),
      slice: BorderImageSlice {
        top: BorderImageSliceValue::Number(1.0),
        right: BorderImageSliceValue::Number(1.0),
        bottom: BorderImageSliceValue::Number(1.0),
        left: BorderImageSliceValue::Number(1.0),
        fill: false,
      },
      repeat: (BorderImageRepeat::Space, BorderImageRepeat::Space),
      ..BorderImage::default()
    };

    let mut painter = Painter::new(14, 14, Rgba::WHITE).expect("painter");
    painter.fill_background();
    painter.paint_borders(0.0, 0.0, 14.0, 14.0, &style, None);

    // Top edge has a gap between tiles when spaced.
    let gap_top = painter.pixmap.pixel(7, 1).unwrap();
    assert_eq!(
      (gap_top.red(), gap_top.green(), gap_top.blue()),
      (255, 255, 255)
    );
    let painted_top = painter.pixmap.pixel(4, 1).unwrap();
    assert_eq!(
      (painted_top.red(), painted_top.green(), painted_top.blue()),
      (255, 0, 255)
    );

    // Left edge similarly spaces tiles vertically.
    let gap_left = painter.pixmap.pixel(1, 7).unwrap();
    assert_eq!(
      (gap_left.red(), gap_left.green(), gap_left.blue()),
      (255, 255, 255)
    );
    let painted_left = painter.pixmap.pixel(1, 4).unwrap();
    assert_eq!(
      (
        painted_left.red(),
        painted_left.green(),
        painted_left.blue()
      ),
      (255, 0, 255)
    );
  }

  #[test]
  fn paint_border_patch_space_repeat_does_not_panic_on_pathological_tile_counts() {
    let mut painter = Painter::new(4, 4, Rgba::WHITE).expect("painter");

    let mut source = Pixmap::new(1, 1).expect("pixmap");
    source.fill(tiny_skia::Color::from_rgba8(255, 0, 255, 255));
    let src_rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

    // Extremely thin destination height makes the computed tile width tiny, which historically led
    // to `Vec::with_capacity(count as usize)` panicking when count overflowed.
    let dest_rect_x = Rect::from_xywh(0.0, 0.0, 10.0, 1e-30);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      painter.paint_border_patch(
        &source,
        src_rect,
        dest_rect_x,
        BorderImageRepeat::Space,
        BorderImageRepeat::Stretch,
      );
    }));
    assert!(result.is_ok());

    // Mirror the failure mode for the y axis by making the destination width tiny.
    let dest_rect_y = Rect::from_xywh(0.0, 0.0, 1e-30, 10.0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      painter.paint_border_patch(
        &source,
        src_rect,
        dest_rect_y,
        BorderImageRepeat::Stretch,
        BorderImageRepeat::Space,
      );
    }));
    assert!(result.is_ok());
  }

  #[test]
  fn border_image_accepts_gradients() {
    let mut style = ComputedStyle::default();
    style.border_top_width = Length::px(4.0);
    style.border_right_width = Length::px(4.0);
    style.border_bottom_width = Length::px(4.0);
    style.border_left_width = Length::px(4.0);
    style.border_top_style = CssBorderStyle::Solid;
    style.border_right_style = CssBorderStyle::Solid;
    style.border_bottom_style = CssBorderStyle::Solid;
    style.border_left_style = CssBorderStyle::Solid;
    style.border_image = BorderImage {
      source: BorderImageSource::Image(Box::new(BackgroundImage::LinearGradient {
        angle: 180.0,
        stops: vec![
          ColorStop {
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
            color: crate::style::color::Color::Rgba(Rgba::new(255, 0, 0, 1.0)),
          },
          ColorStop {
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
            color: crate::style::color::Color::Rgba(Rgba::new(0, 0, 255, 1.0)),
          },
        ],
      })),
      slice: BorderImageSlice {
        top: BorderImageSliceValue::Number(1.0),
        right: BorderImageSliceValue::Number(1.0),
        bottom: BorderImageSliceValue::Number(1.0),
        left: BorderImageSliceValue::Number(1.0),
        fill: false,
      },
      ..BorderImage::default()
    };

    let mut painter = Painter::new(16, 16, Rgba::WHITE).expect("painter");
    painter.fill_background();
    painter.paint_borders(0.0, 0.0, 16.0, 16.0, &style, None);

    let top = painter.pixmap.pixel(8, 0).unwrap();
    assert!(
      top.red() > top.blue(),
      "top should be red-ish, got {:?}",
      top
    );
    let bottom = painter.pixmap.pixel(8, 15).unwrap();
    assert!(
      bottom.blue() > bottom.red(),
      "bottom should be blue-ish, got {:?}",
      bottom
    );
    let center = painter.pixmap.pixel(8, 8).unwrap();
    assert_eq!(
      (center.red(), center.green(), center.blue()),
      (255, 255, 255),
      "center should remain unfilled without border-image fill"
    );
  }

  #[test]
  fn transform_box_uses_content_box_for_translate_percentage() {
    let mut style = ComputedStyle::default();
    style.transform_box = TransformBox::ContentBox;
    style.padding_left = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.border_left_width = Length::px(5.0);
    style.border_right_width = Length::px(5.0);
    style
      .transform
      .push(crate::css::types::Transform::Translate(
        Length::percent(50.0),
        Length::percent(0.0),
      ));

    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 100.0);
    let transform = build_transform_3d(Some(&style), bounds, None).expect("transform should build");
    let transform = transform.to_2d().expect("should be affine");

    assert!((transform.e - 85.0).abs() < 1e-3);
    assert!(transform.f.abs() < 1e-3);
  }

  #[test]
  fn transform_box_moves_origin_into_content_box() {
    let mut style = ComputedStyle::default();
    style.transform_box = TransformBox::ContentBox;
    style.padding_left = Length::px(10.0);
    style.border_left_width = Length::px(5.0);
    style.transform_origin = crate::style::types::TransformOrigin {
      x: Length::percent(0.0),
      y: Length::percent(0.0),
      z: Length::px(0.0),
    };
    style
      .transform
      .push(crate::css::types::Transform::Scale(2.0, 1.0));

    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 100.0);
    let transform = build_transform_3d(Some(&style), bounds, None).expect("transform should build");
    let transform = transform.to_2d().expect("should be affine");

    assert!((transform.e + 15.0).abs() < 1e-3);
    assert!(transform.f.abs() < 1e-3);
  }

  #[test]
  fn gradient_background_respects_size_and_repeat() {
    let mut style = ComputedStyle::default();
    style.background_layers = smallvec::smallvec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 90.0,
        stops: vec![
          ColorStop {
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
            color: crate::style::color::Color::Rgba(Rgba::new(255, 0, 0, 1.0)),
          },
          ColorStop {
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
            color: crate::style::color::Color::Rgba(Rgba::new(0, 0, 255, 1.0)),
          },
        ],
      }),
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(4.0)),
        BackgroundSizeComponent::Length(Length::px(2.0)),
      ),
      ..BackgroundLayer::default()
    }];
    style.background_color = Rgba::WHITE;

    let mut painter = Painter::new(12, 6, Rgba::WHITE).expect("painter");
    painter.fill_background();
    painter.paint_background(0.0, 0.0, 12.0, 6.0, &style, Point::ZERO);

    let top_left = painter.pixmap.pixel(0, 0).unwrap();
    let top_right_tile = painter.pixmap.pixel(3, 0).unwrap();
    let next_tile = painter.pixmap.pixel(5, 0).unwrap();

    assert!(
      top_left.red() > top_left.blue(),
      "first tile should start red, got {:?}",
      top_left
    );
    assert!(
      top_right_tile.blue() > top_right_tile.red(),
      "end of first tile should be blue-ish, got {:?}",
      top_right_tile
    );
    assert!(
      next_tile.red() > next_tile.blue(),
      "second tile should repeat starting red, got {:?}",
      next_tile
    );
  }

  #[test]
  fn painter_applies_variable_font_variations() {
    let font_bytes = match fs::read(crate::testing::fixtures_dir().join("fonts/TestVar.ttf")) {
      Ok(bytes) => bytes,
      Err(_) => return,
    };
    let mut db = FontDatabase::empty();
    db.load_font_data(font_bytes).expect("load variable font");
    let font_ctx = FontContext::with_database(Arc::new(db));

    let mut base_style = ComputedStyle::default();
    base_style.font_family = vec!["TestVar".to_string()].into();
    base_style.font_size = 96.0;

    let mut light_style = base_style.clone();
    light_style.font_weight = FontWeight::Number(100);
    let mut heavy_style = base_style;
    heavy_style.font_weight = FontWeight::Number(900);

    let pipeline = ShapingPipeline::new();
    let light_run = pipeline
      .shape("A", &light_style, &font_ctx)
      .expect("shape light run");
    let heavy_run = pipeline
      .shape("A", &heavy_style, &font_ctx)
      .expect("shape heavy run");
    if light_run.is_empty() || heavy_run.is_empty() {
      return;
    }

    let render_run = |run: &ShapedRun| {
      let mut painter =
        Painter::with_resources(200, 150, Rgba::WHITE, font_ctx.clone(), ImageCache::new())
          .expect("painter");
      painter.paint_shaped_run(run, 40.0, 110.0, Rgba::BLACK, None, None);
      painter.pixmap
    };

    let light_pixmap = render_run(&light_run[0]);
    let heavy_pixmap = render_run(&heavy_run[0]);

    let light_box = bounding_box_for_color(&light_pixmap, |c| c.3 > 0).expect("light glyph paints");
    let heavy_box = bounding_box_for_color(&heavy_pixmap, |c| c.3 > 0).expect("heavy glyph paints");

    let light_width = light_box.2 - light_box.0;
    let heavy_width = heavy_box.2 - heavy_box.0;
    assert!(
      heavy_width > light_width,
      "heavier variation should widen glyph (light width {}, heavy width {})",
      light_width,
      heavy_width
    );
  }

  #[test]
  fn underline_skip_ink_uses_variable_font_bounds() {
    let font_bytes = match fs::read(crate::testing::fixtures_dir().join("fonts/TestVar.ttf")) {
      Ok(bytes) => bytes,
      Err(_) => return,
    };
    let mut db = FontDatabase::empty();
    db.load_font_data(font_bytes).expect("load variable font");
    let font_ctx = FontContext::with_database(Arc::new(db));

    let mut base_style = ComputedStyle::default();
    base_style.font_family = vec!["TestVar".to_string()].into();
    base_style.font_size = 64.0;

    let mut light_style = base_style.clone();
    light_style.font_weight = FontWeight::Number(100);
    let mut heavy_style = base_style;
    heavy_style.font_weight = FontWeight::Number(900);

    let pipeline = ShapingPipeline::new();
    let light_runs = match pipeline.shape("A", &light_style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    let heavy_runs = match pipeline.shape("A", &heavy_style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if light_runs.is_empty() || heavy_runs.is_empty() {
      return;
    }

    let light_bounds =
      collect_underline_exclusions(&light_runs, 0.0, 0.0, -1000.0, 1000.0, true, 1.0, 0.0);
    let heavy_bounds =
      collect_underline_exclusions(&heavy_runs, 0.0, 0.0, -1000.0, 1000.0, true, 1.0, 0.0);
    if light_bounds.is_empty() || heavy_bounds.is_empty() {
      return;
    }

    let light_width = light_bounds[0].1 - light_bounds[0].0;
    let heavy_width = heavy_bounds[0].1 - heavy_bounds[0].0;
    assert!(
      heavy_width > light_width,
      "skip-ink bounds should reflect variations (light {}, heavy {})",
      light_width,
      heavy_width
    );
  }

  #[test]
  fn painter_applies_variable_font_variations_vertical_runs() {
    let font_bytes = match fs::read(crate::testing::fixtures_dir().join("fonts/TestVar.ttf")) {
      Ok(bytes) => bytes,
      Err(_) => return,
    };
    let mut db = FontDatabase::empty();
    db.load_font_data(font_bytes).expect("load variable font");
    let font_ctx = FontContext::with_database(Arc::new(db));

    let mut base_style = ComputedStyle::default();
    base_style.font_family = vec!["TestVar".to_string()].into();
    base_style.font_size = 96.0;

    let mut light_style = base_style.clone();
    light_style.font_weight = FontWeight::Number(100);
    let mut heavy_style = base_style;
    heavy_style.font_weight = FontWeight::Number(900);

    let pipeline = ShapingPipeline::new();
    let light_runs = match pipeline.shape("A", &light_style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    let heavy_runs = match pipeline.shape("A", &heavy_style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if light_runs.is_empty() || heavy_runs.is_empty() {
      return;
    }

    let render_vertical = |run: &ShapedRun| {
      let mut painter =
        Painter::with_resources(200, 200, Rgba::WHITE, font_ctx.clone(), ImageCache::new())
          .expect("painter");
      painter.paint_shaped_run_vertical(run, 40.0, 120.0, Rgba::BLACK, None, None);
      painter.pixmap
    };

    let light_pixmap = render_vertical(&light_runs[0]);
    let heavy_pixmap = render_vertical(&heavy_runs[0]);

    let light_box = bounding_box_for_color(&light_pixmap, |c| c.3 > 0).expect("light glyph paints");
    let heavy_box = bounding_box_for_color(&heavy_pixmap, |c| c.3 > 0).expect("heavy glyph paints");

    let light_area = (light_box.2 - light_box.0) * (light_box.3 - light_box.1);
    let heavy_area = (heavy_box.2 - heavy_box.0) * (heavy_box.3 - heavy_box.1);
    assert!(
      heavy_area > light_area,
      "vertical runs should expand with heavier variations (light area {}, heavy area {})",
      light_area,
      heavy_area
    );
  }

  #[test]
  fn emphasis_string_applies_variable_font_variations() {
    let font_bytes = match fs::read(crate::testing::fixtures_dir().join("fonts/TestVar.ttf")) {
      Ok(bytes) => bytes,
      Err(_) => return,
    };
    let mut db = FontDatabase::empty();
    db.load_font_data(font_bytes).expect("load variable font");
    let font_ctx = FontContext::with_database(Arc::new(db));

    let mut base_style = ComputedStyle::default();
    base_style.font_family = vec!["TestVar".to_string()].into();
    base_style.font_size = 64.0;
    base_style.text_emphasis_style = crate::style::types::TextEmphasisStyle::String("A".into());
    base_style.text_emphasis_color = Some(Rgba::BLACK);
    base_style.color = Rgba::BLACK;

    let mut light_style = base_style.clone();
    light_style.font_weight = FontWeight::Number(100);
    let mut heavy_style = base_style;
    heavy_style.font_weight = FontWeight::Number(900);

    let pipeline = ShapingPipeline::new();
    let light_runs = match pipeline.shape("A", &light_style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    let heavy_runs = match pipeline.shape("A", &heavy_style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if light_runs.is_empty() || heavy_runs.is_empty() {
      return;
    }

    let render_emphasis = |style: &ComputedStyle, runs: &[ShapedRun]| {
      let mut painter =
        Painter::with_resources(200, 200, Rgba::WHITE, font_ctx.clone(), ImageCache::new())
          .expect("painter");
      painter.paint_text_emphasis(style, Some(runs), 60.0, 120.0, false);
      painter.pixmap
    };

    let light_pixmap = render_emphasis(&light_style, &light_runs);
    let heavy_pixmap = render_emphasis(&heavy_style, &heavy_runs);

    let light_box = bounding_box_for_color(&light_pixmap, |c| c.3 > 0).expect("light mark paints");
    let heavy_box = bounding_box_for_color(&heavy_pixmap, |c| c.3 > 0).expect("heavy mark paints");

    let light_width = light_box.2 - light_box.0;
    let heavy_width = heavy_box.2 - heavy_box.0;
    assert!(
      heavy_width > light_width,
      "string emphasis marks should widen with heavier variations (light width {}, heavy width {})",
      light_width,
      heavy_width
    );
  }

  #[test]
  fn snap_upscale_prefers_integer_factor() {
    assert_eq!(snap_upscale(5.0, 2.0), Some((4.0, 0.5)));
    assert_eq!(snap_upscale(2.0, 2.0), None);
    assert_eq!(snap_upscale(1.0, 3.0), None);
  }

  #[test]
  fn iframe_srcdoc_respects_max_depth() {
    let inner = "<style>html, body { margin: 0; padding: 0; background: rgb(255, 0, 0); }</style>";
    let outer = format!(
      r#"
        <style>
          html, body {{ margin: 0; padding: 0; background: rgb(0, 255, 0); }}
          iframe {{ border: 0; width: 100vw; height: 100vh; display: block; }}
        </style>
        <iframe srcdoc='{inner}'></iframe>
      "#,
      inner = inner
    );

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Iframe {
          src: String::new(),
          srcdoc: Some(outer),
          sandbox: crate::tree::box_tree::IframeSandboxAttribute::None,
          referrer_policy: None,
          frame_token: None,
        },
      },
      vec![],
      Arc::new(ComputedStyle::default()),
    );
    let tree = FragmentTree::new(fragment);

    let blocked = Painter::new(8, 8, Rgba::WHITE)
      .expect("painter")
      .with_max_iframe_depth(1)
      .paint(&tree)
      .expect("paint");
    let blocked_pixel = blocked.pixel(4, 4).unwrap();
    assert!(
      blocked_pixel.green() > blocked_pixel.red(),
      "inner iframe should be blocked when depth is exhausted"
    );

    let allowed = Painter::new(8, 8, Rgba::WHITE)
      .expect("painter")
      .with_max_iframe_depth(2)
      .paint(&tree)
      .expect("paint");
    let allowed_pixel = allowed.pixel(4, 4).unwrap();
    assert!(
      allowed_pixel.red() > allowed_pixel.green(),
      "inner iframe should render when depth permits"
    );
  }

  #[test]
  fn iframe_srcdoc_renders_inline_content() {
    let html = r"
        <style>html, body { margin: 0; padding: 0; background: red; }</style>
        ";
    let painter = Painter::new(20, 20, Rgba::WHITE).expect("painter");
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Iframe {
          src: String::new(),
          srcdoc: Some(html.to_string()),
          sandbox: crate::tree::box_tree::IframeSandboxAttribute::None,
          referrer_policy: None,
          frame_token: None,
        },
      },
      vec![],
      Arc::new(ComputedStyle::default()),
    );
    let tree = FragmentTree::new(fragment);
    let pixmap = painter.paint(&tree).expect("paint");

    let center = pixmap.pixel(5, 5).unwrap();
    assert!(
      center.red() > 200 && center.green() < 50 && center.blue() < 50,
      "iframe srcdoc should paint red"
    );
  }

  #[test]
  fn iframe_invalid_src_falls_back_to_placeholder() {
    let mut painter = Painter::new(20, 20, Rgba::WHITE).expect("painter");
    painter.fill_background();
    painter.paint_replaced(
      &ReplacedType::Iframe {
        src: "   ".to_string(),
        srcdoc: None,
        sandbox: crate::tree::box_tree::IframeSandboxAttribute::None,
        referrer_policy: None,
        frame_token: None,
      },
      None,
      None,
      0.0,
      0.0,
      10.0,
      10.0,
    );
    let pixmap = painter.pixmap;
    assert_eq!(
      color_at(&pixmap, 5, 5),
      (200, 200, 200, 255),
      "invalid iframe src should paint UA placeholder fill"
    );
  }

  #[test]
  fn paint_image_from_src_ignores_whitespace_url() {
    let mut painter = Painter::new(10, 10, Rgba::WHITE).expect("painter");
    painter.fill_background();
    let src = crate::tree::box_tree::SelectedImageSource {
      url: "   ",
      descriptor: None,
      density: None,
      from_picture: false,
    };
    assert!(
      !painter.paint_image_from_src(
        &src,
        CrossOriginAttribute::None,
        None,
        None,
        0.0,
        0.0,
        10.0,
        10.0,
        None,
      ),
      "whitespace URL must not be treated as a paintable image source"
    );
    assert_eq!(color_at(&painter.pixmap, 5, 5), (255, 255, 255, 255));
  }

  fn fill_solid_rgba(pixmap: &mut Pixmap, rgba: (u8, u8, u8, u8)) {
    for px in pixmap.data_mut().chunks_exact_mut(4) {
      px[0] = rgba.0;
      px[1] = rgba.1;
      px[2] = rgba.2;
      px[3] = rgba.3;
    }
  }

  fn apply_clip_mask_rect_reference(pixmap: &mut Pixmap, rect: Rect, radii: BorderRadii) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return;
    }
    let width = pixmap.width();
    let height = pixmap.height();
    if width == 0 || height == 0 {
      return;
    }

    let mut mask_pixmap = match new_pixmap(width, height) {
      Some(p) => p,
      None => return,
    };
    let clamped = radii.clamped(rect.width(), rect.height());
    let _ = fill_rounded_rect(
      &mut mask_pixmap,
      rect.x(),
      rect.y(),
      rect.width(),
      rect.height(),
      &clamped,
      Rgba::new(255, 255, 255, 1.0),
    );

    let mask = Mask::from_pixmap(mask_pixmap.as_ref(), MaskType::Alpha);
    pixmap.apply_mask(&mask);

    // Hard-clip pixels outside the rectangle to avoid filter bleed.
    let x0 = rect.x().floor().max(0.0) as u32;
    let y0 = rect.y().floor().max(0.0) as u32;
    let x1 = (rect.x() + rect.width()).ceil().min(width as f32) as u32;
    let y1 = (rect.y() + rect.height()).ceil().min(height as f32) as u32;
    let data = pixmap.data_mut();
    let stride = width as usize * 4;
    for y in 0..height {
      for x in 0..width {
        if x < x0 || x >= x1 || y < y0 || y >= y1 {
          let idx = y as usize * stride + x as usize * 4;
          data[idx] = 0;
          data[idx + 1] = 0;
          data[idx + 2] = 0;
          data[idx + 3] = 0;
        }
      }
    }
  }

  fn build_rounded_rect_mask_reference(
    rect: Rect,
    radii: BorderRadii,
    canvas_w: u32,
    canvas_h: u32,
  ) -> Option<Mask> {
    if canvas_w == 0 || canvas_h == 0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
      return None;
    }
    let mut mask_pixmap = new_pixmap(canvas_w, canvas_h)?;
    let _ = fill_rounded_rect(
      &mut mask_pixmap,
      rect.x(),
      rect.y(),
      rect.width(),
      rect.height(),
      &radii,
      Rgba::new(255, 255, 255, 1.0),
    );
    Some(Mask::from_pixmap(mask_pixmap.as_ref(), MaskType::Alpha))
  }

  fn make_alpha_gradient_mask_layer(
    clip: MaskClip,
    composite: MaskComposite,
    alpha_start: f32,
    alpha_end: f32,
  ) -> MaskLayer {
    let mut layer = MaskLayer::default();
    layer.clip = clip;
    layer.composite = composite;
    layer.repeat = BackgroundRepeat::no_repeat();
    layer.mode = MaskMode::Alpha;
    layer.origin = MaskOrigin::BorderBox;
    layer.image = Some(BackgroundImage::LinearGradient {
      angle: 0.0,
      stops: vec![
        ColorStop {
          color: Color::Rgba(Rgba::new(0, 0, 0, alpha_start)),
          position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
        },
        ColorStop {
          color: Color::Rgba(Rgba::new(0, 0, 0, alpha_end)),
          position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
        },
      ],
    });
    layer
  }

  fn render_mask_reference(
    painter: &mut Painter,
    style: &ComputedStyle,
    css_bounds: Rect,
    layer_bounds: Rect,
    device_size: (u32, u32),
  ) -> Option<Mask> {
    let viewport = (painter.css_width, painter.css_height);
    let rects = background_rects(
      css_bounds.x(),
      css_bounds.y(),
      css_bounds.width(),
      css_bounds.height(),
      style,
      Some(viewport),
    );
    let mut combined: Option<Mask> = None;
    let canvas_clip = layer_bounds;

    for layer in style.mask_layers.iter().rev() {
      let Some(image) = &layer.image else { continue };

      let origin_rect_css = match layer.origin {
        MaskOrigin::BorderBox => rects.border,
        MaskOrigin::PaddingBox => rects.padding,
        MaskOrigin::ContentBox | MaskOrigin::Text => rects.content,
      };
      let clip_rect_css = match layer.clip {
        MaskClip::BorderBox => rects.border,
        MaskClip::PaddingBox => rects.padding,
        MaskClip::ContentBox | MaskClip::Text => rects.content,
        MaskClip::NoClip => canvas_clip,
      };
      if origin_rect_css.width() <= 0.0
        || origin_rect_css.height() <= 0.0
        || clip_rect_css.width() <= 0.0
        || clip_rect_css.height() <= 0.0
      {
        continue;
      }

      let mut dummy = BackgroundLayer::default();
      dummy.size = layer.size;
      let (mut tile_w, mut tile_h) = compute_background_size(
        &dummy,
        style.font_size,
        style.root_font_size,
        viewport,
        origin_rect_css.width(),
        origin_rect_css.height(),
        0.0,
        0.0,
        None,
      );
      if tile_w <= 0.0 || tile_h <= 0.0 {
        continue;
      }

      let mut rounded_x = false;
      let mut rounded_y = false;
      if layer.repeat.x == BackgroundRepeatKeyword::Round {
        tile_w = round_tile_length(origin_rect_css.width(), tile_w);
        rounded_x = true;
      }
      if layer.repeat.y == BackgroundRepeatKeyword::Round {
        tile_h = round_tile_length(origin_rect_css.height(), tile_h);
        rounded_y = true;
      }
      if rounded_x ^ rounded_y
        && matches!(
          layer.size,
          BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto)
        )
      {
        let aspect = 1.0;
        if rounded_x {
          tile_h = tile_w / aspect;
        } else {
          tile_w = tile_h * aspect;
        }
      }

      let (offset_x, offset_y) = resolve_background_offset(
        layer.position,
        origin_rect_css.width(),
        origin_rect_css.height(),
        tile_w,
        tile_h,
        style.font_size,
        style.root_font_size,
        viewport,
      );

      let positions_x = tile_positions(
        layer.repeat.x,
        origin_rect_css.x(),
        origin_rect_css.width(),
        tile_w,
        offset_x,
        clip_rect_css.min_x(),
        clip_rect_css.max_x(),
      );
      let positions_y = tile_positions(
        layer.repeat.y,
        origin_rect_css.y(),
        origin_rect_css.height(),
        tile_h,
        offset_y,
        clip_rect_css.min_y(),
        clip_rect_css.max_y(),
      );

      let pixmap_w = tile_w.ceil().max(1.0) as u32;
      let pixmap_h = tile_h.ceil().max(1.0) as u32;
      let tile = match image {
        BackgroundImage::LinearGradient { .. }
        | BackgroundImage::RepeatingLinearGradient { .. }
        | BackgroundImage::RadialGradient { .. }
        | BackgroundImage::RepeatingRadialGradient { .. }
        | BackgroundImage::ConicGradient { .. }
        | BackgroundImage::RepeatingConicGradient { .. } => {
          painter.render_generated_image(image, style, pixmap_w, pixmap_h)
        }
        BackgroundImage::Url(_) | BackgroundImage::None => None,
      };
      let Some(tile) = tile else { continue };
      let Some(mask_tile) = mask_tile_from_image(&tile, layer.mode).expect("mask_tile_from_image")
      else {
        continue;
      };

      let mut mask_pixmap = new_pixmap(device_size.0, device_size.1)?;
      for ty in positions_y.iter().copied() {
        for tx in positions_x.iter().copied() {
          paint_mask_tile(
            &mut mask_pixmap,
            &mask_tile,
            tx,
            ty,
            tile_w,
            tile_h,
            clip_rect_css,
            painter.origin_offset_css,
            painter.scale,
          );
        }
      }

      let layer_mask = Mask::from_pixmap(mask_pixmap.as_ref(), MaskType::Alpha);
      if let Some(dest) = combined.as_mut() {
        apply_mask_composite(dest, &layer_mask, layer.composite);
      } else {
        combined = Some(layer_mask);
      }
    }

    combined
  }

  #[test]
  fn rounded_rect_mask_matches_reference() {
    let rect = Rect::from_xywh(1.0, 1.0, 8.0, 8.0);
    let radii = BorderRadii::uniform(3.0);

    let optimized = build_rounded_rect_mask(rect, radii, 10, 10).expect("mask");
    let reference = build_rounded_rect_mask_reference(rect, radii, 10, 10).expect("mask");

    assert_eq!(optimized.data(), reference.data());
  }

  #[test]
  fn rounded_rect_mask_avoids_new_pixmap_allocations() {
    let rect = Rect::from_xywh(1.0, 1.0, 8.0, 8.0);
    let radii = BorderRadii::uniform(2.0);
    let recorder = NewPixmapAllocRecorder::start();

    let _mask = build_rounded_rect_mask(rect, radii, 10, 10).expect("mask");

    let allocs = recorder.take();
    assert!(
      allocs.is_empty(),
      "expected build_rounded_rect_mask to avoid new_pixmap allocations, got {allocs:?}"
    );
  }

  #[test]
  fn background_clip_mask_guard_reuses_mask_between_scopes() {
    let rect = Rect::from_xywh(1.0, 1.0, 8.0, 8.0);
    let radii = BorderRadii::uniform(2.0);

    let ptr_first = {
      let mut guard = BackgroundClipMaskGuard::take();
      let mask = guard.mask(rect, radii, 10, 10).expect("mask");
      mask.data().as_ptr()
    };

    let ptr_second = {
      let mut guard = BackgroundClipMaskGuard::take();
      let mask = guard.mask(rect, radii, 10, 10).expect("mask");
      mask.data().as_ptr()
    };

    assert_eq!(
      ptr_first, ptr_second,
      "expected BackgroundClipMaskGuard to reuse the thread-local mask allocation"
    );
  }

  #[test]
  fn background_clip_mask_guard_skips_rebuild_for_same_clip() {
    let rect = Rect::from_xywh(1.0, 1.0, 8.0, 8.0);
    let radii = BorderRadii::uniform(2.0);

    let mut guard = BackgroundClipMaskGuard::take();
    guard.mask(rect, radii, 10, 10).expect("mask");

    let idx = 5 * 10 + 5;
    guard.scratch.mask.as_mut().expect("mask").data_mut()[idx] = 123;

    guard.mask(rect, radii, 10, 10).expect("mask");
    assert_eq!(
      guard.scratch.mask.as_ref().expect("mask").data()[idx],
      123,
      "expected BackgroundClipMaskGuard to reuse the cached mask when clip parameters are unchanged"
    );
  }

  #[test]
  fn render_mask_matches_reference_for_multiple_layers() {
    let mut style = ComputedStyle::default();
    style.padding_top = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.padding_bottom = Length::px(4.0);
    style.padding_left = Length::px(4.0);
    style.mask_layers = smallvec::smallvec![
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 0.0, 1.0),
      make_alpha_gradient_mask_layer(MaskClip::ContentBox, MaskComposite::Add, 1.0, 0.0),
    ];

    let css_bounds = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
    let layer_bounds = css_bounds;
    let device_size = (64, 64);

    let mut optimized_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let optimized = optimized_painter
      .render_mask(&style, css_bounds, layer_bounds, device_size, None)
      .expect("render_mask")
      .expect("mask");

    let mut reference_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let reference = render_mask_reference(
      &mut reference_painter,
      &style,
      css_bounds,
      layer_bounds,
      device_size,
    )
    .expect("mask");

    assert_eq!(optimized.mask().data(), reference.data());
  }

  #[test]
  fn render_mask_matches_reference_for_intersect_composite() {
    let mut style = ComputedStyle::default();
    style.padding_top = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.padding_bottom = Length::px(4.0);
    style.padding_left = Length::px(4.0);
    style.mask_layers = smallvec::smallvec![
      make_alpha_gradient_mask_layer(MaskClip::ContentBox, MaskComposite::Intersect, 1.0, 1.0),
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 0.0, 1.0),
    ];

    let css_bounds = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
    let layer_bounds = css_bounds;
    let device_size = (64, 64);

    let mut optimized_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let optimized = optimized_painter
      .render_mask(&style, css_bounds, layer_bounds, device_size, None)
      .expect("render_mask")
      .expect("mask");

    let mut reference_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let reference = render_mask_reference(
      &mut reference_painter,
      &style,
      css_bounds,
      layer_bounds,
      device_size,
    )
    .expect("mask");

    assert_eq!(optimized.mask().data(), reference.data());

    let viewport = (optimized_painter.css_width, optimized_painter.css_height);
    let rects = background_rects(
      css_bounds.x(),
      css_bounds.y(),
      css_bounds.width(),
      css_bounds.height(),
      &style,
      Some(viewport),
    );
    let expected_dirty = clip_mask_dirty_bounds(
      optimized_painter.device_rect(rects.content),
      device_size.0,
      device_size.1,
    );
    assert_eq!(optimized.dirty, expected_dirty);
  }

  #[test]
  fn render_mask_matches_reference_for_subtract_composite() {
    let mut style = ComputedStyle::default();
    style.padding_top = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.padding_bottom = Length::px(4.0);
    style.padding_left = Length::px(4.0);
    style.mask_layers = smallvec::smallvec![
      make_alpha_gradient_mask_layer(MaskClip::ContentBox, MaskComposite::Subtract, 1.0, 1.0),
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 0.0, 1.0),
    ];

    let css_bounds = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
    let layer_bounds = css_bounds;
    let device_size = (64, 64);

    let mut optimized_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let optimized = optimized_painter
      .render_mask(&style, css_bounds, layer_bounds, device_size, None)
      .expect("render_mask")
      .expect("mask");

    let mut reference_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let reference = render_mask_reference(
      &mut reference_painter,
      &style,
      css_bounds,
      layer_bounds,
      device_size,
    )
    .expect("mask");

    assert_eq!(optimized.mask().data(), reference.data());

    let viewport = (optimized_painter.css_width, optimized_painter.css_height);
    let rects = background_rects(
      css_bounds.x(),
      css_bounds.y(),
      css_bounds.width(),
      css_bounds.height(),
      &style,
      Some(viewport),
    );
    let expected_dirty = clip_mask_dirty_bounds(
      optimized_painter.device_rect(rects.content),
      device_size.0,
      device_size.1,
    );
    assert_eq!(optimized.dirty, expected_dirty);
  }

  #[test]
  fn render_mask_reuses_combined_mask_scratch_between_scopes() {
    CSS_MASK_SCRATCH.with(|cell| {
      *cell.borrow_mut() = CssMaskScratch::default();
    });

    let mut style = ComputedStyle::default();
    style.mask_layers = smallvec::smallvec![make_alpha_gradient_mask_layer(
      MaskClip::BorderBox,
      MaskComposite::Add,
      0.0,
      1.0,
    )];

    let css_bounds = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
    let layer_bounds = css_bounds;
    let device_size = (64, 64);

    let ptr_first = {
      let mut painter = Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
      let rendered = painter
        .render_mask(&style, css_bounds, layer_bounds, device_size, None)
        .expect("render_mask")
        .expect("mask");
      rendered.mask().data().as_ptr()
    };

    let ptr_second = {
      let mut painter = Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
      let rendered = painter
        .render_mask(&style, css_bounds, layer_bounds, device_size, None)
        .expect("render_mask")
        .expect("mask");
      rendered.mask().data().as_ptr()
    };

    assert_eq!(
      ptr_first, ptr_second,
      "expected render_mask to reuse the combined mask allocation between scopes"
    );
  }

  #[test]
  fn render_mask_clears_previous_combined_mask_scratch_between_calls() {
    CSS_MASK_SCRATCH.with(|cell| {
      *cell.borrow_mut() = CssMaskScratch::default();
    });

    let css_bounds = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
    let layer_bounds = css_bounds;
    let device_size = (64, 64);

    let mut border_style = ComputedStyle::default();
    border_style.mask_layers = smallvec::smallvec![make_alpha_gradient_mask_layer(
      MaskClip::BorderBox,
      MaskComposite::Add,
      0.0,
      1.0,
    )];

    let mut content_style = ComputedStyle::default();
    content_style.padding_top = Length::px(4.0);
    content_style.padding_right = Length::px(4.0);
    content_style.padding_bottom = Length::px(4.0);
    content_style.padding_left = Length::px(4.0);
    content_style.mask_layers = smallvec::smallvec![make_alpha_gradient_mask_layer(
      MaskClip::ContentBox,
      MaskComposite::Add,
      1.0,
      0.0,
    )];

    {
      let mut painter = Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
      let _mask = painter
        .render_mask(&border_style, css_bounds, layer_bounds, device_size, None)
        .expect("render_mask")
        .expect("mask");
    }

    let mut optimized_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let optimized = optimized_painter
      .render_mask(&content_style, css_bounds, layer_bounds, device_size, None)
      .expect("render_mask")
      .expect("mask");

    let mut reference_painter =
      Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let reference = render_mask_reference(
      &mut reference_painter,
      &content_style,
      css_bounds,
      layer_bounds,
      device_size,
    )
    .expect("mask");

    assert_eq!(optimized.mask().data(), reference.data());
  }

  #[test]
  fn render_mask_reuses_layer_pixmap_scratch_for_multiple_layers() {
    MASK_LAYER_PIXMAP_SCRATCH.with(|cell| {
      *cell.borrow_mut() = MaskLayerPixmapScratch::default();
    });

    let mut style = ComputedStyle::default();
    style.mask_layers = smallvec::smallvec![
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 0.0, 1.0),
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 1.0, 0.0),
    ];

    let css_bounds = Rect::from_xywh(0.0, 0.0, 32.0, 32.0);
    let layer_bounds = css_bounds;
    let device_size = (64, 64);

    let mut painter = Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let recorder = NewPixmapAllocRecorder::start();
    let _mask = painter
      .render_mask(&style, css_bounds, layer_bounds, device_size, None)
      .expect("render_mask")
      .expect("mask");

    let allocs = recorder.take();
    let scratch_allocs = allocs
      .iter()
      .filter(|record| record.width == device_size.0 && record.height == device_size.1)
      .count();
    assert_eq!(
      scratch_allocs, 1,
      "expected render_mask to allocate its layer scratch pixmap once, got {scratch_allocs} allocations: {allocs:?}"
    );
  }

  #[test]
  fn legacy_render_mask_composite_times_out_via_cancel_callback() {
    CSS_MASK_SCRATCH.with(|cell| {
      *cell.borrow_mut() = CssMaskScratch::default();
    });
    MASK_LAYER_PIXMAP_SCRATCH.with(|cell| {
      *cell.borrow_mut() = MaskLayerPixmapScratch::default();
    });

    let mut style = ComputedStyle::default();
    style.mask_layers = smallvec::smallvec![
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 0.0, 1.0),
      make_alpha_gradient_mask_layer(MaskClip::BorderBox, MaskComposite::Add, 1.0, 0.0),
    ];

    let device_size = (64, 64);
    let css_bounds = Rect::from_xywh(0.0, 0.0, device_size.0 as f32, device_size.1 as f32);
    let layer_bounds = css_bounds;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    // Allow enough deadline checks to reach the second layer's composite step, then cancel.
    let cancel_after = 110usize;
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= cancel_after);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let mut painter = Painter::new(device_size.0, device_size.1, Rgba::WHITE).expect("painter");
    let result = with_deadline(Some(&deadline), || {
      painter.render_mask(&style, css_bounds, layer_bounds, device_size, None)
    });

    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout, got {result:?}"
    );
    assert!(
      calls.load(Ordering::SeqCst) > cancel_after,
      "expected cancel callback to be polled more than {cancel_after} times, got {}",
      calls.load(Ordering::SeqCst)
    );
  }

  #[test]
  fn backdrop_filters_reuse_region_pixmap_scratch() {
    BACKDROP_FILTER_SCRATCH.with(|cell| {
      *cell.borrow_mut() = BackdropFilterScratch::default();
    });

    let mut pixmap = new_pixmap(8, 8).expect("pixmap");
    pixmap.data_mut().fill(200);
    let bounds = Rect::from_xywh(1.0, 1.0, 6.0, 6.0);
    let filters = vec![ResolvedFilter::Brightness(1.25)];

    apply_backdrop_filters(
      &mut pixmap,
      &bounds,
      &filters,
      BorderRadii::uniform(2.0),
      1.0,
      bounds,
    )
    .expect("backdrop filter warm-up");

    let recorder = NewPixmapAllocRecorder::start();
    apply_backdrop_filters(
      &mut pixmap,
      &bounds,
      &filters,
      BorderRadii::uniform(2.0),
      1.0,
      bounds,
    )
    .expect("backdrop filter reuse");
    let allocations = recorder.take();

    assert!(
      allocations.is_empty(),
      "expected apply_backdrop_filters to reuse scratch pixmaps, got {allocations:?}"
    );
  }

  #[test]
  fn legacy_backdrop_filters_times_out_via_cancel_callback() {
    BACKDROP_FILTER_SCRATCH.with(|cell| {
      *cell.borrow_mut() = BackdropFilterScratch::default();
    });

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    // Let the first deadline check through, then trigger cancellation on the next poll.
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 1);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let mut pixmap = new_pixmap(128, 128).expect("pixmap");
    pixmap.data_mut().fill(200);
    let bounds = Rect::from_xywh(0.0, 0.0, 128.0, 128.0);
    // Blur with sigma=0 is a no-op and won't call into the blur implementation, so this test
    // asserts that apply_backdrop_filters itself periodically checks the active deadline.
    let filters = vec![ResolvedFilter::Blur(0.0)];

    let result = with_deadline(Some(&deadline), || {
      apply_backdrop_filters(
        &mut pixmap,
        &bounds,
        &filters,
        BorderRadii::ZERO,
        1.0,
        bounds,
      )
    });

    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout, got {result:?}"
    );
    assert!(
      calls.load(Ordering::SeqCst) >= 2,
      "expected cancel callback to be polled more than once, got {}",
      calls.load(Ordering::SeqCst)
    );
  }

  #[test]
  fn mask_apply_with_dirty_bounds_matches_tiny_skia() {
    let mut base = new_pixmap(8, 8).expect("pixmap");
    for (idx, chunk) in base.data_mut().chunks_exact_mut(4).enumerate() {
      let v = (idx as u8).wrapping_mul(37).wrapping_add(11);
      chunk.copy_from_slice(&[v, v.rotate_left(1), v.rotate_left(2), v.rotate_left(3)]);
    }

    let mut expected = new_pixmap(8, 8).expect("pixmap");
    expected.data_mut().copy_from_slice(base.data());
    let mut actual = new_pixmap(8, 8).expect("pixmap");
    actual.data_mut().copy_from_slice(base.data());

    let mut mask = Mask::new(8, 8).expect("mask");
    mask.data_mut().fill(0);
    for y in 2..6 {
      for x in 1..7 {
        mask.data_mut()[(y * 8 + x) as usize] = (x * 17 + y * 13) as u8;
      }
    }

    expected.apply_mask(&mask);
    apply_mask_with_dirty_bounds_rgba(
      &mut actual,
      &mask,
      Some(ClipMaskDirtyRect {
        x0: 1,
        y0: 2,
        x1: 7,
        y1: 6,
      }),
    )
    .expect("mask apply");

    assert_eq!(actual.data(), expected.data());
  }

  #[test]
  fn clip_mask_rounded_rect_matches_reference() {
    let mut optimized = new_pixmap(10, 10).expect("pixmap");
    let mut reference = new_pixmap(10, 10).expect("pixmap");
    fill_solid_rgba(&mut optimized, (255, 0, 0, 255));
    fill_solid_rgba(&mut reference, (255, 0, 0, 255));

    let rect = Rect::from_xywh(1.0, 1.0, 8.0, 8.0);
    let radii = BorderRadii::uniform(3.0);

    apply_clip_mask_rect(&mut optimized, rect, radii).expect("clip mask");
    apply_clip_mask_rect_reference(&mut reference, rect, radii);

    assert_eq!(optimized.data(), reference.data());
  }

  #[test]
  fn clip_mask_rounded_rect_matches_reference_with_varied_pixels() {
    let mut optimized = new_pixmap(16, 16).expect("pixmap");
    let mut reference = new_pixmap(16, 16).expect("pixmap");

    for (idx, (opt, rf)) in optimized
      .data_mut()
      .chunks_exact_mut(4)
      .zip(reference.data_mut().chunks_exact_mut(4))
      .enumerate()
    {
      // Deterministic but non-trivial RGBA values so we exercise mask multiplication rounding.
      let v = (idx as u32 * 37 + 11) as u8;
      let r = v;
      let g = v.rotate_left(3);
      let b = v.wrapping_mul(5);
      let a = v.wrapping_add(17);
      opt.copy_from_slice(&[r, g, b, a]);
      rf.copy_from_slice(&[r, g, b, a]);
    }

    // Use fractional bounds to ensure anti-aliased edge pixels.
    let rect = Rect::from_xywh(1.3, 0.7, 13.1, 12.4);
    let radii = BorderRadii::uniform(4.0);

    apply_clip_mask_rect(&mut optimized, rect, radii).expect("clip mask");
    apply_clip_mask_rect_reference(&mut reference, rect, radii);

    assert_eq!(optimized.data(), reference.data());
  }

  #[test]
  fn clip_mask_apply_mask_math_matches_tiny_skia() {
    let mut pixmap_expected = new_pixmap(4, 1).expect("pixmap");
    let mut pixmap_actual = new_pixmap(4, 1).expect("pixmap");
    let px = pixmap_expected.data_mut();
    px.copy_from_slice(&[
      1, 2, 3, 4, // exercises rounding for non-255 values
      5, 6, 7, 8, //
      9, 10, 11, 12, //
      13, 14, 15, 16, //
    ]);
    pixmap_actual.data_mut().copy_from_slice(px);

    let mut mask = Mask::new(4, 1).expect("mask");
    mask.data_mut().copy_from_slice(&[0, 64, 128, 255]);

    pixmap_expected.apply_mask(&mask);
    apply_mask_rect_rgba(
      &mut pixmap_actual,
      &mask,
      ClipMaskDirtyRect {
        x0: 0,
        y0: 0,
        x1: 4,
        y1: 1,
      },
    )
    .expect("mask apply");

    assert_eq!(pixmap_actual.data(), pixmap_expected.data());
  }

  #[test]
  fn clip_mask_integer_aligned_rect_without_radii_matches_reference() {
    let mut optimized = new_pixmap(10, 10).expect("pixmap");
    let mut reference = new_pixmap(10, 10).expect("pixmap");
    fill_solid_rgba(&mut optimized, (0, 255, 0, 255));
    fill_solid_rgba(&mut reference, (0, 255, 0, 255));

    let rect = Rect::from_xywh(2.0, 1.0, 6.0, 7.0);

    apply_clip_mask_rect(&mut optimized, rect, BorderRadii::ZERO).expect("clip mask");
    apply_clip_mask_rect_reference(&mut reference, rect, BorderRadii::ZERO);

    assert_eq!(optimized.data(), reference.data());
  }

  #[test]
  fn clip_mask_hard_clips_using_integer_bounds() {
    let mut pixmap = new_pixmap(4, 4).expect("pixmap");
    fill_solid_rgba(&mut pixmap, (10, 20, 30, 255));
    let rect = Rect::from_xywh(1.2, 1.2, 1.0, 1.0);

    apply_clip_mask_rect(&mut pixmap, rect, BorderRadii::ZERO).expect("clip mask");

    // Hard clip uses floor/ceil bounds, so only pixels in [1..3)×[1..3) remain potentially
    // non-zero.
    for y in 0..4 {
      for x in 0..4 {
        let px = pixmap.pixel(x, y).expect("pixel in bounds");
        let inside = (1..3).contains(&x) && (1..3).contains(&y);
        if inside {
          assert!(
            px.alpha() > 0,
            "expected ({x},{y}) to remain inside the clip"
          );
        } else {
          assert_eq!((px.red(), px.green(), px.blue(), px.alpha()), (0, 0, 0, 0));
        }
      }
    }
  }

  #[test]
  fn clip_mask_noop_fast_path_keeps_pixmap_unchanged() {
    let mut pixmap = new_pixmap(3, 3).expect("pixmap");
    for (idx, px) in pixmap.data_mut().chunks_exact_mut(4).enumerate() {
      let v = (idx * 17) as u8;
      px.copy_from_slice(&[v, v.wrapping_add(1), v.wrapping_add(2), 255]);
    }
    let before = pixmap.data().to_vec();

    // Covers the full pixmap within the 0.5px tolerance.
    let rect = Rect::from_xywh(-0.25, -0.25, 3.5, 3.5);
    apply_clip_mask_rect(&mut pixmap, rect, BorderRadii::ZERO).expect("clip mask");

    assert_eq!(pixmap.data(), before.as_slice());
  }

  #[test]
  fn clip_mask_integer_aligned_rect_without_radii_avoids_scratch_allocation() {
    let mut pixmap = new_pixmap(8, 8).expect("pixmap");
    fill_solid_rgba(&mut pixmap, (255, 0, 0, 255));
    let rect = Rect::from_xywh(1.0, 1.0, 6.0, 6.0);

    let recorder = NewPixmapAllocRecorder::start();
    apply_clip_mask_rect(&mut pixmap, rect, BorderRadii::ZERO).expect("clip mask");
    let allocs = recorder.take();

    assert!(
      allocs.is_empty(),
      "expected no scratch new_pixmap allocations for integer-aligned rects without radii, got {allocs:?}"
    );
  }

  #[test]
  fn clip_mask_reuses_scratch_pixmap_on_second_call() {
    let mut pixmap = new_pixmap(8, 8).expect("pixmap");
    fill_solid_rgba(&mut pixmap, (255, 0, 0, 255));
    let rect = Rect::from_xywh(1.0, 1.0, 6.0, 6.0);
    let radii = BorderRadii::uniform(2.0);

    let recorder = NewPixmapAllocRecorder::start();

    apply_clip_mask_rect(&mut pixmap, rect, radii).expect("clip mask");
    let _ = recorder.take();

    apply_clip_mask_rect(&mut pixmap, rect, radii).expect("clip mask");
    let second = recorder.take();
    assert!(
      second.is_empty(),
      "expected no new_pixmap allocations on second call, got {second:?}"
    );
  }

  mod svg_mask_image_reference {
    use super::super::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
    use crate::api::{DiagnosticsLevel, RenderOptions};
    use crate::image_loader::ImageCache;
    use crate::paint::display_list::DisplayList;
    use crate::paint::display_list_builder::DisplayListBuilder;
    use crate::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
    use crate::scroll::ScrollState;
    use crate::text::font_loader::FontContext;
    use crate::tree::fragment_tree::FragmentNode;
    use crate::{FastRender, FontConfig, Point, Rgba};

    fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
      let p = pixmap.pixel(x, y).expect("pixel in bounds");
      (p.red(), p.green(), p.blue(), p.alpha())
    }

    fn build_display_list(html: &str, width: u32, height: u32) -> (DisplayList, FontContext) {
      let mut renderer = FastRender::builder()
        .font_sources(FontConfig::bundled_only())
        .build()
        .expect("renderer");

      let dom = renderer.parse_html(html).expect("parsed");
      let tree = renderer
        .layout_document(&dom, width, height)
        .expect("laid out");
      let font_ctx = renderer.font_context().clone();
      let image_cache = ImageCache::new();
      let viewport = tree.viewport_size();

      let build_for_root = |root: &FragmentNode| -> DisplayList {
        DisplayListBuilder::with_image_cache(image_cache.clone())
          .with_font_context(font_ctx.clone())
          .with_svg_filter_defs(tree.svg_filter_defs.clone())
          .with_svg_id_defs(tree.svg_id_defs.clone())
          .with_scroll_state(ScrollState::default())
          .with_device_pixel_ratio(1.0)
          // Keep display-list building deterministic; these tests focus on renderer effects.
          .with_parallelism(&PaintParallelism::disabled())
          .with_viewport_size(viewport.width, viewport.height)
          .build_with_stacking_tree_offset_checked(root, Point::ZERO)
          .expect("display list")
      };

      let mut list = build_for_root(&tree.root);
      for extra in &tree.additional_fragments {
        list.append(build_for_root(extra));
      }
      (list, font_ctx)
    }

    #[test]
    fn svg_mask_image_reference_resolves_use_dependencies() {
      // Many real-world pages define masks inside hidden SVG <defs> blocks and then reference them
      // from CSS via `mask-image: url(#id)`. Those masks frequently use `<use xlink:href="#...">`
      // to reference other defs.
      let html = r##"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 100px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-mode: alpha;
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
    
        <svg style="display: none" xmlns="http://www.w3.org/2000/svg"
             xmlns:xlink="http://www.w3.org/1999/xlink">
          <defs>
            <rect id="shape" x="0" y="0" width="50" height="100" fill="white"/>
            <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse"
                  x="0" y="0" width="100" height="100">
              <use xlink:href="#shape"/>
            </mask>
          </defs>
        </svg>
    
        <div id="box"></div>
      "##;

      let mut renderer = FastRender::new().expect("renderer");
      let dom = renderer.parse_html(html).expect("parse html");
      let fragments = renderer
        .layout_document(&dom, 100, 100)
        .expect("layout document");

      assert!(
        fragments
          .svg_id_defs
          .as_ref()
          .is_some_and(|defs| { defs.contains_key("m") && defs.contains_key("shape") }),
        "layout should retain defs required by url(#m) mask-image"
      );

      let pixmap = paint_tree_with_resources_scaled_offset_backend(
        &fragments,
        100,
        100,
        Rgba::WHITE,
        renderer.font_context().clone(),
        ImageCache::new(),
        1.0,
        Point::ZERO,
        PaintParallelism::disabled(),
        &ScrollState::default(),
        PaintBackend::DisplayList,
      )
      .expect("paint");

      // Left half is visible (mask contains a 50px-wide white rect via <use>).
      assert_eq!(pixel(&pixmap, 10, 50), (255, 0, 0, 255));
      // Right half is masked out and shows the canvas background.
      assert_eq!(pixel(&pixmap, 90, 50), (255, 255, 255, 255));
    }

    #[test]
    fn svg_mask_image_fragment_reference_masks_element() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
        <svg width="0" height="0" style="position:absolute">
          <mask id="m">
            <rect x="0" y="0" width="100" height="20" fill="black"/>
            <rect x="50" y="0" width="50" height="20" fill="white"/>
          </mask>
        </svg>
        <div id="box"></div>
      "#;

      let (list, font_ctx) = build_display_list(html, 100, 20);
      let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
        .expect("renderer")
        .with_parallelism(PaintParallelism::disabled())
        .render(&list)
        .expect("render");

      assert_eq!(pixel(&pixmap, 10, 10), (255, 255, 255, 255));
      assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
    }

    #[test]
    fn svg_mask_image_match_source_respects_mask_type_alpha() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
        <svg width="0" height="0" style="position:absolute">
          <mask id="m" mask-type="alpha">
            <rect x="0" y="0" width="50" height="20" fill="black"/>
            <rect x="50" y="0" width="50" height="20" fill="white"/>
          </mask>
        </svg>
        <div id="box"></div>
      "#;

      let (list, font_ctx) = build_display_list(html, 100, 20);
      let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
        .expect("renderer")
        .with_parallelism(PaintParallelism::disabled())
        .render(&list)
        .expect("render");

      // With mask-type="alpha", opaque black and opaque white both yield full opacity.
      assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
      assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
    }

    #[test]
    fn svg_mask_image_match_source_respects_mask_type_alpha_css_property() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
          #m { mask-type: alpha; }
        </style>
        <svg width="0" height="0" style="position:absolute">
          <mask id="m">
            <rect x="0" y="0" width="50" height="20" fill="black"/>
            <rect x="50" y="0" width="50" height="20" fill="white"/>
          </mask>
        </svg>
        <div id="box"></div>
      "#;

      let (list, font_ctx) = build_display_list(html, 100, 20);
      let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
        .expect("renderer")
        .with_parallelism(PaintParallelism::disabled())
        .render(&list)
        .expect("render");

      // With mask-type: alpha, opaque black and opaque white both yield full opacity.
      assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
      assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
    }

    #[test]
    fn svg_mask_image_respects_maskContentUnits_object_bounding_box() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
        <svg width="0" height="0" style="position:absolute">
          <mask id="m" maskContentUnits="objectBoundingBox" maskUnits="objectBoundingBox">
            <rect x="0" y="0" width="0.5" height="1" fill="black"/>
            <rect x="0.5" y="0" width="0.5" height="1" fill="white"/>
          </mask>
        </svg>
        <div id="box"></div>
      "#;

      let (list, font_ctx) = build_display_list(html, 100, 20);
      let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
        .expect("renderer")
        .with_parallelism(PaintParallelism::disabled())
        .render(&list)
        .expect("render");

      assert_eq!(pixel(&pixmap, 10, 10), (255, 255, 255, 255));
      assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
    }

    #[test]
    fn svg_mask_image_respects_maskUnits_user_space_on_use() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
        <svg width="0" height="0" style="position:absolute">
          <mask id="m" maskUnits="userSpaceOnUse" x="0" y="0" width="50" height="20">
            <rect width="50" height="20" fill="white"/>
          </mask>
        </svg>
        <div id="box"></div>
      "#;

      let (list, font_ctx) = build_display_list(html, 100, 20);
      let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
        .expect("renderer")
        .with_parallelism(PaintParallelism::disabled())
        .render(&list)
        .expect("render");

      assert_eq!(pixel(&pixmap, 25, 10), (255, 0, 0, 255));
      assert_eq!(pixel(&pixmap, 75, 10), (255, 255, 255, 255));
    }

    #[test]
    fn svg_mask_image_does_not_trigger_fetch_errors() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 100px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#m);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
        <svg width="0" height="0" style="position:absolute">
          <mask id="m">
            <rect x="0" y="0" width="100" height="20" fill="black"/>
            <rect x="50" y="0" width="50" height="20" fill="white"/>
          </mask>
        </svg>
        <div id="box"></div>
      "#;

      let mut renderer = FastRender::builder()
        .font_sources(FontConfig::bundled_only())
        .build()
        .expect("renderer");

      let options = RenderOptions::new()
        .with_viewport(100, 20)
        .with_diagnostics_level(DiagnosticsLevel::Basic);
      let result = renderer
        .render_html_with_diagnostics(html, options)
        .expect("render");

      assert!(
        result.diagnostics.fetch_errors.is_empty(),
        "expected mask-image:url(#m) to stay local, got fetch errors: {:?}",
        result.diagnostics.fetch_errors
      );
    }

    #[test]
    fn svg_mask_image_missing_id_does_not_trigger_fetch_errors() {
      let html = r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box {
            width: 20px;
            height: 20px;
            background: rgb(255 0 0);
            mask-image: url(#missing);
            mask-repeat: no-repeat;
            mask-size: 100% 100%;
            mask-position: 0 0;
          }
        </style>
        <div id="box"></div>
      "#;

      let mut renderer = FastRender::builder()
        .font_sources(FontConfig::bundled_only())
        .build()
        .expect("renderer");

      let options = RenderOptions::new()
        .with_viewport(20, 20)
        .with_diagnostics_level(DiagnosticsLevel::Basic);
      let result = renderer
        .render_html_with_diagnostics(html, options)
        .expect("render");

      assert!(
        result.diagnostics.fetch_errors.is_empty(),
        "expected mask-image:url(#missing) to stay local, got fetch errors: {:?}",
        result.diagnostics.fetch_errors
      );
      assert_eq!(pixel(&result.pixmap, 10, 10), (255, 0, 0, 255));
    }
  }
}
