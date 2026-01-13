//! Display List Builder - Converts Fragment Tree to Display List
//!
//! This module builds a display list from the fragment tree by traversing
//! fragments and emitting paint commands in correct CSS paint order.
//!
//! # Pipeline
//!
//! ```text
//! Fragment Tree → Display List Builder → Display List → Rasterizer → Pixels
//! ```
//!
//! # Paint Order (CSS 2.1 Appendix E)
//!
//! For each fragment:
//! 1. Background color
//! 2. Background image
//! 3. Border
//! 4. Children (recursively)
//!
//! # Example
//!
//! ```rust,ignore
//! use fastrender::paint::{DisplayListBuilder, DisplayList};
//!
//! let builder = DisplayListBuilder::new();
//! let display_list = builder.build_tree(&fragment_tree);
//! ```

use crate::api::DEFAULT_MAX_IFRAME_DEPTH;
use crate::css::types::ColorStop;
use crate::css::types::RadialGradientShape;
use crate::css::types::RadialGradientSize;
use crate::debug::runtime;
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::image_loader::ImageCache;
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::layout::contexts::inline::line_builder::TextItem as InlineTextItem;
use crate::layout::utils::{
  resolve_font_relative_length, resolve_length_with_percentage_metrics, resolve_scrollbar_width,
};
use crate::math::{layout_mathml, MathFragment};
use crate::paint::clip_path::resolve_clip_path;
use crate::paint::display_list::BlendMode;
use crate::paint::display_list::BlendModeItem;
use crate::paint::display_list::BorderGap;
use crate::paint::display_list::BorderImageItem;
use crate::paint::display_list::BorderImageSourceItem;
use crate::paint::display_list::BorderItem;
use crate::paint::display_list::BorderRadii;
use crate::paint::display_list::BorderSide;
use crate::paint::display_list::BoxShadowItem;
use crate::paint::display_list::ClipItem;
use crate::paint::display_list::ClipShape;
use crate::paint::display_list::ConicGradientItem;
use crate::paint::display_list::ConicGradientPatternItem;
use crate::paint::display_list::DecorationPaint;
use crate::paint::display_list::DecorationStroke;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list::DisplayList;
use crate::paint::display_list::EmphasisMark;
use crate::paint::display_list::EmphasisText;
use crate::paint::display_list::EmphasisTextRun;
use crate::paint::display_list::FillRectItem;
use crate::paint::display_list::FillRoundedRectItem;
use crate::paint::display_list::FontId;
use crate::paint::display_list::FontVariation;
use crate::paint::display_list::GlyphInstance;
use crate::paint::display_list::GradientSpread;
use crate::paint::display_list::GradientStop;
use crate::paint::display_list::ImageData;
use crate::paint::display_list::ImageFilterQuality;
use crate::paint::display_list::ImageItem;
use crate::paint::display_list::ImagePatternItem;
use crate::paint::display_list::ImagePatternRepeat;
use crate::paint::display_list::LinearGradientItem;
use crate::paint::display_list::LinearGradientPatternItem;
use crate::paint::display_list::ListMarkerItem;
use crate::paint::display_list::MaskBorderWidths;
use crate::paint::display_list::MaskReferenceRects;
use crate::paint::display_list::OpacityItem;
use crate::paint::display_list::OutlineItem;
use crate::paint::display_list::RadialGradientItem;
use crate::paint::display_list::RadialGradientPatternItem;
use crate::paint::display_list::ResolvedFilter;
use crate::paint::display_list::ResolvedMask;
use crate::paint::display_list::ResolvedMaskBorder;
use crate::paint::display_list::ResolvedMaskImage;
use crate::paint::display_list::ResolvedMaskLayer;
use crate::paint::display_list::StackingContextItem;
use crate::paint::display_list::StrokeRectItem;
use crate::paint::display_list::StrokeRoundedRectItem;
use crate::paint::display_list::TableCollapsedBordersItem;
use crate::paint::display_list::TextDecorationItem;
use crate::paint::display_list::TextEmphasis;
use crate::paint::display_list::TextItem;
use crate::paint::display_list::TextShadowItem;
use crate::paint::display_list::Transform3D;
use crate::paint::display_list::TransformItem;
use crate::paint::display_list_renderer::{PaintParallelism, PaintParallelismMode};
use crate::paint::filter_outset::filter_halo_outset_with_bounds;
use crate::paint::filter_outset::filter_outset_with_bounds;
use crate::paint::iframe::{render_iframe_src, render_iframe_srcdoc};
use crate::paint::object_fit::compute_object_fit;
use crate::paint::object_fit::default_object_position;
use crate::paint::painter::{
  paint_diagnostics_enabled, paint_diagnostics_session_id, with_paint_diagnostics,
  PaintDiagnosticsThreadGuard,
};
use crate::paint::stacking::ClipChainLink;
use crate::paint::stacking::Layer6Item;
use crate::paint::stacking::StackingContext;
use crate::paint::svg_filter::SvgFilterResolver;
use crate::paint::text_decoration::{resolve_underline_side, UnderlineSide};
use crate::paint::text_shadow::resolve_text_shadows_with_viewport;
use crate::paint::transform_resolver::ResolvedTransforms;
use crate::render_control::{
  active_allocation_budget, active_deadline, active_stage, active_stage_heartbeat, check_active,
  check_active_periodic, with_allocation_budget, with_deadline, StageGuard, StageHeartbeatGuard,
};
use crate::scroll::ScrollState;
use crate::style::block_axis_is_horizontal;
use crate::style::block_axis_positive;
use crate::style::color::Rgba;
use crate::style::inline_axis_is_horizontal;
use crate::style::inline_axis_positive;
use crate::style::position::Position;
use crate::style::types::AccentColor;
use crate::style::types::Appearance;
use crate::style::types::BackfaceVisibility;
use crate::style::types::BackgroundAttachment;
use crate::style::types::BackgroundBox;
use crate::style::types::BackgroundImage;
use crate::style::types::BackgroundLayer;
use crate::style::types::BackgroundPosition;
use crate::style::types::BackgroundRepeatKeyword;
use crate::style::types::BackgroundSize;
use crate::style::types::BackgroundSizeComponent;
use crate::style::types::BackgroundSizeKeyword;
use crate::style::types::BorderImageSource;
use crate::style::types::BoxDecorationBreak;
use crate::style::types::CaretColor;
use crate::style::types::ContentVisibility;
use crate::style::types::ImageOrientation;
use crate::style::types::ImageRendering;
use crate::style::types::Isolation;
use crate::style::types::MaskClip;
use crate::style::types::MaskBorderMode;
use crate::style::types::MaskMode;
use crate::style::types::MixBlendMode;
use crate::style::types::ObjectFit;
use crate::style::types::OrientationTransform;
use crate::style::types::ResolvedTextDecoration;
use crate::style::types::TextDecorationLine;
use crate::style::types::TextDecorationSkipBox;
use crate::style::types::TextDecorationSkipInk;
use crate::style::types::TextDecorationStyle;
use crate::style::types::TextDecorationThickness;
use crate::style::types::TextEmphasisPosition;
use crate::style::types::TextEmphasisStyle;
use crate::style::types::TextUnderlineOffset;
use crate::style::types::TextUnderlinePosition;
use crate::style::types::TransformBox;
use crate::style::types::TransformStyle;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::style::ComputedStyle;
use crate::style::PhysicalSide;
use crate::text::caret::{caret_stops_for_runs, caret_x_for_position, CaretAffinity};
use crate::text::font_db::FontStretch;
use crate::text::font_db::FontStyle;
use crate::text::font_db::ScaledMetrics;
use crate::text::font_loader::FontContext;
use crate::text::pipeline::ShapedRun;
use crate::text::pipeline::ShapingPipeline;
use crate::tree::box_tree::CrossOriginAttribute;
use crate::tree::box_tree::FormControl;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::ImageDecodingAttribute;
use crate::tree::box_tree::ImageLoadingAttribute;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SelectItem;
use crate::tree::box_tree::SvgContent;
use crate::tree::box_tree::TextControlKind;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;
use crate::tree::fragment_tree::TextEmphasisOffset;
use lru::LruCache;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};
use unicode_general_category::{get_general_category, GeneralCategory};

const DEFAULT_DECODED_IMAGE_CACHE_MAX_ENTRIES: usize = 256;
const DEFAULT_DECODED_IMAGE_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const ENV_DECODED_IMAGE_CACHE_ITEMS: &str = "FASTR_DECODED_IMAGE_CACHE_ITEMS";
const ENV_DECODED_IMAGE_CACHE_BYTES: &str = "FASTR_DECODED_IMAGE_CACHE_BYTES";
/// Maximum image size (in destination device pixels) that we attempt to decode synchronously when
/// `decoding="async"` is set.
///
/// Chrome's headless `--screenshot` capture often happens before large `decoding="async"` images
/// finish decoding, leaving them transparent in the baseline. We approximate this by deferring
/// decoding when the rendered image would occupy a large number of device pixels.
// NOTE: Calibrated against fixture baselines: some above-the-fold images with
// `decoding="async"` still reliably appear in Chrome screenshots at moderate sizes.
// Keep this threshold high enough to avoid hiding typical hero images.
const ASYNC_IMAGE_DECODE_MAX_DEST_PIXELS: u64 = 200_000;
const DEADLINE_STRIDE: usize = 256;
/// Conservative recursion-depth cap for stacking-context painting.
///
/// `DisplayListBuilder::build_stacking_context` is still implemented recursively. Deep stacking
/// context chains (e.g. adversarial `opacity < 1` wrappers) can otherwise overflow the call stack
/// and abort the process. Prefer the non-recursive fragment path (`build_checked`) when possible.
//
// This value is tuned to keep recursive `build_stacking_context` traversal from overflowing the
// tiny (256KB) stacks used by paint regression tests. If this needs to be increased for real-world
// pages, consider refactoring stacking-context painting to use an explicit stack instead of
// recursion.
const MAX_STACKING_CONTEXT_DEPTH: usize = 32;

/// Builder that converts a fragment tree to a display list
///
/// Walks the fragment tree depth-first, emitting display items
/// for backgrounds, borders, and content in correct CSS paint order.
pub struct DisplayListBuilder {
  /// The display list being built
  list: DisplayList,
  image_cache: Option<ImageCache>,
  saw_gif_image: Arc<AtomicBool>,
  saw_animation_time_dependent_image: Arc<AtomicBool>,
  /// Optional provider used to supply decoded media frames (e.g. `<video>`) during display-list
  /// construction.
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  decoded_image_cache: Arc<Mutex<DecodedImageCache>>,
  /// Serialized SVG filter definitions collected from the document DOM.
  svg_filter_defs: Option<Arc<HashMap<String, String>>>,
  /// Serialized SVG defs (by id) collected from the document DOM.
  svg_id_defs: Option<Arc<HashMap<String, String>>>,
  /// Raw serialized SVG defs (by id) collected from the document DOM.
  ///
  /// Used to inline same-document fragment references across sibling `<svg>` roots while
  /// preserving `currentColor` semantics.
  svg_id_defs_raw: Option<Arc<HashMap<String, String>>>,
  /// Form-control metadata keyed by box id for `appearance: none` controls rendered as normal boxes.
  ///
  /// Replaced controls embed this metadata in `ReplacedType::FormControl`. When a control is laid
  /// out as a normal element (to allow authored/pseudo content), we still need the metadata during
  /// paint to render caret/selection overlays.
  appearance_none_form_controls: Option<Arc<HashMap<usize, Arc<FormControl>>>>,
  /// Viewport size used for resolving viewport-relative units (e.g. vw/vh) during paint.
  viewport: Option<(f32, f32)>,
  /// Optional viewport size used for visibility/culling decisions while building the display list.
  ///
  /// This is separate from `viewport` because callers may paint into a surface that is larger than
  /// the layout viewport (e.g. paged media stacked into one pixmap, or "fit canvas to content").
  /// In those cases, viewport-relative units must continue to resolve against the layout viewport
  /// while culling should be bounded by the actual paint surface.
  culling_viewport: Option<(f32, f32)>,
  font_ctx: FontContext,
  shaper: ShapingPipeline,
  device_pixel_ratio: f32,
  parallel_enabled: bool,
  parallel_mode: PaintParallelismMode,
  parallel_min: usize,
  parallel_root_min: usize,
  parallel_min_explicit: bool,
  parallel_stats: Option<Arc<ParallelStats>>,
  build_breakdown: Option<Arc<BuildBreakdown>>,
  background_tiles: Option<Arc<AtomicU64>>,
  background_layers: Option<Arc<AtomicU64>>,
  background_pattern_fast_paths: Option<Arc<AtomicU64>>,
  /// When extending an element background to the canvas (HTML canvas background propagation),
  /// suppress painting the background on the original element to avoid double-paint seams.
  canvas_background_suppress_box_id: Option<usize>,
  /// When propagating the root element's border to the canvas (e.g. when viewport scrollbar gutters
  /// are reserved), suppress painting the border on the original element to avoid double-paint.
  canvas_border_suppress_box_id: Option<usize>,
  estimated_fragments: Option<usize>,
  scroll_state: ScrollState,
  max_iframe_depth: usize,
  skip_stacking_context_children: bool,
  line_decoration_ctx: Option<LineDecorationContext>,
  error: Option<RenderError>,
}

#[derive(Clone, Copy, Debug)]
struct LineDecorationContext {
  inline_vertical: bool,
  block_baseline: f32,
  line_inline_start: f32,
  line_inline_end: f32,
  skip_inline_start: f32,
  skip_inline_end: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpacerEdge {
  Start,
  End,
}

#[derive(Clone, Copy)]
struct BackgroundRects {
  border: Rect,
  padding: Rect,
  content: Rect,
}

#[derive(Default)]
struct TextEditOverlays {
  selection_rects: Vec<Rect>,
  caret_rect: Option<(Rect, Rgba)>,
}

#[derive(Clone)]
struct RootBackground {
  paint_rect: Rect,
  /// The border box rect used for computing background tiling/positioning metrics (repeat, default
  /// background-size, etc.).
  ///
  /// For HTML canvas background propagation this should generally match the canvas (viewport) rect.
  /// In particular, gradients promoted from `<html>`/`<body>` should interpolate relative to the
  /// viewport size rather than the potentially shorter element box.
  origin_rect: Rect,
  style: Arc<ComputedStyle>,
  paint_border: bool,
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImageKey {
  url: String,
  crossorigin: CrossOriginAttribute,
  referrer_policy: Option<crate::resource::ReferrerPolicy>,
  orientation: OrientationTransform,
  decorative: bool,
  used_resolution_bits: u32,
  device_pixel_ratio_bits: u32,
}

impl ImageKey {
  fn new(
    url: String,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    orientation: OrientationTransform,
    decorative: bool,
    used_resolution: f32,
    device_pixel_ratio: f32,
  ) -> Self {
    Self {
      url,
      crossorigin,
      referrer_policy,
      orientation,
      decorative,
      used_resolution_bits: f32_to_canonical_bits(used_resolution),
      device_pixel_ratio_bits: f32_to_canonical_bits(device_pixel_ratio),
    }
  }
}

struct DecodedImageCache {
  inner: LruCache<ImageKey, CachedImageEntry>,
  max_entries: usize,
  max_bytes: usize,
  current_bytes: usize,
}

struct CachedImageEntry {
  image: Arc<ImageData>,
  bytes: usize,
}

impl DecodedImageCache {
  fn new(max_entries: usize, max_bytes: usize) -> Self {
    Self {
      inner: LruCache::unbounded(),
      max_entries,
      max_bytes,
      current_bytes: 0,
    }
  }

  fn caching_disabled(&self) -> bool {
    self.max_entries == 0 || self.max_bytes == 0
  }

  fn estimate_bytes(image: &ImageData) -> usize {
    image.pixels.len()
  }

  fn get(&mut self, key: &ImageKey) -> Option<Arc<ImageData>> {
    if self.caching_disabled() {
      return None;
    }
    self.inner.get(key).map(|entry| Arc::clone(&entry.image))
  }

  fn insert(&mut self, key: ImageKey, image: Arc<ImageData>) {
    if self.caching_disabled() {
      return;
    }
    let bytes = Self::estimate_bytes(&image);
    if self.max_bytes > 0 && bytes > self.max_bytes {
      // Skip caching entries that would evict the entire cache on their own.
      return;
    }
    if let Some(entry) = self.inner.pop(&key) {
      self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
    }
    self.inner.put(key, CachedImageEntry { image, bytes });
    self.current_bytes = self.current_bytes.saturating_add(bytes);
    self.evict_if_needed();
  }

  fn evict_if_needed(&mut self) {
    while (self.max_entries > 0 && self.inner.len() > self.max_entries)
      || (self.max_bytes > 0 && self.current_bytes > self.max_bytes)
    {
      if let Some((_k, entry)) = self.inner.pop_lru() {
        self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
      } else {
        break;
      }
    }
  }
}

#[derive(Default)]
struct ParallelStats {
  tasks: AtomicUsize,
  parallel_ns: AtomicU64,
  serial_ns: AtomicU64,
  threads: Mutex<HashSet<ThreadId>>,
}

#[derive(Default, Clone, Copy)]
struct ParallelStatsSnapshot {
  tasks: usize,
  threads: usize,
  parallel_ns: u64,
  serial_ns: u64,
}

#[derive(Default)]
struct BuildBreakdown {
  stacking_tree_ns: AtomicU64,
  stacking_tree_calls: AtomicU64,
  fragment_paint_bounds_ns: AtomicU64,
  fragment_paint_bounds_calls: AtomicU64,
  text_shape_ns: AtomicU64,
  text_shape_calls: AtomicU64,
  text_decoration_ns: AtomicU64,
  text_decoration_calls: AtomicU64,
  image_decode_ns: AtomicU64,
  image_decode_calls: AtomicU64,
  clip_path_ns: AtomicU64,
  clip_path_calls: AtomicU64,
  border_radii_ns: AtomicU64,
  border_radii_calls: AtomicU64,
  svg_filter_ns: AtomicU64,
  svg_filter_calls: AtomicU64,
}

#[derive(Default, Clone, Copy)]
struct BuildBreakdownSnapshot {
  stacking_tree_ns: u64,
  stacking_tree_calls: u64,
  fragment_paint_bounds_ns: u64,
  fragment_paint_bounds_calls: u64,
  text_shape_ns: u64,
  text_shape_calls: u64,
  text_decoration_ns: u64,
  text_decoration_calls: u64,
  image_decode_ns: u64,
  image_decode_calls: u64,
  clip_path_ns: u64,
  clip_path_calls: u64,
  border_radii_ns: u64,
  border_radii_calls: u64,
  svg_filter_ns: u64,
  svg_filter_calls: u64,
}

#[derive(Clone, Copy)]
struct Visibility {
  rect: Option<Rect>,
  /// When true, content outside `rect` is guaranteed to be clipped away (e.g.,
  /// due to an explicit clip or mask), allowing more aggressive culling of
  /// effects that would otherwise overflow their base bounds.
  hard_clip: bool,
}

impl Visibility {
  fn none() -> Self {
    Self {
      rect: None,
      hard_clip: false,
    }
  }

  fn with_viewport(rect: Option<Rect>) -> Self {
    Self {
      rect,
      hard_clip: false,
    }
  }

  fn intersect(self, other: Option<Rect>, hard: bool) -> Self {
    let rect = match (self.rect, other) {
      (Some(a), Some(b)) => a.intersection(b),
      (a @ Some(_), None) => a,
      (None, b) => b,
    };
    Self {
      rect,
      hard_clip: self.hard_clip || (hard && rect.is_some()),
    }
  }
}

struct ConservativeGlyphRunBoundsBuilder {
  origin: Point,
  min_x: f32,
  max_x: f32,
}

#[derive(Clone, Copy)]
struct TileAxisPlan {
  start: f32,
  step: f32,
  count: usize,
}

impl TileAxisPlan {
  fn empty() -> Self {
    Self {
      start: 0.0,
      step: 0.0,
      count: 0,
    }
  }

  fn iter(self) -> TileAxisIter {
    TileAxisIter {
      pos: self.start,
      step: self.step,
      remaining: self.count,
    }
  }
}

struct TileAxisIter {
  pos: f32,
  step: f32,
  remaining: usize,
}

impl Iterator for TileAxisIter {
  type Item = f32;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    let out = self.pos;
    self.pos += self.step;
    self.remaining -= 1;
    Some(out)
  }
}

impl ConservativeGlyphRunBoundsBuilder {
  fn new(origin: Point, advance_width: f32) -> Self {
    Self {
      origin,
      min_x: origin.x,
      max_x: origin.x + advance_width,
    }
  }

  fn include_glyph(&mut self, offset_x: f32, advance: f32) {
    let gx = self.origin.x + offset_x;
    self.min_x = self.min_x.min(gx);
    self.max_x = self.max_x.max(gx + advance);
  }

  fn finish(self, font_size: f32) -> Rect {
    let ascent = font_size;
    let descent = font_size * 0.25;
    Rect::from_xywh(
      self.min_x,
      self.origin.y - ascent,
      (self.max_x - self.min_x).max(0.0),
      ascent + descent,
    )
  }
}

struct ParallelPlan {
  chunk_size: usize,
}

impl ParallelStats {
  fn record_parallel<I>(&self, tasks: usize, elapsed: Duration, threads: I)
  where
    I: IntoIterator<Item = ThreadId>,
  {
    self.tasks.fetch_add(tasks, Ordering::Relaxed);
    self.parallel_ns.fetch_add(
      elapsed.as_nanos().min(u64::MAX as u128) as u64,
      Ordering::Relaxed,
    );
    if let Ok(mut guard) = self.threads.lock() {
      for id in threads {
        guard.insert(id);
      }
    }
  }

  fn record_serial(&self, elapsed: Duration) {
    self.serial_ns.fetch_add(
      elapsed.as_nanos().min(u64::MAX as u128) as u64,
      Ordering::Relaxed,
    );
  }

  fn snapshot(&self) -> ParallelStatsSnapshot {
    let threads = self.threads.lock().map(|set| set.len()).unwrap_or(0);
    ParallelStatsSnapshot {
      tasks: self.tasks.load(Ordering::Relaxed),
      threads,
      parallel_ns: self.parallel_ns.load(Ordering::Relaxed),
      serial_ns: self.serial_ns.load(Ordering::Relaxed),
    }
  }
}

impl BuildBreakdown {
  #[inline]
  fn record_pair(ns: &AtomicU64, calls: &AtomicU64, elapsed: Duration) {
    ns.fetch_add(
      elapsed.as_nanos().min(u64::MAX as u128) as u64,
      Ordering::Relaxed,
    );
    calls.fetch_add(1, Ordering::Relaxed);
  }

  fn record_stacking_tree(&self, elapsed: Duration) {
    Self::record_pair(&self.stacking_tree_ns, &self.stacking_tree_calls, elapsed);
  }

  fn record_fragment_paint_bounds(&self, elapsed: Duration) {
    Self::record_pair(
      &self.fragment_paint_bounds_ns,
      &self.fragment_paint_bounds_calls,
      elapsed,
    );
  }

  fn record_text_shape(&self, elapsed: Duration) {
    Self::record_pair(&self.text_shape_ns, &self.text_shape_calls, elapsed);
  }

  fn record_text_decoration(&self, elapsed: Duration) {
    Self::record_pair(
      &self.text_decoration_ns,
      &self.text_decoration_calls,
      elapsed,
    );
  }

  fn record_image_decode(&self, elapsed: Duration) {
    Self::record_pair(&self.image_decode_ns, &self.image_decode_calls, elapsed);
  }

  fn record_clip_path(&self, elapsed: Duration) {
    Self::record_pair(&self.clip_path_ns, &self.clip_path_calls, elapsed);
  }

  fn record_border_radii(&self, elapsed: Duration) {
    Self::record_pair(&self.border_radii_ns, &self.border_radii_calls, elapsed);
  }

  fn record_svg_filter(&self, elapsed: Duration) {
    Self::record_pair(&self.svg_filter_ns, &self.svg_filter_calls, elapsed);
  }

  fn snapshot(&self) -> BuildBreakdownSnapshot {
    BuildBreakdownSnapshot {
      stacking_tree_ns: self.stacking_tree_ns.load(Ordering::Relaxed),
      stacking_tree_calls: self.stacking_tree_calls.load(Ordering::Relaxed),
      fragment_paint_bounds_ns: self.fragment_paint_bounds_ns.load(Ordering::Relaxed),
      fragment_paint_bounds_calls: self.fragment_paint_bounds_calls.load(Ordering::Relaxed),
      text_shape_ns: self.text_shape_ns.load(Ordering::Relaxed),
      text_shape_calls: self.text_shape_calls.load(Ordering::Relaxed),
      text_decoration_ns: self.text_decoration_ns.load(Ordering::Relaxed),
      text_decoration_calls: self.text_decoration_calls.load(Ordering::Relaxed),
      image_decode_ns: self.image_decode_ns.load(Ordering::Relaxed),
      image_decode_calls: self.image_decode_calls.load(Ordering::Relaxed),
      clip_path_ns: self.clip_path_ns.load(Ordering::Relaxed),
      clip_path_calls: self.clip_path_calls.load(Ordering::Relaxed),
      border_radii_ns: self.border_radii_ns.load(Ordering::Relaxed),
      border_radii_calls: self.border_radii_calls.load(Ordering::Relaxed),
      svg_filter_ns: self.svg_filter_ns.load(Ordering::Relaxed),
      svg_filter_calls: self.svg_filter_calls.load(Ordering::Relaxed),
    }
  }
}

fn parallel_config_from_env() -> (bool, usize, bool) {
  let toggles = runtime::runtime_toggles();
  let enabled = toggles.truthy_with_default("FASTR_DISPLAY_LIST_PARALLEL", true);
  let explicit_min = toggles.get("FASTR_DISPLAY_LIST_PARALLEL_MIN").is_some();
  let min = toggles
    .usize_with_default("FASTR_DISPLAY_LIST_PARALLEL_MIN", 32)
    .max(1);
  (enabled, min, explicit_min)
}

fn paint_build_breakdown_enabled() -> bool {
  paint_diagnostics_enabled() && runtime::runtime_toggles().truthy("FASTR_PAINT_BUILD_BREAKDOWN")
}

fn decoded_image_cache_limits_from_env() -> (usize, usize) {
  let toggles = runtime::runtime_toggles();
  let max_entries = toggles.usize_with_default(
    ENV_DECODED_IMAGE_CACHE_ITEMS,
    DEFAULT_DECODED_IMAGE_CACHE_MAX_ENTRIES,
  );
  let max_bytes = toggles.usize_with_default(
    ENV_DECODED_IMAGE_CACHE_BYTES,
    DEFAULT_DECODED_IMAGE_CACHE_MAX_BYTES,
  );
  (max_entries, max_bytes)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn trim_ascii_whitespace_start(value: &str) -> &str {
  value.trim_start_matches(|c: char| {
    matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
  })
}

fn split_at_char_idx(text: &str, char_idx: usize) -> (&str, &str) {
  let byte_idx = text
    .char_indices()
    .nth(char_idx)
    .map(|(idx, _)| idx)
    .unwrap_or(text.len());
  text.split_at(byte_idx)
}

impl DisplayListBuilder {
  fn viewport_rect(&self) -> Option<Rect> {
    self
      .culling_viewport
      .or(self.viewport)
      .map(|(w, h)| {
        // Viewport culling happens in the same coordinate space as the display list being built.
        // The paint pipeline passes an explicit translation offset (e.g. `-scroll`) when building
        // the display list, so the visible viewport remains anchored at (0,0).
        Rect::from_xywh(0.0, 0.0, w, h)
      })
      .filter(|r| r.width() > 0.0 && r.height() > 0.0)
  }

  fn root_visibility(&self) -> Visibility {
    Visibility::with_viewport(self.viewport_rect())
  }

  fn map_rect_with_transform(rect: Rect, transform: &Transform3D) -> Option<Rect> {
    if transform.is_identity() {
      return Some(rect);
    }
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
      let (tx, ty, _tz, tw) = transform.transform_point(x, y, 0.0);
      if !tx.is_finite()
        || !ty.is_finite()
        || !tw.is_finite()
        || tw.abs() < Transform3D::MIN_PROJECTIVE_W
        || tw < 0.0
      {
        return None;
      }
      let px = tx / tw;
      let py = ty / tw;
      min_x = min_x.min(px);
      min_y = min_y.min(py);
      max_x = max_x.max(px);
      max_y = max_y.max(py);
    }

    let width = max_x - min_x;
    let height = max_y - min_y;
    if width <= 0.0 || height <= 0.0 {
      return None;
    }
    Some(Rect::from_xywh(min_x, min_y, width, height))
  }

  fn visible_in_local_space(
    visible: Option<Rect>,
    transform: Option<&Transform3D>,
  ) -> Option<Rect> {
    let Some(view) = visible else {
      return None;
    };
    let Some(transform) = transform else {
      return Some(view);
    };
    // Descendant fragments are emitted in the *pre-transform* coordinate space and are transformed
    // later by `PushStackingContext`. If we can't invert the stacking context transform (e.g. 3D /
    // projective transforms, scale(0), etc), we can't safely map the visible rect back to local
    // space. Returning `None` disables rect-based culling to avoid false negatives (dropped
    // content).
    let Some(inverse) = transform.to_2d().and_then(|t| t.inverse()) else {
      return None;
    };
    Some(inverse.transform_rect(view))
  }
  fn clip_bounds(clip: &ClipItem) -> Option<Rect> {
    match &clip.shape {
      ClipShape::Rect { rect, .. } => Some(*rect),
      ClipShape::Path { path } => Some(path.bounds()),
      ClipShape::Text { runs } => Some(crate::paint::display_list::text_runs_bounds(runs.as_ref())),
      ClipShape::AlphaMask { rect, .. } => Some(*rect),
    }
  }

  fn map_clip_bounds(clip: &ClipItem, transform: Option<&Transform3D>) -> Option<Rect> {
    let bounds = Self::clip_bounds(clip)?;
    match transform {
      Some(t) => Self::map_rect_with_transform(bounds, t),
      None => Some(bounds),
    }
  }

  fn fragment_paint_bounds(
    &self,
    fragment: &FragmentNode,
    absolute_rect: Rect,
    style: Option<&ComputedStyle>,
  ) -> Rect {
    let paint_bounds_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let bounds = crate::paint::paint_bounds::fragment_paint_bounds(
      fragment,
      absolute_rect,
      style,
      self.viewport,
    );
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), paint_bounds_timer) {
      breakdown.record_fragment_paint_bounds(start.elapsed());
    }
    bounds
  }

  fn deadline_reached(&mut self) -> bool {
    if self.error.is_some() {
      return true;
    }
    if let Some(err) = crate::render_control::active_allocation_budget_error() {
      self.error = Some(err);
      return true;
    }
    if let Err(err) = check_active(RenderStage::Paint) {
      self.error = Some(err);
      return true;
    }
    false
  }

  fn deadline_reached_periodic(&mut self, counter: &mut usize, stride: usize) -> bool {
    if stride == 0 {
      return self.deadline_reached();
    }
    if self.error.is_some() {
      return true;
    }
    if let Some(err) = crate::render_control::active_allocation_budget_error() {
      self.error = Some(err);
      return true;
    }
    if let Err(err) = check_active_periodic(counter, stride, RenderStage::Paint) {
      self.error = Some(err);
      return true;
    }
    false
  }

  fn finish(mut self) -> Result<DisplayList> {
    if paint_diagnostics_enabled() {
      self.record_parallel_diagnostics();
      let background_tiles = self
        .background_tiles
        .as_ref()
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0);
      let background_layers = self
        .background_layers
        .as_ref()
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0);
      let background_pattern_fast_paths = self
        .background_pattern_fast_paths
        .as_ref()
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0);

      if background_tiles > 0 || background_layers > 0 || background_pattern_fast_paths > 0 {
        with_paint_diagnostics(|diag| {
          if background_tiles > 0 {
            diag.background_tiles = diag.background_tiles.saturating_add(background_tiles);
          }
          if background_layers > 0 {
            diag.background_layers = diag.background_layers.saturating_add(background_layers);
          }
          if background_pattern_fast_paths > 0 {
            diag.background_pattern_fast_paths = diag
              .background_pattern_fast_paths
              .saturating_add(background_pattern_fast_paths);
          }
        });
      }

      if let Some(breakdown) = self.build_breakdown.as_ref() {
        let snapshot = breakdown.snapshot();
        if snapshot.stacking_tree_calls > 0
          || snapshot.fragment_paint_bounds_calls > 0
          || snapshot.text_shape_calls > 0
          || snapshot.text_decoration_calls > 0
          || snapshot.image_decode_calls > 0
          || snapshot.clip_path_calls > 0
          || snapshot.border_radii_calls > 0
          || snapshot.svg_filter_calls > 0
        {
          with_paint_diagnostics(|diag| {
            diag.build_stacking_tree_ms += snapshot.stacking_tree_ns as f64 / 1_000_000.0;
            diag.build_stacking_tree_calls += snapshot.stacking_tree_calls;
            diag.build_fragment_paint_bounds_ms +=
              snapshot.fragment_paint_bounds_ns as f64 / 1_000_000.0;
            diag.build_fragment_paint_bounds_calls += snapshot.fragment_paint_bounds_calls;
            diag.build_text_shape_ms += snapshot.text_shape_ns as f64 / 1_000_000.0;
            diag.build_text_shape_calls += snapshot.text_shape_calls;
            diag.build_text_decoration_ms += snapshot.text_decoration_ns as f64 / 1_000_000.0;
            diag.build_text_decoration_calls += snapshot.text_decoration_calls;
            diag.build_image_decode_ms += snapshot.image_decode_ns as f64 / 1_000_000.0;
            diag.build_image_decode_calls += snapshot.image_decode_calls;
            diag.build_clip_path_ms += snapshot.clip_path_ns as f64 / 1_000_000.0;
            diag.build_clip_path_calls += snapshot.clip_path_calls;
            diag.build_border_radii_ms += snapshot.border_radii_ns as f64 / 1_000_000.0;
            diag.build_border_radii_calls += snapshot.border_radii_calls;
            diag.build_svg_filter_ms += snapshot.svg_filter_ns as f64 / 1_000_000.0;
            diag.build_svg_filter_calls += snapshot.svg_filter_calls;
          });
        }
      }
    }
    if self.error.is_none() {
      if let Some(err) = crate::render_control::active_allocation_budget_error() {
        self.error = Some(err);
      }
    }
    if let Some(err) = self.error.take() {
      Err(Error::Render(err))
    } else {
      let has_gif_images = self.saw_gif_image.load(Ordering::Relaxed);
      let has_time_dependent_images =
        self.saw_animation_time_dependent_image.load(Ordering::Relaxed);
      let mut list = self.list;
      list.set_has_gif_images(has_gif_images);
      list.set_has_animation_time_dependent_images(has_time_dependent_images);
      Ok(list)
    }
  }

  fn resolve_scaled_metrics(
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
    fragment
      .box_id()
      .and_then(|id| self.scroll_state.elements.get(&id).copied())
      .unwrap_or(Point::ZERO)
  }

  fn snap_form_control_caret_rect(&self, rect: Rect) -> Rect {
    let dpr = self.device_pixel_ratio;
    if !dpr.is_finite() || dpr <= f32::EPSILON {
      return rect;
    }
    if !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
    {
      return rect;
    }
    let x = (rect.x() * dpr).round() / dpr;
    let y = (rect.y() * dpr).round() / dpr;
    let w = (rect.width() * dpr).round().max(1.0) / dpr;
    let h = (rect.height() * dpr).round().max(1.0) / dpr;
    Rect::from_xywh(x, y, w, h)
  }

  fn line_decoration_clip_range(&self, inline_start: f32, inline_len: f32) -> Option<(f32, f32)> {
    let ctx = self.line_decoration_ctx?;
    if inline_len <= 0.0 {
      return None;
    }
    let clip_start = ctx.line_inline_start + ctx.skip_inline_start;
    let clip_end = ctx.line_inline_end - ctx.skip_inline_end;
    if !clip_start.is_finite() || !clip_end.is_finite() {
      return None;
    }

    let frag_end = inline_start + inline_len;
    let start = clip_start.max(inline_start);
    let end = clip_end.min(frag_end);
    if end <= start + f32::EPSILON {
      return Some((0.0, 0.0));
    }
    let rel_start = (start - inline_start).clamp(0.0, inline_len);
    let rel_end = (end - inline_start).clamp(0.0, inline_len);
    if rel_start <= 0.001 && (inline_len - rel_end) <= 0.001 {
      None
    } else {
      Some((rel_start, rel_end))
    }
  }

  fn build_line_decoration_context(
    &mut self,
    line: &FragmentNode,
    child_offset: Point,
    baseline: f32,
  ) -> LineDecorationContext {
    let style_hint = Self::line_style_hint(line);
    let inline_vertical = style_hint.is_some_and(|style| {
      matches!(
        style.writing_mode,
        crate::style::types::WritingMode::VerticalRl
          | crate::style::types::WritingMode::VerticalLr
          | crate::style::types::WritingMode::SidewaysRl
          | crate::style::types::WritingMode::SidewaysLr
      )
    });
    let block_baseline = if inline_vertical {
      child_offset.x + baseline
    } else {
      child_offset.y + baseline
    };

    let mut line_inline_start = f32::INFINITY;
    let mut line_inline_end = f32::NEG_INFINITY;
    for child in line.children.iter() {
      let rect = Rect::new(
        child_offset.translate(child.bounds.origin),
        child.bounds.size,
      );
      let (start, end) = if inline_vertical {
        (rect.y(), rect.y() + rect.height())
      } else {
        (rect.x(), rect.x() + rect.width())
      };
      if end > start {
        line_inline_start = line_inline_start.min(start);
        line_inline_end = line_inline_end.max(end);
      }
    }
    if !line_inline_start.is_finite()
      || !line_inline_end.is_finite()
      || line_inline_end < line_inline_start
    {
      line_inline_start = if inline_vertical {
        child_offset.y
      } else {
        child_offset.x
      };
      let fallback_len = if inline_vertical {
        line.bounds.height()
      } else {
        line.bounds.width()
      };
      line_inline_end = line_inline_start + fallback_len.max(0.0);
    }

    let mut skip_inline_start = 0.0;
    let mut skip_inline_end = 0.0;
    if let Some(style) = style_hint {
      let inline_forward = inline_axis_positive(style.writing_mode, style.direction);
      #[derive(Clone, Copy)]
      struct TextLeaf<'a> {
        fragment: &'a FragmentNode,
        inline_start: f32,
        inline_end: f32,
      }

      fn visit<'a>(
        fragment: &'a FragmentNode,
        offset: Point,
        inline_vertical: bool,
        best_min: &mut Option<TextLeaf<'a>>,
        best_max: &mut Option<TextLeaf<'a>>,
      ) {
        // This used to recurse directly on `fragment.children`, which meant adversarially deep
        // inline fragment chains inside a line box could stack overflow. Use an explicit stack so
        // traversal depth is bounded by heap memory, not the call stack.
        let mut stack: Vec<(&'a FragmentNode, Point)> = vec![(fragment, offset)];
        while let Some((fragment, offset)) = stack.pop() {
          let rect = Rect::new(
            offset.translate(fragment.bounds.origin),
            fragment.bounds.size,
          );
          let (inline_start, inline_end) = if inline_vertical {
            (rect.y(), rect.y() + rect.height())
          } else {
            (rect.x(), rect.x() + rect.width())
          };

          match fragment.content {
            FragmentContent::Text { .. } => {
              if let Some(existing) = best_min.as_ref() {
                if inline_start < existing.inline_start {
                  *best_min = Some(TextLeaf {
                    fragment,
                    inline_start,
                    inline_end,
                  });
                }
              } else {
                *best_min = Some(TextLeaf {
                  fragment,
                  inline_start,
                  inline_end,
                });
              }

              if let Some(existing) = best_max.as_ref() {
                if inline_end > existing.inline_end {
                  *best_max = Some(TextLeaf {
                    fragment,
                    inline_start,
                    inline_end,
                  });
                }
              } else {
                *best_max = Some(TextLeaf {
                  fragment,
                  inline_start,
                  inline_end,
                });
              }
              continue;
            }
            FragmentContent::Replaced { .. }
            | FragmentContent::Line { .. }
            | FragmentContent::RunningAnchor { .. } => continue,
            _ => {}
          }

          if fragment.style.as_deref().is_some_and(|style| {
            style.display.is_inline_level() && style.display.establishes_formatting_context()
          }) {
            continue;
          }

          let child_offset = rect.origin;
          // Preserve the original recursive DFS order: visit children left-to-right.
          for child in fragment.children.iter().rev() {
            stack.push((child, child_offset));
          }
        }
      }

      let mut min_text: Option<TextLeaf<'_>> = None;
      let mut max_text: Option<TextLeaf<'_>> = None;
      for child in line.children.iter() {
        visit(
          child,
          child_offset,
          inline_vertical,
          &mut min_text,
          &mut max_text,
        );
      }

      // If styles vary within the line, match the spec intent by using the skip-spaces value at
      // the logical line start/end rather than relying on an arbitrary style hint.
      let physical_start_spaces = min_text
        .as_ref()
        .and_then(|leaf| leaf.fragment.style.as_deref())
        .map(|style| style.text_decoration_skip_spaces);
      let physical_end_spaces = max_text
        .as_ref()
        .and_then(|leaf| leaf.fragment.style.as_deref())
        .map(|style| style.text_decoration_skip_spaces);
      let start_skip_spaces = if inline_forward {
        physical_start_spaces.unwrap_or(style.text_decoration_skip_spaces)
      } else {
        physical_end_spaces.unwrap_or(style.text_decoration_skip_spaces)
      };
      let end_skip_spaces = if inline_forward {
        physical_end_spaces.unwrap_or(style.text_decoration_skip_spaces)
      } else {
        physical_start_spaces.unwrap_or(style.text_decoration_skip_spaces)
      };
      let skip_start = start_skip_spaces.skips_start();
      let skip_end = end_skip_spaces.skips_end();

      if (skip_start || skip_end) && line_inline_end > line_inline_start {
        let eps = 0.01;
        let leading = min_text
          .filter(|leaf| (leaf.inline_start - line_inline_start).abs() <= eps)
          .map(|leaf| {
            self.text_fragment_spacer_advance(leaf.fragment, SpacerEdge::Start, inline_vertical)
          })
          .unwrap_or(0.0);
        let trailing = max_text
          .filter(|leaf| (leaf.inline_end - line_inline_end).abs() <= eps)
          .map(|leaf| {
            self.text_fragment_spacer_advance(leaf.fragment, SpacerEdge::End, inline_vertical)
          })
          .unwrap_or(0.0);

        if skip_start {
          if inline_forward {
            skip_inline_start = leading;
          } else {
            skip_inline_end = trailing;
          }
        }
        if skip_end {
          if inline_forward {
            skip_inline_end = trailing;
          } else {
            skip_inline_start = leading;
          }
        }

        let max_span = (line_inline_end - line_inline_start).max(0.0);
        skip_inline_start = if skip_inline_start.is_finite() {
          skip_inline_start.clamp(0.0, max_span)
        } else {
          0.0
        };
        skip_inline_end = if skip_inline_end.is_finite() {
          skip_inline_end.clamp(0.0, max_span)
        } else {
          0.0
        };
      }
    }

    LineDecorationContext {
      inline_vertical,
      block_baseline,
      line_inline_start,
      line_inline_end,
      skip_inline_start,
      skip_inline_end,
    }
  }

  fn line_style_hint(fragment: &FragmentNode) -> Option<&ComputedStyle> {
    // This is used for line boxes, which often don't have their own style and need a hint from a
    // descendant. Deeply nested inline fragments can be attacker-controlled, so avoid recursion.
    let mut stack: Vec<&FragmentNode> = vec![fragment];
    while let Some(fragment) = stack.pop() {
      if let Some(style) = fragment.style.as_deref() {
        return Some(style);
      }
      // Preserve the original recursive DFS order: visit children left-to-right.
      for child in fragment.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn text_fragment_spacer_advance(
    &mut self,
    fragment: &FragmentNode,
    edge: SpacerEdge,
    inline_vertical: bool,
  ) -> f32 {
    let (text, shaped) = match &fragment.content {
      FragmentContent::Text { text, shaped, .. } => (text.as_ref(), shaped.as_deref()),
      _ => return 0.0,
    };
    let Some(style) = fragment.style.as_deref() else {
      return 0.0;
    };

    if let Some(runs) = shaped {
      return Self::spacer_advance_in_runs(runs, edge, inline_vertical);
    }

    let Ok(mut runs) = self.shaper.shape(text, style, &self.font_ctx) else {
      return 0.0;
    };
    InlineTextItem::apply_spacing_to_runs(
      &mut runs,
      text,
      style.letter_spacing,
      style.word_spacing,
    );
    Self::spacer_advance_in_runs(&runs, edge, inline_vertical)
  }

  fn spacer_advance_in_runs(runs: &[ShapedRun], edge: SpacerEdge, inline_vertical: bool) -> f32 {
    let mut advance = 0.0;
    match edge {
      SpacerEdge::Start => {
        for run in runs {
          for glyph in &run.glyphs {
            let idx = glyph.cluster as usize;
            let Some(ch) = run.text.get(idx..).and_then(|s| s.chars().next()) else {
              return advance;
            };
            if !Self::is_spacer_char(ch) {
              return advance;
            }
            advance += if inline_vertical {
              if glyph.y_advance.abs() > f32::EPSILON {
                glyph.y_advance
              } else {
                glyph.x_advance
              }
            } else {
              glyph.x_advance
            };
          }
        }
      }
      SpacerEdge::End => {
        for run in runs.iter().rev() {
          for glyph in run.glyphs.iter().rev() {
            let idx = glyph.cluster as usize;
            let Some(ch) = run.text.get(idx..).and_then(|s| s.chars().next()) else {
              return advance;
            };
            if !Self::is_spacer_char(ch) {
              return advance;
            }
            advance += if inline_vertical {
              if glyph.y_advance.abs() > f32::EPSILON {
                glyph.y_advance
              } else {
                glyph.x_advance
              }
            } else {
              glyph.x_advance
            };
          }
        }
      }
    }

    if advance.is_finite() {
      advance.max(0.0)
    } else {
      0.0
    }
  }

  fn is_spacer_char(ch: char) -> bool {
    ch != '\u{202F}' && matches!(get_general_category(ch), GeneralCategory::SpaceSeparator)
  }

  /// Creates a new display list builder
  pub fn new() -> Self {
    let (parallel_enabled, parallel_min, parallel_min_explicit) = parallel_config_from_env();
    let parallel_root_min = if parallel_min_explicit {
      parallel_min
    } else {
      parallel_min.saturating_mul(4)
    };
    let (decoded_image_cache_entries, decoded_image_cache_bytes) = decoded_image_cache_limits_from_env();
    Self {
      list: DisplayList::new(),
      image_cache: Some(ImageCache::new()),
      saw_gif_image: Arc::new(AtomicBool::new(false)),
      saw_animation_time_dependent_image: Arc::new(AtomicBool::new(false)),
      media_provider: None,
      decoded_image_cache: Arc::new(Mutex::new(DecodedImageCache::new(
        decoded_image_cache_entries,
        decoded_image_cache_bytes,
      ))),
      svg_filter_defs: None,
      svg_id_defs: None,
      svg_id_defs_raw: None,
      appearance_none_form_controls: None,
      viewport: None,
      culling_viewport: None,
      font_ctx: FontContext::new(),
      shaper: ShapingPipeline::new(),
      device_pixel_ratio: 1.0,
      parallel_enabled,
      parallel_mode: PaintParallelismMode::Auto,
      parallel_min,
      parallel_root_min,
      parallel_min_explicit,
      parallel_stats: paint_diagnostics_enabled().then(|| Arc::new(ParallelStats::default())),
      build_breakdown: paint_build_breakdown_enabled().then(|| Arc::new(BuildBreakdown::default())),
      background_tiles: paint_diagnostics_enabled().then(|| Arc::new(AtomicU64::new(0))),
      background_layers: paint_diagnostics_enabled().then(|| Arc::new(AtomicU64::new(0))),
      background_pattern_fast_paths: paint_diagnostics_enabled()
        .then(|| Arc::new(AtomicU64::new(0))),
      canvas_background_suppress_box_id: None,
      canvas_border_suppress_box_id: None,
      estimated_fragments: None,
      scroll_state: ScrollState::default(),
      max_iframe_depth: DEFAULT_MAX_IFRAME_DEPTH,
      skip_stacking_context_children: false,
      line_decoration_ctx: None,
      error: None,
    }
  }

  /// Creates a display list builder backed by an image cache to rasterize replaced images.
  pub fn with_image_cache(image_cache: ImageCache) -> Self {
    let (parallel_enabled, parallel_min, parallel_min_explicit) = parallel_config_from_env();
    let parallel_root_min = if parallel_min_explicit {
      parallel_min
    } else {
      parallel_min.saturating_mul(4)
    };
    let (decoded_image_cache_entries, decoded_image_cache_bytes) = decoded_image_cache_limits_from_env();
    Self {
      list: DisplayList::new(),
      image_cache: Some(image_cache),
      saw_gif_image: Arc::new(AtomicBool::new(false)),
      saw_animation_time_dependent_image: Arc::new(AtomicBool::new(false)),
      media_provider: None,
      decoded_image_cache: Arc::new(Mutex::new(DecodedImageCache::new(
        decoded_image_cache_entries,
        decoded_image_cache_bytes,
      ))),
      svg_filter_defs: None,
      svg_id_defs: None,
      svg_id_defs_raw: None,
      appearance_none_form_controls: None,
      viewport: None,
      culling_viewport: None,
      font_ctx: FontContext::new(),
      shaper: ShapingPipeline::new(),
      device_pixel_ratio: 1.0,
      parallel_enabled,
      parallel_mode: PaintParallelismMode::Auto,
      parallel_min,
      parallel_root_min,
      parallel_min_explicit,
      parallel_stats: paint_diagnostics_enabled().then(|| Arc::new(ParallelStats::default())),
      build_breakdown: paint_build_breakdown_enabled().then(|| Arc::new(BuildBreakdown::default())),
      background_tiles: paint_diagnostics_enabled().then(|| Arc::new(AtomicU64::new(0))),
      background_layers: paint_diagnostics_enabled().then(|| Arc::new(AtomicU64::new(0))),
      background_pattern_fast_paths: paint_diagnostics_enabled()
        .then(|| Arc::new(AtomicU64::new(0))),
      canvas_background_suppress_box_id: None,
      canvas_border_suppress_box_id: None,
      estimated_fragments: None,
      scroll_state: ScrollState::default(),
      max_iframe_depth: DEFAULT_MAX_IFRAME_DEPTH,
      skip_stacking_context_children: false,
      line_decoration_ctx: None,
      error: None,
    }
  }

  /// Sets the base URL used for resolving relative image URLs when decoding backgrounds/replaced elements.
  pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
    let url = base_url.into();
    self.image_cache = self.image_cache.take().map(|mut cache| {
      cache.set_base_url(url);
      cache
    });
    self
  }

  /// Updates the base URL on the underlying image cache.
  pub fn set_base_url(&mut self, base_url: impl Into<String>) {
    if let Some(cache) = self.image_cache.as_mut() {
      cache.set_base_url(base_url);
    }
  }

  /// Sets serialized SVG filter definitions to use when resolving `url(#...)` filters.
  pub fn with_svg_filter_defs(mut self, defs: Option<Arc<HashMap<String, String>>>) -> Self {
    self.svg_filter_defs = defs;
    self
  }

  /// Updates the SVG filter registry used for `url(#...)` filters.
  pub fn set_svg_filter_defs(&mut self, defs: Option<Arc<HashMap<String, String>>>) {
    self.svg_filter_defs = defs;
  }

  /// Sets serialized SVG id definitions to use when resolving fragment-only `url(#...)` mask images.
  pub fn with_svg_id_defs(mut self, defs: Option<Arc<HashMap<String, String>>>) -> Self {
    self.svg_id_defs = defs;
    self
  }

  /// Updates the SVG id registry used for fragment-only `url(#...)` mask images.
  pub fn set_svg_id_defs(&mut self, defs: Option<Arc<HashMap<String, String>>>) {
    self.svg_id_defs = defs;
  }

  /// Sets raw serialized SVG id definitions to use when resolving same-document fragment
  /// references inside inline SVG (e.g. `<use href="#id">`).
  pub fn with_svg_id_defs_raw(mut self, defs: Option<Arc<HashMap<String, String>>>) -> Self {
    self.svg_id_defs_raw = defs;
    self
  }

  /// Updates the raw SVG id registry used for inline SVG `<use>`/paint-server references.
  pub fn set_svg_id_defs_raw(&mut self, defs: Option<Arc<HashMap<String, String>>>) {
    self.svg_id_defs_raw = defs;
  }

  /// Sets appearance-none form-control metadata (box id → control) for text-edit overlays.
  pub fn with_appearance_none_form_controls(
    mut self,
    controls: Option<Arc<HashMap<usize, Arc<FormControl>>>>,
  ) -> Self {
    self.appearance_none_form_controls = controls;
    self
  }

  /// Updates appearance-none form-control metadata in place.
  pub fn set_appearance_none_form_controls(
    &mut self,
    controls: Option<Arc<HashMap<usize, Arc<FormControl>>>>,
  ) {
    self.appearance_none_form_controls = controls;
  }

  /// Sets the font context for shaping text into the display list.
  pub fn with_font_context(mut self, font_ctx: FontContext) -> Self {
    self.font_ctx = font_ctx;
    self
  }

  /// Sets the device pixel ratio for density selection (e.g., srcset/image-set).
  pub fn with_device_pixel_ratio(mut self, dpr: f32) -> Self {
    self.device_pixel_ratio = if dpr.is_finite() && dpr > 0.0 {
      dpr
    } else {
      1.0
    };
    self
  }

  /// Updates the device pixel ratio in place.
  pub fn set_device_pixel_ratio(&mut self, dpr: f32) {
    self.device_pixel_ratio = if dpr.is_finite() && dpr > 0.0 {
      dpr
    } else {
      1.0
    };
  }

  /// Sets the viewport size for resolving viewport-relative units (vw/vh) in object-position.
  pub fn with_viewport_size(mut self, width: f32, height: f32) -> Self {
    self.viewport = Some((width, height));
    self
  }

  /// Sets the viewport size used for visibility/culling while building the display list.
  ///
  /// This should typically match the paint surface size in CSS pixels (e.g. the pixmap dimensions
  /// passed to the painter). When unset, culling falls back to the layout viewport (`viewport`).
  pub fn with_culling_viewport_size(mut self, width: f32, height: f32) -> Self {
    self.culling_viewport = Some((width, height));
    self
  }

  /// Sets the scroll state used when translating element content during display list construction.
  pub fn with_scroll_state(mut self, scroll_state: ScrollState) -> Self {
    self.scroll_state = scroll_state;
    self
  }

  /// Sets the media provider used for supplying decoded frames (e.g. `<video>`) while building the
  /// display list.
  pub fn with_media_provider(
    mut self,
    media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  ) -> Self {
    self.media_provider = media_provider;
    self
  }

  /// Sets the maximum iframe nesting depth for nested browsing context renders.
  pub fn with_max_iframe_depth(mut self, max_iframe_depth: usize) -> Self {
    self.max_iframe_depth = max_iframe_depth;
    self
  }

  /// Applies parallelism tuning shared with the display list renderer.
  pub fn with_parallelism(mut self, parallelism: &PaintParallelism) -> Self {
    self.set_parallelism(parallelism);
    self
  }

  /// Updates parallelism tuning in place.
  pub fn set_parallelism(&mut self, parallelism: &PaintParallelism) {
    self.parallel_mode = parallelism.mode;
    if !self.parallel_min_explicit {
      self.parallel_min = parallelism.build_chunk_size.max(1);
    }
    if !self.parallel_min_explicit {
      self.parallel_root_min = parallelism
        .min_build_fragments
        .max(self.parallel_min.saturating_mul(2));
    }
    self.estimated_fragments = None;
  }

  /// Builds a display list from a fragment tree root
  pub fn build(mut self, root: &FragmentNode) -> DisplayList {
    self
      .build_checked(root)
      .unwrap_or_else(|_| DisplayList::new())
  }

  /// Builds a display list from a fragment tree root while surfacing timeouts.
  pub fn build_checked(mut self, root: &FragmentNode) -> Result<DisplayList> {
    if self.viewport.is_none() {
      self.viewport = Some((root.bounds.width(), root.bounds.height()));
    }
    self.estimate_from_roots(std::iter::once(root));
    self.build_fragment(
      root,
      Point::ZERO,
      Visibility::with_viewport(self.viewport_rect()),
    );
    self.finish()
  }

  /// Builds a display list from a FragmentTree
  pub fn build_tree(mut self, tree: &FragmentTree) -> DisplayList {
    self
      .build_tree_checked(tree)
      .unwrap_or_else(|_| DisplayList::new())
  }

  /// Builds a display list from a FragmentTree and propagates timeouts.
  pub fn build_tree_checked(mut self, tree: &FragmentTree) -> Result<DisplayList> {
    if self.viewport.is_none() {
      let viewport = tree.viewport_size();
      self.viewport = Some((viewport.width, viewport.height));
      if self.culling_viewport.is_none() && !tree.has_explicit_viewport() {
        let content_bounds = tree.content_size();
        let max_x = content_bounds.max_x().max(0.0);
        let max_y = content_bounds.max_y().max(0.0);
        if max_x > 0.0 && max_y > 0.0 {
          self.culling_viewport = Some((max_x, max_y));
        }
      }
    }
    self.estimate_from_tree(tree);
    for root in std::iter::once(&tree.root).chain(tree.additional_fragments.iter()) {
      self.build_fragment(
        root,
        Point::ZERO,
        Visibility::with_viewport(self.viewport_rect()),
      );
    }
    self.finish()
  }

  /// Builds a stacking-context-aware display list from a `FragmentTree`.
  pub fn build_tree_with_stacking(mut self, tree: &FragmentTree) -> DisplayList {
    self
      .build_tree_with_stacking_checked(tree)
      .unwrap_or_else(|_| DisplayList::new())
  }

  pub fn build_tree_with_stacking_checked(mut self, tree: &FragmentTree) -> Result<DisplayList> {
    self.skip_stacking_context_children = true;
    if self.viewport.is_none() {
      let viewport = tree.viewport_size();
      self.viewport = Some((viewport.width, viewport.height));
      if self.culling_viewport.is_none() && !tree.has_explicit_viewport() {
        let content_bounds = tree.content_size();
        let max_x = content_bounds.max_x().max(0.0);
        let max_y = content_bounds.max_y().max(0.0);
        if max_x > 0.0 && max_y > 0.0 {
          self.culling_viewport = Some((max_x, max_y));
        }
      }
    }
    self.estimate_from_tree(tree);
    let svg_roots: Vec<&FragmentNode> = std::iter::once(&tree.root)
      .chain(tree.additional_fragments.iter())
      .collect();
    let image_cache = self.image_cache.clone();
    let defs = tree
      .svg_filter_defs
      .clone()
      .or_else(|| self.svg_filter_defs.clone());
    let mut svg_filters = SvgFilterResolver::new(defs, svg_roots, image_cache.as_ref());

    self.svg_id_defs = tree
      .svg_id_defs
      .clone()
      .or_else(|| self.svg_id_defs.clone());
    self.svg_id_defs_raw = tree
      .svg_id_defs_raw
      .clone()
      .or_else(|| self.svg_id_defs_raw.clone());

    let stacking_tree_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let mut contexts = crate::paint::stacking::build_stacking_tree_from_tree_checked_with_scroll(
      tree,
      &self.scroll_state,
    )?;
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), stacking_tree_timer) {
      breakdown.record_stacking_tree(start.elapsed());
    }
    for context in &mut contexts {
      context.compute_bounds(self.viewport, Some(&mut svg_filters))?;
    }
    let visibility = self.root_visibility();
    for context in &contexts {
      let _ =
        self.build_stacking_context(context, Point::ZERO, true, &mut svg_filters, visibility, 0);
    }

    self.finish()
  }

  /// Builds a display list from a stacking context tree (respecting z-order).
  pub fn build_from_stacking(mut self, stacking: &StackingContext) -> DisplayList {
    self
      .build_from_stacking_checked(stacking)
      .unwrap_or_else(|_| DisplayList::new())
  }

  pub fn build_from_stacking_checked(mut self, stacking: &StackingContext) -> Result<DisplayList> {
    self.skip_stacking_context_children = true;
    if self.viewport.is_none() {
      let max_x = stacking.bounds.max_x().max(0.0);
      let max_y = stacking.bounds.max_y().max(0.0);
      if max_x > 0.0 && max_y > 0.0 {
        self.viewport = Some((max_x, max_y));
      }
    }
    let mut svg_roots = Vec::new();
    Self::collect_stacking_fragments(stacking, &mut svg_roots);
    self.estimate_from_roots(svg_roots.iter().copied());
    let image_cache = self.image_cache.clone();
    let mut svg_filters = SvgFilterResolver::new(
      self.svg_filter_defs.clone(),
      svg_roots,
      image_cache.as_ref(),
    );
    let _ = self.build_stacking_context(
      stacking,
      Point::ZERO,
      true,
      &mut svg_filters,
      self.root_visibility(),
      0,
    );
    self.finish()
  }

  /// Builds a display list by first constructing a stacking context tree from the fragment tree.
  pub fn build_with_stacking_tree(mut self, root: &FragmentNode) -> DisplayList {
    self
      .build_with_stacking_tree_checked(root)
      .unwrap_or_else(|_| DisplayList::new())
  }

  pub fn build_with_stacking_tree_checked(mut self, root: &FragmentNode) -> Result<DisplayList> {
    self.skip_stacking_context_children = true;
    if self.viewport.is_none() {
      self.viewport = Some((root.bounds.width(), root.bounds.height()));
    }
    self.estimate_from_roots(std::iter::once(root));
    let stacking_tree_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let mut stacking =
      crate::paint::stacking::build_stacking_tree_from_fragment_tree_checked_with_scroll(
        root,
        &self.scroll_state,
      )?;
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), stacking_tree_timer) {
      breakdown.record_stacking_tree(start.elapsed());
    }
    let image_cache = self.image_cache.clone();
    let mut svg_filters = SvgFilterResolver::new(
      self.svg_filter_defs.clone(),
      vec![root],
      image_cache.as_ref(),
    );
    stacking.compute_bounds(self.viewport, Some(&mut svg_filters))?;
    let _ = self.build_stacking_context(
      &stacking,
      Point::ZERO,
      true,
      &mut svg_filters,
      self.root_visibility(),
      0,
    );
    self.finish()
  }

  /// Builds a display list by first constructing a stacking context tree from the fragment tree
  /// and applying an additional offset to all fragments.
  pub fn build_with_stacking_tree_offset(
    mut self,
    root: &FragmentNode,
    offset: Point,
  ) -> DisplayList {
    self
      .build_with_stacking_tree_offset_checked(root, offset)
      .unwrap_or_else(|_| DisplayList::new())
  }

  /// Builds a display list by first constructing a stacking context tree from the fragment tree
  /// and applying an additional offset to all fragments, surfacing timeouts.
  pub fn build_with_stacking_tree_offset_checked(
    mut self,
    root: &FragmentNode,
    offset: Point,
  ) -> Result<DisplayList> {
    self.skip_stacking_context_children = true;
    if self.viewport.is_none() {
      self.viewport = Some((root.bounds.width(), root.bounds.height()));
    }
    self.estimate_from_roots(std::iter::once(root));
    let stacking_tree_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let mut stacking =
      crate::paint::stacking::build_stacking_tree_from_fragment_tree_checked_with_scroll(
        root,
        &self.scroll_state,
      )?;
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), stacking_tree_timer) {
      breakdown.record_stacking_tree(start.elapsed());
    }
    let image_cache = self.image_cache.clone();
    let mut svg_filters = SvgFilterResolver::new(
      self.svg_filter_defs.clone(),
      vec![root],
      image_cache.as_ref(),
    );
    stacking.compute_bounds(self.viewport, Some(&mut svg_filters))?;
    let _ = self.build_stacking_context(
      &stacking,
      offset,
      true,
      &mut svg_filters,
      self.root_visibility(),
      0,
    );
    self.finish()
  }

  /// Builds a display list from multiple stacking context roots.
  pub fn build_from_stacking_contexts(mut self, stackings: &[StackingContext]) -> DisplayList {
    self
      .build_from_stacking_contexts_checked(stackings)
      .unwrap_or_else(|_| DisplayList::new())
  }

  pub fn build_from_stacking_contexts_checked(
    mut self,
    stackings: &[StackingContext],
  ) -> Result<DisplayList> {
    self.skip_stacking_context_children = true;
    if self.viewport.is_none() {
      let mut max_x: f32 = 0.0;
      let mut max_y: f32 = 0.0;
      for stacking in stackings {
        max_x = max_x.max(stacking.bounds.max_x());
        max_y = max_y.max(stacking.bounds.max_y());
      }
      if max_x > 0.0 && max_y > 0.0 {
        self.viewport = Some((max_x, max_y));
      }
    }
    let mut svg_roots = Vec::new();
    for stacking in stackings {
      Self::collect_stacking_fragments(stacking, &mut svg_roots);
    }
    self.estimate_from_roots(svg_roots.iter().copied());
    let image_cache = self.image_cache.clone();
    let mut svg_filters = SvgFilterResolver::new(
      self.svg_filter_defs.clone(),
      svg_roots,
      image_cache.as_ref(),
    );
    let visibility = self.root_visibility();
    for stacking in stackings {
      let _ = self.build_stacking_context(
        stacking,
        Point::ZERO,
        true,
        &mut svg_filters,
        visibility,
        0,
      );
    }
    self.finish()
  }

  /// Builds a display list by first constructing stacking context trees from a fragment tree.
  pub fn build_with_stacking_tree_from_tree(mut self, tree: &FragmentTree) -> DisplayList {
    if self.viewport.is_none() {
      let viewport = tree.viewport_size();
      self.viewport = Some((viewport.width, viewport.height));
    }
    let defs = tree
      .svg_filter_defs
      .clone()
      .or_else(|| self.svg_filter_defs.clone());
    self.svg_filter_defs = defs;
    self.svg_id_defs = tree
      .svg_id_defs
      .clone()
      .or_else(|| self.svg_id_defs.clone());
    self.svg_id_defs_raw = tree
      .svg_id_defs_raw
      .clone()
      .or_else(|| self.svg_id_defs_raw.clone());
    let stackings = crate::paint::stacking::build_stacking_tree_from_tree_checked_with_scroll(
      tree,
      &self.scroll_state,
    )
    .unwrap_or_else(|_| Vec::new());
    self.estimate_from_tree(tree);
    self.build_from_stacking_contexts(&stackings)
  }

  /// Builds a display list with clipping support
  ///
  /// Fragments with box_ids in the `clips` set will have clipping applied.
  pub fn build_with_clips(
    mut self,
    root: &FragmentNode,
    clips: &HashSet<Option<usize>>,
  ) -> DisplayList {
    self
      .build_with_clips_checked(root, clips)
      .unwrap_or_else(|_| DisplayList::new())
  }

  pub fn build_with_clips_checked(
    mut self,
    root: &FragmentNode,
    clips: &HashSet<Option<usize>>,
  ) -> Result<DisplayList> {
    if self.viewport.is_none() {
      self.viewport = Some((root.bounds.width(), root.bounds.height()));
    }
    self.estimate_from_roots(std::iter::once(root));
    self.build_fragment_with_clips(root, Point::ZERO, clips, self.root_visibility());
    self.finish()
  }

  fn collect_stacking_fragments<'a>(context: &'a StackingContext, out: &mut Vec<&'a FragmentNode>) {
    // Stacking contexts can nest as deeply as fragments (e.g. opacity chains). Avoid recursion so
    // hostile depth cannot overflow the call stack.
    let mut stack: Vec<&'a StackingContext> = vec![context];
    while let Some(context) = stack.pop() {
      out.extend(context.fragments.iter());
      out.extend(context.layer3_blocks.iter());
      out.extend(context.layer4_floats.iter());
      out.extend(context.layer5_inlines.iter());
      out.extend(
        context
          .layer6_positioned
          .iter()
          .map(|ordered| &ordered.fragment),
      );

      // Preserve the original recursive traversal order (preorder, children in source order).
      for child in context.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  fn estimate_from_roots<'a>(&mut self, roots: impl IntoIterator<Item = &'a FragmentNode>) {
    if self.estimated_fragments.is_some() {
      return;
    }
    let mut stack: Vec<&FragmentNode> = roots.into_iter().collect();
    let mut count = 0;
    let mut counter = 0usize;
    while let Some(fragment) = stack.pop() {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      count += 1;
      match &fragment.content {
        FragmentContent::RunningAnchor { snapshot, .. }
        | FragmentContent::FootnoteAnchor { snapshot, .. } => {
          stack.push(snapshot);
        }
        _ => {}
      }
      for child in fragment.children.iter() {
        stack.push(child);
      }
    }
    self.estimated_fragments = Some(count);
  }

  fn estimate_from_tree(&mut self, tree: &FragmentTree) {
    if self.estimated_fragments.is_some() {
      return;
    }
    self.estimated_fragments = Some(tree.fragment_count());
  }

  /// Builds display items for a fragment subtree.
  fn build_fragment(&mut self, fragment: &FragmentNode, offset: Point, visibility: Visibility) {
    self.build_fragment_internal(fragment, offset, true, false, visibility, false);
  }

  /// Builds display items for a fragment without descending into children.
  fn build_fragment_shallow(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    visibility: Visibility,
    clip_rect_already_pushed: bool,
  ) {
    self.build_fragment_internal(
      fragment,
      offset,
      false,
      false,
      visibility,
      clip_rect_already_pushed,
    );
  }

  fn build_fragment_internal(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    recurse_children: bool,
    suppress_opacity: bool,
    visibility: Visibility,
    clip_rect_already_pushed: bool,
  ) {
    // This used to recurse directly on `fragment.children`, which meant adversarially deep fragment
    // trees could stack overflow (crash) before paint-time limits/stacking-context logic had a
    // chance to run. Use an explicit stack to guarantee traversal depth is bounded by heap memory,
    // not the call stack.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stage {
      Enter,
      Blocks,
      Floats,
      SelectionOverlay,
      Inlines,
      CaretOverlay,
      Positioned,
      Exit,
    }

    struct FrameData<'a> {
      style_opt: Option<&'a ComputedStyle>,
      paint_self: bool,
      list_start: usize,
      push_opacity: bool,
      push_backface_visibility: bool,
      absolute_rect: Rect,
      skip_contents: bool,
      child_visibility: Visibility,
      pushed_clips: usize,
      root_clip_pushed: bool,

      // Child traversal state (only used when `recurse_children == true` and contents are not
      // skipped).
      child_offset: Point,
      prev_line_decoration_ctx: Option<LineDecorationContext>,
      before_children: usize,
      children_painted: bool,
      blocks: Vec<usize>,
      floats: Vec<usize>,
      inlines: Vec<usize>,
      positioned: Vec<usize>,
      child_pos: usize,
      counter: usize,
      overlays: Option<TextEditOverlays>,
    }

    struct Frame<'a> {
      stage: Stage,
      fragment: &'a FragmentNode,
      offset: Point,
      recurse_children: bool,
      suppress_opacity: bool,
      visibility: Visibility,
      clip_rect_already_pushed: bool,
      data: Option<FrameData<'a>>,
    }

    impl<'a> Frame<'a> {
      fn new(
        fragment: &'a FragmentNode,
        offset: Point,
        recurse_children: bool,
        suppress_opacity: bool,
        visibility: Visibility,
        clip_rect_already_pushed: bool,
      ) -> Self {
        Self {
          stage: Stage::Enter,
          fragment,
          offset,
          recurse_children,
          suppress_opacity,
          visibility,
          clip_rect_already_pushed,
          data: None,
        }
      }
    }

    let mut stack = Vec::new();
    stack.push(Frame::new(
      fragment,
      offset,
      recurse_children,
      suppress_opacity,
      visibility,
      clip_rect_already_pushed,
    ));

    'work: while let Some(mut frame) = stack.pop() {
      match frame.stage {
        Stage::Enter => {
          if self.deadline_reached() {
            continue;
          }

          let style_opt = frame.fragment.style.as_deref();
          if !self.list.has_scroll_linked_animations()
            && (style_opt.is_some_and(crate::paint::scroll_blit::style_uses_scroll_linked_timelines)
              || frame
                .fragment
                .starting_style
                .as_deref()
                .is_some_and(crate::paint::scroll_blit::style_uses_scroll_linked_timelines))
          {
            self.list.mark_has_scroll_linked_animations();
          }
          let paint_self = style_opt.map_or(true, |style| {
            matches!(
              style.visibility,
              crate::style::computed::Visibility::Visible
            )
          });
          if !paint_self && (!frame.recurse_children || frame.fragment.children.is_empty()) {
            continue;
          }

          if matches!(
            frame.fragment.content,
            FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
          ) {
            continue;
          }

          let list_start = self.list.len();
          let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
          if opacity <= f32::EPSILON {
            continue;
          }
          let push_opacity = !frame.suppress_opacity && opacity < 1.0 - f32::EPSILON;

          let absolute_rect = Rect::new(
            Point::new(
              frame.fragment.bounds.origin.x + frame.offset.x,
              frame.fragment.bounds.origin.y + frame.offset.y,
            ),
            frame.fragment.bounds.size,
          );
          let element_scroll = self.element_scroll_offset(frame.fragment);
          let scroll_delta = Point::new(-element_scroll.x, -element_scroll.y);
          let mut skip_contents = style_opt.is_some_and(|style| match style.content_visibility {
            ContentVisibility::Hidden => true,
            ContentVisibility::Auto => frame
              .visibility
              .rect
              .is_some_and(|vis| !vis.intersects(absolute_rect)),
            ContentVisibility::Visible => false,
          });
          let mut paint_bounds =
            self.fragment_paint_bounds(frame.fragment, absolute_rect, style_opt);
          // `fragment_paint_bounds` only accounts for effects on the fragment's own border box.
          // Descendants can paint outside that box (e.g. absolutely positioned children with
          // `overflow: visible`), so use the already-computed `scroll_overflow` to avoid culling away
          // visible descendants.
          paint_bounds = paint_bounds.union(
            frame
              .fragment
              .scroll_overflow
              .translate(absolute_rect.origin),
          );
          if let Some(vis) = frame.visibility.rect {
            if !vis.intersects(paint_bounds) {
              continue;
            }
            if frame.visibility.hard_clip {
              paint_bounds = match paint_bounds.intersection(vis) {
                Some(intersection)
                  if intersection.width() > 0.0 && intersection.height() > 0.0 =>
                {
                  intersection
                }
                _ => continue,
              };
            } else {
              paint_bounds = paint_bounds.intersection(vis).unwrap_or(paint_bounds);
            }
          }

          let mut absolute_rects: Option<BackgroundRects> = None;
          let (overflow_clip, clip_rect) = if let Some(style) = style_opt {
            // Replaced elements clip their own contents to the content box (see `replaced_content_clip_item`),
            // so avoid applying the generic padding-box overflow clip here. (Form controls compute their
            // own overflow clip in the replaced paint path.)
            let is_replaced = matches!(&frame.fragment.content, FragmentContent::Replaced { .. });
            let clip_x = !is_replaced && Self::overflow_axis_clips(style.overflow_x);
            let clip_y = !is_replaced && Self::overflow_axis_clips(style.overflow_y);
            (
              if clip_x || clip_y {
                let overflow_bounds = absolute_rect.union(
                  frame
                    .fragment
                    .scroll_overflow
                    .translate(absolute_rect.origin),
                );
                let rects = absolute_rects.get_or_insert_with(|| {
                  Self::background_rects(absolute_rect, style, self.viewport)
                });
                // `overflow: hidden` clips to the padding box. When the padding box has no area in a
                // clipped axis (e.g. `width: 0; overflow: hidden`), no descendant pixels can be visible,
                // so skip content painting rather than treating this as "no clip".
                if (clip_x && rects.padding.width() <= 0.0)
                  || (clip_y && rects.padding.height() <= 0.0)
                {
                  skip_contents = true;
                  None
                } else {
                  Self::overflow_clip_from_style_with_rects(
                    style,
                    rects,
                    clip_x,
                    clip_y,
                    overflow_bounds,
                    self.viewport,
                    self.build_breakdown.as_deref(),
                  )
                }
              } else {
                None
              },
              Self::clip_rect_from_style(style, absolute_rect, self.viewport),
            )
          } else {
            (None, None)
          };
          let mut child_visibility = frame.visibility;
          if let Some(clip) = overflow_clip.as_ref() {
            child_visibility = child_visibility.intersect(Self::clip_bounds(clip), true);
          }
          if let Some(clip) = clip_rect.as_ref() {
            child_visibility = child_visibility.intersect(Self::clip_bounds(clip), true);
          }
          if child_visibility.rect.is_none() && frame.visibility.rect.is_some() {
            continue;
          }
          if let Some(vis) = child_visibility.rect {
            if !vis.intersects(paint_bounds) {
              continue;
            }
            if child_visibility.hard_clip {
              match paint_bounds.intersection(vis) {
                Some(intersection)
                  if intersection.width() > 0.0 && intersection.height() > 0.0 => {}
                _ => continue,
              };
            }
          }

          // `backface-visibility: hidden` establishes a stacking context. In the stacking-context-aware
          // pipeline, the value is carried on `StackingContextItem` and culled by the renderer at
          // `PushStackingContext`.
          //
          // When painting without stacking contexts, we still need to cull the element when an ancestor
          // 3D transform flips it away from the viewer, so wrap only elements that would otherwise
          // *not* create a stacking context.
          let push_backface_visibility = style_opt.is_some_and(|style| {
            matches!(style.backface_visibility, BackfaceVisibility::Hidden)
              && !crate::paint::stacking::creates_stacking_context(style, None, false)
          });
          if push_backface_visibility {
            self.list.push(DisplayItem::PushBackfaceVisibility(
              BackfaceVisibility::Hidden,
            ));
          }

          if push_opacity {
            self.push_opacity(opacity);
          }

          // CSS 2.1 `clip` applies to the element's entire rendering (background/border/shadow,
          // contents, and descendants).
          //
          // In the stacking-context-aware path, the stacking context builder pushes `clip_rect`
          // immediately after `PushStackingContext`, so avoid double-pushing here.
          let mut root_clip_pushed = false;
          if !frame.clip_rect_already_pushed {
            if let Some(clip) = clip_rect.as_ref() {
              self.list.push(DisplayItem::PushClip(clip.clone()));
              root_clip_pushed = true;
            }
          }

          if paint_self {
            if let Some(style) = style_opt {
              let suppress_background = self
                .canvas_background_suppress_box_id
                .is_some_and(|id| Self::get_box_id(frame.fragment) == Some(id));
              let suppress_border = self
                .canvas_border_suppress_box_id
                .is_some_and(|id| Self::get_box_id(frame.fragment) == Some(id));
              let (decoration_rect, decoration_clip) =
                Self::decoration_rect_and_clip(frame.fragment, absolute_rect, style);
              let decoration_clip_pushed = decoration_clip.is_some();
              if let Some(clip) = decoration_clip {
                self.list.push(DisplayItem::PushClip(clip));
              }

              let text_clip = if !skip_contents
                && !suppress_background
                && Self::has_paintable_background(style)
                && style
                  .background_layers
                  .iter()
                  .any(|layer| layer.clip == BackgroundBox::Text)
              {
                self.build_text_clip_runs(frame.fragment, frame.offset, child_visibility)
              } else {
                None
              };

              if !style.box_shadow.is_empty() {
                let mut decoration_rects: Option<BackgroundRects> = None;
                let rects = if decoration_rect == absolute_rect {
                  absolute_rects.get_or_insert_with(|| {
                    Self::background_rects(absolute_rect, style, self.viewport)
                  })
                } else {
                  decoration_rects.get_or_insert_with(|| {
                    Self::background_rects(decoration_rect, style, self.viewport)
                  })
                };

                let has_outer = style.box_shadow.iter().any(|shadow| !shadow.inset);
                let has_inset = style.box_shadow.iter().any(|shadow| shadow.inset);
                let outer_radii = if has_outer {
                  Self::border_radii(decoration_rect, style)
                    .clamped(decoration_rect.width(), decoration_rect.height())
                } else {
                  crate::paint::display_list::BorderRadii::ZERO
                };
                let inner_radii = if has_inset {
                  if Self::border_radius_is_zero(style) {
                    crate::paint::display_list::BorderRadii::ZERO
                  } else {
                    Self::resolve_clip_radii(
                      style,
                      rects,
                      BackgroundBox::PaddingBox,
                      self.viewport,
                      self.build_breakdown.as_deref(),
                    )
                  }
                } else {
                  crate::paint::display_list::BorderRadii::ZERO
                };

                if has_outer {
                  self.emit_box_shadows_from_style_with_base(
                    rects.border,
                    outer_radii,
                    decoration_rect.width(),
                    style,
                    false,
                  );
                }
                if !suppress_background {
                  self.emit_background_from_style_with_rects_and_text_clip_and_culling_rect(
                    rects,
                    style,
                    text_clip.as_ref(),
                    frame.visibility.rect,
                    scroll_delta,
                  );
                }
                if has_inset {
                  self.emit_box_shadows_from_style_with_base(
                    rects.padding,
                    inner_radii,
                    decoration_rect.width(),
                    style,
                    true,
                  );
                }
              } else if decoration_rect == absolute_rect {
                if !suppress_background {
                  if let Some(rects) = absolute_rects.as_ref() {
                    self.emit_background_from_style_with_rects_and_text_clip_and_culling_rect(
                      rects,
                      style,
                      text_clip.as_ref(),
                      frame.visibility.rect,
                      scroll_delta,
                    );
                  } else {
                    self.emit_background_from_style_with_text_clip_and_culling_rect(
                      decoration_rect,
                      style,
                      text_clip.as_ref(),
                      frame.visibility.rect,
                      scroll_delta,
                    );
                  }
                }
              } else {
                if !suppress_background {
                  self.emit_background_from_style_with_text_clip_and_culling_rect(
                    decoration_rect,
                    style,
                    text_clip.as_ref(),
                    frame.visibility.rect,
                    scroll_delta,
                  );
                }
              }

              if !suppress_border {
                let gap =
                  self.fieldset_legend_border_gap(frame.fragment, decoration_rect, style);
                self.emit_border_from_style(decoration_rect, style, gap);
              }
              if decoration_clip_pushed {
                self.list.push(DisplayItem::PopClip);
              }
            }
          }

          // CSS Paint Order:
          // 1. Background (handled by caller if style available)
          // 2. Border (handled by caller if style available)
          // 3. Content (text, images)
          // 4. Children

          // Clip descendant/content painting to the padding box when `overflow` clips.
          let mut pushed_clips = 0usize;
          if !skip_contents {
            if let Some(clip) = overflow_clip {
              self.list.push(DisplayItem::PushClip(clip));
              pushed_clips += 1;
            }

            if paint_self {
              self.emit_content(frame.fragment, absolute_rect, child_visibility.rect);
            }
          }

          let mut data = FrameData {
            style_opt,
            paint_self,
            list_start,
            push_opacity,
            push_backface_visibility,
            absolute_rect,
            skip_contents,
            child_visibility,
            pushed_clips,
            root_clip_pushed,
            child_offset: Point::ZERO,
            prev_line_decoration_ctx: None,
            before_children: 0,
            children_painted: false,
            blocks: Vec::new(),
            floats: Vec::new(),
            inlines: Vec::new(),
            positioned: Vec::new(),
            child_pos: 0,
            counter: 0,
            overlays: None,
          };

          if frame.recurse_children && !skip_contents {
            let child_offset = Point::new(
              absolute_rect.origin.x - element_scroll.x,
              absolute_rect.origin.y - element_scroll.y,
            );

            let prev_line_decoration_ctx = self.line_decoration_ctx;
            if let FragmentContent::Line { baseline } = &frame.fragment.content {
              self.line_decoration_ctx =
                Some(self.build_line_decoration_context(frame.fragment, child_offset, *baseline));
            } else if self.line_decoration_ctx.is_some()
              && style_opt.is_some_and(|style| {
                style.display.is_inline_level() && style.display.establishes_formatting_context()
              })
            {
              // Decorations inside atomic inlines (inline-block/inline-table/etc) should be resolved
              // against the atomic inline's own line boxes, not the ancestor line that positioned it.
              self.line_decoration_ctx = None;
            }

            let before_children = self.list.len();
            let mut blocks = Vec::new();
            let mut floats = Vec::new();
            let mut inlines = Vec::new();
            let mut positioned = Vec::new();
            for (idx, child) in frame.fragment.children.iter().enumerate() {
              if self.skip_stacking_context_children {
                if let Some(child_style) = child.style.as_deref() {
                  if crate::paint::stacking::creates_stacking_context(child_style, style_opt, false)
                  {
                    continue;
                  }
                  if !matches!(child_style.position, Position::Static)
                    && !crate::paint::stacking::creates_stacking_context(child_style, None, false)
                  {
                    continue;
                  }
                }
              }

              match child.style.as_deref() {
                Some(child_style) if !matches!(child_style.position, Position::Static) => {
                  positioned.push(idx);
                }
                Some(child_style) if child_style.float.is_floating() => floats.push(idx),
                Some(child_style)
                  if matches!(
                    child_style.display,
                    crate::style::display::Display::Inline
                      | crate::style::display::Display::InlineBlock
                      | crate::style::display::Display::InlineFlex
                      | crate::style::display::Display::InlineGrid
                      | crate::style::display::Display::InlineTable
                  ) =>
                {
                  inlines.push(idx);
                }
                Some(_) => blocks.push(idx),
                None => match child.content {
                  FragmentContent::Text { .. }
                  | FragmentContent::Inline { .. }
                  | FragmentContent::Line { .. } => inlines.push(idx),
                  _ => blocks.push(idx),
                },
              }
            }

            let appearance_none_control = Self::get_box_id(frame.fragment).and_then(|box_id| {
              self
                .appearance_none_form_controls
                .as_ref()
                .and_then(|map| map.get(&box_id))
                .cloned()
            });
            let overlays = match (appearance_none_control.as_deref(), style_opt) {
              (Some(control), Some(style)) => self.appearance_none_text_edit_overlays(
                control,
                style,
                absolute_rect,
                scroll_delta,
              ),
              _ => None,
            };

            data.child_offset = child_offset;
            data.prev_line_decoration_ctx = prev_line_decoration_ctx;
            data.before_children = before_children;
            data.blocks = blocks;
            data.floats = floats;
            data.inlines = inlines;
            data.positioned = positioned;
            data.overlays = overlays;

            frame.data = Some(data);
            frame.stage = Stage::Blocks;
            stack.push(frame);
            continue 'work;
          }

          frame.data = Some(data);
          frame.stage = Stage::Exit;
          stack.push(frame);
        }
        Stage::Blocks => {
            let (child_idx, child_offset, child_visibility) = {
              let data = frame
                .data
                .as_mut()
                .expect("entered fragment frame missing data"); // fastrender-allow-unwrap
              loop {
                if data.child_pos >= data.blocks.len() {
                  break (None, Point::ZERO, Visibility::none());
                }
              if self.deadline_reached_periodic(&mut data.counter, DEADLINE_STRIDE) {
                data.child_pos = data.blocks.len();
                break (None, Point::ZERO, Visibility::none());
              }
              let idx = data.blocks[data.child_pos];
              data.child_pos += 1;
              break (Some(idx), data.child_offset, data.child_visibility);
            }
          };

          if let Some(idx) = child_idx {
            let child = &frame.fragment.children[idx];
            let child_frame = Frame::new(child, child_offset, true, false, child_visibility, false);
            stack.push(frame);
            stack.push(child_frame);
            continue 'work;
          }

          if let Some(data) = frame.data.as_mut() {
            data.child_pos = 0;
          }
          frame.stage = Stage::Floats;
          stack.push(frame);
        }
        Stage::Floats => {
            let (child_idx, child_offset, child_visibility) = {
              let data = frame
                .data
                .as_mut()
                .expect("entered fragment frame missing data"); // fastrender-allow-unwrap
              loop {
                if data.child_pos >= data.floats.len() {
                  break (None, Point::ZERO, Visibility::none());
                }
              if self.deadline_reached_periodic(&mut data.counter, DEADLINE_STRIDE) {
                data.child_pos = data.floats.len();
                break (None, Point::ZERO, Visibility::none());
              }
              let idx = data.floats[data.child_pos];
              data.child_pos += 1;
              break (Some(idx), data.child_offset, data.child_visibility);
            }
          };

          if let Some(idx) = child_idx {
            let child = &frame.fragment.children[idx];
            let child_frame = Frame::new(child, child_offset, true, false, child_visibility, false);
            stack.push(frame);
            stack.push(child_frame);
            continue 'work;
          }

          if let Some(data) = frame.data.as_mut() {
            data.child_pos = 0;
          }
          frame.stage = Stage::SelectionOverlay;
          stack.push(frame);
        }
        Stage::SelectionOverlay => {
          let selection_color = Rgba {
            r: 0,
            g: 120,
            b: 215,
            a: 0.35,
          };
          if let Some(data) = frame.data.as_ref() {
            if let Some(overlays) = data.overlays.as_ref() {
              for rect in &overlays.selection_rects {
                self.list.push(DisplayItem::FillRect(FillRectItem {
                  rect: *rect,
                  color: selection_color,
                }));
              }
            }
          }
          if let Some(data) = frame.data.as_mut() {
            data.child_pos = 0;
          }
          frame.stage = Stage::Inlines;
          stack.push(frame);
        }
        Stage::Inlines => {
            let (child_idx, child_offset, child_visibility) = {
              let data = frame
                .data
                .as_mut()
                .expect("entered fragment frame missing data"); // fastrender-allow-unwrap
              loop {
                if data.child_pos >= data.inlines.len() {
                  break (None, Point::ZERO, Visibility::none());
                }
              if self.deadline_reached_periodic(&mut data.counter, DEADLINE_STRIDE) {
                data.child_pos = data.inlines.len();
                break (None, Point::ZERO, Visibility::none());
              }
              let idx = data.inlines[data.child_pos];
              data.child_pos += 1;
              break (Some(idx), data.child_offset, data.child_visibility);
            }
          };

          if let Some(idx) = child_idx {
            let child = &frame.fragment.children[idx];
            let child_frame = Frame::new(child, child_offset, true, false, child_visibility, false);
            stack.push(frame);
            stack.push(child_frame);
            continue 'work;
          }

          if let Some(data) = frame.data.as_mut() {
            data.child_pos = 0;
          }
          frame.stage = Stage::CaretOverlay;
          stack.push(frame);
        }
        Stage::CaretOverlay => {
          if let Some(data) = frame.data.as_ref() {
            if let Some(overlays) = data.overlays.as_ref() {
              if let Some((rect, color)) = overlays.caret_rect {
                self.list.push(DisplayItem::FillRect(FillRectItem { rect, color }));
              }
            }
          }
          if let Some(data) = frame.data.as_mut() {
            data.child_pos = 0;
          }
          frame.stage = Stage::Positioned;
          stack.push(frame);
        }
        Stage::Positioned => {
            let (child_idx, child_offset, child_visibility) = {
              let data = frame
                .data
                .as_mut()
                .expect("entered fragment frame missing data"); // fastrender-allow-unwrap
              loop {
                if data.child_pos >= data.positioned.len() {
                  break (None, Point::ZERO, Visibility::none());
                }
              if self.deadline_reached_periodic(&mut data.counter, DEADLINE_STRIDE) {
                data.child_pos = data.positioned.len();
                break (None, Point::ZERO, Visibility::none());
              }
              let idx = data.positioned[data.child_pos];
              data.child_pos += 1;
              break (Some(idx), data.child_offset, data.child_visibility);
            }
          };

          if let Some(idx) = child_idx {
            let child = &frame.fragment.children[idx];
            let child_frame = Frame::new(child, child_offset, true, false, child_visibility, false);
            stack.push(frame);
            stack.push(child_frame);
            continue 'work;
          }

          let prev_line_decoration_ctx = {
            let data = frame
              .data
              .as_mut()
              .expect("entered fragment frame missing data"); // fastrender-allow-unwrap
            data.children_painted = self.list.len() != data.before_children;
            data.prev_line_decoration_ctx
          };
          self.line_decoration_ctx = prev_line_decoration_ctx;
          frame.stage = Stage::Exit;
          stack.push(frame);
        }
        Stage::Exit => {
          let Some(data) = frame.data.take() else {
            continue;
          };

          if !data.skip_contents {
            if let Some(table_borders) = frame.fragment.table_borders.as_ref() {
              let mut origin = data.absolute_rect.origin;
              let mut clip_to_slice = false;
              if let Some(style) = data.style_opt {
                if matches!(style.box_decoration_break, BoxDecorationBreak::Slice) {
                  let info = frame.fragment.slice_info;
                  if !(info.is_first && info.is_last) {
                    clip_to_slice = true;
                    if !table_borders.fragment_local {
                      let original_block_size = info.original_block_size.max(0.0);
                      let slice_offset = info.slice_offset.clamp(0.0, original_block_size);
                      if block_axis_is_horizontal(style.writing_mode) {
                        if block_axis_positive(style.writing_mode) {
                          origin.x -= slice_offset;
                        } else {
                          origin.x = origin.x + slice_offset - original_block_size;
                        }
                      } else {
                        origin.y -= slice_offset;
                      }
                    }
                  }
                }
              }

              // `TableCollapsedBorders.paint_bounds` is in table-fragment-local coordinates and
              // can extend outside the table fragment's own `bounds` (including into negative
              // coordinates) when a thicker winning *outer-edge* border segment spills outward
              // beyond the baseline widths used for layout (CSS 2.1 §17.6.2; WPT
              // `border-collapse-basic-001`).
              //
              // Do not clamp this to the fragment rect: the bounds are used for culling/tiling and
              // must include any outward spill to avoid clipping thick outer-edge winners.
              // See comment in `build_fragment` above: collapsed table border bounds can extend
              // outside the table fragment rect (including into negative coordinates) and must not
              // be clamped, otherwise thick outer-edge winners can be clipped.
              let bounds = table_borders.paint_bounds.translate(origin);
              let has_visible_borders = table_borders
                .vertical_borders
                .iter()
                .chain(table_borders.horizontal_borders.iter())
                .chain(table_borders.corner_borders.iter())
                .any(|b| b.is_visible());
              if data.paint_self && has_visible_borders {
                if clip_to_slice {
                  self.list.push(DisplayItem::PushClip(ClipItem {
                    shape: ClipShape::Rect {
                      rect: data.absolute_rect,
                      radii: None,
                    },
                  }));
                }
                self.list.push(DisplayItem::TableCollapsedBorders(
                  TableCollapsedBordersItem {
                    origin,
                    bounds,
                    borders: table_borders.clone(),
                  },
                ));
                if clip_to_slice {
                  self.list.push(DisplayItem::PopClip);
                }
              }
            }

            for _ in 0..data.pushed_clips {
              self.list.push(DisplayItem::PopClip);
            }
          }

          if data.root_clip_pushed {
            self.list.push(DisplayItem::PopClip);
          }

          if data.paint_self {
            if let Some(style) = data.style_opt {
              self.emit_outline(data.absolute_rect, style);
            }
          }

          if data.push_opacity {
            self.pop_opacity();
          }

          if data.push_backface_visibility {
            self.list.push(DisplayItem::PopBackfaceVisibility);
          }

          if !data.paint_self && !data.children_painted {
            self.list.items_mut().truncate(data.list_start);
          }
        }
      }
    }
  }

  /// Builds display items with clipping support.
  ///
  /// This traversal is implemented iteratively to avoid stack overflow on adversarially deep
  /// fragment trees (e.g. when used by selection/highlight paths that need custom clipping).
  fn build_fragment_with_clips(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    clips: &HashSet<Option<usize>>,
    visibility: Visibility,
  ) {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stage {
      Enter,
      Children,
      Exit,
    }

    struct FrameData<'a> {
      style_opt: Option<&'a ComputedStyle>,
      paint_self: bool,
      list_start: usize,
      push_opacity: bool,
      push_backface_visibility: bool,
      absolute_rect: Rect,
      skip_contents: bool,
      did_push_clip: bool,
      child_offset: Point,
      prev_line_decoration_ctx: Option<LineDecorationContext>,
      before_children: usize,
      ordered_children: Vec<usize>,
      child_pos: usize,
      counter: usize,
    }

    struct Frame<'a> {
      stage: Stage,
      fragment: &'a FragmentNode,
      offset: Point,
      visibility: Visibility,
      data: Option<FrameData<'a>>,
    }

    let mut stack: Vec<Frame<'_>> = Vec::new();
    stack.push(Frame {
      stage: Stage::Enter,
      fragment,
      offset,
      visibility,
      data: None,
    });

    'work: while let Some(mut frame) = stack.pop() {
      match frame.stage {
        Stage::Enter => {
          if self.deadline_reached() {
            continue;
          }

          let style_opt = frame.fragment.style.as_deref();
          let paint_self = style_opt.map_or(true, |style| {
            matches!(
              style.visibility,
              crate::style::computed::Visibility::Visible
            )
          });
          if !paint_self && frame.fragment.children.is_empty() {
            continue;
          }

          if matches!(
            frame.fragment.content,
            FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
          ) {
            continue;
          }

          let list_start = self.list.len();
          let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
          let push_opacity = opacity < 1.0 - f32::EPSILON;

          let absolute_rect = Rect::new(
            Point::new(
              frame.fragment.bounds.origin.x + frame.offset.x,
              frame.fragment.bounds.origin.y + frame.offset.y,
            ),
            frame.fragment.bounds.size,
          );
          let element_scroll = self.element_scroll_offset(frame.fragment);
          let scroll_delta = Point::new(-element_scroll.x, -element_scroll.y);
          if let Some(vis) = frame.visibility.rect {
            if !vis.intersects(absolute_rect) {
              continue;
            }
          }
          let skip_contents =
            frame
              .fragment
              .style
              .as_deref()
              .is_some_and(|style| match style.content_visibility {
                ContentVisibility::Hidden => true,
                ContentVisibility::Auto => frame
                  .visibility
                  .rect
                  .is_some_and(|vis| !vis.intersects(absolute_rect)),
                ContentVisibility::Visible => false,
              });

          let push_backface_visibility = style_opt.is_some_and(|style| {
            matches!(style.backface_visibility, BackfaceVisibility::Hidden)
              && !crate::paint::stacking::creates_stacking_context(style, None, false)
          });
          if push_backface_visibility {
            self.list.push(DisplayItem::PushBackfaceVisibility(
              BackfaceVisibility::Hidden,
            ));
          }
          if push_opacity {
            self.push_opacity(opacity);
          }

          if paint_self {
            if let Some(style) = style_opt {
              let (decoration_rect, decoration_clip) =
                Self::decoration_rect_and_clip(frame.fragment, absolute_rect, style);
              let decoration_clip_pushed = decoration_clip.is_some();
              if let Some(clip) = decoration_clip {
                self.list.push(DisplayItem::PushClip(clip));
              }
              self.emit_background_from_style_with_culling_rect(
                decoration_rect,
                style,
                frame.visibility.rect,
                scroll_delta,
              );
              let gap = self.fieldset_legend_border_gap(frame.fragment, decoration_rect, style);
              self.emit_border_from_style(decoration_rect, style, gap);
              if decoration_clip_pushed {
                self.list.push(DisplayItem::PopClip);
              }
            }
          }

          let box_id = Self::get_box_id(frame.fragment);
          let should_clip = clips.contains(&box_id);

          let mut data = FrameData {
            style_opt,
            paint_self,
            list_start,
            push_opacity,
            push_backface_visibility,
            absolute_rect,
            skip_contents,
            did_push_clip: false,
            child_offset: Point::ZERO,
            prev_line_decoration_ctx: None,
            before_children: 0,
            ordered_children: Vec::new(),
            child_pos: 0,
            counter: 0,
          };

          if !skip_contents {
            // Emit content before clipping children.
            if paint_self {
              self.emit_content(frame.fragment, absolute_rect, frame.visibility.rect);
            }

            // Push clip if needed.
            if should_clip {
              self.list.push(DisplayItem::PushClip(ClipItem {
                shape: ClipShape::Rect {
                  rect: absolute_rect,
                  radii: None,
                },
              }));
              data.did_push_clip = true;
            }

            let child_offset = Point::new(
              absolute_rect.origin.x - element_scroll.x,
              absolute_rect.origin.y - element_scroll.y,
            );
            let prev_line_decoration_ctx = self.line_decoration_ctx;
            if let FragmentContent::Line { baseline } = &frame.fragment.content {
              self.line_decoration_ctx = Some(self.build_line_decoration_context(
                frame.fragment,
                child_offset,
                *baseline,
              ));
            } else if self.line_decoration_ctx.is_some()
              && style_opt.is_some_and(|style| {
                style.display.is_inline_level() && style.display.establishes_formatting_context()
              })
            {
              self.line_decoration_ctx = None;
            }

            let before_children = self.list.len();

            let mut blocks = Vec::new();
            let mut floats = Vec::new();
            let mut inlines = Vec::new();
            let mut positioned = Vec::new();
            for (idx, child) in frame.fragment.children.iter().enumerate() {
              match child.style.as_deref() {
                Some(child_style) if !matches!(child_style.position, Position::Static) => {
                  positioned.push(idx);
                }
                Some(child_style) if child_style.float.is_floating() => floats.push(idx),
                Some(child_style)
                  if matches!(
                    child_style.display,
                    crate::style::display::Display::Inline
                      | crate::style::display::Display::InlineBlock
                      | crate::style::display::Display::InlineFlex
                      | crate::style::display::Display::InlineGrid
                      | crate::style::display::Display::InlineTable
                  ) =>
                {
                  inlines.push(idx);
                }
                Some(_) => blocks.push(idx),
                None => match child.content {
                  FragmentContent::Text { .. }
                  | FragmentContent::Inline { .. }
                  | FragmentContent::Line { .. } => inlines.push(idx),
                  _ => blocks.push(idx),
                },
              }
            }

            // CSS2.1 stacking levels 3-6: blocks → floats → inlines → positioned.
            let mut ordered_children = blocks;
            ordered_children.extend(floats);
            ordered_children.extend(inlines);
            ordered_children.extend(positioned);

            data.child_offset = child_offset;
            data.prev_line_decoration_ctx = prev_line_decoration_ctx;
            data.before_children = before_children;
            data.ordered_children = ordered_children;

            frame.data = Some(data);
            frame.stage = Stage::Children;
            stack.push(frame);
            continue 'work;
          }

          frame.data = Some(data);
          frame.stage = Stage::Exit;
          stack.push(frame);
        }
        Stage::Children => {
          let (child_opt, child_offset, visibility) = {
            let data = frame.data.as_mut().expect("missing clip frame data"); // fastrender-allow-unwrap
            if data.child_pos >= data.ordered_children.len() {
              (None, Point::ZERO, Visibility::none())
            } else if self.deadline_reached_periodic(&mut data.counter, DEADLINE_STRIDE) {
              data.child_pos = data.ordered_children.len();
              (None, Point::ZERO, Visibility::none())
            } else {
              let idx = data.ordered_children[data.child_pos];
              data.child_pos += 1;
              (
                Some(&frame.fragment.children[idx]),
                data.child_offset,
                frame.visibility,
              )
            }
          };

          if let Some(child) = child_opt {
            stack.push(frame);
            stack.push(Frame {
              stage: Stage::Enter,
              fragment: child,
              offset: child_offset,
              visibility,
              data: None,
            });
            continue 'work;
          }

          frame.stage = Stage::Exit;
          stack.push(frame);
        }
        Stage::Exit => {
          let data = frame.data.take().expect("missing clip frame data"); // fastrender-allow-unwrap
          let mut children_painted = false;

          if !data.skip_contents {
            children_painted = self.list.len() != data.before_children;
            self.line_decoration_ctx = data.prev_line_decoration_ctx;

            if let Some(table_borders) = frame.fragment.table_borders.as_ref() {
              let mut origin = data.absolute_rect.origin;
              let mut clip_to_slice = false;
              if let Some(style) = data.style_opt {
                if matches!(style.box_decoration_break, BoxDecorationBreak::Slice) {
                  let info = frame.fragment.slice_info;
                  if !(info.is_first && info.is_last) {
                    clip_to_slice = true;
                    if !table_borders.fragment_local {
                      let original_block_size = info.original_block_size.max(0.0);
                      let slice_offset = info.slice_offset.clamp(0.0, original_block_size);
                      if block_axis_is_horizontal(style.writing_mode) {
                        if block_axis_positive(style.writing_mode) {
                          origin.x -= slice_offset;
                        } else {
                          origin.x = origin.x + slice_offset - original_block_size;
                        }
                      } else {
                        origin.y -= slice_offset;
                      }
                    }
                  }
                }
              }

              let bounds = table_borders.paint_bounds.translate(origin);
              let has_visible_borders = table_borders
                .vertical_borders
                .iter()
                .chain(table_borders.horizontal_borders.iter())
                .chain(table_borders.corner_borders.iter())
                .any(|b| b.is_visible());
              if data.paint_self && has_visible_borders {
                if clip_to_slice {
                  self.list.push(DisplayItem::PushClip(ClipItem {
                    shape: ClipShape::Rect {
                      rect: data.absolute_rect,
                      radii: None,
                    },
                  }));
                }
                self.list.push(DisplayItem::TableCollapsedBorders(
                  TableCollapsedBordersItem {
                    origin,
                    bounds,
                    borders: table_borders.clone(),
                  },
                ));
                if clip_to_slice {
                  self.list.push(DisplayItem::PopClip);
                }
              }
            }

            if data.did_push_clip {
              self.list.push(DisplayItem::PopClip);
            }
          }

          if data.paint_self {
            if let Some(style) = data.style_opt {
              self.emit_outline(data.absolute_rect, style);
            }
          }

          if data.push_opacity {
            self.pop_opacity();
          }

          if data.push_backface_visibility {
            self.list.push(DisplayItem::PopBackfaceVisibility);
          }

          if !data.paint_self && !children_painted {
            self.list.items_mut().truncate(data.list_start);
          }
        }
      }
    }
  }

  fn push_clip_chain(&mut self, chain: &[ClipChainLink], offset: Point) -> usize {
    if chain.is_empty() {
      return 0;
    }

    let mut pushed = 0usize;
    for link in chain {
      let rect = link.rect.translate(offset);
      let style = link.style.as_ref();
      let clip_x = !link.is_replaced && Self::overflow_axis_clips(style.overflow_x);
      let clip_y = !link.is_replaced && Self::overflow_axis_clips(style.overflow_y);
      if clip_x || clip_y {
        let overflow_bounds = rect.union(link.scroll_overflow.translate(rect.origin));
        let rects = Self::background_rects(rect, style, self.viewport);
        if let Some(clip) = Self::overflow_clip_from_style_with_rects(
          style,
          &rects,
          clip_x,
          clip_y,
          overflow_bounds,
          self.viewport,
          self.build_breakdown.as_deref(),
        ) {
          self.list.push(DisplayItem::PushClip(clip));
          pushed += 1;
        }
      }

      if let Some(clip) = Self::clip_rect_from_style(style, rect, self.viewport) {
        self.list.push(DisplayItem::PushClip(clip));
        pushed += 1;
      }
    }

    pushed
  }

  fn pop_clips(&mut self, count: usize) {
    for _ in 0..count {
      self.list.push(DisplayItem::PopClip);
    }
  }

  #[inline]
  fn push_backface_visibility_chain(&mut self, depth: usize) -> usize {
    if depth == 0 {
      return 0;
    }
    for _ in 0..depth {
      self.list.push(DisplayItem::PushBackfaceVisibility(
        BackfaceVisibility::Hidden,
      ));
    }
    depth
  }

  #[inline]
  fn pop_backface_visibility_chain(&mut self, depth: usize) {
    for _ in 0..depth {
      self.list.push(DisplayItem::PopBackfaceVisibility);
    }
  }

  /// Computes whether a stacking context should be rendered as an *isolated group*.
  ///
  /// This corresponds to [`StackingContextItem::is_isolated`] (CSS Compositing & Blending isolated
  /// groups), and is **not** the same thing as a Filter Effects Level 2 Backdrop Root.
  ///
  /// Rules of thumb:
  /// - Set for explicit `isolation:isolate`.
  /// - Also set for `backdrop-filter` (it needs an intermediate surface) and when we need to
  ///   confine blend-mode descendants to this stacking context.
  /// - Non-`normal` `mix-blend-mode` does **not** imply an isolated group; it only requires a
  ///   compositing group surface (which may be non-isolated).
  ///
  /// Spec: <https://www.w3.org/TR/compositing-1/#isolatedgroups>
  fn stacking_context_is_isolated(
    mix_blend_mode: BlendMode,
    root_style: Option<&ComputedStyle>,
    has_blend_mode_children: bool,
  ) -> bool {
    let style_isolated = root_style
      .map(|style| {
        matches!(style.isolation, Isolation::Isolate) || !style.backdrop_filter.is_empty()
      })
      .unwrap_or(false);

    if mix_blend_mode != BlendMode::Normal {
      style_isolated
    } else {
      style_isolated || has_blend_mode_children
    }
  }

  /// Computes whether a stacking context establishes a Filter Effects Level 2 *Backdrop Root*.
  ///
  /// This corresponds to [`StackingContextItem::establishes_backdrop_root`] and is used to scope
  /// descendant `backdrop-filter` sampling (Backdrop Root Image). It does **not** imply the
  /// subtree is composited as an isolated group (see [`Self::stacking_context_is_isolated`]).
  ///
  /// Rules of thumb:
  /// - Set for the root element.
  /// - Set for Backdrop Root triggers like `filter`, `backdrop-filter`, `opacity < 1`,
  ///   `mask`/`clip-path`, and non-`normal` `mix-blend-mode`.
  /// - Also set for `will-change` hints of the above (so the renderer can allocate boundaries
  ///   proactively).
  ///
  /// Spec: <https://drafts.fxtf.org/filter-effects-2/#BackdropRoot>
  fn stacking_context_establishes_backdrop_root(
    is_root: bool,
    root_style: Option<&ComputedStyle>,
    mix_blend_mode: BlendMode,
    filters: &[ResolvedFilter],
    backdrop_filters: &[ResolvedFilter],
    has_opacity: bool,
    has_mask: bool,
    has_clip_path: bool,
  ) -> bool {
    // Backdrop Root triggers are based on property *presence*, not resolved effect lists.
    //
    // The filter/mask/clip-path resolvers may optimize away no-op effects like `blur(0px)` or
    // `clip-path: inset(0)`. Per Filter Effects Level 2 those still establish Backdrop Roots for
    // descendant `backdrop-filter` sampling and `mix-blend-mode` blending.
    let style_has_filter = root_style.is_some_and(|style| !style.filter.is_empty());
    let style_has_backdrop_filter =
      root_style.is_some_and(|style| !style.backdrop_filter.is_empty());
    let style_has_mask_image =
      root_style.is_some_and(|style| style.mask_layers.iter().any(|layer| layer.image.is_some()));
    let style_has_mask_border = root_style.is_some_and(|style| style.mask_border.is_active());

    is_root
      || style_has_filter
      || !filters.is_empty()
      || has_opacity
      || style_has_mask_image
      || style_has_mask_border
      || has_mask
      || has_clip_path
      || style_has_backdrop_filter
      || !backdrop_filters.is_empty()
      || mix_blend_mode != BlendMode::Normal
      || root_style.is_some_and(|style| style.will_change.establishes_backdrop_root())
  }

  fn build_stacking_context(
    &mut self,
    context: &StackingContext,
    offset: Point,
    is_root: bool,
    svg_filters: &mut SvgFilterResolver,
    visibility: Visibility,
    depth: usize,
  ) -> bool {
    if depth >= MAX_STACKING_CONTEXT_DEPTH {
      // Stacking-context painting is still recursive. Bail out early on hostile nesting rather than
      // risking a process-wide stack overflow.
      if self.error.is_none() {
        self.error = Some(RenderError::PaintFailed {
          operation: format!(
            "stacking context nesting too deep (limit {MAX_STACKING_CONTEXT_DEPTH})"
          ),
        });
      }
      return false;
    }
    if self.deadline_reached() {
      return false;
    }
    // Children are already sorted by (z_index, tree_order) by the stacking tree builder.
    // Split into negative and positive z-index slices; z-index==0 contexts are handled by
    // layer 6 so they are not double-painted here.
    let children = context.children.as_slice();
    let first_non_neg = children.partition_point(|c| c.z_index < 0);
    let first_pos = children.partition_point(|c| c.z_index <= 0);
    let neg = &children[..first_non_neg];
    let pos = &children[first_pos..];

    // Descendants are positioned relative to the stacking context's origin (the first fragment).
    let descendant_offset = Point::new(
      offset.x + context.offset_from_parent_context.x,
      offset.y + context.offset_from_parent_context.y,
    );

    let root_fragment = context.fragments.first();
    let descendant_content_offset = descendant_offset;
    let root_style = root_fragment.and_then(|f| f.style.as_deref());
    let root_opacity = root_style.map(|s| s.opacity).unwrap_or(1.0);
    if root_opacity <= f32::EPSILON {
      return false;
    }
    let has_opacity = root_opacity < 1.0 - f32::EPSILON;
    let paint_contained = root_style.map(|s| s.containment.paint).unwrap_or(false);
    let context_bounds = context.bounds.translate(offset);
    let root_fragment_offset = root_fragment
      .map(|fragment| {
        Point::new(
          descendant_offset.x - fragment.bounds.origin.x,
          descendant_offset.y - fragment.bounds.origin.y,
        )
      })
      .unwrap_or(offset);
    let root_fragment_rect =
      root_fragment.map(|fragment| fragment.bounds.translate(root_fragment_offset));
    let root_border_bounds = root_fragment_rect.unwrap_or(context_bounds);
    let appearance_none_control = root_fragment.and_then(|fragment| {
      Self::get_box_id(fragment).and_then(|box_id| {
        self
          .appearance_none_form_controls
          .as_ref()
          .and_then(|map| map.get(&box_id))
          .cloned()
      })
    });
    let scroll_delta = root_fragment
      .map(|fragment| {
        let scroll = self.element_scroll_offset(fragment);
        Point::new(-scroll.x, -scroll.y)
      })
      .unwrap_or(Point::ZERO);
    let appearance_none_overlays = match (appearance_none_control.as_deref(), root_style) {
      (Some(control), Some(style)) => {
        self.appearance_none_text_edit_overlays(control, style, root_border_bounds, scroll_delta)
      }
      _ => None,
    };

    let mut mask = root_style.and_then(|style| self.resolve_mask(style, root_border_bounds));
    if mask
      .as_ref()
      .is_some_and(|mask| mask.layers.iter().any(|layer| matches!(layer.clip, MaskClip::Text)))
    {
      if let (Some(mask), Some(fragment)) = (mask.as_mut(), root_fragment) {
        mask.text_clip = self.build_text_clip_runs(fragment, root_fragment_offset, visibility);
      }
    }
    let style_has_mask_border = root_style.is_some_and(|style| style.mask_border.is_active());
    let mask_border =
      root_style.and_then(|style| self.resolve_mask_border(style, root_border_bounds));
    let is_paged_media_page_root = is_root
      && root_fragment.is_some_and(|fragment| {
        Self::get_box_id(fragment).is_none()
          && fragment
            .children
            .iter()
            .any(|child| child.stacking_context.forced_z_index() == Some(0))
      });
    let root_background = if is_root {
      root_fragment.and_then(|fragment| {
        // The layout viewport can differ from the actual paint surface size when the renderer
        // reserves viewport scrollbar gutters (or when a caller paints into a surface larger than
        // the layout viewport). Canvas background propagation must cover the *paint* surface so
        // the exposed gutter region is painted with the document background (matching browsers).
        let canvas = self
          .culling_viewport
          .or(self.viewport)
          .unwrap_or_else(|| (context_bounds.width(), context_bounds.height()));
        if canvas.0 <= 0.0 || canvas.1 <= 0.0 {
          return None;
        }
        // `scrollbar-gutter: stable both-edges` lays out the scrollport in a reduced viewport, then
        // centers that scrollport within the full paint viewport. Painting applies a global
        // translation to shift all fragments into the centered scrollport; however the propagated
        // canvas background needs to cover the full paint viewport (including the left gutter).
        let viewport_inset = root_style
          .filter(|style| style.scrollbar_gutter.stable && style.scrollbar_gutter.both_edges)
          .map(|style| {
            let gutter = resolve_scrollbar_width(style).max(0.0);
            if gutter <= 0.0 {
              return Point::ZERO;
            }
            let paint_viewport = self.viewport.unwrap_or(canvas);
            let diff_x = paint_viewport.0 - fragment.bounds.width();
            let diff_y = paint_viewport.1 - fragment.bounds.height();
            let epsilon = 0.51;
            let inset_x = if diff_x > 0.0 && (diff_x - 2.0 * gutter).abs() <= epsilon {
              diff_x / 2.0
            } else {
              0.0
            };
            let inset_y = if diff_y > 0.0 && (diff_y - 2.0 * gutter).abs() <= epsilon {
              diff_y / 2.0
            } else {
              0.0
            };
            Point::new(inset_x, inset_y)
          })
          .unwrap_or(Point::ZERO);
        // The document canvas size (used for HTML canvas background propagation) should follow the
        // scrollable overflow area, *not* conservative paint bounds for the root stacking context.
        //
        // `context_bounds` includes paint overflow from descendants (e.g. outlines/filters, or
        // positioned elements far offscreen). Using it here can dramatically skew
        // background-size/background-position for propagated `<html>`/`<body>` backgrounds (notably
        // `background-size: cover` and gradients), because the background positioning area becomes
        // dominated by out-of-band paint bounds that do not contribute to the document canvas.
        //
        // Instead, derive the canvas extent from the root fragment's scroll overflow, clamping away
        // negative overflow (content cannot extend the scroll origin left/up of (0,0)).
        let scroll_canvas = fragment
          .scroll_overflow
          .union(Rect::new(Point::ZERO, fragment.bounds.size));
        let scroll_max_x = scroll_canvas.max_x().max(0.0);
        let scroll_max_y = scroll_canvas.max_y().max(0.0);

        let target_w = canvas.0.max(scroll_max_x);
        let target_h = canvas.1.max(scroll_max_y);
        let default_target_rect = Rect::from_xywh(
          root_border_bounds.x(),
          root_border_bounds.y(),
          target_w,
          target_h,
        );
        // CSS Page 3 draws the document canvas background as the page box background, which is
        // confined to the page area (inside the page margins).
        //
        // Pagination translates the page content subtree into the page box, so we can use that
        // translated subtree bounds as the paint target for propagated HTML canvas backgrounds.
        let target_rect = if is_paged_media_page_root {
          Self::paged_media_document_canvas_rect(fragment, descendant_offset)
            .unwrap_or(default_target_rect)
        } else {
          default_target_rect
        };
        let target_rect = if viewport_inset != Point::ZERO {
          target_rect.translate(Point::new(-viewport_inset.x, -viewport_inset.y))
        } else {
          target_rect
        };
        let (style, suppress_box_id, source_rect) =
          Self::root_background_candidate(fragment, descendant_offset)?;
        if !Self::has_paintable_background(&style) {
          return None;
        }
        // The canvas background is painted onto the *viewport* rect, so background positioning and
        // gradients should be computed relative to the viewport size rather than the potentially
        // smaller `<html>`/`<body>` box.
        let origin_rect = target_rect;
        let paint_border = Self::has_paintable_border(&style)
          && self.should_propagate_root_border(&style, source_rect, target_rect);
        // We normally only need to propagate the canvas background when the source element's
        // border box does not fully cover the paint target.
        //
        // In paged media, the document canvas background is a distinct layer above the page
        // background and below the page border/content group (CSS Page 3 §3.1). It must be painted
        // before page borders even when the `<html>`/`<body>` element covers the page.
        let source_covers_target = source_rect.min_x() <= target_rect.min_x()
          && source_rect.min_y() <= target_rect.min_y()
          && source_rect.max_x() >= target_rect.max_x()
          && source_rect.max_y() >= target_rect.max_y();
        // If the source element already covers the entire paint target, we can often skip explicit
        // canvas background propagation and let normal fragment painting handle it.
        //
        // However, when the canvas background originates from `<body>` (rather than the stacking
        // context root `<html>`), relying on normal fragment painting would paint the body
        // background in the in-flow layer. That would incorrectly cover negative z-index stacking
        // contexts promoted to the root stacking context (WPT paint/stacking negative z-index
        // reftests).
        //
        // Only skip propagation when the source is the root fragment itself.
        let root_box_id = Self::get_box_id(fragment).unwrap_or(usize::MAX);
        if source_covers_target && !is_paged_media_page_root && suppress_box_id == root_box_id {
          return None;
        }
        self.canvas_background_suppress_box_id = Some(suppress_box_id);
        if paint_border {
          self.canvas_border_suppress_box_id = Some(suppress_box_id);
        }
        Some(RootBackground {
          paint_rect: target_rect,
          origin_rect,
          style,
          paint_border,
        })
      })
    } else {
      None
    };
    let skip_contents = root_style.is_some_and(|style| match style.content_visibility {
      ContentVisibility::Hidden => true,
      ContentVisibility::Auto => visibility
        .rect
        .is_some_and(|vis| root_fragment_rect.is_some_and(|rect| !vis.intersects(rect))),
      ContentVisibility::Visible => false,
    });
    let plane_rect = match (root_style, root_fragment_rect) {
      (Some(style), Some(rect)) => Self::transform_reference_box(style, rect, self.viewport),
      (_, Some(rect)) => rect,
      _ => context_bounds,
    };

    let mix_blend_mode = root_style
      .map(|s| Self::convert_blend_mode(s.mix_blend_mode))
      .unwrap_or(BlendMode::Normal);
    let has_blend_mode_children = !is_root
      && children.iter().any(|child| {
        child
          .fragments
          .first()
          .and_then(|fragment| fragment.style.as_deref())
          .is_some_and(|style| !matches!(style.mix_blend_mode, MixBlendMode::Normal))
      });
    let is_isolated =
      Self::stacking_context_is_isolated(mix_blend_mode, root_style, has_blend_mode_children);
    let (filters, backdrop_filters, radii) = root_style
      .map(|style| {
        let breakdown = self.build_breakdown.as_deref();
        (
          Self::resolve_filters(
            &style.filter,
            style,
            self.viewport,
            &self.font_ctx,
            svg_filters,
            breakdown,
          ),
          Self::resolve_filters(
            &style.backdrop_filter,
            style,
            self.viewport,
            &self.font_ctx,
            svg_filters,
            breakdown,
          ),
          {
            let border_timer = breakdown.map(|_| Instant::now());
            let radii = Self::resolve_border_radii(Some(style), root_border_bounds, self.viewport);
            if let (Some(breakdown), Some(start)) = (breakdown, border_timer) {
              breakdown.record_border_radii(start.elapsed());
            }
            radii
          },
        )
      })
      .unwrap_or((
        Vec::new(),
        Vec::new(),
        crate::paint::display_list::BorderRadii::ZERO,
      ));
    let transform_bounds = root_border_bounds;
    let transforms = root_style
      .map(|style| {
        crate::paint::transform_resolver::resolve_transforms(style, transform_bounds, self.viewport)
      })
      .unwrap_or_default();
    let transform = transforms.self_transform;
    let child_perspective = transforms.child_perspective;
    let transform_style = root_style
      .map(crate::paint::css_transforms::used_transform_style)
      .unwrap_or(TransformStyle::Flat);
    let backface_visibility = root_style
      .map(|style| style.backface_visibility)
      .unwrap_or(BackfaceVisibility::Visible);

    let filter_outset = filter_outset_with_bounds(&filters, 1.0, Some(context.bounds));
    let backdrop_outset = filter_outset_with_bounds(&backdrop_filters, 1.0, Some(context.bounds));
    let expand_left = filter_outset.left.max(backdrop_outset.left);
    let expand_top = filter_outset.top.max(backdrop_outset.top);
    let expand_right = filter_outset.right.max(backdrop_outset.right);
    let expand_bottom = filter_outset.bottom.max(backdrop_outset.bottom);
    let mut local_bounds = context_bounds;
    if expand_left > 0.0 || expand_top > 0.0 || expand_right > 0.0 || expand_bottom > 0.0 {
      local_bounds = Rect::from_xywh(
        local_bounds.min_x() - expand_left,
        local_bounds.min_y() - expand_top,
        local_bounds.width() + expand_left + expand_right,
        local_bounds.height() + expand_top + expand_bottom,
      );
    }
    let mut world_bounds = Some(local_bounds);
    if let Some(t) = transform.as_ref() {
      world_bounds = Self::map_rect_with_transform(local_bounds, t);
    }
    // `visibility` is expressed in the parent stacking context's coordinate space (i.e. the
    // output/viewport space after the parent transform has been applied). Fragments within this
    // stacking context are emitted in *local* coordinates and will be transformed at render time
    // via the `PushStackingContext` item. For correct culling we must therefore convert the
    // visible rect into the local (pre-transform) coordinate space.
    //
    // Additionally, when this stacking context applies filters (e.g. `blur()`), offscreen pixels
    // within the filter's kernel radius can contribute to visible output (e.g. a blurred element
    // just outside the viewport edge). Expand the culling rect by the filter outsets so we don't
    // incorrectly cull those source pixels away.
    let mut context_visibility = Visibility {
      rect: Self::visible_in_local_space(visibility.rect, transform.as_ref()).map(|rect| {
        if expand_left > 0.0 || expand_top > 0.0 || expand_right > 0.0 || expand_bottom > 0.0 {
          Rect::from_xywh(
            rect.x() - expand_left,
            rect.y() - expand_top,
            rect.width() + expand_left + expand_right,
            rect.height() + expand_top + expand_bottom,
          )
        } else {
          rect
        }
      }),
      hard_clip: visibility.hard_clip,
    };
    // Filters like `blur()` sample pixels outside the visible/clipped region. To avoid culling away
    // offscreen content that can blur back into view, expand the inherited visibility rect by the
    // conservative filter outsets we already compute for stacking-context bounds.
    if expand_left > 0.0 || expand_top > 0.0 || expand_right > 0.0 || expand_bottom > 0.0 {
      if let Some(vis) = context_visibility.rect {
        context_visibility.rect = Some(Rect::from_xywh(
          vis.x() - expand_left,
          vis.y() - expand_top,
          vis.width() + expand_left + expand_right,
          vis.height() + expand_top + expand_bottom,
        ));
      }
    }
    if child_perspective.is_none() {
      if let (Some(vis), Some(bounds)) = (visibility.rect, world_bounds) {
        if !vis.intersects(bounds) {
          return false;
        }
      }
    }
    context_visibility = context_visibility.intersect(Some(local_bounds), false);

    if !filters.is_empty() || !backdrop_filters.is_empty() {
      // When only part of a filtered stacking context is visible (e.g. it sits on the viewport
      // edge), pixels just outside the visible rect can still influence the visible result through
      // kernel-based effects like `blur()`. Culling descendants strictly to the visible output
      // region would drop those offscreen source pixels and truncate the filter halo.
      //
      // Expand the visibility/culling rect by a conservative filter halo so offscreen contributors
      // are still emitted into the display list (see
      // `src/paint/tests/filter_blur_bleeds_from_offscreen_source.rs`).
      if let Some(visible) = context_visibility.rect {
        let filters_halo = filter_halo_outset_with_bounds(&filters, 1.0, Some(context.bounds));
        let backdrop_halo =
          filter_halo_outset_with_bounds(&backdrop_filters, 1.0, Some(context.bounds));
        let halo_left = filters_halo.left.max(backdrop_halo.left);
        let halo_top = filters_halo.top.max(backdrop_halo.top);
        let halo_right = filters_halo.right.max(backdrop_halo.right);
        let halo_bottom = filters_halo.bottom.max(backdrop_halo.bottom);

        if halo_left > 0.0 || halo_top > 0.0 || halo_right > 0.0 || halo_bottom > 0.0 {
          let expanded = Rect::from_xywh(
            visible.min_x() - halo_left,
            visible.min_y() - halo_top,
            visible.width() + halo_left + halo_right,
            visible.height() + halo_top + halo_bottom,
          );
          context_visibility.rect = expanded.intersection(local_bounds).or(Some(expanded));
        }
      }
    }

    let style_has_clip_path = root_style
      .is_some_and(|style| !matches!(style.clip_path, crate::style::types::ClipPath::None));

    let viewport = self
      .viewport
      .unwrap_or_else(|| (root_border_bounds.width(), root_border_bounds.height()));
    let clip_path_timer = if self.build_breakdown.is_some() && root_style.is_some() {
      Some(Instant::now())
    } else {
      None
    };
    let clip_path = match root_style {
      Some(style) => match resolve_clip_path(style, root_border_bounds, viewport, &self.font_ctx) {
        Ok(clip_path) => clip_path,
        Err(err) => {
          self.error = Some(err);
          return false;
        }
      },
      None => None,
    };
    let clip_path_mask = root_style.and_then(|style| {
      let (src, reference_override) = match &style.clip_path {
        crate::style::types::ClipPath::Url(src, reference_override) => {
          (src.as_str(), *reference_override)
        }
        _ => return None,
      };

      let reference = reference_override.unwrap_or(crate::style::types::ReferenceBox::BorderBox);
      let reference_rect = crate::paint::clip_path::resolve_clip_path_reference_box_rect(
        style,
        root_border_bounds,
        viewport,
        &self.font_ctx,
        reference,
      );
      // `clip-path: url(#id)` is defined in the coordinate space of the chosen reference box, but
      // the clipPath geometry may extend outside that box (e.g. `clipPathUnits="userSpaceOnUse"`
      // with negative coordinates or `clipPathUnits="objectBoundingBox"` with values outside [0,1]).
      //
      // Rasterize the clip-path mask over the full stacking context bounds and shift the SVG
      // viewBox so (0,0) still corresponds to the reference box origin.
      //
      // For `objectBoundingBox` clip paths, the SVG coordinate system is relative to the clipped
      // element's bounding box. When inlining the SVG we rewrite these clip paths to
      // `userSpaceOnUse` anchored to the reference box so bbox-relative scaling stays correct even
      // when the mask surface is larger than the reference box.
      let mask_bounds_css = context_bounds;
      let image = self.decode_clip_path_url(src, style, reference_rect, mask_bounds_css)?;
      Some((image, mask_bounds_css))
    });
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), clip_path_timer) {
      breakdown.record_clip_path(start.elapsed());
    }
    let clip_rect = root_style
      .and_then(|style| Self::clip_rect_from_style(style, root_border_bounds, Some(viewport)));

    // `context_bounds` is computed from layout and includes paint overflow for the entire document.
    // When building a display list for a specific visible region (viewport culling), far-offscreen
    // overflow (e.g. `text-indent:-9999em` used for accessible-but-hidden labels) can dramatically
    // inflate stacking-context bounds and force the renderer to consider enormous offscreen
    // surfaces for layer allocation.
    //
    // Clamp the stacking context bounds to the rect we are actually building/keeping display
    // items for (the current visibility rect in local/pre-transform coordinates, plus any filter
    // halo expansion). Do **not** include overflow clipping here: overflow clips are applied only
    // to descendants, while the stacking context root may still paint outside the scrollport (e.g.
    // box-shadow).
    let mut stacking_context_bounds = context_bounds;
    let mut bounds_visibility = context_visibility;
    if let Some(bounds) = clip_path.as_ref().map(|clip| clip.bounds()) {
      bounds_visibility = bounds_visibility.intersect(Some(bounds), true);
    } else if let Some((_, rect)) = clip_path_mask.as_ref() {
      // `clip-path: url(#id)` alpha masks are rasterized over the stacking context bounds and act
      // as a hard clip for the subtree.
      bounds_visibility = bounds_visibility.intersect(Some(*rect), true);
    }
    if let Some(bounds) = clip_rect.as_ref().and_then(|clip| Self::clip_bounds(clip)) {
      bounds_visibility = bounds_visibility.intersect(Some(bounds), true);
    }
    if let Some(vis) = bounds_visibility.rect {
      if let Some(intersection) = context_bounds.intersection(vis) {
        stacking_context_bounds = intersection;
      }
    }

    let overflow_clip = root_style.and_then(|style| {
      Self::overflow_clip_from_style(
        style,
        root_border_bounds,
        context_bounds,
        self.viewport,
        self.build_breakdown.as_deref(),
      )
    });
    let mut paint_containment_clip = if paint_contained {
      root_fragment
        .and_then(|fragment| {
          let style = root_style?;
          let rect = Rect::new(
            Point::new(
              fragment.bounds.origin.x + root_fragment_offset.x,
              fragment.bounds.origin.y + root_fragment_offset.y,
            ),
            fragment.bounds.size,
          );
          let rects = Self::background_rects(rect, style, self.viewport);
          let radii = if Self::border_radius_is_zero(style) {
            crate::paint::display_list::BorderRadii::ZERO
          } else {
            Self::resolve_clip_radii(
              style,
              &rects,
              BackgroundBox::PaddingBox,
              self.viewport,
              self.build_breakdown.as_deref(),
            )
          };
          Some(ClipItem {
            shape: ClipShape::Rect {
              rect: rects.padding,
              radii: if radii.is_zero() { None } else { Some(radii) },
            },
          })
        })
        .or_else(|| {
          Some(ClipItem {
            shape: ClipShape::Rect {
              rect: context_bounds,
              radii: None,
            },
          })
        })
    } else {
      None
    };

    let mut child_visibility = context_visibility;
    if expand_left > 0.0 || expand_top > 0.0 || expand_right > 0.0 || expand_bottom > 0.0 {
      child_visibility.rect = child_visibility.rect.map(|rect| {
        Rect::from_xywh(
          rect.min_x() - expand_left,
          rect.min_y() - expand_top,
          rect.width() + expand_left + expand_right,
          rect.height() + expand_top + expand_bottom,
        )
      });
    }
    if let Some(bounds) = clip_path.as_ref().map(|clip| clip.bounds()) {
      child_visibility = child_visibility.intersect(Some(bounds), true);
    } else if let Some((_, rect)) = clip_path_mask.as_ref() {
      // Fragment-only `clip-path: url(#id)` masks are rasterized over the stacking context bounds,
      // so those bounds are a safe hard-clip rectangle for culling purposes.
      child_visibility = child_visibility.intersect(Some(*rect), true);
    }
    if let Some(bounds) = clip_rect.as_ref().and_then(|clip| Self::clip_bounds(clip)) {
      child_visibility = child_visibility.intersect(Some(bounds), true);
    }
    if let Some(bounds) = overflow_clip
      .as_ref()
      .and_then(|clip| Self::clip_bounds(clip))
    {
      child_visibility = child_visibility.intersect(Some(bounds), true);
    }
    if let Some(bounds) = paint_containment_clip
      .as_ref()
      .and_then(|clip| Self::clip_bounds(clip))
    {
      child_visibility = child_visibility.intersect(Some(bounds), true);
    }
    // If the visible rect is empty after intersecting local bounds/clips, we can early-out.
    if child_visibility.rect.is_none() && visibility.rect.is_some() {
      return false;
    }

    let local_child_visibility = child_visibility;

    let will_change_backdrop_root =
      root_style.is_some_and(|style| style.will_change.establishes_backdrop_root());

    let has_effects = is_isolated
      || transform.is_some()
      || child_perspective.is_some()
      || mix_blend_mode != BlendMode::Normal
      || !filters.is_empty()
      || !backdrop_filters.is_empty()
      || style_has_clip_path
      || clip_rect.is_some()
      || overflow_clip.is_some()
      || paint_contained
      || !radii.is_zero()
      || mask.is_some()
      || style_has_mask_border
      || mask_border.is_some()
      || will_change_backdrop_root
      || has_opacity;

    let establishes_backdrop_root = Self::stacking_context_establishes_backdrop_root(
      is_root,
      root_style,
      mix_blend_mode,
      &filters,
      &backdrop_filters,
      has_opacity,
      mask.is_some(),
      style_has_clip_path,
    );
    let ancestor_backface_pushed =
      self.push_backface_visibility_chain(context.backface_visibility_depth);
    let ancestor_clips_pushed = self.push_clip_chain(&context.clip_chain, offset);

    // Track whether this stacking context has any *descendant* stacking context that requires
    // backdrop sampling (`backdrop-filter` or non-normal `mix-blend-mode`).
    //
    // Note: The return value of `build_stacking_context` includes the current stacking context
    // itself so ancestors can decide whether they need to preserve Backdrop Root boundaries (e.g.
    // for `will-change`). The value stored on the display list, however, is strictly for
    // descendants so render-time decisions (like non-isolated group surface initialization) can
    // distinguish between leaf blend-mode elements and those that need to provide a seeded
    // backdrop for their children.
    let self_is_backdrop_sensitive =
      !backdrop_filters.is_empty() || mix_blend_mode != BlendMode::Normal;
    let mut has_backdrop_sensitive_descendants = false;

    if is_root && !has_effects {
      if !is_paged_media_page_root {
        if let Some(root_background) = root_background.as_ref() {
          self.emit_root_background(root_background);
        }
      }
      self.emit_fragment_list_shallow(
        &context.fragments,
        root_fragment_offset,
        has_opacity,
        local_child_visibility,
        false,
      );
      if skip_contents {
        if is_paged_media_page_root {
          if let Some(root_background) = root_background.as_ref() {
            self.emit_root_background(root_background);
          }
        }
        self.pop_clips(ancestor_clips_pushed);
        self.pop_backface_visibility_chain(ancestor_backface_pushed);
        return self_is_backdrop_sensitive || has_backdrop_sensitive_descendants;
      }
      let mut deadline_counter = 0usize;
      for child in neg {
        if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
          break;
        }
        has_backdrop_sensitive_descendants |= self.build_stacking_context(
          child,
          descendant_content_offset,
          false,
          svg_filters,
          local_child_visibility,
          depth + 1,
        );
      }

      if is_paged_media_page_root {
        if let Some(root_background) = root_background.as_ref() {
          self.emit_root_background(root_background);
        }
      }
      self.emit_fragment_list(
        &context.layer3_blocks,
        descendant_content_offset,
        local_child_visibility,
      );
      self.emit_fragment_list(
        &context.layer4_floats,
        descendant_content_offset,
        local_child_visibility,
      );

      if let Some(overlays) = appearance_none_overlays.as_ref() {
        let selection_color = Rgba {
          r: 0,
          g: 120,
          b: 215,
          a: 0.35,
        };
        for rect in &overlays.selection_rects {
          self.list.push(DisplayItem::FillRect(FillRectItem {
            rect: *rect,
            color: selection_color,
          }));
        }
      }
      self.emit_fragment_list(
        &context.layer5_inlines,
        descendant_content_offset,
        local_child_visibility,
      );
      if let Some(overlays) = appearance_none_overlays.as_ref() {
        if let Some((rect, color)) = overlays.caret_rect {
          self.list.push(DisplayItem::FillRect(FillRectItem { rect, color }));
        }
      }
      for item in context.layer6_iter() {
        if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
          break;
        }
        match item {
          Layer6Item::Positioned(fragment) => {
            let backface_pushed =
              self.push_backface_visibility_chain(fragment.backface_visibility_depth);
            let pushed = self.push_clip_chain(&fragment.clip_chain, descendant_content_offset);
            self.build_fragment(
              &fragment.fragment,
              descendant_content_offset,
              local_child_visibility,
            );
            self.pop_clips(pushed);
            self.pop_backface_visibility_chain(backface_pushed);
          }
          Layer6Item::ZeroContext(child) => {
            has_backdrop_sensitive_descendants |= self.build_stacking_context(
              child,
              descendant_content_offset,
              false,
              svg_filters,
              local_child_visibility,
              depth + 1,
            );
          }
        }
      }

      for child in pos {
        if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
          break;
        }
        has_backdrop_sensitive_descendants |= self.build_stacking_context(
          child,
          descendant_content_offset,
          false,
          svg_filters,
          local_child_visibility,
          depth + 1,
        );
      }
      self.pop_clips(ancestor_clips_pushed);
      self.pop_backface_visibility_chain(ancestor_backface_pushed);
      return self_is_backdrop_sensitive || has_backdrop_sensitive_descendants;
    }

    let needs_layer_bounds = is_isolated
      || mix_blend_mode != BlendMode::Normal
      || !filters.is_empty()
      || !backdrop_filters.is_empty()
      || has_opacity
      || mask.is_some()
      || mask_border.is_some()
      || clip_path.is_some()
      || clip_path_mask.is_some();

    let stacking_push_index = self.list.len();
    self
      .list
      .push(DisplayItem::PushStackingContext(StackingContextItem {
        z_index: context.z_index,
        creates_stacking_context: true,
        is_root,
        establishes_backdrop_root,
        has_backdrop_sensitive_descendants: false,
        bounds: stacking_context_bounds,
        plane_rect,
        mix_blend_mode,
        opacity: root_opacity,
        is_isolated,
        transform,
        child_perspective,
        transform_style,
        backface_visibility,
        filters,
        backdrop_filters,
        radii,
        mask,
        mask_border,
        has_clip_path: style_has_clip_path,
      }));

    let mut pushed_clips = 0;
    if let Some(path) = clip_path {
      self.list.push(DisplayItem::PushClip(ClipItem {
        shape: ClipShape::Path { path },
      }));
      pushed_clips += 1;
    } else if let Some((image, rect)) = clip_path_mask {
      self.list.push(DisplayItem::PushClip(ClipItem {
        shape: ClipShape::AlphaMask { image, rect },
      }));
      pushed_clips += 1;
    }
    let clip_rect_pushed_for_root = clip_rect.is_some();
    if let Some(clip) = clip_rect {
      self.list.push(DisplayItem::PushClip(clip));
      pushed_clips += 1;
    }

    if !is_paged_media_page_root {
      if let Some(root_background) = root_background.as_ref() {
        self.emit_root_background(root_background);
      }
    }
    // Paint the stacking context root (backgrounds, borders, shadows) before applying overflow
    // clipping so outer effects remain visible.
    self.emit_fragment_list_shallow(
      &context.fragments,
      root_fragment_offset,
      has_opacity,
      local_child_visibility,
      clip_rect_pushed_for_root,
    );
    if skip_contents {
      if is_paged_media_page_root {
        if let Some(root_background) = root_background.as_ref() {
          self.emit_root_background(root_background);
        }
      }
      for _ in 0..pushed_clips {
        self.list.push(DisplayItem::PopClip);
      }
      if needs_layer_bounds {
        self.expand_stacking_context_bounds_from_items(stacking_push_index, stacking_context_bounds);
      }
      self.list.push(DisplayItem::PopStackingContext);
      if let Some(DisplayItem::PushStackingContext(item)) =
        self.list.items_mut().get_mut(stacking_push_index)
      {
        item.has_backdrop_sensitive_descendants = has_backdrop_sensitive_descendants;
      }
      self.pop_clips(ancestor_clips_pushed);
      self.pop_backface_visibility_chain(ancestor_backface_pushed);
      return self_is_backdrop_sensitive || has_backdrop_sensitive_descendants;
    }

    let mut paint_containment_clip_pushed = false;
    if let Some(clip) = paint_containment_clip.take() {
      self.list.push(DisplayItem::PushClip(clip));
      paint_containment_clip_pushed = true;
    }

    let mut overflow_clip_pushed = false;
    if let Some(clip) = overflow_clip {
      self.list.push(DisplayItem::PushClip(clip));
      overflow_clip_pushed = true;
    }

    let mut deadline_counter = 0usize;
    for child in neg {
      if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
        break;
      }
      has_backdrop_sensitive_descendants |= self.build_stacking_context(
        child,
        descendant_content_offset,
        false,
        svg_filters,
        local_child_visibility,
        depth + 1,
      );
    }
    if is_paged_media_page_root {
      if let Some(root_background) = root_background.as_ref() {
        self.emit_root_background(root_background);
      }
    }
    self.emit_fragment_list(
      &context.layer3_blocks,
      descendant_content_offset,
      local_child_visibility,
    );
    self.emit_fragment_list(
      &context.layer4_floats,
      descendant_content_offset,
      local_child_visibility,
    );

    if let Some(overlays) = appearance_none_overlays.as_ref() {
      let selection_color = Rgba {
        r: 0,
        g: 120,
        b: 215,
        a: 0.35,
      };
      for rect in &overlays.selection_rects {
        self.list.push(DisplayItem::FillRect(FillRectItem {
          rect: *rect,
          color: selection_color,
        }));
      }
    }
    self.emit_fragment_list(
      &context.layer5_inlines,
      descendant_content_offset,
      local_child_visibility,
    );
    if let Some(overlays) = appearance_none_overlays.as_ref() {
      if let Some((rect, color)) = overlays.caret_rect {
        self.list.push(DisplayItem::FillRect(FillRectItem { rect, color }));
      }
    }
    for item in context.layer6_iter() {
      if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
        break;
      }
      match item {
        Layer6Item::Positioned(fragment) => {
          let backface_pushed =
            self.push_backface_visibility_chain(fragment.backface_visibility_depth);
          let pushed = self.push_clip_chain(&fragment.clip_chain, descendant_content_offset);
          self.build_fragment(
            &fragment.fragment,
            descendant_content_offset,
            local_child_visibility,
          );
          self.pop_clips(pushed);
          self.pop_backface_visibility_chain(backface_pushed);
        }
        Layer6Item::ZeroContext(child) => {
          has_backdrop_sensitive_descendants |= self.build_stacking_context(
            child,
            descendant_content_offset,
            false,
            svg_filters,
            local_child_visibility,
            depth + 1,
          );
        }
      }
    }

    for child in pos {
      if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
        break;
      }
      has_backdrop_sensitive_descendants |= self.build_stacking_context(
        child,
        descendant_content_offset,
        false,
        svg_filters,
        local_child_visibility,
        depth + 1,
      );
    }

    if overflow_clip_pushed {
      self.list.push(DisplayItem::PopClip);
    }
    if paint_containment_clip_pushed {
      self.list.push(DisplayItem::PopClip);
    }
    for _ in 0..pushed_clips {
      self.list.push(DisplayItem::PopClip);
    }
    if needs_layer_bounds {
      self.expand_stacking_context_bounds_from_items(stacking_push_index, stacking_context_bounds);
    }
    self.list.push(DisplayItem::PopStackingContext);
    if let Some(DisplayItem::PushStackingContext(item)) =
      self.list.items_mut().get_mut(stacking_push_index)
    {
      item.has_backdrop_sensitive_descendants = has_backdrop_sensitive_descendants;
    }
    self.pop_clips(ancestor_clips_pushed);
    self.pop_backface_visibility_chain(ancestor_backface_pushed);
    self_is_backdrop_sensitive || has_backdrop_sensitive_descendants
  }

  fn expand_stacking_context_bounds_from_items(&mut self, push_index: usize, base_bounds: Rect) {
    let end = self.list.len();
    if push_index + 1 >= end {
      return;
    }

    let mut paint_bounds: Option<Rect> = None;
    {
      let items = self.list.items();
      let slice = &items[push_index + 1..end];
      for item in slice {
        let bounds = match item {
          // Clips can be larger than the painted ink they restrict; avoid using them for layer
          // bounds so we don't accidentally allocate massive layers.
          DisplayItem::PushClip(_) => None,
          _ => item.bounds(),
        };
        let Some(bounds) = bounds else {
          continue;
        };
        if bounds.width() <= 0.0
          || bounds.height() <= 0.0
          || !bounds.x().is_finite()
          || !bounds.y().is_finite()
          || !bounds.width().is_finite()
          || !bounds.height().is_finite()
        {
          continue;
        }
        paint_bounds = Some(match paint_bounds {
          Some(existing) => existing.union(bounds),
          None => bounds,
        });
      }
    }

    let Some(paint_bounds) = paint_bounds else {
      return;
    };
    let expanded = base_bounds.union(paint_bounds);

    if let Some(DisplayItem::PushStackingContext(sc)) = self.list.items_mut().get_mut(push_index) {
      sc.bounds = expanded;
    }
  }

  fn overflow_clip_from_style(
    style: &ComputedStyle,
    border_bounds: Rect,
    expansion_bounds: Rect,
    viewport: Option<(f32, f32)>,
    breakdown: Option<&BuildBreakdown>,
  ) -> Option<ClipItem> {
    let clip_x = Self::overflow_axis_clips(style.overflow_x);
    let clip_y = Self::overflow_axis_clips(style.overflow_y);
    if !clip_x && !clip_y {
      return None;
    }

    let rects = Self::background_rects(border_bounds, style, viewport);
    Self::overflow_clip_from_style_with_rects(
      style,
      &rects,
      clip_x,
      clip_y,
      expansion_bounds,
      viewport,
      breakdown,
    )
  }

  fn overflow_axis_clips(overflow: crate::style::types::Overflow) -> bool {
    matches!(
      overflow,
      crate::style::types::Overflow::Hidden
        | crate::style::types::Overflow::Scroll
        | crate::style::types::Overflow::Auto
        | crate::style::types::Overflow::Clip
    )
  }

  fn overflow_clip_from_style_with_rects(
    style: &ComputedStyle,
    rects: &BackgroundRects,
    clip_x: bool,
    clip_y: bool,
    expansion_bounds: Rect,
    viewport: Option<(f32, f32)>,
    _breakdown: Option<&BuildBreakdown>,
  ) -> Option<ClipItem> {
    let percentage_base = rects.border.width().max(0.0);
    let raw_margin = &style.overflow_clip_margin.margin;
    let resolved_margin = Self::resolve_length_for_paint(
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

    let margin_x = if matches!(style.overflow_x, crate::style::types::Overflow::Clip) {
      resolved_margin
    } else {
      0.0
    };
    let margin_y = if matches!(style.overflow_y, crate::style::types::Overflow::Clip) {
      resolved_margin
    } else {
      0.0
    };

    let clip_box_x = if matches!(style.overflow_x, crate::style::types::Overflow::Clip) {
      style.overflow_clip_margin.visual_box
    } else {
      crate::style::types::VisualBox::PaddingBox
    };
    let clip_box_y = if matches!(style.overflow_y, crate::style::types::Overflow::Clip) {
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
      return None;
    }

    if !clip_x {
      let min_x = clip_rect.min_x().min(expansion_bounds.min_x());
      let max_x = clip_rect.max_x().max(expansion_bounds.max_x());
      clip_rect.origin.x = min_x;
      clip_rect.size.width = (max_x - min_x).max(0.0);
    }
    if !clip_y {
      let min_y = clip_rect.min_y().min(expansion_bounds.min_y());
      let max_y = clip_rect.max_y().max(expansion_bounds.max_y());
      clip_rect.origin.y = min_y;
      clip_rect.size.height = (max_y - min_y).max(0.0);
    }

    if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
      return None;
    }

    let radii = if clip_x && clip_y && !Self::border_radius_is_zero(style) {
      let base = Self::resolve_border_radii(Some(style), rects.border, viewport);
      if base.is_zero() {
        None
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
            crate::style::types::VisualBox::PaddingBox => (padding_inset_left, padding_inset_right),
            crate::style::types::VisualBox::ContentBox => (content_inset_left, content_inset_right),
          }
        };
        let inset_y = |vb: crate::style::types::VisualBox| -> (f32, f32) {
          match vb {
            crate::style::types::VisualBox::BorderBox => (0.0, 0.0),
            crate::style::types::VisualBox::PaddingBox => (padding_inset_top, padding_inset_bottom),
            crate::style::types::VisualBox::ContentBox => (content_inset_top, content_inset_bottom),
          }
        };

        let (inset_left, inset_right) = inset_x(clip_box_x);
        let (inset_top, inset_bottom) = inset_y(clip_box_y);

        let offset_left = -inset_left + margin_x;
        let offset_right = -inset_right + margin_x;
        let offset_top = -inset_top + margin_y;
        let offset_bottom = -inset_bottom + margin_y;

        let radii = crate::paint::display_list::BorderRadii {
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
        .clamped(clip_rect.width(), clip_rect.height());
        (!radii.is_zero()).then_some(radii)
      }
    } else {
      None
    };

    Some(ClipItem {
      shape: ClipShape::Rect {
        rect: clip_rect,
        radii,
      },
    })
  }

  fn clip_rect_from_style(
    style: &ComputedStyle,
    bounds: Rect,
    viewport: Option<(f32, f32)>,
  ) -> Option<ClipItem> {
    // CSS 2.1 `clip` applies only to absolutely positioned elements (position: absolute|fixed).
    // For other positioning schemes the used value is `auto`, meaning no clip is applied.
    if !matches!(style.position, Position::Absolute | Position::Fixed) {
      return None;
    }
    let clip = style.clip.as_ref()?;
    let width = bounds.width().max(0.0);
    let height = bounds.height().max(0.0);
    // CSS 2.1 `clip`: rect() components are offsets from the element's border box.
    // Convert to absolute coordinates by adding the element's origin.
    let left_offset = match &clip.left {
      crate::style::types::ClipComponent::Auto => 0.0,
      crate::style::types::ClipComponent::Length(len) => {
        Self::resolve_length_for_paint(len, style.font_size, style.root_font_size, width, viewport)
      }
    };
    let top_offset = match &clip.top {
      crate::style::types::ClipComponent::Auto => 0.0,
      crate::style::types::ClipComponent::Length(len) => {
        Self::resolve_length_for_paint(len, style.font_size, style.root_font_size, height, viewport)
      }
    };
    let right_offset = match &clip.right {
      crate::style::types::ClipComponent::Auto => width,
      crate::style::types::ClipComponent::Length(len) => {
        Self::resolve_length_for_paint(len, style.font_size, style.root_font_size, width, viewport)
      }
    };
    let bottom_offset = match &clip.bottom {
      crate::style::types::ClipComponent::Auto => height,
      crate::style::types::ClipComponent::Length(len) => {
        Self::resolve_length_for_paint(len, style.font_size, style.root_font_size, height, viewport)
      }
    };

    let left = bounds.x() + left_offset;
    let top = bounds.y() + top_offset;
    let right = bounds.x() + right_offset;
    let bottom = bounds.y() + bottom_offset;

    // CSS2 `clip` can define an empty clipping region (e.g. `rect(1px, 1px, 1px, 1px)`), which
    // should clip all painting for the element. Preserve that behavior by emitting an empty clip
    // item rather than treating it as "no clip".
    let rect = Rect::from_xywh(left, top, (right - left).max(0.0), (bottom - top).max(0.0));
    Some(ClipItem {
      shape: ClipShape::Rect { rect, radii: None },
    })
  }

  fn convert_blend_mode(mode: MixBlendMode) -> BlendMode {
    match mode {
      MixBlendMode::Normal => BlendMode::Normal,
      MixBlendMode::Multiply => BlendMode::Multiply,
      MixBlendMode::Screen => BlendMode::Screen,
      MixBlendMode::Overlay => BlendMode::Overlay,
      MixBlendMode::Darken => BlendMode::Darken,
      MixBlendMode::Lighten => BlendMode::Lighten,
      MixBlendMode::ColorDodge => BlendMode::ColorDodge,
      MixBlendMode::ColorBurn => BlendMode::ColorBurn,
      MixBlendMode::HardLight => BlendMode::HardLight,
      MixBlendMode::SoftLight => BlendMode::SoftLight,
      MixBlendMode::Difference => BlendMode::Difference,
      MixBlendMode::Exclusion => BlendMode::Exclusion,
      MixBlendMode::Hue => BlendMode::Hue,
      MixBlendMode::Saturation => BlendMode::Saturation,
      MixBlendMode::Color => BlendMode::Color,
      MixBlendMode::Luminosity => BlendMode::Luminosity,
      MixBlendMode::PlusLighter => BlendMode::PlusLighter,
      MixBlendMode::PlusDarker => BlendMode::PlusDarker,
      MixBlendMode::HueHsv => BlendMode::HueHsv,
      MixBlendMode::SaturationHsv => BlendMode::SaturationHsv,
      MixBlendMode::ColorHsv => BlendMode::ColorHsv,
      MixBlendMode::LuminosityHsv => BlendMode::LuminosityHsv,
      MixBlendMode::HueOklch => BlendMode::HueOklch,
      MixBlendMode::ChromaOklch => BlendMode::ChromaOklch,
      MixBlendMode::ColorOklch => BlendMode::ColorOklch,
      MixBlendMode::LuminosityOklch => BlendMode::LuminosityOklch,
    }
  }

  fn decoration_rect_and_clip(
    fragment: &FragmentNode,
    absolute_rect: Rect,
    style: &ComputedStyle,
  ) -> (Rect, Option<ClipItem>) {
    if !matches!(style.box_decoration_break, BoxDecorationBreak::Slice) {
      return (absolute_rect, None);
    }

    let info = fragment.slice_info;
    if info.is_first && info.is_last {
      return (absolute_rect, None);
    }

    let horizontal_block = block_axis_is_horizontal(style.writing_mode);
    let block_positive = block_axis_positive(style.writing_mode);
    let original_block = if horizontal_block {
      info.original_block_size.max(absolute_rect.width()).max(0.0)
    } else {
      info
        .original_block_size
        .max(absolute_rect.height())
        .max(0.0)
    };
    let slice_offset = info.slice_offset.clamp(0.0, original_block);
    let rect = if horizontal_block {
      let x = if block_positive {
        absolute_rect.x() - slice_offset
      } else {
        absolute_rect.max_x() + slice_offset - original_block
      };
      Rect::from_xywh(x, absolute_rect.y(), original_block, absolute_rect.height())
    } else {
      Rect::from_xywh(
        absolute_rect.x(),
        absolute_rect.y() - slice_offset,
        absolute_rect.width(),
        original_block,
      )
    };
    let clip = ClipItem {
      shape: ClipShape::Rect {
        rect: absolute_rect,
        radii: None,
      },
    };
    (rect, Some(clip))
  }

  fn background_rects(
    rect: Rect,
    style: &ComputedStyle,
    viewport: Option<(f32, f32)>,
  ) -> BackgroundRects {
    let font_size = style.font_size;
    let base = rect.width().max(0.0);

    let border_left = Self::resolve_length_for_paint(
      &style.used_border_left_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let border_right = Self::resolve_length_for_paint(
      &style.used_border_right_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let border_top = Self::resolve_length_for_paint(
      &style.used_border_top_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let border_bottom = Self::resolve_length_for_paint(
      &style.used_border_bottom_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );

    let padding_left = Self::resolve_length_for_paint(
      &style.padding_left,
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let padding_right = Self::resolve_length_for_paint(
      &style.padding_right,
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let padding_top = Self::resolve_length_for_paint(
      &style.padding_top,
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let padding_bottom = Self::resolve_length_for_paint(
      &style.padding_bottom,
      font_size,
      style.root_font_size,
      base,
      viewport,
    );

    let border_rect = rect;
    let padding_rect = Self::inset_rect(
      border_rect,
      border_left,
      border_top,
      border_right,
      border_bottom,
    );
    let content_rect = Self::inset_rect(
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

  fn replaced_content_rect_and_radii(
    &self,
    border_rect: Rect,
    style: Option<&ComputedStyle>,
  ) -> (Rect, BorderRadii) {
    let Some(style) = style else {
      return (border_rect, BorderRadii::ZERO);
    };

    let rects = Self::background_rects(border_rect, style, self.viewport);
    let radii = if Self::border_radius_is_zero(style) {
      BorderRadii::ZERO
    } else {
      Self::resolve_clip_radii(
        style,
        &rects,
        BackgroundBox::ContentBox,
        self.viewport,
        self.build_breakdown.as_deref(),
      )
    };

    (rects.content, radii)
  }

  fn replaced_content_clip_item(
    style: Option<&ComputedStyle>,
    content_rect: Rect,
    dest_rect: Rect,
    clip_radii: BorderRadii,
  ) -> Option<ClipItem> {
    let Some(style) = style else {
      return None;
    };

    let clip_x = Self::overflow_axis_clips(style.overflow_x);
    let clip_y = Self::overflow_axis_clips(style.overflow_y);
    if !clip_x && !clip_y {
      return None;
    }

    let mut clip_rect = content_rect;
    if !clip_x {
      let min_x = clip_rect.min_x().min(dest_rect.min_x());
      let max_x = clip_rect.max_x().max(dest_rect.max_x());
      clip_rect.origin.x = min_x;
      clip_rect.size.width = (max_x - min_x).max(0.0);
    }
    if !clip_y {
      let min_y = clip_rect.min_y().min(dest_rect.min_y());
      let max_y = clip_rect.max_y().max(dest_rect.max_y());
      clip_rect.origin.y = min_y;
      clip_rect.size.height = (max_y - min_y).max(0.0);
    }
    if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
      return None;
    }

    let radii = if clip_x && clip_y && !clip_radii.is_zero() {
      Some(clip_radii)
    } else {
      None
    };

    Some(ClipItem {
      shape: ClipShape::Rect {
        rect: clip_rect,
        radii,
      },
    })
  }

  fn inline_svg_for_svg_mask(&self, mask_id: &str, bounds: Rect) -> Option<String> {
    let defs = self.svg_id_defs.as_ref()?;
    let width = bounds.width().ceil().max(1.0) as u32;
    let height = bounds.height().ceil().max(1.0) as u32;
    crate::paint::svg_mask_image::inline_svg_for_mask_id(defs, mask_id, width, height)
  }

  fn inline_svg_for_svg_clip_path(
    &self,
    clip_id: &str,
    reference_rect: Rect,
    mask_bounds_css: Rect,
  ) -> Option<String> {
    let defs = self.svg_id_defs.as_ref()?;
    let reference_width = reference_rect.width();
    let reference_height = reference_rect.height();
    if !reference_width.is_finite()
      || !reference_height.is_finite()
      || reference_width <= 0.0
      || reference_height <= 0.0
    {
      return None;
    }
    let viewbox_x = mask_bounds_css.x() - reference_rect.x();
    let viewbox_y = mask_bounds_css.y() - reference_rect.y();
    if !viewbox_x.is_finite() || !viewbox_y.is_finite() {
      return None;
    }

    let view_w = if mask_bounds_css.width().is_finite() && mask_bounds_css.width() > 0.0 {
      mask_bounds_css.width()
    } else {
      1.0
    };
    let view_h = if mask_bounds_css.height().is_finite() && mask_bounds_css.height() > 0.0 {
      mask_bounds_css.height()
    } else {
      1.0
    };

    let dpr = if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0 {
      self.device_pixel_ratio
    } else {
      1.0
    };
    let width = (view_w * dpr).ceil().max(1.0) as u32;
    let height = (view_h * dpr).ceil().max(1.0) as u32;
    crate::paint::svg_mask_image::inline_svg_for_clip_path_id_with_view_box_offset(
      defs,
      clip_id,
      reference_width,
      reference_height,
      viewbox_x,
      viewbox_y,
      view_w,
      view_h,
      width,
      height,
    )
  }

  fn decode_mask_image_url(
    &self,
    src: &crate::style::types::UrlImage,
    style: &ComputedStyle,
    bounds: Rect,
  ) -> Option<Arc<ImageData>> {
    let trimmed = trim_ascii_whitespace(&src.url);
    if let Some(id) = trimmed.strip_prefix('#') {
      let defs = self.svg_id_defs.as_ref()?;

      let view_w = if bounds.width().is_finite() && bounds.width() > 0.0 {
        bounds.width()
      } else {
        1.0
      }
      .ceil()
      .max(1.0);
      let view_h = if bounds.height().is_finite() && bounds.height() > 0.0 {
        bounds.height()
      } else {
        1.0
      }
      .ceil()
      .max(1.0);

      let dpr = if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0 {
        self.device_pixel_ratio
      } else {
        1.0
      };
      let render_w = (view_w * dpr).ceil().max(1.0) as u32;
      let render_h = (view_h * dpr).ceil().max(1.0) as u32;

      let svg = crate::paint::svg_mask_image::inline_svg_for_mask_id_with_view_box(
        defs, id, view_w, view_h, render_w, render_h,
      )?;
      let decoded = self.decode_image(
        &svg,
        Some(style),
        true,
        CrossOriginAttribute::None,
        None,
        false,
        None,
      )?;

      // The mask image should keep its intrinsic size in CSS px (for `mask-size: auto`), but be
      // rasterized at device resolution so it does not get upscaled by `device_pixel_ratio` during
      // painting.
      let mut adjusted = (*decoded).clone();
      adjusted.css_width = view_w;
      adjusted.css_height = view_h;
      return Some(Arc::new(adjusted));
    }
    self.decode_image(
      &src.url,
      Some(style),
      true,
      CrossOriginAttribute::None,
      None,
      false,
      src.override_resolution,
    )
  }

  fn decode_clip_path_url(
    &self,
    src: &str,
    style: &ComputedStyle,
    reference_rect: Rect,
    mask_bounds_css: Rect,
  ) -> Option<Arc<ImageData>> {
    let trimmed = trim_ascii_whitespace(src);
    if let Some(id) = trimmed.strip_prefix('#') {
      if let Some(svg) = self.inline_svg_for_svg_clip_path(id, reference_rect, mask_bounds_css) {
        return self.decode_image(
          &svg,
          Some(style),
          true,
          CrossOriginAttribute::None,
          None,
          false,
          None,
        );
      }
      // Fragment-only URLs (`url(#id)`) refer to in-document SVG resources. If we cannot resolve
      // the id via `svg_id_defs`, treat the clip-path as missing instead of attempting an
      // external fetch.
      return None;
    }

    // External URLs of the form `url(<document-url>#<id>)` refer to `<clipPath>` defs inside the
    // referenced SVG document. Fetch the document (via ImageCache), extract all `id`-mapped SVG
    // fragments, inline the requested id, then rasterize it into an alpha mask.
    let (doc_url, id) = trimmed.rsplit_once('#')?;
    if doc_url.is_empty() || id.is_empty() {
      return None;
    }

    let image_cache = self.image_cache.as_ref()?;
    let resolved_doc_url = image_cache.resolve_url(doc_url);
    let cached = image_cache
      .load_with_crossorigin(&resolved_doc_url, CrossOriginAttribute::None)
      .ok()?;
    if !cached.is_vector {
      return None;
    }
    let svg_markup = cached.svg_content.as_deref()?;
    let defs = crate::paint::svg_mask_image::collect_svg_id_defs_from_svg_document(svg_markup);

    let reference_width = reference_rect.width();
    let reference_height = reference_rect.height();
    if !reference_width.is_finite()
      || !reference_height.is_finite()
      || reference_width <= 0.0
      || reference_height <= 0.0
    {
      return None;
    }

    let viewbox_x = mask_bounds_css.x() - reference_rect.x();
    let viewbox_y = mask_bounds_css.y() - reference_rect.y();
    if !viewbox_x.is_finite() || !viewbox_y.is_finite() {
      return None;
    }

    let view_w = if mask_bounds_css.width().is_finite() && mask_bounds_css.width() > 0.0 {
      mask_bounds_css.width()
    } else {
      1.0
    };
    let view_h = if mask_bounds_css.height().is_finite() && mask_bounds_css.height() > 0.0 {
      mask_bounds_css.height()
    } else {
      1.0
    };

    let dpr = if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0 {
      self.device_pixel_ratio
    } else {
      1.0
    };
    let width = (view_w * dpr).ceil().max(1.0) as u32;
    let height = (view_h * dpr).ceil().max(1.0) as u32;

    let svg = crate::paint::svg_mask_image::inline_svg_for_clip_path_id_with_view_box_offset(
      &defs,
      id,
      reference_width,
      reference_height,
      viewbox_x,
      viewbox_y,
      view_w,
      view_h,
      width,
      height,
    )?;

    self.decode_image(
      &svg,
      Some(style),
      true,
      CrossOriginAttribute::None,
      None,
      false,
      None,
    )
  }

  fn resolve_mask(&self, style: &ComputedStyle, bounds: Rect) -> Option<ResolvedMask> {
    if !style.mask_layers.iter().any(|layer| layer.image.is_some()) {
      return None;
    }

    let rects = Self::background_rects(bounds, style, self.viewport);
    let rects = MaskReferenceRects {
      border: rects.border,
      padding: rects.padding,
      content: rects.content,
    };

    let mut layers = Vec::new();
    for layer in &style.mask_layers {
      let Some(image) = &layer.image else { continue };
      let resolved_image = match image {
        BackgroundImage::LinearGradient { .. }
        | BackgroundImage::RepeatingLinearGradient { .. }
        | BackgroundImage::RadialGradient { .. }
        | BackgroundImage::RepeatingRadialGradient { .. }
        | BackgroundImage::ConicGradient { .. }
        | BackgroundImage::RepeatingConicGradient { .. } => {
          ResolvedMaskImage::Generated(Box::new(image.clone()))
        }
        BackgroundImage::Url(src) => {
          let Some(image) = self.decode_mask_image_url(src, style, bounds) else {
            continue;
          };
          ResolvedMaskImage::Raster((*image).clone())
        }
        BackgroundImage::None => continue,
      };

      // `mask-mode: match-source` selects alpha for images with an alpha channel and luminance
      // otherwise. We resolve this here while we still have access to the decoded image metadata.
      // (The renderer only sees `ImageData`, which is always RGBA after decoding.)
      let resolved_mode = match layer.mode {
        MaskMode::MatchSource => match image {
          BackgroundImage::Url(src) => {
            let trimmed = trim_ascii_whitespace_start(&src.url);
            if trimmed.starts_with('#') {
              // Fragment-only URLs are resolved via `svg_id_defs` (see `decode_mask_image_url`).
              // They do not correspond to fetchable external images, so skip probing the network
              // when selecting the mask mode.
              MaskMode::Alpha
            } else if trimmed.starts_with('<') {
              // Inline SVG is rasterized to RGBA; treat it as alpha-masked.
              MaskMode::Alpha
            } else if let Some(image_cache) = self.image_cache.as_ref() {
              let resolved_src = image_cache.resolve_url(&src.url);
              match image_cache.load_with_crossorigin(&resolved_src, CrossOriginAttribute::None) {
                Ok(cached) => {
                    if cached.is_vector || cached.has_alpha {
                      MaskMode::Alpha
                    } else {
                    MaskMode::Luminance
                  }
                }
                Err(_) => MaskMode::Alpha,
              }
            } else {
              // Without the image cache we can't determine the source color type; fall back to
              // alpha so we preserve previous behavior.
              MaskMode::Alpha
            }
          }
          _ => MaskMode::Alpha,
        },
        other => other,
      };

      layers.push(ResolvedMaskLayer {
        image: resolved_image,
        repeat: layer.repeat,
        position: layer.position.clone(),
        size: layer.size.clone(),
        origin: layer.origin,
        clip: layer.clip,
        mode: resolved_mode,
        composite: layer.composite,
      });
    }

    if layers.is_empty() {
      return None;
    }

    Some(ResolvedMask {
      layers,
      text_clip: None,
      color: style.color,
      used_dark_color_scheme: style.used_dark_color_scheme,
      forced_colors: style.forced_colors,
      font_size: style.font_size,
      root_font_size: style.root_font_size,
      viewport: self.viewport,
      rects,
    })
  }

  fn resolve_mask_border(&self, style: &ComputedStyle, bounds: Rect) -> Option<ResolvedMaskBorder> {
    let bg = match &style.mask_border.source {
      BorderImageSource::Image(bg) => bg,
      BorderImageSource::None => return None,
    };

    // Resolve `mask-border-mode: match-source` using the image metadata while we still have access
    // to the `ImageCache` (the renderer only sees RGBA pixels).
    let resolved_mode = match style.mask_border.mode {
      MaskBorderMode::MatchSource => match bg.as_ref() {
        BackgroundImage::Url(src) => {
          let trimmed = trim_ascii_whitespace_start(&src.url);
          if trimmed.starts_with('#') {
            // Fragment-only URLs are resolved via `svg_id_defs` (see `decode_image`).
            MaskBorderMode::Alpha
          } else if trimmed.starts_with('<') {
            // Inline SVG is rasterized to RGBA; treat it as alpha-masked.
            MaskBorderMode::Alpha
          } else if let Some(image_cache) = self.image_cache.as_ref() {
            let resolved_src = image_cache.resolve_url(&src.url);
            match image_cache.load_with_crossorigin(&resolved_src, CrossOriginAttribute::None) {
              Ok(cached) => {
                if cached.is_vector || cached.has_alpha {
                  MaskBorderMode::Alpha
                } else {
                  MaskBorderMode::Luminance
                }
              }
              Err(_) => MaskBorderMode::Alpha,
            }
          } else {
            // Without the image cache we can't determine the source color type; fall back to alpha.
            MaskBorderMode::Alpha
          }
        }
        _ => MaskBorderMode::Alpha,
      },
      other => other,
    };

    let source = match bg.as_ref() {
      BackgroundImage::Url(src) => self
        .decode_image(
          &src.url,
          Some(style),
          true,
          CrossOriginAttribute::None,
          None,
          false,
          src.override_resolution,
        )
        .map(|image| BorderImageSourceItem::Raster((*image).clone())),
      BackgroundImage::LinearGradient { .. }
      | BackgroundImage::RepeatingLinearGradient { .. }
      | BackgroundImage::RadialGradient { .. }
      | BackgroundImage::RepeatingRadialGradient { .. }
      | BackgroundImage::ConicGradient { .. }
      | BackgroundImage::RepeatingConicGradient { .. } => {
        Some(BorderImageSourceItem::Generated(Box::new((**bg).clone())))
      }
      BackgroundImage::None => None,
    }?;

    // Used border widths are needed to resolve `<number>` values in mask-border-width/outset.
    let percentage_base = bounds.width().max(0.0);
    let font_size = style.font_size;
    let border_widths = MaskBorderWidths {
      top: Self::resolve_length_for_paint(
        &style.used_border_top_width(),
        font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      ),
      right: Self::resolve_length_for_paint(
        &style.used_border_right_width(),
        font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      ),
      bottom: Self::resolve_length_for_paint(
        &style.used_border_bottom_width(),
        font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      ),
      left: Self::resolve_length_for_paint(
        &style.used_border_left_width(),
        font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      ),
    };

    Some(ResolvedMaskBorder {
      source,
      slice: style.mask_border.slice.clone(),
      width: style.mask_border.width.clone(),
      outset: style.mask_border.outset.clone(),
      repeat: style.mask_border.repeat,
      mode: resolved_mode,
      rect: bounds,
      border_widths,
      current_color: style.color,
      used_dark_color_scheme: style.used_dark_color_scheme,
      forced_colors: style.forced_colors,
      font_size: style.font_size,
      root_font_size: style.root_font_size,
      viewport: self.viewport,
    })
  }

  fn resolve_border_radii(
    style: Option<&ComputedStyle>,
    bounds: Rect,
    viewport: Option<(f32, f32)>,
  ) -> crate::paint::display_list::BorderRadii {
    let Some(style) = style else {
      return crate::paint::display_list::BorderRadii::ZERO;
    };
    if Self::border_radius_is_zero(style) {
      return crate::paint::display_list::BorderRadii::ZERO;
    }
    let w = bounds.width().max(0.0);
    let h = bounds.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return crate::paint::display_list::BorderRadii::ZERO;
    }

    let resolve_radius = |len: &Length, reference: f32| -> f32 {
      let resolved = Self::resolve_length_for_paint(
        len,
        style.font_size,
        style.root_font_size,
        reference,
        viewport,
      );
      if resolved.is_finite() {
        resolved.max(0.0)
      } else {
        0.0
      }
    };

    crate::paint::display_list::BorderRadii {
      top_left: crate::paint::display_list::BorderRadius {
        x: resolve_radius(&style.border_top_left_radius.x, w),
        y: resolve_radius(&style.border_top_left_radius.y, h),
      },
      top_right: crate::paint::display_list::BorderRadius {
        x: resolve_radius(&style.border_top_right_radius.x, w),
        y: resolve_radius(&style.border_top_right_radius.y, h),
      },
      bottom_right: crate::paint::display_list::BorderRadius {
        x: resolve_radius(&style.border_bottom_right_radius.x, w),
        y: resolve_radius(&style.border_bottom_right_radius.y, h),
      },
      bottom_left: crate::paint::display_list::BorderRadius {
        x: resolve_radius(&style.border_bottom_left_radius.x, w),
        y: resolve_radius(&style.border_bottom_left_radius.y, h),
      },
    }
    .clamped(w, h)
  }

  fn resolve_clip_radii(
    style: &ComputedStyle,
    rects: &BackgroundRects,
    clip: BackgroundBox,
    viewport: Option<(f32, f32)>,
    breakdown: Option<&BuildBreakdown>,
  ) -> crate::paint::display_list::BorderRadii {
    let timer = breakdown.map(|_| Instant::now());
    let base = Self::resolve_border_radii(Some(style), rects.border, viewport);
    if base.is_zero() {
      if let (Some(breakdown), Some(start)) = (breakdown, timer) {
        breakdown.record_border_radii(start.elapsed());
      }
      return base;
    }

    let percentage_base = rects.border.width().max(0.0);
    let font_size = style.font_size;
    let border_left = Self::resolve_length_for_paint(
      &style.used_border_left_width(),
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );
    let border_right = Self::resolve_length_for_paint(
      &style.used_border_right_width(),
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );
    let border_top = Self::resolve_length_for_paint(
      &style.used_border_top_width(),
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );
    let border_bottom = Self::resolve_length_for_paint(
      &style.used_border_bottom_width(),
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );

    let padding_left = Self::resolve_length_for_paint(
      &style.padding_left,
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );
    let padding_right = Self::resolve_length_for_paint(
      &style.padding_right,
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );
    let padding_top = Self::resolve_length_for_paint(
      &style.padding_top,
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );
    let padding_bottom = Self::resolve_length_for_paint(
      &style.padding_bottom,
      font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    );

    let out = match clip {
      BackgroundBox::BorderBox => base,
      BackgroundBox::PaddingBox => {
        let shrunk = crate::paint::display_list::BorderRadii {
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
      BackgroundBox::ContentBox | BackgroundBox::Text => {
        let shrink_left = border_left + padding_left;
        let shrink_right = border_right + padding_right;
        let shrink_top = border_top + padding_top;
        let shrink_bottom = border_bottom + padding_bottom;
        let shrunk = crate::paint::display_list::BorderRadii {
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
    };

    if let (Some(breakdown), Some(start)) = (breakdown, timer) {
      breakdown.record_border_radii(start.elapsed());
    }
    out
  }

  fn normalize_color_stops(
    stops: &[ColorStop],
    current_color: Rgba,
    gradient_length: f32,
    font_size: f32,
    root_font_size: f32,
    viewport: Option<(f32, f32)>,
    is_dark: bool,
    forced_colors: bool,
  ) -> Vec<(f32, Rgba)> {
    // CSS Images 3: Gradient color stop “fixup”.
    //
    // https://www.w3.org/TR/css-images-3/#color-stop-fixup
    //
    // Notably, stop positions are not clamped to the [0%, 100%] range; they may appear anywhere on
    // the infinite gradient line (e.g. `-50%`, `150%`).
    if stops.is_empty() {
      return Vec::new();
    }

    let gradient_length = if gradient_length.is_finite() && gradient_length > 0.0 {
      gradient_length
    } else {
      0.0
    };
    let (vw, vh) = viewport.unwrap_or((0.0, 0.0));

    let mut positions: Vec<Option<f32>> = stops
      .iter()
      .map(|stop| match stop.position {
        Some(crate::css::types::ColorStopPosition::Fraction(v)) => Some(v),
        Some(crate::css::types::ColorStopPosition::Length(len)) => {
          if gradient_length <= 0.0 {
            None
          } else {
            len
              .resolve_with_context(Some(gradient_length), vw, vh, font_size, root_font_size)
              .map(|px| px / gradient_length)
          }
        }
        None => None,
      })
      .collect();

    // If no stops had positions at all, evenly distribute them from 0%..100%.
    if positions.iter().all(|p| p.is_none()) {
      if stops.len() == 1 {
        return vec![(
          0.0,
          stops[0].color.to_rgba_with_scheme_and_forced_colors(
            current_color,
            is_dark,
            forced_colors,
          ),
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

    // Pair the resolved positions with colors, keeping the result monotonic.
    let mut output = Vec::with_capacity(stops.len());
    let mut prev = f32::NEG_INFINITY;
    for (stop, pos_opt) in stops.iter().zip(positions.iter()) {
      let pos = pos_opt.unwrap_or(prev);
      let used = if pos < prev { prev } else { pos };
      prev = used;
      output.push((
        used,
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
    viewport: Option<(f32, f32)>,
    is_dark: bool,
    forced_colors: bool,
  ) -> Vec<(f32, Rgba)> {
    Self::normalize_color_stops(
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

  fn gradient_stops(stops: &[(f32, Rgba)]) -> Vec<GradientStop> {
    stops
      .iter()
      .map(|(pos, color)| GradientStop {
        position: *pos,
        color: *color,
      })
      .collect()
  }

  fn gradient_stops_unclamped(stops: &[(f32, Rgba)]) -> Vec<GradientStop> {
    Self::gradient_stops(stops)
  }

  fn radial_geometry(
    rect: Rect,
    position: &BackgroundPosition,
    size: &RadialGradientSize,
    shape: RadialGradientShape,
    font_size: f32,
    root_font_size: f32,
    viewport: Option<(f32, f32)>,
  ) -> (f32, f32, f32, f32) {
    let (align_x, off_x, align_y, off_y) = match position {
      BackgroundPosition::Position { x, y } => {
        let ox = Self::resolve_length_for_paint(
          &x.offset,
          font_size,
          root_font_size,
          rect.width(),
          viewport,
        );
        let oy = Self::resolve_length_for_paint(
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

    let (mut rx, mut ry) = match size {
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
          Self::resolve_length_for_paint(x, font_size, root_font_size, rect.width(), viewport)
            .max(0.0);
        let ry = y
          .as_ref()
          .map(|yy| {
            Self::resolve_length_for_paint(yy, font_size, root_font_size, rect.height(), viewport)
              .max(0.0)
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
        let avg = (rx * rx + ry * ry) / 2.0;
        avg.sqrt()
      } else {
        rx.min(ry)
      };
      rx = r;
      ry = r;
    }

    (cx, cy, rx, ry)
  }

  fn resolve_gradient_center(
    rect: Rect,
    position: &BackgroundPosition,
    font_size: f32,
    root_font_size: f32,
    viewport: Option<(f32, f32)>,
  ) -> Point {
    let (align_x, off_x, align_y, off_y) = match position {
      BackgroundPosition::Position { x, y } => {
        let ox = Self::resolve_length_for_paint(
          &x.offset,
          font_size,
          root_font_size,
          rect.width(),
          viewport,
        );
        let oy = Self::resolve_length_for_paint(
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
    Point::new(cx, cy)
  }

  fn resolve_filters(
    filters: &[crate::style::types::FilterFunction],
    style: &ComputedStyle,
    viewport: Option<(f32, f32)>,
    font_ctx: &FontContext,
    svg_filters: &mut SvgFilterResolver,
    breakdown: Option<&BuildBreakdown>,
  ) -> Vec<ResolvedFilter> {
    let viewport = viewport.unwrap_or((0.0, 0.0));
    filters
      .iter()
      .filter_map(|f| match f {
        crate::style::types::FilterFunction::Blur(len) => {
          let radius = Self::resolve_filter_length(len, style, viewport, font_ctx)?;
          (radius >= 0.0).then_some(ResolvedFilter::Blur(radius))
        }
        crate::style::types::FilterFunction::Brightness(v) => {
          Some(ResolvedFilter::Brightness((*v).max(0.0)))
        }
        crate::style::types::FilterFunction::Contrast(v) => {
          Some(ResolvedFilter::Contrast((*v).max(0.0)))
        }
        crate::style::types::FilterFunction::Grayscale(v) => {
          Some(ResolvedFilter::Grayscale(v.clamp(0.0, 1.0)))
        }
        crate::style::types::FilterFunction::Sepia(v) => {
          Some(ResolvedFilter::Sepia(v.clamp(0.0, 1.0)))
        }
        crate::style::types::FilterFunction::Saturate(v) => {
          Some(ResolvedFilter::Saturate((*v).max(0.0)))
        }
        crate::style::types::FilterFunction::HueRotate(deg) => {
          Some(ResolvedFilter::HueRotate(*deg))
        }
        crate::style::types::FilterFunction::Invert(v) => {
          Some(ResolvedFilter::Invert(v.clamp(0.0, 1.0)))
        }
        crate::style::types::FilterFunction::Opacity(v) => {
          Some(ResolvedFilter::Opacity(v.clamp(0.0, 1.0)))
        }
        crate::style::types::FilterFunction::DropShadow(shadow) => {
          let color = match shadow.color {
            crate::style::types::FilterColor::CurrentColor => style.color,
            crate::style::types::FilterColor::Color(c) => c,
          };
          let offset_x = Self::resolve_filter_length(&shadow.offset_x, style, viewport, font_ctx)?;
          let offset_y = Self::resolve_filter_length(&shadow.offset_y, style, viewport, font_ctx)?;
          let blur_radius =
            Self::resolve_filter_length(&shadow.blur_radius, style, viewport, font_ctx)?;
          if blur_radius < 0.0 {
            return None;
          }
          let spread = Self::resolve_filter_length(&shadow.spread, style, viewport, font_ctx)?;
          Some(ResolvedFilter::DropShadow {
            offset_x,
            offset_y,
            blur_radius,
            spread,
            color,
          })
        }
        crate::style::types::FilterFunction::Url(url) => {
          let timer = breakdown.map(|_| Instant::now());
          let resolved = svg_filters.resolve(url);
          if let (Some(breakdown), Some(start)) = (breakdown, timer) {
            breakdown.record_svg_filter(start.elapsed());
          }
          resolved.map(ResolvedFilter::SvgFilter)
        }
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

  fn compute_background_size(
    layer: &BackgroundLayer,
    font_size: f32,
    root_font_size: f32,
    viewport: Option<(f32, f32)>,
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
            BackgroundSizeComponent::Length(len) => Some(Self::resolve_length_for_paint(
              &len,
              font_size,
              root_font_size,
              area,
              viewport,
            ))
            .map(|v| v.max(0.0)),
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
              if let Some(w) = natural_w {
                (w, (w / ratio).max(0.0))
              } else if let Some(h) = natural_h {
                ((h * ratio).max(0.0), h)
              } else {
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
              }
            } else if let Some(w) = natural_w {
              (w, area_h.max(0.0))
            } else if let Some(h) = natural_h {
              (area_w.max(0.0), h)
            } else {
              (area_w.max(0.0), area_h.max(0.0))
            }
          }
        }
      }
    }
  }

  fn svg_intrinsic_dimensions_for_css(
    svg_markup: &str,
    font_size: f32,
    root_font_size: f32,
  ) -> crate::svg::SvgIntrinsicDimensions {
    // Use the same SVG intrinsic-size rules as `ImageCache` probing/rendering: resolve `<svg>`
    // width/height when they are absolute lengths, ignore percentages, and keep viewBox-only SVGs
    // as having no intrinsic size (only an intrinsic ratio).
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let svg_markup = crate::svg::svg_markup_for_roxmltree(svg_markup);
      let doc = roxmltree::Document::parse(svg_markup.as_ref()).ok()?;
      let root = doc.root_element();
      if !root.tag_name().name().eq_ignore_ascii_case("svg") {
        return None;
      }
      let intrinsic = crate::svg::svg_intrinsic_dimensions_from_attributes(
        root.attribute("width"),
        root.attribute("height"),
        root.attribute("viewBox"),
        root.attribute("preserveAspectRatio"),
        font_size,
        root_font_size,
      );
      Some(intrinsic)
    }))
    .ok()
    .flatten()
    .unwrap_or(crate::svg::SvgIntrinsicDimensions {
      width: None,
      height: None,
      aspect_ratio: None,
      aspect_ratio_none: false,
    })
  }

  fn resolve_background_offset(
    pos: BackgroundPosition,
    area_w: f32,
    area_h: f32,
    tile_w: f32,
    tile_h: f32,
    font_size: f32,
    root_font_size: f32,
    viewport: Option<(f32, f32)>,
  ) -> (f32, f32) {
    let resolve_axis =
      |comp: crate::style::types::BackgroundPositionComponent, area: f32, tile: f32| -> f32 {
        let available = area - tile;
        let needs_viewport = comp.offset.unit.is_viewport_relative()
          || comp
            .offset
            .calc
            .as_ref()
            .map(|c| c.has_viewport_relative())
            .unwrap_or(false);
        let (vw, vh) = match viewport {
          Some(vp) => vp,
          None if needs_viewport => (f32::NAN, f32::NAN),
          None => (0.0, 0.0),
        };
        let offset = comp
          .offset
          .resolve_with_context(Some(available), vw, vh, font_size, root_font_size)
          .unwrap_or_else(|| {
            if comp.offset.unit.is_absolute() {
              comp.offset.to_px()
            } else {
              0.0
            }
          });
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

  fn tile_axis_plan(
    repeat: BackgroundRepeatKeyword,
    area_start: f32,
    area_len: f32,
    tile_len: f32,
    offset: f32,
    clip_min: f32,
    clip_max: f32,
  ) -> TileAxisPlan {
    if !tile_len.is_finite() || tile_len <= 0.0 {
      return TileAxisPlan::empty();
    }

    if !area_start.is_finite()
      || !area_len.is_finite()
      || !offset.is_finite()
      || !clip_min.is_finite()
      || !clip_max.is_finite()
    {
      return TileAxisPlan::empty();
    }

    let plan_with_step = |start: f32, step: f32| -> TileAxisPlan {
      if !start.is_finite() || !step.is_finite() || step <= 0.0 {
        return TileAxisPlan::empty();
      }

      let span = clip_max - start;
      if !span.is_finite() || span <= 0.0 {
        return TileAxisPlan::empty();
      }

      let raw = (span / step).ceil();
      if !raw.is_finite() || raw <= 0.0 {
        return TileAxisPlan::empty();
      }

      TileAxisPlan {
        start,
        step,
        count: raw as usize,
      }
    };

    match repeat {
      BackgroundRepeatKeyword::NoRepeat => {
        let start = area_start + offset;
        if !start.is_finite() {
          return TileAxisPlan::empty();
        }
        TileAxisPlan {
          start,
          step: tile_len,
          count: 1,
        }
      }
      BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round => {
        let start = Self::aligned_start(area_start + offset, tile_len, clip_min);
        plan_with_step(start, tile_len)
      }
      BackgroundRepeatKeyword::Space => {
        let count = (area_len / tile_len).floor() as i32;
        if count >= 2 {
          let spacing = (area_len - tile_len * count as f32) / (count as f32 - 1.0);
          let step = tile_len + spacing;
          let anchor = area_start;
          let k = ((clip_min - anchor) / step).floor();
          let start = anchor + k * step;
          plan_with_step(start, step)
        } else {
          let centered = area_start + offset + (area_len - tile_len) * 0.5;
          if !centered.is_finite() {
            return TileAxisPlan::empty();
          }
          TileAxisPlan {
            start: centered,
            step: tile_len,
            count: 1,
          }
        }
      }
    }
  }

  fn resolve_length_for_paint(
    len: &Length,
    font_size: f32,
    root_font_size: f32,
    percentage_base: f32,
    viewport: Option<(f32, f32)>,
  ) -> f32 {
    crate::paint::paint_bounds::resolve_length_for_paint(
      len,
      font_size,
      root_font_size,
      percentage_base,
      viewport,
    )
  }

  #[inline]
  fn border_radius_is_zero(style: &ComputedStyle) -> bool {
    style.border_top_left_radius.x.is_zero()
      && style.border_top_left_radius.y.is_zero()
      && style.border_top_right_radius.x.is_zero()
      && style.border_top_right_radius.y.is_zero()
      && style.border_bottom_right_radius.x.is_zero()
      && style.border_bottom_right_radius.y.is_zero()
      && style.border_bottom_left_radius.x.is_zero()
      && style.border_bottom_left_radius.y.is_zero()
  }

  fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
    let new_x = rect.x() + left;
    let new_y = rect.y() + top;
    let new_w = (rect.width() - left - right).max(0.0);
    let new_h = (rect.height() - top - bottom).max(0.0);
    Rect::from_xywh(new_x, new_y, new_w, new_h)
  }

  fn border_radii(rect: Rect, style: &ComputedStyle) -> crate::paint::display_list::BorderRadii {
    Self::resolve_border_radii(Some(style), rect, None)
  }

  fn border_style_visible(style: crate::style::types::BorderStyle) -> bool {
    !matches!(
      style,
      crate::style::types::BorderStyle::None | crate::style::types::BorderStyle::Hidden
    )
  }

  fn border_side_visible(side: &BorderSide) -> bool {
    side.width > 0.0 && Self::border_style_visible(side.style) && !side.color.is_transparent()
  }

  fn transform_reference_box(
    style: &ComputedStyle,
    bounds: Rect,
    viewport: Option<(f32, f32)>,
  ) -> Rect {
    let rects = Self::background_rects(bounds, style, viewport);
    match style.transform_box {
      TransformBox::ContentBox => rects.content,
      TransformBox::BorderBox
      | TransformBox::FillBox
      | TransformBox::StrokeBox
      | TransformBox::ViewBox => rects.border,
    }
  }

  fn build_transform(
    style: &ComputedStyle,
    bounds: Rect,
    viewport: Option<(f32, f32)>,
  ) -> ResolvedTransforms {
    crate::paint::transform_resolver::resolve_transforms(style, bounds, viewport)
  }

  /// Exposes transform resolution for debugging/inspection tools.
  pub fn debug_resolve_transform(
    style: &ComputedStyle,
    bounds: Rect,
    viewport: Option<(f32, f32)>,
  ) -> Option<Transform3D> {
    crate::paint::transform_resolver::resolve_transform3d(style, bounds, viewport)
  }

  fn emit_fragment_list(
    &mut self,
    fragments: &[FragmentNode],
    offset: Point,
    visibility: Visibility,
  ) {
    let len = fragments.len();
    let total = self.estimated_fragments.unwrap_or(len);
    if self.parallel_enabled
      && !matches!(self.parallel_mode, PaintParallelismMode::Disabled)
      && len >= self.parallel_min
      && total >= self.parallel_root_min
    {
      let paint_pool = crate::paint::paint_thread_pool::paint_pool();
      let pool = paint_pool.pool;
      let threads = paint_pool.threads;
      if let Some(plan) = self.plan_parallel(len, threads) {
        let start = self.parallel_stats.as_ref().map(|_| Instant::now());
        let deadline = active_deadline();
        let root_deadline = crate::render_control::root_deadline();
        let stage = active_stage();
        let stage_heartbeat = active_stage_heartbeat();
        let allocation_budget = active_allocation_budget();
        let diagnostics_session = paint_diagnostics_session_id();
        let run_build = || -> Vec<(usize, DisplayList, ThreadId)> {
          fragments
            .par_chunks(plan.chunk_size)
            .enumerate()
            .map(|(chunk_idx, chunk)| {
              let chunk_offset = chunk_idx * plan.chunk_size;
              let _diagnostics_guard = diagnostics_session.map(PaintDiagnosticsThreadGuard::enter);
              let deadline = deadline.clone();
              let root_deadline = root_deadline.clone();
              with_allocation_budget(allocation_budget.as_ref(), || {
                let _heartbeat_guard = StageHeartbeatGuard::install(stage_heartbeat);
                with_deadline(root_deadline.as_ref(), || {
                  with_deadline(deadline.as_ref(), || {
                    let _stage_guard = StageGuard::install(stage);
                    let mut builder = self.fork();
                    let mut counter = 0usize;
                    for fragment in chunk {
                      if builder.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
                        break;
                      }
                      builder.build_fragment(fragment, offset, visibility);
                    }
                    (chunk_offset, builder.list, std::thread::current().id())
                  })
                })
              })
            })
            .collect()
        };
        let mut partials: Vec<(usize, DisplayList, ThreadId)> = if let Some(pool) = pool {
          pool.install(run_build)
        } else {
          run_build()
        };
        if let (Some(stats), Some(start)) = (self.parallel_stats.as_ref(), start) {
          stats.record_parallel(
            partials.len(),
            start.elapsed(),
            partials.iter().map(|(_, _, thread)| *thread),
          );
        }
        partials.sort_by_key(|(idx, _, _)| *idx);
        for (_, list, _) in partials {
          self.list.append(list);
        }
        return;
      }
    }

    let serial_start = self.parallel_stats.as_ref().map(|_| Instant::now());
    let mut counter = 0usize;
    for fragment in fragments {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      self.build_fragment(fragment, offset, visibility);
    }
    if let (Some(stats), Some(start)) = (self.parallel_stats.as_ref(), serial_start) {
      stats.record_serial(start.elapsed());
    }
  }

  fn emit_fragment_list_shallow(
    &mut self,
    fragments: &[FragmentNode],
    offset: Point,
    suppress_opacity: bool,
    visibility: Visibility,
    clip_rect_already_pushed: bool,
  ) {
    let len = fragments.len();
    let total = self.estimated_fragments.unwrap_or(len);
    if self.parallel_enabled
      && !matches!(self.parallel_mode, PaintParallelismMode::Disabled)
      && len >= self.parallel_min
      && total >= self.parallel_root_min
    {
      let paint_pool = crate::paint::paint_thread_pool::paint_pool();
      let pool = paint_pool.pool;
      let threads = paint_pool.threads;
      if let Some(plan) = self.plan_parallel(len, threads) {
        let start = self.parallel_stats.as_ref().map(|_| Instant::now());
        let deadline = active_deadline();
        let root_deadline = crate::render_control::root_deadline();
        let stage = active_stage();
        let stage_heartbeat = active_stage_heartbeat();
        let allocation_budget = active_allocation_budget();
        let diagnostics_session = paint_diagnostics_session_id();
        let run_build = || -> Vec<(usize, DisplayList, ThreadId)> {
          fragments
            .par_chunks(plan.chunk_size)
            .enumerate()
            .map(|(chunk_idx, chunk)| {
              let chunk_offset = chunk_idx * plan.chunk_size;
              let _diagnostics_guard = diagnostics_session.map(PaintDiagnosticsThreadGuard::enter);
              let deadline = deadline.clone();
              let root_deadline = root_deadline.clone();
              with_allocation_budget(allocation_budget.as_ref(), || {
                let _heartbeat_guard = StageHeartbeatGuard::install(stage_heartbeat);
                with_deadline(root_deadline.as_ref(), || {
                  with_deadline(deadline.as_ref(), || {
                    let _stage_guard = StageGuard::install(stage);
                    let mut builder = self.fork();
                    let mut counter = 0usize;
                    for fragment in chunk {
                      if builder.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
                        break;
                      }
                      if suppress_opacity {
                        builder.build_fragment_internal(
                          fragment,
                          offset,
                          false,
                          true,
                          visibility,
                          clip_rect_already_pushed,
                        );
                      } else {
                        builder.build_fragment_shallow(
                          fragment,
                          offset,
                          visibility,
                          clip_rect_already_pushed,
                        );
                      }
                    }
                    (chunk_offset, builder.list, std::thread::current().id())
                  })
                })
              })
            })
            .collect()
        };
        let mut partials: Vec<(usize, DisplayList, ThreadId)> = if let Some(pool) = pool {
          pool.install(run_build)
        } else {
          run_build()
        };
        if let (Some(stats), Some(start)) = (self.parallel_stats.as_ref(), start) {
          stats.record_parallel(
            partials.len(),
            start.elapsed(),
            partials.iter().map(|(_, _, thread)| *thread),
          );
        }
        partials.sort_by_key(|(idx, _, _)| *idx);
        for (_, list, _) in partials {
          self.list.append(list);
        }
        return;
      }
    }

    let serial_start = self.parallel_stats.as_ref().map(|_| Instant::now());
    let mut counter = 0usize;
    for fragment in fragments {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      if suppress_opacity {
        self.build_fragment_internal(
          fragment,
          offset,
          false,
          true,
          visibility,
          clip_rect_already_pushed,
        );
      } else {
        self.build_fragment_shallow(fragment, offset, visibility, clip_rect_already_pushed);
      }
    }
    if let (Some(stats), Some(start)) = (self.parallel_stats.as_ref(), serial_start) {
      stats.record_serial(start.elapsed());
    }
  }

  fn plan_parallel(&self, len: usize, threads: usize) -> Option<ParallelPlan> {
    let threads = threads.max(1);
    if matches!(self.parallel_mode, PaintParallelismMode::Disabled)
      || !self.parallel_enabled
      || len < self.parallel_min
      || threads <= 1
    {
      return None;
    }
    let total = self.estimated_fragments.unwrap_or(len);
    let root_threshold = if self.parallel_min_explicit {
      self.parallel_root_min
    } else {
      self
        .parallel_root_min
        .max(self.parallel_min.saturating_mul(threads))
    };
    if total < root_threshold {
      return None;
    }
    let max_tasks = threads.saturating_mul(4).max(2);
    let estimated_tasks = (total / self.parallel_min).max(1);
    let target_tasks = estimated_tasks.clamp(2, max_tasks);
    let chunk_size = ((len + target_tasks - 1) / target_tasks).max(self.parallel_min);
    if len <= chunk_size {
      return None;
    }
    Some(ParallelPlan { chunk_size })
  }

  fn record_parallel_diagnostics(&self) {
    let Some(stats) = &self.parallel_stats else {
      return;
    };
    let snapshot = stats.snapshot();
    if snapshot.tasks == 0 && snapshot.parallel_ns == 0 && snapshot.serial_ns == 0 {
      return;
    }
    with_paint_diagnostics(|diag| {
      diag.parallel_tasks += snapshot.tasks;
      diag.parallel_threads = diag.parallel_threads.max(snapshot.threads);
      diag.parallel_ms += snapshot.parallel_ns as f64 / 1_000_000.0;
      diag.serial_ms += snapshot.serial_ns as f64 / 1_000_000.0;
    });
  }

  fn fork(&self) -> DisplayListBuilder {
    DisplayListBuilder {
      list: DisplayList::new(),
      image_cache: self.image_cache.clone(),
      saw_gif_image: Arc::clone(&self.saw_gif_image),
      saw_animation_time_dependent_image: Arc::clone(&self.saw_animation_time_dependent_image),
      media_provider: self.media_provider.clone(),
      decoded_image_cache: Arc::clone(&self.decoded_image_cache),
      svg_filter_defs: self.svg_filter_defs.clone(),
      svg_id_defs: self.svg_id_defs.clone(),
      svg_id_defs_raw: self.svg_id_defs_raw.clone(),
      appearance_none_form_controls: self.appearance_none_form_controls.clone(),
      viewport: self.viewport,
      culling_viewport: self.culling_viewport,
      font_ctx: self.font_ctx.clone(),
      shaper: self.shaper.clone(),
      device_pixel_ratio: self.device_pixel_ratio,
      parallel_enabled: self.parallel_enabled,
      parallel_mode: self.parallel_mode,
      parallel_min: self.parallel_min,
      parallel_root_min: self.parallel_root_min,
      parallel_min_explicit: self.parallel_min_explicit,
      parallel_stats: self.parallel_stats.clone(),
      build_breakdown: self.build_breakdown.clone(),
      background_tiles: self.background_tiles.clone(),
      background_layers: self.background_layers.clone(),
      background_pattern_fast_paths: self.background_pattern_fast_paths.clone(),
      canvas_background_suppress_box_id: self.canvas_background_suppress_box_id,
      canvas_border_suppress_box_id: self.canvas_border_suppress_box_id,
      estimated_fragments: self.estimated_fragments,
      scroll_state: self.scroll_state.clone(),
      max_iframe_depth: self.max_iframe_depth,
      skip_stacking_context_children: self.skip_stacking_context_children,
      line_decoration_ctx: self.line_decoration_ctx,
      error: self.error.clone(),
    }
  }

  /// Emits display items for fragment content
  fn emit_content(&mut self, fragment: &FragmentNode, rect: Rect, culling_rect: Option<Rect>) {
    match &fragment.content {
      FragmentContent::Text {
        text,
        baseline_offset,
        shaped,
        is_marker,
        emphasis_offset,
        document_selection,
        ..
      } => {
        if text.is_empty() {
          return;
        }

        let style_opt = fragment.style.as_deref();
        let current = style_opt.map(|s| s.color).unwrap_or(Rgba::BLACK);
        let color = style_opt
          .map(|s| {
            s.webkit_text_fill_color
              .to_rgba_with_scheme_and_forced_colors(
                current,
                s.used_dark_color_scheme,
                s.forced_colors,
              )
          })
          .unwrap_or(current);
        let shadows = Self::text_shadows_from_style(style_opt, self.viewport);
        let inline_vertical = style_opt.is_some_and(|s| {
          matches!(
            s.writing_mode,
            crate::style::types::WritingMode::VerticalRl
              | crate::style::types::WritingMode::VerticalLr
              | crate::style::types::WritingMode::SidewaysRl
              | crate::style::types::WritingMode::SidewaysLr
          )
        });
        let (baseline_block, baseline_inline) = if inline_vertical {
          (rect.origin.x + baseline_offset, rect.origin.y)
        } else {
          (rect.origin.y + baseline_offset, rect.origin.x)
        };

        let mut shaped_storage: Option<Vec<ShapedRun>> = None;
        let runs_ref: Option<&[ShapedRun]> = if let Some(runs) = shaped {
          Some(runs.as_ref())
        } else if let Some(style) = style_opt {
          let shape_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
          let shaped_result = self.shaper.shape(text, style, &self.font_ctx);
          if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), shape_timer) {
            breakdown.record_text_shape(start.elapsed());
          }
          if let Ok(mut runs) = shaped_result {
            InlineTextItem::apply_spacing_to_runs(
              &mut runs,
              text,
              style.letter_spacing,
              style.word_spacing,
            );
            shaped_storage = Some(runs);
          }
          shaped_storage.as_deref()
        } else {
          None
        };

        if let Some(selection_ranges) = document_selection.as_deref() {
          let selection_color = Rgba {
            r: 0,
            g: 120,
            b: 215,
            a: 0.35,
          };
          let runs: &[ShapedRun] = runs_ref.unwrap_or(&[]);
          for range in selection_ranges.iter() {
            if range.start >= range.end {
              continue;
            }
            for (x1, x2) in crate::text::caret::selection_segments_for_char_range(
              text,
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
              let sel_rect = if let Some(cull) = culling_rect {
                sel_rect.intersection(cull)
              } else {
                Some(sel_rect)
              };
              if let Some(sel_rect) =
                sel_rect.filter(|r| r.width() > f32::EPSILON && r.height() > f32::EPSILON)
              {
                self.list.push(DisplayItem::FillRect(FillRectItem {
                  rect: sel_rect,
                  color: selection_color,
                }));
              }
            }
          }
        }

        if let Some(runs) = runs_ref {
          if runtime::runtime_toggles().truthy("FASTR_TEXT_METRICS_CHECK") {
            if let Some(style) = style_opt {
              if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
                let mut max_line_height = 0.0_f32;
                for run in runs {
                  if let Some(scaled) = self.font_ctx.get_scaled_metrics_with_variations(
                    run.font.as_ref(),
                    run.font_size,
                    &run.variations,
                  ) {
                    max_line_height = max_line_height.max(scaled.line_height);
                  }
                }

                let line_height = if max_line_height > 0.0 {
                  max_line_height
                } else {
                  Self::resolve_scaled_metrics(style, &self.font_ctx)
                    .map(|m| m.line_height)
                    .unwrap_or(style.font_size * 1.2)
                };

                let mut expected = InlineTextItem::metrics_from_runs(
                  &self.font_ctx,
                  runs,
                  line_height,
                  style.font_size,
                );
                InlineTextItem::apply_text_emphasis_metrics(&mut expected, style);

                let block_size = if inline_vertical {
                  rect.width()
                } else {
                  rect.height()
                };
                let baseline_delta = (expected.baseline_offset - *baseline_offset).abs();
                let block_delta = (expected.height - block_size).abs();
                let tol = 0.5;
                if baseline_delta > tol || block_delta > tol {
                  eprintln!(
                    "text metrics mismatch line-height=normal baseline_delta={baseline_delta:.2} block_delta={block_delta:.2} expected(base={:.2} h={:.2}) layout(base={:.2} h={:.2}) text={:?}",
                    expected.baseline_offset,
                    expected.height,
                    baseline_offset,
                    block_size,
                    text,
                  );
                  debug_assert!(
                    baseline_delta <= tol && block_delta <= tol,
                    "layout/paint text metric mismatch"
                  );
                }
              }
            }
          }

          if inline_vertical {
            if *is_marker {
              self.emit_list_marker_runs_vertical(
                runs,
                color,
                baseline_block,
                baseline_inline,
                &shadows,
                style_opt,
                *emphasis_offset,
              );
            } else {
              self.emit_shaped_runs_vertical(
                runs,
                color,
                baseline_block,
                baseline_inline,
                &shadows,
                style_opt,
                *emphasis_offset,
              );
            }
          } else if *is_marker {
            self.emit_list_marker_runs(
              runs,
              color,
              baseline_block,
              baseline_inline,
              &shadows,
              style_opt,
              inline_vertical,
              *emphasis_offset,
            );
          } else {
            self.emit_shaped_runs(
              runs,
              color,
              baseline_block,
              baseline_inline,
              &shadows,
              style_opt,
              inline_vertical,
              *emphasis_offset,
            );
          }
        } else {
          // Fallback: naive glyphs when shaping fails or no style is present
          let font_size = style_opt.map(|s| s.font_size).unwrap_or(16.0);
          let char_width = font_size * 0.6;
          let advance_width = if inline_vertical {
            0.0
          } else {
            text.len() as f32 * char_width
          };
          let origin = if inline_vertical {
            Point::new(baseline_block, baseline_inline)
          } else {
            Point::new(baseline_inline, baseline_block)
          };
          let mut bounds = ConservativeGlyphRunBoundsBuilder::new(origin, advance_width);
          let mut glyphs = Vec::new();
          for (i, _c) in text.chars().enumerate() {
            let (offset, advance) = if inline_vertical {
              (Point::new(0.0, i as f32 * char_width), 0.0)
            } else {
              (Point::new(i as f32 * char_width, 0.0), char_width)
            };
            bounds.include_glyph(offset.x, advance);
            glyphs.push(GlyphInstance {
              glyph_id: i as u32,
              cluster: i as u32,
              x_offset: offset.x,
              y_offset: offset.y,
              x_advance: advance,
              y_advance: 0.0,
            });
          }
          let cached_bounds = bounds.finish(font_size);
          let (stroke_width, stroke_color) = style_opt
            .map(|s| self.resolve_webkit_text_stroke_for_run(s, font_size))
            .unwrap_or_default();
          let allow_subpixel_aa = style_opt.map(|s| s.allow_subpixel_aa).unwrap_or(true);

          if *is_marker {
            let item = ListMarkerItem {
              origin,
              cached_bounds: Some(cached_bounds),
              glyphs,
              color,
              allow_subpixel_aa,
              stroke_width,
              stroke_color,
              font_smoothing: style_opt.map(|s| s.font_smoothing).unwrap_or_default(),
              shadows: shadows.clone(),
              font_size,
              advance_width,
              ..Default::default()
            };
            self.list.push(DisplayItem::ListMarker(item));
          } else {
            let item = TextItem {
              origin,
              cached_bounds: Some(cached_bounds),
              glyphs,
              color,
              allow_subpixel_aa,
              stroke_width,
              stroke_color,
              font_smoothing: style_opt.map(|s| s.font_smoothing).unwrap_or_default(),
              shadows: shadows.clone(),
              font_size,
              advance_width,
              decorations: Vec::new(),
              ..Default::default()
            };
            self.list.push(DisplayItem::Text(item));
          }
        }

        if let Some(style) = style_opt {
          let (inline_start, inline_len) = if inline_vertical {
            (rect.y(), rect.height())
          } else {
            (rect.x(), rect.width())
          };
          let decoration_baseline = self
            .line_decoration_ctx
            .map(|ctx| ctx.block_baseline)
            .unwrap_or(baseline_block);
          let clip = self.line_decoration_clip_range(inline_start, inline_len);
          self.emit_text_decorations(
            style,
            runs_ref,
            inline_start,
            inline_len,
            decoration_baseline,
            inline_vertical,
            clip,
          );
        }
      }

      FragmentContent::Replaced { replaced_type, .. } => {
        let style_for_image = fragment.style.as_deref();

        'paint: {
          if let ReplacedType::FormControl(control) = replaced_type {
            let clip_contents = style_for_image.and_then(|style| {
              let rects = Self::background_rects(rect, style, self.viewport);
              let clip_x = Self::overflow_axis_clips(style.overflow_x);
              let clip_y = Self::overflow_axis_clips(style.overflow_y);
              if clip_x || clip_y {
                let overflow_bounds = rect.union(fragment.scroll_overflow.translate(rect.origin));
                // Form controls can paint UI affordances (e.g. dropdown arrows) into the padding
                // box. Clip to the padding box (not the content box) so `overflow: clip` doesn't
                // cut them off.
                return Self::overflow_clip_from_style_with_rects(
                  style,
                  &rects,
                  clip_x,
                  clip_y,
                  overflow_bounds,
                  self.viewport,
                  self.build_breakdown.as_deref(),
                );
              }

              // Form controls should clip their internal painting by default, even when
              // `overflow` does not clip. For single-line text inputs, CSS UI specifies that the
              // inline axis is clipped to the content edge while the block axis is clipped to the
              // padding edge (so vertically-centered text can use available padding space).
              //
              // Use the content box for most controls to avoid allowing text to bleed into
              // padding (unlike `overflow: clip`, which clips to the padding box).
              let content_rect = rects.content;
              let padding_rect = rects.padding;
              let clip_rect = if matches!(&control.control, FormControlKind::Text { .. }) {
                let inline_vertical = block_axis_is_horizontal(style.writing_mode);
                if inline_vertical {
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
                }
              } else {
                content_rect
              };
              if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
                return None;
              }

              let radii = if clip_rect == content_rect {
                let radii = Self::resolve_clip_radii(
                  style,
                  &rects,
                  BackgroundBox::ContentBox,
                  self.viewport,
                  self.build_breakdown.as_deref(),
                )
                .clamped(content_rect.width(), content_rect.height());
                (!radii.is_zero()).then_some(radii)
              } else {
                // The default text-input clip rectangle mixes content/padding edges, so the usual
                // background radii do not apply cleanly. Prefer a rectangular clip here.
                None
              };

              Some(ClipItem {
                shape: ClipShape::Rect {
                  rect: clip_rect,
                  radii,
                },
              })
            });
            if let Some(clip) = clip_contents.as_ref() {
              self.list.push(DisplayItem::PushClip(clip.clone()));
            }
            // `emit_form_control` expects the border box and computes the padding/content
            // boxes internally. Passing an already-inset rect causes double insets.
            let painted = self.emit_form_control(control, fragment, rect, culling_rect);
            if clip_contents.is_some() {
              self.list.push(DisplayItem::PopClip);
            }
            if painted {
              break 'paint;
            }
          }

          if let ReplacedType::Math(math) = replaced_type {
            let fallback_style = ComputedStyle::default();
            let style_ref = style_for_image.unwrap_or(&fallback_style);
            let (content_rect, clip_radii) =
              self.replaced_content_rect_and_radii(rect, style_for_image);
            let clip_contents = Self::replaced_content_clip_item(
              style_for_image,
              content_rect,
              content_rect,
              clip_radii,
            );
            if let Some(clip) = clip_contents.as_ref() {
              self.list.push(DisplayItem::PushClip(clip.clone()));
            }
            let layout_owned = math
              .layout
              .as_ref()
              .map(|l| l.as_ref().clone())
              .unwrap_or_else(|| layout_mathml(&math.root, style_ref, &self.font_ctx));
            let base_color = style_ref.color;
            let used_dark_color_scheme = style_ref.used_dark_color_scheme;
            let shadows = Self::text_shadows_from_style(Some(style_ref), self.viewport);
            let layout_w = layout_owned.width.max(0.01);
            let layout_h = layout_owned.height.max(0.01);
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
            for frag in layout_owned.fragments {
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
                  let scaled_run = Self::scale_run(&run, scale_x, scale_y);
                  let baseline_y = content_rect.y() + origin.y * scale_y;
                  let start_x = content_rect.x() + origin.x * scale_x;
                  self.emit_shaped_runs(
                    &[scaled_run],
                    paint_color,
                    baseline_y,
                    start_x,
                    &shadows,
                    Some(style_ref),
                    false,
                    TextEmphasisOffset::default(),
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
                  self.list.push(DisplayItem::FillRect(FillRectItem {
                    rect: scaled_rect,
                    color: paint_color,
                  }));
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
                  let thickness = width * uniform;
                  if len.is_finite() && len > 0.0 && thickness.is_finite() && thickness > 0.0 {
                    let angle = dy.atan2(dx);
                    let transform = Transform3D::translate(start.x, start.y, 0.0)
                      .multiply(&Transform3D::rotate_z(angle));
                    self
                      .list
                      .push(DisplayItem::PushTransform(TransformItem { transform }));
                    self.list.push(DisplayItem::FillRect(FillRectItem {
                      rect: Rect::from_xywh(0.0, -thickness * 0.5, len, thickness),
                      color: paint_color,
                    }));
                    self.list.push(DisplayItem::PopTransform);
                  }
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
                  let stroke_width = width * uniform;
                  let scaled_radius = radius * uniform;
                  if scaled_radius > 0.0 {
                    self
                      .list
                      .push(DisplayItem::StrokeRoundedRect(StrokeRoundedRectItem {
                        rect: scaled_rect,
                        color: paint_color,
                        width: stroke_width,
                        radii: BorderRadii::uniform(scaled_radius),
                      }));
                  } else {
                    self.list.push(DisplayItem::StrokeRect(StrokeRectItem {
                      rect: scaled_rect,
                      color: paint_color,
                      width: stroke_width,
                      blend_mode: BlendMode::Normal,
                    }));
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
                  let stroke_width = width * uniform;
                  let scaled_radius_x = radius_x * scale_x;
                  let scaled_radius_y = radius_y * scale_y;
                  if scaled_radius_x > 0.0 || scaled_radius_y > 0.0 {
                    let border_radius = crate::paint::display_list::BorderRadius {
                      x: scaled_radius_x.max(0.0),
                      y: scaled_radius_y.max(0.0),
                    };
                    self
                      .list
                      .push(DisplayItem::StrokeRoundedRect(StrokeRoundedRectItem {
                        rect: scaled_rect,
                        color: paint_color,
                        width: stroke_width,
                        radii: BorderRadii {
                          top_left: border_radius,
                          top_right: border_radius,
                          bottom_right: border_radius,
                          bottom_left: border_radius,
                        },
                      }));
                  } else {
                    self.list.push(DisplayItem::StrokeRect(StrokeRectItem {
                      rect: scaled_rect,
                      color: paint_color,
                      width: stroke_width,
                      blend_mode: BlendMode::Normal,
                    }));
                  }
                }
              }
            }
            if clip_contents.is_some() {
              self.list.push(DisplayItem::PopClip);
            }
            break 'paint;
          }

          if let ReplacedType::Svg { content } = replaced_type {
            if self.emit_inline_svg(content, rect, style_for_image) {
              break 'paint;
            }
          }

          if let ReplacedType::Iframe {
            src,
            srcdoc,
            sandbox,
            referrer_policy,
            frame_token: _,
            ..
          } = replaced_type
          {
            let sandbox = *sandbox;
            let sandbox_opaque_origin = sandbox.opaque_origin();
            if let Some(cache) = self.image_cache.as_ref() {
              // When site isolation is enabled, cross-origin iframes become out-of-process and the
              // parent frame must not recursively render their content. Instead, emit a metadata
              // slot so the browser compositor can interleave the child surface at the correct
              // paint order.
              if runtime::runtime_toggles().truthy("FASTR_SITE_ISOLATION")
                && (srcdoc.is_none() || sandbox_opaque_origin)
              {
                // `srcdoc` iframes always navigate to `about:srcdoc`.
                let resolved_src = if srcdoc.is_some() {
                  "about:srcdoc".to_string()
                } else {
                  cache.resolve_url(src.as_str())
                };
                let is_about = resolved_src
                  .trim_matches(|c: char| {
                    matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
                  })
                  .to_ascii_lowercase()
                  .starts_with("about:");
                if !resolved_src.is_empty() && (!is_about || sandbox_opaque_origin) {
                  let parent_origin = cache
                    .resource_context()
                    .and_then(|ctx| ctx.policy.document_origin)
                    .or_else(|| {
                      cache
                        .base_url()
                        .as_deref()
                        .and_then(crate::resource::origin_from_url)
                    });
                  let child_origin = crate::resource::origin_from_url(&resolved_src);

                  let is_cross_origin = match (parent_origin.as_ref(), child_origin.as_ref()) {
                    (Some(parent), Some(child)) => !child.same_origin(parent),
                    // If origin data is unavailable, avoid switching iframe semantics; treat as
                    // same-origin so existing single-process behavior is preserved.
                    _ => false,
                  };

                  if sandbox_opaque_origin || is_cross_origin {
                    let (content_rect, clip_radii) =
                      self.replaced_content_rect_and_radii(rect, style_for_image);
                    let clip_item = Self::replaced_content_clip_item(
                      style_for_image,
                      content_rect,
                      content_rect,
                      clip_radii,
                    );
                    let clip = clip_item.and_then(|item| match item.shape {
                      ClipShape::Rect { rect, radii } => Some(crate::paint::display_list::RemoteFrameClip {
                        rect,
                        radii,
                      }),
                      _ => None,
                    });

                    self.list.push(DisplayItem::RemoteFrameSlot(
                      crate::paint::display_list::RemoteFrameSlotItem {
                        // Slot indices are assigned deterministically when building the final
                        // layered paint plan (after display list optimization). The builder uses a
                        // placeholder value so parallel display-list construction does not need to
                        // coordinate on a global counter.
                        slot_index: 0,
                        src: resolved_src,
                        rect: content_rect,
                        clip,
                      },
                    ));
                    break 'paint;
                  }
                }
              }

              let (content_rect, _) = self.replaced_content_rect_and_radii(rect, style_for_image);
              if let Some(image) = srcdoc.as_deref().and_then(|html| {
                render_iframe_srcdoc(
                  html,
                  Some(src.as_str()),
                  *referrer_policy,
                  content_rect,
                  style_for_image,
                  cache,
                  &self.font_ctx,
                  self.device_pixel_ratio,
                  self.max_iframe_depth,
                )
              }) {
                self.emit_iframe_image(image, rect, style_for_image);
                break 'paint;
              }

              if let Some(image) = render_iframe_src(
                src,
                *referrer_policy,
                content_rect,
                style_for_image,
                cache,
                &self.font_ctx,
                self.device_pixel_ratio,
                self.max_iframe_depth,
              ) {
                self.emit_iframe_image(image, rect, style_for_image);
                break 'paint;
              }
            }
          }

          if let ReplacedType::Video { src, .. } = replaced_type {
            if let Some(provider) = self.media_provider.as_ref() {
              let (content_rect, clip_radii) =
                self.replaced_content_rect_and_radii(rect, style_for_image);
              let size_hint = Some(crate::media::MediaFrameSizeHint::new(
                Size::new(content_rect.width().max(0.0), content_rect.height().max(0.0)),
                self.device_pixel_ratio,
              ));
              if let Some(frame) = provider.video_frame(fragment.box_id(), src, size_hint) {
                let image = frame;
                let (dest_x, dest_y, dest_w, dest_h) = {
                  let (fit, position, font_size, root_font_size) =
                    if let Some(style) = fragment.style.as_deref() {
                      (
                        style.object_fit,
                        style.object_position,
                        style.font_size,
                        style.root_font_size,
                      )
                    } else {
                      (ObjectFit::Fill, default_object_position(), 16.0, 16.0)
                    };

                  compute_object_fit(
                    fit,
                    position,
                    content_rect.width(),
                    content_rect.height(),
                    image.css_width,
                    image.css_height,
                    image.has_intrinsic_ratio,
                    font_size,
                    root_font_size,
                    self.viewport,
                  )
                  .unwrap_or_else(|| (0.0, 0.0, content_rect.width(), content_rect.height()))
                };

                let dest_rect = Rect::from_xywh(
                  content_rect.x() + dest_x,
                  content_rect.y() + dest_y,
                  dest_w,
                  dest_h,
                );

                let clip_contents = Self::replaced_content_clip_item(
                  style_for_image,
                  content_rect,
                  dest_rect,
                  clip_radii,
                );
                if let Some(clip) = clip_contents.as_ref() {
                  self.list.push(DisplayItem::PushClip(clip.clone()));
                }
                self.list.push(DisplayItem::Image(ImageItem {
                  dest_rect,
                  image,
                  filter_quality: Self::image_filter_quality(fragment.style.as_deref()),
                  src_rect: None,
                }));
                if clip_contents.is_some() {
                  self.list.push(DisplayItem::PopClip);
                }
                break 'paint;
              }
            }
          }

          let media_ctx = self.viewport.map(|(w, h)| {
            crate::style::media::MediaContext::screen(w, h)
              .with_device_pixel_ratio(self.device_pixel_ratio)
              .with_env_overrides()
          });
          let cache_base = self.image_cache.as_ref().and_then(|cache| cache.base_url());
          let (slot_rect, _) = self.replaced_content_rect_and_radii(rect, style_for_image);
          let sources = replaced_type.image_sources_with_fallback(
            crate::tree::box_tree::ImageSelectionContext {
              device_pixel_ratio: self.device_pixel_ratio,
              // Match Chrome's responsive image selection: width-descriptor srcset candidates use
              // the evaluated `sizes` attribute (or 100vw) rather than the post-layout slot width.
              //
              // Passing the layout width here can cause us to select "missing" placeholder
              // candidates in pageset fixtures where only the Chrome-chosen asset was captured.
              slot_width: None,
              viewport: self.viewport.map(|(w, h)| crate::geometry::Size::new(w, h)),
              media_context: media_ctx.as_ref(),
              font_size: fragment.style.as_deref().map(|s| s.font_size),
              root_font_size: fragment.style.as_deref().map(|s| s.root_font_size),
              base_url: cache_base.as_deref(),
            },
          );

          // Track whether this paint contains any GIF image URLs (including any `srcset` candidates)
          // so incremental paint paths can conservatively disable scroll-blitting when
          // `animation_time` affects image decoding.
          if let Some(image_cache) = self.image_cache.as_ref() {
            if let ReplacedType::Image {
              src,
              srcset,
              picture_sources,
              ..
            } = replaced_type
            {
              self.note_resolved_image_url(&image_cache.resolve_url(src));
              for candidate in srcset.iter() {
                self.note_resolved_image_url(&image_cache.resolve_url(&candidate.url));
              }
              for source in picture_sources.iter() {
                for candidate in source.srcset.iter() {
                  self.note_resolved_image_url(&image_cache.resolve_url(&candidate.url));
                }
              }
            }
          }

          let crossorigin = match replaced_type {
            ReplacedType::Image { crossorigin, .. } => *crossorigin,
            _ => CrossOriginAttribute::None,
          };
          let loading = match replaced_type {
            ReplacedType::Image { loading, .. } => *loading,
            _ => ImageLoadingAttribute::Auto,
          };
          let decoding = match replaced_type {
            ReplacedType::Image { decoding, .. } => *decoding,
            _ => ImageDecodingAttribute::Auto,
          };
          let referrer_policy = match replaced_type {
            ReplacedType::Image {
              referrer_policy, ..
            } => *referrer_policy,
            _ => None,
          };
          // `ImageCache` may return an internal 1×1 fully-transparent placeholder for non-fetchable
          // URLs (e.g. `about:blank`) as well as other "treat as missing" cases (empty body, markup
          // payloads, ...).
          //
          // For CSS `background-image` / masks, treating those as transparent is correct. For
          // replaced elements we generally want to treat placeholder images as "missing" so we can
          // fall back to UA-defined missing-content behavior (alt text for `<img>`, iframe-ish
          // fallback for `<embed>`/`<object>`, etc.).
          let reject_placeholder_image = matches!(
            replaced_type,
            ReplacedType::Image { .. } | ReplacedType::Embed { .. } | ReplacedType::Object { .. }
          );
          // `<embed>` / `<object>` are special: when the URL isn't a valid image, Chrome may still
          // render HTML content. We model that by attempting an iframe render after rejecting the
          // placeholder.
          let try_iframe_fallback = matches!(
            replaced_type,
            ReplacedType::Embed { .. } | ReplacedType::Object { .. }
          );
          // Once an `<img>`/`<picture>` candidate has been selected, browsers do not fall back to
          // the `<img src>` (or other candidates) when the chosen resource fails to decode (e.g.
          // `srcset` points at markup). Instead they render the "broken image" placeholder (+alt).
          //
          // Only attempt to decode the selected candidate (the first entry in `sources`).
          let mut deferred_async = false;
          let candidate = sources.first().copied();
          let decoded = candidate.and_then(|source| {
            // When an element is clipped by `overflow`/`clip`, `slot_rect` can be much larger than
            // the pixels that are actually visible in the current paint. Prefer the intersection
            // with the current culling rect when deciding whether to defer expensive image work.
            //
            // NOTE: `culling_rect` is already mapped into the local (pre-transform) coordinate space
            // at stacking-context boundaries (see `visible_in_local_space`), so it can safely be
            // compared against `slot_rect` even when ancestor transforms move the element into view
            // (common for carousels and centered layouts).
            let visible_slot = culling_rect
              .and_then(|vis| vis.intersection(slot_rect))
              .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0);
            let (visible_w, visible_h) = visible_slot
              .map(|rect| (rect.width(), rect.height()))
              .unwrap_or((slot_rect.width(), slot_rect.height()));

            if loading == ImageLoadingAttribute::Lazy {
              // HTML `loading="lazy"` allows deferring images that are not needed for the current
              // render. When we have culling bounds (typical viewport paints), the fragment walker
              // has already culled fully offscreen fragments, so reaching this point usually means
              // the image intersects the viewport and should be fetched/decoded like Chrome.
              //
              // When painting without culling bounds (e.g. full-content renders), treat lazy images
              // like eager images so the output is complete.
              if culling_rect.is_some() && visible_slot.is_none() {
                deferred_async = true;
                return None;
              }
            }
            if decoding == ImageDecodingAttribute::Async
              && loading != ImageLoadingAttribute::Eager
              && self.should_defer_async_image_decode(
                visible_w,
                visible_h,
                source.url,
                crossorigin,
                referrer_policy,
              )
            {
              deferred_async = true;
              return None;
            }
            self.decode_image(
              source.url,
              style_for_image,
              false,
              crossorigin,
              referrer_policy,
              reject_placeholder_image,
              source.density,
            )
          });
          if let Some(image) = decoded {
            let (content_rect, clip_radii) =
              self.replaced_content_rect_and_radii(rect, style_for_image);
            let candidate = match candidate {
              Some(c) => c,
              None => {
                self.emit_replaced_placeholder(replaced_type, fragment, rect);
                if let ReplacedType::Image { alt: Some(alt), .. } = replaced_type {
                  let _ = self.emit_alt_text(alt, fragment, rect);
                }
                break 'paint;
              }
            };

            let mut image = image;
            let (dest_x, dest_y, dest_w, dest_h) = {
              let (fit, position, font_size, root_font_size) =
                if let Some(style) = fragment.style.as_deref() {
                  (
                    style.object_fit,
                    style.object_position,
                    style.font_size,
                    style.root_font_size,
                  )
                } else {
                  (ObjectFit::Fill, default_object_position(), 16.0, 16.0)
                };

              compute_object_fit(
                fit,
                position,
                content_rect.width(),
                content_rect.height(),
                image.css_width,
                image.css_height,
                image.has_intrinsic_ratio,
                font_size,
                root_font_size,
                self.viewport,
              )
              .unwrap_or_else(|| (0.0, 0.0, content_rect.width(), content_rect.height()))
            };

            let dest_rect = Rect::from_xywh(
              content_rect.x() + dest_x,
              content_rect.y() + dest_y,
              dest_w,
              dest_h,
            );

            // SVG `<img>` needs to be rasterized at the used size so root `preserveAspectRatio`
            // letterboxing is applied in the correct viewport.
            if let Some(image_cache) = self.image_cache.as_ref() {
              let trimmed = trim_ascii_whitespace_start(candidate.url);
              let inline_svg = trimmed.starts_with('<');

              let maybe_cached = if inline_svg {
                let mut hasher = DefaultHasher::new();
                trimmed.hash(&mut hasher);
                let cache_key = format!("inline-svg:{:016x}:{}", hasher.finish(), trimmed.len());
                image_cache
                  .render_svg(trimmed)
                  .ok()
                  .map(|img| (cache_key, "inline-svg".to_string(), img))
              } else {
                let resolved_src = image_cache.resolve_url(candidate.url);
                image_cache
                  .load_with_crossorigin_and_referrer_policy(
                    &resolved_src,
                    crossorigin,
                    referrer_policy,
                  )
                  .ok()
                  .map(|img| (resolved_src.clone(), resolved_src, img))
              };

              if let Some((cache_base_url, render_url, cached)) = maybe_cached {
                if cached.is_vector {
                  if let Some(svg_markup) = cached.svg_content.as_deref() {
                    let image_resolution = style_for_image
                      .map(|s| s.image_resolution)
                      .unwrap_or_default();
                    let orientation = style_for_image
                      .map(|s| s.image_orientation.resolve(cached.orientation, false))
                      .unwrap_or_else(|| {
                        ImageOrientation::default().resolve(cached.orientation, false)
                      });
                    let used_resolution = image_resolution.used_resolution(
                      None,
                      cached.resolution,
                      self.device_pixel_ratio,
                    );

                    let base_dpr =
                      if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0 {
                        self.device_pixel_ratio
                      } else {
                        1.0
                      };

                    if dest_w.is_finite() && dest_h.is_finite() && dest_w > 0.0 && dest_h > 0.0 {
                      let render_w = (dest_w * base_dpr).ceil().max(1.0) as u32;
                      let render_h = (dest_h * base_dpr).ceil().max(1.0) as u32;

                      let cache_key = ImageKey::new(
                        format!("{cache_base_url}:{render_w}x{render_h}"),
                        crossorigin,
                        referrer_policy,
                        orientation,
                        false,
                        used_resolution,
                        self.device_pixel_ratio,
                      );

                      if let Some(existing) = {
                        let mut decoded_cache = self
                          .decoded_image_cache
                          .lock()
                          .unwrap_or_else(|e| e.into_inner());
                        decoded_cache.get(&cache_key)
                      } {
                        image = existing;
                      } else {
                        fn contains_foreign_object_tag(svg: &str) -> bool {
                          const NEEDLE: &[u8] = b"foreignobject";
                          let bytes = svg.as_bytes();
                          bytes
                            .windows(NEEDLE.len())
                            .any(|window| window.eq_ignore_ascii_case(NEEDLE))
                        }

                        let (render_w_unoriented, render_h_unoriented) =
                          if orientation.quarter_turns % 2 == 1 {
                            (render_h, render_w)
                          } else {
                            (render_w, render_h)
                          };

                        let svg_to_render: Cow<'_, str> = if contains_foreign_object_tag(svg_markup)
                        {
                          let (rendered_w_css, rendered_h_css) =
                            if orientation.quarter_turns % 2 == 1 {
                              (dest_h, dest_w)
                            } else {
                              (dest_w, dest_h)
                            };
                          let intrinsic_w_css = cached.width() as f32;
                          let intrinsic_h_css = cached.height() as f32;
                          let foreign_object_dpr =
                             crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
                               svg_markup,
                               base_dpr,
                               rendered_w_css,
                               rendered_h_css,
                               intrinsic_w_css,
                               intrinsic_h_css,
                             );
                          crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
                            svg_markup,
                            "",
                            &self.font_ctx,
                            image_cache,
                            foreign_object_dpr,
                            self.max_iframe_depth,
                          )
                          .map(Cow::Owned)
                          .unwrap_or_else(|| Cow::Borrowed(svg_markup))
                        } else {
                          Cow::Borrowed(svg_markup)
                        };

                        if let Ok(pixmap) = image_cache.render_svg_pixmap_at_size(
                          svg_to_render.as_ref(),
                          render_w_unoriented,
                          render_h_unoriented,
                          &render_url,
                          base_dpr,
                        ) {
                          // Converting a `Pixmap` into display-list `ImageData` requires cloning the
                          // pixel buffer (`pixmap.data().to_vec()`). This is a major allocation
                          // hotspot that bypasses `paint/pixmap.rs`, so we explicitly charge it to
                          // any active stage allocation budget before allocating.
                          if let Some(bytes) = u64::from(render_w_unoriented)
                            .checked_mul(u64::from(render_h_unoriented))
                            .and_then(|px| px.checked_mul(4))
                          {
                            if let Err(err) = crate::render_control::reserve_allocation_with(
                              bytes,
                              || {
                                format!(
                                  "image data pixel buffer {}x{} url={}",
                                  render_w_unoriented, render_h_unoriented, render_url
                                )
                              },
                            ) {
                              self.error.get_or_insert(err);
                              return;
                            }
                          }
                          let image_data = if orientation.quarter_turns % 4 == 0
                            && !orientation.flip_x
                          {
                            let mut data = ImageData::from_pixmap(pixmap.as_ref(), dest_w, dest_h);
                            data.has_intrinsic_ratio = image.has_intrinsic_ratio;
                            Arc::new(data)
                          } else {
                            let mut rgba = image::RgbaImage::from_raw(
                              render_w_unoriented,
                              render_h_unoriented,
                              pixmap.data().to_vec(),
                            )
                            .unwrap_or_else(|| image::RgbaImage::new(1, 1));
                            match orientation.quarter_turns % 4 {
                              0 => {}
                              1 => rgba = image::imageops::rotate90(&rgba),
                              2 => rgba = image::imageops::rotate180(&rgba),
                              3 => rgba = image::imageops::rotate270(&rgba),
                              _ => {}
                            }
                            if orientation.flip_x {
                              rgba = image::imageops::flip_horizontal(&rgba);
                            }
                            let (w, h) = rgba.dimensions();
                            let mut data =
                              ImageData::new_premultiplied(w, h, dest_w, dest_h, rgba.into_raw());
                            data.has_intrinsic_ratio = image.has_intrinsic_ratio;
                            Arc::new(data)
                          };

                          let mut decoded_cache = self
                            .decoded_image_cache
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                          if let Some(existing) = decoded_cache.get(&cache_key) {
                            image = existing;
                          } else {
                            decoded_cache.insert(cache_key, image_data.clone());
                            image = image_data;
                          }
                        }
                      }
                    }
                  }
                }
              }
            }
            let clip_contents = Self::replaced_content_clip_item(
              style_for_image,
              content_rect,
              dest_rect,
              clip_radii,
            );
            if let Some(clip) = clip_contents.as_ref() {
              self.list.push(DisplayItem::PushClip(clip.clone()));
            }
            self.list.push(DisplayItem::Image(ImageItem {
              dest_rect,
              image,
              filter_quality: Self::image_filter_quality(fragment.style.as_deref()),
              src_rect: None,
            }));
            if clip_contents.is_some() {
              self.list.push(DisplayItem::PopClip);
            }
            break 'paint;
          }

          if deferred_async {
            // `loading="lazy"` / `decoding="async"` allow deferring image work. When we choose to
            // defer, keep the image transparent (no UA placeholder) to match browser behavior while
            // loading/decoding is still pending.
            break 'paint;
          }

          if try_iframe_fallback {
            if let Some(cache) = self.image_cache.as_ref() {
              let (content_rect, _) = self.replaced_content_rect_and_radii(rect, style_for_image);
              if let Some(candidate) = sources.first() {
                if let Some(image) = render_iframe_src(
                  candidate.url,
                  referrer_policy,
                  content_rect,
                  style_for_image,
                  cache,
                  &self.font_ctx,
                  self.device_pixel_ratio,
                  self.max_iframe_depth,
                ) {
                  self.emit_iframe_image(image, rect, style_for_image);
                  break 'paint;
                }
              }
            }
          }

          self.emit_replaced_placeholder(replaced_type, fragment, rect);
          if let ReplacedType::Image { alt: Some(alt), .. } = replaced_type {
            let _ = self.emit_alt_text(alt, fragment, rect);
          }
        }

        if let (Some(style), Some(ctx)) = (style_for_image, self.line_decoration_ctx) {
          let mut fallback_decorations: Option<[ResolvedTextDecoration; 1]> = None;
          let (ancestor_decorations, own_decorations) =
            Self::split_text_decorations(style, &mut fallback_decorations);

          let rects = Self::background_rects(rect, style, self.viewport);
          let ancestor_rect = if style.text_decoration_skip_box == TextDecorationSkipBox::All {
            rects.content
          } else {
            rect
          };

          if !ancestor_decorations.is_empty() {
            let (inline_start, inline_len) = if ctx.inline_vertical {
              (ancestor_rect.y(), ancestor_rect.height())
            } else {
              (ancestor_rect.x(), ancestor_rect.width())
            };
            let clip = self.line_decoration_clip_range(inline_start, inline_len);
            self.emit_text_decorations_for_decorations(
              style,
              ancestor_decorations,
              None,
              inline_start,
              inline_len,
              ctx.block_baseline,
              ctx.inline_vertical,
              clip,
            );
          }

          // A decorating box never draws over its own margin/border/padding, so always clip the
          // decoration it originates to its content box.
          if !own_decorations.is_empty() {
            let (inline_start, inline_len) = if ctx.inline_vertical {
              (rects.content.y(), rects.content.height())
            } else {
              (rects.content.x(), rects.content.width())
            };
            let clip = self.line_decoration_clip_range(inline_start, inline_len);
            self.emit_text_decorations_for_decorations(
              style,
              own_decorations,
              None,
              inline_start,
              inline_len,
              ctx.block_baseline,
              ctx.inline_vertical,
              clip,
            );
          }
        }
      }

      FragmentContent::Block { .. } | FragmentContent::Inline { .. } => {
        if let (Some(style), Some(ctx)) = (fragment.style.as_deref(), self.line_decoration_ctx) {
          if style.display.is_inline_level() {
            if style.display.establishes_formatting_context() {
              let mut fallback_decorations: Option<[ResolvedTextDecoration; 1]> = None;
              let (ancestor_decorations, _own_decorations) =
                Self::split_text_decorations(style, &mut fallback_decorations);

              let rects = Self::background_rects(rect, style, self.viewport);
              let ancestor_rect = if style.text_decoration_skip_box == TextDecorationSkipBox::All {
                rects.content
              } else {
                rect
              };

              if !ancestor_decorations.is_empty() {
                let (inline_start, inline_len) = if ctx.inline_vertical {
                  (ancestor_rect.y(), ancestor_rect.height())
                } else {
                  (ancestor_rect.x(), ancestor_rect.width())
                };
                let clip = self.line_decoration_clip_range(inline_start, inline_len);
                self.emit_text_decorations_for_decorations(
                  style,
                  ancestor_decorations,
                  None,
                  inline_start,
                  inline_len,
                  ctx.block_baseline,
                  ctx.inline_vertical,
                  clip,
                );
              }

              // Atomic inlines (inline-block/inline-flex/...) establish their own formatting
              // context. Browsers do not paint their own `text-decoration` as a full-width line for
              // the atomic box itself (e.g. `<a style="display:inline-block"><img/></a>` should
              // not be underlined). Instead, decorations are painted by descendant text runs.
            } else if matches!(fragment.content, FragmentContent::Inline { .. })
              && style.text_decoration_skip_box == TextDecorationSkipBox::None
            {
              // For non-atomic inline boxes, descendant text paints decorations within the content
              // box. When `text-decoration-skip-box` is `none`, ancestor decorations should extend
              // through the inline box's border/padding areas too.
              let mut fallback_decorations: Option<[ResolvedTextDecoration; 1]> = None;
              let (ancestor_decorations, _own_decorations) =
                Self::split_text_decorations(style, &mut fallback_decorations);
              if ancestor_decorations.is_empty() {
                return;
              }

              let rects = Self::background_rects(rect, style, self.viewport);
              let (border_start, border_end, content_start, content_end) = if ctx.inline_vertical {
                (
                  rect.y(),
                  rect.y() + rect.height(),
                  rects.content.y(),
                  rects.content.y() + rects.content.height(),
                )
              } else {
                (
                  rect.x(),
                  rect.x() + rect.width(),
                  rects.content.x(),
                  rects.content.x() + rects.content.width(),
                )
              };

              let start_edge = (content_start - border_start).max(0.0);
              let end_edge = (border_end - content_end).max(0.0);

              if start_edge > 0.0 {
                let clip = self.line_decoration_clip_range(border_start, start_edge);
                self.emit_text_decorations_for_decorations(
                  style,
                  ancestor_decorations,
                  None,
                  border_start,
                  start_edge,
                  ctx.block_baseline,
                  ctx.inline_vertical,
                  clip,
                );
              }
              if end_edge > 0.0 {
                let clip = self.line_decoration_clip_range(content_end, end_edge);
                self.emit_text_decorations_for_decorations(
                  style,
                  ancestor_decorations,
                  None,
                  content_end,
                  end_edge,
                  ctx.block_baseline,
                  ctx.inline_vertical,
                  clip,
                );
              }
            }
          }
        }
      }

      // Line, RunningAnchor, and others - no direct content
      _ => {}
    }
  }

  /// Gets the box_id from a fragment
  fn get_box_id(fragment: &FragmentNode) -> Option<usize> {
    match &fragment.content {
      FragmentContent::Block { box_id } => *box_id,
      FragmentContent::Inline { box_id, .. } => *box_id,
      FragmentContent::Text { box_id, .. } => *box_id,
      FragmentContent::Replaced { box_id, .. } => *box_id,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. } => None,
      FragmentContent::Line { .. } => None,
    }
  }

  fn has_paintable_background(style: &ComputedStyle) -> bool {
    style.background_color.alpha_u8() > 0
      || style
        .background_layers
        .iter()
        .any(|layer| layer.image.is_some())
  }

  fn has_paintable_border(style: &ComputedStyle) -> bool {
    use crate::style::types::BorderStyle;

    fn side(width: Length, style: BorderStyle, color: Rgba) -> bool {
      width.to_px() > 0.0
        && !matches!(style, BorderStyle::None | BorderStyle::Hidden)
        && color.alpha_u8() > 0
    }

    if !matches!(style.border_image.source, BorderImageSource::None) {
      return true;
    }

    side(
      style.border_top_width,
      style.border_top_style,
      style.border_top_color,
    ) || side(
      style.border_right_width,
      style.border_right_style,
      style.border_right_color,
    ) || side(
      style.border_bottom_width,
      style.border_bottom_style,
      style.border_bottom_color,
    ) || side(
      style.border_left_width,
      style.border_left_style,
      style.border_left_color,
    )
  }

  fn should_propagate_root_border(
    &self,
    style: &ComputedStyle,
    source_rect: Rect,
    target_rect: Rect,
  ) -> bool {
    // Only treat the canvas expansion as a viewport scrollbar gutter when it matches the resolved
    // scrollbar width. Otherwise, callers like `fit_canvas_to_content` can request much larger
    // surfaces and we should not expand borders into that area.
    let gutter = resolve_scrollbar_width(style).max(0.0);
    if gutter <= 0.0 {
      return false;
    }

    let diff_w = (target_rect.width() - source_rect.width()).max(0.0);
    let diff_h = (target_rect.height() - source_rect.height()).max(0.0);
    let epsilon = 0.51;

    let matches_gutter = |diff: f32| {
      diff <= epsilon || (diff - gutter).abs() <= epsilon || (diff - gutter * 2.0).abs() <= epsilon
    };

    (diff_w > epsilon || diff_h > epsilon) && matches_gutter(diff_w) && matches_gutter(diff_h)
  }

  fn root_background_candidate(
    fragment: &FragmentNode,
    origin: Point,
  ) -> Option<(Arc<ComputedStyle>, usize, Rect)> {
    // Most renderer-produced fragment trees use the root element (the `<html>` box) as the tree
    // root, so `fragment` already has a `box_id`.
    //
    // However, pagination introduces synthetic page fragments (page box + margin boxes) that do not
    // originate from a DOM box and therefore have no `box_id`. In that case we still want to apply
    // the usual HTML canvas background propagation (from `<html>` or `<body>`) without accidentally
    // treating @page margin boxes as candidates.
    //
    // To do that, when the root has no `box_id` and multiple children (page root + margin boxes),
    // we locate the first *non-fixed* fragment with a `box_id` inside the "document wrapper"
    // subtree (the first non-box-id child). This reliably finds the real `<html>` element for
    // paginated pages while ignoring margin boxes.
    let mut html = fragment;
    let mut html_origin = origin;

    // Some renderer-produced trees wrap the actual `<html>` element in an anonymous root box.
    // That wrapper is DOM-less but *does* have a `box_id` (assigned during box-tree fixup) and
    // typically carries the default `ComputedStyle` (`display: inline`). Treat it like a wrapper
    // and locate the first non-fixed DOM-backed child instead so HTML canvas background
    // propagation still finds `<html>`/`<body>`.
    if Self::get_box_id(fragment).is_some()
      && fragment
        .style
        .as_deref()
        .is_some_and(|style| matches!(style.display, crate::style::display::Display::Inline))
    {
      if let Some(child) = fragment.children.iter().find(|child| {
        Self::get_box_id(child).is_some()
          && !child
            .style
            .as_deref()
            .is_some_and(|style| style.position == Position::Fixed)
      }) {
        html = child;
        html_origin = origin.translate(child.bounds.origin);
      }
    }

    if Self::get_box_id(html).is_none() {
      if fragment.children.len() == 1 {
        let child = fragment.children.first().unwrap_or(fragment);
        // Some unit tests construct a synthetic viewport fragment with a single child representing
        // the root element. Renderer-produced fragment trees always use the root element itself as
        // the fragment tree root (even when it only has one child), so only treat a single-child
        // root as a wrapper when the root is *clearly* synthetic (no style or default inline style
        // with no paintable background/border).
        // Pagination can also create a single-child synthetic page root, where the lone child is a
        // "document wrapper" fragment (no `box_id`) that contains the real `<html>` element. In
        // that case, still apply normal HTML canvas background propagation by finding the first
        // non-fixed DOM-backed fragment inside the wrapper.
        if Self::get_box_id(child).is_none() && child.stacking_context.forced_z_index().is_some() {
          let child_origin = origin.translate(child.bounds.origin);
          let mut stack: Vec<(&FragmentNode, Point)> = Vec::new();
          stack.push((child, child_origin));
          while let Some((node, node_origin)) = stack.pop() {
            let is_fixed = node
              .style
              .as_deref()
              .is_some_and(|style| style.position == Position::Fixed);
            if !is_fixed && Self::get_box_id(node).is_some() {
              html = node;
              html_origin = node_origin;
              break;
            }
            for child in node.children.iter().rev() {
              stack.push((child, node_origin.translate(child.bounds.origin)));
            }
          }
          if Self::get_box_id(html).is_none() {
            return None;
          }
        } else {
          let root_is_synthetic_wrapper = fragment.style.as_deref().map_or(true, |style| {
            matches!(style.display, crate::style::display::Display::Inline)
              && !Self::has_paintable_background(style)
              && !Self::has_paintable_border(style)
          });
          if root_is_synthetic_wrapper {
            html = child;
            html_origin = origin.translate(child.bounds.origin);
          }
        }
      } else if !fragment.children.is_empty() {
        let doc_root = fragment
          .children
          .iter()
          .find(|child| Self::get_box_id(child).is_none())
          .unwrap_or(&fragment.children[0]);
        let doc_origin = origin.translate(doc_root.bounds.origin);

        // Iterative DFS (tree order) so we can compute absolute origins without recursion.
        let mut stack: Vec<(&FragmentNode, Point)> = Vec::new();
        stack.push((doc_root, doc_origin));
        while let Some((node, node_origin)) = stack.pop() {
          let is_fixed = node
            .style
            .as_deref()
            .is_some_and(|style| style.position == Position::Fixed);
          if !is_fixed && Self::get_box_id(node).is_some() {
            html = node;
            html_origin = node_origin;
            break;
          }
          for child in node.children.iter().rev() {
            stack.push((child, node_origin.translate(child.bounds.origin)));
          }
        }

        // If we couldn't find a DOM-backed fragment, don't guess: skip canvas propagation rather
        // than accidentally selecting a page-margin box or fixed fragment.
        if Self::get_box_id(html).is_none() {
          return None;
        }
      }
    }

    if let Some(html_id) = Self::get_box_id(html) {
      // HTML canvas background propagation:
      //
      // Prefer `<body>` as the canvas background source when it has a paintable background, even
      // when `<html>` also has a background. This matches browser behavior where body background
      // fills the canvas (including the body margin area), and ensures negative z-index stacking
      // contexts remain visible above the page background.
      if let Some(body_id) = html_id.checked_add(1) {
        // In some layout modes the `<body>` fragment may not be a direct child of `<html>` (e.g.
        // anonymous wrappers for scrolling/overflow). Search by box id rather than assuming a
        // specific tree shape.
        let mut stack: Vec<(&FragmentNode, Point)> = Vec::new();
        stack.push((html, html_origin));
        while let Some((node, node_origin)) = stack.pop() {
          if Self::get_box_id(node) == Some(body_id) {
            if let Some(style) = node.style.clone() {
              if Self::has_paintable_background(&style) {
                return Some((style, body_id, Rect::new(node_origin, node.bounds.size)));
              }
            }
            break;
          }

          for child in node.children.iter().rev() {
            stack.push((child, node_origin.translate(child.bounds.origin)));
          }
        }
      }

      if let Some(style) = html.style.clone() {
        if Self::has_paintable_background(&style) {
          return Some((style, html_id, Rect::new(html_origin, html.bounds.size)));
        }
      }
      return None;
    }

    if let Some(style) = html.style.clone() {
      if Self::has_paintable_background(&style) {
        let suppress_box_id = Self::get_box_id(html).unwrap_or(usize::MAX);
        return Some((style, suppress_box_id, Rect::new(html_origin, html.bounds.size)));
      }
    }

    // Fallback for fragment trees without box IDs (unit tests): treat the first *in-flow* paintable
    // child as the body element.
    //
    // Important: avoid selecting floats/positioned elements, which would incorrectly promote their
    // background to the canvas (and break CSS2 paint order expectations in unit tests).
    for child in html.children.iter() {
      if let Some(style) = child.style.clone() {
        if Self::has_paintable_background(&style) {
          let style_ref = style.as_ref();
          let is_out_of_flow = matches!(style_ref.position, Position::Absolute | Position::Fixed);
          let is_float = style_ref.float.is_floating();
          let is_inline_level = style_ref.display.is_inline_level();
          if is_out_of_flow || is_float || is_inline_level {
            continue;
          }
          let suppress_box_id = Self::get_box_id(child).unwrap_or(usize::MAX);
          let rect = Rect::new(
            html_origin.translate(child.bounds.origin),
            child.bounds.size,
          );
          return Some((style, suppress_box_id, rect));
        }
      }
    }

    None
  }

  fn paged_media_document_canvas_rect(page_root: &FragmentNode, origin: Point) -> Option<Rect> {
    // Locate the synthetic paged-media document wrapper (forced z-index: 0) and use its bounds as
    // the document canvas area (CSS Page 3 §3.1: the canvas is drawn as the page box background).
    //
    // Pagination positions the wrapper inside the @page margins, so this automatically excludes
    // the margin box areas while still covering the page box's border/padding/content.
    let mut wrapper: Option<&FragmentNode> = None;
    let mut wrapper_area = -1.0f32;
    for child in page_root.children.iter() {
      if child.stacking_context.forced_z_index() != Some(0) {
        continue;
      }
      let area = child.bounds.width().max(0.0) * child.bounds.height().max(0.0);
      if area > wrapper_area {
        wrapper = Some(child);
        wrapper_area = area;
      }
    }
    let wrapper = wrapper?;
    let wrapper_origin = origin.translate(wrapper.bounds.origin);
    Some(Rect::new(wrapper_origin, wrapper.bounds.size))
  }

  fn build_text_clip_runs(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    visibility: Visibility,
  ) -> Option<Arc<[TextItem]>> {
    let mut runs = Vec::new();
    self.collect_text_clip_runs(fragment, offset, visibility, &mut runs);
    if runs.is_empty() {
      None
    } else {
      Some(runs.into())
    }
  }

  fn collect_text_clip_runs(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    visibility: Visibility,
    out: &mut Vec<TextItem>,
  ) {
    // This routine used to recurse directly on `fragment.children`, which meant adversarially deep
    // fragment trees combined with `background-clip:text` could stack overflow. Traverse the
    // fragment subtree iteratively instead.
    struct WorkItem<'a> {
      fragment: &'a FragmentNode,
      offset: Point,
      visibility: Visibility,
    }

    let mut stack: Vec<WorkItem<'_>> = Vec::new();
    stack.push(WorkItem {
      fragment,
      offset,
      visibility,
    });

    let mut counter = 0usize;
    while let Some(item) = stack.pop() {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }

      let fragment = item.fragment;
      let offset = item.offset;
      let visibility = item.visibility;

      let style_opt = fragment.style.as_deref();
      let paint_self = style_opt.map_or(true, |style| {
        matches!(
          style.visibility,
          crate::style::computed::Visibility::Visible
        )
      });
      if !paint_self && fragment.children.is_empty() {
        continue;
      }

      if matches!(
        fragment.content,
        FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
      ) {
        continue;
      }

      let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
      if opacity <= f32::EPSILON {
        continue;
      }

      let absolute_rect = Rect::new(
        Point::new(
          fragment.bounds.origin.x + offset.x,
          fragment.bounds.origin.y + offset.y,
        ),
        fragment.bounds.size,
      );
      let skip_contents = style_opt.is_some_and(|style| match style.content_visibility {
        ContentVisibility::Hidden => true,
        ContentVisibility::Auto => visibility
          .rect
          .is_some_and(|vis| !vis.intersects(absolute_rect)),
        ContentVisibility::Visible => false,
      });

      let mut paint_bounds = self.fragment_paint_bounds(fragment, absolute_rect, style_opt);
      paint_bounds = paint_bounds.union(fragment.scroll_overflow.translate(absolute_rect.origin));
      if let Some(vis) = visibility.rect {
        if !vis.intersects(paint_bounds) {
          continue;
        }
        if visibility.hard_clip {
          paint_bounds = match paint_bounds.intersection(vis) {
            Some(intersection) if intersection.width() > 0.0 && intersection.height() > 0.0 => {
              intersection
            }
            _ => continue,
          };
        } else {
          paint_bounds = paint_bounds.intersection(vis).unwrap_or(paint_bounds);
        }
      }

      // Mirror the main fragment traversal's clipping rules so we only include text that will
      // actually be painted. In particular, text outside overflow/clip edges should not contribute
      // to `background-clip:text`.
      let mut absolute_rects: Option<BackgroundRects> = None;
      let (overflow_clip, clip_rect) = if let Some(style) = style_opt {
        let is_replaced = matches!(&fragment.content, FragmentContent::Replaced { .. });
        let clip_x = !is_replaced && Self::overflow_axis_clips(style.overflow_x);
        let clip_y = !is_replaced && Self::overflow_axis_clips(style.overflow_y);
        (
          if clip_x || clip_y {
            let overflow_bounds =
              absolute_rect.union(fragment.scroll_overflow.translate(absolute_rect.origin));
            let rects = absolute_rects
              .get_or_insert_with(|| Self::background_rects(absolute_rect, style, self.viewport));
            Self::overflow_clip_from_style_with_rects(
              style,
              rects,
              clip_x,
              clip_y,
              overflow_bounds,
              self.viewport,
              self.build_breakdown.as_deref(),
            )
          } else {
            None
          },
          Self::clip_rect_from_style(style, absolute_rect, self.viewport),
        )
      } else {
        (None, None)
      };
      let mut child_visibility = visibility;
      if let Some(clip) = overflow_clip.as_ref() {
        child_visibility = child_visibility.intersect(Self::clip_bounds(clip), true);
      }
      if let Some(clip) = clip_rect.as_ref() {
        child_visibility = child_visibility.intersect(Self::clip_bounds(clip), true);
      }
      if child_visibility.rect.is_none() && visibility.rect.is_some() {
        continue;
      }
      if let Some(vis) = child_visibility.rect {
        if !vis.intersects(paint_bounds) {
          continue;
        }
        if child_visibility.hard_clip {
          match paint_bounds.intersection(vis) {
            Some(intersection) if intersection.width() > 0.0 && intersection.height() > 0.0 => {}
            _ => continue,
          };
        }
      }

      if paint_self {
        if let FragmentContent::Text {
          text,
          baseline_offset,
          shaped,
          is_marker,
          ..
        } = &fragment.content
        {
          if !text.is_empty() && !is_marker {
            if let Some(style) = style_opt {
              let inline_vertical = matches!(
                style.writing_mode,
                crate::style::types::WritingMode::VerticalRl
                  | crate::style::types::WritingMode::VerticalLr
                  | crate::style::types::WritingMode::SidewaysRl
                  | crate::style::types::WritingMode::SidewaysLr
              );
              let (baseline_block, baseline_inline) = if inline_vertical {
                (
                  absolute_rect.origin.x + baseline_offset,
                  absolute_rect.origin.y,
                )
              } else {
                (
                  absolute_rect.origin.x,
                  absolute_rect.origin.y + baseline_offset,
                )
              };

              let mut shaped_storage: Option<Vec<ShapedRun>> = None;
              let runs_ref: Option<&[ShapedRun]> = if let Some(runs) = shaped {
                Some(runs.as_ref())
              } else {
                let shape_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
                let shaped_result = self.shaper.shape(text, style, &self.font_ctx);
                if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), shape_timer)
                {
                  breakdown.record_text_shape(start.elapsed());
                }
                if let Ok(mut runs) = shaped_result {
                  InlineTextItem::apply_spacing_to_runs(
                    &mut runs,
                    text,
                    style.letter_spacing,
                    style.word_spacing,
                  );
                  shaped_storage = Some(runs);
                }
                shaped_storage.as_deref()
              };

              if let Some(runs) = runs_ref {
                if inline_vertical {
                  self.collect_shaped_runs_for_text_clip_vertical(
                    out,
                    runs,
                    baseline_block,
                    baseline_inline,
                    style.allow_subpixel_aa,
                  );
                } else {
                  self.collect_shaped_runs_for_text_clip(
                    out,
                    runs,
                    baseline_inline,
                    baseline_block,
                    style.allow_subpixel_aa,
                  );
                }
              }
            }
          }
        }
      }

      if skip_contents {
        continue;
      }

      let element_scroll = self.element_scroll_offset(fragment);
      let child_offset = Point::new(
        absolute_rect.origin.x - element_scroll.x,
        absolute_rect.origin.y - element_scroll.y,
      );

      // Preserve the original recursive DFS order: visit children left-to-right.
      for child in fragment.children.iter().rev() {
        stack.push(WorkItem {
          fragment: child,
          offset: child_offset,
          visibility: child_visibility,
        });
      }
    }
  }

  /// Emits a background fill for a fragment
  pub fn emit_background(&mut self, rect: Rect, color: Rgba) {
    if !color.is_transparent() {
      self
        .list
        .push(DisplayItem::FillRect(FillRectItem { rect, color }));
    }
  }

  #[inline]
  fn emit_background_tile(&mut self, item: DisplayItem) {
    self.list.push(item);
    if let Some(counter) = self.background_tiles.as_ref() {
      counter.fetch_add(1, Ordering::Relaxed);
    }
  }

  /// Emits border strokes for a fragment
  pub fn emit_border(&mut self, rect: Rect, width: f32, color: Rgba) {
    if width > 0.0 && !color.is_transparent() {
      self.list.push(DisplayItem::StrokeRect(StrokeRectItem {
        rect,
        color,
        width,
        blend_mode: BlendMode::Normal,
      }));
    }
  }

  fn emit_background_from_style(&mut self, rect: Rect, style: &ComputedStyle, scroll_delta: Point) {
    self.emit_background_from_style_with_text_clip_and_culling_rect(
      rect,
      style,
      None,
      self.viewport_rect(),
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_culling_rect(
    &mut self,
    rect: Rect,
    style: &ComputedStyle,
    culling_rect: Option<Rect>,
    scroll_delta: Point,
  ) {
    self.emit_background_from_style_with_text_clip_and_culling_rect(
      rect,
      style,
      None,
      culling_rect,
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_text_clip(
    &mut self,
    rect: Rect,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
    scroll_delta: Point,
  ) {
    self.emit_background_from_style_with_text_clip_and_culling_rect(
      rect,
      style,
      text_clip,
      self.viewport_rect(),
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_text_clip_and_culling_rect(
    &mut self,
    rect: Rect,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
    culling_rect: Option<Rect>,
    scroll_delta: Point,
  ) {
    let has_images = style.background_layers.iter().any(|l| l.image.is_some());
    if style.background_color.is_transparent() && !has_images {
      return;
    }

    let rects = Self::background_rects(rect, style, self.viewport);
    self.emit_background_from_style_with_rects_and_text_clip_and_culling_rect(
      &rects,
      style,
      text_clip,
      culling_rect,
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_rects(
    &mut self,
    rects: &BackgroundRects,
    style: &ComputedStyle,
    scroll_delta: Point,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(
      rects,
      rects,
      style,
      None,
      self.viewport_rect(),
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_rects_and_origin(
    &mut self,
    rects: &BackgroundRects,
    origin_rects: &BackgroundRects,
    style: &ComputedStyle,
    scroll_delta: Point,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(
      rects,
      origin_rects,
      style,
      None,
      self.viewport_rect(),
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_rects_and_text_clip(
    &mut self,
    rects: &BackgroundRects,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
    scroll_delta: Point,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(
      rects,
      rects,
      style,
      text_clip,
      self.viewport_rect(),
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_rects_and_text_clip_and_culling_rect(
    &mut self,
    rects: &BackgroundRects,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
    culling_rect: Option<Rect>,
    scroll_delta: Point,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(
      rects,
      rects,
      style,
      text_clip,
      culling_rect,
      scroll_delta,
    );
  }

  fn emit_background_from_style_with_rects_and_origin_and_text_clip(
    &mut self,
    rects: &BackgroundRects,
    origin_rects: &BackgroundRects,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
    culling_rect: Option<Rect>,
    scroll_delta: Point,
  ) {
    let has_images = style.background_layers.iter().any(|l| l.image.is_some());
    if style.background_color.is_transparent() && !has_images {
      return;
    }

    let fallback = BackgroundLayer::default();
    let color_layer = style.background_layers.first().unwrap_or(&fallback);
    let color_clips_text = color_layer.clip == BackgroundBox::Text;
    let color_clip_rect = match color_layer.clip {
      BackgroundBox::BorderBox => rects.border,
      BackgroundBox::PaddingBox => rects.padding,
      BackgroundBox::ContentBox | BackgroundBox::Text => rects.content,
    };
    if style.background_color.alpha_u8() > 0
      && color_clip_rect.width() > 0.0
      && color_clip_rect.height() > 0.0
    {
      let pushed_text_clip =
        color_clips_text && text_clip.is_some_and(|runs| !runs.is_empty()) && {
          self.list.push(DisplayItem::PushClip(ClipItem {
            shape: ClipShape::Text {
              runs: Arc::clone(text_clip.unwrap()),
            },
          }));
          true
        };
      if !color_clips_text || pushed_text_clip {
        let radii = if Self::border_radius_is_zero(style) {
          crate::paint::display_list::BorderRadii::ZERO
        } else {
          Self::resolve_clip_radii(
            style,
            rects,
            color_layer.clip,
            self.viewport,
            self.build_breakdown.as_deref(),
          )
        };
        if radii.is_zero() {
          self.emit_background(color_clip_rect, style.background_color);
        } else {
          self.list.push(DisplayItem::FillRoundedRect(
            crate::paint::display_list::FillRoundedRectItem {
              rect: color_clip_rect,
              color: style.background_color,
              radii,
            },
          ));
        }
        if pushed_text_clip {
          self.list.push(DisplayItem::PopClip);
        }
      }
    }

    for layer in style.background_layers.iter().rev() {
      if let Some(image) = &layer.image {
        self.emit_background_layer_with_origin_rects(
          rects,
          origin_rects,
          style,
          layer,
          image,
          text_clip,
          culling_rect,
          scroll_delta,
        );
      }
    }
  }

  fn emit_root_background(&mut self, root: &RootBackground) {
    let rects = Self::background_rects(root.paint_rect, &root.style, self.viewport);
    let origin_rects = Self::background_rects(root.origin_rect, &root.style, self.viewport);
    self.emit_background_from_style_with_rects_and_origin(
      &rects,
      &origin_rects,
      &root.style,
      Point::ZERO,
    );
    if root.paint_border {
      self.emit_border_from_style(root.paint_rect, &root.style, None);
    }
  }

  fn emit_background_layer_with_origin_rects(
    &mut self,
    rects: &BackgroundRects,
    origin_rects: &BackgroundRects,
    style: &ComputedStyle,
    layer: &BackgroundLayer,
    bg: &BackgroundImage,
    text_clip: Option<&Arc<[TextItem]>>,
    culling_rect: Option<Rect>,
    scroll_delta: Point,
  ) {
    if self.deadline_reached() {
      return;
    }

    let is_local = layer.attachment == BackgroundAttachment::Local;
    let clips_text = layer.clip == BackgroundBox::Text;
    let clip_box = if is_local {
      match layer.clip {
        BackgroundBox::ContentBox | BackgroundBox::Text => BackgroundBox::ContentBox,
        _ => BackgroundBox::PaddingBox,
      }
    } else {
      layer.clip
    };
    let clip_rect = match clip_box {
      BackgroundBox::BorderBox => rects.border,
      BackgroundBox::PaddingBox => rects.padding,
      BackgroundBox::ContentBox | BackgroundBox::Text => rects.content,
    };
    let origin_rect = if layer.attachment == BackgroundAttachment::Fixed {
      if let Some((w, h)) = self.viewport {
        Rect::from_xywh(0.0, 0.0, w, h)
      } else {
        rects.border
      }
    } else if is_local {
      match layer.origin {
        BackgroundBox::ContentBox | BackgroundBox::Text => origin_rects.content,
        _ => origin_rects.padding,
      }
    } else {
      match layer.origin {
        BackgroundBox::BorderBox => origin_rects.border,
        BackgroundBox::PaddingBox => origin_rects.padding,
        BackgroundBox::ContentBox | BackgroundBox::Text => origin_rects.content,
      }
    };
    let origin_rect = if is_local {
      origin_rect.translate(scroll_delta)
    } else {
      origin_rect
    };

    if clip_rect.width() <= 0.0
      || clip_rect.height() <= 0.0
      || origin_rect.width() <= 0.0
      || origin_rect.height() <= 0.0
    {
      return;
    }

    // Tiling is computed from `clip_rect`, which may be enormous for very tall pages. When we're
    // building a display list for a viewport-sized canvas we should only emit tiles that can
    // actually land on the canvas; the optimizer will cull off-screen items anyway.
    //
    // IMPORTANT: background tiles are emitted in the *pre-transform* coordinate space and later
    // transformed by `PushStackingContext`. The caller must therefore pass a culling rectangle in
    // the same pre-transform space (typically from `visibility.rect`, already mapped into local
    // space via `visible_in_local_space` at stacking-context boundaries). Using the raw viewport
    // rectangle here would incorrectly drop background pixels that become visible after transforms
    // (e.g. `left: 50%` + `translateX(-50%)` centering).
    let visible_clip = match culling_rect.filter(|r| r.width() > 0.0 && r.height() > 0.0) {
      Some(cull) => {
        let Some(intersection) = clip_rect.intersection(cull) else {
          return;
        };
        if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
          return;
        }
        intersection
      }
      None => clip_rect,
    };

    if !matches!(bg, BackgroundImage::None) {
      if let Some(counter) = self.background_layers.as_ref() {
        counter.fetch_add(1, Ordering::Relaxed);
      }
    }

    let clip_radii = if Self::border_radius_is_zero(style) {
      crate::paint::display_list::BorderRadii::ZERO
    } else {
      Self::resolve_clip_radii(
        style,
        rects,
        clip_box,
        self.viewport,
        self.build_breakdown.as_deref(),
      )
    };
    let blend_mode = Self::convert_blend_mode(layer.blend_mode);
    let use_blend = blend_mode != BlendMode::Normal;
    let pushed_radii_clip = !clip_radii.is_zero() && {
      self.list.push(DisplayItem::PushClip(ClipItem {
        shape: ClipShape::Rect {
          rect: clip_rect,
          radii: Some(clip_radii),
        },
      }));
      true
    };
    let pushed_text_clip = clips_text && text_clip.is_some_and(|runs| !runs.is_empty()) && {
      self.list.push(DisplayItem::PushClip(ClipItem {
        shape: ClipShape::Text {
          runs: Arc::clone(text_clip.unwrap()),
        },
      }));
      true
    };
    if clips_text && !pushed_text_clip {
      // No text to clip to; skip painting this layer entirely.
      if pushed_radii_clip {
        self.list.push(DisplayItem::PopClip);
      }
      return;
    }
    if use_blend {
      self.list.push(DisplayItem::PushBlendMode(BlendModeItem {
        mode: blend_mode,
      }));
    }

    let repeat_both_axes = matches!(
      layer.repeat,
      crate::style::types::BackgroundRepeat {
        x: BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round,
        y: BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round,
      }
    );

    let compute_tile_metrics = |img_w: f32, img_h: f32| -> Option<(f32, f32, f32, f32)> {
      let (size_area_w, size_area_h) = (origin_rect.width(), origin_rect.height());
      let (mut tile_w, mut tile_h) = Self::compute_background_size(
        layer,
        style.font_size,
        style.root_font_size,
        self.viewport,
        size_area_w,
        size_area_h,
        img_w,
        img_h,
        None,
      );
      if !tile_w.is_finite() || !tile_h.is_finite() || tile_w <= 0.0 || tile_h <= 0.0 {
        return None;
      }

      let mut rounded_x = false;
      let mut rounded_y = false;
      if layer.repeat.x == BackgroundRepeatKeyword::Round {
        tile_w = Self::round_tile_length(origin_rect.width(), tile_w);
        rounded_x = true;
      }
      if layer.repeat.y == BackgroundRepeatKeyword::Round {
        tile_h = Self::round_tile_length(origin_rect.height(), tile_h);
        rounded_y = true;
      }
      if rounded_x ^ rounded_y
        && matches!(
          layer.size,
          BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto)
        )
      {
        let aspect = if img_h != 0.0 { img_w / img_h } else { 1.0 };
        if rounded_x {
          tile_h = tile_w / aspect;
        } else {
          tile_w = tile_h * aspect;
        }
      }

      if !tile_w.is_finite() || !tile_h.is_finite() || tile_w <= 0.0 || tile_h <= 0.0 {
        return None;
      }

      let (offset_x, offset_y) = Self::resolve_background_offset(
        layer.position,
        origin_rect.width(),
        origin_rect.height(),
        tile_w,
        tile_h,
        style.font_size,
        style.root_font_size,
        self.viewport,
      );

      Some((tile_w, tile_h, offset_x, offset_y))
    };

    let compute_tiles =
      |img_w: f32, img_h: f32| -> Option<(f32, f32, f32, f32, TileAxisPlan, TileAxisPlan)> {
        let (tile_w, tile_h, offset_x, offset_y) = compute_tile_metrics(img_w, img_h)?;
        let positions_x = Self::tile_axis_plan(
          layer.repeat.x,
          origin_rect.x(),
          origin_rect.width(),
          tile_w,
          offset_x,
          visible_clip.min_x(),
          visible_clip.max_x(),
        );
        let positions_y = Self::tile_axis_plan(
          layer.repeat.y,
          origin_rect.y(),
          origin_rect.height(),
          tile_h,
          offset_y,
          visible_clip.min_y(),
          visible_clip.max_y(),
        );

        Some((tile_w, tile_h, offset_x, offset_y, positions_x, positions_y))
      };

    let record_pattern_fast_path = |tile_w: f32, tile_h: f32, offset_x: f32, offset_y: f32| {
      if let Some(counter) = self.background_pattern_fast_paths.as_ref() {
        counter.fetch_add(1, Ordering::Relaxed);
      }
      if let Some(counter) = self.background_tiles.as_ref() {
        let positions_x = Self::tile_axis_plan(
          layer.repeat.x,
          origin_rect.x(),
          origin_rect.width(),
          tile_w,
          offset_x,
          visible_clip.min_x(),
          visible_clip.max_x(),
        );
        let positions_y = Self::tile_axis_plan(
          layer.repeat.y,
          origin_rect.y(),
          origin_rect.height(),
          tile_h,
          offset_y,
          visible_clip.min_y(),
          visible_clip.max_y(),
        );
        let tiles = (positions_x.count as u64).saturating_mul(positions_y.count as u64);
        if tiles > 0 {
          counter.fetch_add(tiles, Ordering::Relaxed);
        }
      }
    };

    match bg {
      BackgroundImage::LinearGradient { angle, stops } => {
        let rad = angle.to_radians();
        let dx = rad.sin();
        let dy = -rad.cos();

        if repeat_both_axes {
          if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
            let resolved = Self::normalize_color_stops(
              stops,
              style.color,
              tile_w * dx.abs() + tile_h * dy.abs(),
              style.font_size,
              style.root_font_size,
              self.viewport,
              style.used_dark_color_scheme,
              style.forced_colors,
            );
            if !resolved.is_empty() {
              let stops = Self::gradient_stops(&resolved);

              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              let len = 0.5 * (tile_w * dx.abs() + tile_h * dy.abs());
              let cx = tile_w * 0.5;
              let cy = tile_h * 0.5;
              let start = Point::new(cx - dx * len, cy - dy * len);
              let end = Point::new(cx + dx * len, cy + dy * len);

              self.list.push(DisplayItem::LinearGradientPattern(
                LinearGradientPatternItem {
                  dest_rect: visible_clip,
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  start,
                  end,
                  stops,
                  spread: GradientSpread::Pad,
                },
              ));
            }
          }
        } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
          compute_tiles(0.0, 0.0)
        {
          let resolved = Self::normalize_color_stops(
            stops,
            style.color,
            tile_w * dx.abs() + tile_h * dy.abs(),
            style.font_size,
            style.root_font_size,
            self.viewport,
            style.used_dark_color_scheme,
            style.forced_colors,
          );
          if !resolved.is_empty() {
            let stops = Self::gradient_stops(&resolved);

            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();
            let len = 0.5 * (tile_w * dx.abs() + tile_h * dy.abs());

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let cx = tile_rect.x() + tile_w / 2.0;
                let cy = tile_rect.y() + tile_h / 2.0;
                let start = Point::new(
                  cx - dx * len - intersection.x(),
                  cy - dy * len - intersection.y(),
                );
                let end = Point::new(
                  cx + dx * len - intersection.x(),
                  cy + dy * len - intersection.y(),
                );

                self.emit_background_tile(DisplayItem::LinearGradient(LinearGradientItem {
                  rect: intersection,
                  start,
                  end,
                  stops: stops.clone(),
                  spread: GradientSpread::Pad,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::RepeatingLinearGradient { angle, stops } => {
        let rad = angle.to_radians();
        let dx = rad.sin();
        let dy = -rad.cos();

        if repeat_both_axes {
          if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
            let resolved = Self::normalize_color_stops(
              stops,
              style.color,
              tile_w * dx.abs() + tile_h * dy.abs(),
              style.font_size,
              style.root_font_size,
              self.viewport,
              style.used_dark_color_scheme,
              style.forced_colors,
            );
            if !resolved.is_empty() {
              let stops = Self::gradient_stops(&resolved);

              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              let len = 0.5 * (tile_w * dx.abs() + tile_h * dy.abs());
              let cx = tile_w * 0.5;
              let cy = tile_h * 0.5;
              let start = Point::new(cx - dx * len, cy - dy * len);
              let end = Point::new(cx + dx * len, cy + dy * len);

              self.list.push(DisplayItem::LinearGradientPattern(
                LinearGradientPatternItem {
                  dest_rect: visible_clip,
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  start,
                  end,
                  stops,
                  spread: GradientSpread::Repeat,
                },
              ));
            }
          }
        } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
          compute_tiles(0.0, 0.0)
        {
          let resolved = Self::normalize_color_stops(
            stops,
            style.color,
            tile_w * dx.abs() + tile_h * dy.abs(),
            style.font_size,
            style.root_font_size,
            self.viewport,
            style.used_dark_color_scheme,
            style.forced_colors,
          );
          if !resolved.is_empty() {
            let stops = Self::gradient_stops(&resolved);

            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();
            let len = 0.5 * (tile_w * dx.abs() + tile_h * dy.abs());

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let cx = tile_rect.x() + tile_w / 2.0;
                let cy = tile_rect.y() + tile_h / 2.0;
                let start = Point::new(
                  cx - dx * len - intersection.x(),
                  cy - dy * len - intersection.y(),
                );
                let end = Point::new(
                  cx + dx * len - intersection.x(),
                  cy + dy * len - intersection.y(),
                );

                self.emit_background_tile(DisplayItem::LinearGradient(LinearGradientItem {
                  rect: intersection,
                  start,
                  end,
                  stops: stops.clone(),
                  spread: GradientSpread::Repeat,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::ConicGradient {
        from_angle,
        position,
        stops,
      } => {
        let resolved = Self::normalize_color_stops_unclamped(
          stops,
          style.color,
          1.0,
          style.font_size,
          style.root_font_size,
          self.viewport,
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if !resolved.is_empty() {
          let stops = Self::gradient_stops_unclamped(&resolved);

          if repeat_both_axes {
            if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              let center = Self::resolve_gradient_center(
                Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
                position,
                style.font_size,
                style.root_font_size,
                self.viewport,
              );

              self.list.push(DisplayItem::ConicGradientPattern(
                ConicGradientPatternItem {
                  dest_rect: visible_clip,
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  center,
                  from_angle: *from_angle,
                  stops,
                  repeating: false,
                },
              ));
            }
          } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
            compute_tiles(0.0, 0.0)
          {
            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let center_abs = Self::resolve_gradient_center(
                  Rect::from_xywh(tile_rect.x(), tile_rect.y(), tile_w, tile_h),
                  position,
                  style.font_size,
                  style.root_font_size,
                  self.viewport,
                );
                let center = Point::new(
                  center_abs.x - intersection.x(),
                  center_abs.y - intersection.y(),
                );

                self.emit_background_tile(DisplayItem::ConicGradient(ConicGradientItem {
                  rect: intersection,
                  center,
                  from_angle: *from_angle,
                  stops: stops.clone(),
                  repeating: false,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::RepeatingConicGradient {
        from_angle,
        position,
        stops,
      } => {
        let resolved = Self::normalize_color_stops_unclamped(
          stops,
          style.color,
          1.0,
          style.font_size,
          style.root_font_size,
          self.viewport,
          style.used_dark_color_scheme,
          style.forced_colors,
        );
        if !resolved.is_empty() {
          let stops = Self::gradient_stops_unclamped(&resolved);

          if repeat_both_axes {
            if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              let center = Self::resolve_gradient_center(
                Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
                position,
                style.font_size,
                style.root_font_size,
                self.viewport,
              );

              self.list.push(DisplayItem::ConicGradientPattern(
                ConicGradientPatternItem {
                  dest_rect: visible_clip,
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  center,
                  from_angle: *from_angle,
                  stops,
                  repeating: true,
                },
              ));
            }
          } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
            compute_tiles(0.0, 0.0)
          {
            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let center_abs = Self::resolve_gradient_center(
                  Rect::from_xywh(tile_rect.x(), tile_rect.y(), tile_w, tile_h),
                  position,
                  style.font_size,
                  style.root_font_size,
                  self.viewport,
                );
                let center = Point::new(
                  center_abs.x - intersection.x(),
                  center_abs.y - intersection.y(),
                );

                self.emit_background_tile(DisplayItem::ConicGradient(ConicGradientItem {
                  rect: intersection,
                  center,
                  from_angle: *from_angle,
                  stops: stops.clone(),
                  repeating: true,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::RadialGradient {
        shape,
        size,
        position,
        stops,
      } => {
        if repeat_both_axes {
          if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
            let (cx, cy, radius_x, radius_y) = Self::radial_geometry(
              Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
              position,
              size,
              *shape,
              style.font_size,
              style.root_font_size,
              self.viewport,
            );
            let resolved = Self::normalize_color_stops(
              stops,
              style.color,
              radius_x.max(radius_y),
              style.font_size,
              style.root_font_size,
              self.viewport,
              style.used_dark_color_scheme,
              style.forced_colors,
            );
            if !resolved.is_empty() {
              let stops = Self::gradient_stops(&resolved);

              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              self.list.push(DisplayItem::RadialGradientPattern(
                RadialGradientPatternItem {
                  dest_rect: visible_clip,
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  center: Point::new(cx, cy),
                  radii: Point::new(radius_x, radius_y),
                  stops,
                  spread: GradientSpread::Pad,
                },
              ));
            }
          }
        } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
          compute_tiles(0.0, 0.0)
        {
          let (_cx, _cy, radius_x, radius_y) = Self::radial_geometry(
            Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
            position,
            size,
            *shape,
            style.font_size,
            style.root_font_size,
            self.viewport,
          );
          let resolved = Self::normalize_color_stops(
            stops,
            style.color,
            radius_x.max(radius_y),
            style.font_size,
            style.root_font_size,
            self.viewport,
            style.used_dark_color_scheme,
            style.forced_colors,
          );
          if !resolved.is_empty() {
            let stops = Self::gradient_stops(&resolved);

            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let (cx, cy, radius_x, radius_y) = Self::radial_geometry(
                  Rect::from_xywh(tile_rect.x(), tile_rect.y(), tile_w, tile_h),
                  position,
                  size,
                  *shape,
                  style.font_size,
                  style.root_font_size,
                  self.viewport,
                );
                let center = Point::new(cx - intersection.x(), cy - intersection.y());

                self.emit_background_tile(DisplayItem::RadialGradient(RadialGradientItem {
                  rect: intersection,
                  center,
                  radii: Point::new(radius_x, radius_y),
                  stops: stops.clone(),
                  spread: GradientSpread::Pad,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::RepeatingRadialGradient {
        shape,
        size,
        position,
        stops,
      } => {
        if repeat_both_axes {
          if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
            let (cx, cy, radius_x, radius_y) = Self::radial_geometry(
              Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
              position,
              size,
              *shape,
              style.font_size,
              style.root_font_size,
              self.viewport,
            );
            let resolved = Self::normalize_color_stops(
              stops,
              style.color,
              radius_x.max(radius_y),
              style.font_size,
              style.root_font_size,
              self.viewport,
              style.used_dark_color_scheme,
              style.forced_colors,
            );
            if !resolved.is_empty() {
              let stops = Self::gradient_stops(&resolved);

              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              self.list.push(DisplayItem::RadialGradientPattern(
                RadialGradientPatternItem {
                  dest_rect: visible_clip,
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  center: Point::new(cx, cy),
                  radii: Point::new(radius_x, radius_y),
                  stops,
                  spread: GradientSpread::Repeat,
                },
              ));
            }
          }
        } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
          compute_tiles(0.0, 0.0)
        {
          let (_cx, _cy, radius_x, radius_y) = Self::radial_geometry(
            Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
            position,
            size,
            *shape,
            style.font_size,
            style.root_font_size,
            self.viewport,
          );
          let resolved = Self::normalize_color_stops(
            stops,
            style.color,
            radius_x.max(radius_y),
            style.font_size,
            style.root_font_size,
            self.viewport,
            style.used_dark_color_scheme,
            style.forced_colors,
          );
          if !resolved.is_empty() {
            let stops = Self::gradient_stops(&resolved);

            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let (cx, cy, radius_x, radius_y) = Self::radial_geometry(
                  Rect::from_xywh(tile_rect.x(), tile_rect.y(), tile_w, tile_h),
                  position,
                  size,
                  *shape,
                  style.font_size,
                  style.root_font_size,
                  self.viewport,
                );
                let center = Point::new(cx - intersection.x(), cy - intersection.y());

                self.emit_background_tile(DisplayItem::RadialGradient(RadialGradientItem {
                  rect: intersection,
                  center,
                  radii: Point::new(radius_x, radius_y),
                  stops: stops.clone(),
                  spread: GradientSpread::Repeat,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::Url(src) => {
        'paint_url: {
          let Some(image_cache) = self.image_cache.as_ref() else {
            break 'paint_url;
          };

          let trimmed = trim_ascii_whitespace_start(&src.url);
          let inline_svg = trimmed.starts_with('<');

          let (resolved_src, cached) = if inline_svg {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            trimmed.hash(&mut hasher);
            let cache_key = format!("inline-svg:{:016x}:{}", hasher.finish(), trimmed.len());
            let image = match image_cache.render_svg(trimmed) {
              Ok(image) => image,
              Err(_) => break 'paint_url,
            };
            (cache_key, image)
          } else {
            let resolved_src = image_cache.resolve_url(&src.url);
            let image = match image_cache.load_with_crossorigin_and_referrer_policy(
              &resolved_src,
              CrossOriginAttribute::None,
              None,
            ) {
              Ok(image) => image,
              Err(_) => break 'paint_url,
            };
            (resolved_src, image)
          };

          let orientation = style.image_orientation.resolve(cached.orientation, true);
          let (img_w, img_h, intrinsic_ratio) = if cached.is_vector {
            let Some(svg_markup) = cached.svg_content.as_deref() else {
              break 'paint_url;
            };

            let intrinsic = Self::svg_intrinsic_dimensions_for_css(
              svg_markup,
              style.font_size,
              style.root_font_size,
            );
            let mut w = intrinsic.width;
            let mut h = intrinsic.height;
            let mut ratio = intrinsic.aspect_ratio;
            if orientation.quarter_turns % 2 == 1 {
              std::mem::swap(&mut w, &mut h);
              if let Some(r) = ratio {
                if r.is_finite() && r != 0.0 {
                  ratio = Some(1.0 / r);
                } else {
                  ratio = None;
                }
              }
            }
            (
              w.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.0),
              h.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.0),
              ratio,
            )
          } else {
            let Some((w, h)) = cached.css_dimensions(
              orientation,
              &style.image_resolution,
              self.device_pixel_ratio,
              src.override_resolution,
            ) else {
              break 'paint_url;
            };
            if w <= 0.0 || h <= 0.0 {
              break 'paint_url;
            }
            (w, h, cached.intrinsic_ratio(orientation))
          };

          let (mut tile_w, mut tile_h) = Self::compute_background_size(
            layer,
            style.font_size,
            style.root_font_size,
            self.viewport,
            origin_rect.width(),
            origin_rect.height(),
            img_w,
            img_h,
            intrinsic_ratio,
          );

          let mut rounded_x = false;
          let mut rounded_y = false;
          if layer.repeat.x == BackgroundRepeatKeyword::Round {
            tile_w = Self::round_tile_length(origin_rect.width(), tile_w);
            rounded_x = true;
          }
          if layer.repeat.y == BackgroundRepeatKeyword::Round {
            tile_h = Self::round_tile_length(origin_rect.height(), tile_h);
            rounded_y = true;
          }
          if rounded_x ^ rounded_y
            && matches!(
              layer.size,
              BackgroundSize::Explicit(
                BackgroundSizeComponent::Auto,
                BackgroundSizeComponent::Auto
              )
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

          if tile_w <= 0.0 || tile_h <= 0.0 {
            break 'paint_url;
          }

          let (offset_x, offset_y) = Self::resolve_background_offset(
            layer.position,
            origin_rect.width(),
            origin_rect.height(),
            tile_w,
            tile_h,
            style.font_size,
            style.root_font_size,
            self.viewport,
          );

          let quality = Self::image_filter_quality(Some(style));
          let origin_x = origin_rect.x() + offset_x;
          let origin_y = origin_rect.y() + offset_y;

          // We can emit a single repeating pattern fill (faster + tends to avoid per-tile sampling
          // seams) when either:
          // - the background repeats in both axes (`repeat`/`round`), or
          // - the background repeats in exactly one axis and the visible clip region fits entirely
          //   within the origin tile in the other axis (so repetition there is unobservable).
          //
          // This is especially common for `repeat-x` underline textures where the image is taller
          // than the element's background painting area, meaning only the bottom slice is visible.
          let clip_within_origin_tile = |clip_min: f32, clip_max: f32, origin: f32, tile: f32| -> bool {
            if !clip_min.is_finite() || !clip_max.is_finite() || !origin.is_finite() || !tile.is_finite() {
              return false;
            }
            if tile <= 0.0 {
              return false;
            }
            if clip_max <= clip_min {
              return false;
            }
            // Allow a small epsilon for float noise from layout/pixel snapping.
            let eps = 1e-3;
            clip_min + eps >= origin && clip_max <= origin + tile + eps
          };

          let repeats_x =
            matches!(layer.repeat.x, BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round);
          let repeats_y =
            matches!(layer.repeat.y, BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round);
          let repeat_both_axes = repeats_x
            && repeats_y
            || (repeats_x
              && layer.repeat.y == BackgroundRepeatKeyword::NoRepeat
              && clip_within_origin_tile(
                visible_clip.min_y(),
                visible_clip.max_y(),
                origin_y,
                tile_h,
              ))
            || (repeats_y
              && layer.repeat.x == BackgroundRepeatKeyword::NoRepeat
              && clip_within_origin_tile(
                visible_clip.min_x(),
                visible_clip.max_x(),
                origin_x,
                tile_w,
              ));

          let image: Arc<ImageData> = if cached.is_vector {
            let Some(svg) = cached.svg_content.as_deref() else {
              break 'paint_url;
            };
            fn contains_foreign_object_tag(svg: &str) -> bool {
              const NEEDLE: &[u8] = b"foreignobject";
              let bytes = svg.as_bytes();
              bytes
                .windows(NEEDLE.len())
                .any(|window| window.eq_ignore_ascii_case(NEEDLE))
            }

            let svg_markup = svg;
            let resolved_svg = if contains_foreign_object_tag(svg_markup) {
              let base_dpr = if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0
              {
                self.device_pixel_ratio
              } else {
                1.0
              };
              let foreign_object_dpr =
                crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
                  svg_markup, base_dpr, tile_w, tile_h, img_w, img_h,
                );
              crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
                svg_markup,
                "",
                &self.font_ctx,
                image_cache,
                foreign_object_dpr,
                self.max_iframe_depth,
              )
            } else {
              None
            };
            let svg = resolved_svg.as_deref().unwrap_or(svg_markup);

            let render_w = (tile_w * self.device_pixel_ratio).ceil().max(1.0) as u32;
            let render_h = (tile_h * self.device_pixel_ratio).ceil().max(1.0) as u32;

            let used_resolution = style.image_resolution.used_resolution(
              src.override_resolution,
              cached.resolution,
              self.device_pixel_ratio,
            );
            let cache_url = format!("{resolved_src}:{render_w}x{render_h}");
            let cache_key = ImageKey::new(
              cache_url,
              CrossOriginAttribute::None,
              None,
              orientation,
              true,
              used_resolution,
              self.device_pixel_ratio,
            );

            if let Some(image) = {
              let mut decoded_cache = self
                .decoded_image_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
              decoded_cache.get(&cache_key)
            } {
              image
            } else {
              let pixmap = match image_cache.render_svg_pixmap_at_size(
                svg,
                render_w,
                render_h,
                &resolved_src,
                self.device_pixel_ratio,
              ) {
                Ok(pixmap) => pixmap,
                Err(_) => break 'paint_url,
              };
              // Cloning SVG pixmap bytes into `ImageData` can be large. Charge the allocation before
              // we allocate/copy the pixel buffer.
              if let Some(bytes) = u64::from(pixmap.width())
                .checked_mul(u64::from(pixmap.height()))
                .and_then(|px| px.checked_mul(4))
              {
                if let Err(err) = crate::render_control::reserve_allocation_with(bytes, || {
                  format!(
                    "image data pixel buffer {}x{} url={}",
                    pixmap.width(),
                    pixmap.height(),
                    resolved_src
                  )
                }) {
                  self.error.get_or_insert(err);
                  break 'paint_url;
                }
              }
              let image_data = Arc::new(ImageData::from_pixmap(pixmap.as_ref(), tile_w, tile_h));
              let mut decoded_cache = self
                .decoded_image_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
              if let Some(existing) = decoded_cache.get(&cache_key) {
                existing
              } else {
                decoded_cache.insert(cache_key, image_data.clone());
                image_data
              }
            }
          } else {
            let Some(image) = self.decode_image(
              &src.url,
              Some(style),
              true,
              CrossOriginAttribute::None,
              None,
              false,
              src.override_resolution,
            ) else {
              break 'paint_url;
            };
            image
          };

          let positions_x = Self::tile_axis_plan(
            layer.repeat.x,
            origin_rect.x(),
            origin_rect.width(),
            tile_w,
            offset_x,
            visible_clip.min_x(),
            visible_clip.max_x(),
          );
          let positions_y = Self::tile_axis_plan(
            layer.repeat.y,
            origin_rect.y(),
            origin_rect.height(),
            tile_h,
            offset_y,
            visible_clip.min_y(),
            visible_clip.max_y(),
          );
          let tiles = (positions_x.count as u64).saturating_mul(positions_y.count as u64);

          // Fast path: emit a single pattern fill instead of one item per tile.
          //
          // Only use this when more than one tile is visible. When the tiling plan resolves to a
          // single tile, falling back to `Image` avoids subtle sampling differences vs a pattern
          // shader and does not cost extra display items.
          let use_pattern = repeat_both_axes && tiles > 1;
          if use_pattern {
            if let Some(counter) = self.background_pattern_fast_paths.as_ref() {
              counter.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(counter) = self.background_tiles.as_ref() {
              if tiles > 0 {
                counter.fetch_add(tiles, Ordering::Relaxed);
              }
            }

            self.list.push(DisplayItem::ImagePattern(ImagePatternItem {
              dest_rect: visible_clip,
              image: image.clone(),
              tile_size: Size::new(tile_w, tile_h),
              origin: Point::new(origin_x, origin_y),
              repeat: ImagePatternRepeat::Repeat,
              filter_quality: quality,
            }));
          } else {
            let max_x = visible_clip.max_x();
            let max_y = visible_clip.max_y();

            let mut deadline_counter = 0usize;
            'tiles: for ty in positions_y.iter() {
              for tx in positions_x.iter() {
                if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
                  break 'tiles;
                }
                if tx >= max_x || ty >= max_y {
                  continue;
                }

                let tile_rect = Rect::from_xywh(tx, ty, tile_w, tile_h);
                let Some(intersection) = tile_rect.intersection(visible_clip) else {
                  continue;
                };
                if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
                  continue;
                }

                let scale_x = image.width as f32 / tile_w;
                let scale_y = image.height as f32 / tile_h;
                if !scale_x.is_finite() || !scale_y.is_finite() {
                  continue;
                }

                let src_rect = Rect::from_xywh(
                  (intersection.x() - tile_rect.x()) * scale_x,
                  (intersection.y() - tile_rect.y()) * scale_y,
                  intersection.width() * scale_x,
                  intersection.height() * scale_y,
                );
                let src_rect = {
                  // Avoid baking a crop when the computed source rect matches the full decoded
                  // image (common for unclipped tiles).
                  let full_src_rect = src_rect.x().abs() < 1e-3
                    && src_rect.y().abs() < 1e-3
                    && (src_rect.width() - image.width as f32).abs() < 1e-3
                    && (src_rect.height() - image.height as f32).abs() < 1e-3;
                  if full_src_rect {
                    None
                  } else {
                    // Clamp to the decoded image bounds. Floating-point math can leave tiny
                    // out-of-range values (e.g. `x == image.width`) at tile edges.
                    let x0 = src_rect.x().max(0.0);
                    let y0 = src_rect.y().max(0.0);
                    let x1 = src_rect.max_x().min(image.width as f32);
                    let y1 = src_rect.max_y().min(image.height as f32);
                    let w = x1 - x0;
                    let h = y1 - y0;
                    if w <= 0.0 || h <= 0.0 || !w.is_finite() || !h.is_finite() {
                      continue;
                    }
                    Some(Rect::from_xywh(x0, y0, w, h))
                  }
                };

                self.emit_background_tile(DisplayItem::Image(ImageItem {
                  dest_rect: intersection,
                  image: image.clone(),
                  filter_quality: quality,
                  src_rect,
                }));
              }
            }
          }
        }
      }
      BackgroundImage::None => {}
    }

    if use_blend {
      self.list.push(DisplayItem::PopBlendMode);
    }
    if pushed_text_clip {
      self.list.push(DisplayItem::PopClip);
    }
    if pushed_radii_clip {
      self.list.push(DisplayItem::PopClip);
    }
  }

  fn emit_box_shadows_from_style(&mut self, rect: Rect, style: &ComputedStyle, inset: bool) {
    if style.box_shadow.is_empty() {
      return;
    }
    let rects = Self::background_rects(rect, style, self.viewport);
    let outer_radii = Self::border_radii(rect, style).clamped(rect.width(), rect.height());
    let inner_radii = if Self::border_radius_is_zero(style) {
      crate::paint::display_list::BorderRadii::ZERO
    } else {
      Self::resolve_clip_radii(
        style,
        &rects,
        BackgroundBox::PaddingBox,
        self.viewport,
        self.build_breakdown.as_deref(),
      )
    };
    let base_rect = if inset { rects.padding } else { rects.border };
    let radii = if inset { inner_radii } else { outer_radii };
    self.emit_box_shadows_from_style_with_base(base_rect, radii, rect.width(), style, inset);
  }

  fn emit_box_shadows_from_style_with_base(
    &mut self,
    base_rect: Rect,
    radii: crate::paint::display_list::BorderRadii,
    percentage_base: f32,
    style: &ComputedStyle,
    inset: bool,
  ) {
    // CSS Backgrounds and Borders: box-shadow lists are ordered front-to-back (first is on top),
    // but we paint back-to-front so later shadows don't cover earlier ones.
    for shadow in style.box_shadow.iter().rev() {
      if shadow.inset != inset {
        continue;
      }
      let offset_x = Self::resolve_length_for_paint(
        &shadow.offset_x,
        style.font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      );
      let offset_y = Self::resolve_length_for_paint(
        &shadow.offset_y,
        style.font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      );
      let blur_radius = Self::resolve_length_for_paint(
        &shadow.blur_radius,
        style.font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      )
      .max(0.0);
      let spread = Self::resolve_length_for_paint(
        &shadow.spread_radius,
        style.font_size,
        style.root_font_size,
        percentage_base,
        self.viewport,
      )
      .max(-1e6);

      self.list.push(DisplayItem::BoxShadow(BoxShadowItem {
        rect: base_rect,
        radii,
        offset: Point::new(offset_x, offset_y),
        blur_radius,
        spread_radius: spread,
        color: shadow.color,
        inset,
      }));
    }
  }

  fn fieldset_legend_border_gap(
    &self,
    fragment: &crate::tree::fragment_tree::FragmentNode,
    rect: Rect,
    style: &crate::style::ComputedStyle,
  ) -> Option<BorderGap> {
    let legend = fragment.children.iter().find(|child| {
      child
        .style
        .as_deref()
        .is_some_and(|s| s.shrink_to_fit_inline_size)
    })?;

    let horizontal = block_axis_is_horizontal(style.writing_mode);
    let positive = block_axis_positive(style.writing_mode);
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

  fn emit_border_from_style(&mut self, rect: Rect, style: &ComputedStyle, gap: Option<BorderGap>) {
    if matches!(style.border_image.source, BorderImageSource::None)
      && !Self::border_style_visible(style.border_top_style)
      && !Self::border_style_visible(style.border_right_style)
      && !Self::border_style_visible(style.border_bottom_style)
      && !Self::border_style_visible(style.border_left_style)
    {
      return;
    }

    let widths = (
      Self::resolve_length_for_paint(
        &style.used_border_top_width(),
        style.font_size,
        style.root_font_size,
        rect.width(),
        self.viewport,
      ),
      Self::resolve_length_for_paint(
        &style.used_border_right_width(),
        style.font_size,
        style.root_font_size,
        rect.width(),
        self.viewport,
      ),
      Self::resolve_length_for_paint(
        &style.used_border_bottom_width(),
        style.font_size,
        style.root_font_size,
        rect.width(),
        self.viewport,
      ),
      Self::resolve_length_for_paint(
        &style.used_border_left_width(),
        style.font_size,
        style.root_font_size,
        rect.width(),
        self.viewport,
      ),
    );

    let sides = (
      BorderSide {
        width: widths.0,
        style: style.border_top_style,
        color: style.border_top_color,
      },
      BorderSide {
        width: widths.1,
        style: style.border_right_style,
        color: style.border_right_color,
      },
      BorderSide {
        width: widths.2,
        style: style.border_bottom_style,
        color: style.border_bottom_color,
      },
      BorderSide {
        width: widths.3,
        style: style.border_left_style,
        color: style.border_left_color,
      },
    );

    let border_image = match &style.border_image.source {
      BorderImageSource::Image(bg) => {
        let source = match bg.as_ref() {
          BackgroundImage::Url(src) => self
            .decode_image(
              &src.url,
              Some(style),
              true,
              CrossOriginAttribute::None,
              None,
              false,
              src.override_resolution,
            )
            .map(|image| BorderImageSourceItem::Raster((*image).clone())),
          BackgroundImage::LinearGradient { .. }
          | BackgroundImage::RepeatingLinearGradient { .. }
          | BackgroundImage::RadialGradient { .. }
          | BackgroundImage::RepeatingRadialGradient { .. }
          | BackgroundImage::ConicGradient { .. }
          | BackgroundImage::RepeatingConicGradient { .. } => {
            Some(BorderImageSourceItem::Generated(Box::new((**bg).clone())))
          }
          BackgroundImage::None => None,
        };
        source.map(|source| BorderImageItem {
          source,
          slice: style.border_image.slice.clone(),
          width: style.border_image.width.clone(),
          outset: style.border_image.outset.clone(),
          repeat: style.border_image.repeat,
          current_color: style.color,
          used_dark_color_scheme: style.used_dark_color_scheme,
          forced_colors: style.forced_colors,
          font_size: style.font_size,
          root_font_size: style.root_font_size,
          viewport: self.viewport,
        })
      }
      BorderImageSource::None => None,
    };
    // `border-image` paints even when the border stroke itself is fully transparent, so consider
    // the border visible whenever a border image is present.
    let any_visible = border_image.is_some()
      || Self::border_side_visible(&sides.0)
      || Self::border_side_visible(&sides.1)
      || Self::border_side_visible(&sides.2)
      || Self::border_side_visible(&sides.3);
    if !any_visible {
      return;
    }

    // A border-image can be visible even when the border colors are fully transparent (author
    // patterns often set `border: <width> solid transparent` to establish the border box).
    let any_visible = border_image.is_some()
      || Self::border_side_visible(&sides.0)
      || Self::border_side_visible(&sides.1)
      || Self::border_side_visible(&sides.2)
      || Self::border_side_visible(&sides.3);
    if !any_visible {
      return;
    }

    let radii = Self::border_radii(rect, style).clamped(rect.width(), rect.height());
    self.list.push(DisplayItem::Border(Box::new(BorderItem {
      rect,
      top: sides.0,
      right: sides.1,
      bottom: sides.2,
      left: sides.3,
      image: border_image,
      radii,
      gap,
    })));
  }

  fn emit_outline(&mut self, rect: Rect, style: &ComputedStyle) {
    let ow = Self::resolve_length_for_paint(
      &style.outline_width,
      style.font_size,
      style.root_font_size,
      rect.width(),
      self.viewport,
    )
    .max(0.0);
    let outline_style = style.outline_style.to_border_style();
    if ow <= 0.0
      || matches!(
        outline_style,
        crate::style::types::BorderStyle::None | crate::style::types::BorderStyle::Hidden
      )
    {
      return;
    }
    let offset = Self::resolve_length_for_paint(
      &style.outline_offset,
      style.font_size,
      style.root_font_size,
      rect.width(),
      self.viewport,
    );
    let radii = Self::border_radii(rect, style).clamped(rect.width(), rect.height());
    let (color, invert) = style.outline_color.resolve(style.color);
    if ow > 0.0 && !color.is_transparent() {
      self.list.push(DisplayItem::Outline(OutlineItem {
        rect,
        radii,
        width: ow,
        style: outline_style,
        color,
        offset,
        invert,
      }));
    }
  }

  /// Begins an opacity layer
  pub fn push_opacity(&mut self, opacity: f32) {
    self
      .list
      .push(DisplayItem::PushOpacity(OpacityItem { opacity }));
  }

  /// Ends an opacity layer
  pub fn pop_opacity(&mut self) {
    self.list.push(DisplayItem::PopOpacity);
  }

  /// Begins a clip region
  pub fn push_clip(&mut self, rect: Rect) {
    self.list.push(DisplayItem::PushClip(ClipItem {
      shape: ClipShape::Rect { rect, radii: None },
    }));
  }

  /// Ends a clip region
  pub fn pop_clip(&mut self) {
    self.list.push(DisplayItem::PopClip);
  }

  fn emit_shaped_runs(
    &mut self,
    runs: &[ShapedRun],
    color: Rgba,
    baseline_y: f32,
    start_x: f32,
    shadows: &[TextShadowItem],
    style: Option<&ComputedStyle>,
    inline_vertical: bool,
    emphasis_offset: TextEmphasisOffset,
  ) {
    let mut pen_x = start_x;
    let allow_subpixel_aa = style.map(|s| s.allow_subpixel_aa).unwrap_or(true);
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let origin_x = pen_x;
      let (glyphs, cached_bounds) = self.glyphs_from_run(run, origin_x, baseline_y);
      let font_id = self.font_id_from_run(run);
      let (stroke_width, stroke_color) = style
        .map(|s| self.resolve_webkit_text_stroke_for_run(s, run.font_size))
        .unwrap_or_default();
      let emphasis = style.and_then(|s| {
        self.build_emphasis(
          run,
          s,
          origin_x,
          baseline_y,
          inline_vertical,
          emphasis_offset,
        )
      });
      let variations: Vec<FontVariation> = run
        .variations
        .iter()
        .map(|v| FontVariation::new(v.tag, v.value))
        .collect();

      let item = TextItem {
        origin: Point::new(origin_x, baseline_y),
        cached_bounds: Some(cached_bounds),
        glyphs,
        color,
        allow_subpixel_aa,
        stroke_width,
        stroke_color,
        font_smoothing: style.map(|s| s.font_smoothing).unwrap_or_default(),
        palette_index: run.palette_index,
        palette_overrides: Arc::clone(&run.palette_overrides),
        palette_override_hash: run.palette_override_hash,
        rotation: run.rotation,
        scale: run.scale,
        shadows: shadows.to_vec(),
        font_size: run.font_size,
        advance_width: run.advance,
        font_id,
        font: Some(run.font.clone()),
        variations,
        synthetic_bold: run.synthetic_bold,
        synthetic_oblique: run.synthetic_oblique,
        emphasis,
        decorations: Vec::new(),
        ..Default::default()
      };
      self.list.push(DisplayItem::Text(item));

      pen_x += run.advance;
    }
  }

  fn collect_shaped_runs_for_text_clip(
    &mut self,
    out: &mut Vec<TextItem>,
    runs: &[ShapedRun],
    baseline_y: f32,
    start_x: f32,
    allow_subpixel_aa: bool,
  ) {
    let mut pen_x = start_x;
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let origin_x = pen_x;
      let (glyphs, cached_bounds) = self.glyphs_from_run(run, origin_x, baseline_y);
      let font_id = self.font_id_from_run(run);
      let variations: Vec<FontVariation> = run
        .variations
        .iter()
        .map(|v| FontVariation::new(v.tag, v.value))
        .collect();
      out.push(TextItem {
        origin: Point::new(origin_x, baseline_y),
        cached_bounds: Some(cached_bounds),
        glyphs,
        color: Rgba::WHITE,
        allow_subpixel_aa,
        palette_index: run.palette_index,
        palette_overrides: Arc::clone(&run.palette_overrides),
        palette_override_hash: run.palette_override_hash,
        rotation: run.rotation,
        scale: run.scale,
        shadows: Vec::new(),
        font_size: run.font_size,
        advance_width: run.advance,
        font_id,
        font: Some(run.font.clone()),
        variations,
        synthetic_bold: run.synthetic_bold,
        synthetic_oblique: run.synthetic_oblique,
        emphasis: None,
        decorations: Vec::new(),
        ..Default::default()
      });
      pen_x += run.advance;
    }
  }

  fn emit_shaped_runs_vertical(
    &mut self,
    runs: &[ShapedRun],
    color: Rgba,
    block_baseline: f32,
    inline_start: f32,
    shadows: &[TextShadowItem],
    style: Option<&ComputedStyle>,
    emphasis_offset: TextEmphasisOffset,
  ) {
    let mut pen_inline = inline_start;
    let allow_subpixel_aa = style.map(|s| s.allow_subpixel_aa).unwrap_or(true);
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let run_origin_inline = pen_inline;
      let (glyphs, cached_bounds) =
        self.glyphs_from_run_vertical(run, block_baseline, run_origin_inline);
      let font_id = self.font_id_from_run(run);
      let (stroke_width, stroke_color) = style
        .map(|s| self.resolve_webkit_text_stroke_for_run(s, run.font_size))
        .unwrap_or_default();
      let emphasis = style.and_then(|s| {
        self.build_emphasis(
          run,
          s,
          run_origin_inline,
          block_baseline,
          true,
          emphasis_offset,
        )
      });
      let variations: Vec<FontVariation> = run
        .variations
        .iter()
        .map(|v| FontVariation::new(v.tag, v.value))
        .collect();

      let item = TextItem {
        origin: Point::new(block_baseline, run_origin_inline),
        cached_bounds: Some(cached_bounds),
        glyphs,
        color,
        allow_subpixel_aa,
        stroke_width,
        stroke_color,
        font_smoothing: style.map(|s| s.font_smoothing).unwrap_or_default(),
        palette_index: run.palette_index,
        palette_overrides: Arc::clone(&run.palette_overrides),
        palette_override_hash: run.palette_override_hash,
        rotation: run.rotation,
        scale: run.scale,
        shadows: shadows.to_vec(),
        font_size: run.font_size,
        advance_width: run.advance,
        font_id,
        font: Some(run.font.clone()),
        variations,
        synthetic_bold: run.synthetic_bold,
        synthetic_oblique: run.synthetic_oblique,
        emphasis,
        decorations: Vec::new(),
        ..Default::default()
      };
      self.list.push(DisplayItem::Text(item));

      pen_inline += run.advance;
    }
  }

  fn collect_shaped_runs_for_text_clip_vertical(
    &mut self,
    out: &mut Vec<TextItem>,
    runs: &[ShapedRun],
    block_baseline: f32,
    inline_start: f32,
    allow_subpixel_aa: bool,
  ) {
    let mut pen_inline = inline_start;
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let run_origin_inline = pen_inline;
      let (glyphs, cached_bounds) =
        self.glyphs_from_run_vertical(run, block_baseline, run_origin_inline);
      let font_id = self.font_id_from_run(run);
      let variations: Vec<FontVariation> = run
        .variations
        .iter()
        .map(|v| FontVariation::new(v.tag, v.value))
        .collect();
      out.push(TextItem {
        origin: Point::new(block_baseline, run_origin_inline),
        cached_bounds: Some(cached_bounds),
        glyphs,
        color: Rgba::WHITE,
        allow_subpixel_aa,
        palette_index: run.palette_index,
        palette_overrides: Arc::clone(&run.palette_overrides),
        palette_override_hash: run.palette_override_hash,
        rotation: run.rotation,
        scale: run.scale,
        shadows: Vec::new(),
        font_size: run.font_size,
        advance_width: run.advance,
        font_id,
        font: Some(run.font.clone()),
        variations,
        synthetic_bold: run.synthetic_bold,
        synthetic_oblique: run.synthetic_oblique,
        emphasis: None,
        decorations: Vec::new(),
        ..Default::default()
      });
      pen_inline += run.advance;
    }
  }

  fn emit_list_marker_runs(
    &mut self,
    runs: &[ShapedRun],
    color: Rgba,
    baseline_y: f32,
    start_x: f32,
    shadows: &[TextShadowItem],
    style: Option<&ComputedStyle>,
    inline_vertical: bool,
    emphasis_offset: TextEmphasisOffset,
  ) {
    let mut pen_x = start_x;
    let allow_subpixel_aa = style.map(|s| s.allow_subpixel_aa).unwrap_or(true);
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let origin_x = pen_x;
      let (glyphs, cached_bounds) = self.glyphs_from_run(run, origin_x, baseline_y);
      let font_id = self.font_id_from_run(run);
      let (stroke_width, stroke_color) = style
        .map(|s| self.resolve_webkit_text_stroke_for_run(s, run.font_size))
        .unwrap_or_default();
      let emphasis = style.and_then(|s| {
        self.build_emphasis(
          run,
          s,
          origin_x,
          baseline_y,
          inline_vertical,
          emphasis_offset,
        )
      });
      let variations: Vec<FontVariation> = run
        .variations
        .iter()
        .map(|v| FontVariation::new(v.tag, v.value))
        .collect();
      let item = ListMarkerItem {
        origin: Point::new(origin_x, baseline_y),
        cached_bounds: Some(cached_bounds),
        glyphs,
        font_size: run.font_size,
        color,
        allow_subpixel_aa,
        stroke_width,
        stroke_color,
        font_smoothing: style.map(|s| s.font_smoothing).unwrap_or_default(),
        shadows: shadows.to_vec(),
        advance_width: run.advance,
        font_id,
        palette_index: run.palette_index,
        palette_overrides: Arc::clone(&run.palette_overrides),
        palette_override_hash: run.palette_override_hash,
        rotation: run.rotation,
        scale: run.scale,
        font: Some(run.font.clone()),
        variations,
        synthetic_bold: run.synthetic_bold,
        synthetic_oblique: run.synthetic_oblique,
        emphasis,
        background: None,
        ..Default::default()
      };
      self.list.push(DisplayItem::ListMarker(item));

      pen_x += run.advance;
    }
  }

  fn emit_list_marker_runs_vertical(
    &mut self,
    runs: &[ShapedRun],
    color: Rgba,
    block_baseline: f32,
    inline_start: f32,
    shadows: &[TextShadowItem],
    style: Option<&ComputedStyle>,
    emphasis_offset: TextEmphasisOffset,
  ) {
    let mut pen_inline = inline_start;
    let allow_subpixel_aa = style.map(|s| s.allow_subpixel_aa).unwrap_or(true);
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let run_origin_inline = pen_inline;
      let (glyphs, cached_bounds) =
        self.glyphs_from_run_vertical(run, block_baseline, run_origin_inline);
      let font_id = self.font_id_from_run(run);
      let (stroke_width, stroke_color) = style
        .map(|s| self.resolve_webkit_text_stroke_for_run(s, run.font_size))
        .unwrap_or_default();
      let emphasis = style.and_then(|s| {
        self.build_emphasis(
          run,
          s,
          run_origin_inline,
          block_baseline,
          true,
          emphasis_offset,
        )
      });
      let variations: Vec<FontVariation> = run
        .variations
        .iter()
        .map(|v| FontVariation::new(v.tag, v.value))
        .collect();
      let item = ListMarkerItem {
        origin: Point::new(block_baseline, run_origin_inline),
        cached_bounds: Some(cached_bounds),
        glyphs,
        font_size: run.font_size,
        color,
        allow_subpixel_aa,
        stroke_width,
        stroke_color,
        font_smoothing: style.map(|s| s.font_smoothing).unwrap_or_default(),
        shadows: shadows.to_vec(),
        advance_width: run.advance,
        font_id,
        palette_index: run.palette_index,
        palette_overrides: Arc::clone(&run.palette_overrides),
        palette_override_hash: run.palette_override_hash,
        rotation: run.rotation,
        scale: run.scale,
        font: Some(run.font.clone()),
        variations,
        synthetic_bold: run.synthetic_bold,
        synthetic_oblique: run.synthetic_oblique,
        emphasis,
        background: None,
        ..Default::default()
      };
      self.list.push(DisplayItem::ListMarker(item));

      pen_inline += run.advance;
    }
  }

  fn emit_text_decorations(
    &mut self,
    style: &ComputedStyle,
    runs: Option<&[ShapedRun]>,
    inline_start: f32,
    inline_len: f32,
    block_baseline: f32,
    inline_vertical: bool,
    clip: Option<(f32, f32)>,
  ) {
    if !style.applied_text_decorations.is_empty() {
      self.emit_text_decorations_for_decorations(
        style,
        style.applied_text_decorations.as_slice(),
        runs,
        inline_start,
        inline_len,
        block_baseline,
        inline_vertical,
        clip,
      );
      return;
    }

    if style.text_decoration.lines.is_empty() {
      return;
    }

    let decorations = [ResolvedTextDecoration {
      decoration: style.text_decoration.clone(),
      skip_ink: style.text_decoration_skip_ink,
      underline_offset: style.text_underline_offset,
      underline_position: style.text_underline_position,
    }];
    self.emit_text_decorations_for_decorations(
      style,
      &decorations,
      runs,
      inline_start,
      inline_len,
      block_baseline,
      inline_vertical,
      clip,
    );
  }

  fn emit_text_decorations_for_decorations(
    &mut self,
    style: &ComputedStyle,
    decorations: &[ResolvedTextDecoration],
    runs: Option<&[ShapedRun]>,
    inline_start: f32,
    inline_len: f32,
    block_baseline: f32,
    inline_vertical: bool,
    clip: Option<(f32, f32)>,
  ) {
    if inline_len <= 0.0 {
      return;
    }
    let clip =
      clip.map(|(start, end)| (start.max(0.0).min(inline_len), end.max(0.0).min(inline_len)));
    if let Some((start, end)) = clip {
      if end <= start + f32::EPSILON {
        return;
      }
    }

    if decorations.is_empty() {
      return;
    }

    let clip_segments = |segments: Option<Vec<(f32, f32)>>| -> Option<Vec<(f32, f32)>> {
      let Some((clip_start, clip_end)) = clip else {
        return segments;
      };
      if clip_start <= 0.001 && (inline_len - clip_end) <= 0.001 {
        return segments;
      }
      match segments {
        Some(existing) => Some(
          existing
            .into_iter()
            .filter_map(|(start, end)| {
              let start = start.max(clip_start);
              let end = end.min(clip_end);
              (end > start).then_some((start, end))
            })
            .collect(),
        ),
        None => Some(vec![(clip_start, clip_end)]),
      }
    };

    let Some(metrics) = self.decoration_metrics(runs, style) else {
      return;
    };

    let mut paints = Vec::new();
    let mut min_block = f32::INFINITY;
    let mut max_block = f32::NEG_INFINITY;

    for deco in decorations {
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
        stroke: DecorationStroke,
      }

      // CSS Text Decoration Level 4: spelling/grammar error decorations are UA-defined, and the UA
      // must disregard other `text-decoration-*` sub-properties and paint-affecting properties
      // (text-decoration-color/style/thickness, underline-position/offset, skip-ink, etc).
      const SPELLING_ERROR_COLOR: Rgba = Rgba::RED;
      // CSS `green` is #008000; keep the grammar underline closer to typical browser output than
      // `Rgba::GREEN` (which is #00FF00 / `lime`).
      const GRAMMAR_ERROR_COLOR: Rgba = Rgba::rgb(0, 128, 0);

      let lines = deco.decoration.lines;

      let decoration_color = deco.decoration.color.unwrap_or(style.color);
      let standard_visible = decoration_color.alpha_u8() != 0;

      let used_thickness =
        self.resolve_text_decoration_thickness_override(deco.decoration.thickness, style);

      let mut paint = DecorationPaint {
        style: deco.decoration.style,
        color: decoration_color,
        underline: None,
        overline: None,
        line_through: None,
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
          TextDecorationSkipInk::Auto | TextDecorationSkipInk::All
        ) {
          runs.map(|r| {
            let stroke_width = r
              .iter()
              .map(|run| self.resolve_webkit_text_stroke_for_run(style, run.font_size).0)
              .fold(0.0_f32, f32::max);
            self.build_underline_segments(
              r,
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
          stroke: DecorationStroke {
            center,
            thickness,
            segments: clip_segments(segments),
          },
        });
      }

      if lines.contains(TextDecorationLine::SPELLING_ERROR) {
        let thickness = metrics.underline_thickness;
        let center = self.underline_center(
          &metrics,
          TextUnderlinePosition::Auto,
          TextUnderlineOffset::Auto,
          thickness,
          block_baseline,
          inline_vertical,
          style,
        );
        underline_like.push(UnderlineDecoration {
          kind: UnderlineDecorationKind::SpellingError,
          style: TextDecorationStyle::Wavy,
          color: SPELLING_ERROR_COLOR,
          stroke: DecorationStroke {
            center,
            thickness,
            segments: clip_segments(None),
          },
        });
      }

      if lines.contains(TextDecorationLine::GRAMMAR_ERROR) {
        let thickness = metrics.underline_thickness;
        let center = self.underline_center(
          &metrics,
          TextUnderlinePosition::Auto,
          TextUnderlineOffset::Auto,
          thickness,
          block_baseline,
          inline_vertical,
          style,
        );
        underline_like.push(UnderlineDecoration {
          kind: UnderlineDecorationKind::GrammarError,
          style: TextDecorationStyle::Wavy,
          color: GRAMMAR_ERROR_COLOR,
          stroke: DecorationStroke {
            center,
            thickness,
            segments: clip_segments(None),
          },
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
          let delta = underline_like[idx].stroke.center - baseline;
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
            let da = (underline_like[a].stroke.center - baseline).abs();
            let db = (underline_like[b].stroke.center - baseline).abs();
            da.partial_cmp(&db)
              .unwrap_or(std::cmp::Ordering::Equal)
              .then_with(|| underline_like[a].kind.priority().cmp(&underline_like[b].kind.priority()))
          });

          let mut prev_pos: Option<f32> = None;
          let mut prev_half: f32 = 0.0;
          let mut prev_thickness: f32 = 0.0;
          for &idx in indices.iter() {
            let delta = underline_like[idx].stroke.center - baseline;
            let pos = delta.abs();
            let thickness = underline_like[idx].stroke.thickness;
            let half_extent = Self::stroke_half_extent(underline_like[idx].style, thickness);
            let adjusted = match prev_pos {
              Some(prev) => {
                let gap = prev_thickness.max(thickness) * 0.5;
                let min_pos = prev + prev_half + half_extent + gap;
                pos.max(min_pos)
              }
              None => pos,
            };
            underline_like[idx].stroke.center = baseline + sign * adjusted;
            prev_pos = Some(adjusted);
            prev_half = half_extent;
            prev_thickness = thickness;
          }
        };

        adjust_group(&mut pos, 1.0);
        adjust_group(&mut neg, -1.0);
      }

      // Finish standard underline + bounds updates.
      for underline in underline_like.iter() {
        let half_extent = Self::stroke_half_extent(underline.style, underline.stroke.thickness);
        min_block = min_block.min(underline.stroke.center - half_extent);
        max_block = max_block.max(underline.stroke.center + half_extent);
        match underline.kind {
          UnderlineDecorationKind::Underline => paint.underline = Some(underline.stroke.clone()),
          UnderlineDecorationKind::SpellingError | UnderlineDecorationKind::GrammarError => {}
        }
      }

      if standard_visible && lines.contains(TextDecorationLine::OVERLINE) {
        let thickness = used_thickness.unwrap_or(metrics.underline_thickness);
        let center = block_baseline - metrics.ascent;
        paint.overline = Some(DecorationStroke {
          center,
          thickness,
          segments: clip_segments(None),
        });
        let half_extent = Self::stroke_half_extent(deco.decoration.style, thickness);
        min_block = min_block.min(center - half_extent);
        max_block = max_block.max(center + half_extent);
      }
      if standard_visible && lines.contains(TextDecorationLine::LINE_THROUGH) {
        let thickness = used_thickness.unwrap_or(metrics.strike_thickness);
        let center = block_baseline - metrics.strike_pos;
        paint.line_through = Some(DecorationStroke {
          center,
          thickness,
          segments: clip_segments(None),
        });
        let half_extent = Self::stroke_half_extent(deco.decoration.style, thickness);
        min_block = min_block.min(center - half_extent);
        max_block = max_block.max(center + half_extent);
      }

      if paint.underline.is_some() || paint.overline.is_some() || paint.line_through.is_some() {
        paints.push(paint);
      }

      for underline in underline_like {
        match underline.kind {
          UnderlineDecorationKind::SpellingError | UnderlineDecorationKind::GrammarError => {
            paints.push(DecorationPaint {
              style: underline.style,
              color: underline.color,
              underline: Some(underline.stroke),
              overline: None,
              line_through: None,
            });
          }
          UnderlineDecorationKind::Underline => {}
        }
      }
    }

    if paints.is_empty() {
      return;
    }

    let block_span = (max_block - min_block).max(0.0);
    let bounds = if inline_vertical {
      Rect::from_xywh(min_block, inline_start, block_span, inline_len)
    } else {
      Rect::from_xywh(inline_start, min_block, inline_len, block_span)
    };
    self
      .list
      .push(DisplayItem::TextDecoration(TextDecorationItem {
        bounds,
        line_start: inline_start,
        line_width: inline_len,
        inline_vertical,
        decorations: paints,
      }));
  }

  fn split_text_decorations<'a>(
    style: &'a ComputedStyle,
    fallback: &'a mut Option<[ResolvedTextDecoration; 1]>,
  ) -> (&'a [ResolvedTextDecoration], &'a [ResolvedTextDecoration]) {
    let all = if !style.applied_text_decorations.is_empty() {
      style.applied_text_decorations.as_slice()
    } else if !style.text_decoration.lines.is_empty() {
      *fallback = Some([ResolvedTextDecoration {
        decoration: style.text_decoration.clone(),
        skip_ink: style.text_decoration_skip_ink,
        underline_offset: style.text_underline_offset,
        underline_position: style.text_underline_position,
      }]);
      match fallback.as_ref() {
        Some(arr) => arr.as_slice(),
        None => &[],
      }
    } else {
      &[]
    };

    if all.is_empty() {
      return (&[], &[]);
    }

    if style.text_decoration.lines.is_empty() {
      return (all, &[]);
    }

    // When `text-decoration` is specified on this element, cascade propagation appends it to the
    // end of `applied_text_decorations`. This allows callers to treat the final entry as the
    // decoration originating from this box (which should always skip the box decoration area).
    let split = all.len().saturating_sub(1);
    all.split_at(split)
  }

  fn resolve_text_decoration_thickness_override(
    &self,
    thickness: TextDecorationThickness,
    style: &ComputedStyle,
  ) -> Option<f32> {
    match thickness {
      TextDecorationThickness::Auto => {
        // `text-decoration-thickness: auto` is UA-defined. Keep legacy behavior stable by mapping
        // to a simple font-size-relative default (clamped to >=1px) rather than relying on
        // font-provided underline thickness, which can vary across fonts/platforms.
        let font_size = if style.font_size.is_finite() {
          style.font_size.max(0.0)
        } else {
          0.0
        };
        Some((font_size * 0.1).max(1.0))
      }
      // `from-font` uses per-font underline/strikeout thickness, so let the caller fall back to
      // `DecorationMetrics::{underline_thickness,strike_thickness}`.
      TextDecorationThickness::FromFont => None,
      TextDecorationThickness::Length(l) => {
        if l.unit == LengthUnit::Percent {
          l.resolve_against(style.font_size)
        } else if l.unit.is_viewport_relative() {
          self.viewport.and_then(|(vw, vh)| {
            l.resolve_with_viewport_for_writing_mode(vw, vh, style.writing_mode)
          })
        } else {
          Some(resolve_font_relative_length(l, style, &self.font_ctx))
        }
      }
    }
  }

  fn stroke_half_extent(style: TextDecorationStyle, thickness: f32) -> f32 {
    match style {
      TextDecorationStyle::Double => thickness * 2.5,
      TextDecorationStyle::Wavy => thickness * 2.0,
      _ => thickness * 0.5,
    }
  }

  fn resolve_underline_offset_length(
    &self,
    offset: TextUnderlineOffset,
    style: &ComputedStyle,
  ) -> Option<f32> {
    match offset {
      TextUnderlineOffset::Auto => None,
      TextUnderlineOffset::Length(l) => Some(if l.unit == LengthUnit::Percent {
        l.resolve_against(style.font_size).unwrap_or(0.0)
      } else if l.unit.is_font_relative() {
        resolve_font_relative_length(l, style, &self.font_ctx)
      } else if l.unit.is_viewport_relative() {
        self
          .viewport
          .and_then(|(vw, vh)| l.resolve_with_viewport_for_writing_mode(vw, vh, style.writing_mode))
          .unwrap_or_else(|| l.to_px())
      } else if l.unit.is_absolute() {
        l.to_px()
      } else {
        l.value * style.font_size
      }),
    }
  }

  fn underline_auto_offset_value(
    &self,
    metrics: &DecorationMetrics,
    position: TextUnderlinePosition,
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
      TextUnderlinePosition::FromFont if metrics.has_font_underline_metrics => 0.0,
      TextUnderlinePosition::Under
      | TextUnderlinePosition::UnderLeft
      | TextUnderlinePosition::UnderRight => (-metrics.descent - underline_pos).max(0.0),
      TextUnderlinePosition::Left if inline_vertical => {
        (-metrics.descent - underline_pos).max(0.0)
      }
      TextUnderlinePosition::Right if inline_vertical => 0.0,
      _ => (-underline_pos).max(0.0),
    }
  }

  fn underline_center(
    &self,
    metrics: &DecorationMetrics,
    position: TextUnderlinePosition,
    offset: TextUnderlineOffset,
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
        TextUnderlinePosition::UnderLeft => TextUnderlinePosition::Left,
        TextUnderlinePosition::UnderRight => TextUnderlinePosition::Right,
        _ => position,
      }
    } else {
      match position {
        TextUnderlinePosition::Left | TextUnderlinePosition::Right => TextUnderlinePosition::Auto,
        _ => position,
      }
    };

    let over_positioned = inline_vertical && matches!(position, TextUnderlinePosition::Right);
    let dir = if over_positioned { 1.0 } else { -1.0 };
    let zero_pos = match position {
      TextUnderlinePosition::Auto => 0.0,
      TextUnderlinePosition::FromFont => {
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
      TextUnderlinePosition::Under
      | TextUnderlinePosition::UnderLeft
      | TextUnderlinePosition::UnderRight
      | TextUnderlinePosition::Left => -metrics.descent,
      TextUnderlinePosition::Right => {
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
    // The spec defines `text-underline-offset` relative to a zero position determined by
    // `text-underline-position`, with the underline stroke aligned to the outside of that
    // position (extending thickness in the positive direction only).
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

  fn decoration_metrics(
    &self,
    runs: Option<&[ShapedRun]>,
    style: &ComputedStyle,
  ) -> Option<DecorationMetrics> {
    // Prefer using the actual shaped runs (including fallback fonts and size-adjust / metric
    // overrides) so underline/overline/strike-through placement matches inline layout.
    let mut metrics_source = runs.and_then(|rs| {
      rs.iter().find_map(|run| {
        let scaled = self.font_ctx.get_scaled_metrics_with_variations(
          run.font.as_ref(),
          run.font_size,
          &run.variations,
        )?;
        let coords: Vec<_> = run.variations.iter().map(|v| (v.tag, v.value)).collect();
        let raw = if coords.is_empty() {
          run.font.metrics()
        } else {
          run
            .font
            .metrics_with_variations(&coords)
            .or_else(|_| run.font.metrics())
        }
        .ok()?;
        Some((scaled, raw))
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
          let scaled =
            self
              .font_ctx
              .get_scaled_metrics_with_variations(&font, used_font_size, &variations)?;
          let coords: Vec<_> = variations.iter().map(|v| (v.tag, v.value)).collect();
          let raw = if coords.is_empty() {
            font.metrics()
          } else {
            font
              .metrics_with_variations(&coords)
              .or_else(|_| font.metrics())
          }
          .ok()?;
          Some((scaled, raw))
        });
    }

    if let Some((scaled, raw)) = metrics_source {
      let scale = scaled.scale;

      let underline_pos = scaled.underline_position;
      let underline_thickness = scaled.underline_thickness.max(1.0);
      let descent = scaled.descent;
      let strike_pos = raw
        .strikeout_position
        .map(|p| p as f32 * scale)
        .unwrap_or_else(|| scaled.ascent * 0.3);
      let strike_thickness = raw
        .strikeout_thickness
        .map(|t| t as f32 * scale)
        .unwrap_or(underline_thickness);
      let ascent = scaled.ascent;

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

  fn build_underline_segments(
    &self,
    runs: &[ShapedRun],
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

    let decoration_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
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
        baseline_y,
        band_left,
        band_right,
        skip_ink == TextDecorationSkipInk::All,
        extra_pad,
      )
    } else {
      let band_top = center - band_half;
      let band_bottom = center + band_half;
      collect_underline_exclusions(
        runs,
        baseline_y,
        band_top,
        band_bottom,
        skip_ink == TextDecorationSkipInk::All,
        extra_pad,
      )
    };

    let mut segments = subtract_intervals((0.0, line_width), &mut exclusions);
    if segments.is_empty() && skip_ink != TextDecorationSkipInk::All {
      // Never drop the underline entirely when skipping ink; fall back to a full span.
      segments.push((0.0, line_width));
    }

    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decoration_timer) {
      breakdown.record_text_decoration(start.elapsed());
    }
    segments
  }

  fn scale_run(run: &ShapedRun, scale_x: f32, scale_y: f32) -> ShapedRun {
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
  }

  fn glyphs_from_run(
    &self,
    run: &ShapedRun,
    origin_x: f32,
    baseline_y: f32,
  ) -> (Vec<GlyphInstance>, Rect) {
    let mut glyphs = Vec::with_capacity(run.glyphs.len());
    let origin = Point::new(origin_x, baseline_y);
    let mut bounds = ConservativeGlyphRunBoundsBuilder::new(origin, run.advance);

    for glyph in &run.glyphs {
      let x_offset = glyph.x_offset;
      let y_offset = -glyph.y_offset;
      bounds.include_glyph(x_offset, glyph.x_advance);
      glyphs.push(GlyphInstance {
        glyph_id: glyph.glyph_id,
        cluster: glyph.cluster,
        x_offset,
        y_offset,
        x_advance: glyph.x_advance,
        y_advance: 0.0,
      });
    }

    (glyphs, bounds.finish(run.font_size))
  }

  fn glyphs_from_run_vertical(
    &self,
    run: &ShapedRun,
    block_baseline: f32,
    inline_origin: f32,
  ) -> (Vec<GlyphInstance>, Rect) {
    let glyphs = run
      .glyphs
      .iter()
      .map(|glyph| GlyphInstance {
        glyph_id: glyph.glyph_id,
        cluster: glyph.cluster,
        x_offset: glyph.x_offset,
        y_offset: -glyph.y_offset,
        x_advance: glyph.x_advance,
        y_advance: glyph.y_advance,
      })
      .collect::<Vec<_>>();

    // Conservative bounds for vertical runs: expand along the inline axis using `run.advance`
    // and assume glyph outlines are roughly one font-size wide on the block axis.
    let ascent = run.font_size;
    let descent = run.font_size * 0.25;
    let width = (ascent + descent).max(0.0);
    let height = (run.advance + ascent + descent).max(0.0);
    let origin = Point::new(block_baseline, inline_origin);
    let bounds = Rect::from_xywh(origin.x - ascent, origin.y - ascent, width, height);

    (glyphs, bounds)
  }

  fn build_emphasis(
    &self,
    run: &ShapedRun,
    style: &ComputedStyle,
    inline_origin: f32,
    block_baseline: f32,
    inline_vertical: bool,
    emphasis_offset: TextEmphasisOffset,
  ) -> Option<TextEmphasis> {
    if style.text_emphasis_style.is_none() {
      return None;
    }
    let mark_color = style.text_emphasis_color.unwrap_or(style.color);
    let (ascent, descent) = self
      .font_ctx
      .get_scaled_metrics_with_variations(run.font.as_ref(), run.font_size, &run.variations)
      .map(|scaled| (scaled.ascent, scaled.descent))
      .unwrap_or_else(|| {
        let fallback_size = (run.font_size * run.scale).max(0.0);
        (fallback_size * 0.8, fallback_size * 0.2)
      });
    let mark_size = (style.font_size * 0.5).max(1.0);
    let gap = mark_size * 0.3;
    let resolved_position = match style.text_emphasis_position {
      TextEmphasisPosition::Auto => TextEmphasisPosition::Over,
      other => other,
    };
    let block_center = if inline_vertical {
      let offset = gap + mark_size * 0.5;
      if crate::style::is_vertical_typographic_mode(style.writing_mode) {
        // In vertical typographic modes, emphasis placement is controlled by `right`/`left`.
        // The `over`/`under` component is only meaningful in horizontal typographic modes.
        let mark_on_left = matches!(
          resolved_position,
          TextEmphasisPosition::OverLeft | TextEmphasisPosition::UnderLeft
        );
        let block_side =
          crate::style::resolve_text_emphasis_block_side(style.writing_mode, resolved_position);
        let baseline_to_edge = match block_side {
          crate::style::BlockSide::Start => ascent,
          crate::style::BlockSide::End => descent,
        };
        let extra_offset = match block_side {
          crate::style::BlockSide::Start => emphasis_offset.over,
          crate::style::BlockSide::End => emphasis_offset.under,
        };
        if mark_on_left {
          block_baseline - baseline_to_edge - offset - extra_offset
        } else {
          block_baseline + baseline_to_edge + offset + extra_offset
        }
      } else {
        let mut center = match resolved_position {
          TextEmphasisPosition::Over
          | TextEmphasisPosition::OverLeft
          | TextEmphasisPosition::OverRight => block_baseline + offset,
          TextEmphasisPosition::Under
          | TextEmphasisPosition::UnderLeft
          | TextEmphasisPosition::UnderRight => block_baseline - offset,
          TextEmphasisPosition::Auto => block_baseline + offset,
        };
        match resolved_position {
          TextEmphasisPosition::Over
          | TextEmphasisPosition::OverLeft
          | TextEmphasisPosition::OverRight => center += emphasis_offset.over,
          TextEmphasisPosition::Under
          | TextEmphasisPosition::UnderLeft
          | TextEmphasisPosition::UnderRight => center -= emphasis_offset.under,
          TextEmphasisPosition::Auto => {}
        }
        center
      }
    } else {
      let mut center = match resolved_position {
        TextEmphasisPosition::Over
        | TextEmphasisPosition::OverLeft
        | TextEmphasisPosition::OverRight => block_baseline - ascent - gap - mark_size * 0.5,
        TextEmphasisPosition::Under
        | TextEmphasisPosition::UnderLeft
        | TextEmphasisPosition::UnderRight => block_baseline + descent + gap + mark_size * 0.5,
        TextEmphasisPosition::Auto => block_baseline - ascent - gap - mark_size * 0.5,
      };
      match resolved_position {
        TextEmphasisPosition::Over
        | TextEmphasisPosition::OverLeft
        | TextEmphasisPosition::OverRight => center -= emphasis_offset.over,
        TextEmphasisPosition::Under
        | TextEmphasisPosition::UnderLeft
        | TextEmphasisPosition::UnderRight => center += emphasis_offset.under,
        TextEmphasisPosition::Auto => {}
      }
      center
    };

    let mut marks = Vec::new();
    if !run.glyphs.is_empty() && !run.text.is_empty() {
      use unicode_segmentation::UnicodeSegmentation;

      // `text-emphasis` marks are drawn once per typographic character unit (extended grapheme
      // cluster). HarfBuzz cluster values can span multiple grapheme clusters (e.g. ligatures), so
      // derive per-grapheme positions by distributing each HarfBuzz cluster's advance across the
      // graphemes that fall within its text span.
      let graphemes: Vec<(usize, &str)> = run.text.grapheme_indices(true).collect();

      let mut cluster_starts: Vec<usize> = run.glyphs.iter().map(|g| g.cluster as usize).collect();
      cluster_starts.sort_unstable();
      cluster_starts.dedup();

      let mut cluster_end_for = HashMap::new();
      for (idx, start) in cluster_starts.iter().enumerate() {
        let end = cluster_starts
          .get(idx + 1)
          .copied()
          .unwrap_or_else(|| run.text.len());
        cluster_end_for.insert(*start, end);
      }

      let mut cursor_inline = inline_origin;
      let mut glyph_idx = 0;
      while glyph_idx < run.glyphs.len() {
        let cluster_start = run.glyphs[glyph_idx].cluster as usize;
        let cluster_end = cluster_end_for
          .get(&cluster_start)
          .copied()
          .unwrap_or_else(|| run.text.len());

        let first_glyph = &run.glyphs[glyph_idx];
        let cluster_offset = if inline_vertical {
          // For vertical runs, HarfBuzz offsets use a y-up coordinate system. Flip the inline
          // offset so we stay in CSS's y-down space.
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
          cursor_inline += cluster_advance;
          continue;
        }

        let per_unit_advance = cluster_advance / (count as f32);
        for (idx, (_, grapheme)) in graphemes[start_idx..end_idx].iter().enumerate() {
          let inline_center =
            cursor_inline + cluster_offset + (idx as f32 + 0.5) * per_unit_advance;

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

          let center = if inline_vertical {
            Point::new(block_center, inline_center)
          } else {
            Point::new(inline_center, block_center)
          };
          marks.push(EmphasisMark { center });
        }

        cursor_inline += cluster_advance;
      }
    }

    let text = if let TextEmphasisStyle::String(ref s) = style.text_emphasis_style {
      // CSS Text Decoration 4: for `text-emphasis-style: <string>`, only the first *typographic
      // character unit* is used. Use the first extended grapheme cluster so multi-codepoint
      // clusters (e.g. flags, emoji sequences) render as a single mark.
      use unicode_segmentation::UnicodeSegmentation;

      let raw = s.as_str();
      let mark_str = raw.graphemes(true).next().unwrap_or("");
      if mark_str.is_empty() {
        None
      } else {
        let mut mark_style = style.clone();
        mark_style.font_size = style.font_size * 0.5;
        mark_style.color = mark_color;
        mark_style.font_variant_east_asian.ruby = true;
        if crate::style::is_vertical_typographic_mode(style.writing_mode) {
          // CSS Text Decoration 4: emphasis marks must remain upright in vertical typographic modes,
          // regardless of the element's authored `text-orientation`.
          mark_style.text_orientation = crate::style::types::TextOrientation::Upright;
        }
        let shape_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
        let shaped = self.shaper.shape(mark_str, &mark_style, &self.font_ctx);
        if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), shape_timer) {
          breakdown.record_text_shape(start.elapsed());
        }
        match shaped {
          Ok(mark_runs) if !mark_runs.is_empty() => {
            let mut runs = Vec::with_capacity(mark_runs.len());
            let mut width = 0.0_f32;
            let mut ascent: f32 = 0.0;
            let mut descent: f32 = 0.0;
            for r in &mark_runs {
              if let Some(scaled) = self.font_ctx.get_scaled_metrics_with_variations(
                r.font.as_ref(),
                r.font_size,
                &r.variations,
              ) {
                ascent = ascent.max(scaled.ascent);
                descent = descent.max(scaled.descent);
              }
            }
            if ascent == 0.0 && descent == 0.0 {
              let fallback = mark_runs
                .iter()
                .map(|r| r.font_size * r.scale)
                .fold(0.0_f32, f32::max)
                .max(0.0);
              ascent = fallback * 0.8;
              descent = fallback * 0.2;
            }
            for r in &mark_runs {
              let run_advance = r.advance;
              let glyphs = r
                .glyphs
                .iter()
                .map(|g| GlyphInstance {
                  glyph_id: g.glyph_id,
                  cluster: g.cluster,
                  x_offset: g.x_offset,
                  y_offset: -g.y_offset,
                  x_advance: g.x_advance,
                  y_advance: g.y_advance,
                })
                .collect();
              runs.push(EmphasisTextRun {
                glyphs,
                font: Some(r.font.clone()),
                font_id: self.font_id_from_run(r),
                font_size: r.font_size * r.scale,
                advance_width: run_advance,
                variations: r
                  .variations
                  .iter()
                  .map(|v| FontVariation::new(v.tag, v.value))
                  .collect(),
                palette_index: r.palette_index,
                palette_overrides: Arc::clone(&r.palette_overrides),
                palette_override_hash: r.palette_override_hash,
                synthetic_bold: r.synthetic_bold,
                synthetic_oblique: r.synthetic_oblique,
              });
              width += run_advance;
            }
            Some(EmphasisText {
              runs,
              width,
              height: ascent + descent,
              baseline_offset: ascent,
            })
          }
          _ => None,
        }
      }
    } else {
      None
    };

    let emphasis_style = match &style.text_emphasis_style {
      TextEmphasisStyle::Mark { fill, shape: None } => {
        let default_shape = if crate::style::is_vertical_typographic_mode(style.writing_mode) {
          crate::style::types::TextEmphasisShape::Sesame
        } else {
          crate::style::types::TextEmphasisShape::Circle
        };
        TextEmphasisStyle::Mark {
          fill: *fill,
          shape: Some(default_shape),
        }
      }
      other => other.clone(),
    };

    Some(TextEmphasis {
      style: emphasis_style,
      color: mark_color,
      position: resolved_position,
      size: mark_size,
      marks,
      inline_vertical,
      text,
    })
  }

  fn font_id_from_run(&self, run: &ShapedRun) -> Option<FontId> {
    run.font.id
  }

  fn text_shadows_from_style(
    style: Option<&ComputedStyle>,
    viewport: Option<(f32, f32)>,
  ) -> Vec<TextShadowItem> {
    style
      .map(|s| {
        resolve_text_shadows_with_viewport(s, viewport)
          .into_iter()
          .map(|shadow| TextShadowItem {
            offset: Point::new(shadow.offset_x, shadow.offset_y),
            blur_radius: shadow.blur_radius,
            color: shadow.color,
          })
          .collect()
      })
      .unwrap_or_default()
  }

  fn resolve_webkit_text_stroke_for_run(
    &self,
    style: &ComputedStyle,
    run_font_size: f32,
  ) -> (f32, Rgba) {
    let current = style.color;
    let stroke_color = style.webkit_text_stroke_color.to_rgba(current);
    let (vw, vh) = self.viewport.unwrap_or((0.0, 0.0));
    let width = style
      .webkit_text_stroke_width
      .resolve_with_context(None, vw, vh, run_font_size, style.root_font_size)
      .unwrap_or(0.0);
    let width = if width.is_finite() {
      width.max(0.0)
    } else {
      0.0
    };
    (width, stroke_color)
  }

  fn missing_image_icon_size(content_rect: Rect) -> f32 {
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

  fn emit_inside_border_rect(&mut self, rect: Rect, color: Rgba) {
    if !rect.width().is_finite() || !rect.height().is_finite() {
      return;
    }
    let w = rect.width().max(0.0);
    let h = rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return;
    }

    // Chrome's broken-image icon uses a crisp 1px border that sits inside the icon rect. Using
    // `StrokeRect` here would center the stroke on the edge, which gets clipped/anti-aliased and
    // ends up darker than browsers.
    let thickness: f32 = 1.0;
    let th = thickness.min(h);
    let tw = thickness.min(w);
    let x = rect.x();
    let y = rect.y();
    let bottom_y = y + h - th;
    let right_x = x + w - tw;

    self.list.push(DisplayItem::FillRect(FillRectItem {
      rect: Rect::from_xywh(x, y, w, th),
      color,
    }));
    if bottom_y > y {
      self.list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(x, bottom_y, w, th),
        color,
      }));
    }

    self.list.push(DisplayItem::FillRect(FillRectItem {
      rect: Rect::from_xywh(x, y, tw, h),
      color,
    }));
    if right_x > x {
      self.list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(right_x, y, tw, h),
        color,
      }));
    }
  }

  fn emit_broken_image_icon(&mut self, icon_rect: Rect) {
    let inner_rect = Rect::from_xywh(
      icon_rect.x() + 1.0,
      icon_rect.y() + 1.0,
      (icon_rect.width() - 2.0).max(0.0),
      (icon_rect.height() - 2.0).max(0.0),
    );

    if inner_rect.width() > 0.0 && inner_rect.height() > 0.0 {
      self.list.push(DisplayItem::FillRect(FillRectItem {
        rect: inner_rect,
        color: Rgba::WHITE,
      }));

      // Sky.
      let sky_h = (inner_rect.height() * 0.62)
        .floor()
        .clamp(0.0, inner_rect.height());
      if sky_h > 0.0 {
        self.list.push(DisplayItem::FillRect(FillRectItem {
          rect: Rect::from_xywh(inner_rect.x(), inner_rect.y(), inner_rect.width(), sky_h),
          color: Rgba::rgb(198, 216, 244),
        }));
      }

      // Ground.
      let ground_h = (inner_rect.height() * 0.3)
        .floor()
        .clamp(0.0, inner_rect.height());
      if ground_h > 0.0 {
        self.list.push(DisplayItem::FillRect(FillRectItem {
          rect: Rect::from_xywh(
            inner_rect.x(),
            inner_rect.y() + inner_rect.height() - ground_h,
            inner_rect.width(),
            ground_h,
          ),
          color: Rgba::rgb(88, 174, 57),
        }));
      }

      // "Sun" highlight.
      self.list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(inner_rect.x() + 2.0, inner_rect.y() + 2.0, 3.0, 3.0),
        color: Rgba::WHITE,
      }));
    }

    // Chrome's broken-image icon border is a slightly darker gray than the old full-frame border.
    self.emit_inside_border_rect(icon_rect, Rgba::rgb(163, 163, 163));
  }

  fn measure_text_advance_width(&mut self, text: &str, style: &ComputedStyle) -> Option<f32> {
    let shaped = self.shaper.shape(text, style, &self.font_ctx).ok()?;
    let mut runs = shaped;
    InlineTextItem::apply_spacing_to_runs(
      &mut runs,
      text,
      style.letter_spacing,
      style.word_spacing,
    );
    Some(runs.iter().map(|run| run.advance).sum())
  }

  fn emit_video_controls_placeholder_ui(&mut self, content_rect: Rect) {
    let w = content_rect.width().max(0.0);
    let h = content_rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return;
    }

    // Chromium generally keeps the native video controls hidden until the user interacts, but it
    // still paints a thin scrubber track within a dark control-bar region. Avoid painting a fully
    // custom UI (play/volume/settings/fullscreen) since that tends to diverge from real UAs and
    // dominate page diffs.
    //
    // Keep this placeholder minimal and deterministic: just paint a scrubber track.
    let bar_h = 32.0_f32.min(h);
    if bar_h < 12.0_f32 {
      return;
    }

    // Align to device pixels to avoid fuzzy lines when the <video> box has fractional offsets.
    let overlay_bottom = content_rect.max_y().ceil();
    let bar_top = overlay_bottom - bar_h;

    let track_inset = 16.0_f32;
    let track_h = 4.0_f32;
    let track_y = bar_top + 8.0_f32;

    let track_x = content_rect.x().ceil() + track_inset;
    let track_w = (w - track_inset * 2.0).max(0.0);
    if track_w <= 0.0 {
      return;
    }

    let track_r = (track_h * 0.5).min(2.0_f32);
    self.list.push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
      rect: Rect::from_xywh(track_x, track_y, track_w, track_h),
      color: Rgba::WHITE.with_alpha(0.3_f32),
      radii: BorderRadii::uniform(track_r),
    }));
  }

  fn emit_play_triangle_icon(&mut self, rect: Rect, color: Rgba) {
    let w = rect.width().max(0.0);
    let h = rect.height().max(0.0);
    if w <= 0.0 || h <= 0.0 {
      return;
    }

    // Approximate a triangle with vertical strips.
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
      self.list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(x, y, col_w, col_h),
        color,
      }));
    }
  }

  fn emit_replaced_placeholder(
    &mut self,
    replaced_type: &ReplacedType,
    fragment: &FragmentNode,
    rect: Rect,
  ) {
    // Replaced fallback rendering is UA-defined and varies between browsers.
    //
    // For `<canvas>` with no drawn content, browsers keep the element transparent. Painting a
    // placeholder would incorrectly obscure background content.
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
    // When `<img>` has an `alt` attribute, browsers render a combined broken-image icon + alt text
    // fallback. `emit_alt_text` handles that rendering so the icon can be positioned relative to
    // the text (respecting `text-align`).
    if matches!(replaced_type, ReplacedType::Image { alt: Some(_), .. }) {
      return;
    }

    let style = fragment.style.as_deref();
    let (content_rect, clip_radii) = self.replaced_content_rect_and_radii(rect, style);
    let clip_contents =
      Self::replaced_content_clip_item(style, content_rect, content_rect, clip_radii);
    if let Some(clip) = clip_contents.as_ref() {
      self.list.push(DisplayItem::PushClip(clip.clone()));
    }

    // For `<video controls>` without a poster/frame, browsers still paint a dark video surface and
    // chrome for the native controls. Emit a stable approximation rather than leaving the element
    // transparent.
    if matches!(
      replaced_type,
      ReplacedType::Video {
        controls: true, ..
      }
    ) {
      self.list.push(DisplayItem::FillRect(FillRectItem {
        rect: content_rect,
        color: Rgba::rgb(51, 51, 51),
      }));

      let h = content_rect.height().max(0.0);
      let bar_h = 32.0_f32.min(h);
      let shadow_h = 72.0_f32.min((h - bar_h).max(0.0));
      if shadow_h > 0.0 && h > 0.0 {
        let start = (h - bar_h - shadow_h) / h;
        let end = (h - bar_h) / h;
        // Chromium's native controls darken all the way to black at the very bottom of the video.
        // Keep the same overall shape, but blend to full black so fixture diffs don't get dominated
        // by a 1px/2px value mismatch across the entire control bar.
        let end_alpha = 0.85;
        let black = |alpha: f32| Rgba::rgb(0, 0, 0).with_alpha(alpha.clamp(0.0, 1.0));
        let mut stops = Vec::new();
        stops.push(GradientStop {
          position: 0.0,
          color: black(0.0),
        });
        stops.push(GradientStop {
          position: start,
          color: black(0.0),
        });
        for t in [0.25_f32, 0.5, 0.75] {
          stops.push(GradientStop {
            position: start + (end - start) * t,
            color: black(end_alpha * t.powf(1.5)),
          });
        }
        stops.push(GradientStop {
          position: end,
          color: black(end_alpha),
        });
        stops.push(GradientStop {
          position: 1.0,
          color: black(1.0),
        });
        self.list.push(DisplayItem::LinearGradient(LinearGradientItem {
          rect: content_rect,
          start: Point::new(0.0, 0.0),
          end: Point::new(0.0, content_rect.height()),
          stops,
          spread: GradientSpread::Pad,
        }));
      }

      self.emit_video_controls_placeholder_ui(content_rect);

      if clip_contents.is_some() {
        self.list.push(DisplayItem::PopClip);
      }
      return;
    }
    match replaced_type {
      // Chrome renders a broken-image icon inside a small 1px-bordered square and leaves the
      // image box itself transparent (so author-provided backgrounds show through). It does **not**
      // draw a full border around the replaced box itself.
      ReplacedType::Image { .. } => {
        // Draw a small icon in the top-left when there is enough room. Keep it from dominating
        // tiny boxes (e.g. 20×20) so author-provided backgrounds remain visible.
        let icon_inset = 2.0;
        let icon_size = Self::missing_image_icon_size(content_rect);
        if icon_size > 0.0 {
          let icon_rect = Rect::from_xywh(
            content_rect.x() + icon_inset,
            content_rect.y() + icon_inset,
            icon_size,
            icon_size,
          );
          self.emit_broken_image_icon(icon_rect);
        }
      }
      _ => {
        // Keep placeholder styling stable and browser-like for other replaced elements.
        let placeholder_color = Rgba::rgb(200, 200, 200);
        let stroke_color = Rgba::rgb(150, 150, 150);
        self.list.push(DisplayItem::FillRect(FillRectItem {
          rect: content_rect,
          color: placeholder_color,
        }));

        self.list.push(DisplayItem::StrokeRect(StrokeRectItem {
          rect: content_rect,
          color: stroke_color,
          width: 1.0,
          blend_mode: BlendMode::Normal,
        }));
      }
    }

    let label = replaced_type.placeholder_label();

    if let Some(label_text) = label {
      let label_style = fragment.style.as_deref().map(|style| {
        let mut clone = style.clone();
        clone.color = Rgba::rgb(120, 120, 120);
        clone
      });
      let inset = 2.0;
      let label_rect = Rect::from_xywh(
        content_rect.x() + inset,
        content_rect.y() + inset,
        (content_rect.width() - inset * 2.0).max(0.0),
        (content_rect.height() - inset * 2.0).max(0.0),
      );
      let label_style_ref = label_style
        .as_ref()
        .map(|s| s as &ComputedStyle)
        .or(fragment.style.as_deref());
      let _ = self.emit_text_with_style(label_text, label_style_ref, label_rect);
    }

    if clip_contents.is_some() {
      self.list.push(DisplayItem::PopClip);
    }
  }

  fn resolved_accent_color(style: &ComputedStyle) -> Rgba {
    match style.accent_color {
      AccentColor::Color(c) => c,
      // `accent-color: auto` is a UA-defined value. Use a stable, browser-like default rather
      // than inheriting from the text color so checkbox/radio controls don't render as black.
      AccentColor::Auto => Rgba::rgb(26, 115, 232),
    }
  }

  /// Splits `rect` into logical inline-start and inline-end segments.
  ///
  /// `inline_start_len` is the length of the inline-start segment along the element's inline axis.
  ///
  /// For `writing-mode: horizontal-tb`:
  /// - LTR: inline-start is physical left, inline-end is physical right
  /// - RTL: inline-start is physical right, inline-end is physical left
  fn split_rect_inline_start(
    rect: Rect,
    style: &ComputedStyle,
    inline_start_len: f32,
  ) -> (Rect, Rect) {
    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);
    if inline_axis_is_horizontal(style.writing_mode) {
      let total = rect.width().max(0.0);
      let inline_start_len = inline_start_len.clamp(0.0, total);
      let inline_end_len = (total - inline_start_len).max(0.0);
      if inline_positive {
        let inline_start = Rect::from_xywh(rect.x(), rect.y(), inline_start_len, rect.height());
        let inline_end = Rect::from_xywh(
          rect.x() + inline_start_len,
          rect.y(),
          inline_end_len,
          rect.height(),
        );
        (inline_start, inline_end)
      } else {
        let inline_start = Rect::from_xywh(
          rect.max_x() - inline_start_len,
          rect.y(),
          inline_start_len,
          rect.height(),
        );
        let inline_end = Rect::from_xywh(rect.x(), rect.y(), inline_end_len, rect.height());
        (inline_start, inline_end)
      }
    } else {
      let total = rect.height().max(0.0);
      let inline_start_len = inline_start_len.clamp(0.0, total);
      let inline_end_len = (total - inline_start_len).max(0.0);
      if inline_positive {
        let inline_start = Rect::from_xywh(rect.x(), rect.y(), rect.width(), inline_start_len);
        let inline_end = Rect::from_xywh(
          rect.x(),
          rect.y() + inline_start_len,
          rect.width(),
          inline_end_len,
        );
        (inline_start, inline_end)
      } else {
        let inline_start = Rect::from_xywh(
          rect.x(),
          rect.max_y() - inline_start_len,
          rect.width(),
          inline_start_len,
        );
        let inline_end = Rect::from_xywh(rect.x(), rect.y(), rect.width(), inline_end_len);
        (inline_start, inline_end)
      }
    }
  }

  /// Splits `rect` into logical inline-start (returned first) and inline-end segments, where
  /// `inline_end_len` is the length of the inline-end segment.
  fn split_rect_inline_end(rect: Rect, style: &ComputedStyle, inline_end_len: f32) -> (Rect, Rect) {
    let total = if inline_axis_is_horizontal(style.writing_mode) {
      rect.width().max(0.0)
    } else {
      rect.height().max(0.0)
    };
    let inline_end_len = inline_end_len.clamp(0.0, total);
    let inline_start_len = (total - inline_end_len).max(0.0);
    Self::split_rect_inline_start(rect, style, inline_start_len)
  }

  fn appearance_none_text_edit_overlays(
    &mut self,
    control: &FormControl,
    style: &ComputedStyle,
    border_rect: Rect,
    scroll_delta: Point,
  ) -> Option<TextEditOverlays> {
    if !control.focused || control.disabled {
      return None;
    }

    let rects = Self::background_rects(border_rect, style, self.viewport);
    let content_rect = rects.content;
    if content_rect.width() <= 0.0 || content_rect.height() <= 0.0 {
      return None;
    }

    let text_rect = content_rect.translate(scroll_delta);

    let shape_text_runs =
      |builder: &mut Self, text: &str, style: &ComputedStyle| -> Option<Vec<ShapedRun>> {
        if text.is_empty() {
          return Some(Vec::new());
        }
        let shape_timer = builder.build_breakdown.as_ref().map(|_| Instant::now());
        let shaped = builder.shaper.shape(text, style, &builder.font_ctx);
        if let (Some(breakdown), Some(start)) = (builder.build_breakdown.as_ref(), shape_timer) {
          breakdown.record_text_shape(start.elapsed());
        }
        let mut runs = shaped.ok()?;
        InlineTextItem::apply_spacing_to_runs(
          &mut runs,
          text,
          style.letter_spacing,
          style.word_spacing,
        );
        Some(runs)
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
        let preedit = control
          .ime_preedit
          .as_ref()
          .filter(|state| !state.text.is_empty());
        let committed_is_empty = value.is_empty();
        let display_is_empty = committed_is_empty && preedit.is_none();
        let committed_len = value.chars().count();
        let caret_committed = (*caret).min(committed_len);
        let preedit_len = preedit
          .map(|state| state.text.chars().count())
          .unwrap_or(0);
        let mut paint_text_owned: Option<String> = None;
        let mut paint_text: Option<&str> = None;
        let mut is_placeholder = false;
        let mut ime_caret_idx: Option<usize> = None;
        let mut ime_selection: Option<(usize, usize)> = None;

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
              let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
              let replace_start = replace_start.min(committed_len);
              let replace_end = replace_end.min(committed_len);
              let replaced_len = if preedit.is_some() {
                replace_end.saturating_sub(replace_start)
              } else {
                0
              };
              let mask_len = committed_len
                .saturating_sub(replaced_len)
                .saturating_add(preedit_len)
                .clamp(3, 50);
              paint_text_owned = Some("•".repeat(mask_len));
              paint_text = paint_text_owned.as_deref();

              if let Some(preedit) = preedit {
                let preedit_len = preedit.text.chars().count();
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
              is_placeholder = true;
            }
          }
          TextControlKind::Number | TextControlKind::Date | TextControlKind::Plain => {
            if let Some(preedit) = preedit {
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
              paint_text_owned = Some(combined);
              paint_text = paint_text_owned.as_deref();

              let preedit_len = preedit.text.chars().count();
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
            } else if !committed_is_empty {
              paint_text = Some(value.as_str());
            } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
              paint_text = Some(ph);
              is_placeholder = true;
            } else if matches!(kind, TextControlKind::Date) {
              // Date-like inputs render a UA placeholder when empty.
              paint_text = Some("yyyy-mm-dd");
              is_placeholder = true;
            }
          }
        }

        let placeholder_pseudo_style = if is_placeholder {
          placeholder_style
            .as_deref()
            .or(control.placeholder_style.as_deref())
        } else {
          None
        };
        let text_style = if let Some(pseudo_style) = placeholder_pseudo_style {
          let mut cloned = (*pseudo_style).clone();
          let opacity = cloned.opacity.clamp(0.0, 1.0);
          let alpha = (cloned.color.a * opacity).clamp(0.0, 1.0);
          cloned.color = cloned.color.with_alpha(alpha);
          cloned.opacity = 1.0;
          cloned
        } else {
          style.clone()
        };

        let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
        let metrics_scaled = Self::resolve_scaled_metrics(&text_style, &self.font_ctx);
        let line_height = compute_line_height_with_metrics_viewport(
          &text_style,
          metrics_scaled.as_ref(),
          viewport,
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

        let mut sample_text = paint_text.unwrap_or("M");
        if trim_ascii_whitespace(sample_text).is_empty() {
          sample_text = "M";
        }
        let metrics_runs = shape_text_runs(self, sample_text, &text_style).unwrap_or_default();
        let metrics = InlineTextItem::metrics_from_runs(
          &self.font_ctx,
          &metrics_runs,
          line_height,
          text_style.font_size,
        );
        let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
        let baseline_y = text_rect.y() + baseline_offset_y + half_leading + metrics.baseline_offset;
        let top = baseline_y - metrics.ascent;
        let bottom = baseline_y + metrics.descent;

        let display_text = paint_text.unwrap_or("");
        let text_runs = shape_text_runs(self, display_text, &text_style).unwrap_or_default();
        let fallback_advance = display_text.chars().count() as f32 * text_style.font_size * 0.6;
        let total_advance: f32 = if !text_runs.is_empty() {
          text_runs.iter().map(|run| run.advance).sum()
        } else {
          fallback_advance
        };
        let start_x = Self::aligned_text_start_x(&text_style, text_rect, total_advance);
        let max_chars = display_text.chars().count();
        let fallback_char_advance = if max_chars > 0 {
          (fallback_advance / max_chars as f32).max(0.0)
        } else {
          0.0
        };
        let caret_stops = caret_stops_for_runs(display_text, &text_runs, total_advance);

        let mut overlays = TextEditOverlays::default();

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
              let rect = Rect::from_xywh(
                start_x + seg_start,
                top,
                (seg_end - seg_start).max(0.0),
                (bottom - top).max(0.0),
              );
              if let Some(clipped) =
                rect.intersection(content_rect).filter(|r| r.width() > 0.0 && r.height() > 0.0)
              {
                overlays.selection_rects.push(clipped);
              }
            }
          }
        }

        overlays
          .selection_rects
          .retain(|r| r.width() > 0.0 && r.height() > 0.0);

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
            + caret_x_for_position(&caret_stops, caret_idx, caret_affinity_for_paint).unwrap_or(0.0);
          let max_caret_x = (text_rect.max_x() - 1.0).max(text_rect.x());
          let caret_x = caret_x.clamp(text_rect.x(), max_caret_x);
          let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
          let caret_rect_raw = self.snap_form_control_caret_rect(caret_rect_raw);
          if let Some(clipped) = caret_rect_raw.intersection(content_rect) {
            if clipped.width() > 0.0 && clipped.height() > 0.0 {
              overlays.caret_rect = Some((clipped, caret_color));
            }
          }
        }

        if overlays.selection_rects.is_empty() && overlays.caret_rect.is_none() {
          None
        } else {
          Some(overlays)
        }
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
        let preedit = control
          .ime_preedit
          .as_ref()
          .filter(|state| !state.text.is_empty());
        let mut paint_text_owned: Option<String> = None;
        let mut paint_text: Option<&str> = None;
        let mut is_placeholder = false;
        let mut ime_caret_idx: Option<usize> = None;
        let mut ime_selection: Option<(usize, usize)> = None;

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
          paint_text_owned = Some(combined);
          paint_text = paint_text_owned.as_deref();

          let preedit_len = preedit.text.chars().count();
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
        } else if !value.is_empty() {
          paint_text = Some(value.as_str());
        } else if let Some(placeholder) = placeholder.as_deref().filter(|p| !p.is_empty()) {
          paint_text = Some(placeholder);
          is_placeholder = true;
        }

        let placeholder_pseudo_style = if is_placeholder {
          placeholder_style
            .as_deref()
            .or(control.placeholder_style.as_deref())
        } else {
          None
        };
        let text_style = if let Some(pseudo_style) = placeholder_pseudo_style {
          let mut cloned = (*pseudo_style).clone();
          let opacity = cloned.opacity.clamp(0.0, 1.0);
          let alpha = (cloned.color.a * opacity).clamp(0.0, 1.0);
          cloned.color = cloned.color.with_alpha(alpha);
          cloned.opacity = 1.0;
          cloned
        } else {
          style.clone()
        };

        let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
        let metrics_scaled = Self::resolve_scaled_metrics(&text_style, &self.font_ctx);
        let line_height = compute_line_height_with_metrics_viewport(
          &text_style,
          metrics_scaled.as_ref(),
          viewport,
          self.font_ctx.root_font_metrics(),
        );
        if line_height <= 0.0 || !line_height.is_finite() {
          return None;
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
        let caret_color = match style.caret_color {
          CaretColor::Color(c) => c,
          CaretColor::Auto => style.color,
        };

        let mut overlays = TextEditOverlays::default();
        let mut caret_rect: Option<Rect> = None;
        let mut last_visible_line: Option<(
          /*line*/ &str,
          /*len*/ usize,
          Rect,
          f32,
          f32,
          f32,
          f32,
          f32,
        )> = None;

        let mut metrics_sample = display_text;
        if trim_ascii_whitespace(metrics_sample).is_empty() {
          metrics_sample = "M";
        }
        let metrics_runs = shape_text_runs(self, metrics_sample, &text_style).unwrap_or_default();
        let metrics = InlineTextItem::metrics_from_runs(
          &self.font_ctx,
          &metrics_runs,
          line_height,
          text_style.font_size,
        );
        let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;

        let mut y = text_rect.y();
        let mut line_start = 0usize;
        for line in display_text.split('\n') {
          if y > text_rect.y() + text_rect.height() {
            break;
          }

          let line_len = line.chars().count();
          let line_end = line_start + line_len;
          let line_rect = Rect::from_xywh(text_rect.x(), y, text_rect.width(), line_height);

          let line_runs = shape_text_runs(self, line, &text_style).unwrap_or_default();
          let fallback_advance = line_len as f32 * text_style.font_size * 0.6;
          let total_advance: f32 = if !line_runs.is_empty() {
            line_runs.iter().map(|run| run.advance).sum()
          } else {
            fallback_advance
          };
          let start_x = Self::aligned_text_start_x(&text_style, line_rect, total_advance);
          let caret_stops = caret_stops_for_runs(line, &line_runs, total_advance);

          let baseline_y = y + half_leading + metrics.baseline_offset;
          let top = baseline_y - metrics.ascent;
          let bottom = baseline_y + metrics.descent;

          let caret_height = (bottom - top).max(0.0);
          if caret_height > 0.0 {
            let caret_band = Rect::from_xywh(line_rect.x(), top, 1.0, caret_height);
            if let Some(intersection) = caret_band.intersection(content_rect) {
              if intersection.height() > 0.0 {
                last_visible_line = Some((
                  line,
                  line_len,
                  line_rect,
                  start_x,
                  total_advance,
                  top,
                  bottom,
                  fallback_advance,
                ));
              }
            }
          }

          if let Some((sel_start, sel_end)) = selection {
            let seg_start = sel_start.max(line_start).min(line_end);
            let seg_end = sel_end.max(line_start).min(line_end);
            if seg_start < seg_end {
              let start_col = seg_start - line_start;
              let end_col = seg_end - line_start;
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
                  line,
                  &line_runs,
                  start_col,
                  end_col,
                )
              };
              for (seg_start, seg_end) in segments {
                let sel_rect = Rect::from_xywh(
                  start_x + seg_start,
                  top,
                  (seg_end - seg_start).max(0.0),
                  (bottom - top).max(0.0),
                );
                if let Some(clipped) = sel_rect.intersection(content_rect) {
                  if clipped.width() > 0.0 && clipped.height() > 0.0 {
                    overlays.selection_rects.push(clipped);
                  }
                }
              }
            }
          }

          if caret_rect.is_none()
            && caret_idx <= line_end
            && !caret_color.is_transparent()
          {
            let caret_col = caret_idx.saturating_sub(line_start).min(line_len);
            let caret_x = start_x
              + caret_x_for_position(&caret_stops, caret_col, caret_affinity_for_paint).unwrap_or(0.0);
            let max_caret_x = (line_rect.max_x() - 1.0).max(line_rect.x());
            let caret_x = caret_x.clamp(line_rect.x(), max_caret_x);
            let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            let caret_rect_raw = self.snap_form_control_caret_rect(caret_rect_raw);
            caret_rect = caret_rect_raw
              .intersection(content_rect)
              .filter(|r| r.width() > 0.0 && r.height() > 0.0);
          }

          y += line_height;
          line_start = line_end.saturating_add(1);
        }

        if caret_rect.is_none() && !caret_color.is_transparent() {
          if let Some((line, line_len, line_rect, start_x, total_advance, top, bottom, _fallback)) =
            last_visible_line
          {
            let line_runs = shape_text_runs(self, line, &text_style).unwrap_or_default();
            let caret_x = start_x
              + caret_x_for_position(
                &caret_stops_for_runs(line, &line_runs, total_advance),
                line_len,
                CaretAffinity::Downstream,
              )
              .unwrap_or(0.0);
            let max_caret_x = (line_rect.max_x() - 1.0).max(line_rect.x());
            let caret_x = caret_x.clamp(line_rect.x(), max_caret_x);
            let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            let caret_rect_raw = self.snap_form_control_caret_rect(caret_rect_raw);
            caret_rect = caret_rect_raw
              .intersection(content_rect)
              .filter(|r| r.width() > 0.0 && r.height() > 0.0);
          }
        }

        overlays.caret_rect = caret_rect.map(|rect| (rect, caret_color));

        if overlays.selection_rects.is_empty() && overlays.caret_rect.is_none() {
          None
        } else {
          Some(overlays)
        }
      }
      _ => None,
    }
  }

  fn emit_form_control(
    &mut self,
    control: &FormControl,
    fragment: &FragmentNode,
    rect: Rect,
    culling_rect: Option<Rect>,
  ) -> bool {
    let Some(style) = fragment.style.as_deref() else {
      return false;
    };

    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);

    let rects = Self::background_rects(rect, style, self.viewport);
    let padding_rect = rects.padding;
    let content_rect = rects.content;
    if padding_rect.width() <= 0.0 || padding_rect.height() <= 0.0 {
      return true;
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

    let emit_text_aligned = |builder: &mut Self,
                             text: &str,
                             style: &ComputedStyle,
                             rect: Rect,
                             center_x: bool,
                             center_y: bool|
     -> bool {
      if text.is_empty() {
        return false;
      }

      let Some(viewport) = builder.viewport.map(|(w, h)| Size::new(w, h)) else {
        return builder.emit_text_with_style_raw(text, Some(style), rect);
      };

      let shape_timer = builder.build_breakdown.as_ref().map(|_| Instant::now());
      let shaped = builder.shaper.shape(text, style, &builder.font_ctx);
      if let (Some(breakdown), Some(start)) = (builder.build_breakdown.as_ref(), shape_timer) {
        breakdown.record_text_shape(start.elapsed());
      }
      let mut runs = match shaped {
        Ok(r) => r,
        Err(_) => return builder.emit_naive_text(text, rect, Some(style)),
      };
      InlineTextItem::apply_spacing_to_runs(
        &mut runs,
        text,
        style.letter_spacing,
        style.word_spacing,
      );

      let metrics_scaled = Self::resolve_scaled_metrics(style, &builder.font_ctx);
      let line_height = compute_line_height_with_metrics_viewport(
        style,
        metrics_scaled.as_ref(),
        Some(viewport),
        builder.font_ctx.root_font_metrics(),
      );
      let metrics = match &style.line_height {
        crate::style::types::LineHeight::Normal => InlineTextItem::metrics_from_runs(
          &builder.font_ctx,
          &runs,
          line_height,
          style.font_size,
        ),
        _ => InlineTextItem::metrics_from_first_available_font(
          metrics_scaled.as_ref(),
          line_height,
          style.font_size,
        ),
      };
      let baseline_offset_y = if center_y {
        (rect.height() - line_height) / 2.0
      } else {
        0.0
      };
      let baseline = rect.y() + baseline_offset_y + metrics.baseline_offset;

      let advance_width: f32 = runs.iter().map(|run| run.advance).sum();
      let start_x = if center_x {
        rect.x() + ((rect.width() - advance_width).max(0.0) / 2.0)
      } else {
        Self::aligned_text_start_x(style, rect, advance_width)
      };

      let shadows = Self::text_shadows_from_style(Some(style), builder.viewport);
      builder.emit_shaped_runs(
        &runs,
        style.color,
        baseline,
        start_x,
        &shadows,
        Some(style),
        false,
        TextEmphasisOffset::default(),
      );
      true
    };

    let shape_text_runs =
      |builder: &mut Self, text: &str, style: &ComputedStyle| -> Option<Vec<ShapedRun>> {
        if text.is_empty() {
          return Some(Vec::new());
        }
        let shape_timer = builder.build_breakdown.as_ref().map(|_| Instant::now());
        let shaped = builder.shaper.shape(text, style, &builder.font_ctx);
        if let (Some(breakdown), Some(start)) = (builder.build_breakdown.as_ref(), shape_timer) {
          breakdown.record_text_shape(start.elapsed());
        }
        let mut runs = shaped.ok()?;
        InlineTextItem::apply_spacing_to_runs(
          &mut runs,
          text,
          style.letter_spacing,
          style.word_spacing,
        );
        Some(runs)
      };

    let measure_shaped_advance = |builder: &mut Self, text: &str, style: &ComputedStyle| -> f32 {
      if text.is_empty() {
        return 0.0;
      }
      match shape_text_runs(builder, text, style) {
        Some(runs) if !runs.is_empty() => runs.iter().map(|run| run.advance).sum(),
        _ => text.chars().count() as f32 * style.font_size * 0.6,
      }
    };

    // Do not paint internal focus/required/invalid "tint" overlays for native controls.
    //
    // These states are exposed via pseudo-classes (e.g. `:focus-visible`, `:required`, `:invalid`)
    // and should be styled via CSS rather than added as extra paint operations.

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
        let preedit = control
          .ime_preedit
          .as_ref()
          .filter(|state| !state.text.is_empty());
        let committed_is_empty = value.is_empty();
        // Treat any in-progress IME preedit as part of the displayed text so we:
        // - suppress placeholder rendering
        // - draw the caret after the preedit text
        let display_is_empty = committed_is_empty && preedit.is_none();
        let committed_len = value.chars().count();
        let caret_committed = (*caret).min(committed_len);
        let preedit_len = preedit
          .map(|state| state.text.chars().count())
          .unwrap_or(0);
        let mut paint_text_owned: Option<String> = None;
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
              let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
              let replace_start = replace_start.min(committed_len);
              let replace_end = replace_end.min(committed_len);
              let replaced_len = if preedit.is_some() {
                replace_end.saturating_sub(replace_start)
              } else {
                0
              };
              let mask_len = committed_len
                .saturating_sub(replaced_len)
                .saturating_add(preedit_len)
                .clamp(3, 50);
              paint_text_owned = Some("•".repeat(mask_len));
              paint_text = paint_text_owned.as_deref();
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
          TextControlKind::Number | TextControlKind::Date | TextControlKind::Plain => {
            if let Some(preedit) = preedit {
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
              paint_text_owned = Some(combined);
              paint_text = paint_text_owned.as_deref();
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
            } else if !committed_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
              paint_text = Some(ph);
              fallback_color = placeholder_color;
              is_placeholder = true;
            } else if matches!(kind, TextControlKind::Date) {
              paint_text = Some("yyyy-mm-dd");
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
        let mut text_style = if let Some(pseudo_style) = placeholder_pseudo_style {
          let mut cloned = (*pseudo_style).clone();
          let opacity = cloned.opacity.clamp(0.0, 1.0);
          let alpha = (cloned.color.a * opacity).clamp(0.0, 1.0);
          cloned.color = cloned.color.with_alpha(alpha);
          cloned.opacity = 1.0;
          cloned
        } else {
          let mut cloned = style.clone();
          cloned.color = fallback_color;
          cloned
        };
        // HTML `<input>` controls paint their value as a single line without wrapping; overflow is
        // handled via clipping/scrolling rather than soft line breaks.
        text_style.white_space = crate::style::types::WhiteSpace::Nowrap;
        text_style.text_wrap = crate::style::types::TextWrap::NoWrap;

        let mut text_rect = inset_rect(content_rect, 2.0);
        let mut affordance_space = 0.0;
        if !matches!(control.appearance, Appearance::None) {
          match kind {
            TextControlKind::Number => affordance_space = 14.0,
            TextControlKind::Date => affordance_space = 12.0,
            _ => {}
          }
        }
        let mut affordance_rect: Option<Rect> = None;
        if affordance_space > 0.0 {
          let (text, affordance) = Self::split_rect_inline_end(text_rect, style, affordance_space);
          text_rect = text;
          affordance_rect = Some(affordance);
        }

        let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
        let metrics_scaled = Self::resolve_scaled_metrics(&text_style, &self.font_ctx);
        let line_height = compute_line_height_with_metrics_viewport(
          &text_style,
          metrics_scaled.as_ref(),
          viewport,
          self.font_ctx.root_font_metrics(),
        );
        let baseline_offset_y = if line_height.is_finite() {
          (text_rect.height() - line_height) / 2.0
        } else {
          0.0
        };
        let baseline_offset_y = if baseline_offset_y.is_finite() {
          baseline_offset_y
        } else {
          0.0
        };
        let centered_text_rect = Rect::from_xywh(
          text_rect.x(),
          text_rect.y() + baseline_offset_y,
          text_rect.width(),
          text_rect.height(),
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
          if trim_ascii_whitespace(sample_text).is_empty() {
            sample_text = "M";
          }
          let metrics_runs = shape_text_runs(self, sample_text, &text_style).unwrap_or_default();
          let metrics = match &text_style.line_height {
            crate::style::types::LineHeight::Normal => InlineTextItem::metrics_from_runs(
              &self.font_ctx,
              &metrics_runs,
              line_height,
              text_style.font_size,
            ),
            _ => InlineTextItem::metrics_from_first_available_font(
              metrics_scaled.as_ref(),
              line_height,
              text_style.font_size,
            ),
          };
          let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
          let baseline_y =
            text_rect.y() + baseline_offset_y + half_leading + metrics.baseline_offset;
          let top = baseline_y - metrics.ascent;
          let bottom = baseline_y + metrics.descent;

          let display_text = paint_text.unwrap_or("");
          let text_runs = shape_text_runs(self, display_text, &text_style).unwrap_or_default();
          let fallback_advance = display_text.chars().count() as f32 * text_style.font_size * 0.6;
          let total_advance: f32 = if !text_runs.is_empty() {
            text_runs.iter().map(|run| run.advance).sum()
          } else {
            fallback_advance
          };
          let start_x = Self::aligned_text_start_x(&text_style, text_rect, total_advance);
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
                let rect = Rect::from_xywh(
                  start_x + seg_start,
                  top,
                  (seg_end - seg_start).max(0.0),
                  (bottom - top).max(0.0),
                );
                if let Some(clipped) =
                  rect.intersection(text_rect).filter(|r| r.width() > 0.0 && r.height() > 0.0)
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
            let max_caret_x = (text_rect.max_x() - 1.0).max(text_rect.x());
            let caret_x = caret_x.clamp(text_rect.x(), max_caret_x);

            let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            let caret_rect_raw = self.snap_form_control_caret_rect(caret_rect_raw);
            if let Some(clipped) = caret_rect_raw.intersection(padding_rect) {
              if clipped.width() > 0.0 && clipped.height() > 0.0 {
                caret_rect = Some((clipped, caret_color));
              }
            }
          }
        }

        for selection_rect in selection_rects {
          self.list.push(DisplayItem::FillRect(FillRectItem {
            rect: selection_rect,
            color: selection_color,
          }));
        }
        if text_style.color.a > f32::EPSILON {
          if let Some(text) = paint_text {
            let _ = self.emit_text_with_style_raw(text, Some(&text_style), centered_text_rect);
          }
        }
        if let Some(affordance_rect) = affordance_rect {
          let mut affordance_style = style.clone();
          affordance_style.color = muted_accent;
          affordance_style.font_size = (style.font_size * 0.7).max(8.0);
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
              let _ = emit_text_aligned(self, "▲", &affordance_style, upper, true, true);
              let _ = emit_text_aligned(self, "▼", &affordance_style, lower, true, true);
            }
            TextControlKind::Date => {
              let _ = emit_text_aligned(self, "▾", &affordance_style, affordance_rect, true, true);
            }
            _ => {}
          }
        }

        for preedit_underline_rect in preedit_underline_rects {
          self.list.push(DisplayItem::FillRect(FillRectItem {
            rect: preedit_underline_rect,
            color: text_style.color,
          }));
        }
        if let Some((caret_rect, caret_color)) = caret_rect {
          self.list.push(DisplayItem::FillRect(FillRectItem {
            rect: caret_rect,
            color: caret_color,
          }));
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
        let rect = inset_rect(content_rect, 2.0);

        let mut paint_text_owned: Option<String> = None;
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
          paint_text_owned = Some(combined);
          paint_text = paint_text_owned.as_deref();
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
        } else if !value.is_empty() {
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
          let mut cloned = (*pseudo_style).clone();
          let opacity = cloned.opacity.clamp(0.0, 1.0);
          let alpha = (cloned.color.a * opacity).clamp(0.0, 1.0);
          cloned.color = cloned.color.with_alpha(alpha);
          cloned.opacity = 1.0;
          cloned
        } else {
          let mut cloned = style.clone();
          cloned.color = fallback_color;
          cloned
        };
        let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
        let metrics_scaled = Self::resolve_scaled_metrics(&text_style, &self.font_ctx);
        let line_height = compute_line_height_with_metrics_viewport(
          &text_style,
          metrics_scaled.as_ref(),
          viewport,
          self.font_ctx.root_font_metrics(),
        );
        if line_height <= 0.0 || !line_height.is_finite() {
          return true;
        }

        let selection_color = Rgba {
          r: 0,
          g: 120,
          b: 215,
          a: 0.35,
        };
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
        let caret_color = match style.caret_color {
          CaretColor::Color(c) => c,
          CaretColor::Auto => style.color,
        };
        let mut caret_rect: Option<Rect> = None;

        let mut metrics_sample = display_text;
        if trim_ascii_whitespace(metrics_sample).is_empty() {
          metrics_sample = "M";
        }
        let metrics_runs = shape_text_runs(self, metrics_sample, &text_style).unwrap_or_default();
        let metrics = match &text_style.line_height {
          crate::style::types::LineHeight::Normal => InlineTextItem::metrics_from_runs(
            &self.font_ctx,
            &metrics_runs,
            line_height,
            text_style.font_size,
          ),
          _ => InlineTextItem::metrics_from_first_available_font(
            metrics_scaled.as_ref(),
            line_height,
            text_style.font_size,
          ),
        };
        let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;

        let mut scroll_y = self.element_scroll_offset(fragment).y;
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

        let caret_line_idx =
          crate::textarea::textarea_visual_line_index_for_caret(display_text, &layout, caret_idx);

        let start_line = (scroll_y / line_height).floor().max(0.0) as usize;
        let end_line = ((scroll_y + viewport_height) / line_height)
          .ceil()
          .max(start_line as f32) as usize;
        let end_line = end_line.min(layout.lines.len());
        let y_offset = scroll_y - start_line as f32 * line_height;
        let mut y = rect.y() - y_offset;

        self.push_clip(rect);
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

          let line_runs = shape_text_runs(self, line_text, &text_style).unwrap_or_default();
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
                  let sel_rect = Rect::from_xywh(
                    start_x + seg_start,
                    top,
                    (seg_end - seg_start).max(0.0),
                    (bottom - top).max(0.0),
                  );
                  if let Some(clipped) = sel_rect.intersection(rect) {
                    if clipped.width() > 0.0 && clipped.height() > 0.0 {
                      self.list.push(DisplayItem::FillRect(FillRectItem {
                        rect: clipped,
                        color: selection_color,
                      }));
                    }
                  }
                }
              }
            }
          }

          if text_style.color.a > f32::EPSILON && !line_text.is_empty() {
            let _ = self.emit_text_with_style_raw(line_text, Some(&text_style), line_rect);
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
                      self.list.push(DisplayItem::FillRect(FillRectItem {
                        rect: clipped,
                        color: text_style.color,
                      }));
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
              + caret_x_for_position(&caret_stops, caret_col, caret_affinity_for_paint)
                .unwrap_or(0.0);
            let max_caret_x = (line_rect.max_x() - 1.0).max(line_rect.x());
            let caret_x = caret_x.clamp(line_rect.x(), max_caret_x);
            let caret_rect_raw = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            let caret_rect_raw = self.snap_form_control_caret_rect(caret_rect_raw);
            caret_rect = caret_rect_raw
              .intersection(rect)
              .filter(|r| r.width() > 0.0 && r.height() > 0.0);
          }
        }
        self.pop_clip();

        if let Some(caret_rect) = caret_rect {
          self.list.push(DisplayItem::FillRect(FillRectItem {
            rect: caret_rect,
            color: caret_color,
          }));
        }
        true
      }
      FormControlKind::Select(select) => {
        let is_listbox = select.multiple || select.size > 1;

        if is_listbox {
          let total_rows = select.items.len();
          if total_rows == 0 {
            return true;
          }

          let viewport_height = content_rect.height().max(0.0);
          if viewport_height <= 0.0 || !viewport_height.is_finite() {
            return true;
          }

          // Listbox selects paint `size` rows inside the control. When the author specifies an
          // explicit `height`, stretch/shrink the row height so the visible rows always fill the
          // content box.
          let visible_rows = select.size.max(1) as f32;
          let row_height = viewport_height / visible_rows;
          if row_height <= 0.0 || !row_height.is_finite() {
            return true;
          }

          let content_height = row_height * total_rows as f32;
          let mut scroll_y = self.element_scroll_offset(fragment).y;
          if !scroll_y.is_finite() {
            scroll_y = 0.0;
          }
          let max_scroll_y = (content_height - viewport_height).max(0.0);
          scroll_y = scroll_y.clamp(0.0, max_scroll_y);

          let scrollbar_width = if max_scroll_y > 0.0 {
            resolve_scrollbar_width(style).min(content_rect.width().max(0.0))
          } else {
            0.0
          };
          let (text_rect, scrollbar_rect) = if scrollbar_width > 0.0 {
            let (text, scrollbar) =
              Self::split_rect_inline_end(content_rect, style, scrollbar_width);
            (text, Some(scrollbar))
          } else {
            (content_rect, None)
          };

          let start_row = (scroll_y / row_height).floor().max(0.0) as usize;
          let end_row = ((scroll_y + viewport_height) / row_height)
            .ceil()
            .max(start_row as f32) as usize;
          let end_row = end_row.min(total_rows);
          let y_offset = scroll_y - start_row as f32 * row_height;
          let mut y = content_rect.y() - y_offset;

          self.push_clip(content_rect);
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
                // Indent optgroup labels on the inline-start side (mirrors in RTL).
                let (_, row_rect) = Self::split_rect_inline_start(row_rect, style, 2.0);
                let _ = self.emit_text_with_style_raw(label, Some(&row_style), row_rect);
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
                  self.list.push(DisplayItem::FillRect(FillRectItem {
                    rect: row_rect,
                    color: highlight,
                  }));
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
                // Indent options on the inline-start side so nesting mirrors in RTL.
                let (_, row_rect) = Self::split_rect_inline_start(row_rect, style, indent);
                let _ = self.emit_text_with_style_raw(label, Some(&row_style), row_rect);
              }
            }
          }

          if let Some(track_rect) = scrollbar_rect.filter(|_| content_rect.height() > 0.0) {
            self.list.push(DisplayItem::FillRect(FillRectItem {
              rect: track_rect,
              color: Rgba::rgb(240, 240, 240),
            }));

            let visible_fraction = (viewport_height / content_height.max(1.0)).clamp(0.0, 1.0);
            let thumb_height = (track_rect.height() * visible_fraction).max(8.0);
            let thumb_height = thumb_height.min(track_rect.height());
            let travel = (track_rect.height() - thumb_height).max(0.0);
            let thumb_y = track_rect.y() + travel * (scroll_y / max_scroll_y.max(1.0));
            let thumb_rect =
              Rect::from_xywh(track_rect.x(), thumb_y, track_rect.width(), thumb_height);
            self.list.push(DisplayItem::FillRect(FillRectItem {
              rect: thumb_rect,
              color: Rgba::rgb(180, 180, 180),
            }));
          }

          self.pop_clip();
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

          let mut select_style = style.clone();
          if control.invalid {
            select_style.color = accent;
          }
          let mut label_rect = content_rect;
          let arrow_rect = if matches!(control.appearance, Appearance::None) {
            None
          } else {
            let inline_end_padding = if inline_axis_is_horizontal(style.writing_mode) {
              if inline_positive {
                (padding_rect.max_x() - content_rect.max_x()).max(0.0)
              } else {
                (content_rect.x() - padding_rect.x()).max(0.0)
              }
            } else if inline_positive {
              (padding_rect.max_y() - content_rect.max_y()).max(0.0)
            } else {
              (content_rect.y() - padding_rect.y()).max(0.0)
            };

            if inline_end_padding > 0.0 {
              Some(if inline_axis_is_horizontal(style.writing_mode) {
                if inline_positive {
                  Rect::from_xywh(
                    content_rect.max_x(),
                    content_rect.y(),
                    inline_end_padding,
                    content_rect.height(),
                  )
                } else {
                  Rect::from_xywh(
                    padding_rect.x(),
                    content_rect.y(),
                    inline_end_padding,
                    content_rect.height(),
                  )
                }
              } else if inline_positive {
                Rect::from_xywh(
                  content_rect.x(),
                  content_rect.max_y(),
                  content_rect.width(),
                  inline_end_padding,
                )
              } else {
                Rect::from_xywh(
                  content_rect.x(),
                  padding_rect.y(),
                  content_rect.width(),
                  inline_end_padding,
                )
              })
            } else {
              let inline_len = if inline_axis_is_horizontal(style.writing_mode) {
                content_rect.width().max(0.0)
              } else {
                content_rect.height().max(0.0)
              };
              let arrow_space = 14.0_f32.min(inline_len);
              if arrow_space <= 0.0 {
                None
              } else {
                let (label, arrow) = Self::split_rect_inline_end(content_rect, style, arrow_space);
                label_rect = label;
                Some(arrow)
              }
            }
          };

          let _ = self.emit_text_with_style_raw(label, Some(&select_style), label_rect);

          if let Some(arrow_rect) = arrow_rect {
            let mut arrow_style = select_style;
            arrow_style.color = muted_accent;
            arrow_style.font_size = (style.font_size * 0.9).max(8.0);
            let _ = emit_text_aligned(self, "▾", &arrow_style, arrow_rect, true, true);
          }
          true
        }
      }
      FormControlKind::Button { label } => {
        if label.is_empty() {
          return true;
        }
        let rect = content_rect;
        let mut button_style = style.clone();
        if control.invalid {
          button_style.color = accent;
        }
        let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
        let metrics_scaled = Self::resolve_scaled_metrics(&button_style, &self.font_ctx);
        let line_height = compute_line_height_with_metrics_viewport(
          &button_style,
          metrics_scaled.as_ref(),
          viewport,
          self.font_ctx.root_font_metrics(),
        );
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
        let _ = self.emit_text_with_style_raw(label, Some(&button_style), centered_rect);
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

        let fill_rect = padding_rect;
        if *is_radio {
          if *checked {
            let diameter = fill_rect.width().min(fill_rect.height()) * 0.55;
            let dot_rect = Rect::from_xywh(
              fill_rect.x() + (fill_rect.width() - diameter) / 2.0,
              fill_rect.y() + (fill_rect.height() - diameter) / 2.0,
              diameter,
              diameter,
            );
            self
              .list
              .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
                rect: dot_rect,
                color: muted_accent,
                radii: BorderRadii::uniform(diameter / 2.0),
              }));
          }
          return true;
        }

        // Fill the inner area using the accent color, then draw the check/indeterminate mark in a
        // contrasting color.
        let radii = Self::resolve_clip_radii(
          style,
          &rects,
          BackgroundBox::PaddingBox,
          self.viewport,
          self.build_breakdown.as_deref(),
        )
        .clamped(fill_rect.width(), fill_rect.height());
        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect: fill_rect,
            color: muted_accent,
            radii,
          }));

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
        let _ = emit_text_aligned(self, glyph, &mark_style, fill_rect, true, true);
        true
      }
      FormControlKind::Range { value, min, max } => {
        let min_val = *min;
        let max_val = *max;
        let span = (max_val - min_val).abs().max(0.0001);
        let clamped = ((*value - min_val) / span).clamp(0.0, 1.0);
        let appearance_none = matches!(control.appearance, Appearance::None);
        let track_style = control.slider_track_style.as_deref();
        let thumb_style = control.slider_thumb_style.as_deref();
        let viewport = self.viewport;

        let resolve_px = |style: &ComputedStyle, len: Length, percentage_base: f32| -> f32 {
          Self::resolve_length_for_paint(
            &len,
            style.font_size,
            style.root_font_size,
            percentage_base,
            viewport,
          )
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
        let knob_center_x = if inline_positive {
          padding_rect.x() + knob_width / 2.0 + clamped * knob_travel
        } else {
          padding_rect.max_x() - knob_width / 2.0 - clamped * knob_travel
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

        // Default range styling draws a track + accent fill; `appearance: none` should avoid
        // injecting UA-specific fills, but author track pseudo-element styling is still expected
        // to paint.
        let should_paint_track = if appearance_none {
          track_style.is_some()
        } else {
          true
        };

        if should_paint_track {
          let track_opacity = track_style
            .map(|style| style.opacity.clamp(0.0, 1.0))
            .unwrap_or(1.0);
          let push_track_opacity =
            track_style.is_some() && track_opacity > 0.0 && track_opacity < 1.0 - f32::EPSILON;
          if push_track_opacity {
            self.push_opacity(track_opacity);
          }
          if track_style.is_some() && track_opacity <= 0.0 {
            // Fully transparent track pseudo-element.
          } else {
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
                self.emit_box_shadows_from_style(track_rect, track_style, false);
                self.emit_background_from_style_with_culling_rect(
                  track_rect,
                  track_style,
                  culling_rect,
                  Point::ZERO,
                );
              } else {
                self
                  .list
                  .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
                    rect: track_rect,
                    color: Rgba::rgb(190, 190, 190),
                    radii: BorderRadii::uniform(track_height / 2.0),
                  }));
              }

              if !appearance_none {
                let filled_rect = if inline_positive {
                  Rect::from_xywh(
                    track_rect.x(),
                    track_rect.y(),
                    (knob_center_x - track_rect.x()).max(0.0),
                    track_rect.height(),
                  )
                } else {
                  Rect::from_xywh(
                    knob_center_x,
                    track_rect.y(),
                    (track_rect.max_x() - knob_center_x).max(0.0),
                    track_rect.height(),
                  )
                };
                if filled_rect.width() > 0.0 {
                  let radii = BorderRadii::uniform(track_height / 2.0)
                    .clamped(filled_rect.width(), filled_rect.height());
                  self
                    .list
                    .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
                      rect: filled_rect,
                      color: muted_accent,
                      radii,
                    }));
                }
              }

              if let Some(track_style) = track_style {
                self.emit_box_shadows_from_style(track_rect, track_style, true);
                self.emit_border_from_style(track_rect, track_style, None);
              }
            }
          }
          if push_track_opacity {
            self.pop_opacity();
          }
        }

        if let Some(thumb_style) = thumb_style {
          let thumb_opacity = thumb_style.opacity.clamp(0.0, 1.0);
          let push_thumb_opacity = thumb_opacity > 0.0 && thumb_opacity < 1.0 - f32::EPSILON;
          if push_thumb_opacity {
            self.push_opacity(thumb_opacity);
          }
          if thumb_opacity <= 0.0 {
            // Fully transparent thumb pseudo-element.
          } else {
            let mut style_for_thumb;
            let style_for_thumb = if Self::border_radius_is_zero(thumb_style) {
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

            self.emit_box_shadows_from_style(knob_rect, style_for_thumb, false);
            self.emit_background_from_style_with_culling_rect(
              knob_rect,
              style_for_thumb,
              culling_rect,
              Point::ZERO,
            );
            self.emit_box_shadows_from_style(knob_rect, style_for_thumb, true);
            self.emit_border_from_style(knob_rect, style_for_thumb, None);
          }
          if push_thumb_opacity {
            self.pop_opacity();
          }
        } else {
          let knob_radius = knob_width.min(knob_height) / 2.0;
          self
            .list
            .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
              rect: knob_rect,
              color: Rgba::rgb(255, 255, 255),
              radii: BorderRadii::uniform(knob_radius),
            }));
          self
            .list
            .push(DisplayItem::StrokeRoundedRect(StrokeRoundedRectItem {
              rect: knob_rect,
              color: Rgba::rgb(130, 130, 130),
              width: 1.0,
              radii: BorderRadii::uniform(knob_radius),
            }));
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

        let track_radii = Self::resolve_clip_radii(
          track_style.unwrap_or(style),
          &rects,
          BackgroundBox::ContentBox,
          self.viewport,
          self.build_breakdown.as_deref(),
        )
        .clamped(track_rect.width(), track_rect.height());

        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect: track_rect,
            color: track_color,
            radii: track_radii,
          }));

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
            // Match common browser behaviour: RTL progress fills from the right edge.
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

        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect: fill_rect,
            color: fill_color,
            radii: fill_radii,
          }));
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

        let track_radii = Self::resolve_clip_radii(
          track_style.unwrap_or(style),
          &rects,
          BackgroundBox::ContentBox,
          self.viewport,
          self.build_breakdown.as_deref(),
        )
        .clamped(track_rect.width(), track_rect.height());

        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect: track_rect,
            color: track_color,
            radii: track_radii,
          }));

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

        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect: fill_rect,
            color: fill_color,
            radii: fill_radii,
          }));
        true
      }
      FormControlKind::Color { value, raw } => {
        if matches!(control.appearance, Appearance::None) {
          // `appearance: none` leaves the author-provided background/border visible; suppress the
          // native swatch/label painting.
          return true;
        }
        let rect = inset_rect(padding_rect, 2.0);
        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect,
            color: *value,
            radii: BorderRadii::uniform((rect.height().min(rect.width()) / 5.0).max(2.0)),
          }));
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
        let _ = emit_text_aligned(self, &label, &text_style, rect, true, true);
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
          button_text_style.color = if control.disabled {
            base_color.with_alpha(0.5)
          } else {
            base_color
          };
        }

        let rect = inset_rect(content_rect, 2.0);
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
          return true;
        }

        let viewport = self.viewport;
        let resolve_px = |style: &ComputedStyle, len: Length, percentage_base: f32| -> f32 {
          Self::resolve_length_for_paint(
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

        let inline_len = if inline_axis_is_horizontal(style.writing_mode) {
          rect.width().max(0.0)
        } else {
          rect.height().max(0.0)
        };
        let gap = 6.0_f32.min(inline_len);
        let (button_plus_gap, file_rect) =
          Self::split_rect_inline_start(rect, style, (button_w + gap).min(inline_len));
        let (button_base, _) = Self::split_rect_inline_start(
          button_plus_gap,
          style,
          button_w.min(if inline_axis_is_horizontal(style.writing_mode) {
            button_plus_gap.width().max(0.0)
          } else {
            button_plus_gap.height().max(0.0)
          }),
        );
        let button_rect = Rect::from_xywh(
          button_base.x(),
          rect.y() + (rect.height() - button_h) / 2.0,
          button_base.width(),
          button_h,
        );

        if let Some(button_style) = button_pseudo_style {
          self.emit_box_shadows_from_style(button_rect, button_style, false);
          self.emit_background_from_style_with_culling_rect(
            button_rect,
            button_style,
            culling_rect,
            Point::ZERO,
          );
          self.emit_box_shadows_from_style(button_rect, button_style, true);
          self.emit_border_from_style(button_rect, button_style, None);
        } else if !appearance_none {
          let button_bg = if control.disabled {
            Rgba::rgb(235, 235, 235)
          } else {
            Rgba::rgb(245, 245, 245)
          };
          let border = Rgba::rgb(180, 180, 180);
          let radius = BorderRadii::uniform((button_rect.height() / 6.0).max(2.0));
          self
            .list
            .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
              rect: button_rect,
              color: button_bg,
              radii: radius,
            }));
          self
            .list
            .push(DisplayItem::StrokeRoundedRect(StrokeRoundedRectItem {
              rect: button_rect,
              color: border,
              width: 1.0,
              radii: radius,
            }));
        }

        if button_rect.width() > 0.0 && button_rect.height() > 0.0 {
          let _ = emit_text_aligned(
            self,
            button_label,
            &button_text_style,
            button_rect,
            true,
            true,
          );
        }

        if file_rect.width() > 0.0 && file_rect.height() > 0.0 {
          let text_rect = inset_rect(file_rect, 2.0);
          let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
          let metrics_scaled = Self::resolve_scaled_metrics(&file_style, &self.font_ctx);
          let line_height = compute_line_height_with_metrics_viewport(
            &file_style,
            metrics_scaled.as_ref(),
            viewport,
            self.font_ctx.root_font_metrics(),
          );
          let baseline_offset_y = if line_height.is_finite() {
            (text_rect.height() - line_height) / 2.0
          } else {
            0.0
          };
          let baseline_offset_y = if baseline_offset_y.is_finite() {
            baseline_offset_y
          } else {
            0.0
          };
          let centered_text_rect = Rect::from_xywh(
            text_rect.x(),
            text_rect.y() + baseline_offset_y,
            text_rect.width(),
            text_rect.height(),
          );
          let _ = self.emit_text_with_style_raw(file_label, Some(&file_style), centered_text_rect);
        }

        true
      }
      FormControlKind::Unknown { label } => {
        if let Some(text) = label {
          let rect = content_rect;
          let mut unknown_style = style.clone();
          if control.invalid {
            unknown_style.color = accent;
          }
          let _ = self.emit_text_with_style_raw(text, Some(&unknown_style), rect);
        }
        true
      }
    }
  }

  fn emit_alt_text(&mut self, alt: &str, fragment: &FragmentNode, rect: Rect) -> bool {
    let style = fragment.style.as_deref();
    let (content_rect, clip_radii) = self.replaced_content_rect_and_radii(rect, style);
    let clip_contents =
      Self::replaced_content_clip_item(style, content_rect, content_rect, clip_radii);

    if let Some(clip) = clip_contents.as_ref() {
      self.list.push(DisplayItem::PushClip(clip.clone()));
    }
    // Browser UAs paint broken-image alt text alongside the "broken image" icon rather than
    // starting at the very left edge of the image box (which would overlap the icon/border).
    //
    // Keep the behavior stable by matching the placeholder icon sizing rules from
    // `emit_replaced_placeholder`.
    let ok = if matches!(
      fragment.content,
      FragmentContent::Replaced {
        replaced_type: ReplacedType::Image { .. },
        ..
      }
    ) {
      let icon_inset = 2.0;
      let icon_gap = 2.0;
      let icon_size = Self::missing_image_icon_size(content_rect);

      let content_inner_rect = Rect::from_xywh(
        content_rect.x() + icon_inset,
        content_rect.y() + icon_inset,
        (content_rect.width() - icon_inset * 2.0).max(0.0),
        (content_rect.height() - icon_inset * 2.0).max(0.0),
      );

      if icon_size > 0.0 {
        // Chrome positions the broken-image icon at the top-left of the replaced box. The
        // accompanying alt text is laid out in the remaining inline space and can be affected by
        // `text-align` (e.g. `center` aligns the alt text within the space to the right of the
        // icon). Don't override `text-align` here; doing so causes noticeable diffs on pages that
        // center-align hero content (e.g. kotlinlang.org).
        let icon_rect =
          Rect::from_xywh(content_inner_rect.x(), content_inner_rect.y(), icon_size, icon_size);
        self.emit_broken_image_icon(icon_rect);

        let text_x = icon_rect.x() + icon_size + icon_gap;
        let text_rect = Rect::from_xywh(
          text_x,
          content_inner_rect.y(),
          (content_inner_rect.x() + content_inner_rect.width() - text_x).max(0.0),
          content_inner_rect.height(),
        );

        self.emit_text_with_style(alt, style, text_rect)
      } else {
        self.emit_text_with_style(alt, style, content_inner_rect)
      }
    } else {
      self.emit_text_with_style(alt, style, content_rect)
    };
    if clip_contents.is_some() {
      self.list.push(DisplayItem::PopClip);
    }
    ok
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

  fn emit_text_with_style(
    &mut self,
    text: &str,
    style: Option<&ComputedStyle>,
    rect: Rect,
  ) -> bool {
    self.emit_text_with_style_impl(text, style, rect, true)
  }

  fn emit_text_with_style_raw(
    &mut self,
    text: &str,
    style: Option<&ComputedStyle>,
    rect: Rect,
  ) -> bool {
    self.emit_text_with_style_impl(text, style, rect, false)
  }

  fn emit_text_with_style_impl(
    &mut self,
    text: &str,
    style: Option<&ComputedStyle>,
    rect: Rect,
    trim: bool,
  ) -> bool {
    let text = if trim {
      trim_ascii_whitespace(text)
    } else {
      text
    };
    if text.is_empty() {
      return false;
    }

    let Some(style) = style else {
      return self.emit_naive_text(text, rect, None);
    };

    let shape_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let shaped = self.shaper.shape(text, style, &self.font_ctx);
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), shape_timer) {
      breakdown.record_text_shape(start.elapsed());
    }
    let mut runs = match shaped {
      Ok(r) => r,
      Err(_) => return self.emit_naive_text(text, rect, Some(style)),
    };
    InlineTextItem::apply_spacing_to_runs(
      &mut runs,
      text,
      style.letter_spacing,
      style.word_spacing,
    );

    let metrics_scaled = Self::resolve_scaled_metrics(style, &self.font_ctx);
    let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
    let mut line_height = compute_line_height_with_metrics_viewport(
      style,
      metrics_scaled.as_ref(),
      viewport,
      self.font_ctx.root_font_metrics(),
    );
    if !line_height.is_finite() || line_height <= 0.0 {
      line_height = style.font_size.max(1.0);
    }

    let metrics = match &style.line_height {
      crate::style::types::LineHeight::Normal => {
        InlineTextItem::metrics_from_runs(&self.font_ctx, &runs, line_height, style.font_size)
      }
      _ => InlineTextItem::metrics_from_first_available_font(
        metrics_scaled.as_ref(),
        line_height,
        style.font_size,
      ),
    };
    let baseline = rect.y() + metrics.baseline_offset;
    let advance_width: f32 = runs.iter().map(|run| run.advance).sum();
    let start_x = Self::aligned_text_start_x(style, rect, advance_width);
    let shadows = Self::text_shadows_from_style(Some(style), self.viewport);

    // Text emitted via this helper (alt-text fallback, placeholder labels, etc.) needs to be
    // wrapped within its destination rect; otherwise long `<img alt>` strings end up rendered as a
    // single clipped line (a large source of fixture diffs for pages with missing images).
    //
    // We implement a lightweight greedy line-breaking pass over the shaped glyphs so we can wrap
    // without re-running the shaping pipeline for every line.
    let max_width = rect.width();
    let max_height = rect.height();

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
    let breaks = crate::text::line_break::find_break_opportunities(text);
    let has_mandatory_break = breaks
      .iter()
      .any(|brk| brk.is_mandatory() && brk.byte_offset < text.len());

    let needs_wrap = crate::style::inline_axis_is_horizontal(style.writing_mode)
      && max_width.is_finite()
      && max_width > 0.0
      && max_height.is_finite()
      && max_height > 0.0
      && ((allows_soft_wrap && advance_width > max_width)
        || (preserves_newlines && has_mandatory_break));

    if needs_wrap {
      let baseline0 = baseline;
      let max_lines = if line_height.is_finite() && line_height > 0.0 {
        (max_height / line_height).floor().max(1.0) as usize
      } else {
        1
      };

      // Flatten glyph advances into a single slice for the line breaker, converting HarfBuzz's
      // per-run cluster offsets into offsets relative to the full string.
      let mut glyphs: Vec<crate::text::pipeline::GlyphPosition> = Vec::new();
      let mut run_glyph_starts: Vec<usize> = Vec::with_capacity(runs.len());
      for run in &runs {
        run_glyph_starts.push(glyphs.len());
        let run_start = run.start;
        for glyph in &run.glyphs {
          let mut g = *glyph;
          g.cluster = (run_start + glyph.cluster as usize) as u32;
          glyphs.push(g);
        }
      }

      let break_opportunities: Vec<crate::text::line_break::BreakOpportunity> = if allows_soft_wrap
      {
        breaks
      } else {
        breaks
          .into_iter()
          .filter(|brk| brk.is_mandatory())
          .collect()
      };
      let lines = crate::text::line_break::break_lines(&glyphs, max_width, &break_opportunities);

      // Prefix sums for quickly turning glyph index ranges into widths.
      let mut prefix_adv: Vec<f32> = Vec::with_capacity(glyphs.len() + 1);
      prefix_adv.push(0.0);
      for glyph in &glyphs {
        let prev = prefix_adv.last().copied().unwrap_or(0.0);
        let next = prev + glyph.x_advance;
        prefix_adv.push(next);
      }

      // CSS collapses trailing spaces at the end of wrapped lines for the common collapsing
      // white-space values. Avoid counting/painting these glyphs so alignment matches browsers.
      let trim_trailing_spaces = matches!(
        style.white_space,
        crate::style::types::WhiteSpace::Normal
          | crate::style::types::WhiteSpace::Nowrap
          | crate::style::types::WhiteSpace::PreLine
      );

      for (line_idx, line) in lines.iter().enumerate() {
        if line_idx >= max_lines {
          break;
        }
        let Some(segment) = line.segments.first() else {
          continue;
        };
        if segment.glyph_count == 0 {
          continue;
        }

        let glyph_start = segment.glyph_start;
        let mut glyph_end = segment.glyph_end().min(glyphs.len());
        if glyph_start >= glyph_end {
          continue;
        }

        if trim_trailing_spaces {
          while glyph_end > glyph_start {
            let cluster = glyphs[glyph_end - 1].cluster as usize;
            if cluster < text.len() && text.as_bytes()[cluster] == b' ' {
              glyph_end -= 1;
              continue;
            }
            break;
          }
          if glyph_start >= glyph_end {
            continue;
          }
        }

        let baseline = baseline0 + line_idx as f32 * line_height;
        if !baseline.is_finite() {
          break;
        }
        if baseline + metrics.descent > rect.max_y() + 0.01 {
          break;
        }

        let line_width = (prefix_adv[glyph_end] - prefix_adv[glyph_start]).max(0.0);
        let start_x = Self::aligned_text_start_x(style, rect, line_width);

        let mut line_runs: Vec<ShapedRun> = Vec::new();
        for (run_idx, run) in runs.iter().enumerate() {
          let run_start = *run_glyph_starts.get(run_idx).unwrap_or(&0);
          let run_end = run_start + run.glyphs.len();
          let seg_start = glyph_start.max(run_start);
          let seg_end = glyph_end.min(run_end);
          if seg_start >= seg_end {
            continue;
          }
          let local_start = seg_start - run_start;
          let local_end = seg_end - run_start;
          let slice = run.glyphs[local_start..local_end].to_vec();
          if slice.is_empty() {
            continue;
          }
          let advance: f32 = if run.vertical {
            slice.iter().map(|g| g.y_advance).sum()
          } else {
            slice.iter().map(|g| g.x_advance).sum()
          };
          line_runs.push(ShapedRun {
            text: String::new(),
            start: 0,
            end: 0,
            glyphs: slice,
            direction: run.direction,
            level: run.level,
            advance,
            font: Arc::clone(&run.font),
            font_size: run.font_size,
            baseline_shift: run.baseline_shift,
            language: run.language.clone(),
            features: Arc::clone(&run.features),
            synthetic_bold: run.synthetic_bold,
            synthetic_oblique: run.synthetic_oblique,
            rotation: run.rotation,
            vertical: run.vertical,
            palette_index: run.palette_index,
            palette_overrides: Arc::clone(&run.palette_overrides),
            palette_override_hash: run.palette_override_hash,
            variations: run.variations.clone(),
            scale: run.scale,
          });
        }

        if !line_runs.is_empty() {
          self.emit_shaped_runs(
            &line_runs,
            style.color,
            baseline,
            start_x,
            &shadows,
            Some(style),
            false,
            TextEmphasisOffset::default(),
          );
        }
      }
      return true;
    }

    self.emit_shaped_runs(
      &runs,
      style.color,
      baseline,
      start_x,
      &shadows,
      Some(style),
      false,
      TextEmphasisOffset::default(),
    );
    true
  }

  fn emit_naive_text(&mut self, text: &str, rect: Rect, style: Option<&ComputedStyle>) -> bool {
    let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
    let color = style.map(|s| s.color).unwrap_or(Rgba::BLACK);
    let allow_subpixel_aa = style.map(|s| s.allow_subpixel_aa).unwrap_or(true);
    let shadows = Self::text_shadows_from_style(style, self.viewport);
    let char_width = font_size * 0.6;
    let advance_width = text.len() as f32 * char_width;
    let start_x = style
      .map(|style| Self::aligned_text_start_x(style, rect, advance_width))
      .unwrap_or(rect.x());
    let origin = Point::new(start_x, rect.y() + font_size * 0.8);
    let mut bounds = ConservativeGlyphRunBoundsBuilder::new(origin, advance_width);
    let mut glyphs = Vec::new();
    for (i, _) in text.chars().enumerate() {
      let offset = Point::new(i as f32 * char_width, 0.0);
      bounds.include_glyph(offset.x, char_width);
      glyphs.push(GlyphInstance {
        glyph_id: 0,
        cluster: i as u32,
        x_offset: offset.x,
        y_offset: offset.y,
        x_advance: char_width,
        y_advance: 0.0,
      });
    }
    let cached_bounds = bounds.finish(font_size);
    let (stroke_width, stroke_color) = style
      .map(|s| self.resolve_webkit_text_stroke_for_run(s, font_size))
      .unwrap_or_default();

    let item = TextItem {
      origin,
      cached_bounds: Some(cached_bounds),
      glyphs,
      color,
      allow_subpixel_aa,
      stroke_width,
      stroke_color,
      font_smoothing: style.map(|s| s.font_smoothing).unwrap_or_default(),
      shadows,
      font_size,
      advance_width,
      decorations: Vec::new(),
      ..Default::default()
    };
    self.list.push(DisplayItem::Text(item));
    true
  }

  fn emit_iframe_image(
    &mut self,
    image: Arc<ImageData>,
    rect: Rect,
    style: Option<&ComputedStyle>,
  ) {
    let (content_rect, clip_radii) = self.replaced_content_rect_and_radii(rect, style);
    let clip_contents =
      Self::replaced_content_clip_item(style, content_rect, content_rect, clip_radii);

    if let Some(clip) = clip_contents.as_ref() {
      self.list.push(DisplayItem::PushClip(clip.clone()));
    }

    self.list.push(DisplayItem::Image(ImageItem {
      dest_rect: content_rect,
      image,
      filter_quality: Self::image_filter_quality(style),
      src_rect: None,
    }));

    if clip_contents.is_some() {
      self.list.push(DisplayItem::PopClip);
    }
  }

  fn emit_inline_svg(
    &mut self,
    content: &SvgContent,
    rect: Rect,
    style: Option<&ComputedStyle>,
  ) -> bool {
    let Some(image_cache) = self.image_cache.as_ref() else {
      return false;
    };

    let base_svg = if content.svg.is_empty() {
      content.fallback_svg.as_str()
    } else {
      content.svg.as_str()
    };
    if base_svg.is_empty() {
      return false;
    }

    let meta = match image_cache.probe_svg_content(base_svg, "inline-svg") {
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
    let Some((img_w_css, img_h_css)) = meta.css_dimensions(
      orientation,
      &image_resolution,
      self.device_pixel_ratio,
      None,
    ) else {
      return false;
    };

    let fit = style.map(|s| s.object_fit).unwrap_or(ObjectFit::Fill);
    let pos = style
      .map(|s| s.object_position)
      .unwrap_or_else(default_object_position);
    let font_size = style.map(|s| s.font_size).unwrap_or(16.0);
    let root_font_size = style.map(|s| s.root_font_size).unwrap_or(font_size);
    let has_intrinsic_ratio = meta.intrinsic_ratio(orientation).is_some();

    let (content_rect, clip_radii) = self.replaced_content_rect_and_radii(rect, style);
    let (dest_x, dest_y, dest_w, dest_h) = match compute_object_fit(
      fit,
      pos,
      content_rect.width(),
      content_rect.height(),
      img_w_css,
      img_h_css,
      has_intrinsic_ratio,
      font_size,
      root_font_size,
      self.viewport,
    ) {
      Some(v) => v,
      None => return true,
    };
    if dest_w <= 0.0 || dest_h <= 0.0 {
      return true;
    }
    let dest_rect = Rect::from_xywh(
      content_rect.x() + dest_x,
      content_rect.y() + dest_y,
      dest_w,
      dest_h,
    );
    let clip_contents =
      Self::replaced_content_clip_item(style, content_rect, dest_rect, clip_radii);

    let dest_w_device = dest_w * self.device_pixel_ratio;
    let dest_h_device = dest_h * self.device_pixel_ratio;
    if dest_w_device <= 0.0 || dest_h_device <= 0.0 {
      return true;
    }
    let render_w = dest_w_device.ceil().max(1.0) as u32;
    let render_h = dest_h_device.ceil().max(1.0) as u32;

    let resolved_svg = if content.foreign_objects.is_empty() {
      None
    } else {
      let foreign_object_svg = if content.svg.is_empty() {
        base_svg
      } else {
        content.svg.as_str()
      };
      let foreign_object_dpr =
        crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
          foreign_object_svg,
          self.device_pixel_ratio,
          dest_w,
          dest_h,
          img_w_css,
          img_h_css,
        );
      crate::paint::svg_foreign_object::inline_svg_with_foreign_objects(
        &content.svg,
        &content.foreign_objects,
        &content.shared_css,
        &self.font_ctx,
        image_cache,
        foreign_object_dpr,
        self.max_iframe_depth,
      )
    };
    let svg = if let Some(resolved) = resolved_svg.as_deref() {
      resolved
    } else if content.foreign_objects.is_empty() {
      content.svg.as_str()
    } else if !content.fallback_svg.is_empty() {
      content.fallback_svg.as_str()
    } else {
      content.svg.as_str()
    };
    if svg.is_empty() {
      return false;
    }

    let mut injected_svg: Option<String> = None;
    let mut injected_insert_pos: Option<usize> = None;
    if let Some(defs) = self.svg_id_defs_raw.as_ref() {
      if let Some((patched, pos)) =
        crate::paint::svg_id_defs_injection::inject_svg_id_defs_raw(svg, defs.as_ref())
      {
        injected_svg = Some(patched);
        injected_insert_pos = Some(pos);
      }
    }
    let svg = injected_svg.as_deref().unwrap_or(svg);

    let style_injection = content.document_css_injection.as_ref();
    let defs_injection = self
      .svg_id_defs
      .as_ref()
      .and_then(|defs| crate::paint::svg_mask_image::defs_injection_for_svg_fragment(defs, svg));

    let mut insert_pos = injected_insert_pos.or_else(|| style_injection.map(|inj| inj.insert_pos));
    if let Some(pos) = insert_pos {
      if svg.get(..pos).is_none() || svg.get(pos..).is_none() {
        insert_pos = None;
      }
    }
    if insert_pos.is_none() && (defs_injection.is_some() || style_injection.is_some()) {
      insert_pos = crate::paint::svg_mask_image::svg_root_start_tag_end(svg);
    }

    let injection: Option<(usize, Cow<'_, str>)> =
      match (insert_pos, defs_injection, style_injection) {
        (Some(pos), Some(defs), Some(style)) => {
          let mut combined = defs;
          combined.reserve(style.style_element.len());
          combined.push_str(style.style_element.as_ref());
          Some((pos, Cow::Owned(combined)))
        }
        (Some(pos), Some(defs), None) => Some((pos, Cow::Owned(defs))),
        (Some(pos), None, Some(style)) => Some((pos, Cow::Borrowed(style.style_element.as_ref()))),
        _ => None,
      };

    let (content_hash, content_len) = match injection.as_ref() {
      Some((insert_pos, injected)) => match (svg.get(..*insert_pos), svg.get(*insert_pos..)) {
        // Hash the final SVG markup (with injected markup) without allocating it, matching
        // `str::hash` semantics (single 0xFF terminator byte).
        (Some(prefix), Some(suffix)) => {
          let injected = injected.as_ref();
          let mut hasher = DefaultHasher::new();
          hasher.write(prefix.as_bytes());
          hasher.write(injected.as_bytes());
          hasher.write(suffix.as_bytes());
          hasher.write_u8(0xff);
          (
            hasher.finish(),
            prefix.len() + injected.len() + suffix.len(),
          )
        }
        _ => {
          let mut hasher = DefaultHasher::new();
          svg.hash(&mut hasher);
          (hasher.finish(), svg.len())
        }
      },
      None => {
        let mut hasher = DefaultHasher::new();
        svg.hash(&mut hasher);
        (hasher.finish(), svg.len())
      }
    };

    // Avoid caching large inline SVG markup strings in display-list builder caches by using a
    // compact hash-based identifier.
    let inline_key = format!(
      "inline-svg:{:016x}:{}:{}x{}",
      content_hash, content_len, render_w, render_h
    );
    let used_resolution =
      image_resolution.used_resolution(None, meta.resolution, self.device_pixel_ratio);
    let cache_key = ImageKey::new(
      inline_key,
      CrossOriginAttribute::None,
      None,
      orientation,
      false,
      used_resolution,
      self.device_pixel_ratio,
    );

    {
      let mut decoded_cache = self
        .decoded_image_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner());
      if let Some(image) = decoded_cache.get(&cache_key) {
        if let Some(clip) = clip_contents.as_ref() {
          self.list.push(DisplayItem::PushClip(clip.clone()));
        }
        self.list.push(DisplayItem::Image(ImageItem {
          dest_rect,
          image,
          filter_quality: Self::image_filter_quality(style),
          src_rect: None,
        }));
        if clip_contents.is_some() {
          self.list.push(DisplayItem::PopClip);
        }
        return true;
      }
    }

    let decode_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let pixmap = match injection.as_ref() {
      Some((insert_pos, injected)) => image_cache.render_svg_pixmap_at_size_with_injected_style(
        svg,
        *insert_pos,
        injected.as_ref(),
        render_w,
        render_h,
        "inline-svg",
        self.device_pixel_ratio,
      ),
      None => image_cache.render_svg_pixmap_at_size(
        svg,
        render_w,
        render_h,
        "inline-svg",
        self.device_pixel_ratio,
      ),
    };
    let pixmap = match pixmap {
      Ok(pixmap) => pixmap,
      Err(_) => {
        if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
          breakdown.record_image_decode(start.elapsed());
        }
        return false;
      }
    };

    // `ImageData::from_pixmap` clones the underlying pixel buffer, so charge the bytes before we
    // allocate/copy them.
    if let Some(bytes) = u64::from(pixmap.width())
      .checked_mul(u64::from(pixmap.height()))
      .and_then(|px| px.checked_mul(4))
    {
      if let Err(err) = crate::render_control::reserve_allocation_with(bytes, || {
        format!(
          "image data pixel buffer {}x{} url=inline-svg",
          pixmap.width(),
          pixmap.height()
        )
      }) {
        self.error.get_or_insert(err);
        if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
          breakdown.record_image_decode(start.elapsed());
        }
        return false;
      }
    }

    let image_data = Arc::new(ImageData::from_pixmap(pixmap.as_ref(), dest_w, dest_h));

    let mut decoded_cache = self
      .decoded_image_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = decoded_cache.get(&cache_key) {
      if let Some(clip) = clip_contents.as_ref() {
        self.list.push(DisplayItem::PushClip(clip.clone()));
      }
      self.list.push(DisplayItem::Image(ImageItem {
        dest_rect,
        image: existing,
        filter_quality: Self::image_filter_quality(style),
        src_rect: None,
      }));
      if clip_contents.is_some() {
        self.list.push(DisplayItem::PopClip);
      }
      if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
        breakdown.record_image_decode(start.elapsed());
      }
      return true;
    }
    decoded_cache.insert(cache_key, image_data.clone());

    if let Some(clip) = clip_contents.as_ref() {
      self.list.push(DisplayItem::PushClip(clip.clone()));
    }
    self.list.push(DisplayItem::Image(ImageItem {
      dest_rect,
      image: image_data,
      filter_quality: Self::image_filter_quality(style),
      src_rect: None,
    }));
    if clip_contents.is_some() {
      self.list.push(DisplayItem::PopClip);
    }
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
      breakdown.record_image_decode(start.elapsed());
    }
    true
  }

  fn should_defer_async_image_decode(
    &self,
    dest_width: f32,
    dest_height: f32,
    src: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
  ) -> bool {
    let Some(image_cache) = self.image_cache.as_ref() else {
      return false;
    };
    if !dest_width.is_finite()
      || !dest_height.is_finite()
      || dest_width <= 0.0
      || dest_height <= 0.0
    {
      return false;
    }
    let dpr = if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0 {
      self.device_pixel_ratio
    } else {
      1.0
    };
    let device_w = (dest_width * dpr).ceil().max(0.0) as u64;
    let device_h = (dest_height * dpr).ceil().max(0.0) as u64;
    let dest_pixels = device_w.saturating_mul(device_h);
    if dest_pixels <= ASYNC_IMAGE_DECODE_MAX_DEST_PIXELS {
      return false;
    }
    let meta = match image_cache.probe_with_crossorigin_and_referrer_policy(
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

  #[inline]
  fn note_resolved_image_url(&self, resolved_url: &str) {
    if !crate::image_loader::url_looks_like_gif(resolved_url) {
      return;
    }
    self.saw_gif_image.store(true, Ordering::Relaxed);

    if self
      .image_cache
      .as_ref()
      .and_then(|cache| cache.animation_time_ms())
      .is_some()
    {
      self
        .saw_animation_time_dependent_image
        .store(true, Ordering::Relaxed);
    }
  }

  fn decode_image(
    &self,
    src: &str,
    style: Option<&ComputedStyle>,
    decorative: bool,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    reject_placeholder: bool,
    override_resolution: Option<f32>,
  ) -> Option<Arc<ImageData>> {
    let image_cache = self.image_cache.as_ref()?;
    let trimmed = trim_ascii_whitespace_start(src);
    let inline_svg = trimmed.starts_with('<');

    fn contains_foreign_object_tag(svg: &str) -> bool {
      const NEEDLE: &[u8] = b"foreignobject";
      let bytes = svg.as_bytes();
      bytes
        .windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
    }

    let (resolved_src, image) = if inline_svg {
      use std::collections::hash_map::DefaultHasher;
      use std::hash::{Hash, Hasher};
      let mut hasher = DefaultHasher::new();
      trimmed.hash(&mut hasher);
      let cache_key = format!("inline-svg:{:016x}:{}", hasher.finish(), trimmed.len());
      let image = image_cache.render_svg(trimmed).ok()?;
      (cache_key, image)
    } else {
      let resolved_src = image_cache.resolve_url(src);
      let image = image_cache
        .load_with_crossorigin_and_referrer_policy(&resolved_src, crossorigin, referrer_policy)
        .ok()?;
      (resolved_src, image)
    };

    self.note_resolved_image_url(&resolved_src);

    if reject_placeholder && !inline_svg && image_cache.is_placeholder_image(&image) {
      return None;
    }

    let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
    let orientation = style
      .map(|s| s.image_orientation.resolve(image.orientation, decorative))
      .unwrap_or_else(|| ImageOrientation::default().resolve(image.orientation, decorative));
    let has_intrinsic_ratio = image.intrinsic_ratio(orientation).is_some();
    let used_resolution = image_resolution.used_resolution(
      override_resolution,
      image.resolution,
      self.device_pixel_ratio,
    );
    let key = ImageKey::new(
      resolved_src,
      crossorigin,
      referrer_policy,
      orientation,
      decorative,
      used_resolution,
      self.device_pixel_ratio,
    );

    {
      let mut decoded_cache = self
        .decoded_image_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner());
      if let Some(image) = decoded_cache.get(&key) {
        return Some(image);
      }
    }

    let decode_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let (css_w, css_h) = if decorative {
      let (w, h) = image.css_natural_dimensions(
        orientation,
        &image_resolution,
        self.device_pixel_ratio,
        override_resolution,
      );
      (w.unwrap_or(0.0), h.unwrap_or(0.0))
    } else {
      match image.css_dimensions(
        orientation,
        &image_resolution,
        self.device_pixel_ratio,
        override_resolution,
      ) {
        Some(dimensions) => dimensions,
        None => {
          if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
            breakdown.record_image_decode(start.elapsed());
          }
          return None;
        }
      }
    };

    let mut rendered_svg_with_foreign_object: Option<ImageData> = None;
    if image.is_vector {
      let svg_markup = if inline_svg {
        Some(trimmed)
      } else {
        image.svg_content.as_deref()
      };
      if let Some(svg_markup) = svg_markup.filter(|svg| contains_foreign_object_tag(svg)) {
        let base_dpr = if self.device_pixel_ratio.is_finite() && self.device_pixel_ratio > 0.0 {
          self.device_pixel_ratio
        } else {
          1.0
        };

        let rendered_width_css = image.width() as f32;
        let rendered_height_css = image.height() as f32;
        let foreign_object_dpr =
          crate::paint::svg_foreign_object::foreign_object_html_device_pixel_ratio(
            svg_markup,
            base_dpr,
            rendered_width_css,
            rendered_height_css,
            rendered_width_css,
            rendered_height_css,
          );

        if let Some(resolved_svg) =
          crate::paint::svg_foreign_object::inline_svg_foreign_objects_from_markup(
            svg_markup,
            "",
            &self.font_ctx,
            image_cache,
            foreign_object_dpr,
            self.max_iframe_depth,
          )
        {
          let render_w = ((image.width() as f32) * base_dpr).ceil().max(1.0) as u32;
          let render_h = ((image.height() as f32) * base_dpr).ceil().max(1.0) as u32;
          if render_w > 0 && render_h > 0 {
            let render_url = key.url.as_str();
            if let Ok(pixmap) = image_cache.render_svg_pixmap_at_size(
              &resolved_svg,
              render_w,
              render_h,
              render_url,
              base_dpr,
            ) {
              if let Some(bytes) = u64::from(render_w)
                .checked_mul(u64::from(render_h))
                .and_then(|px| px.checked_mul(4))
              {
                if crate::render_control::reserve_allocation_with(bytes, || {
                  format!(
                    "image data pixel buffer {}x{} url={}",
                    render_w, render_h, render_url
                  )
                })
                .is_err()
                {
                  if let (Some(breakdown), Some(start)) =
                    (self.build_breakdown.as_ref(), decode_timer)
                  {
                    breakdown.record_image_decode(start.elapsed());
                  }
                  return None;
                }
              }
              let pixels = pixmap.data().to_vec();
              // Apply `image-orientation` semantics to match `CachedImage::to_oriented_rgba`.
              let mut rgba = image::RgbaImage::from_raw(render_w, render_h, pixels)?;
              match orientation.quarter_turns % 4 {
                0 => {}
                1 => rgba = image::imageops::rotate90(&rgba),
                2 => rgba = image::imageops::rotate180(&rgba),
                3 => rgba = image::imageops::rotate270(&rgba),
                _ => {}
              }
              if orientation.flip_x {
                rgba = image::imageops::flip_horizontal(&rgba);
              }

              let (w, h) = rgba.dimensions();
              if w > 0 && h > 0 {
                let mut data = ImageData::new_premultiplied(w, h, css_w, css_h, rgba.into_raw());
                data.has_intrinsic_ratio = has_intrinsic_ratio;
                rendered_svg_with_foreign_object = Some(data);
              }
            }
          }
        }
      }
    }

    let mut image_data = if let Some(custom) = rendered_svg_with_foreign_object {
      custom
    } else {
      // `CachedImage::to_oriented_rgba` can allocate a full RGBA8 buffer even when the source image
      // is already decoded. Budget the output buffer so extremely large images can be aborted
      // deterministically.
      let (w, h) = image.oriented_dimensions(orientation);
      if let Some(bytes) = u64::from(w)
        .checked_mul(u64::from(h))
        .and_then(|px| px.checked_mul(4))
      {
        if crate::render_control::reserve_allocation_with(bytes, || {
          format!("image data pixel buffer {}x{} url={}", w, h, key.url)
        })
        .is_err()
        {
          if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
            breakdown.record_image_decode(start.elapsed());
          }
          return None;
        }
      }
      let rgba = image.to_oriented_rgba(orientation);
      let (w, h) = rgba.dimensions();
      if w == 0 || h == 0 {
        if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
          breakdown.record_image_decode(start.elapsed());
        }
        return None;
      }
      ImageData::new(w, h, css_w, css_h, rgba.into_raw())
    };
    image_data.has_intrinsic_ratio = has_intrinsic_ratio;
    let image_data = Arc::new(image_data);

    let mut decoded_cache = self
      .decoded_image_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    if let Some(image) = decoded_cache.get(&key) {
      if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
        breakdown.record_image_decode(start.elapsed());
      }
      return Some(image);
    }
    decoded_cache.insert(key, image_data.clone());
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
      breakdown.record_image_decode(start.elapsed());
    }
    Some(image_data)
  }

  fn image_filter_quality(style: Option<&ComputedStyle>) -> ImageFilterQuality {
    match style.map(|s| s.image_rendering) {
      Some(ImageRendering::CrispEdges) | Some(ImageRendering::Pixelated) => {
        ImageFilterQuality::Nearest
      }
      _ => ImageFilterQuality::Linear,
    }
  }
}

#[derive(Debug, Clone)]
struct DecorationMetrics {
  underline_pos: f32,
  underline_thickness: f32,
  strike_pos: f32,
  strike_thickness: f32,
  ascent: f32,
  descent: f32,
  has_font_underline_metrics: bool,
}

fn collect_underline_exclusions(
  runs: &[ShapedRun],
  baseline_y: f32,
  band_top: f32,
  band_bottom: f32,
  skip_all: bool,
  extra_pad_px: f32,
) -> Vec<(f32, f32)> {
  crate::paint::text_decoration_skip_ink::collect_underline_exclusions(
    runs,
    0.0,
    baseline_y,
    band_top,
    band_bottom,
    skip_all,
    1.0,
    extra_pad_px,
  )
}

fn collect_underline_exclusions_vertical(
  runs: &[ShapedRun],
  block_baseline: f32,
  band_left: f32,
  band_right: f32,
  skip_all: bool,
  extra_pad_px: f32,
) -> Vec<(f32, f32)> {
  crate::paint::text_decoration_skip_ink::collect_underline_exclusions_vertical(
    runs,
    0.0,
    block_baseline,
    band_left,
    band_right,
    skip_all,
    1.0,
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

impl Default for DisplayListBuilder {
  fn default() -> Self {
    Self::new()
  }
}

/// Resolves the computed `transform`/`perspective`/motion path into a 3D matrix for painting.
pub(crate) fn resolve_transform3d(
  style: &ComputedStyle,
  bounds: Rect,
  viewport: Option<(f32, f32)>,
) -> Option<Transform3D> {
  DisplayListBuilder::build_transform(style, bounds, viewport).self_transform
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::Declaration;
  use crate::css::types::PropertyValue;
  use crate::css::types::Transform;
  use crate::error::{Error, RenderError, RenderStage};
  use crate::image_loader::ImageCache;
  use crate::paint::display_list::text_bounds;
  use crate::paint::display_list::ResolvedMaskImage;
  use crate::paint::display_list_renderer::DisplayListRenderer;
  use crate::paint::stacking::StackingContext;
  use crate::paint::stacking::StackingContextReason;
  use crate::render_control::RenderDeadline;
  use crate::render_control::{StageAllocationBudget, StageAllocationBudgetGuard, StageHeartbeat};
  use crate::style::color::Color;
  use crate::style::color::Rgba;
  use crate::style::content::parse_content;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;
  use crate::style::display::Display;
  use crate::style::position::Position;
  use crate::style::properties::apply_declaration;
  use crate::style::properties::with_image_set_dpr;
  use crate::style::types::BackgroundImage;
  use crate::style::types::BackgroundImageUrl;
  use crate::style::types::BackgroundLayer;
  use crate::style::types::BackgroundRepeat;
  use crate::style::types::BackgroundRepeatKeyword;
  use crate::style::types::BackgroundSize;
  use crate::style::types::BackgroundSizeComponent;
  use crate::style::types::BasicShape;
  use crate::style::types::UrlImage;
  use crate::style::types::ClipComponent;
  use crate::style::types::ClipPath;
  use crate::style::types::ClipRect;
  use crate::style::types::Containment;
  use crate::style::types::ImageOrientation;
  use crate::style::types::ImageRendering;
  use crate::style::types::LineHeight;
  use crate::style::types::MixBlendMode;
  use crate::style::types::MotionPathCommand;
  use crate::style::types::MotionPosition;
  use crate::style::types::OffsetAnchor;
  use crate::style::types::OffsetPath;
  use crate::style::types::Overflow;
  use crate::style::types::TextDecorationLine;
  use crate::style::types::TextDecorationThickness;
  use crate::style::types::TextEmphasisSkip;
  use crate::style::types::TextUnderlineOffset;
  use crate::style::types::TextUnderlinePosition;
  use crate::style::types::TransformBox;
  use crate::style::types::TransformStyle;
  use crate::style::types::WillChange;
  use crate::style::types::WillChangeHint;
  use crate::style::values::CalcLength;
  use crate::style::values::Length;
  use crate::style::values::LengthUnit;
  use crate::style::ComputedStyle;
  use crate::text::face_cache;
  use crate::text::font_db::FontDatabase;
  use crate::text::font_loader::FontContext;
  use crate::text::pipeline::ShapingPipeline;
  use crate::tree::box_tree::{CrossOriginAttribute, ReplacedType, SrcsetCandidate, SrcsetDescriptor};
  use crate::tree::fragment_tree::FragmentTree;
  use crate::{debug::runtime::RuntimeToggles, paint::painter::enable_paint_diagnostics};
  use base64::engine::general_purpose;
  use base64::Engine as _;
  use image::codecs::png::PngEncoder;
  use image::ColorType;
  use image::ImageEncoder;
  use std::collections::HashMap;
  use std::path::PathBuf;
  use std::sync::Arc;

  fn create_block_fragment(x: f32, y: f32, width: f32, height: f32) -> FragmentNode {
    FragmentNode::new_block(Rect::from_xywh(x, y, width, height), vec![])
  }

  fn create_text_fragment(x: f32, y: f32, width: f32, height: f32, text: &str) -> FragmentNode {
    FragmentNode::new_text(Rect::from_xywh(x, y, width, height), text.to_string(), 12.0)
  }

  fn styled_element(tag: &str) -> crate::style::cascade::StyledNode {
    crate::style::cascade::StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Element {
          tag_name: tag.to_string(),
          namespace: crate::dom::HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: crate::style::cascade::StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }
  }

  #[test]
  fn fork_preserves_media_provider_for_parallel_build() {
    // Parallel display list building is clamped by `crate::system::cpu_budget()` (e.g. cgroup CPU
    // quotas). When the budget is single-threaded, `DisplayListBuilder` will correctly fall back to
    // serial traversal and will not exercise the `fork()` path.
    if crate::system::cpu_budget() <= 1 {
      return;
    }

    struct MockProvider;

    impl crate::media::MediaFrameProvider for MockProvider {
      fn video_frame(
        &self,
        _box_id: Option<usize>,
        src: &str,
        _size_hint: Option<crate::media::MediaFrameSizeHint>,
      ) -> Option<Arc<ImageData>> {
        if src == "v.mp4" {
          Some(Arc::new(ImageData::new_pixels(1, 1, vec![0, 255, 0, 255])))
        } else {
          None
        }
      }
    }

    let video = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 16.0, 16.0),
      ReplacedType::Video {
        src: "v.mp4".to_string(),
        poster: None,
        controls: false,
      },
    );
    let sibling = FragmentNode::new_block(Rect::from_xywh(16.0, 0.0, 16.0, 16.0), vec![]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 32.0, 16.0), vec![video, sibling]);

    let mut raw = HashMap::new();
    raw.insert("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "1".to_string());
    let toggles = Arc::new(RuntimeToggles::from_map(raw));

    let list = crate::debug::runtime::with_thread_runtime_toggles(toggles, || {
      let parallel = PaintParallelism {
        mode: PaintParallelismMode::Enabled,
        min_build_fragments: 1,
        build_chunk_size: 1,
        ..PaintParallelism::enabled()
      };
      let mock =
        Some(Arc::new(MockProvider) as Arc<dyn crate::media::MediaFrameProvider>);
      let builder = DisplayListBuilder::new()
        .with_parallelism(&parallel)
        .with_media_provider(mock);

      let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .expect("rayon thread pool");
      pool.install(|| builder.build(&root))
    });

    assert!(
      list.items()
        .iter()
        .any(|item| matches!(item, DisplayItem::Image(_))),
      "expected display list to contain an Image item from the video frame provider"
    );
  }

  mod fixed_position_ignores_viewport_scroll_test {
    use crate::css::types::Transform;
    use crate::geometry::{Point, Rect};
    use crate::paint::display_list_builder::DisplayListBuilder;
    use crate::paint::display_list_renderer::DisplayListRenderer;
    use crate::scroll::ScrollState;
    use crate::style::position::Position;
    use crate::style::values::Length;
    use crate::text::font_loader::FontContext;
    use crate::tree::fragment_tree::FragmentNode;
    use crate::{ComputedStyle, Rgba};
    use std::sync::Arc;

    fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
      let px = pixmap.pixel(x, y).expect("pixel inside viewport");
      (px.red(), px.green(), px.blue(), px.alpha())
    }

    fn render(root: &FragmentNode, scroll: Point) -> tiny_skia::Pixmap {
      let scroll_state = ScrollState::with_viewport(scroll);
      let offset = Point::new(-scroll.x, -scroll.y);

      let list = DisplayListBuilder::new()
        .with_scroll_state(scroll_state)
        .build_with_stacking_tree_offset(root, offset);

      DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
        .unwrap()
        .render(&list)
        .unwrap()
    }

    fn base_scene_with_fixed_root() -> FragmentNode {
      let mut green_style = ComputedStyle::default();
      green_style.background_color = Rgba::GREEN;
      let green_style = Arc::new(green_style);

      let mut blue_style = ComputedStyle::default();
      blue_style.background_color = Rgba::BLUE;
      let blue_style = Arc::new(blue_style);

      let mut red_fixed_style = ComputedStyle::default();
      red_fixed_style.background_color = Rgba::RED;
      red_fixed_style.position = Position::Fixed;
      let red_fixed_style = Arc::new(red_fixed_style);

      let stripe_a = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
        vec![],
        green_style,
      );
      let stripe_b = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
        vec![],
        blue_style,
      );
      let fixed = FragmentNode::new_block_styled(
        // Cover the left half so we can check both covered/uncovered pixels.
        Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
        vec![],
        red_fixed_style,
      );

      // Place the fixed element last so it paints over the scrolled content.
      FragmentNode::new_block(
        Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
        vec![stripe_a, stripe_b, fixed],
      )
    }

    fn scene_with_fixed_containing_block() -> FragmentNode {
      let mut container_style = ComputedStyle::default();
      container_style.containment.layout = true;
      let container_style = Arc::new(container_style);

      let mut green_style = ComputedStyle::default();
      green_style.background_color = Rgba::GREEN;
      let green_style = Arc::new(green_style);

      let mut blue_style = ComputedStyle::default();
      blue_style.background_color = Rgba::BLUE;
      let blue_style = Arc::new(blue_style);

      let mut red_fixed_style = ComputedStyle::default();
      red_fixed_style.background_color = Rgba::RED;
      red_fixed_style.position = Position::Fixed;
      let red_fixed_style = Arc::new(red_fixed_style);

      let stripe_a = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
        vec![],
        green_style,
      );
      let stripe_b = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
        vec![],
        blue_style,
      );
      let fixed = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
        vec![],
        red_fixed_style,
      );

      let container = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
        vec![stripe_a, stripe_b, fixed],
        container_style,
      );

      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![container])
    }

    fn scene_with_transformed_fixed_containing_block() -> FragmentNode {
      let mut container_style = ComputedStyle::default();
      container_style.transform = vec![Transform::TranslateX(Length::px(0.0))];
      let container_style = Arc::new(container_style);

      let mut green_style = ComputedStyle::default();
      green_style.background_color = Rgba::GREEN;
      let green_style = Arc::new(green_style);

      let mut blue_style = ComputedStyle::default();
      blue_style.background_color = Rgba::BLUE;
      let blue_style = Arc::new(blue_style);

      let mut red_fixed_style = ComputedStyle::default();
      red_fixed_style.background_color = Rgba::RED;
      red_fixed_style.position = Position::Fixed;
      let red_fixed_style = Arc::new(red_fixed_style);

      let stripe_a = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
        vec![],
        green_style,
      );
      let stripe_b = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
        vec![],
        blue_style,
      );
      let fixed = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
        vec![],
        red_fixed_style,
      );

      let container = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
        vec![stripe_a, stripe_b, fixed],
        container_style,
      );

      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![container])
    }

    fn scene_with_perspective_fixed_containing_block() -> FragmentNode {
      let mut container_style = ComputedStyle::default();
      container_style.perspective = Some(Length::px(100.0));
      let container_style = Arc::new(container_style);

      let mut green_style = ComputedStyle::default();
      green_style.background_color = Rgba::GREEN;
      let green_style = Arc::new(green_style);

      let mut blue_style = ComputedStyle::default();
      blue_style.background_color = Rgba::BLUE;
      let blue_style = Arc::new(blue_style);

      let mut red_fixed_style = ComputedStyle::default();
      red_fixed_style.background_color = Rgba::RED;
      red_fixed_style.position = Position::Fixed;
      let red_fixed_style = Arc::new(red_fixed_style);

      let stripe_a = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
        vec![],
        green_style,
      );
      let stripe_b = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
        vec![],
        blue_style,
      );
      let fixed = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
        vec![],
        red_fixed_style,
      );

      let container = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
        vec![stripe_a, stripe_b, fixed],
        container_style,
      );

      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![container])
    }

    fn scene_with_nested_fixed_elements() -> FragmentNode {
      let mut blue_style = ComputedStyle::default();
      blue_style.background_color = Rgba::BLUE;
      let blue_style = Arc::new(blue_style);

      let mut black_style = ComputedStyle::default();
      black_style.background_color = Rgba::BLACK;
      let black_style = Arc::new(black_style);

      let mut outer_fixed_style = ComputedStyle::default();
      outer_fixed_style.background_color = Rgba::RED;
      outer_fixed_style.position = Position::Fixed;
      let outer_fixed_style = Arc::new(outer_fixed_style);

      let mut inner_fixed_style = ComputedStyle::default();
      inner_fixed_style.background_color = Rgba::GREEN;
      inner_fixed_style.position = Position::Fixed;
      let inner_fixed_style = Arc::new(inner_fixed_style);

      let stripe_a = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
        vec![],
        blue_style,
      );
      let stripe_b = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
        vec![],
        black_style,
      );

      // Nested fixed elements remain positioned relative to the viewport. Model the nested fixed
      // element by giving it an origin relative to the outer fixed element such that its absolute
      // position is still (0, 0) at scroll=0.
      let inner_fixed = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, -2.0, 8.0, 2.0),
        vec![],
        inner_fixed_style,
      );
      let outer_fixed = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
        vec![inner_fixed],
        outer_fixed_style,
      );

      // Place the outer fixed element last so it paints over the scrolling content.
      FragmentNode::new_block(
        Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
        vec![stripe_a, stripe_b, outer_fixed],
      )
    }

    #[test]
    fn fixed_position_is_not_translated_by_viewport_scroll() {
      let root = base_scene_with_fixed_root();
      let pixmap = render(&root, Point::new(0.0, 2.0));

      // Fixed element stays pinned to the viewport.
      assert_eq!(pixel(&pixmap, 1, 0), (255, 0, 0, 255));
      assert_eq!(pixel(&pixmap, 1, 1), (255, 0, 0, 255));

      // Content scrolls underneath it: the second (blue) stripe moves from y=2..4 to y=0..2.
      assert_eq!(pixel(&pixmap, 6, 0), (0, 0, 255, 255));
      assert_eq!(pixel(&pixmap, 6, 1), (0, 0, 255, 255));
    }

    #[test]
    fn fixed_position_inside_fixed_containing_block_is_translated_by_viewport_scroll() {
      let root = scene_with_fixed_containing_block();
      let pixmap = render(&root, Point::new(0.0, 2.0));

      // The container establishes the fixed containing block, so the fixed element scrolls away.
      assert_eq!(pixel(&pixmap, 1, 0), (0, 0, 255, 255));
      assert_eq!(pixel(&pixmap, 1, 1), (0, 0, 255, 255));
    }

    #[test]
    fn fixed_position_inside_transformed_containing_block_is_translated_by_viewport_scroll() {
      let root = scene_with_transformed_fixed_containing_block();
      let pixmap = render(&root, Point::new(0.0, 2.0));

      // A non-none transform establishes the fixed containing block, so the fixed element scrolls away.
      assert_eq!(pixel(&pixmap, 1, 0), (0, 0, 255, 255));
      assert_eq!(pixel(&pixmap, 1, 1), (0, 0, 255, 255));
    }

    #[test]
    fn fixed_position_inside_perspective_containing_block_is_translated_by_viewport_scroll() {
      let root = scene_with_perspective_fixed_containing_block();
      let pixmap = render(&root, Point::new(0.0, 2.0));

      // A perspective value establishes the fixed containing block, so the fixed element scrolls away.
      assert_eq!(pixel(&pixmap, 1, 0), (0, 0, 255, 255));
      assert_eq!(pixel(&pixmap, 1, 1), (0, 0, 255, 255));
    }

    #[test]
    fn nested_fixed_position_is_not_double_translated_by_viewport_scroll() {
      let root = scene_with_nested_fixed_elements();
      let pixmap = render(&root, Point::new(0.0, 2.0));

      // Inner fixed stays pinned at y=0 and does not cancel scroll twice.
      assert_eq!(pixel(&pixmap, 1, 0), (0, 255, 0, 255));
      // Outer fixed stays pinned at y=2.
      assert_eq!(pixel(&pixmap, 1, 2), (255, 0, 0, 255));
    }
  }

  #[test]
  fn svg_viewbox_only_background_auto_size_behaves_like_contain() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 450 175"></svg>"#;
    let intrinsic = DisplayListBuilder::svg_intrinsic_dimensions_for_css(svg, 16.0, 16.0);
    assert_eq!(intrinsic.width, None);
    assert_eq!(intrinsic.height, None);
    let ratio = intrinsic.aspect_ratio.expect("expected viewBox ratio");
    assert!((ratio - (450.0 / 175.0)).abs() < 1e-6, "unexpected ratio {ratio}");

    let layer = BackgroundLayer::default();
    let (tile_w, tile_h) = DisplayListBuilder::compute_background_size(
      &layer,
      16.0,
      16.0,
      None,
      450.0,
      175.0,
      intrinsic.width.unwrap_or(0.0),
      intrinsic.height.unwrap_or(0.0),
      intrinsic.aspect_ratio,
    );
    assert!(
      (tile_w - 450.0).abs() < 1e-3,
      "expected tile width to match background area; got {tile_w}"
    );
    assert!(
      (tile_h - 175.0).abs() < 1e-3,
      "expected tile height to match background area; got {tile_h}"
    );
  }

  #[test]
  fn display_list_build_allocation_budget_exceeded() {
    let budget = Arc::new(StageAllocationBudget::new(0));
    let _guard = StageAllocationBudgetGuard::install(Some(&budget));
    let mut builder = DisplayListBuilder::new();
    builder.emit_background(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), Rgba::RED);
    let err = builder.finish().unwrap_err();
    match err {
      Error::Render(RenderError::StageAllocationBudgetExceeded {
        stage,
        heartbeat,
        ..
      }) => {
        assert_eq!(stage, RenderStage::Paint);
        assert_eq!(heartbeat, StageHeartbeat::PaintBuild);
      }
      other => panic!("expected StageAllocationBudgetExceeded, got {other:?}"),
    }
  }

  #[test]
  fn inline_svg_image_data_allocation_budget_exceeded() {
    // Render a 1×1 inline SVG into a pixmap (4 bytes) and then clone it into display-list
    // `ImageData` (another 4 bytes). With a budget of 5 bytes we should exceed once we attempt to
    // allocate the `ImageData` pixel buffer.
    let budget = Arc::new(StageAllocationBudget::new(5));
    let _guard = StageAllocationBudgetGuard::install(Some(&budget));
    let _stage_guard = crate::render_control::StageGuard::install(Some(RenderStage::Paint));
    let _heartbeat_guard =
      crate::render_control::StageHeartbeatGuard::install(Some(StageHeartbeat::PaintBuild));
    let mut builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let svg = crate::tree::box_tree::SvgContent::raw(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#,
    );
    let _ = builder.emit_inline_svg(&svg, Rect::from_xywh(0.0, 0.0, 1.0, 1.0), None);
    let err = builder.finish().unwrap_err();
    match err {
      Error::Render(RenderError::StageAllocationBudgetExceeded {
        stage,
        heartbeat,
        ..
      }) => {
        assert_eq!(stage, RenderStage::Paint);
        assert_eq!(heartbeat, StageHeartbeat::PaintBuild);
      }
      other => panic!("expected StageAllocationBudgetExceeded, got {other:?}"),
    }
  }

  #[test]
  fn mask_image_fragment_only_urls_do_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let defs = HashMap::from([(
      "mask".to_string(),
      r#"<mask id="mask"><rect width="100%" height="100%" fill="white"/></mask>"#.to_string(),
    )]);
    let builder = DisplayListBuilder::new().with_svg_id_defs(Some(Arc::new(defs)));
    let style = ComputedStyle::default();
    let bounds = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);
    let mask_url = UrlImage::new("#mask".to_string());
    let mask_url_with_nbsp = UrlImage::new(format!("#mask{nbsp}"));

    assert!(
      builder
        .decode_mask_image_url(&mask_url, &style, bounds)
        .is_some(),
      "expected fragment-only URL to resolve with SVG id defs"
    );
    assert!(
      builder
        .decode_mask_image_url(&mask_url_with_nbsp, &style, bounds)
        .is_none(),
      "expected NBSP-suffixed fragment to not match existing SVG ids"
    );
  }

  #[test]
  fn mask_image_fragment_only_urls_rasterize_at_device_pixel_ratio() {
    let defs = HashMap::from([(
      "mask".to_string(),
      r#"<mask id="mask"><rect width="100%" height="100%" fill="white"/></mask>"#.to_string(),
    )]);
    let builder = DisplayListBuilder::new()
      .with_svg_id_defs(Some(Arc::new(defs)))
      .with_device_pixel_ratio(2.0);
    let style = ComputedStyle::default();
    let bounds = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);
    let mask_url = UrlImage::new("#mask".to_string());

    let image = builder
      .decode_mask_image_url(&mask_url, &style, bounds)
      .expect("expected fragment-only mask to resolve");

    assert_eq!(image.width, 32);
    assert_eq!(image.height, 32);
    assert_eq!(image.css_width, 16.0);
    assert_eq!(image.css_height, 16.0);
  }

  #[test]
  fn clip_path_fragment_only_urls_do_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let defs = HashMap::from([(
      "clip".to_string(),
      r#"<clipPath xmlns="http://www.w3.org/2000/svg" id="clip"><rect width="100%" height="100%"/></clipPath>"#
        .to_string(),
    )]);
    let builder = DisplayListBuilder::new().with_svg_id_defs(Some(Arc::new(defs)));
    let style = ComputedStyle::default();
    let bounds = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

    assert!(
      builder
        .decode_clip_path_url("#clip", &style, bounds, bounds)
        .is_some(),
      "expected fragment-only URL to resolve with SVG id defs"
    );
    assert!(
      builder
        .decode_clip_path_url(&format!("#clip{nbsp}"), &style, bounds, bounds)
        .is_none(),
      "expected NBSP-suffixed fragment to not match existing SVG ids"
    );
  }

  #[test]
  fn non_ascii_whitespace_emit_text_with_style_does_not_trim_nbsp() {
    let mut builder = DisplayListBuilder::new();
    let style = ComputedStyle::default();
    let rect = Rect::from_xywh(0.0, 0.0, 100.0, 20.0);
    assert!(
      builder.emit_text_with_style("\u{00A0}", Some(&style), rect),
      "NBSP must not be treated as trim() whitespace when emitting text"
    );
  }

  #[test]
  fn emit_text_with_style_baseline_offset_does_not_double_count_half_leading() {
    let mut builder = DisplayListBuilder::new();
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 20.0;
    style.line_height = LineHeight::Length(Length::px(60.0));

    let rect = Rect::from_xywh(0.0, 0.0, 200.0, 80.0);
    let text = "baseline test";
    assert!(
      builder.emit_text_with_style(text, Some(&style), rect),
      "expected text emission to succeed"
    );

    let items: Vec<&TextItem> = builder
      .list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::Text(text) => Some(text),
        _ => None,
      })
      .collect();
    assert!(!items.is_empty(), "expected at least one text item");

    let mut runs = builder
      .shaper
      .shape(text, &style, &builder.font_ctx)
      .expect("shape");
    InlineTextItem::apply_spacing_to_runs(
      &mut runs,
      text,
      style.letter_spacing,
      style.word_spacing,
    );
    let metrics_scaled = DisplayListBuilder::resolve_scaled_metrics(&style, &builder.font_ctx);
    let viewport = builder.viewport.map(|(w, h)| Size::new(w, h));
    let line_height = compute_line_height_with_metrics_viewport(
      &style,
      metrics_scaled.as_ref(),
      viewport,
      builder.font_ctx.root_font_metrics(),
    );
    let metrics =
      InlineTextItem::metrics_from_runs(&builder.font_ctx, &runs, line_height, style.font_size);
    let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
    assert!(
      half_leading > 0.5,
      "expected non-zero half-leading for regression coverage"
    );
    let expected_baseline = rect.y() + metrics.baseline_offset;

    for item in items {
      assert!(
        (item.origin.y - expected_baseline).abs() < 1e-3,
        "expected baseline to be rect.y() + metrics.baseline_offset; got {} expected {} (half-leading {})",
        item.origin.y,
        expected_baseline,
        half_leading
      );
    }
  }

  #[test]
  fn focused_text_control_does_not_paint_internal_tint_overlay() {
    // Regression test: native control painting used to apply a semi-transparent "state tint" fill
    // for focused/focus-visible controls. This is not driven by CSS and should not be painted as an
    // extra overlay.
    let mut builder = DisplayListBuilder::new();
    let style = Arc::new(ComputedStyle::default());
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
      disabled: false,
      focused: true,
      focus_visible: true,
      required: false,
      invalid: false,
      ime_preedit: None,
    };

    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 20.0);
    let mut fragment = FragmentNode::new_replaced(bounds, ReplacedType::FormControl(control.clone()));
    fragment.style = Some(style);

    builder.emit_form_control(&control, &fragment, bounds, None);

    assert!(
      !builder.list.items().iter().any(|item| match item {
        DisplayItem::FillRoundedRect(fill) => fill.color.a > 0.0 && fill.color.a < 1.0,
        _ => false,
      }),
      "focused text controls should not paint internal semi-transparent tint overlays"
    );
  }

  #[test]
  fn resolve_length_for_paint_resolves_rem_against_root_font_size() {
    let len = Length::rem(1.0);
    let resolved = DisplayListBuilder::resolve_length_for_paint(&len, 10.0, 20.0, 0.0, None);
    assert!((resolved - 20.0).abs() < 1e-6);
  }

  #[test]
  fn resolve_length_for_paint_does_not_fallback_to_element_font_size_for_rem() {
    let len = Length::rem(1.0);
    let resolved = DisplayListBuilder::resolve_length_for_paint(&len, 10.0, f32::NAN, 0.0, None);
    assert_eq!(resolved, 0.0);
  }

  #[test]
  fn resolve_length_for_paint_resolves_viewport_units_against_viewport() {
    let len = Length::new(10.0, LengthUnit::Vw);
    let resolved =
      DisplayListBuilder::resolve_length_for_paint(&len, 10.0, 10.0, 0.0, Some((200.0, 100.0)));
    assert!((resolved - 20.0).abs() < 1e-6);
  }

  #[test]
  fn resolve_length_for_paint_viewport_units_require_viewport() {
    let len = Length::new(10.0, LengthUnit::Vw);
    let resolved = DisplayListBuilder::resolve_length_for_paint(&len, 10.0, 10.0, 0.0, None);
    assert_eq!(resolved, 0.0);
  }

  #[test]
  fn clip_rect_from_style_offsets_are_relative_to_bounds_origin() {
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.clip = Some(ClipRect {
      top: ClipComponent::Length(Length::px(10.0)),
      right: ClipComponent::Length(Length::px(60.0)),
      bottom: ClipComponent::Length(Length::px(70.0)),
      left: ClipComponent::Length(Length::px(20.0)),
    });

    let bounds = Rect::from_xywh(50.0, 40.0, 100.0, 80.0);
    let clip = DisplayListBuilder::clip_rect_from_style(&style, bounds, None).expect("clip rect");
    let rect = match clip.shape {
      ClipShape::Rect { rect, .. } => rect,
      other => panic!("expected rect clip, got {other:?}"),
    };

    assert!((rect.x() - 70.0).abs() < 1e-6);
    assert!((rect.y() - 50.0).abs() < 1e-6);
    assert!((rect.width() - 40.0).abs() < 1e-6);
    assert!((rect.height() - 60.0).abs() < 1e-6);
  }

  #[test]
  fn clip_rect_from_style_allows_empty_rects() {
    // Sites often use `clip` + `position: absolute` to implement visually-hidden accessibility
    // text. A degenerate clip rect should behave like a clip-to-empty (paint nothing), not as
    // `clip: auto`.
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.clip = Some(ClipRect {
      top: ClipComponent::Length(Length::px(1.0)),
      right: ClipComponent::Length(Length::px(1.0)),
      bottom: ClipComponent::Length(Length::px(1.0)),
      left: ClipComponent::Length(Length::px(1.0)),
    });

    let bounds = Rect::from_xywh(50.0, 40.0, 100.0, 80.0);
    let clip = DisplayListBuilder::clip_rect_from_style(&style, bounds, None)
      .expect("expected empty clip rect to produce a clip item");
    let rect = match clip.shape {
      ClipShape::Rect { rect, .. } => rect,
      other => panic!("expected rect clip, got {other:?}"),
    };

    assert!((rect.x() - 51.0).abs() < 1e-6);
    assert!((rect.y() - 41.0).abs() < 1e-6);
    assert!((rect.width() - 0.0).abs() < 1e-6);
    assert!((rect.height() - 0.0).abs() < 1e-6);
  }

  #[test]
  fn clip_rect_from_style_resolves_rem_against_root_font_size() {
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.font_size = 10.0;
    style.root_font_size = 20.0;
    style.clip = Some(ClipRect {
      top: ClipComponent::Length(Length::px(0.0)),
      right: ClipComponent::Auto,
      bottom: ClipComponent::Auto,
      left: ClipComponent::Length(Length::rem(1.0)),
    });

    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 80.0);
    let clip = DisplayListBuilder::clip_rect_from_style(&style, bounds, None).expect("clip rect");
    let rect = match clip.shape {
      ClipShape::Rect { rect, .. } => rect,
      other => panic!("expected rect clip, got {other:?}"),
    };

    assert!((rect.x() - 20.0).abs() < 1e-6);
  }

  #[test]
  fn clip_rect_from_style_viewport_units_require_viewport() {
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.clip = Some(ClipRect {
      top: ClipComponent::Length(Length::px(0.0)),
      right: ClipComponent::Auto,
      bottom: ClipComponent::Auto,
      left: ClipComponent::Length(Length::new(10.0, LengthUnit::Vw)),
    });

    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 100.0);
    let clip = DisplayListBuilder::clip_rect_from_style(&style, bounds, None).expect("clip rect");
    let rect = match clip.shape {
      ClipShape::Rect { rect, .. } => rect,
      other => panic!("expected rect clip, got {other:?}"),
    };
    assert!((rect.x() - 0.0).abs() < 1e-6);

    let clip =
      DisplayListBuilder::clip_rect_from_style(&style, bounds, Some((200.0, 100.0))).expect("clip");
    let rect = match clip.shape {
      ClipShape::Rect { rect, .. } => rect,
      other => panic!("expected rect clip, got {other:?}"),
    };
    assert!((rect.x() - 20.0).abs() < 1e-6);
  }

  #[test]
  fn background_size_viewport_units_require_viewport() {
    let mut layer = BackgroundLayer::default();
    layer.size = BackgroundSize::Explicit(
      BackgroundSizeComponent::Length(Length::new(10.0, LengthUnit::Vw)),
      BackgroundSizeComponent::Length(Length::new(10.0, LengthUnit::Vh)),
    );

    let (w, h) = DisplayListBuilder::compute_background_size(
      &layer, 10.0, 10.0, None, 100.0, 100.0, 0.0, 0.0, None,
    );
    assert_eq!(w, 0.0);
    assert_eq!(h, 0.0);

    let (w, h) = DisplayListBuilder::compute_background_size(
      &layer,
      10.0,
      10.0,
      Some((200.0, 100.0)),
      100.0,
      100.0,
      0.0,
      0.0,
      None,
    );
    assert!((w - 20.0).abs() < 1e-6);
    assert!((h - 10.0).abs() < 1e-6);
  }

  #[test]
  fn background_size_auto_without_intrinsic_dimensions_uses_contain_when_ratio_present() {
    let layer = BackgroundLayer::default();
    // No intrinsic dimensions (img_w/img_h=0) but an intrinsic ratio. `auto auto` sizing should act
    // like `contain` and preserve the ratio within the positioning area.
    let (w, h) = DisplayListBuilder::compute_background_size(
      &layer,
      16.0,
      16.0,
      None,
      200.0,
      100.0,
      0.0,
      0.0,
      Some(1.0),
    );
    assert!((w - 100.0).abs() < 1e-6);
    assert!((h - 100.0).abs() < 1e-6);
  }

  #[test]
  fn text_underline_offset_is_relative_to_baseline_for_auto_position() {
    let builder = DisplayListBuilder::new();
    let metrics = DecorationMetrics {
      underline_pos: 0.0,
      underline_thickness: 1.0,
      strike_pos: 0.0,
      strike_thickness: 1.0,
      ascent: 0.0,
      descent: 0.0,
      has_font_underline_metrics: false,
    };

    let style = ComputedStyle::default();
    let baseline = 12.0;
    let thickness = 10.0;

    let center = builder.underline_center(
      &metrics,
      TextUnderlinePosition::Auto,
      TextUnderlineOffset::Length(Length::px(0.0)),
      thickness,
      baseline,
      false,
      &style,
    );
    assert!((center - (baseline + thickness * 0.5)).abs() < 0.01);

    let center = builder.underline_center(
      &metrics,
      TextUnderlinePosition::Auto,
      TextUnderlineOffset::Length(Length::px(10.0)),
      thickness,
      baseline,
      false,
      &style,
    );
    assert!((center - (baseline + thickness * 0.5 + 10.0)).abs() < 0.01);
  }

  #[test]
  fn text_emphasis_fill_only_defaults_shape_in_sideways_writing_mode() {
    let builder = DisplayListBuilder::new();
    let font = test_font();
    let cached_face = face_cache::get_ttf_face(font.as_ref()).expect("parse test font");
    let face = cached_face.face();
    let ch = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test font to contain at least one ASCII glyph");
    let run = shaped_run_for_char(Arc::clone(&font), ch, 16.0);

    let mut style = ComputedStyle::default();
    style.font_size = 16.0;
    style.writing_mode = crate::style::types::WritingMode::SidewaysRl;
    style.text_emphasis_style = TextEmphasisStyle::Mark {
      fill: crate::style::types::TextEmphasisFill::Open,
      shape: None,
    };

    let emphasis = builder
      .build_emphasis(&run, &style, 0.0, 0.0, true, TextEmphasisOffset::default())
      .expect("emphasis");

    assert!(matches!(
      emphasis.style,
      TextEmphasisStyle::Mark {
        fill: crate::style::types::TextEmphasisFill::Open,
        shape: Some(crate::style::types::TextEmphasisShape::Circle)
      }
    ));
  }

  #[test]
  fn text_emphasis_skip_skips_punctuation_by_default() {
    let builder = DisplayListBuilder::new();
    let font = test_font();
    let cached_face = face_cache::get_ttf_face(font.as_ref()).expect("parse test font");
    let face = cached_face.face();
    // The subset font fixture doesn't necessarily include punctuation glyphs. For mark skipping we
    // only need a cluster -> text mapping, not a specific glyph id, so synthesize a run using any
    // glyph from the font while providing punctuation text.
    let ch = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test font to contain at least one ASCII glyph");
    let mut run = shaped_run_for_char(Arc::clone(&font), ch, 16.0);
    run.text = ",".to_string();
    run.end = run.text.len();

    let mut style = ComputedStyle::default();
    style.font_size = 16.0;
    style.text_emphasis_style = TextEmphasisStyle::Mark {
      fill: crate::style::types::TextEmphasisFill::Filled,
      shape: Some(crate::style::types::TextEmphasisShape::Dot),
    };

    let emphasis = builder
      .build_emphasis(&run, &style, 0.0, 0.0, false, TextEmphasisOffset::default())
      .expect("emphasis");
    assert!(
      emphasis.marks.is_empty(),
      "expected punctuation to be skipped by default (initial text-emphasis-skip: spaces punctuation)"
    );

    style.text_emphasis_skip = TextEmphasisSkip::SPACES;
    let emphasis = builder
      .build_emphasis(&run, &style, 0.0, 0.0, false, TextEmphasisOffset::default())
      .expect("emphasis");
    assert_eq!(emphasis.marks.len(), 1);
  }

  #[test]
  fn image_key_canonicalizes_negative_zero() {
    let key = ImageKey::new(
      "https://example.com/image.png".to_string(),
      CrossOriginAttribute::None,
      None,
      OrientationTransform::IDENTITY,
      false,
      0.0,
      0.0,
    );
    let key_neg = ImageKey::new(
      "https://example.com/image.png".to_string(),
      CrossOriginAttribute::None,
      None,
      OrientationTransform::IDENTITY,
      false,
      -0.0,
      -0.0,
    );
    assert_eq!(key, key_neg);
  }

  fn data_url_for_color(color: [u8; 4]) -> String {
    data_url_for_solid_png(1, 1, color)
  }

  fn data_url_for_solid_png(width: u32, height: u32, color: [u8; 4]) -> String {
    let mut buf = Vec::new();
    let len = width as usize * height as usize * 4;
    let mut pixels = vec![0u8; len];
    for chunk in pixels.chunks_exact_mut(4) {
      chunk.copy_from_slice(&color);
    }
    PngEncoder::new(&mut buf)
      .write_image(&pixels, width, height, ColorType::Rgba8.into())
      .expect("encode png");
    format!(
      "data:image/png;base64,{}",
      general_purpose::STANDARD.encode(buf)
    )
  }

  fn data_url_for_solid_rgba(width: u32, height: u32, color: [u8; 4]) -> String {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..(width * height) {
      pixels.extend_from_slice(&color);
    }
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
      .write_image(&pixels, width, height, ColorType::Rgba8.into())
      .expect("encode png");
    format!(
      "data:image/png;base64,{}",
      general_purpose::STANDARD.encode(buf)
    )
  }

  fn test_font() -> Arc<crate::text::font_db::LoadedFont> {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    Arc::new(crate::text::font_db::LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: crate::text::font_db::FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    })
  }

  fn test_cjk_font() -> Arc<crate::text::font_db::LoadedFont> {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/NotoSansSC-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test CJK font"));
    Arc::new(crate::text::font_db::LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "Noto Sans SC Subset".to_string(),
      weight: crate::text::font_db::FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    })
  }

  fn shaped_run_for_char(
    font: Arc<crate::text::font_db::LoadedFont>,
    ch: char,
    font_size: f32,
  ) -> ShapedRun {
    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let glyph_id = face
      .glyph_index(ch)
      .unwrap_or_else(|| panic!("expected glyph for {ch}"))
      .0 as u32;
    let text = ch.to_string();
    let end = text.len();
    let advance = font_size * 0.6;
    ShapedRun {
      text,
      start: 0,
      end,
      glyphs: vec![crate::text::pipeline::GlyphPosition {
        glyph_id,
        cluster: 0,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: advance,
        y_advance: 0.0,
      }],
      direction: crate::text::pipeline::Direction::LeftToRight,
      level: 0,
      advance,
      font,
      font_size,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: crate::text::pipeline::RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    }
  }

  #[test]
  fn width_descriptor_srcset_selection_uses_sizes_not_layout_slot_width() {
    // Regression test: the display-list backend should pick width-descriptor srcset candidates based
    // on the evaluated `sizes` attribute (or 100vw), not the final layout width of the `<img>`
    // fragment.
    //
    // This matches Chrome and avoids selecting placeholder/missing candidates in pageset fixtures
    // (e.g. bbc.com) where only the Chrome-chosen resource was captured.
    let small = data_url_for_solid_rgba(1, 1, [255, 0, 0, 255]);
    let large = data_url_for_solid_rgba(2, 2, [0, 0, 255, 255]);

    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 300.0, 150.0),
      ReplacedType::Image {
        // Base `src` shouldn't be consulted for width-descriptor srcset selection when `srcset`
        // provides usable candidates.
        src: small.clone(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: Some(crate::tree::box_tree::SizesList {
          entries: vec![crate::tree::box_tree::SizesEntry {
            media: None,
            length: Length::px(600.0).into(),
          }],
        }),
        srcset: vec![
          crate::tree::box_tree::SrcsetCandidate {
            url: small,
            descriptor: crate::tree::box_tree::SrcsetDescriptor::Width(300),
          },
          crate::tree::box_tree::SrcsetCandidate {
            url: large,
            descriptor: crate::tree::box_tree::SrcsetDescriptor::Width(600),
          },
        ],
        picture_sources: Vec::new(),
      },
    );

    let list = DisplayListBuilder::with_image_cache(ImageCache::new())
      .with_viewport_size(800.0, 600.0)
      .build(&fragment);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("expected image display item");

    assert_eq!(
      img.image.width, 2,
      "expected the 600w srcset candidate to be selected"
    );
    assert_eq!(img.image.height, 2);
  }

  fn shaped_run_for_char_with_direction(
    font: Arc<crate::text::font_db::LoadedFont>,
    ch: char,
    font_size: f32,
    direction: crate::text::pipeline::Direction,
    level: u8,
  ) -> ShapedRun {
    let mut run = shaped_run_for_char(font, ch, font_size);
    run.direction = direction;
    run.level = level;
    run
  }

  #[test]
  fn background_clip_text_emits_text_clip_item() {
    let font = test_font();
    let cached_face = face_cache::get_ttf_face(font.as_ref()).expect("parse test font");
    let face = cached_face.face();
    let ch = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test font to contain at least one ASCII glyph");
    let run = shaped_run_for_char(Arc::clone(&font), ch, 16.0);
    let runs: Arc<Vec<ShapedRun>> = Arc::new(vec![run]);

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let text_contents = ch.to_string();
    let text = FragmentNode::new_text_shaped(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      text_contents,
      14.0,
      runs,
      Arc::clone(&text_style),
    );

    let mut bg_style = ComputedStyle::default();
    bg_style.background_color = Rgba::rgb(255, 0, 0);
    let mut layer = BackgroundLayer::default();
    layer.clip = BackgroundBox::Text;
    bg_style.background_layers = vec![layer].into();

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 200.0, 60.0),
      vec![text],
      Arc::new(bg_style),
    );

    let builder = DisplayListBuilder::new();
    let tree = FragmentTree::new(fragment);
    let list = builder.build_tree(&tree);

    let items = list.items();
    let mut found = false;
    for idx in 0..items.len().saturating_sub(2) {
      let DisplayItem::PushClip(clip) = &items[idx] else {
        continue;
      };
      let ClipShape::Text { runs } = &clip.shape else {
        continue;
      };
      if runs.is_empty() {
        continue;
      }
      let DisplayItem::FillRect(fill) = &items[idx + 1] else {
        continue;
      };
      if fill.color != Rgba::rgb(255, 0, 0) {
        continue;
      }
      if !matches!(items[idx + 2], DisplayItem::PopClip) {
        continue;
      }
      found = true;
      break;
    }

    assert!(
      found,
      "expected background-clip:text to push a text clip around background-color paints"
    );
  }

  #[test]
  fn background_clip_text_rtl_clip_origin_matches_text_origin() {
    let font = test_font();
    let cached_face = face_cache::get_ttf_face(font.as_ref()).expect("parse test font");
    let face = cached_face.face();
    let ch = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test font to contain at least one ASCII glyph");

    let run = shaped_run_for_char_with_direction(
      Arc::clone(&font),
      ch,
      16.0,
      crate::text::pipeline::Direction::RightToLeft,
      1,
    );
    assert!(
      run.direction.is_rtl() && run.advance > 0.0,
      "expected an RTL run with positive advance for regression coverage"
    );
    let runs: Arc<Vec<ShapedRun>> = Arc::new(vec![run]);

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let text_contents = ch.to_string();
    let text = FragmentNode::new_text_shaped(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      text_contents,
      14.0,
      runs,
      Arc::clone(&text_style),
    );

    let mut bg_style = ComputedStyle::default();
    bg_style.background_color = Rgba::rgb(255, 0, 0);
    let mut layer = BackgroundLayer::default();
    layer.clip = BackgroundBox::Text;
    bg_style.background_layers = vec![layer].into();

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 200.0, 60.0),
      vec![text],
      Arc::new(bg_style),
    );

    let builder = DisplayListBuilder::new();
    let tree = FragmentTree::new(fragment);
    let list = builder.build_tree(&tree);

    let items = list.items();
    let text_origin_x = items
      .iter()
      .find_map(|item| match item {
        DisplayItem::Text(text) => Some(text.origin.x),
        _ => None,
      })
      .expect("expected a Text display item");

    let mut clip_origin_x: Option<f32> = None;
    for idx in 0..items.len().saturating_sub(2) {
      let DisplayItem::PushClip(clip) = &items[idx] else {
        continue;
      };
      let ClipShape::Text { runs } = &clip.shape else {
        continue;
      };
      if runs.is_empty() {
        continue;
      }
      let DisplayItem::FillRect(fill) = &items[idx + 1] else {
        continue;
      };
      if fill.color != Rgba::rgb(255, 0, 0) {
        continue;
      }
      if !matches!(items[idx + 2], DisplayItem::PopClip) {
        continue;
      }
      clip_origin_x = Some(runs[0].origin.x);
      break;
    }

    let clip_origin_x = clip_origin_x.expect("expected background-clip:text to emit a text clip");
    assert!(
      (clip_origin_x - text_origin_x).abs() < 1e-6,
      "expected background-clip:text clip origin.x ({clip_origin_x}) to match normal text origin.x ({text_origin_x}) for RTL runs"
    );
  }

  #[test]
  fn webkit_text_fill_color_overrides_text_item_color() {
    let font = test_font();
    let cached_face = face_cache::get_ttf_face(font.as_ref()).expect("parse test font");
    let face = cached_face.face();
    let ch = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test font to contain at least one ASCII glyph");
    let run = shaped_run_for_char(Arc::clone(&font), ch, 16.0);
    let runs: Arc<Vec<ShapedRun>> = Arc::new(vec![run]);

    let mut style = ComputedStyle::default();
    style.font_size = 16.0;
    style.color = Rgba::rgb(255, 0, 0);
    style.webkit_text_fill_color = Color::Rgba(Rgba::rgb(0, 0, 255));
    let style = Arc::new(style);

    let text_contents = ch.to_string();
    let text = FragmentNode::new_text_shaped(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      text_contents,
      14.0,
      runs,
      Arc::clone(&style),
    );

    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 60.0), vec![text]);

    let builder = DisplayListBuilder::new();
    let tree = FragmentTree::new(fragment);
    let list = builder.build_tree(&tree);

    let painted = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Text(text) => Some(text),
        _ => None,
      })
      .expect("expected a Text display item");

    assert_eq!(
      painted.color,
      Rgba::rgb(0, 0, 255),
      "expected -webkit-text-fill-color to override the glyph fill color"
    );
  }

  #[test]
  fn text_decoration_thickness_auto_resolves_to_ua_default() {
    let builder = DisplayListBuilder::new();
    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    let auto_small = builder
      .resolve_text_decoration_thickness_override(TextDecorationThickness::Auto, &style)
      .unwrap();
    assert!((auto_small - 1.0).abs() < 0.0001);

    style.font_size = 200.0;
    let auto_large = builder
      .resolve_text_decoration_thickness_override(TextDecorationThickness::Auto, &style)
      .unwrap();
    assert!((auto_large - 20.0).abs() < 0.0001);

    assert!(builder
      .resolve_text_decoration_thickness_override(TextDecorationThickness::FromFont, &style)
      .is_none());
  }

  #[test]
  fn underline_skip_ink_does_not_carve_ideographic_runs() {
    let font = test_cjk_font();
    let cached_face = face_cache::get_ttf_face(font.as_ref()).expect("parse test CJK font");
    let face = cached_face.face();
    let ch = ['中', '你', '好', '国', '流', '浪', '地', '球']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test CJK font to contain at least one common CJK glyph");
    let run = shaped_run_for_char(Arc::clone(&font), ch, 12.0);
    let runs: Arc<Vec<ShapedRun>> = Arc::new(vec![run]);

    let mut style = ComputedStyle::default();
    style.font_size = 12.0;
    style.text_decoration.lines = TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    style.text_decoration_skip_ink = TextDecorationSkipInk::Auto;
    let style = Arc::new(style);

    let text_contents = ch.to_string();
    let text_width = runs[0].advance;
    let text = FragmentNode::new_text_shaped(
      Rect::from_xywh(0.0, 0.0, text_width, 20.0),
      text_contents,
      12.0,
      runs,
      Arc::clone(&style),
    );
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 60.0), vec![text]);

    let list = DisplayListBuilder::new().build(&fragment);
    let deco = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::TextDecoration(dec) => Some(dec),
        _ => None,
      })
      .expect("expected a TextDecoration display item");
    let underline = deco
      .decorations
      .first()
      .and_then(|d| d.underline.as_ref())
      .expect("expected underline decoration");
    let segments = underline
      .segments
      .as_ref()
      .expect("skip-ink should produce underline segments");
    assert_eq!(
      segments.len(),
      1,
      "expected ideographic underline to avoid skip-ink carving"
    );
    let (start, end) = segments[0];
    assert!(start.abs() < 0.001);
    assert!((end - deco.line_width).abs() < 0.001);
  }

  fn create_image_fragment(x: f32, y: f32, width: f32, height: f32, src: &str) -> FragmentNode {
    FragmentNode::new_replaced(
      Rect::from_xywh(x, y, width, height),
      ReplacedType::Image {
        src: src.to_string(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
    )
  }

  fn text_fragment_at(x: f32, label: &str) -> FragmentNode {
    FragmentNode::new_text(Rect::from_xywh(x, 0.0, 10.0, 10.0), label.to_string(), 12.0)
  }

  fn stacking_clip_order(items: &[DisplayItem]) -> (usize, usize, usize, usize) {
    let push_sc = items
      .iter()
      .position(|item| matches!(item, DisplayItem::PushStackingContext(_)))
      .expect("stacking context push missing");
    let push_clip = items
      .iter()
      .position(|item| matches!(item, DisplayItem::PushClip(_)))
      .expect("clip push missing");
    let pop_clip = items
      .iter()
      .rposition(|item| matches!(item, DisplayItem::PopClip))
      .expect("clip pop missing");
    let pop_sc = items
      .iter()
      .rposition(|item| matches!(item, DisplayItem::PopStackingContext))
      .expect("stacking context pop missing");

    assert!(
      push_sc < push_clip,
      "clip should be emitted inside the stacking context"
    );
    assert!(push_clip < pop_clip, "clip should wrap painted items");
    assert!(
      pop_clip < pop_sc,
      "stacking context should be popped after its clips"
    );

    (push_sc, push_clip, pop_clip, pop_sc)
  }

  fn has_paint_between(items: &[DisplayItem], start: usize, end: usize) -> bool {
    items.iter().enumerate().any(|(idx, item)| {
      idx > start
        && idx < end
        && !matches!(
          item,
          DisplayItem::PushClip(_)
            | DisplayItem::PopClip
            | DisplayItem::PushStackingContext(_)
            | DisplayItem::PopStackingContext
        )
    })
  }

  #[test]
  fn will_change_filter_establishes_backdrop_root() {
    let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    child_style.will_change = WillChange::Hints(vec![WillChangeHint::Property("filter".into())]);
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
    let root =
      FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });

    let child_context = child_context.expect("expected a stacking context for will-change:filter");
    assert!(
      child_context.establishes_backdrop_root,
      "will-change: filter should establish a backdrop root even if no filter functions are present"
    );
    assert!(child_context.filters.is_empty());
    assert!(child_context.backdrop_filters.is_empty());
    assert!(child_context.mask.is_none());
    assert_eq!(child_context.mix_blend_mode, BlendMode::Normal);
    assert!((child_context.opacity - 1.0).abs() <= f32::EPSILON);
  }

  #[test]
  fn filter_establishes_backdrop_root_even_when_unresolved() {
    let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

    let mut child_style = ComputedStyle::default();
    // Filter Effects Level 2 defines Backdrop Root triggers based on property presence rather than
    // resolved filter lists. An unresolved `url(#missing)` filter must still establish a Backdrop
    // Root boundary for descendant `backdrop-filter` sampling.
    child_style.filter = vec![crate::style::types::FilterFunction::Url("#missing".into())];
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
    let root =
      FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });
    let child_context = child_context.expect("expected a stacking context for filter:url()");

    assert!(
      child_context.establishes_backdrop_root,
      "filter:url() should establish a backdrop root even when the URL cannot be resolved"
    );
    assert!(
      child_context.filters.is_empty(),
      "unresolved filter:url() should not resolve to any paint-time filters"
    );
  }

  #[test]
  fn backdrop_filter_establishes_backdrop_root_even_when_unresolved() {
    let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

    let mut child_style = ComputedStyle::default();
    child_style.backdrop_filter = vec![crate::style::types::FilterFunction::Url("#missing".into())];
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
    let root =
      FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });
    let child_context =
      child_context.expect("expected a stacking context for backdrop-filter:url()");

    assert!(
      child_context.establishes_backdrop_root,
      "backdrop-filter:url() should establish a backdrop root even when the URL cannot be resolved"
    );
    assert!(
      child_context.backdrop_filters.is_empty(),
      "unresolved backdrop-filter:url() should not resolve to any paint-time filters"
    );
  }

  #[test]
  fn mask_image_establishes_backdrop_root_even_when_unresolved() {
    let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

    let mut child_style = ComputedStyle::default();
    let mut layer = crate::style::types::MaskLayer::default();
    layer.image = Some(BackgroundImage::Url(BackgroundImageUrl::new("#missing")));
    child_style.set_mask_layers(vec![layer]);
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
    let root =
      FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    // Without an image cache (and without an in-document SVG id definition), the mask-image cannot
    // be resolved for painting. Even so, the element still establishes a Backdrop Root boundary
    // because the trigger is based on property presence (filter-effects-2), not resolved masks.
    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });
    let child_context = child_context.expect("expected a stacking context for mask-image:url()");

    assert!(
      child_context.establishes_backdrop_root,
      "mask-image:url(#missing) should establish a backdrop root even when the image cannot be resolved"
    );
    assert!(
      child_context.mask.is_none(),
      "unresolved mask-image:url(#missing) should not resolve to any paint-time mask"
    );
  }

  #[test]
  fn clip_path_establishes_backdrop_root_even_when_resolved_none() {
    let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

    let mut child_style = ComputedStyle::default();
    // Degenerate polygon => `resolve_clip_path` returns `None`, but the computed style is still
    // non-`none` and must trigger Backdrop Root semantics.
    child_style.clip_path = ClipPath::BasicShape(
      Box::new(BasicShape::Polygon {
        fill: crate::style::types::FillRule::NonZero,
        points: vec![
          (Length::px(0.0), Length::px(0.0)),
          (Length::px(1.0), Length::px(0.0)),
        ],
      }),
      None,
    );
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
    let root =
      FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });
    let child_context = child_context.expect("expected a stacking context for clip-path");

    assert!(
      child_context.has_clip_path,
      "non-`none` clip-path should set StackingContextItem.has_clip_path even when it resolves to None"
    );
    assert!(
      child_context.establishes_backdrop_root,
      "non-`none` clip-path should establish a backdrop root even when it resolves to None"
    );

    // Ensure we didn't emit a clip-path clip item, confirming the degenerate polygon resolved to
    // `None` for painting.
    assert!(
      !list.items().iter().any(|item| matches!(
        item,
        DisplayItem::PushClip(ClipItem {
          shape: ClipShape::Path { .. }
        })
      )),
      "expected degenerate clip-path to omit PushClip(Path) emission"
    );
  }

  #[test]
  fn clip_path_url_establishes_backdrop_root_even_when_unresolved() {
    let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

    let mut child_style = ComputedStyle::default();
    child_style.clip_path = ClipPath::Url("#missing".into(), None);
    let child_style = Arc::new(child_style);

    let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
    let root =
      FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    // Without an image cache (and without an in-document SVG id definition), the clip-path cannot
    // be resolved for painting. Even so, the element still establishes a Backdrop Root boundary
    // because the trigger is based on property presence (filter-effects-2), not resolved clips.
    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });
    let child_context = child_context.expect("expected a stacking context for clip-path:url()");

    assert!(
      child_context.has_clip_path,
      "clip-path:url() should set StackingContextItem.has_clip_path even when the URL cannot be resolved"
    );
    assert!(
      child_context.establishes_backdrop_root,
      "clip-path:url() should establish a backdrop root even when the URL cannot be resolved"
    );
    assert!(
      child_context.mask.is_none(),
      "unresolved clip-path:url(#missing) should not resolve to any paint-time mask"
    );

    assert!(
      !list.items().iter().any(|item| matches!(
        item,
        DisplayItem::PushClip(ClipItem {
          shape: ClipShape::Path { .. }
        })
      )),
      "expected url clip-path to omit PushClip(Path) emission"
    );
  }

  #[test]
  fn paint_build_skips_clip_radii_when_border_radius_is_zero() {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      ("FASTR_PAINT_BUILD_BREAKDOWN".to_string(), "1".to_string()),
      ("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "0".to_string()),
    ])));

    runtime::with_thread_runtime_toggles(toggles, || {
      enable_paint_diagnostics();

      let mut style = ComputedStyle::default();
      style.background_color = Rgba::new(16, 32, 64, 1.0);
      style.overflow_x = Overflow::Hidden;
      style.overflow_y = Overflow::Hidden;
      let style = Arc::new(style);

      let depth = 64;
      let rect = Rect::from_xywh(0.0, 0.0, 200.0, 200.0);
      let mut tree = FragmentNode::new_block_styled(rect, vec![], style.clone());
      for _ in 0..depth {
        tree = FragmentNode::new_block_styled(rect, vec![tree], style.clone());
      }

      let list = DisplayListBuilder::new()
        .with_viewport_size(rect.width(), rect.height())
        .build_with_stacking_tree_checked(&tree)
        .expect("display list should build");

      let diagnostics =
        crate::paint::painter::take_paint_diagnostics().expect("paint diagnostics enabled");

      let stacking_contexts = list
        .items()
        .iter()
        .filter(|item| matches!(item, DisplayItem::PushStackingContext(_)))
        .count() as u64;
      assert_eq!(
        diagnostics.build_border_radii_calls, stacking_contexts,
        "border radii work should only run for stacking contexts when border-radius is zero"
      );

      for item in list.items() {
        match item {
          DisplayItem::FillRoundedRect(_) | DisplayItem::StrokeRoundedRect(_) => {
            panic!("unexpected rounded-rect paint when border-radius is zero");
          }
          DisplayItem::PushClip(clip) => {
            if let ClipShape::Rect { radii, .. } = &clip.shape {
              assert!(
                radii.is_none(),
                "border-radius is zero so rect clips should not carry radii"
              );
            }
          }
          _ => {}
        }
      }
    });
  }

  #[test]
  fn test_builder_empty_fragment() {
    let fragment = create_block_fragment(0.0, 0.0, 100.0, 100.0);
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(list.is_empty());
  }

  #[test]
  fn builder_paints_floats_above_following_block_backgrounds() {
    // Floats should be painted in CSS2 stacking layer 4 (after in-flow blocks).
    // This matters even when the float comes earlier in tree order, because later
    // in-flow block backgrounds must not cover it.
    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 200.0);

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = crate::style::float::Float::Left;
    float_style.background_color = Rgba::RED;
    let float_style = Arc::new(float_style);
    let float_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 120.0, 120.0),
      vec![],
      float_style,
    );

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    block_style.background_color = Rgba::GREEN;
    let block_style = Arc::new(block_style);
    let block_fragment = FragmentNode::new_block_styled(bounds, vec![], block_style);

    // Use a non-stacking-context container so the paint order comes from
    // `build_fragment_internal` child ordering (not from the stacking tree layers).
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Block;
    let container_style = Arc::new(container_style);
    let container = FragmentNode::new_block_styled(
      bounds,
      vec![float_fragment, block_fragment],
      container_style,
    );

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let root = FragmentNode::new_block_styled(bounds, vec![container], Arc::new(root_style));

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let mut red_idx = None;
    let mut green_idx = None;
    for (idx, item) in list.items().iter().enumerate() {
      let color = match item {
        DisplayItem::FillRect(fill) => Some(fill.color),
        DisplayItem::FillRoundedRect(fill) => Some(fill.color),
        _ => None,
      };
      match color {
        Some(c) if c == Rgba::RED && red_idx.is_none() => red_idx = Some(idx),
        Some(c) if c == Rgba::GREEN && green_idx.is_none() => green_idx = Some(idx),
        _ => {}
      }
    }

    let red_idx = red_idx.expect("expected a red background fill for the float");
    let green_idx = green_idx.expect("expected a green background fill for the block");
    assert!(
      green_idx < red_idx,
      "expected in-flow block backgrounds to be painted before floats"
    );
  }

  #[test]
  fn test_builder_text_fragment() {
    let fragment = create_text_fragment(10.0, 20.0, 100.0, 20.0, "Hello");
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert_eq!(list.len(), 1);
    assert!(matches!(list.items()[0], DisplayItem::Text(_)));
  }

  #[test]
  fn builder_populates_text_cached_bounds() {
    let mut db = FontDatabase::empty();
    let font_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fonts");
    db.load_fonts_dir(font_dir);
    db.refresh_generic_fallbacks();
    let font_ctx = FontContext::with_database(Arc::new(db));

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(10.0, 20.0, 100.0, 20.0),
      "Hello".to_string(),
      12.0,
      Arc::new(ComputedStyle::default()),
    );
    let mut builder = DisplayListBuilder::new();
    builder.font_ctx = font_ctx;
    let list = builder.build(&fragment);

    let text_item = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Text(text) => Some(text),
        _ => None,
      })
      .expect("display list should include a Text item");

    let cached = text_item
      .cached_bounds
      .expect("TextItem.cached_bounds should be populated during build");
    let mut uncached = text_item.clone();
    uncached.cached_bounds = None;
    let expected = text_bounds(&uncached);
    assert_eq!(cached, expected);
  }

  #[test]
  fn test_builder_text_position() {
    let fragment = create_text_fragment(10.0, 20.0, 100.0, 20.0, "Hello");
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    if let DisplayItem::Text(text) = &list.items()[0] {
      assert_eq!(text.origin.x, 10.0);
      assert_eq!(text.glyphs.len(), 5);
    } else {
      panic!("Expected Text item");
    }
  }

  #[test]
  fn parallel_builder_matches_sequential_output() {
    let child1 = create_text_fragment(0.0, 0.0, 20.0, 10.0, "One");
    let child2 = create_text_fragment(0.0, 10.0, 20.0, 10.0, "Two");
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![child1, child2]);

    let parallel_toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      ("FASTR_DISPLAY_LIST_PARALLEL_MIN".to_string(), "1".to_string()),
      ("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "1".to_string()),
    ])));
    let parallel = runtime::with_runtime_toggles(parallel_toggles, || {
      DisplayListBuilder::new().build(&root)
    });

    let sequential_toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_DISPLAY_LIST_PARALLEL".to_string(),
      "0".to_string(),
    )])));
    let sequential = runtime::with_runtime_toggles(sequential_toggles, || {
      DisplayListBuilder::new().build(&root)
    });

    assert_eq!(parallel.len(), sequential.len());
    let parallel_debug: Vec<String> = parallel
      .items()
      .iter()
      .map(|item| format!("{:?}", item))
      .collect();
    let sequential_debug: Vec<String> = sequential
      .items()
      .iter()
      .map(|item| format!("{:?}", item))
      .collect();
    assert_eq!(parallel_debug, sequential_debug);
  }

  #[test]
  fn builder_emits_text_decorations() {
    let mut style = ComputedStyle::default();
    style.text_decoration.lines = TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 16.0),
      "Hi".to_string(),
      12.0,
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);
    assert!(
      list
        .items()
        .iter()
        .any(|i| matches!(i, DisplayItem::TextDecoration(_))),
      "display list should include text decoration items"
    );
  }

  #[test]
  fn atomic_inline_does_not_emit_text_decoration_items_without_text_runs() {
    let mut style = ComputedStyle::default();
    style.display = Display::InlineBlock;
    style.text_decoration.lines = TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    style.font_size = 12.0;

    let atomic = FragmentNode::new_inline_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 16.0),
      0,
      vec![],
      Arc::new(style),
    );
    let line = FragmentNode::new_line(Rect::from_xywh(0.0, 0.0, 50.0, 16.0), 12.0, vec![atomic]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 16.0), vec![line]);

    let list = DisplayListBuilder::new().build(&root);
    assert!(
      !list
        .items()
        .iter()
        .any(|i| matches!(i, DisplayItem::TextDecoration(_))),
      "atomic inlines should not emit full-width text decoration items without shaped text runs"
    );
  }

  #[test]
  fn builder_text_decoration_uses_current_color_when_none() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::GREEN;
    style.text_decoration.lines = TextDecorationLine::UNDERLINE;
    // Leave text_decoration.color as None so it should resolve to currentColor.

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 16.0),
      "Hi".to_string(),
      12.0,
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    let deco_color = list
      .items()
      .iter()
      .find_map(|item| {
        if let DisplayItem::TextDecoration(dec) = item {
          dec.decorations.first().map(|d| d.color)
        } else {
          None
        }
      })
      .expect("text decoration emitted");

    assert_eq!(deco_color, Rgba::GREEN);
  }

  #[test]
  fn builder_prefers_explicit_text_decoration_color() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::GREEN;
    style.text_decoration.lines = TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLUE);

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 16.0),
      "Hi".to_string(),
      12.0,
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let deco_color = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::TextDecoration(dec) => dec.decorations.first().map(|d| d.color),
        _ => None,
      })
      .expect("decoration color present");

    assert_eq!(deco_color, Rgba::BLUE);
  }

  #[test]
  fn builder_resolves_font_relative_underline_offset() {
    let mut style = ComputedStyle::default();
    style.text_decoration.lines = TextDecorationLine::UNDERLINE;
    style.text_decoration.color = Some(Rgba::BLACK);
    style.font_size = 20.0;

    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 16.0),
      "Hi".to_string(),
      12.0,
      Arc::new(style.clone()),
    );

    let list_auto = DisplayListBuilder::new().build(&fragment);
    let auto_center = list_auto
      .items()
      .iter()
      .find_map(|item| {
        if let DisplayItem::TextDecoration(dec) = item {
          dec
            .decorations
            .first()
            .and_then(|d| d.underline.as_ref())
            .map(|u| u.center)
        } else {
          None
        }
      })
      .expect("underline present");

    let mut ex_style = style;
    ex_style.text_underline_offset = TextUnderlineOffset::Length(Length::ex(1.0));
    let fragment_ex = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 16.0),
      "Hi".to_string(),
      12.0,
      Arc::new(ex_style),
    );
    let list_ex = DisplayListBuilder::new().build(&fragment_ex);
    let ex_center = list_ex
      .items()
      .iter()
      .find_map(|item| {
        if let DisplayItem::TextDecoration(dec) = item {
          dec
            .decorations
            .first()
            .and_then(|d| d.underline.as_ref())
            .map(|u| u.center)
        } else {
          None
        }
      })
      .expect("underline present");

    assert!(
      ex_center > auto_center,
      "font-relative underline offset should move the underline further from the baseline"
    );
  }

  #[test]
  fn test_builder_image_fragment() {
    let fragment = create_image_fragment(
            0.0,
            0.0,
            100.0,
            100.0,
            "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E",
        );
    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    assert_eq!(
      list
        .items()
        .iter()
        .filter(|item| matches!(item, DisplayItem::Image(_)))
        .count(),
      1,
      "expected exactly one image item"
    );
  }

  #[test]
  fn default_builder_decodes_images_without_explicit_cache() {
    // 1x1 blue inline SVG
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="blue"/></svg>"#;
    let fragment = create_image_fragment(0.0, 0.0, 10.0, 10.0, svg);

    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("expected image display item");
    // Replaced SVGs are rasterized at their used size so `preserveAspectRatio` is applied in the
    // correct viewport.
    assert_eq!(img.image.width, 10);
    assert_eq!(img.image.height, 10);
  }

  #[test]
  fn stacking_context_order_respected() {
    let mut root = StackingContext::root();
    root.layer5_inlines.push(text_fragment_at(20.0, "root"));

    let mut neg = StackingContext::with_reason(-1, StackingContextReason::PositionedWithZIndex, 1);
    neg.layer5_inlines.push(text_fragment_at(0.0, "neg"));

    let mut pos = StackingContext::with_reason(1, StackingContextReason::PositionedWithZIndex, 2);
    pos.layer5_inlines.push(text_fragment_at(40.0, "pos"));

    root.add_child(neg);
    root.add_child(pos);
    root.sort_children();
    root.compute_bounds(None, None).unwrap();

    let list = DisplayListBuilder::new().build_from_stacking(&root);
    let origins: Vec<f32> = list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::Text(t) => Some(t.origin.x),
        _ => None,
      })
      .collect();

    assert_eq!(origins, vec![0.0, 20.0, 40.0]);
  }

  #[test]
  fn stacking_context_resolves_mask_images() {
    let mut style = ComputedStyle::default();
    let mut layer = crate::style::types::MaskLayer::default();
    layer.image = Some(BackgroundImage::Url(BackgroundImageUrl::new(
      data_url_for_color([0, 0, 0, 0]),
    )));
    layer.repeat = BackgroundRepeat::no_repeat();
    style.set_mask_layers(vec![layer]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let list =
      DisplayListBuilder::with_image_cache(ImageCache::new()).build_with_stacking_tree(&fragment);

    let push = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::PushStackingContext(ctx) => Some(ctx),
        _ => None,
      })
      .expect("stacking context emitted");
    let mask = push.mask.as_ref().expect("mask resolved");
    assert_eq!(mask.layers.len(), 1);
    match &mask.layers[0].image {
      ResolvedMaskImage::Raster(image) => {
        assert_eq!(image.width, 1);
        assert_eq!(image.height, 1);
      }
      other => panic!("expected raster mask image, got {other:?}"),
    }
  }

  fn root_transform_style(list: &DisplayList) -> TransformStyle {
    list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::PushStackingContext(ctx) => Some(ctx.transform_style),
        _ => None,
      })
      .expect("expected a root stacking context")
  }

  #[test]
  fn overflow_hidden_flattens_preserve_3d() {
    let mut style = ComputedStyle::default();
    style.transform = vec![Transform::Translate(Length::px(1.0), Length::px(2.0))];
    style.transform_style = TransformStyle::Preserve3d;
    style.overflow_x = Overflow::Hidden;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);

    assert_eq!(root_transform_style(&list), TransformStyle::Flat);
  }

  #[test]
  fn contain_paint_flattens_preserve_3d() {
    let mut style = ComputedStyle::default();
    style.transform = vec![Transform::Translate(Length::px(1.0), Length::px(2.0))];
    style.transform_style = TransformStyle::Preserve3d;
    style.containment = Containment::with_flags(false, false, false, false, true);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);

    assert_eq!(root_transform_style(&list), TransformStyle::Flat);
  }

  #[test]
  fn clip_rect_flattens_preserve_3d() {
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.transform = vec![Transform::Translate(Length::px(1.0), Length::px(2.0))];
    style.transform_style = TransformStyle::Preserve3d;
    style.clip = Some(ClipRect {
      top: ClipComponent::Auto,
      right: ClipComponent::Auto,
      bottom: ClipComponent::Auto,
      left: ClipComponent::Auto,
    });

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);

    assert_eq!(root_transform_style(&list), TransformStyle::Flat);
  }

  #[test]
  fn build_with_stacking_tree_respects_z_order_from_styles() {
    fn styled_fragment(x: f32, label: &str, z: i32) -> FragmentNode {
      let mut style = ComputedStyle::default();
      style.position = Position::Relative;
      style.z_index = Some(z);
      FragmentNode::new_inline_styled(
        Rect::from_xywh(x, 0.0, 10.0, 10.0),
        0,
        vec![FragmentNode::new_text(
          Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
          label.to_string(),
          12.0,
        )],
        Arc::new(style),
      )
    }

    let child_neg = styled_fragment(0.0, "neg", -1);
    let child_zero = styled_fragment(20.0, "zero", 0);
    let child_pos = styled_fragment(40.0, "pos", 1);

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![child_neg, child_zero, child_pos],
    );

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let origins: Vec<f32> = list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::Text(t) => Some(t.origin.x),
        _ => None,
      })
      .collect();

    assert_eq!(origins, vec![0.0, 20.0, 40.0]);
  }

  #[test]
  fn stacking_context_opacity_wraps_entire_subtree() {
    let mut style = ComputedStyle::default();
    style.opacity = 0.5;
    style.background_color = Rgba::RED;

    let child = create_text_fragment(0.0, 0.0, 10.0, 10.0, "child");
    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![child],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let items = list.items();

    let push_sc = items
      .iter()
      .position(|item| {
        matches!(
          item,
          DisplayItem::PushStackingContext(ctx) if (ctx.opacity - 0.5).abs() < 1e-6
        )
      })
      .expect("expected stacking context push with opacity");
    let pop_sc = items
      .iter()
      .rposition(|item| matches!(item, DisplayItem::PopStackingContext))
      .expect("expected stacking context pop");

    let background = items
      .iter()
      .position(|item| matches!(item, DisplayItem::FillRect(_)))
      .expect("expected root background");
    let text = items
      .iter()
      .position(|item| matches!(item, DisplayItem::Text(_)))
      .expect("expected child text");

    assert_eq!(
      items
        .iter()
        .filter(|item| matches!(item, DisplayItem::PushStackingContext(_)))
        .count(),
      1
    );
    assert_eq!(
      items
        .iter()
        .filter(|item| matches!(item, DisplayItem::PopStackingContext))
        .count(),
      1
    );
    assert!(
      items
        .iter()
        .filter(|item| matches!(item, DisplayItem::PushOpacity(_)))
        .count()
        == 0,
      "opacity should be represented on stacking contexts, not via PushOpacity"
    );
    assert!(push_sc < background && background < text && text < pop_sc);
  }

  #[test]
  fn stacking_context_plane_rect_uses_root_fragment_bounds() {
    let mut style = ComputedStyle::default();
    style
      .transform
      .push(Transform::Translate(Length::px(0.0), Length::px(0.0)));

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::WHITE;
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(80.0, 80.0, 50.0, 50.0),
      vec![],
      Arc::new(child_style),
    );
    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new()
      .with_culling_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let stacking = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::PushStackingContext(context) => Some(context),
        _ => None,
      })
      .expect("stacking context present");

    assert_eq!(stacking.bounds, Rect::from_xywh(0.0, 0.0, 130.0, 130.0));
    assert_eq!(stacking.plane_rect, Rect::from_xywh(0.0, 0.0, 100.0, 100.0));
  }

  #[test]
  fn stacking_context_transform_origin_uses_plane_rect() {
    let mut style = ComputedStyle::default();
    style.transform.push(Transform::Scale(2.0, 2.0));

    let child = FragmentNode::new_block(Rect::from_xywh(80.0, 80.0, 50.0, 50.0), vec![]);
    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let stacking = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::PushStackingContext(context) => Some(context),
        _ => None,
      })
      .expect("stacking context present");

    let transform = stacking.transform.as_ref().expect("transform present");
    let transform = transform.to_2d().expect("transform should be 2d");
    assert!((transform.a - 2.0).abs() < 1e-3);
    assert!((transform.e + 50.0).abs() < 1e-3);
    assert!((transform.f + 50.0).abs() < 1e-3);
  }

  #[test]
  fn stacking_context_offsets_include_non_context_ancestors() {
    let text = FragmentNode::new_text(
      Rect::from_xywh(3.0, 4.0, 10.0, 10.0),
      "hello".to_string(),
      0.0,
    );

    let mut stacking_style = ComputedStyle::default();
    stacking_style.opacity = 0.5;
    let stacking = FragmentNode::new_block_styled(
      Rect::from_xywh(5.0, 6.0, 20.0, 20.0),
      vec![text],
      Arc::new(stacking_style),
    );

    let intermediate =
      FragmentNode::new_block(Rect::from_xywh(10.0, 20.0, 50.0, 50.0), vec![stacking]);

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![intermediate]);

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let text_item = list.items().iter().find_map(|item| match item {
      DisplayItem::Text(t) => Some(t),
      _ => None,
    });

    let text_item = text_item.expect("text item emitted");
    assert_eq!(text_item.origin.x, 10.0 + 5.0 + 3.0);
    assert_eq!(text_item.origin.y, 20.0 + 6.0 + 4.0);
  }

  #[test]
  fn zero_z_contexts_interleave_with_positioned_descendants() {
    let stacking_text = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      "stack".to_string(),
      12.0,
    );
    let mut stacking_style = ComputedStyle::default();
    stacking_style.opacity = 0.5;
    let stacking_context = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![stacking_text],
      Arc::new(stacking_style),
    );

    let positioned_text = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      "pos".to_string(),
      12.0,
    );
    let mut positioned_style = ComputedStyle::default();
    positioned_style.position = Position::Relative;
    let positioned = FragmentNode::new_block_styled(
      Rect::from_xywh(20.0, 0.0, 10.0, 10.0),
      vec![positioned_text],
      Arc::new(positioned_style),
    );

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![stacking_context, positioned],
    );

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let origins: Vec<f32> = list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::Text(t) => Some(t.origin.x),
        _ => None,
      })
      .collect();

    assert_eq!(origins, vec![0.0, 20.0]);
  }

  #[test]
  fn test_builder_nested_fragments() {
    let child1 = create_text_fragment(0.0, 0.0, 50.0, 20.0, "One");
    let child2 = create_text_fragment(0.0, 20.0, 50.0, 20.0, "Two");
    let parent = FragmentNode::new_block(
      Rect::from_xywh(10.0, 10.0, 100.0, 50.0),
      vec![child1, child2],
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&parent);

    assert_eq!(list.len(), 2);
  }

  #[test]
  fn test_builder_position_offset() {
    let child = create_text_fragment(10.0, 10.0, 50.0, 20.0, "Hi");
    let parent = FragmentNode::new_block(Rect::from_xywh(20.0, 20.0, 100.0, 50.0), vec![child]);

    let builder = DisplayListBuilder::new();
    let list = builder.build(&parent);

    if let DisplayItem::Text(text) = &list.items()[0] {
      assert_eq!(text.origin.x, 30.0);
    } else {
      panic!("Expected Text item");
    }
  }

  #[test]
  fn test_builder_with_clips() {
    let child = create_text_fragment(0.0, 0.0, 50.0, 20.0, "Clipped");
    let parent =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), 42, vec![child]);

    let mut clips = HashSet::new();
    clips.insert(Some(42));

    let builder = DisplayListBuilder::new();
    let list = builder.build_with_clips(&parent, &clips);

    assert_eq!(list.len(), 3);
    assert!(matches!(list.items()[0], DisplayItem::PushClip(_)));
    assert!(matches!(list.items()[1], DisplayItem::Text(_)));
    assert!(matches!(list.items()[2], DisplayItem::PopClip));
  }

  #[test]
  fn test_builder_no_clips() {
    let child = create_text_fragment(0.0, 0.0, 50.0, 20.0, "NotClipped");
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), vec![child]);

    let clips = HashSet::new();

    let builder = DisplayListBuilder::new();
    let list = builder.build_with_clips(&parent, &clips);

    assert_eq!(list.len(), 1);
    assert!(matches!(list.items()[0], DisplayItem::Text(_)));
  }

  #[test]
  fn clip_property_emits_clip_item() {
    let mut style = ComputedStyle::default();
    style.position = Position::Absolute;
    style.clip = Some(crate::style::types::ClipRect {
      top: crate::style::types::ClipComponent::Length(Length::px(5.0)),
      right: crate::style::types::ClipComponent::Length(Length::px(15.0)),
      bottom: crate::style::types::ClipComponent::Length(Length::px(15.0)),
      left: crate::style::types::ClipComponent::Length(Length::px(5.0)),
    });
    style.background_color = Rgba::RED;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build_with_stacking_tree(&fragment);

    assert!(list
      .items()
      .iter()
      .any(|item| matches!(item, DisplayItem::PushClip(_))));
  }

  #[test]
  fn overflow_hidden_emits_clip_item() {
    let mut style = ComputedStyle::default();
    style.overflow_x = crate::style::types::Overflow::Hidden;
    style.overflow_y = crate::style::types::Overflow::Hidden;
    style.background_color = Rgba::BLUE;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build_with_stacking_tree(&fragment);

    assert!(list
      .items()
      .iter()
      .any(|item| matches!(item, DisplayItem::PushClip(_))));
  }

  #[test]
  fn axis_specific_overflow_clip_expands_rect_and_skips_radii() {
    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Visible;
    style.overflow_y = Overflow::Clip;
    style.background_color = Rgba::BLUE;
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let child = create_text_fragment(0.0, 0.0, 100.0, 10.0, "wide");
    let mut fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      vec![child],
      Arc::new(style),
    );
    fragment.scroll_overflow = Rect::from_xywh(0.0, 0.0, 100.0, 50.0);

    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);
    let clip_radii = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, radii } if *rect == Rect::from_xywh(0.0, 0.0, 100.0, 50.0) => {
            Some(*radii)
          }
          _ => None,
        },
        _ => None,
      })
      .expect("Expected axis-specific overflow clip");
    assert!(
      clip_radii.is_none(),
      "axis-specific overflow clipping should ignore border-radius"
    );
  }

  #[test]
  fn axis_specific_overflow_clip_expands_rect_vertically_and_skips_radii() {
    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Clip;
    style.overflow_y = Overflow::Visible;
    style.background_color = Rgba::BLUE;
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let child = create_text_fragment(0.0, 0.0, 10.0, 100.0, "tall");
    let mut fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      vec![child],
      Arc::new(style),
    );
    fragment.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 100.0);

    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);
    let clip_radii = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, radii } if *rect == Rect::from_xywh(0.0, 0.0, 50.0, 100.0) => {
            Some(*radii)
          }
          _ => None,
        },
        _ => None,
      })
      .expect("Expected axis-specific overflow clip");
    assert!(
      clip_radii.is_none(),
      "axis-specific overflow clipping should ignore border-radius"
    );
  }

  #[test]
  fn stacking_context_overflow_clip_uses_root_border_box() {
    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    style.background_color = Rgba::BLUE;
    style.display = Display::Block;
    style.opacity = 0.5;

    let mut fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(5.0, 6.0, 100.0, 100.0),
      vec![],
      Arc::new(style),
    );
    fragment.scroll_overflow = Rect::from_xywh(0.0, 0.0, 130.0, 100.0);

    let intermediate =
      FragmentNode::new_block(Rect::from_xywh(10.0, 20.0, 150.0, 150.0), vec![fragment]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![intermediate]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let items = list.items();

    let sc_bounds = items.iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) => Some(sc.bounds),
      _ => None,
    });
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(15.0, 26.0, 130.0, 100.0)),
      "stacking context bounds should include scroll overflow"
    );

    let clip_rects: Vec<Rect> = items
      .iter()
      .filter_map(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, .. } => Some(*rect),
          _ => None,
        },
        _ => None,
      })
      .collect();
    assert!(
      !clip_rects.is_empty(),
      "expected overflow clipping to push at least one clip"
    );
    assert!(
      clip_rects
        .iter()
        .all(|rect| *rect == Rect::from_xywh(15.0, 26.0, 100.0, 100.0)),
      "overflow clips should be resolved against the border box, not scroll overflow"
    );
  }

  #[test]
  fn stacking_context_bounds_clamped_to_viewport_visibility() {
    // Regression: `StackingContext.bounds` is computed from layout and includes paint overflow for
    // the whole document. When building a display list for a viewport, far-offscreen overflow (like
    // `text-indent:-9999em`) should not inflate stacking context bounds for layer allocation.
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.isolation = crate::style::types::Isolation::Isolate;
    style.background_color = Rgba::BLUE;

    let offscreen_text = create_text_fragment(-10_000.0, 0.0, 100.0, 20.0, "hidden");
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      vec![offscreen_text],
      Arc::new(style),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let sc_bounds = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) if sc.is_isolated => Some(sc.bounds),
      _ => None,
    });
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(0.0, 0.0, 50.0, 20.0)),
      "expected stacking context bounds to be clamped to the viewport visible rect"
    );
  }

  #[test]
  fn stacking_context_bounds_clamped_for_opacity_layer() {
    // Regression: clamping should also apply to opacity stacking contexts (which allocate a
    // compositing surface and therefore consume `StackingContextItem::bounds` during layer
    // allocation).
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.opacity = 0.5;
    child_style.background_color = Rgba::BLUE;

    let offscreen_text = create_text_fragment(-10_000.0, 0.0, 100.0, 20.0, "hidden");
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      vec![offscreen_text],
      Arc::new(child_style),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let sc_bounds = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) if (sc.opacity - 0.5).abs() < 1e-6 => Some(sc.bounds),
      _ => None,
    });
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(0.0, 0.0, 50.0, 20.0)),
      "expected opacity stacking context bounds to be clamped to the visible rect"
    );
  }

  #[test]
  fn stacking_context_bounds_include_box_shadow_paint_overflow() {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.isolation = crate::style::types::Isolation::Isolate;
    style.box_shadow = vec![crate::css::types::BoxShadow {
      offset_x: Length::px(0.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      spread_radius: Length::px(10.0),
      color: Rgba::RED,
      inset: false,
    }];

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let items = list.items();

    let sc_bounds = items.iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) if sc.is_isolated => Some(sc.bounds),
      _ => None,
    });
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(30.0, 30.0, 40.0, 40.0)),
      "stacking context bounds should include outer box-shadow paint overflow"
    );
  }

  #[test]
  fn box_shadow_blur_radius_keeps_css_radius() {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.box_shadow = vec![crate::css::types::BoxShadow {
      offset_x: Length::px(0.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(10.0),
      spread_radius: Length::px(0.0),
      color: Rgba::BLACK,
      inset: false,
    }];

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let item = list.items().iter().find_map(|item| match item {
      DisplayItem::BoxShadow(shadow) => Some(shadow),
      _ => None,
    });
    let item = item.expect("expected display list to contain a box shadow item");
    assert!((item.blur_radius - 10.0).abs() < 1e-6);
  }

  #[test]
  fn stacking_context_bounds_include_descendant_box_shadow_paint_overflow() {
    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    outer_style.isolation = crate::style::types::Isolation::Isolate;

    let mut inner_style = ComputedStyle::default();
    inner_style.display = Display::Block;
    inner_style.box_shadow = vec![crate::css::types::BoxShadow {
      offset_x: Length::px(0.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      spread_radius: Length::px(10.0),
      color: Rgba::RED,
      inset: false,
    }];

    let inner = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(inner_style),
    );
    let mid = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![inner]);
    let outer = FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 20.0, 20.0),
      vec![mid],
      Arc::new(outer_style),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![outer]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let items = list.items();

    let sc_bounds = items.iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) if sc.is_isolated => Some(sc.bounds),
      _ => None,
    });
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(30.0, 30.0, 40.0, 40.0)),
      "stacking context bounds should include descendant box-shadow paint overflow"
    );
  }

  #[test]
  fn stacking_context_bounds_include_outline_paint_overflow() {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.isolation = crate::style::types::Isolation::Isolate;
    style.outline_style = crate::style::types::OutlineStyle::Solid;
    style.outline_width = Length::px(8.0);
    style.outline_offset = Length::px(0.0);

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let items = list.items();

    let sc_bounds = items.iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) if sc.is_isolated => Some(sc.bounds),
      _ => None,
    });
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(32.0, 32.0, 36.0, 36.0)),
      "stacking context bounds should include outline paint overflow"
    );
  }

  #[test]
  fn stacking_context_bounds_include_text_shadow_paint_overflow() {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.isolation = crate::style::types::Isolation::Isolate;
    style.text_shadow = Arc::from([crate::css::types::TextShadow {
      offset_x: Length::px(-20.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(5.0),
      color: Some(Rgba::BLACK),
    }]);

    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);

    let list = DisplayListBuilder::new()
      .with_viewport_size(200.0, 200.0)
      .build_with_stacking_tree(&root);
    let items = list.items();

    let sc_bounds = items.iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(sc) if sc.is_isolated => Some(sc.bounds),
      _ => None,
    });
    let blur_outset = crate::paint::blur::css_shadow_blur_radius_to_sigma(5.0) * 3.0;
    let expected = Rect::from_xywh(
      20.0 - blur_outset,
      40.0 - blur_outset,
      40.0 + blur_outset,
      20.0 + blur_outset * 2.0,
    );
    let sc_bounds = sc_bounds.expect("expected isolated stacking context bounds");
    let eps = 1e-5;
    assert!(
      (sc_bounds.x() - expected.x()).abs() <= eps
        && (sc_bounds.y() - expected.y()).abs() <= eps
        && (sc_bounds.width() - expected.width()).abs() <= eps
        && (sc_bounds.height() - expected.height()).abs() <= eps,
      "stacking context bounds should include text-shadow paint overflow (got={sc_bounds:?} expected={expected:?})"
    );
  }

  #[test]
  fn clip_path_clip_is_inside_stacking_context() {
    let mut style = ComputedStyle::default();
    style.clip_path = ClipPath::BasicShape(
      Box::new(BasicShape::Inset {
        top: Length::px(2.0),
        right: Length::px(2.0),
        bottom: Length::px(2.0),
        left: Length::px(2.0),
        border_radius: Box::new(None),
      }),
      None,
    );
    style.background_color = Rgba::RED;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);
    let items = list.items();
    let (_, push_clip, pop_clip, _) = stacking_clip_order(items);

    assert!(
      has_paint_between(items, push_clip, pop_clip),
      "expected paint between clip operations inside stacking context"
    );
  }

  #[test]
  fn clip_path_url_clip_emits_alpha_mask_clip() {
    let defs = HashMap::from([(
      "clip".to_string(),
      r#"<clipPath xmlns="http://www.w3.org/2000/svg" id="clip"><rect x="0" y="0" width="10" height="20"/></clipPath>"#
        .to_string(),
    )]);

    let mut style = ComputedStyle::default();
    style.clip_path = ClipPath::Url("#clip".to_string(), None);
    style.background_color = Rgba::RED;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new()
      .with_svg_id_defs(Some(Arc::new(defs)))
      .build_with_stacking_tree(&fragment);
    let items = list.items();
    let (_push_sc, push_clip, _pop_clip, _pop_sc) = stacking_clip_order(items);

    let DisplayItem::PushClip(clip) = &items[push_clip] else {
      panic!("expected push clip display item");
    };
    match &clip.shape {
      ClipShape::AlphaMask { rect, .. } => assert_eq!(*rect, fragment.bounds),
      other => panic!("expected alpha mask clip, got {other:?}"),
    }
  }

  #[test]
  fn clip_path_path_parsing_aborts_on_cancel_callback() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 10);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let mut data = String::from("M0 0");
    for _ in 0..10_000 {
      data.push_str(" L1 1");
    }

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::RED;
    style.clip_path = ClipPath::BasicShape(
      Box::new(BasicShape::Path {
        fill: crate::style::types::FillRule::NonZero,
        data: Arc::from(data),
      }),
      None,
    );

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 32.0, 32.0),
      vec![],
      Arc::new(style),
    );

    let result = with_deadline(Some(&deadline), || {
      DisplayListBuilder::new().build_with_stacking_tree_checked(&fragment)
    });

    assert!(
      matches!(
        result,
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        }))
      ),
      "expected timeout, got {result:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 11);
  }

  #[test]
  fn transformed_overflow_clip_is_inside_stacking_context() {
    let mut style = ComputedStyle::default();
    style.transform = vec![Transform::Translate(Length::px(5.0), Length::px(0.0))];
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    style.background_color = Rgba::GREEN;

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
      vec![],
      Arc::new(child_style),
    );

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 30.0, 30.0),
      vec![child],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build_with_stacking_tree(&fragment);
    let items = list.items();
    let (push_sc, push_clip, pop_clip, _pop_sc) = stacking_clip_order(items);

    if let DisplayItem::PushStackingContext(stacking) = &items[push_sc] {
      assert!(
        stacking.transform.is_some(),
        "stacking context should carry transform"
      );
    } else {
      panic!("expected stacking context push item");
    }
    assert!(
      has_paint_between(items, push_clip, pop_clip),
      "expected painted content to be clipped inside stacking context"
    );
  }

  #[test]
  fn overflow_clip_preserves_radii_when_clipping_both_axes() {
    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(10.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_left_radius = radius;
    style.border_bottom_right_radius = radius;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 30.0, 30.0),
      vec![],
      Arc::new(style),
    );

    let clip = DisplayListBuilder::overflow_clip_from_style(
      fragment.style.as_deref().unwrap(),
      fragment.bounds,
      fragment.bounds,
      None,
      None,
    )
    .expect("overflow clip missing");

    let ClipShape::Rect { rect, radii } = &clip.shape else {
      panic!("expected rectangular overflow clip");
    };
    assert_eq!(*rect, fragment.bounds);
    assert!(
      radii.is_some(),
      "expected rounded overflow clip when both axes clip"
    );
  }

  #[test]
  fn test_emit_background() {
    let mut builder = DisplayListBuilder::new();
    builder.emit_background(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), Rgba::RED);

    let list = builder.list;
    assert_eq!(list.len(), 1);
    assert!(matches!(list.items()[0], DisplayItem::FillRect(_)));
  }

  #[test]
  fn test_emit_background_transparent_skipped() {
    let mut builder = DisplayListBuilder::new();
    builder.emit_background(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), Rgba::TRANSPARENT);

    let list = builder.list;
    assert!(list.is_empty());
  }

  #[test]
  fn background_repeating_linear_gradient_uses_pattern_item() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 45.0,
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
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(10.0)),
        BackgroundSizeComponent::Length(Length::px(10.0)),
      ),
      repeat: BackgroundRepeat::repeat(),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 30.0, 30.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let pattern_items = list
      .items()
      .iter()
      .filter(|item| matches!(item, DisplayItem::LinearGradientPattern(_)))
      .count();
    assert_eq!(
      pattern_items, 1,
      "expected exactly one gradient pattern item"
    );
    assert!(
      !list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::LinearGradient(_))),
      "builder should not emit per-tile gradient items for repeat+repeat gradients"
    );
  }

  #[test]
  fn background_linear_gradient_resolves_length_stop_positions() {
    // Regression test: `<length>` color stop positions in gradients should be resolved relative to
    // the gradient geometry (rather than treated as invalid and dropping the whole gradient).
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 90.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Length(Length::rem(
              2.0,
            ))),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Length(Length::rem(
              2.0,
            ))),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }]);

    // Root font size defaults to 16px, so 2rem should resolve to 32px. With a 64px-wide element,
    // the resolved stop positions should land at 0.5.
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 64.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let gradient = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::LinearGradient(item) => Some(item),
        _ => None,
      })
      .expect("expected a linear gradient display item");

    assert_eq!(gradient.stops.len(), 4);
    assert!((gradient.stops[0].position - 0.0).abs() < 1e-6);
    assert!((gradient.stops[1].position - 0.5).abs() < 1e-6);
    assert!((gradient.stops[2].position - 0.5).abs() < 1e-6);
    assert!((gradient.stops[3].position - 1.0).abs() < 1e-6);
  }

  #[test]
  fn background_linear_gradient_preserves_out_of_range_stop_positions() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 180.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(crate::css::types::ColorStopPosition::Fraction(-0.5)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.5)),
          },
        ],
      }),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }]);
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 64.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let stops = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::LinearGradient(item) => Some(item.stops.as_slice()),
        DisplayItem::LinearGradientPattern(item) => Some(item.stops.as_slice()),
        _ => None,
      })
      .expect("expected a linear gradient display item");

    assert_eq!(stops.len(), 2);
    assert!((stops[0].position - (-0.5)).abs() < 1e-6);
    assert!((stops[1].position - 1.5).abs() < 1e-6);
  }

  #[test]
  fn root_background_extension_sizes_gradient_to_viewport() {
    // When the canvas is taller than the root stacking context bounds, we extend the root
    // background paint rect to the viewport. The propagated gradient should behave as if it were
    // painted on the canvas itself (i.e. sized to the viewport), so it should not unexpectedly
    // repeat when the root element is shorter than the viewport.
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.background_color = Rgba::TRANSPARENT;

    let mut body_style = ComputedStyle::default();
    body_style.display = Display::Block;
    body_style.background_color = Rgba::TRANSPARENT;
    body_style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 180.0,
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

    let body_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(body_style),
    );
    let root_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![body_fragment],
      Arc::new(root_style),
    );
    let tree = FragmentTree::with_viewport(root_fragment, Size::new(10.0, 20.0));

    let list = DisplayListBuilder::new()
      .with_viewport_size(10.0, 20.0)
      .build_tree_with_stacking(&tree);

    let patterns: Vec<_> = list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::LinearGradientPattern(pattern) => Some(pattern),
        _ => None,
      })
      .collect();
    assert!(
      !patterns.is_empty(),
      "expected at least one linear gradient pattern item"
    );

    let extended = patterns
      .iter()
      .find(|pattern| (pattern.dest_rect.height() - 20.0).abs() < 0.01)
      .expect("expected an extended root background pattern covering the viewport height");
    assert!(
      (extended.tile_size.height - 20.0).abs() < 0.01,
      "expected root background tile height to match viewport height (20px), got {}",
      extended.tile_size.height
    );
  }

  #[test]
  fn background_blend_mode_emits_push_and_pop() {
    let mut style = ComputedStyle::default();
    style.background_color = Rgba::BLUE;
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
      repeat: BackgroundRepeat::no_repeat(),
      blend_mode: MixBlendMode::Multiply,
      ..BackgroundLayer::default()
    }]);
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let mut push_idx = None;
    let mut gradient_idx = None;
    let mut pop_idx = None;
    for (idx, item) in list.items().iter().enumerate() {
      match item {
        DisplayItem::PushBlendMode(mode) => {
          push_idx = Some(idx);
          assert_eq!(mode.mode, BlendMode::Multiply);
        }
        DisplayItem::LinearGradient(_) => gradient_idx = Some(idx),
        DisplayItem::PopBlendMode => pop_idx = Some(idx),
        _ => {}
      }
    }

    assert!(push_idx.is_some(), "blend push missing");
    assert!(pop_idx.is_some(), "blend pop missing");
    assert!(gradient_idx.is_some(), "background gradient missing");
    assert!(
      push_idx.unwrap() < gradient_idx.unwrap() && gradient_idx.unwrap() < pop_idx.unwrap(),
      "blend mode should wrap background layer"
    );
  }

  #[test]
  fn background_url_resolves_relative_to_base() {
    // Create a 1x1 PNG on disk.
    let mut path: PathBuf = std::env::temp_dir();
    path.push(format!(
      "fastrender_dl_base_url_{}_{}.png",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    let img = image::RgbaImage::from_raw(1, 1, vec![0, 0, 0, 255]).expect("raw rgba");
    img.save(&path).expect("write png");

    let dir = path.parent().unwrap().to_path_buf();
    let base_url = url::Url::from_directory_path(&dir).unwrap().to_string();

    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(
        path.file_name().unwrap().to_str().unwrap().to_string(),
      ))),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let list = DisplayListBuilder::new()
      .with_base_url(base_url)
      .build(&fragment);

    // Expect one image item in the list (background image decoded).
    assert!(
      list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::Image(_))),
      "background image should decode via base URL"
    );
  }

  #[test]
  fn background_svg_rasterizes_at_tile_size() {
    let svg = "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%22300%22%20height=%22150%22%3E%3Crect%20width=%22100%25%22%20height=%22100%25%22%20fill=%22red%22/%3E%3C/svg%3E";

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(svg))),
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(20.0)),
        BackgroundSizeComponent::Length(Length::px(20.0)),
      ),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::with_image_cache(ImageCache::new()).build(&fragment);
    let image_data = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(&img.image),
        DisplayItem::ImagePattern(pattern) => Some(&pattern.image),
        _ => None,
      })
      .expect("background image should emit an image item");

    assert_eq!(
      (image_data.width, image_data.height),
      (20, 20),
      "SVG background images should rasterize at the resolved tile size"
    );
  }

  #[test]
  fn background_viewbox_only_svg_auto_size_uses_contain_sizing() {
    // SVG with an intrinsic ratio (from viewBox) but no intrinsic size (no width/height).
    // `background-size: auto auto` should size this as if `contain` were specified.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 20"><rect width="40" height="20" fill="red"/></svg>"#;

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(svg))),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 400.0, 200.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::with_image_cache(ImageCache::new()).build(&fragment);
    let (tile_w, tile_h) = list
      .items()
      .iter()
      .find_map(|item| match item {
        // When the resolved tiling plan only paints one tile, the builder emits an Image item.
        DisplayItem::Image(img) => Some((img.dest_rect.width(), img.dest_rect.height())),
        DisplayItem::ImagePattern(pattern) => Some((pattern.tile_size.width, pattern.tile_size.height)),
        _ => None,
      })
      .expect("background should emit an image item");

    assert!(
      (tile_w - 400.0).abs() < 1e-6,
      "expected contain sizing to pick tile width equal to area width, got {}",
      tile_w
    );
    assert!(
      (tile_h - 200.0).abs() < 1e-6,
      "expected contain sizing to pick tile height equal to area height, got {}",
      tile_h
    );
  }

  #[test]
  fn background_image_set_respects_device_pixel_ratio() {
    let low = data_url_for_color([255, 0, 0, 255]);
    let high = data_url_for_color([0, 255, 0, 255]);

    let mut style = ComputedStyle::default();
    with_image_set_dpr(2.0, || {
      apply_declaration(
        &mut style,
        &Declaration {
          property: "background-image".into(),
          value: PropertyValue::Keyword(format!(
            "image-set(url(\"{}\") 1x, url(\"{}\") 2x)",
            low, high
          )),
          contains_var: false,
          raw_value: String::new(),
          important: false,
        },
        &ComputedStyle::default(),
        16.0,
        16.0,
      );
    });
    style.background_color = Rgba::TRANSPARENT;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let image_data = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(&img.image),
        DisplayItem::ImagePattern(pattern) => Some(&pattern.image),
        _ => None,
      })
      .expect("background image should emit an image item");

    assert_eq!(&image_data.pixels[..4], &[0, 255, 0, 255]);
  }

  #[test]
  fn content_image_set_respects_device_pixel_ratio() {
    let low = data_url_for_color([255, 0, 0, 255]);
    let high = data_url_for_color([0, 0, 255, 255]);

    let content = with_image_set_dpr(2.0, || {
      parse_content(&format!(
        "image-set(url(\"{}\") 1x, url(\"{}\") 2x)",
        low, high
      ))
      .expect("parse content")
    });

    let chosen = match &content {
      ContentValue::Items(items) if items.len() == 1 => match &items[0] {
        ContentItem::Url(url) => url.clone(),
        other => panic!("unexpected content item: {other:?}"),
      },
      other => panic!("unexpected content value: {other:?}"),
    };
    let srcset = chosen
      .override_resolution
      .filter(|d| d.is_finite() && *d > 0.0)
      .map(|density| {
        vec![SrcsetCandidate {
          url: chosen.url.clone(),
          descriptor: SrcsetDescriptor::Density(density),
        }]
      })
      .unwrap_or_default();

    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      ReplacedType::Image {
        src: chosen.url.clone(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset,
        picture_sources: Vec::new(),
      },
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let image = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("content image should emit an image item");

    assert_eq!(&image.image.pixels[..4], &[0, 0, 255, 255]);
  }

  #[test]
  fn list_style_image_image_set_respects_device_pixel_ratio() {
    let low = data_url_for_solid_png(10, 10, [255, 0, 0, 255]);
    let high = data_url_for_solid_png(20, 20, [0, 255, 0, 255]);

    let mut style = ComputedStyle::default();
    with_image_set_dpr(2.0, || {
      apply_declaration(
        &mut style,
        &Declaration {
          property: "list-style-image".into(),
          value: PropertyValue::Keyword(format!(
            "image-set(url(\"{}\") 1x, url(\"{}\") 2x)",
            low, high
          )),
          contains_var: false,
          raw_value: String::new(),
          important: false,
        },
        &ComputedStyle::default(),
        16.0,
        16.0,
      );
    });

    let styled = styled_element("li");
    let counters = crate::style::counters::CounterManager::new();
    let mut quote_depth = 0usize;
    let marker = crate::tree::box_generation::marker_content_from_style(
      &styled,
      &style,
      &counters,
      &mut quote_depth,
    )
    .expect("marker content");
    let replaced = match marker {
      crate::tree::box_tree::MarkerContent::Image(replaced) => replaced,
      other => panic!("unexpected marker content: {other:?}"),
    };

    let fragment =
      FragmentNode::new_replaced(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), replaced.replaced_type);

    let list = DisplayListBuilder::with_image_cache(ImageCache::new())
      .with_device_pixel_ratio(2.0)
      .build(&fragment);
    let image = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("marker image should emit an image item");

    assert_eq!(&image.image.pixels[..4], &[0, 255, 0, 255]);
    assert_eq!((image.image.width, image.image.height), (20, 20));
    assert!(
      (image.image.css_width - 10.0).abs() < 1e-6,
      "expected 20px @2x asset to report css width 10px, got {}",
      image.image.css_width
    );
    assert!(
      (image.image.css_height - 10.0).abs() < 1e-6,
      "expected 20px @2x asset to report css height 10px, got {}",
      image.image.css_height
    );
  }

  #[test]
  fn test_emit_border() {
    let mut builder = DisplayListBuilder::new();
    builder.emit_border(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), 2.0, Rgba::BLACK);

    let list = builder.list;
    assert_eq!(list.len(), 1);
    assert!(matches!(list.items()[0], DisplayItem::StrokeRect(_)));
  }

  #[test]
  fn test_push_pop_opacity() {
    let mut builder = DisplayListBuilder::new();
    builder.push_opacity(0.5);
    builder.emit_background(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), Rgba::RED);
    builder.pop_opacity();

    let list = builder.list;
    assert_eq!(list.len(), 3);
    assert!(matches!(list.items()[0], DisplayItem::PushOpacity(_)));
    assert!(matches!(list.items()[1], DisplayItem::FillRect(_)));
    assert!(matches!(list.items()[2], DisplayItem::PopOpacity));
  }

  #[test]
  fn fragments_with_zero_opacity_emit_nothing() {
    let mut frag = create_block_fragment(0.0, 0.0, 50.0, 50.0);
    let mut style = ComputedStyle::default();
    style.opacity = 0.0;
    frag.style = Some(Arc::new(style));

    let list = DisplayListBuilder::new().build(&frag);
    assert!(list.is_empty(), "zero-opacity fragments should be skipped");
  }

  #[test]
  fn test_push_pop_clip() {
    let mut builder = DisplayListBuilder::new();
    builder.push_clip(Rect::from_xywh(0.0, 0.0, 50.0, 50.0));
    builder.emit_background(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), Rgba::RED);
    builder.pop_clip();

    let list = builder.list;
    assert_eq!(list.len(), 3);
    assert!(matches!(list.items()[0], DisplayItem::PushClip(_)));
    assert!(matches!(list.items()[1], DisplayItem::FillRect(_)));
    assert!(matches!(list.items()[2], DisplayItem::PopClip));
  }

  #[test]
  fn test_fragment_tree_wrapper() {
    let child = create_text_fragment(10.0, 10.0, 50.0, 20.0, "Tree");
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), vec![child]);
    let tree = FragmentTree::new(root);

    let builder = DisplayListBuilder::new();
    let list = builder.build_tree(&tree);

    assert_eq!(list.len(), 1);
  }

  #[test]
  fn test_empty_text_skipped() {
    let fragment = create_text_fragment(0.0, 0.0, 100.0, 20.0, "");
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(list.is_empty());
  }

  #[test]
  fn visibility_hidden_skips_display_items() {
    let mut style = ComputedStyle::default();
    style.visibility = crate::style::computed::Visibility::Hidden;
    let fragment = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      "hidden".to_string(),
      16.0,
      Arc::new(style),
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(list.is_empty());
  }

  #[test]
  fn visibility_hidden_allows_visible_descendants() {
    let mut parent_style = ComputedStyle::default();
    parent_style.visibility = crate::style::computed::Visibility::Hidden;
    parent_style.opacity = 0.5;
    parent_style.background_color = Rgba::RED;
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.visibility = crate::style::computed::Visibility::Visible;

    let child = FragmentNode::new_text_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      "visible".to_string(),
      16.0,
      Arc::new(child_style),
    );
    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 120.0, 40.0),
      vec![child],
      Arc::new(parent_style),
    );

    let list = DisplayListBuilder::new().build(&parent);
    let items = list.items();
    assert!(
      matches!(items.first(), Some(DisplayItem::PushOpacity(_))),
      "expected hidden ancestor opacity to wrap descendants"
    );
    assert!(
      items
        .iter()
        .any(|item| matches!(item, DisplayItem::Text(_))),
      "expected visible descendant content to paint"
    );
    assert!(
      matches!(items.last(), Some(DisplayItem::PopOpacity)),
      "expected hidden ancestor opacity to wrap descendants"
    );
    assert!(
      !items
        .iter()
        .any(|item| matches!(item, DisplayItem::FillRect(fill) if fill.color == Rgba::RED)),
      "hidden ancestor should not paint its own background"
    );
  }

  #[test]
  fn outline_emits_stroke_rect() {
    let mut style = ComputedStyle::default();
    style.outline_style = crate::style::types::OutlineStyle::Solid;
    style.outline_width = Length::px(2.0);
    style.outline_color = crate::style::types::OutlineColor::Color(Rgba::RED);
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);
    assert!(
      list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::Outline(_))),
      "outline should emit outline item"
    );
  }

  #[test]
  fn outline_emits_even_when_clipped() {
    let mut style = ComputedStyle::default();
    style.outline_style = crate::style::types::OutlineStyle::Solid;
    style.outline_width = Length::px(2.0);
    style.outline_color = crate::style::types::OutlineColor::Color(Rgba::RED);
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );
    let clips = vec![None].into_iter().collect();
    let builder = DisplayListBuilder::new();
    let list = builder.build_with_clips(&fragment, &clips);
    assert!(
      list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::Outline(_))),
      "outline should be emitted even when fragment is clipped"
    );
  }

  #[test]
  fn hidden_fragment_skipped_with_clips() {
    let mut style = ComputedStyle::default();
    style.visibility = crate::style::computed::Visibility::Hidden;
    let mut child_style = ComputedStyle::default();
    child_style.visibility = crate::style::computed::Visibility::Hidden;
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![FragmentNode::new_text_styled(
        Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
        "hidden".to_string(),
        12.0,
        Arc::new(child_style),
      )],
      Arc::new(style),
    );
    let clips = HashSet::from([None]);
    let builder = DisplayListBuilder::new();
    let list = builder.build_with_clips(&fragment, &clips);
    assert!(
      list.is_empty(),
      "hidden fragments should not emit display items"
    );
  }

  #[test]
  fn test_deeply_nested() {
    let text = create_text_fragment(5.0, 5.0, 20.0, 10.0, "X");
    let level3 = FragmentNode::new_block(Rect::from_xywh(5.0, 5.0, 30.0, 20.0), vec![text]);
    let level2 = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 50.0, 40.0), vec![level3]);
    let level1 = FragmentNode::new_block(Rect::from_xywh(20.0, 20.0, 70.0, 60.0), vec![level2]);

    let builder = DisplayListBuilder::new();
    let list = builder.build(&level1);

    assert_eq!(list.len(), 1);
    if let DisplayItem::Text(text) = &list.items()[0] {
      assert_eq!(text.origin.x, 40.0);
    }
  }

  #[test]
  fn test_complex_tree() {
    let text1 = create_text_fragment(0.0, 0.0, 100.0, 20.0, "Line1");
    let text2 = create_text_fragment(0.0, 20.0, 100.0, 20.0, "Line2");
    let image = create_image_fragment(
            0.0,
            40.0,
            50.0,
            50.0,
            "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E",
        );

    let inner = FragmentNode::new_block(
      Rect::from_xywh(10.0, 10.0, 120.0, 100.0),
      vec![text1, text2, image],
    );
    let outer = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![inner]);

    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&outer);

    let text_count = list
      .items()
      .iter()
      .filter(|i| matches!(i, DisplayItem::Text(_)))
      .count();
    let image_count = list
      .items()
      .iter()
      .filter(|i| matches!(i, DisplayItem::Image(_)))
      .count();

    assert_eq!(text_count, 2);
    assert_eq!(image_count, 1);
  }

  #[test]
  fn test_image_decoding_uses_cache() {
    // 1x1 red inline SVG
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="red"/></svg>"#;
    let fragment = create_image_fragment(0.0, 0.0, 10.0, 10.0, svg);
    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    // Replaced SVGs are rasterized at their used size so `preserveAspectRatio` is applied in the
    // correct viewport.
    assert_eq!(img.image.width, 10);
    assert_eq!(img.image.height, 10);
    let pixels = img.image.pixels.as_ref();
    assert_eq!(pixels.len(), 10 * 10 * 4);
    assert!(pixels
      .chunks_exact(4)
      .all(|px| px == [255, 0, 0, 255]));
  }

  #[test]
  fn decode_image_viewbox_only_svg_has_no_css_natural_size_when_decorative() {
    let builder = DisplayListBuilder::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><circle cx="5" cy="5" r="4" fill="black"/></svg>"#;
    let decoded = builder
      .decode_image(svg, None, true, CrossOriginAttribute::None, None, false, None)
      .expect("decoded viewBox-only svg");

    assert_eq!(decoded.css_width, 0.0);
    assert_eq!(decoded.css_height, 0.0);
    assert!(
      decoded.has_intrinsic_ratio,
      "viewBox-only SVGs still have an intrinsic ratio"
    );
  }

  #[test]
  fn decode_image_cache_reuses_converted_pixels() {
    let src = data_url_for_color([5, 6, 7, 8]);
    let mut builder = DisplayListBuilder::new();

    let first = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false, None)
      .expect("first decode");
    let second = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false, None)
      .expect("cached decode");
    assert!(Arc::ptr_eq(&first, &second));

    let overridden = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false, Some(2.0))
      .expect("override decode");
    assert!(!Arc::ptr_eq(&first, &overridden));
    let overridden_cached = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false, Some(2.0))
      .expect("cached override decode");
    assert!(Arc::ptr_eq(&overridden, &overridden_cached));

    let mut rotated_style = ComputedStyle::default();
    rotated_style.image_orientation = ImageOrientation::Angle {
      quarter_turns: 1,
      flip: false,
    };
    let rotated = builder
      .decode_image(
        &src,
        Some(&rotated_style),
        false,
        CrossOriginAttribute::None,
        None,
        false,
        None,
      )
      .expect("rotated decode");
    assert!(!Arc::ptr_eq(&first, &rotated));

    builder.set_device_pixel_ratio(2.0);
    let hidpi = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false, None)
      .expect("hi-dpi decode");
    assert!(!Arc::ptr_eq(&first, &hidpi));
    let hidpi_cached = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false, None)
      .expect("cached hi-dpi decode");
    assert!(Arc::ptr_eq(&hidpi, &hidpi_cached));
  }

  #[test]
  fn decode_image_cache_partitions_by_crossorigin_and_enforces_cors() {
    use crate::api::ResourceContext;
    use crate::error::Result;
    use crate::resource::{
      FetchDestination, FetchRequest, FetchedResource, ResourceAccessPolicy, ResourceFetcher,
    };

    #[derive(Clone)]
    struct RecordingFetcher {
      bytes: Arc<Vec<u8>>,
      destinations: Arc<std::sync::Mutex<Vec<FetchDestination>>>,
    }

    impl RecordingFetcher {
      fn new(bytes: Vec<u8>) -> (Self, Arc<std::sync::Mutex<Vec<FetchDestination>>>) {
        let destinations = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
          Self {
            bytes: Arc::new(bytes),
            destinations: Arc::clone(&destinations),
          },
          destinations,
        )
      }

      fn make_resource(&self, url: &str) -> FetchedResource {
        let mut res = FetchedResource::new((*self.bytes).clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        res
      }
    }

    impl ResourceFetcher for RecordingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Ok(self.make_resource(url))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if let Ok(mut guard) = self.destinations.lock() {
          guard.push(req.destination);
        }
        self.fetch(req.url)
      }
    }

    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
      .write_image(&[0u8, 0, 0, 255], 1, 1, ColorType::Rgba8.into())
      .expect("encode png");
    let (fetcher, destinations) = RecordingFetcher::new(buf);

    let doc_url = "https://doc.test/";
    let policy =
      ResourceAccessPolicy::default().for_origin(crate::resource::origin_from_url(doc_url));
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher));
    cache.set_resource_context(Some(ResourceContext {
      document_url: Some(doc_url.to_string()),
      policy,
      ..Default::default()
    }));

    crate::debug::runtime::with_thread_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        "FASTR_FETCH_ENFORCE_CORS".to_string(),
        "1".to_string(),
      )]))),
      || {
        let builder = DisplayListBuilder::with_image_cache(cache);
        let url = "https://img.test/no-acao.png";

        // Baseline: no-cors loads should succeed and populate the decode cache.
        assert!(
          builder
            .decode_image(url, None, false, CrossOriginAttribute::None, None, false, None)
            .is_some(),
          "expected no-cors decode to succeed"
        );

        // Crossorigin loads should not be able to reuse the no-cors decoded pixels when CORS
        // enforcement is enabled; missing ACAO should surface as a load failure.
        assert!(
          builder
            .decode_image(
              url,
              None,
              false,
              CrossOriginAttribute::Anonymous,
              None,
              false,
              None,
            )
            .is_none(),
          "expected crossorigin decode to fail without ACAO"
        );
      },
    );

    let recorded = destinations.lock().unwrap().clone();
    assert_eq!(
      recorded,
      vec![FetchDestination::Image, FetchDestination::ImageCors],
      "expected decode cache to keep no-cors and cors-mode fetch profiles separate"
    );
  }

  #[test]
  fn embed_and_object_decode_images() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="blue"/></svg>"#;

    let embed_fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      ReplacedType::Embed {
        src: svg.to_string(),
      },
    );
    let embed_list = DisplayListBuilder::with_image_cache(ImageCache::new()).build(&embed_fragment);
    let embed_img = embed_list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("expected image item for embed");
    // Replaced SVGs are rasterized at their used size so `preserveAspectRatio` is applied in the
    // correct viewport.
    assert_eq!(embed_img.image.width, 10);
    assert_eq!(embed_img.image.height, 10);

    let object_fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      ReplacedType::Object {
        data: svg.to_string(),
      },
    );
    let object_list =
      DisplayListBuilder::with_image_cache(ImageCache::new()).build(&object_fragment);
    let object_img = object_list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("expected image item for object");
    // Replaced SVGs are rasterized at their used size so `preserveAspectRatio` is applied in the
    // correct viewport.
    assert_eq!(object_img.image.width, 10);
    assert_eq!(object_img.image.height, 10);
  }

  #[test]
  fn test_object_fit_contain_applied_in_display_list() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.object_fit = crate::style::types::ObjectFit::Contain;
    style.object_position = crate::style::types::ObjectPosition {
      x: crate::style::types::PositionComponent::Keyword(
        crate::style::types::PositionKeyword::Center,
      ),
      y: crate::style::types::PositionComponent::Keyword(
        crate::style::types::PositionKeyword::Center,
      ),
    };

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    // Image is 1x1, box is 200x100, contain => scale to min(200,100) => 100x100, centered horizontally.
    assert!((img.dest_rect.width() - 100.0).abs() < 0.1);
    assert!((img.dest_rect.height() - 100.0).abs() < 0.1);
    assert!((img.dest_rect.x() - 50.0).abs() < 0.1);
    assert!((img.dest_rect.y() - 0.0).abs() < 0.1);
  }

  #[test]
  fn replaced_object_fit_uses_content_box_and_clips_when_overflow_hidden() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.object_fit = crate::style::types::ObjectFit::Fill;
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    style.padding_top = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.padding_bottom = Length::px(10.0);
    style.padding_left = Length::px(10.0);
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    let img_idx = list
      .items()
      .iter()
      .position(|item| matches!(item, DisplayItem::Image(_)))
      .expect("Expected image item");
    let img = match &list.items()[img_idx] {
      DisplayItem::Image(img) => img,
      _ => unreachable!(),
    };

    let content_rect = Rect::from_xywh(10.0, 10.0, 80.0, 80.0);
    assert_eq!(img.dest_rect, content_rect);

    let content_clip_idx = list
      .items()
      .iter()
      .position(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, radii } => *rect == content_rect && radii.is_some(),
          _ => false,
        },
        _ => false,
      })
      .expect("Expected content-box clip around replaced contents");
    assert!(
      content_clip_idx < img_idx,
      "content clip should be pushed before the image"
    );

    let clip_radii = match &list.items()[content_clip_idx] {
      DisplayItem::PushClip(clip) => match &clip.shape {
        ClipShape::Rect { rect, radii } if *rect == content_rect => *radii,
        _ => None,
      },
      _ => None,
    };
    assert_eq!(clip_radii, Some(BorderRadii::uniform(10.0)));

    assert!(
      list
        .items()
        .iter()
        .skip(img_idx + 1)
        .any(|item| matches!(item, DisplayItem::PopClip)),
      "expected clip pop after the image"
    );
  }

  #[test]
  fn replaced_object_fit_does_not_clip_when_overflow_visible() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.object_fit = crate::style::types::ObjectFit::Fill;
    style.overflow_x = Overflow::Visible;
    style.overflow_y = Overflow::Visible;
    style.padding_top = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.padding_bottom = Length::px(10.0);
    style.padding_left = Length::px(10.0);
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    assert_eq!(img.dest_rect, Rect::from_xywh(10.0, 10.0, 80.0, 80.0));

    assert!(
      !list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::PushClip(_))),
      "expected no replaced-content clip when overflow is visible"
    );
  }

  #[test]
  fn replaced_object_fit_clips_only_y_when_overflow_x_visible_overflow_y_clip() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.object_fit = crate::style::types::ObjectFit::Cover;
    style.overflow_x = Overflow::Visible;
    style.overflow_y = Overflow::Clip;
    style.padding_top = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.padding_bottom = Length::px(10.0);
    style.padding_left = Length::px(10.0);
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%222%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    let img_idx = list
      .items()
      .iter()
      .position(|item| matches!(item, DisplayItem::Image(_)))
      .expect("Expected image item");
    let img = match &list.items()[img_idx] {
      DisplayItem::Image(img) => img,
      _ => unreachable!(),
    };
    let expected_dest = Rect::from_xywh(-30.0, 10.0, 160.0, 80.0);
    assert_eq!(img.dest_rect, expected_dest);

    let clip_idx = list
      .items()
      .iter()
      .position(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, radii } => *rect == expected_dest && radii.is_none(),
          _ => false,
        },
        _ => false,
      })
      .expect("Expected axis-specific clip around replaced contents");
    assert!(clip_idx < img_idx, "clip should be pushed before image");
    assert!(
      list
        .items()
        .iter()
        .skip(img_idx + 1)
        .any(|item| matches!(item, DisplayItem::PopClip)),
      "expected clip pop after image"
    );
  }

  #[test]
  fn replaced_object_fit_clips_only_x_when_overflow_x_clip_overflow_y_visible() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.object_fit = crate::style::types::ObjectFit::Cover;
    style.overflow_x = Overflow::Clip;
    style.overflow_y = Overflow::Visible;
    style.padding_top = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.padding_bottom = Length::px(10.0);
    style.padding_left = Length::px(10.0);
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%222%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    let img_idx = list
      .items()
      .iter()
      .position(|item| matches!(item, DisplayItem::Image(_)))
      .expect("Expected image item");
    let img = match &list.items()[img_idx] {
      DisplayItem::Image(img) => img,
      _ => unreachable!(),
    };
    let expected_dest = Rect::from_xywh(10.0, -30.0, 80.0, 160.0);
    assert_eq!(img.dest_rect, expected_dest);

    let clip_idx = list
      .items()
      .iter()
      .position(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, radii } => *rect == expected_dest && radii.is_none(),
          _ => false,
        },
        _ => false,
      })
      .expect("Expected axis-specific clip around replaced contents");
    assert!(clip_idx < img_idx, "clip should be pushed before image");
    assert!(
      list
        .items()
        .iter()
        .skip(img_idx + 1)
        .any(|item| matches!(item, DisplayItem::PopClip)),
      "expected clip pop after image"
    );
  }

  #[test]
  fn replaced_iframe_uses_content_box_and_clips_when_overflow_hidden() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    style.padding_top = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.padding_bottom = Length::px(10.0);
    style.padding_left = Length::px(10.0);
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Iframe {
          src: "about:blank".to_string(),
          srcdoc: Some("<html></html>".to_string()),
          sandbox: crate::tree::box_tree::IframeSandboxAttribute::None,
          referrer_policy: None,
          frame_token: None,
        },
      },
      vec![],
      Arc::new(style),
    );

    let mut image_cache = ImageCache::new();
    image_cache.set_resource_context(Some(crate::api::ResourceContext {
      iframe_depth_remaining: Some(0),
      ..crate::api::ResourceContext::default()
    }));
    let builder = DisplayListBuilder::with_image_cache(image_cache);
    let list = builder.build(&fragment);

    let img_idx = list
      .items()
      .iter()
      .position(|item| matches!(item, DisplayItem::Image(_)))
      .expect("Expected image item");
    let img = match &list.items()[img_idx] {
      DisplayItem::Image(img) => img,
      _ => unreachable!(),
    };

    let content_rect = Rect::from_xywh(10.0, 10.0, 80.0, 80.0);
    assert_eq!(img.dest_rect, content_rect);
    assert_eq!(img.image.css_width, 80.0);
    assert_eq!(img.image.css_height, 80.0);

    let content_clip_idx = list
      .items()
      .iter()
      .position(|item| match item {
        DisplayItem::PushClip(clip) => match &clip.shape {
          ClipShape::Rect { rect, radii } => *rect == content_rect && radii.is_some(),
          _ => false,
        },
        _ => false,
      })
      .expect("Expected content-box clip around iframe contents");
    assert!(
      content_clip_idx < img_idx,
      "content clip should be pushed before the iframe image"
    );

    let clip_radii = match &list.items()[content_clip_idx] {
      DisplayItem::PushClip(clip) => match &clip.shape {
        ClipShape::Rect { rect, radii } if *rect == content_rect => *radii,
        _ => None,
      },
      _ => None,
    };
    assert_eq!(clip_radii, Some(BorderRadii::uniform(10.0)));

    assert!(
      list
        .items()
        .iter()
        .skip(img_idx + 1)
        .any(|item| matches!(item, DisplayItem::PopClip)),
      "expected clip pop after the iframe image"
    );
  }

  #[test]
  fn replaced_iframe_does_not_clip_when_overflow_visible() {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    style.overflow_x = Overflow::Visible;
    style.overflow_y = Overflow::Visible;
    style.padding_top = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.padding_bottom = Length::px(10.0);
    style.padding_left = Length::px(10.0);
    let radius = crate::style::types::BorderCornerRadius::uniform(Length::px(20.0));
    style.border_top_left_radius = radius;
    style.border_top_right_radius = radius;
    style.border_bottom_right_radius = radius;
    style.border_bottom_left_radius = radius;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Iframe {
          src: "about:blank".to_string(),
          srcdoc: Some("<html></html>".to_string()),
          sandbox: crate::tree::box_tree::IframeSandboxAttribute::None,
          referrer_policy: None,
          frame_token: None,
        },
      },
      vec![],
      Arc::new(style),
    );

    let mut image_cache = ImageCache::new();
    image_cache.set_resource_context(Some(crate::api::ResourceContext {
      iframe_depth_remaining: Some(0),
      ..crate::api::ResourceContext::default()
    }));
    let builder = DisplayListBuilder::with_image_cache(image_cache);
    let list = builder.build(&fragment);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    assert_eq!(img.dest_rect, Rect::from_xywh(10.0, 10.0, 80.0, 80.0));

    assert!(
      !list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::PushClip(_))),
      "expected no iframe-content clip when overflow is visible"
    );
  }

  #[test]
  fn object_position_viewport_units_resolve_in_display_list() {
    let mut style = ComputedStyle::default();
    style.object_fit = crate::style::types::ObjectFit::None;
    // Position 10vw from the left of the box. With 200px viewport width, free space is 50px (100-50).
    style.object_position = crate::style::types::ObjectPosition {
      x: crate::style::types::PositionComponent::Length(crate::style::values::Length::new(
        10.0,
        crate::style::values::LengthUnit::Vw,
      )),
      y: crate::style::types::PositionComponent::Keyword(
        crate::style::types::PositionKeyword::Start,
      ),
    };

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let tree = FragmentTree::with_viewport(fragment, crate::geometry::Size::new(200.0, 200.0));
    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build_tree(&tree);

    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    // free_x = 100 - 1 = 99; but we align with 10vw (20px), so dest_rect.x should be ~20.
    assert!((img.dest_rect.x() - 20.0).abs() < 0.5);
  }

  #[test]
  fn image_rendering_pixelated_sets_nearest_filter_quality() {
    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::Pixelated;
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    assert_eq!(img.filter_quality, ImageFilterQuality::Nearest);
  }

  #[test]
  fn image_rendering_crisp_edges_sets_nearest_filter_quality() {
    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::CrispEdges;
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E".to_string(),
          alt: None,
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let img = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("Expected image item");
    assert_eq!(img.filter_quality, ImageFilterQuality::Nearest);
  }

  #[test]
  fn background_image_rendering_crisp_edges_sets_nearest_filter_quality() {
    let url = data_url_for_color([255, 0, 0, 255]);

    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::CrispEdges;
    style.background_color = Rgba::TRANSPARENT;
    style.background_layers = smallvec::smallvec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }];

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let image = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("background image should emit an image item");

    assert_eq!(image.filter_quality, ImageFilterQuality::Nearest);
  }

  #[test]
  fn background_image_rendering_pixelated_sets_nearest_filter_quality() {
    let url = data_url_for_color([255, 0, 0, 255]);

    let mut style = ComputedStyle::default();
    style.image_rendering = ImageRendering::Pixelated;
    style.background_color = Rgba::TRANSPARENT;
    style.background_layers = smallvec::smallvec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      repeat: BackgroundRepeat::no_repeat(),
      ..BackgroundLayer::default()
    }];

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![],
      Arc::new(style),
    );

    let list = DisplayListBuilder::new().build(&fragment);
    let image = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("background image should emit an image item");

    assert_eq!(image.filter_quality, ImageFilterQuality::Nearest);
  }

  #[test]
  fn filters_resolve_font_relative_lengths_in_display_list() {
    let mut style = ComputedStyle::default();
    style.font_size = 20.0;
    style.filter = vec![crate::style::types::FilterFunction::Blur(Length::new(
      1.0,
      LengthUnit::Ex,
    ))];

    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = DisplayListBuilder::resolve_filters(
      &style.filter,
      &style,
      Some((200.0, 100.0)),
      &FontContext::new(),
      &mut resolver,
      None,
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
  fn plus_lighter_mix_blend_mode_converts() {
    assert!(matches!(
      DisplayListBuilder::convert_blend_mode(MixBlendMode::PlusLighter),
      BlendMode::PlusLighter
    ));
  }

  #[test]
  fn unit_interval_filters_clamp_in_display_list() {
    let mut style = ComputedStyle::default();
    style.filter = vec![
      crate::style::types::FilterFunction::Grayscale(2.0),
      crate::style::types::FilterFunction::Sepia(1.5),
      crate::style::types::FilterFunction::Invert(1.3),
      crate::style::types::FilterFunction::Opacity(1.8),
    ];

    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = DisplayListBuilder::resolve_filters(
      &style.filter,
      &style,
      Some((200.0, 100.0)),
      &FontContext::new(),
      &mut resolver,
      None,
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
  fn multiplicative_filters_keep_values_in_display_list() {
    let mut style = ComputedStyle::default();
    style.filter = vec![
      crate::style::types::FilterFunction::Brightness(2.25),
      crate::style::types::FilterFunction::Contrast(1.5),
      crate::style::types::FilterFunction::Saturate(3.75),
    ];

    let mut resolver = SvgFilterResolver::new(None, Vec::new(), None);
    let filters = DisplayListBuilder::resolve_filters(
      &style.filter,
      &style,
      Some((200.0, 100.0)),
      &FontContext::new(),
      &mut resolver,
      None,
    );
    assert_eq!(filters.len(), 3);
    assert!(filters
      .iter()
      .any(|f| matches!(f, ResolvedFilter::Brightness(v) if (*v - 2.25).abs() < 0.001)));
    assert!(filters
      .iter()
      .any(|f| matches!(f, ResolvedFilter::Contrast(v) if (*v - 1.5).abs() < 0.001)));
    assert!(filters
      .iter()
      .any(|f| matches!(f, ResolvedFilter::Saturate(v) if (*v - 3.75).abs() < 0.001)));
  }

  #[test]
  fn alt_text_emitted_when_image_missing() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 12.0;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: String::new(),
          alt: Some("alt text".to_string()),
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(!list.is_empty());
    let text = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Text(text) => Some(text),
        _ => None,
      })
      .expect("Expected text item for alt fallback");
    assert!(text.advance_width > 0.0);
    assert!(
      list.items().iter().any(|item| {
        matches!(item, DisplayItem::FillRect(_))
      }),
      "Expected placeholder items for missing image"
    );
  }

  #[test]
  fn missing_image_alt_text_is_inset_past_icon() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 12.0;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: String::new(),
          alt: Some("alt text".to_string()),
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    let text = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Text(text) => Some(text),
        _ => None,
      })
      .expect("Expected text item for alt fallback");

    // Placeholder icon sizing in `emit_replaced_placeholder` yields a 16px icon with a 2px inset +
    // 2px gap inside a 100×40 image box.
    let expected_x = 20.0;
    assert!(
      (text.origin.x - expected_x).abs() < 0.01,
      "expected missing-image alt text to start at x≈{}, got x={}",
      expected_x,
      text.origin.x
    );
  }

  #[test]
  fn missing_image_alt_text_ignores_text_align() {
    use crate::style::types::TextAlign;

    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 12.0;
    style.text_align = TextAlign::Center;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
      FragmentContent::Replaced {
        box_id: None,
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
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    let text = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Text(text) => Some(text),
        _ => None,
      })
      .expect("Expected text item for alt fallback");

    // Chrome's UA broken-image rendering is always left-aligned within the image box, even when
    // `text-align: center` is inherited.
    let border_x = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::FillRect(fill)
          if fill.color == Rgba::rgb(163, 163, 163)
            && fill.rect == Rect::from_xywh(2.0, 2.0, 16.0, 1.0) =>
        {
          Some(fill.rect.x())
        }
        _ => None,
      })
      .expect("Expected broken-image icon border fill");

    assert!(
      (border_x - 2.0).abs() < 0.01,
      "expected broken-image icon to remain at x=2 when text-align:center, got x={border_x}",
    );
    assert!(
      (text.origin.x - 20.0).abs() < 0.5,
      "expected missing-image alt text to start at x≈20 when text-align:center, got x={}",
      text.origin.x
    );
  }

  #[test]
  fn alt_text_wraps_into_multiple_lines_within_replaced_rect() {
    let mut style = ComputedStyle::default();
    style.color = Rgba::BLACK;
    style.font_size = 12.0;

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 20.0, 40.0),
      FragmentContent::Replaced {
        box_id: None,
        replaced_type: ReplacedType::Image {
          src: String::new(),
          alt: Some("A A A A A A A A A A".to_string()),
          loading: Default::default(),
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
      },
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    let mut baselines: Vec<f32> = list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::Text(text) => Some(text.origin.y),
        _ => None,
      })
      .collect();
    baselines.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    baselines.dedup_by(|a, b| (*a - *b).abs() < 0.1);

    assert!(
      baselines.len() >= 2,
      "expected wrapped alt text to emit multiple baselines; got {baselines:?}"
    );
    assert!(
      baselines.last().unwrap_or(&0.0) - baselines.first().unwrap_or(&0.0) > 6.0,
      "expected baseline separation to be > ~half a line; got {baselines:?}"
    );
  }

  #[test]
  fn missing_image_without_alt_emits_no_placeholder_when_too_small() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 40.0, 10.0),
      ReplacedType::Image {
        src: String::new(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(
      list.items().iter().any(|item| {
        matches!(
          item,
          DisplayItem::FillRect(fill) if fill.rect == Rect::from_xywh(0.0, 0.0, 40.0, 1.0)
        )
      }),
      "expected a broken-image border for missing images even when the icon is too small to draw"
    );
    assert!(
      !list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::FillRect(fill) if fill.color == Rgba::WHITE)),
      "expected broken-image icon to be suppressed for very small boxes"
    );
  }

  #[test]
  fn missing_image_emits_icon_when_large_enough() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
      ReplacedType::Image {
        src: String::new(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(
      list.items().iter().any(|item| {
        matches!(
          item,
          DisplayItem::FillRect(fill)
            if fill.rect == Rect::from_xywh(2.0, 2.0, 16.0, 1.0)
        )
      }),
      "expected broken-image icon border placeholder"
    );
    assert!(list.len() >= 8, "expected broken-image icon items");
    assert!(
      !list.items().iter().any(|item| {
        matches!(
          item,
          DisplayItem::FillRect(fill)
            if fill.rect == Rect::from_xywh(0.0, 0.0, 40.0, 1.0)
        )
      }),
      "missing images should not draw a full-frame border"
    );
  }

  #[test]
  fn video_without_poster_emits_no_placeholder() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 40.0, 20.0),
      ReplacedType::Video {
        src: String::new(),
        poster: None,
        controls: false,
      },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(
      list.is_empty(),
      "video without a poster should paint nothing rather than a placeholder"
    );
  }

  #[test]
  fn video_controls_without_poster_emits_placeholder() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 120.0, 100.0),
      ReplacedType::Video {
        src: String::new(),
        poster: None,
        controls: true,
      },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(
      list.items().iter().any(|item| matches!(item, DisplayItem::FillRect(_))),
      "expected video surface fill"
    );
    assert!(
      list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::LinearGradient(_))),
      "expected control shadow gradient"
    );
  }

  #[test]
  fn video_controls_without_poster_emits_controls_ui() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      ReplacedType::Video {
        src: String::new(),
        poster: None,
        controls: true,
      },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert!(
      list.items().iter().any(|item| {
        matches!(
          item,
          DisplayItem::FillRect(fill)
            if fill.rect == Rect::from_xywh(8.0, 140.0, 184.0, 4.0)
        )
      }),
      "expected a progress-bar track in the video controls placeholder"
    );
    assert!(
      list.items().iter().any(|item| {
        matches!(
          item,
          DisplayItem::FillRoundedRect(fill)
            if fill.rect == Rect::from_xywh(4.0, 138.0, 8.0, 8.0)
        )
      }),
      "expected a progress-bar knob in the video controls placeholder"
    );
  }

  #[test]
  fn audio_replaced_uses_labeled_placeholder() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 30.0, 12.0),
      ReplacedType::Audio { src: String::new() },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert_eq!(list.len(), 3, "audio placeholder should include label text");
    assert!(matches!(list.items()[0], DisplayItem::FillRect(_)));
    assert!(matches!(list.items()[1], DisplayItem::StrokeRect(_)));
    assert!(matches!(list.items()[2], DisplayItem::Text(_)));
  }

  #[test]
  fn video_poster_decodes_before_placeholder() {
    let poster = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"2\"><rect width=\"4\" height=\"2\" fill=\"red\"/></svg>";
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 40.0, 20.0),
      ReplacedType::Video {
        src: String::new(),
        poster: Some(poster.to_string()),
        controls: false,
      },
    );
    let builder = DisplayListBuilder::with_image_cache(ImageCache::new());
    let list = builder.build(&fragment);

    assert_eq!(
      list
        .items()
        .iter()
        .filter(|item| matches!(item, DisplayItem::Image(_)))
        .count(),
      1,
      "poster image should render as content"
    );
    assert!(
      !list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::FillRect(_) | DisplayItem::StrokeRect(_))),
      "poster should not fall back to the labeled placeholder"
    );
  }

  #[test]
  fn transform_box_uses_content_box_for_translate_percentages() {
    let mut style = ComputedStyle::default();
    style.transform_box = TransformBox::ContentBox;
    style.padding_left = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.border_left_width = Length::px(5.0);
    style.border_right_width = Length::px(5.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.border_right_style = crate::style::types::BorderStyle::Solid;
    style.transform.push(Transform::Translate(
      Length::percent(50.0),
      Length::percent(0.0),
    ));

    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 100.0);
    let transform = DisplayListBuilder::build_transform(&style, bounds, None)
      .self_transform
      .expect("transform should build");
    let transform = transform.to_2d().expect("2d transform");

    assert!((transform.e - 85.0).abs() < 1e-3);
    assert!((transform.f).abs() < 1e-3);
  }

  #[test]
  fn transform_box_shifts_origin_to_content_box() {
    let mut style = ComputedStyle::default();
    style.transform_box = TransformBox::ContentBox;
    style.padding_left = Length::px(10.0);
    style.border_left_width = Length::px(5.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.transform_origin = crate::style::types::TransformOrigin {
      x: Length::percent(0.0),
      y: Length::percent(0.0),
      z: Length::px(0.0),
    };
    style.transform.push(Transform::Scale(2.0, 1.0));

    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 100.0);
    let transform = DisplayListBuilder::build_transform(&style, bounds, None)
      .self_transform
      .expect("transform should build");
    let transform = transform.to_2d().expect("2d transform");

    assert!((transform.e + 15.0).abs() < 1e-3);
    assert!((transform.f).abs() < 1e-3);
  }

  #[test]
  fn motion_path_combines_with_existing_transforms() {
    let mut style = ComputedStyle::default();
    style.transform_origin = crate::style::types::TransformOrigin {
      x: Length::percent(0.0),
      y: Length::percent(0.0),
      z: Length::px(0.0),
    };
    style.transform.push(Transform::Scale(2.0, 1.0));
    style.offset_path = OffsetPath::Path(vec![
      MotionPathCommand::MoveTo(MotionPosition {
        x: Length::px(0.0),
        y: Length::px(0.0),
      }),
      MotionPathCommand::LineTo(MotionPosition {
        x: Length::px(100.0),
        y: Length::px(0.0),
      }),
    ]);
    style.offset_anchor = OffsetAnchor::Position {
      x: Length::px(0.0),
      y: Length::px(0.0),
    };
    style.offset_distance = Length::percent(100.0);

    let bounds = Rect::from_xywh(0.0, 0.0, 20.0, 20.0);
    let transform = DisplayListBuilder::build_transform(&style, bounds, None)
      .self_transform
      .expect("transform should build");
    let transform = transform.to_2d().expect("2d transform");

    // Motion path translation composes before the transform list, so scaling affects translation.
    assert!((transform.e - 200.0).abs() < 1e-3);
    assert!((transform.a - 2.0).abs() < 1e-3);
  }

  #[test]
  fn motion_path_composes_with_parent_transform() {
    let mut style = ComputedStyle::default();
    style.transform_origin = crate::style::types::TransformOrigin {
      x: Length::percent(0.0),
      y: Length::percent(0.0),
      z: Length::px(0.0),
    };
    style.transform.push(Transform::Scale(2.0, 1.0));
    style.offset_path = OffsetPath::Path(vec![
      MotionPathCommand::MoveTo(MotionPosition {
        x: Length::px(0.0),
        y: Length::px(0.0),
      }),
      MotionPathCommand::LineTo(MotionPosition {
        x: Length::px(100.0),
        y: Length::px(0.0),
      }),
    ]);
    style.offset_anchor = OffsetAnchor::Position {
      x: Length::px(0.0),
      y: Length::px(0.0),
    };
    style.offset_distance = Length::percent(50.0);

    let bounds = Rect::from_xywh(0.0, 0.0, 20.0, 20.0);
    let child = DisplayListBuilder::build_transform(&style, bounds, None)
      .self_transform
      .expect("transform");
    let parent = Transform3D::translate(25.0, 5.0, 0.0);
    let combined = parent.multiply(&child).to_2d().expect("2d transform");

    assert!((combined.e - 125.0).abs() < 1e-3);
    assert!((combined.f - 5.0).abs() < 1e-3);
    assert!((combined.a - 2.0).abs() < 1e-3);
  }

  #[test]
  fn transform_translate_resolves_calc_components() {
    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    style.root_font_size = 12.0;
    let calc = CalcLength::single(LengthUnit::Percent, 50.0)
      .add_scaled(&CalcLength::single(LengthUnit::Em, 2.0), 1.0)
      .expect("calc terms");
    style.transform.push(Transform::Translate(
      Length::calc(calc),
      Length::percent(0.0),
    ));

    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 100.0);
    let transform = DisplayListBuilder::build_transform(&style, bounds, None)
      .self_transform
      .expect("transform should build");
    let transform = transform.to_2d().expect("2d transform");

    // 50% of 200 = 100; 2em at 10px = 20 -> total 120.
    assert!((transform.e - 120.0).abs() < 1e-3);
    assert!((transform.f).abs() < 1e-3);
  }

  #[test]
  fn matrix3d_transform_preserves_values() {
    let mut style = ComputedStyle::default();
    let values = [
      1.0, 0.0, 0.0, 0.0, // column 1
      0.0, 1.0, 0.0, 0.0, // column 2
      0.0, 0.0, 1.0, 0.0, // column 3
      5.0, 6.0, 0.0, 1.0, // translation
    ];
    style.transform.push(Transform::Matrix3d(values));

    let bounds = Rect::from_xywh(0.0, 0.0, 50.0, 50.0);
    let transform = DisplayListBuilder::build_transform(&style, bounds, None)
      .self_transform
      .expect("matrix3d should build");

    assert_eq!(transform.m[12], 5.0);
    assert_eq!(transform.m[13], 6.0);
    assert_eq!(transform.m[15], 1.0);
    let affine = transform
      .to_2d()
      .expect("matrix3d translation stays affine");
    assert!((affine.e - 5.0).abs() < 1e-6);
    assert!((affine.f - 6.0).abs() < 1e-6);
  }

  #[test]
  fn perspective_property_builds_transform() {
    let mut style = ComputedStyle::default();
    style.perspective = Some(Length::px(500.0));

    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let transforms = DisplayListBuilder::build_transform(&style, bounds, None);

    assert!(
      transforms.self_transform.is_none(),
      "perspective property should not affect self"
    );
    let perspective = transforms
      .child_perspective
      .expect("perspective builds child transform");
    assert!(
      perspective.to_2d().is_none(),
      "perspective should keep 3d components"
    );
    assert!((perspective.m[11] + 1.0 / 500.0).abs() < 1e-6);
  }

  #[test]
  fn background_position_calc_vw_requires_viewport() {
    let pos = BackgroundPosition::Position {
      x: crate::style::types::BackgroundPositionComponent {
        alignment: 0.0,
        offset: Length::calc(CalcLength::single(LengthUnit::Vw, 10.0)),
      },
      y: crate::style::types::BackgroundPositionComponent {
        alignment: 0.0,
        offset: Length::percent(0.0),
      },
    };

    // With a 200px viewport, 10vw = 20px offset along the x-axis.
    let (x, y) = DisplayListBuilder::resolve_background_offset(
      pos,
      100.0,
      100.0,
      0.0,
      0.0,
      16.0,
      16.0,
      Some((200.0, 100.0)),
    );
    assert!((x - 20.0).abs() < 0.01);
    assert!((y - 0.0).abs() < 0.01);

    // Without a viewport, viewport-relative calc stays unresolved and falls back to zero.
    let (x, y) =
      DisplayListBuilder::resolve_background_offset(pos, 100.0, 100.0, 0.0, 0.0, 16.0, 16.0, None);
    assert!((x - 0.0).abs() < 0.01);
    assert!((y - 0.0).abs() < 0.01);
  }

  #[cfg(debug_assertions)]
  #[test]
  fn underline_exclusions_reuse_cached_faces() {
    let ctx = FontContext::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 14.0;
    let pipeline = ShapingPipeline::new();

    let runs = match pipeline.shape("underline cache check", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs.is_empty() {
      return;
    }

    let _guard = face_cache::FaceParseCountGuard::start();
    for _ in 0..3 {
      let _ = collect_underline_exclusions(&runs, 0.0, -2.0, 2.0, false, 0.0);
    }

    assert!(
      face_cache::face_parse_count() <= 1,
      "underline exclusions should reuse cached faces"
    );
  }

  #[test]
  fn underline_exclusions_apply_run_scale() {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = crate::text::font_db::LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: crate::text::font_db::FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let (ch, glyph_id, bbox) = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find_map(|ch| {
        let glyph_id = face.glyph_index(ch)?;
        let bbox = face.glyph_bounding_box(glyph_id)?;
        Some((ch, glyph_id.0 as u32, bbox))
      })
      .expect("expected at least one ASCII glyph with bbox in test font");
    let units_per_em = face.units_per_em() as f32;
    assert!(units_per_em > 0.0);

    let font_size = 20.0;
    let run_scale = 0.5;
    let scale = font_size / units_per_em * run_scale;
    let expected_width = (bbox.x_max as f32 - bbox.x_min as f32) * scale + 1.0; // tolerance = 0.5px per side
    let text = ch.to_string();
    let end = text.len();

    let run = ShapedRun {
      text,
      start: 0,
      end,
      glyphs: vec![crate::text::pipeline::GlyphPosition {
        glyph_id,
        cluster: 0,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 0.0,
        y_advance: 0.0,
      }],
      direction: crate::text::pipeline::Direction::LeftToRight,
      level: 0,
      advance: 0.0,
      font: Arc::new(font),
      font_size,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: crate::text::pipeline::RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: run_scale,
    };

    let intervals = collect_underline_exclusions(&[run], 0.0, -1.0, 1.0, true, 0.0);
    assert_eq!(intervals.len(), 1);
    let width = intervals[0].1 - intervals[0].0;
    assert!(
      (width - expected_width).abs() < 0.01,
      "expected exclusion width ~{} for glyph '{}', got {}",
      expected_width,
      ch,
      width
    );
  }

  #[test]
  fn backface_hidden_skips_display_items() {
    let mut style = ComputedStyle::default();
    style.backface_visibility = BackfaceVisibility::Hidden;
    style.transform.push(Transform::RotateY(180.0));
    style.background_color = Rgba::BLACK;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let tree = FragmentTree::new(fragment);
    let list = builder.build_tree_with_stacking(&tree);

    assert!(
      !list.is_empty(),
      "display items should be retained (backface culling happens at paint time)"
    );

    let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
      .expect("renderer")
      .render(&list)
      .expect("render");
    assert!(
      (0..pixmap.height())
        .flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
        .all(|(x, y)| {
          let px = pixmap.pixel(x, y).expect("pixel in bounds");
          px.red() == 255 && px.green() == 255 && px.blue() == 255 && px.alpha() == 255
        }),
      "backface hidden element should be culled during rendering"
    );
  }

  #[test]
  fn backface_hidden_with_perspective_still_culls() {
    let mut style = ComputedStyle::default();
    style.backface_visibility = BackfaceVisibility::Hidden;
    style.perspective = Some(Length::px(400.0));
    style.transform.push(Transform::RotateX(190.0));
    style.background_color = Rgba::BLACK;

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      Arc::new(style),
    );

    let builder = DisplayListBuilder::new();
    let tree = FragmentTree::new(fragment);
    let list = builder.build_tree_with_stacking(&tree);

    assert!(
      !list.is_empty(),
      "display items should be retained (backface culling happens at paint time)"
    );

    let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
      .expect("renderer")
      .render(&list)
      .expect("render");
    assert!(
      (0..pixmap.height())
        .flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
        .all(|(x, y)| {
          let px = pixmap.pixel(x, y).expect("pixel in bounds");
          px.red() == 255 && px.green() == 255 && px.blue() == 255 && px.alpha() == 255
        }),
      "backface should be culled with perspective"
    );
  }

  fn legacy_tile_positions(
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
        let start = DisplayListBuilder::aligned_start(area_start + offset, tile_len, clip_min);
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
          let centered = area_start + offset + (area_len - tile_len) * 0.5;
          vec![centered]
        }
      }
    }
  }

  #[test]
  fn tile_axis_plan_matches_legacy_tile_positions() {
    let cases = [
      (
        BackgroundRepeatKeyword::NoRepeat,
        0.0,
        100.0,
        10.0,
        5.0,
        0.0,
        100.0,
      ),
      (
        BackgroundRepeatKeyword::Repeat,
        0.0,
        100.0,
        10.0,
        0.0,
        25.0,
        75.0,
      ),
      (
        BackgroundRepeatKeyword::Round,
        0.0,
        100.0,
        10.0,
        0.0,
        -5.0,
        35.0,
      ),
      (
        BackgroundRepeatKeyword::Space,
        0.0,
        100.0,
        30.0,
        0.0,
        0.0,
        100.0,
      ),
      (
        BackgroundRepeatKeyword::Space,
        0.0,
        20.0,
        30.0,
        0.0,
        0.0,
        100.0,
      ),
    ];

    for (repeat, area_start, area_len, tile_len, offset, clip_min, clip_max) in cases {
      let legacy = legacy_tile_positions(
        repeat, area_start, area_len, tile_len, offset, clip_min, clip_max,
      );
      let plan = DisplayListBuilder::tile_axis_plan(
        repeat, area_start, area_len, tile_len, offset, clip_min, clip_max,
      );
      let planned: Vec<f32> = plan.iter().collect();
      assert_eq!(planned, legacy, "repeat={repeat:?}");
    }
  }

  #[test]
  fn background_repeat_x_uses_pattern_fast_path_when_y_fits_in_origin_tile() {
    // Background-repeat: repeat-x + a tile taller than the painted area (so only one Y tile can
    // contribute) should be emitted as a single ImagePattern item. This reduces per-tile sampling
    // seams and avoids O(N) display items for common underline textures.
    let url = data_url_for_color([0, 0, 0, 255]);

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      repeat: BackgroundRepeat {
        x: BackgroundRepeatKeyword::Repeat,
        y: BackgroundRepeatKeyword::NoRepeat,
      },
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(10.0)),
        BackgroundSizeComponent::Length(Length::px(40.0)),
      ),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 18.0),
      vec![],
      Arc::new(style),
    );

    let tree = FragmentTree::new(fragment);
    let list = DisplayListBuilder::new().build_tree_with_stacking(&tree);

    let patterns = list
      .iter()
      .filter(|item| matches!(item, DisplayItem::ImagePattern(_)))
      .count();
    let images = list.iter().filter(|item| matches!(item, DisplayItem::Image(_))).count();

    assert_eq!(patterns, 1, "expected a single pattern fill");
    assert_eq!(images, 0, "expected pattern fast path to avoid per-tile images");
  }

  #[test]
  fn background_repeat_x_does_not_use_pattern_when_origin_tile_does_not_cover_clip() {
    // When the origin tile begins inside the clip region, `repeat-y` would become visible if we
    // emitted a repeating pattern. Ensure we fall back to the per-tile path.
    let url = data_url_for_color([0, 0, 0, 255]);

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      repeat: BackgroundRepeat {
        x: BackgroundRepeatKeyword::Repeat,
        y: BackgroundRepeatKeyword::NoRepeat,
      },
      position: BackgroundPosition::Position {
        x: crate::style::types::BackgroundPositionComponent {
          alignment: 0.0,
          offset: Length::percent(0.0),
        },
        y: crate::style::types::BackgroundPositionComponent {
          alignment: 0.0,
          offset: Length::px(10.0),
        },
      },
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(10.0)),
        BackgroundSizeComponent::Length(Length::px(40.0)),
      ),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 18.0),
      vec![],
      Arc::new(style),
    );

    let tree = FragmentTree::new(fragment);
    let list = DisplayListBuilder::new().build_tree_with_stacking(&tree);

    let patterns = list
      .iter()
      .filter(|item| matches!(item, DisplayItem::ImagePattern(_)))
      .count();
    let images = list.iter().filter(|item| matches!(item, DisplayItem::Image(_))).count();

    assert_eq!(patterns, 0, "expected per-tile path (no ImagePattern)");
    assert!(images > 0, "expected background to be emitted as Image tile(s)");
  }

  #[test]
  fn background_repeat_both_axes_uses_single_image_when_only_one_tile_is_visible() {
    // When the resolved tiling plan would paint exactly one tile, emitting an Image item matches the
    // repeat semantics without going through the ImagePattern renderer.
    let url = data_url_for_color([0, 0, 0, 255]);

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(BackgroundImageUrl::new(url))),
      repeat: BackgroundRepeat {
        x: BackgroundRepeatKeyword::Repeat,
        y: BackgroundRepeatKeyword::Repeat,
      },
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(100.0)),
        BackgroundSizeComponent::Length(Length::px(18.0)),
      ),
      ..BackgroundLayer::default()
    }]);

    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 18.0),
      vec![],
      Arc::new(style),
    );

    let tree = FragmentTree::new(fragment);
    let list = DisplayListBuilder::new().build_tree_with_stacking(&tree);

    let patterns = list
      .iter()
      .filter(|item| matches!(item, DisplayItem::ImagePattern(_)))
      .count();
    let images = list.iter().filter(|item| matches!(item, DisplayItem::Image(_))).count();

    assert_eq!(patterns, 0, "expected no ImagePattern for a single visible tile");
    assert_eq!(images, 1, "expected exactly one Image item");
  }

  #[test]
  fn background_tiling_aborts_on_deadline() {
    // Make each deadline check take ~10ms so we can deterministically force the render deadline to
    // expire mid-way through background tiling (as opposed to at fragment traversal boundaries).
    //
    // Note: avoid using the global `set_test_render_delay_ms` override here. That value is
    // process-wide and can interfere with unrelated deadline tests when Rust runs unit tests in
    // parallel.
    let delay = Duration::from_millis(10);
    let cancel_callback = Arc::new(move || {
      std::thread::sleep(delay);
      false
    });
    let deadline = RenderDeadline::new(Some(Duration::from_millis(35)), Some(cancel_callback));

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLACK),
            position: Some(crate::css::types::ColorStopPosition::Fraction(0.0)),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::WHITE),
            position: Some(crate::css::types::ColorStopPosition::Fraction(1.0)),
          },
        ],
      }),
      repeat: BackgroundRepeat {
        x: BackgroundRepeatKeyword::Repeat,
        // Use a non-(repeat|round) keyword in one axis so we take the slower tiling path instead of
        // emitting a single pattern item.
        y: BackgroundRepeatKeyword::Space,
      },
      size: BackgroundSize::Explicit(
        BackgroundSizeComponent::Length(Length::px(1.0)),
        BackgroundSizeComponent::Length(Length::px(1.0)),
      ),
      ..BackgroundLayer::default()
    }]);
    let fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 256.0, 256.0),
      vec![],
      Arc::new(style),
    );

    let result = with_deadline(Some(&deadline), || {
      DisplayListBuilder::new().build_with_stacking_tree_offset_checked(&fragment, Point::ZERO)
    });

    assert!(matches!(
      result,
      Err(Error::Render(RenderError::Timeout {
        stage: RenderStage::Paint,
        ..
      }))
    ));
  }

  fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let px = pixmap.pixel(x, y).expect("pixel inside viewport");
    (px.red(), px.green(), px.blue(), px.alpha())
  }

  #[test]
  fn clip_rect_clips_stacking_context_children() {
    // Regression coverage for the stacking-context clip chain:
    //
    // `clip: rect(...)` establishes a clipping scope for descendants without creating a stacking
    // context. When a descendant does create a stacking context, it is promoted to the nearest
    // ancestor stacking context during painting, but the clip must still apply.

    let mut parent_style = ComputedStyle::default();
    parent_style.position = Position::Absolute;
    parent_style.clip = Some(ClipRect {
      top: ClipComponent::Length(Length::px(0.0)),
      right: ClipComponent::Length(Length::px(4.0)),
      bottom: ClipComponent::Length(Length::px(4.0)),
      left: ClipComponent::Length(Length::px(0.0)),
    });
    let parent_style = Arc::new(parent_style);

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Relative;
    child_style.z_index = Some(1);
    child_style.background_color = Rgba::RED;
    let child_style = Arc::new(child_style);

    let child =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 6.0, 6.0), vec![], child_style);
    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 6.0, 6.0),
      vec![child],
      parent_style,
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let pixmap = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
      .unwrap()
      .render(&list)
      .unwrap();

    // Pixel inside the clip rect paints red.
    assert_eq!(pixel(&pixmap, 2, 2), (255, 0, 0, 255));
    // Pixel inside the child but outside the clip rect stays white.
    assert_eq!(pixel(&pixmap, 5, 2), (255, 255, 255, 255));
  }

  #[test]
  fn clip_rect_is_ignored_unless_absolutely_positioned() {
    // CSS 2.1 `clip` only applies to absolutely positioned elements (absolute/fixed). When authors
    // specify `clip` on other positioning schemes the used value should be `auto` (no clip).

    let mut parent_style = ComputedStyle::default();
    parent_style.position = Position::Relative;
    parent_style.clip = Some(ClipRect {
      top: ClipComponent::Length(Length::px(0.0)),
      right: ClipComponent::Length(Length::px(2.0)),
      bottom: ClipComponent::Length(Length::px(4.0)),
      left: ClipComponent::Length(Length::px(0.0)),
    });
    let parent_style = Arc::new(parent_style);

    let mut child_style = ComputedStyle::default();
    child_style.background_color = Rgba::RED;
    let child_style = Arc::new(child_style);

    let child =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 4.0, 4.0), vec![], child_style);

    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(2.0, 2.0, 4.0, 4.0),
      vec![child],
      parent_style,
    );

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
    let pixmap = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
      .unwrap()
      .render(&list)
      .unwrap();

    // Outside the parent box remains white.
    assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));
    // Inside the child paints red (clip ignored because parent isn't absolute/fixed).
    assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
    // This pixel would be clipped if `clip` incorrectly applied to relative positioning.
    assert_eq!(pixel(&pixmap, 5, 3), (255, 0, 0, 255));
  }
}
