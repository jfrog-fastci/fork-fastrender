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
use crate::layout::utils::{resolve_font_relative_length, resolve_scrollbar_width};
use crate::math::{layout_mathml, MathFragment};
use crate::paint::clip_path::resolve_clip_path;
use crate::paint::display_list::BlendMode;
use crate::paint::display_list::BlendModeItem;
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
use crate::paint::display_list::MaskReferenceRects;
use crate::paint::display_list::OpacityItem;
use crate::paint::display_list::OutlineItem;
use crate::paint::display_list::RadialGradientItem;
use crate::paint::display_list::RadialGradientPatternItem;
use crate::paint::display_list::ResolvedFilter;
use crate::paint::display_list::ResolvedMask;
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
use crate::paint::display_list_renderer::{PaintParallelism, PaintParallelismMode};
use crate::paint::filter_outset::filter_outset_with_bounds;
use crate::paint::iframe::{render_iframe_src, render_iframe_srcdoc};
use crate::paint::object_fit::compute_object_fit;
use crate::paint::object_fit::default_object_position;
use crate::paint::painter::{
  paint_diagnostics_enabled, paint_diagnostics_session_id, with_paint_diagnostics,
  PaintDiagnosticsThreadGuard,
};
use crate::paint::stacking::Layer6Item;
use crate::paint::stacking::ClipChainLink;
use crate::paint::stacking::StackingContext;
use crate::paint::svg_filter::SvgFilterResolver;
use crate::paint::text_decoration::{resolve_underline_side, UnderlineSide};
use crate::paint::text_shadow::resolve_text_shadows_with_viewport;
use crate::paint::transform_resolver::ResolvedTransforms;
use crate::render_control::{
  active_deadline, active_stage, check_active, check_active_periodic, with_deadline, StageGuard,
};
use crate::scroll::ScrollState;
use crate::style::block_axis_is_horizontal;
use crate::style::block_axis_positive;
use crate::style::color::Rgba;
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
use crate::style::types::MaskMode;
use crate::style::types::MixBlendMode;
use crate::style::types::ObjectFit;
use crate::style::types::OrientationTransform;
use crate::style::types::ResolvedTextDecoration;
use crate::style::types::TextDecorationLine;
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
use crate::text::font_db::FontStretch;
use crate::text::font_db::FontStyle;
use crate::text::font_db::ScaledMetrics;
use crate::text::font_loader::FontContext;
use crate::text::pipeline::ShapedRun;
use crate::text::pipeline::ShapingPipeline;
use crate::tree::box_tree::CrossOriginAttribute;
use crate::tree::box_tree::FormControl;
use crate::tree::box_tree::FormControlKind;
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
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

const DECODED_IMAGE_CACHE_MAX_ENTRIES: usize = 256;
const DECODED_IMAGE_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const DEADLINE_STRIDE: usize = 256;

/// Builder that converts a fragment tree to a display list
///
/// Walks the fragment tree depth-first, emitting display items
/// for backgrounds, borders, and content in correct CSS paint order.
pub struct DisplayListBuilder {
  /// The display list being built
  list: DisplayList,
  image_cache: Option<ImageCache>,
  decoded_image_cache: Arc<Mutex<DecodedImageCache>>,
  /// Serialized SVG filter definitions collected from the document DOM.
  svg_filter_defs: Option<Arc<HashMap<String, String>>>,
  /// Serialized SVG defs (by id) collected from the document DOM.
  svg_id_defs: Option<Arc<HashMap<String, String>>>,
  viewport: Option<(f32, f32)>,
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
  estimated_fragments: Option<usize>,
  scroll_state: ScrollState,
  max_iframe_depth: usize,
  skip_stacking_context_children: bool,
  error: Option<RenderError>,
}

#[derive(Clone, Copy)]
struct BackgroundRects {
  border: Rect,
  padding: Rect,
  content: Rect,
}

#[derive(Clone)]
struct RootBackground {
  paint_rect: Rect,
  origin_rect: Rect,
  style: Arc<ComputedStyle>,
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

  fn estimate_bytes(image: &ImageData) -> usize {
    image.pixels.len()
  }

  fn get(&mut self, key: &ImageKey) -> Option<Arc<ImageData>> {
    self.inner.get(key).map(|entry| Arc::clone(&entry.image))
  }

  fn insert(&mut self, key: ImageKey, image: Arc<ImageData>) {
    let bytes = Self::estimate_bytes(&image);
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
  let (min, explicit_min) = match std::env::var("FASTR_DISPLAY_LIST_PARALLEL_MIN") {
    Ok(raw) => (raw.parse::<usize>().unwrap_or(32).max(1), true),
    Err(_) => (
      toggles
        .usize_with_default("FASTR_DISPLAY_LIST_PARALLEL_MIN", 32)
        .max(1),
      false,
    ),
  };
  (enabled, min, explicit_min)
}

fn paint_build_breakdown_enabled() -> bool {
  paint_diagnostics_enabled() && runtime::runtime_toggles().truthy("FASTR_PAINT_BUILD_BREAKDOWN")
}

impl DisplayListBuilder {
  fn viewport_rect(&self) -> Option<Rect> {
    self
      .viewport
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
    if let Some(err) = self.error.take() {
      Err(Error::Render(err))
    } else {
      Ok(self.list)
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

  /// Creates a new display list builder
  pub fn new() -> Self {
    let (parallel_enabled, parallel_min, parallel_min_explicit) = parallel_config_from_env();
    let parallel_root_min = if parallel_min_explicit {
      parallel_min
    } else {
      parallel_min.saturating_mul(4)
    };
    Self {
      list: DisplayList::new(),
      image_cache: Some(ImageCache::new()),
      decoded_image_cache: Arc::new(Mutex::new(DecodedImageCache::new(
        DECODED_IMAGE_CACHE_MAX_ENTRIES,
        DECODED_IMAGE_CACHE_MAX_BYTES,
      ))),
      svg_filter_defs: None,
      svg_id_defs: None,
      viewport: None,
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
      estimated_fragments: None,
      scroll_state: ScrollState::default(),
      max_iframe_depth: DEFAULT_MAX_IFRAME_DEPTH,
      skip_stacking_context_children: false,
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
    Self {
      list: DisplayList::new(),
      image_cache: Some(image_cache),
      decoded_image_cache: Arc::new(Mutex::new(DecodedImageCache::new(
        DECODED_IMAGE_CACHE_MAX_ENTRIES,
        DECODED_IMAGE_CACHE_MAX_BYTES,
      ))),
      svg_filter_defs: None,
      svg_id_defs: None,
      viewport: None,
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
      estimated_fragments: None,
      scroll_state: ScrollState::default(),
      max_iframe_depth: DEFAULT_MAX_IFRAME_DEPTH,
      skip_stacking_context_children: false,
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

  /// Sets the scroll state used when translating element content during display list construction.
  pub fn with_scroll_state(mut self, scroll_state: ScrollState) -> Self {
    self.scroll_state = scroll_state;
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

    let stacking_tree_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let contexts = crate::paint::stacking::build_stacking_tree_from_tree_checked(tree)?;
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), stacking_tree_timer) {
      breakdown.record_stacking_tree(start.elapsed());
    }
    let visibility = self.root_visibility();
    for context in &contexts {
      let _ = self.build_stacking_context(context, Point::ZERO, true, &mut svg_filters, visibility);
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
      self.viewport = Some((stacking.bounds.width(), stacking.bounds.height()));
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
    let stacking = crate::paint::stacking::build_stacking_tree_from_fragment_tree_checked(root)?;
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), stacking_tree_timer) {
      breakdown.record_stacking_tree(start.elapsed());
    }
    let image_cache = self.image_cache.clone();
    let mut svg_filters = SvgFilterResolver::new(
      self.svg_filter_defs.clone(),
      vec![root],
      image_cache.as_ref(),
    );
    let _ = self.build_stacking_context(
      &stacking,
      Point::ZERO,
      true,
      &mut svg_filters,
      self.root_visibility(),
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
    let stacking = crate::paint::stacking::build_stacking_tree_from_fragment_tree_checked(root)?;
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), stacking_tree_timer) {
      breakdown.record_stacking_tree(start.elapsed());
    }
    let mut svg_roots = Vec::new();
    Self::collect_stacking_fragments(&stacking, &mut svg_roots);
    let image_cache = self.image_cache.clone();
    let mut svg_filters = SvgFilterResolver::new(
      self.svg_filter_defs.clone(),
      svg_roots,
      image_cache.as_ref(),
    );
    let _ = self.build_stacking_context(
      &stacking,
      offset,
      true,
      &mut svg_filters,
      self.root_visibility(),
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
      if let Some(first) = stackings.first() {
        self.viewport = Some((first.bounds.width(), first.bounds.height()));
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
      let _ =
        self.build_stacking_context(stacking, Point::ZERO, true, &mut svg_filters, visibility);
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
    let stackings = crate::paint::stacking::build_stacking_tree_from_tree_checked(tree)
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
    self.estimate_from_roots(std::iter::once(root));
    self.build_fragment_with_clips(root, Point::ZERO, clips, self.root_visibility());
    self.finish()
  }

  fn collect_stacking_fragments<'a>(context: &'a StackingContext, out: &mut Vec<&'a FragmentNode>) {
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
    for child in &context.children {
      Self::collect_stacking_fragments(child, out);
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
        FragmentContent::RunningAnchor { snapshot, .. } | FragmentContent::FootnoteAnchor { snapshot } => {
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

  /// Recursively builds display items for a fragment
  fn build_fragment(&mut self, fragment: &FragmentNode, offset: Point, visibility: Visibility) {
    self.build_fragment_internal(fragment, offset, true, false, visibility);
  }

  /// Builds display items for a fragment without descending into children.
  fn build_fragment_shallow(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    visibility: Visibility,
  ) {
    self.build_fragment_internal(fragment, offset, false, false, visibility);
  }

  fn build_fragment_internal(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    recurse_children: bool,
    suppress_opacity: bool,
    visibility: Visibility,
  ) {
    if self.deadline_reached() {
      return;
    }

    let style_opt = fragment.style.as_deref();
    let paint_self = style_opt.map_or(true, |style| {
      matches!(
        style.visibility,
        crate::style::computed::Visibility::Visible
      )
    });
    if !paint_self && (!recurse_children || fragment.children.is_empty()) {
      return;
    }

    if matches!(
      fragment.content,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
    ) {
      return;
    }

    let list_start = self.list.len();
    let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
    if opacity <= f32::EPSILON {
      return;
    }
    let push_opacity = !suppress_opacity && opacity < 1.0 - f32::EPSILON;

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
    // `fragment_paint_bounds` only accounts for effects on the fragment's own border box.
    // Descendants can paint outside that box (e.g. absolutely positioned children with
    // `overflow: visible`), so use the already-computed `scroll_overflow` to avoid culling away
    // visible descendants.
    paint_bounds = paint_bounds.union(fragment.scroll_overflow.translate(absolute_rect.origin));
    if let Some(vis) = visibility.rect {
      if !vis.intersects(paint_bounds) {
        return;
      }
      if visibility.hard_clip {
        paint_bounds = match paint_bounds.intersection(vis) {
          Some(intersection) if intersection.width() > 0.0 && intersection.height() > 0.0 => {
            intersection
          }
          _ => return,
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
      return;
    }
    if let Some(vis) = child_visibility.rect {
      if !vis.intersects(paint_bounds) {
        return;
      }
      if child_visibility.hard_clip {
        match paint_bounds.intersection(vis) {
          Some(intersection) if intersection.width() > 0.0 && intersection.height() > 0.0 => {}
          _ => return,
        };
      }
    }

    if push_opacity {
      self.push_opacity(opacity);
    }

    if paint_self {
      if let Some(style) = style_opt {
        let suppress_background = self
          .canvas_background_suppress_box_id
          .is_some_and(|id| Self::get_box_id(fragment) == Some(id));
        let (decoration_rect, decoration_clip) =
          Self::decoration_rect_and_clip(fragment, absolute_rect, style);
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
          self.build_text_clip_runs(fragment, offset, child_visibility)
        } else {
          None
        };

        if !style.box_shadow.is_empty() {
          let mut decoration_rects: Option<BackgroundRects> = None;
          let rects = if decoration_rect == absolute_rect {
            absolute_rects
              .get_or_insert_with(|| Self::background_rects(absolute_rect, style, self.viewport))
          } else {
            decoration_rects
              .get_or_insert_with(|| Self::background_rects(decoration_rect, style, self.viewport))
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
            self.emit_background_from_style_with_rects_and_text_clip(
              rects,
              style,
              text_clip.as_ref(),
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
              self.emit_background_from_style_with_rects_and_text_clip(
                rects,
                style,
                text_clip.as_ref(),
              );
            } else {
              self.emit_background_from_style_with_text_clip(
                decoration_rect,
                style,
                text_clip.as_ref(),
              );
            }
          }
        } else if !suppress_background {
          self.emit_background_from_style_with_text_clip(
            decoration_rect,
            style,
            text_clip.as_ref(),
          );
        }

        self.emit_border_from_style(decoration_rect, style);
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

    // Clip descendant/content painting but leave outer effects (e.g., box shadows, outlines)
    // unaffected.
    let mut children_painted = false;
    if !skip_contents {
      let mut pushed_clips = 0;
      if let Some(clip) = overflow_clip {
        self.list.push(DisplayItem::PushClip(clip));
        pushed_clips += 1;
      }
      if let Some(clip) = clip_rect {
        self.list.push(DisplayItem::PushClip(clip));
        pushed_clips += 1;
      }

      if paint_self {
        self.emit_content(fragment, absolute_rect);
      }

      if recurse_children {
        let element_scroll = self.element_scroll_offset(fragment);
        let child_offset = Point::new(
          absolute_rect.origin.x - element_scroll.x,
          absolute_rect.origin.y - element_scroll.y,
        );
        let before_children = self.list.len();
        let mut blocks = Vec::new();
        let mut floats = Vec::new();
        let mut inlines = Vec::new();
        let mut positioned = Vec::new();
        for (idx, child) in fragment.children.iter().enumerate() {
          if self.skip_stacking_context_children {
            if let Some(child_style) = child.style.as_deref() {
              if crate::paint::stacking::creates_stacking_context(child_style, style_opt, false) {
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

        // CSS2.1 stacking levels 3-6: blocks → floats → inlines → positioned.
        let mut counter = 0usize;
        for idx in blocks
          .into_iter()
          .chain(floats)
          .chain(inlines)
          .chain(positioned)
        {
          if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
            break;
          }
          let child = &fragment.children[idx];
          self.build_fragment_internal(child, child_offset, true, false, child_visibility);
        }
        children_painted = self.list.len() != before_children;
      }

      if let Some(table_borders) = fragment.table_borders.as_ref() {
        let origin = absolute_rect.origin;
        let bounds = table_borders.paint_bounds.translate(origin);
        let has_visible_borders = table_borders
          .vertical_borders
          .iter()
          .chain(table_borders.horizontal_borders.iter())
          .chain(table_borders.corner_borders.iter())
          .any(|b| b.is_visible());
        if paint_self && has_visible_borders {
          self.list.push(DisplayItem::TableCollapsedBorders(
            TableCollapsedBordersItem {
              origin,
              bounds,
              borders: table_borders.clone(),
            },
          ));
        }
      }

      for _ in 0..pushed_clips {
        self.list.push(DisplayItem::PopClip);
      }
    }

    if paint_self {
      if let Some(style) = style_opt {
        self.emit_outline(absolute_rect, style);
      }
    }

    if push_opacity {
      self.pop_opacity();
    }

    if !paint_self && !children_painted {
      self.list.items_mut().truncate(list_start);
    }
  }

  /// Recursively builds display items with clipping support
  fn build_fragment_with_clips(
    &mut self,
    fragment: &FragmentNode,
    offset: Point,
    clips: &HashSet<Option<usize>>,
    visibility: Visibility,
  ) {
    if self.deadline_reached() {
      return;
    }
    let style_opt = fragment.style.as_deref();
    let paint_self = style_opt.map_or(true, |style| {
      matches!(
        style.visibility,
        crate::style::computed::Visibility::Visible
      )
    });
    if !paint_self && fragment.children.is_empty() {
      return;
    }

    if matches!(
      fragment.content,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
    ) {
      return;
    }

    let list_start = self.list.len();
    let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
    let push_opacity = opacity < 1.0 - f32::EPSILON;

    let absolute_rect = Rect::new(
      Point::new(
        fragment.bounds.origin.x + offset.x,
        fragment.bounds.origin.y + offset.y,
      ),
      fragment.bounds.size,
    );
    if let Some(vis) = visibility.rect {
      if !vis.intersects(absolute_rect) {
        return;
      }
    }
    let skip_contents =
      fragment
        .style
        .as_deref()
        .is_some_and(|style| match style.content_visibility {
          ContentVisibility::Hidden => true,
          ContentVisibility::Auto => visibility
            .rect
            .is_some_and(|vis| !vis.intersects(absolute_rect)),
          ContentVisibility::Visible => false,
        });
    if push_opacity {
      self.push_opacity(opacity);
    }

    if paint_self {
      if let Some(style) = style_opt {
        let (decoration_rect, decoration_clip) =
          Self::decoration_rect_and_clip(fragment, absolute_rect, style);
        let decoration_clip_pushed = decoration_clip.is_some();
        if let Some(clip) = decoration_clip {
          self.list.push(DisplayItem::PushClip(clip));
        }
        self.emit_background_from_style(decoration_rect, style);
        self.emit_border_from_style(decoration_rect, style);
        if decoration_clip_pushed {
          self.list.push(DisplayItem::PopClip);
        }
      }
    }

    let box_id = Self::get_box_id(fragment);
    let should_clip = clips.contains(&box_id);

    let mut children_painted = false;
    if !skip_contents {
      // Emit content before clipping children
      if paint_self {
        self.emit_content(fragment, absolute_rect);
      }

      // Push clip if needed
      if should_clip {
        self.list.push(DisplayItem::PushClip(ClipItem {
          shape: ClipShape::Rect {
            rect: absolute_rect,
            radii: None,
          },
        }));
      }

      // Recurse to children
      let element_scroll = self.element_scroll_offset(fragment);
      let child_offset = Point::new(
        absolute_rect.origin.x - element_scroll.x,
        absolute_rect.origin.y - element_scroll.y,
      );
      let before_children = self.list.len();
      let mut blocks = Vec::new();
      let mut floats = Vec::new();
      let mut inlines = Vec::new();
      let mut positioned = Vec::new();
      for (idx, child) in fragment.children.iter().enumerate() {
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
      let mut counter = 0usize;
      for idx in blocks
        .into_iter()
        .chain(floats)
        .chain(inlines)
        .chain(positioned)
      {
        if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
          break;
        }
        let child = &fragment.children[idx];
        self.build_fragment_with_clips(child, child_offset, clips, visibility);
      }
      children_painted = self.list.len() != before_children;

      if let Some(table_borders) = fragment.table_borders.as_ref() {
        let origin = absolute_rect.origin;
        let bounds = table_borders.paint_bounds.translate(origin);
        let has_visible_borders = table_borders
          .vertical_borders
          .iter()
          .chain(table_borders.horizontal_borders.iter())
          .chain(table_borders.corner_borders.iter())
          .any(|b| b.is_visible());
        if paint_self && has_visible_borders {
          self.list.push(DisplayItem::TableCollapsedBorders(
            TableCollapsedBordersItem {
              origin,
              bounds,
              borders: table_borders.clone(),
            },
          ));
        }
      }

      // Pop clip
      if should_clip {
        self.list.push(DisplayItem::PopClip);
      }
    }

    if paint_self {
      if let Some(style) = style_opt {
        self.emit_outline(absolute_rect, style);
      }
    }

    if push_opacity {
      self.pop_opacity();
    }

    if !paint_self && !children_painted {
      self.list.items_mut().truncate(list_start);
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

  /// Computes whether a stacking context should be rendered as an *isolated group*.
  ///
  /// This corresponds to [`StackingContextItem::is_isolated`] (CSS Compositing & Blending isolated
  /// groups), and is **not** the same thing as a Filter Effects Level 2 Backdrop Root.
  ///
  /// Rules of thumb:
  /// - Set for explicit `isolation:isolate`.
  /// - Also set for `backdrop-filter` and `mix-blend-mode != normal` (both require group
  ///   compositing), and when we need to confine blend-mode descendants.
  ///
  /// Spec: <https://www.w3.org/TR/compositing-1/#isolatedgroups>
  fn stacking_context_is_isolated(
    mix_blend_mode: BlendMode,
    root_style: Option<&ComputedStyle>,
    has_blend_mode_children: bool,
  ) -> bool {
    mix_blend_mode != BlendMode::Normal
      || root_style
        .map(|style| {
          matches!(style.isolation, Isolation::Isolate) || !style.backdrop_filter.is_empty()
        })
        .unwrap_or(false)
      || has_blend_mode_children
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
    let style_has_backdrop_filter = root_style.is_some_and(|style| !style.backdrop_filter.is_empty());
    let style_has_mask_image = root_style
      .is_some_and(|style| style.mask_layers.iter().any(|layer| layer.image.is_some()));

    is_root
      || style_has_filter
      || !filters.is_empty()
      || has_opacity
      || style_has_mask_image
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
  ) -> bool {
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
    let mask = root_style.and_then(|style| self.resolve_mask(style, root_border_bounds));
    let root_background = if is_root {
      root_fragment.and_then(|fragment| {
        let viewport = self
          .viewport
          .unwrap_or_else(|| (context_bounds.width(), context_bounds.height()));
        if viewport.0 <= 0.0 || viewport.1 <= 0.0 {
          return None;
        }
        let target_w = viewport.0.max(context_bounds.width());
        let target_h = viewport.1.max(context_bounds.height());
        let target_rect =
          Rect::from_xywh(context_bounds.x(), context_bounds.y(), target_w, target_h);
        let (style, suppress_box_id, source_rect) =
          Self::root_background_candidate(fragment, descendant_offset)?;
        if !Self::has_paintable_background(&style) {
          return None;
        }
        // We only need to propagate the canvas background when the source element's border box
        // does not fully cover the paint target.
        let source_covers_target = source_rect.min_x() <= target_rect.min_x()
          && source_rect.min_y() <= target_rect.min_y()
          && source_rect.max_x() >= target_rect.max_x()
          && source_rect.max_y() >= target_rect.max_y();
        if source_covers_target {
          return None;
        }
        self.canvas_background_suppress_box_id = Some(suppress_box_id);
        Some(RootBackground {
          paint_rect: target_rect,
          // When propagating the canvas background, Chrome continues to resolve
          // `background-size:auto` against the *source element's* background positioning area
          // (typically the body's padding box). This can be shorter than the viewport, meaning
          // generated images like gradients repeat to fill the canvas when `background-repeat`
          // is `repeat` (the default).
          origin_rect: source_rect,
          style,
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
      .map(|style| Self::used_transform_style(style))
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
    let mut context_visibility = Visibility {
      rect: Self::visible_in_local_space(visibility.rect, transform.as_ref()),
      hard_clip: visibility.hard_clip,
    };
    if child_perspective.is_none() {
      if let (Some(vis), Some(bounds)) = (visibility.rect, world_bounds) {
        if !vis.intersects(bounds) {
          return false;
        }
      }
    }
    context_visibility = context_visibility.intersect(Some(local_bounds), false);

    let style_has_clip_path = root_style.is_some_and(|style| {
      !matches!(style.clip_path, crate::style::types::ClipPath::None)
    });

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
    if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), clip_path_timer) {
      breakdown.record_clip_path(start.elapsed());
    }
    let clip_rect = root_style
      .and_then(|style| Self::clip_rect_from_style(style, root_border_bounds, Some(viewport)));
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
    if let Some(bounds) = clip_path.as_ref().map(|clip| clip.bounds()) {
      child_visibility = child_visibility.intersect(Some(bounds), true);
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
    let ancestor_clips_pushed = self.push_clip_chain(&context.clip_chain, offset);

    let mut has_backdrop_sensitive_descendants =
      !backdrop_filters.is_empty() || mix_blend_mode != BlendMode::Normal;

    if is_root && !has_effects {
      if let Some(root_background) = root_background.as_ref() {
        self.emit_root_background(root_background);
      }
      if skip_contents {
        self.emit_fragment_list_shallow(
          &context.fragments,
          root_fragment_offset,
          has_opacity,
          local_child_visibility,
        );
        self.pop_clips(ancestor_clips_pushed);
        return has_backdrop_sensitive_descendants;
      }
      let mut deadline_counter = 0usize;
      for child in neg {
        if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
          break;
        }
        has_backdrop_sensitive_descendants |= self.build_stacking_context(
          child,
          descendant_offset,
          false,
          svg_filters,
          local_child_visibility,
        );
      }

      self.emit_fragment_list_shallow(
        &context.fragments,
        root_fragment_offset,
        has_opacity,
        local_child_visibility,
      );
      self.emit_fragment_list(
        &context.layer3_blocks,
        descendant_offset,
        local_child_visibility,
      );
      self.emit_fragment_list(
        &context.layer4_floats,
        descendant_offset,
        local_child_visibility,
      );
      self.emit_fragment_list(
        &context.layer5_inlines,
        descendant_offset,
        local_child_visibility,
      );
      for item in context.layer6_iter() {
        if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
          break;
        }
        match item {
          Layer6Item::Positioned(fragment) => {
            let pushed = self.push_clip_chain(&fragment.clip_chain, descendant_offset);
            self.build_fragment(&fragment.fragment, descendant_offset, local_child_visibility);
            self.pop_clips(pushed);
          }
          Layer6Item::ZeroContext(child) => {
            has_backdrop_sensitive_descendants |= self.build_stacking_context(
              child,
              descendant_offset,
              false,
              svg_filters,
              local_child_visibility,
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
          descendant_offset,
          false,
          svg_filters,
          local_child_visibility,
        );
      }
      self.pop_clips(ancestor_clips_pushed);
      return has_backdrop_sensitive_descendants;
    }

    let needs_layer_bounds = is_isolated
      || mix_blend_mode != BlendMode::Normal
      || !filters.is_empty()
      || !backdrop_filters.is_empty()
      || has_opacity
      || mask.is_some()
      || clip_path.is_some();

    let stacking_push_index = self.list.len();
    self
      .list
      .push(DisplayItem::PushStackingContext(StackingContextItem {
        z_index: context.z_index,
        creates_stacking_context: true,
        is_root,
        establishes_backdrop_root,
        has_backdrop_sensitive_descendants: false,
        bounds: context_bounds,
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
        has_clip_path: style_has_clip_path,
      }));

    let mut pushed_clips = 0;
    if let Some(path) = clip_path {
      self.list.push(DisplayItem::PushClip(ClipItem {
        shape: ClipShape::Path { path },
      }));
      pushed_clips += 1;
    }
    if let Some(clip) = clip_rect {
      self.list.push(DisplayItem::PushClip(clip));
      pushed_clips += 1;
    }

    if let Some(root_background) = root_background.as_ref() {
      self.emit_root_background(root_background);
    }
    // Paint the stacking context root (backgrounds, borders, shadows) before applying overflow
    // clipping so outer effects remain visible.
    self.emit_fragment_list_shallow(
      &context.fragments,
      root_fragment_offset,
      has_opacity,
      local_child_visibility,
    );
    if skip_contents {
      for _ in 0..pushed_clips {
        self.list.push(DisplayItem::PopClip);
      }
      if needs_layer_bounds {
        self.expand_stacking_context_bounds_from_items(stacking_push_index, context_bounds);
      }
      self.list.push(DisplayItem::PopStackingContext);
      if let Some(DisplayItem::PushStackingContext(item)) =
        self.list.items_mut().get_mut(stacking_push_index)
      {
        item.has_backdrop_sensitive_descendants = has_backdrop_sensitive_descendants;
      }
      self.pop_clips(ancestor_clips_pushed);
      return has_backdrop_sensitive_descendants;
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
        descendant_offset,
        false,
        svg_filters,
        local_child_visibility,
      );
    }
    self.emit_fragment_list(
      &context.layer3_blocks,
      descendant_offset,
      local_child_visibility,
    );
    self.emit_fragment_list(
      &context.layer4_floats,
      descendant_offset,
      local_child_visibility,
    );
    self.emit_fragment_list(
      &context.layer5_inlines,
      descendant_offset,
      local_child_visibility,
    );
    for item in context.layer6_iter() {
      if self.deadline_reached_periodic(&mut deadline_counter, DEADLINE_STRIDE) {
        break;
      }
      match item {
        Layer6Item::Positioned(fragment) => {
          let pushed = self.push_clip_chain(&fragment.clip_chain, descendant_offset);
          self.build_fragment(&fragment.fragment, descendant_offset, local_child_visibility);
          self.pop_clips(pushed);
        }
        Layer6Item::ZeroContext(child) => {
          has_backdrop_sensitive_descendants |= self.build_stacking_context(
            child,
            descendant_offset,
            false,
            svg_filters,
            local_child_visibility,
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
        descendant_offset,
        false,
        svg_filters,
        local_child_visibility,
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
      self.expand_stacking_context_bounds_from_items(stacking_push_index, context_bounds);
    }
    self.list.push(DisplayItem::PopStackingContext);
    if let Some(DisplayItem::PushStackingContext(item)) =
      self.list.items_mut().get_mut(stacking_push_index)
    {
      item.has_backdrop_sensitive_descendants = has_backdrop_sensitive_descendants;
    }
    self.pop_clips(ancestor_clips_pushed);
    has_backdrop_sensitive_descendants
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

    if let Some(DisplayItem::PushStackingContext(sc)) =
      self.list.items_mut().get_mut(push_index)
    {
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
    breakdown: Option<&BuildBreakdown>,
  ) -> Option<ClipItem> {
    let mut clip_rect = rects.padding;
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
      let radii =
        Self::resolve_clip_radii(style, rects, BackgroundBox::PaddingBox, viewport, breakdown);
      (!radii.is_zero()).then_some(radii)
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

    let rect = Rect::from_xywh(left, top, right - left, bottom - top);
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      None
    } else {
      Some(ClipItem {
        shape: ClipShape::Rect { rect, radii: None },
      })
    }
  }

  fn used_transform_style(style: &ComputedStyle) -> TransformStyle {
    if Self::is_3d_flattening_boundary(style) {
      TransformStyle::Flat
    } else {
      style.transform_style
    }
  }

  fn is_3d_flattening_boundary(style: &ComputedStyle) -> bool {
    if !style.filter.is_empty() || !style.backdrop_filter.is_empty() {
      return true;
    }
    if style.opacity < 1.0 - f32::EPSILON {
      return true;
    }
    if !matches!(style.clip_path, crate::style::types::ClipPath::None) {
      return true;
    }
    if style.clip.is_some() {
      return true;
    }
    if Self::overflow_axis_clips(style.overflow_x) || Self::overflow_axis_clips(style.overflow_y) {
      return true;
    }
    if style.mask_layers.iter().any(|layer| layer.image.is_some()) {
      return true;
    }
    if !matches!(style.mix_blend_mode, MixBlendMode::Normal) {
      return true;
    }
    if matches!(style.isolation, Isolation::Isolate) {
      return true;
    }
    if style.containment.isolates_paint() {
      return true;
    }
    false
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

  fn decode_mask_image_url(
    &self,
    src: &str,
    style: &ComputedStyle,
    bounds: Rect,
  ) -> Option<Arc<ImageData>> {
    let trimmed = src.trim();
    if let Some(id) = trimmed.strip_prefix('#') {
      if let Some(svg) = self.inline_svg_for_svg_mask(id, bounds) {
        return self.decode_image(&svg, Some(style), true, CrossOriginAttribute::None, None, false);
      }
      // Fragment-only URLs (`url(#id)`) refer to in-document SVG resources. If we cannot resolve
      // the id via `svg_id_defs`, treat the mask image as missing instead of attempting an
      // external fetch.
      return None;
    }
    self.decode_image(src, Some(style), true, CrossOriginAttribute::None, None, false)
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
            let trimmed = src.trim_start();
            if trimmed.starts_with('#') {
              // Fragment-only URLs are resolved via `svg_id_defs` (see `decode_mask_image_url`).
              // They do not correspond to fetchable external images, so skip probing the network
              // when selecting the mask mode.
              MaskMode::Alpha
            } else if trimmed.starts_with('<') {
              // Inline SVG is rasterized to RGBA; treat it as alpha-masked.
              MaskMode::Alpha
            } else if let Some(image_cache) = self.image_cache.as_ref() {
              let resolved_src = image_cache.resolve_url(src);
              match image_cache.load_with_crossorigin(&resolved_src, CrossOriginAttribute::None) {
                Ok(cached) => {
                  if cached.image.color().has_alpha() {
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
      color: style.color,
      font_size: style.font_size,
      root_font_size: style.root_font_size,
      viewport: self.viewport,
      rects,
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

  fn normalize_color_stops(stops: &[ColorStop], current_color: Rgba) -> Vec<(f32, Rgba)> {
    if stops.is_empty() {
      return Vec::new();
    }

    let mut positions: Vec<Option<f32>> = stops.iter().map(|s| s.position).collect();
    if positions.iter().all(|p| p.is_none()) {
      if stops.len() == 1 {
        return vec![(0.0, stops[0].color.to_rgba(current_color))];
      }
      let denom = (stops.len() - 1) as f32;
      return stops
        .iter()
        .enumerate()
        .map(|(i, s)| (i as f32 / denom, s.color.to_rgba(current_color)))
        .collect();
    }

    if positions.first().and_then(|p| *p).is_none() {
      positions[0] = Some(0.0);
    }
    if positions.last().and_then(|p| *p).is_none() {
      if let Some(last) = positions.last_mut() {
        *last = Some(1.0);
      }
    }

    let mut last_known: Option<(usize, f32)> = None;
    for i in 0..positions.len() {
      if let Some(pos) = positions[i] {
        if let Some((start_idx, start_pos)) = last_known {
          let gap = i.saturating_sub(start_idx + 1);
          if gap > 0 {
            let step = (pos - start_pos) / (gap as f32 + 1.0);
            for (offset, slot) in positions[start_idx + 1..i].iter_mut().enumerate() {
              *slot = Some((start_pos + step * (offset + 1) as f32).max(start_pos));
            }
          }
        } else if i > 0 {
          let gap = i;
          let step = pos / gap as f32;
          for (j, slot) in positions.iter_mut().take(i).enumerate() {
            *slot = Some(step * j as f32);
          }
        }
        last_known = Some((i, pos));
      }
    }

    let mut output = Vec::with_capacity(stops.len());
    let mut prev = 0.0;
    for (idx, pos_opt) in positions.into_iter().enumerate() {
      let pos = pos_opt.unwrap_or(prev);
      let clamped = pos.max(prev).clamp(0.0, 1.0);
      prev = clamped;
      output.push((clamped, stops[idx].color.to_rgba(current_color)));
    }

    output
  }

  fn normalize_color_stops_unclamped(stops: &[ColorStop], current_color: Rgba) -> Vec<(f32, Rgba)> {
    if stops.is_empty() {
      return Vec::new();
    }

    let mut positions: Vec<Option<f32>> = stops.iter().map(|s| s.position).collect();
    if positions.iter().all(|p| p.is_none()) {
      if stops.len() == 1 {
        return vec![(0.0, stops[0].color.to_rgba(current_color))];
      }
      let denom = (stops.len() - 1) as f32;
      return stops
        .iter()
        .enumerate()
        .map(|(i, s)| (i as f32 / denom, s.color.to_rgba(current_color)))
        .collect();
    }

    if positions.first().and_then(|p| *p).is_none() {
      positions[0] = Some(0.0);
    }
    if positions.last().and_then(|p| *p).is_none() {
      if let Some(last) = positions.last_mut() {
        *last = Some(1.0);
      }
    }

    let mut last_known: Option<(usize, f32)> = None;
    for i in 0..positions.len() {
      if let Some(pos) = positions[i] {
        if let Some((start_idx, start_pos)) = last_known {
          let gap = i.saturating_sub(start_idx + 1);
          if gap > 0 {
            let step = (pos - start_pos) / (gap as f32 + 1.0);
            for (offset, slot) in positions[start_idx + 1..i].iter_mut().enumerate() {
              *slot = Some(start_pos + step * (offset + 1) as f32);
            }
          }
        } else if i > 0 {
          let gap = i;
          let step = pos / gap as f32;
          for (j, slot) in positions.iter_mut().take(i).enumerate() {
            *slot = Some(step * j as f32);
          }
        }
        last_known = Some((i, pos));
      }
    }

    let mut output = Vec::with_capacity(stops.len());
    let mut prev = 0.0;
    for (idx, pos_opt) in positions.into_iter().enumerate() {
      let pos = pos_opt.unwrap_or(prev);
      let monotonic = pos.max(prev);
      prev = monotonic;
      output.push((monotonic, stops[idx].color.to_rgba(current_color)));
    }
    output
  }

  fn gradient_stops(stops: &[(f32, Rgba)]) -> Vec<GradientStop> {
    stops
      .iter()
      .map(|(pos, color)| GradientStop {
        position: pos.clamp(0.0, 1.0),
        color: *color,
      })
      .collect()
  }

  fn gradient_stops_unclamped(stops: &[(f32, Rgba)]) -> Vec<GradientStop> {
    stops
      .iter()
      .map(|(pos, color)| GradientStop {
        position: *pos,
        color: *color,
      })
      .collect()
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
    let resolved = match len.unit {
      LengthUnit::Percent => None,
      unit if unit.is_font_relative() => Some(resolve_font_relative_length(*len, style, font_ctx)),
      unit if unit.is_viewport_relative() => len.resolve_with_viewport(viewport.0, viewport.1),
      unit if unit.is_absolute() => Some(len.to_px()),
      _ => None,
    }?;
    if resolved.is_finite() {
      Some(resolved)
    } else {
      None
    }
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
    has_intrinsic_ratio: bool,
  ) -> (f32, f32) {
    let natural_w = if img_w > 0.0 { Some(img_w) } else { None };
    let natural_h = if img_h > 0.0 { Some(img_h) } else { None };
    let ratio = if has_intrinsic_ratio && img_w > 0.0 && img_h > 0.0 {
      Some(img_w / img_h)
    } else {
      None
    };

    match layer.size {
      BackgroundSize::Keyword(BackgroundSizeKeyword::Cover) => {
        if has_intrinsic_ratio {
          if let (Some(w), Some(h)) = (natural_w, natural_h) {
            let scale = (area_w / w).max(area_h / h);
            (w * scale, h * scale)
          } else {
            (area_w.max(0.0), area_h.max(0.0))
          }
        } else {
          (area_w.max(0.0), area_h.max(0.0))
        }
      }
      BackgroundSize::Keyword(BackgroundSizeKeyword::Contain) => {
        if has_intrinsic_ratio {
          if let (Some(w), Some(h)) = (natural_w, natural_h) {
            let scale = (area_w / w).min(area_h / h);
            (w * scale, h * scale)
          } else {
            (area_w.max(0.0), area_h.max(0.0))
          }
        } else {
          (area_w.max(0.0), area_h.max(0.0))
        }
      }
      BackgroundSize::Explicit(x, y) => {
        let resolve = |component: BackgroundSizeComponent, area: f32| -> Option<f32> {
          match component {
            BackgroundSizeComponent::Auto => None,
            BackgroundSizeComponent::Length(len) => {
              Some(Self::resolve_length_for_paint(
                &len,
                font_size,
                root_font_size,
                area,
                viewport,
              ))
              .map(|v| v.max(0.0))
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
                      builder.build_fragment_internal(fragment, offset, false, true, visibility);
                    } else {
                      builder.build_fragment_shallow(fragment, offset, visibility);
                    }
                  }
                  (chunk_offset, builder.list, std::thread::current().id())
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
        self.build_fragment_internal(fragment, offset, false, true, visibility);
      } else {
        self.build_fragment_shallow(fragment, offset, visibility);
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
      decoded_image_cache: Arc::clone(&self.decoded_image_cache),
      svg_filter_defs: self.svg_filter_defs.clone(),
      svg_id_defs: self.svg_id_defs.clone(),
      viewport: self.viewport,
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
      estimated_fragments: self.estimated_fragments,
      scroll_state: self.scroll_state.clone(),
      max_iframe_depth: self.max_iframe_depth,
      skip_stacking_context_children: self.skip_stacking_context_children,
      error: self.error.clone(),
    }
  }

  /// Emits display items for fragment content
  fn emit_content(&mut self, fragment: &FragmentNode, rect: Rect) {
    match &fragment.content {
      FragmentContent::Text {
        text,
        baseline_offset,
        shaped,
        is_marker,
        emphasis_offset,
        ..
      } => {
        if text.is_empty() {
          return;
        }

        let style_opt = fragment.style.as_deref();
        let current = style_opt.map(|s| s.color).unwrap_or(Rgba::BLACK);
        let color = style_opt
          .map(|s| s.webkit_text_fill_color.to_rgba(current))
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

        if let Some(runs) = runs_ref {
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

          if *is_marker {
            let item = ListMarkerItem {
              origin,
              cached_bounds: Some(cached_bounds),
              glyphs,
              color,
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
          let decoration_baseline = baseline_block;
          self.emit_text_decorations(
            style,
            runs_ref,
            inline_start,
            inline_len,
            decoration_baseline,
            inline_vertical,
          );
        }
      }

      FragmentContent::Replaced { replaced_type, .. } => {
        if let ReplacedType::FormControl(control) = replaced_type {
          let style = fragment.style.as_deref();
          let clip_contents = style.and_then(|style| {
            let clip_x = Self::overflow_axis_clips(style.overflow_x);
            let clip_y = Self::overflow_axis_clips(style.overflow_y);
            if !clip_x && !clip_y {
              return None;
            }
            let overflow_bounds = rect.union(fragment.scroll_overflow.translate(rect.origin));
            let rects = Self::background_rects(rect, style, self.viewport);
            // Form controls can paint UI affordances (e.g. dropdown arrows) into the padding box.
            // Clip to the padding box (not the content box) so `overflow: clip` doesn't cut them
            // off.
            Self::overflow_clip_from_style_with_rects(
              style,
              &rects,
              clip_x,
              clip_y,
              overflow_bounds,
              self.viewport,
              self.build_breakdown.as_deref(),
            )
          });
          if let Some(clip) = clip_contents.as_ref() {
            self.list.push(DisplayItem::PushClip(clip.clone()));
          }
          // `emit_form_control` expects the border box and computes the padding/content
          // boxes internally. Passing an already-inset rect causes double insets.
          let painted = self.emit_form_control(control, fragment, rect);
          if clip_contents.is_some() {
            self.list.push(DisplayItem::PopClip);
          }
          if painted {
            return;
          }
        }

        if let ReplacedType::Math(math) = replaced_type {
          let fallback_style = ComputedStyle::default();
          let style = fragment.style.as_deref();
          let style_ref = style.unwrap_or(&fallback_style);
          let (content_rect, clip_radii) = self.replaced_content_rect_and_radii(rect, style);
          let clip_contents =
            Self::replaced_content_clip_item(style, content_rect, content_rect, clip_radii);
          if let Some(clip) = clip_contents.as_ref() {
            self.list.push(DisplayItem::PushClip(clip.clone()));
          }
          let layout_owned = math
            .layout
            .as_ref()
            .map(|l| l.as_ref().clone())
            .unwrap_or_else(|| layout_mathml(&math.root, style_ref, &self.font_ctx));
          let current = style_ref.color;
          let color = style_ref.webkit_text_fill_color.to_rgba(current);
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
              MathFragment::Glyph { origin, run } => {
                let scaled_run = Self::scale_run(&run, scale_x, scale_y);
                let baseline_y = content_rect.y() + origin.y * scale_y;
                let start_x = content_rect.x() + origin.x * scale_x;
                self.emit_shaped_runs(
                  &[scaled_run],
                  color,
                  baseline_y,
                  start_x,
                  &shadows,
                  Some(style_ref),
                  false,
                  TextEmphasisOffset::default(),
                );
              }
              MathFragment::Rule(r) => {
                let scaled_rect = Rect::from_xywh(
                  content_rect.x() + r.x() * scale_x,
                  content_rect.y() + r.y() * scale_y,
                  r.width() * scale_x,
                  r.height() * scale_y,
                );
                self.list.push(DisplayItem::FillRect(FillRectItem {
                  rect: scaled_rect,
                  color,
                }));
              }
              MathFragment::StrokeRect {
                rect: stroke_rect,
                radius,
                width,
              } => {
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
                      color,
                      width: stroke_width,
                      radii: BorderRadii::uniform(scaled_radius),
                    }));
                } else {
                  self.list.push(DisplayItem::StrokeRect(StrokeRectItem {
                    rect: scaled_rect,
                    color,
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
          return;
        }

        let style_for_image = fragment.style.as_deref();

        if let ReplacedType::Svg { content } = replaced_type {
          if self.emit_inline_svg(content, rect, style_for_image) {
            return;
          }
        }

        if let ReplacedType::Iframe {
          src,
          srcdoc,
          referrer_policy,
        } = replaced_type
        {
          if let Some(cache) = self.image_cache.as_ref() {
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
              return;
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
              return;
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
        let sources =
          replaced_type.image_sources_with_fallback(crate::tree::box_tree::ImageSelectionContext {
            device_pixel_ratio: self.device_pixel_ratio,
            slot_width: Some(slot_rect.width()),
            viewport: self.viewport.map(|(w, h)| crate::geometry::Size::new(w, h)),
            media_context: media_ctx.as_ref(),
            font_size: fragment.style.as_deref().map(|s| s.font_size),
            root_font_size: fragment.style.as_deref().map(|s| s.root_font_size),
            base_url: cache_base.as_deref(),
          });

        let crossorigin = match replaced_type {
          ReplacedType::Image { crossorigin, .. } => *crossorigin,
          _ => CrossOriginAttribute::None,
        };
        let referrer_policy = match replaced_type {
          ReplacedType::Image { referrer_policy, .. } => *referrer_policy,
          _ => None,
        };
        let reject_placeholder = matches!(
          replaced_type,
          ReplacedType::Embed { .. } | ReplacedType::Object { .. }
        );
        if let Some(image) = sources.iter().find_map(|s| {
          self.decode_image(
            s.url,
            style_for_image,
            false,
            crossorigin,
            referrer_policy,
            reject_placeholder,
          )
        }) {
          let (content_rect, clip_radii) =
            self.replaced_content_rect_and_radii(rect, style_for_image);
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
          let clip_contents =
            Self::replaced_content_clip_item(style_for_image, content_rect, dest_rect, clip_radii);
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
          return;
        }

        if reject_placeholder {
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
                return;
              }
            }
          }
        }

        if let ReplacedType::Image { alt: Some(alt), .. } = replaced_type {
          if self.emit_alt_text(alt, fragment, rect) {
            return;
          }
        }

        self.emit_replaced_placeholder(replaced_type, fragment, rect);
      }

      // Block, Inline, Line, other replaced types - no direct content
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

  fn root_background_candidate(
    fragment: &FragmentNode,
    origin: Point,
  ) -> Option<(Arc<ComputedStyle>, usize, Rect)> {
    let (html, html_origin) = if fragment.children.len() == 1 {
      let child = fragment.children.first().unwrap_or(fragment);
      (child, origin.translate(child.bounds.origin))
    } else {
      (fragment, origin)
    };

    if let Some(style) = html.style.clone() {
      if Self::has_paintable_background(&style) {
        let box_id = Self::get_box_id(html)?;
        return Some((style, box_id, Rect::new(html_origin, html.bounds.size)));
      }
    }

    for child in html.children.iter() {
      if let Some(style) = child.style.clone() {
        if Self::has_paintable_background(&style) {
          let box_id = Self::get_box_id(child)?;
          let rect = Rect::new(
            html_origin.translate(child.bounds.origin),
            child.bounds.size,
          );
          return Some((style, box_id, rect));
        }
      }
    }

    None
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
    if self.deadline_reached() {
      return;
    }

    let style_opt = fragment.style.as_deref();
    let paint_self = style_opt.map_or(true, |style| {
      matches!(
        style.visibility,
        crate::style::computed::Visibility::Visible
      )
    });
    if !paint_self && fragment.children.is_empty() {
      return;
    }

    if matches!(
      fragment.content,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
    ) {
      return;
    }

    let opacity = style_opt.map(|s| s.opacity).unwrap_or(1.0);
    if opacity <= f32::EPSILON {
      return;
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
        return;
      }
      if visibility.hard_clip {
        paint_bounds = match paint_bounds.intersection(vis) {
          Some(intersection) if intersection.width() > 0.0 && intersection.height() > 0.0 => {
            intersection
          }
          _ => return,
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
      return;
    }
    if let Some(vis) = child_visibility.rect {
      if !vis.intersects(paint_bounds) {
        return;
      }
      if child_visibility.hard_clip {
        match paint_bounds.intersection(vis) {
          Some(intersection) if intersection.width() > 0.0 && intersection.height() > 0.0 => {}
          _ => return,
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
            };

            if let Some(runs) = runs_ref {
              if inline_vertical {
                self.collect_shaped_runs_for_text_clip_vertical(
                  out,
                  runs,
                  baseline_block,
                  baseline_inline,
                );
              } else {
                self.collect_shaped_runs_for_text_clip(out, runs, baseline_inline, baseline_block);
              }
            }
          }
        }
      }
    }

    if skip_contents {
      return;
    }

    let element_scroll = self.element_scroll_offset(fragment);
    let child_offset = Point::new(
      absolute_rect.origin.x - element_scroll.x,
      absolute_rect.origin.y - element_scroll.y,
    );
    let mut counter = 0usize;
    for child in fragment.children.iter() {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      self.collect_text_clip_runs(child, child_offset, child_visibility, out);
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

  fn emit_background_from_style(&mut self, rect: Rect, style: &ComputedStyle) {
    self.emit_background_from_style_with_text_clip(rect, style, None);
  }

  fn emit_background_from_style_with_text_clip(
    &mut self,
    rect: Rect,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
  ) {
    let has_images = style.background_layers.iter().any(|l| l.image.is_some());
    if style.background_color.is_transparent() && !has_images {
      return;
    }

    let rects = Self::background_rects(rect, style, self.viewport);
    self.emit_background_from_style_with_rects_and_text_clip(&rects, style, text_clip);
  }

  fn emit_background_from_style_with_rects(
    &mut self,
    rects: &BackgroundRects,
    style: &ComputedStyle,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(rects, rects, style, None);
  }

  fn emit_background_from_style_with_rects_and_origin(
    &mut self,
    rects: &BackgroundRects,
    origin_rects: &BackgroundRects,
    style: &ComputedStyle,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(
      rects,
      origin_rects,
      style,
      None,
    );
  }

  fn emit_background_from_style_with_rects_and_text_clip(
    &mut self,
    rects: &BackgroundRects,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
  ) {
    self.emit_background_from_style_with_rects_and_origin_and_text_clip(
      rects, rects, style, text_clip,
    );
  }

  fn emit_background_from_style_with_rects_and_origin_and_text_clip(
    &mut self,
    rects: &BackgroundRects,
    origin_rects: &BackgroundRects,
    style: &ComputedStyle,
    text_clip: Option<&Arc<[TextItem]>>,
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
        );
      }
    }
  }

  fn emit_root_background(&mut self, root: &RootBackground) {
    let rects = Self::background_rects(root.paint_rect, &root.style, self.viewport);
    let origin_rects = Self::background_rects(root.origin_rect, &root.style, self.viewport);
    self.emit_background_from_style_with_rects_and_origin(&rects, &origin_rects, &root.style);
  }

  fn emit_background_layer_with_origin_rects(
    &mut self,
    rects: &BackgroundRects,
    origin_rects: &BackgroundRects,
    style: &ComputedStyle,
    layer: &BackgroundLayer,
    bg: &BackgroundImage,
    text_clip: Option<&Arc<[TextItem]>>,
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

    if clip_rect.width() <= 0.0
      || clip_rect.height() <= 0.0
      || origin_rect.width() <= 0.0
      || origin_rect.height() <= 0.0
    {
      return;
    }

    // Tiling is computed from `clip_rect`, which may be enormous for very tall pages. When we're
    // building a display list for a viewport-sized canvas we should only emit tiles that can
    // actually land on the canvas; the optimizer will cull off-screen items anyway. Clamping here
    // keeps paint build time proportional to what is visible.
    let visible_clip = match self.viewport_rect() {
      Some(viewport_rect) => {
        let Some(intersection) = clip_rect.intersection(viewport_rect) else {
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
        false,
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
        let resolved = Self::normalize_color_stops(stops, style.color);
        if !resolved.is_empty() {
          let stops = Self::gradient_stops(&resolved);
          let rad = angle.to_radians();
          let dx = rad.sin();
          let dy = -rad.cos();

          if repeat_both_axes {
            if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
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
          } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
            compute_tiles(0.0, 0.0)
          {
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
        let resolved = Self::normalize_color_stops(stops, style.color);
        if !resolved.is_empty() {
          let stops = Self::gradient_stops(&resolved);
          let rad = angle.to_radians();
          let dx = rad.sin();
          let dy = -rad.cos();

          if repeat_both_axes {
            if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
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
          } else if let Some((tile_w, tile_h, _offset_x, _offset_y, positions_x, positions_y)) =
            compute_tiles(0.0, 0.0)
          {
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
        let resolved = Self::normalize_color_stops_unclamped(stops, style.color);
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
        let resolved = Self::normalize_color_stops_unclamped(stops, style.color);
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
        let resolved = Self::normalize_color_stops(stops, style.color);
        if !resolved.is_empty() {
          let stops = Self::gradient_stops(&resolved);

          if repeat_both_axes {
            if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              let (cx, cy, radius_x, radius_y) = Self::radial_geometry(
                Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
                position,
                size,
                *shape,
                style.font_size,
                style.root_font_size,
                self.viewport,
              );

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
        let resolved = Self::normalize_color_stops(stops, style.color);
        if !resolved.is_empty() {
          let stops = Self::gradient_stops(&resolved);

          if repeat_both_axes {
            if let Some((tile_w, tile_h, offset_x, offset_y)) = compute_tile_metrics(0.0, 0.0) {
              record_pattern_fast_path(tile_w, tile_h, offset_x, offset_y);

              let (cx, cy, radius_x, radius_y) = Self::radial_geometry(
                Rect::from_xywh(0.0, 0.0, tile_w, tile_h),
                position,
                size,
                *shape,
                style.font_size,
                style.root_font_size,
                self.viewport,
              );

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
        if let Some(image) =
          self.decode_image(src, Some(style), true, CrossOriginAttribute::None, None, false)
        {
          let img_w = image.css_width;
          let img_h = image.css_height;
          if img_w > 0.0 && img_h > 0.0 {
            let (mut tile_w, mut tile_h) = Self::compute_background_size(
              layer,
              style.font_size,
              style.root_font_size,
              self.viewport,
              origin_rect.width(),
              origin_rect.height(),
              img_w,
              img_h,
              image.has_intrinsic_ratio,
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
              if image.has_intrinsic_ratio {
                let aspect = if img_h != 0.0 { img_w / img_h } else { 1.0 };
                if rounded_x {
                  tile_h = tile_w / aspect;
                } else {
                  tile_w = tile_h * aspect;
                }
              }
            }

            if tile_w > 0.0 && tile_h > 0.0 {
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
              let repeat_both_axes = matches!(
                layer.repeat,
                crate::style::types::BackgroundRepeat {
                  x: BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round,
                  y: BackgroundRepeatKeyword::Repeat | BackgroundRepeatKeyword::Round,
                }
              );

              // Fast path: for the common `repeat`/`round` in both axes case, emit a single pattern
              // fill instead of one item per tile.
              if repeat_both_axes {
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

                self.list.push(DisplayItem::ImagePattern(ImagePatternItem {
                  dest_rect: visible_clip,
                  image: image.clone(),
                  tile_size: Size::new(tile_w, tile_h),
                  origin: Point::new(origin_rect.x() + offset_x, origin_rect.y() + offset_y),
                  repeat: ImagePatternRepeat::Repeat,
                  filter_quality: quality,
                }));
              } else {
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
                      let src_x = src_rect.x().max(0.0).floor() as u32;
                      let src_y = src_rect.y().max(0.0).floor() as u32;
                      let src_w = src_rect.width().ceil() as u32;
                      let src_h = src_rect.height().ceil() as u32;
                      let max_x = image.width.saturating_sub(src_x);
                      let max_y = image.height.saturating_sub(src_y);
                      let crop_w = src_w.min(max_x);
                      let crop_h = src_h.min(max_y);
                      if src_x == 0 && src_y == 0 && crop_w == image.width && crop_h == image.height
                      {
                        None
                      } else {
                        Some(src_rect)
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
    for shadow in &style.box_shadow {
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
      let blur = Self::resolve_length_for_paint(
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
        blur_radius: blur,
        spread_radius: spread,
        color: shadow.color,
        inset,
      }));
    }
  }

  fn emit_border_from_style(&mut self, rect: Rect, style: &ComputedStyle) {
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

    let any_visible = Self::border_side_visible(&sides.0)
      || Self::border_side_visible(&sides.1)
      || Self::border_side_visible(&sides.2)
      || Self::border_side_visible(&sides.3);

    let border_image = match &style.border_image.source {
      BorderImageSource::Image(bg) => {
        let source = match bg.as_ref() {
          BackgroundImage::Url(src) => self
            .decode_image(src, Some(style), true, CrossOriginAttribute::None, None, false)
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
          font_size: style.font_size,
          root_font_size: style.root_font_size,
          viewport: self.viewport,
        })
      }
      BorderImageSource::None => None,
    };
    // `border-image` paints even when the border stroke itself is fully transparent. Only skip the
    // border display item when neither the border-image nor the stroked border can produce pixels.
    if border_image.is_none() && !any_visible {
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
    let (color, invert) = style.outline_color.resolve(style.color);
    if ow > 0.0 && !color.is_transparent() {
      self.list.push(DisplayItem::Outline(OutlineItem {
        rect,
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
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let origin_x = pen_x;
      let (glyphs, cached_bounds) = self.glyphs_from_run(run, origin_x, baseline_y);
      let font_id = self.font_id_from_run(run);
      let emphasis = style.and_then(|s| {
        self.build_emphasis(run, s, origin_x, baseline_y, inline_vertical, emphasis_offset)
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
  ) {
    let mut pen_x = start_x;
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let origin_x = if run.direction.is_rtl() {
        pen_x + run.advance
      } else {
        pen_x
      };
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
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let run_origin_inline = pen_inline;
      let (glyphs, cached_bounds) =
        self.glyphs_from_run_vertical(run, block_baseline, run_origin_inline);
      let font_id = self.font_id_from_run(run);
      let emphasis = style.and_then(|s| {
        self.build_emphasis(run, s, block_baseline, run_origin_inline, true, emphasis_offset)
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
  ) {
    let mut pen_inline = inline_start;
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let run_origin_inline = if run.direction.is_rtl() {
        pen_inline + run.advance
      } else {
        pen_inline
      };
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
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let origin_x = pen_x;
      let (glyphs, cached_bounds) = self.glyphs_from_run(run, origin_x, baseline_y);
      let font_id = self.font_id_from_run(run);
      let emphasis = style.and_then(|s| {
        self.build_emphasis(run, s, origin_x, baseline_y, inline_vertical, emphasis_offset)
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
    let mut counter = 0usize;
    for run in runs {
      if self.deadline_reached_periodic(&mut counter, DEADLINE_STRIDE) {
        break;
      }
      let run_origin_inline = pen_inline;
      let (glyphs, cached_bounds) =
        self.glyphs_from_run_vertical(run, block_baseline, run_origin_inline);
      let font_id = self.font_id_from_run(run);
      let emphasis = style.and_then(|s| {
        self.build_emphasis(run, s, block_baseline, run_origin_inline, true, emphasis_offset)
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
  ) {
    if inline_len <= 0.0 {
      return;
    }

    let decorations = if !style.applied_text_decorations.is_empty() {
      style.applied_text_decorations.clone()
    } else if !style.text_decoration.lines.is_empty() {
      vec![ResolvedTextDecoration {
        decoration: style.text_decoration.clone(),
        skip_ink: style.text_decoration_skip_ink,
        underline_offset: style.text_underline_offset,
        underline_position: style.text_underline_position,
      }]
    } else {
      Vec::new()
    };
    if decorations.is_empty() {
      return;
    }

    let Some(metrics) = self.decoration_metrics(runs, style) else {
      return;
    };

    let mut paints = Vec::new();
    let mut min_block = f32::INFINITY;
    let mut max_block = f32::NEG_INFINITY;

    for deco in decorations {
      let decoration_color = deco.decoration.color.unwrap_or(style.color);
      if decoration_color.alpha_u8() == 0 {
        continue;
      }

      let used_thickness =
        self.resolve_text_decoration_thickness_override(deco.decoration.thickness, style);

      let mut paint = DecorationPaint {
        style: deco.decoration.style,
        color: decoration_color,
        underline: None,
        overline: None,
        line_through: None,
      };

      if deco
        .decoration
        .lines
        .contains(TextDecorationLine::UNDERLINE)
      {
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
            self.build_underline_segments(
              r,
              inline_len,
              center,
              thickness,
              block_baseline,
              inline_vertical,
              deco.skip_ink,
            )
          })
        } else {
          None
        };
        paint.underline = Some(DecorationStroke {
          center,
          thickness,
          segments,
        });
        let half_extent = Self::stroke_half_extent(deco.decoration.style, thickness);
        min_block = min_block.min(center - half_extent);
        max_block = max_block.max(center + half_extent);
      }
      if deco.decoration.lines.contains(TextDecorationLine::OVERLINE) {
        let thickness = used_thickness.unwrap_or(metrics.underline_thickness);
        let center = block_baseline - metrics.ascent;
        paint.overline = Some(DecorationStroke {
          center,
          thickness,
          segments: None,
        });
        let half_extent = Self::stroke_half_extent(deco.decoration.style, thickness);
        min_block = min_block.min(center - half_extent);
        max_block = max_block.max(center + half_extent);
      }
      if deco
        .decoration
        .lines
        .contains(TextDecorationLine::LINE_THROUGH)
      {
        let thickness = used_thickness.unwrap_or(metrics.strike_thickness);
        let center = block_baseline - metrics.strike_pos;
        paint.line_through = Some(DecorationStroke {
          center,
          thickness,
          segments: None,
        });
        let half_extent = Self::stroke_half_extent(deco.decoration.style, thickness);
        min_block = min_block.min(center - half_extent);
        max_block = max_block.max(center + half_extent);
      }

      if paint.underline.is_some() || paint.overline.is_some() || paint.line_through.is_some() {
        paints.push(paint);
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

  fn resolve_text_decoration_thickness_override(
    &self,
    thickness: TextDecorationThickness,
    style: &ComputedStyle,
  ) -> Option<f32> {
    match thickness {
      // `auto` uses the UA default thickness, which is derived from font size rather than the
      // font-provided underline metrics. (See CSS Text Decoration Level 4.)
      //
      // Chrome's behavior is effectively `0.1em` with a `1px` minimum (validated by diffing
      // `text-decoration-thickness: auto` vs an explicit thickness while holding underline
      // position constant via `text-underline-position: from-font`).
      TextDecorationThickness::Auto => Some((style.font_size * 0.1).max(1.0)),
      // `from-font` uses per-font underline/strikeout thickness, so let the caller fall back to
      // `DecorationMetrics::{underline_thickness,strike_thickness}`.
      TextDecorationThickness::FromFont => None,
      TextDecorationThickness::Length(l) => {
        if l.unit == LengthUnit::Percent {
          l.resolve_against(style.font_size)
        } else if l.unit.is_viewport_relative() {
          self
            .viewport
            .and_then(|(vw, vh)| l.resolve_with_viewport(vw, vh))
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
      TextUnderlineOffset::Length(l) => {
        Some(if l.unit == LengthUnit::Percent {
          l.resolve_against(style.font_size).unwrap_or(0.0)
        } else if l.unit.is_font_relative() {
          resolve_font_relative_length(l, style, &self.font_ctx)
        } else if l.unit.is_viewport_relative() {
          self
            .viewport
            .and_then(|(vw, vh)| l.resolve_with_viewport(vw, vh))
            .unwrap_or_else(|| l.to_px())
        } else if l.unit.is_absolute() {
          l.to_px()
        } else {
          l.value * style.font_size
        })
      }
    }
  }

  fn underline_auto_offset_value(
    &self,
    metrics: &DecorationMetrics,
    position: TextUnderlinePosition,
    inline_vertical: bool,
  ) -> f32 {
    // `text-underline-offset: auto` is UA-defined. We preserve existing underline placement
    // behavior by defaulting to the font-provided underline position (or clamping to the
    // text-under edge for `text-underline-position: under`). The CSS Text Decoration Level 4 spec
    // requires this offset to be zero when `text-underline-position: from-font` and font metrics
    // are available.
    match position {
      TextUnderlinePosition::FromFont if metrics.has_font_underline_metrics => 0.0,
      TextUnderlinePosition::Under
      | TextUnderlinePosition::UnderLeft
      | TextUnderlinePosition::UnderRight => (-metrics.descent - metrics.underline_pos).max(0.0),
      TextUnderlinePosition::Left if inline_vertical => (-metrics.descent - metrics.underline_pos).max(0.0),
      TextUnderlinePosition::Right if inline_vertical => 0.0,
      _ => -metrics.underline_pos,
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
          metrics.underline_pos
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

  fn build_underline_segments(
    &self,
    runs: &[ShapedRun],
    line_width: f32,
    center: f32,
    thickness: f32,
    baseline_y: f32,
    inline_vertical: bool,
    skip_ink: TextDecorationSkipInk,
  ) -> Vec<(f32, f32)> {
    if line_width <= 0.0 {
      return Vec::new();
    }

    let decoration_timer = self.build_breakdown.as_ref().map(|_| Instant::now());
    let band_half = (thickness * 0.5).abs();
    let mut exclusions = if inline_vertical {
      let band_left = center - band_half;
      let band_right = center + band_half;
      collect_underline_exclusions_vertical(
        runs,
        baseline_y,
        band_left,
        band_right,
        skip_ink == TextDecorationSkipInk::All,
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
    let (ascent, descent) = if let Ok(metrics) = run.font.metrics() {
      let scaled = metrics.scale(run.font_size);
      (scaled.ascent, scaled.descent)
    } else {
      (run.font_size * 0.8, run.font_size * 0.2)
    };
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
        if mark_on_left {
          block_baseline - offset - emphasis_offset.under
        } else {
          block_baseline + offset + emphasis_offset.over
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
        while glyph_idx < run.glyphs.len() && run.glyphs[glyph_idx].cluster as usize == cluster_start
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
      use unicode_segmentation::UnicodeSegmentation;
      let mark_str = s.graphemes(true).next().unwrap_or("");
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
              if let Ok(m) = r.font.metrics() {
                let scaled = m.scale(r.font_size);
                ascent = ascent.max(scaled.ascent);
                descent = descent.max(scaled.descent);
              }
            }
            if ascent == 0.0 && descent == 0.0 {
              ascent = mark_style.font_size * 0.8;
              descent = mark_style.font_size * 0.2;
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
                font_size: r.font_size,
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

  fn emit_replaced_placeholder(
    &mut self,
    replaced_type: &ReplacedType,
    fragment: &FragmentNode,
    rect: Rect,
  ) {
    // Chrome (and friends) do not draw a UA placeholder for `<video>` when there is no poster and
    // no video frame available. Keeping this transparent is important for real pages (e.g. when
    // a thumbnail image sits behind the video element until it loads).
    //
    // Likewise, `<canvas>` is transparent when nothing has been drawn (and we don't execute JS),
    // so a placeholder would incorrectly obscure background content.
    if matches!(
      replaced_type,
      ReplacedType::Video { poster: None, .. } | ReplacedType::Canvas
    ) {
      return;
    }

    let style = fragment.style.as_deref();
    let (content_rect, clip_radii) = self.replaced_content_rect_and_radii(rect, style);
    let clip_contents =
      Self::replaced_content_clip_item(style, content_rect, content_rect, clip_radii);
    if let Some(clip) = clip_contents.as_ref() {
      self.list.push(DisplayItem::PushClip(clip.clone()));
    }

    let placeholder_color = Rgba::rgb(200, 200, 200);
    self.list.push(DisplayItem::FillRect(FillRectItem {
      rect: content_rect,
      color: placeholder_color,
    }));

    let stroke_color = Rgba::rgb(150, 150, 150);
    self.list.push(DisplayItem::StrokeRect(StrokeRectItem {
      rect: content_rect,
      color: stroke_color,
      width: 1.0,
      blend_mode: BlendMode::Normal,
    }));

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

  fn emit_form_control(
    &mut self,
    control: &FormControl,
    fragment: &FragmentNode,
    rect: Rect,
  ) -> bool {
    let Some(style) = fragment.style.as_deref() else {
      return false;
    };

    let rects = Self::background_rects(rect, style, self.viewport);
    let padding_rect = rects.padding;
    let content_rect = rects.content;
    if padding_rect.width() <= 0.0 || padding_rect.height() <= 0.0 {
      return true;
    }
    let content_clip = (content_rect.width() > 0.0 && content_rect.height() > 0.0).then(|| {
      let radii = if Self::border_radius_is_zero(style) {
        None
      } else {
        let radii = Self::resolve_clip_radii(
          style,
          &rects,
          BackgroundBox::ContentBox,
          self.viewport,
          self.build_breakdown.as_deref(),
        )
        .clamped(content_rect.width(), content_rect.height());
        (!radii.is_zero()).then_some(radii)
      };
      ClipItem {
        shape: ClipShape::Rect {
          rect: content_rect,
          radii,
        },
      }
    });

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
        return builder.emit_text_with_style(text, Some(style), rect);
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
      let line_height =
        compute_line_height_with_metrics_viewport(style, metrics_scaled.as_ref(), Some(viewport));
      let metrics =
        InlineTextItem::metrics_from_runs(&builder.font_ctx, &runs, line_height, style.font_size);
      let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
      let baseline_offset_y = if center_y {
        (rect.height() - line_height) / 2.0
      } else {
        0.0
      };
      let baseline = rect.y() + baseline_offset_y + half_leading + metrics.baseline_offset;

      let advance_width: f32 = runs.iter().map(|run| run.advance).sum();
      let start_x = if center_x {
        rect.x() + ((rect.width() - advance_width).max(0.0) / 2.0)
      } else {
        rect.x()
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
        let text = text.trim();
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

    let highlight = if control.invalid {
      Some(muted_accent.with_alpha((muted_accent.a * 0.25).max(0.18)))
    } else if control.focus_visible {
      Some(muted_accent.with_alpha((muted_accent.a * 0.22).max(0.14)))
    } else if control.focused {
      Some(muted_accent.with_alpha((muted_accent.a * 0.16).max(0.1)))
    } else if control.required {
      Some(muted_accent.with_alpha(0.08))
    } else {
      None
    };
    if let Some(tint) = highlight {
      let rect = inset_rect(content_rect, 1.0);
      if rect.width() > 0.0 && rect.height() > 0.0 {
        let radii = Self::resolve_clip_radii(
          style,
          &rects,
          BackgroundBox::ContentBox,
          self.viewport,
          self.build_breakdown.as_deref(),
        )
        .clamped(rect.width(), rect.height());
        self
          .list
          .push(DisplayItem::FillRoundedRect(FillRoundedRectItem {
            rect,
            color: tint,
            radii,
          }));
      }
    }

    match &control.control {
      FormControlKind::Text {
        value,
        placeholder,
        placeholder_style,
        kind,
        ..
      } => {
        let value_is_empty = value.is_empty();
        let base_color = if control.invalid { accent } else { style.color };
        let placeholder_color = base_color.with_alpha(0.6);

        let mut generated: Option<String> = None;
        let mut paint_text: Option<&str> = None;
        let mut fallback_color = base_color;
        let mut is_placeholder = false;

        match kind {
          TextControlKind::Password => {
            if !value_is_empty {
              let mask_len = value.chars().count().clamp(3, 50);
              generated = Some("•".repeat(mask_len));
              paint_text = generated.as_deref();
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref() {
              if !ph.is_empty() {
                paint_text = Some(ph);
                fallback_color = placeholder_color;
                is_placeholder = true;
              }
            }
          }
          TextControlKind::Number => {
            if !value_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref() {
              if !ph.is_empty() {
                paint_text = Some(ph);
                fallback_color = placeholder_color;
                is_placeholder = true;
              }
            }
          }
          TextControlKind::Date => {
            if !value_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref() {
              if !ph.is_empty() {
                paint_text = Some(ph);
                fallback_color = placeholder_color;
                is_placeholder = true;
              } else {
                paint_text = Some("yyyy-mm-dd");
                fallback_color = placeholder_color;
                is_placeholder = true;
              }
            } else {
              paint_text = Some("yyyy-mm-dd");
              fallback_color = placeholder_color;
              is_placeholder = true;
            }
          }
          TextControlKind::Plain => {
            if !value_is_empty {
              paint_text = Some(value.as_str());
              fallback_color = base_color;
            } else if let Some(ph) = placeholder.as_deref() {
              if !ph.is_empty() {
                paint_text = Some(ph);
                fallback_color = placeholder_color;
                is_placeholder = true;
              }
            }
          }
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

        let mut text_rect = content_rect;
        let mut affordance_space = 0.0;
        if !matches!(control.appearance, Appearance::None) {
          match kind {
            TextControlKind::Number => affordance_space = 14.0,
            TextControlKind::Date => affordance_space = 12.0,
            _ => {}
          }
        }
        let mut affordance_rect: Option<Rect> = None;
        if affordance_space > 0.0 && padding_rect.width() > 0.0 {
          let right = padding_rect.max_x();
          let left = (right - affordance_space).max(content_rect.x());
          affordance_rect = Some(Rect::from_xywh(
            left,
            content_rect.y(),
            (right - left).max(0.0),
            content_rect.height(),
          ));
          text_rect = Rect::from_xywh(
            content_rect.x(),
            content_rect.y(),
            (left - content_rect.x()).max(0.0),
            content_rect.height(),
          );
        }

        if text_style.color.a > f32::EPSILON {
          if let Some(text) = paint_text {
            if let Some(clip) = content_clip.as_ref() {
              self.list.push(DisplayItem::PushClip(clip.clone()));
            }
            let _ = self.emit_text_with_style(text, Some(&text_style), text_rect);
            if content_clip.is_some() {
              self.list.push(DisplayItem::PopClip);
            }
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

        if control.focused && !control.disabled {
          let caret_color = match style.caret_color {
            CaretColor::Color(c) => c,
            CaretColor::Auto => style.color,
          };
          if !caret_color.is_transparent() {
            let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
            let metrics_scaled = Self::resolve_scaled_metrics(&text_style, &self.font_ctx);
            let line_height = compute_line_height_with_metrics_viewport(
              &text_style,
              metrics_scaled.as_ref(),
              viewport,
            );
            let caret_x = if value_is_empty {
              text_rect.x()
            } else {
              let caret_text = match kind {
                TextControlKind::Password => generated.as_deref().unwrap_or(value.as_str()),
                _ => value.as_str(),
              };
              text_rect.x() + measure_shaped_advance(self, caret_text, &text_style)
            };

            let mut sample_text = paint_text.unwrap_or("M");
            if sample_text.trim().is_empty() {
              sample_text = "M";
            }
            let runs = shape_text_runs(self, sample_text, &text_style).unwrap_or_default();
            let metrics = InlineTextItem::metrics_from_runs(
              &self.font_ctx,
              &runs,
              line_height,
              text_style.font_size,
            );
            let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
            let baseline_y = text_rect.y() + half_leading + metrics.baseline_offset;
            let top = baseline_y - metrics.ascent;
            let bottom = baseline_y + metrics.descent;
            let caret_rect = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            if let Some(clipped) = caret_rect.intersection(content_rect) {
              if clipped.width() > 0.0 && clipped.height() > 0.0 {
                self.list.push(DisplayItem::FillRect(FillRectItem {
                  rect: clipped,
                  color: caret_color,
                }));
              }
            }
          }
        }
        true
      }
      FormControlKind::TextArea {
        value,
        placeholder,
        placeholder_style,
        ..
      } => {
        let base_color = if control.invalid { accent } else { style.color };
        let placeholder_color = base_color.with_alpha(0.6);

        let value_is_empty = value.is_empty();
        let rect = content_rect;

        let mut paint_text: Option<&str> = None;
        let mut fallback_color = base_color;
        let mut is_placeholder = false;
        if !value_is_empty {
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
        let line_height =
          compute_line_height_with_metrics_viewport(&text_style, metrics_scaled.as_ref(), viewport);

        if text_style.color.a > f32::EPSILON {
          if let Some(text) = paint_text {
            let mut y = rect.y();
            if let Some(clip) = content_clip.as_ref() {
              self.list.push(DisplayItem::PushClip(clip.clone()));
            }
            for line in text.split('\n') {
              if y > rect.y() + rect.height() {
                break;
              }
              let line_rect = Rect::from_xywh(rect.x(), y, rect.width(), line_height);
              let _ = self.emit_text_with_style(line, Some(&text_style), line_rect);
              y += line_height;
            }
            if content_clip.is_some() {
              self.list.push(DisplayItem::PopClip);
            }
          }
        }

        if control.focused && !control.disabled {
          let caret_color = match style.caret_color {
            CaretColor::Color(c) => c,
            CaretColor::Auto => style.color,
          };
          if !caret_color.is_transparent() {
            let (caret_y, caret_line) = if value_is_empty {
              (
                rect.y(),
                paint_text
                  .and_then(|t| t.split('\n').next())
                  .unwrap_or(""),
              )
            } else {
              let mut y = rect.y();
              let mut last_y = y;
              let mut last_line = "";
              for line in value.split('\n') {
                if y > rect.y() + rect.height() {
                  break;
                }
                last_y = y;
                last_line = line;
                y += line_height;
              }
              (last_y, last_line)
            };

            let caret_x = if value_is_empty || caret_line.is_empty() {
              rect.x()
            } else {
              rect.x() + measure_shaped_advance(self, caret_line, &text_style)
            };

            let sample_text = if caret_line.trim().is_empty() {
              "M"
            } else {
              caret_line
            };
            let runs = shape_text_runs(self, sample_text, &text_style).unwrap_or_default();
            let metrics = InlineTextItem::metrics_from_runs(
              &self.font_ctx,
              &runs,
              line_height,
              text_style.font_size,
            );
            let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
            let baseline_y = caret_y + half_leading + metrics.baseline_offset;
            let top = baseline_y - metrics.ascent;
            let bottom = baseline_y + metrics.descent;
            let caret_rect = Rect::from_xywh(caret_x, top, 1.0, (bottom - top).max(0.0));
            if let Some(clipped) = caret_rect.intersection(rect) {
              if clipped.width() > 0.0 && clipped.height() > 0.0 {
                self.list.push(DisplayItem::FillRect(FillRectItem {
                  rect: clipped,
                  color: caret_color,
                }));
              }
            }
          }
        }
        true
      }
      FormControlKind::Select(select) => {
        let is_listbox = select.multiple || select.size > 1;

        if is_listbox {
          let metrics_scaled = Self::resolve_scaled_metrics(style, &self.font_ctx);
          let viewport = self.viewport.map(|(w, h)| Size::new(w, h));
          let row_height =
            compute_line_height_with_metrics_viewport(style, metrics_scaled.as_ref(), viewport);
          if row_height <= 0.0 || !row_height.is_finite() {
            return true;
          }

          let total_rows = select.items.len();
          let viewport_height = content_rect.height().max(0.0);
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
          let text_rect = Rect::from_xywh(
            content_rect.x(),
            content_rect.y(),
            (content_rect.width() - scrollbar_width).max(0.0),
            content_rect.height(),
          );

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
                let row_rect = Rect::from_xywh(
                  row_rect.x() + 2.0,
                  row_rect.y(),
                  (row_rect.width() - 2.0).max(0.0),
                  row_rect.height(),
                );
                let _ = self.emit_text_with_style(label, Some(&row_style), row_rect);
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
                let row_rect = Rect::from_xywh(
                  row_rect.x() + indent,
                  row_rect.y(),
                  (row_rect.width() - indent).max(0.0),
                  row_rect.height(),
                );
                let _ = self.emit_text_with_style(label, Some(&row_style), row_rect);
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
          if let Some(clip) = content_clip.as_ref() {
            self.list.push(DisplayItem::PushClip(clip.clone()));
          }
          let _ = self.emit_text_with_style(label, Some(&select_style), content_rect);
          if content_clip.is_some() {
            self.list.push(DisplayItem::PopClip);
          }

          if !matches!(control.appearance, Appearance::None) {
            let arrow_space = 14.0_f32.min(padding_rect.width().max(0.0));
            let arrow_left = (padding_rect.max_x() - arrow_space).max(content_rect.max_x());
            let arrow_rect = Rect::from_xywh(
              arrow_left,
              content_rect.y(),
              (padding_rect.max_x() - arrow_left).max(0.0),
              content_rect.height(),
            );
            if arrow_rect.width() > 0.0 {
              let mut arrow_style = select_style;
              arrow_style.color = muted_accent;
              arrow_style.font_size = (style.font_size * 0.9).max(8.0);
              let _ = emit_text_aligned(self, "▾", &arrow_style, arrow_rect, true, true);
            }
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
        if let Some(clip) = content_clip.as_ref() {
          self.list.push(DisplayItem::PushClip(clip.clone()));
        }
        let _ = emit_text_aligned(self, label, &button_style, rect, true, true);
        if content_clip.is_some() {
          self.list.push(DisplayItem::PopClip);
        }
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
          .and_then(|style| style.width.map(|len| resolve_px(style, len, padding_rect.width())))
          .filter(|px| px.is_finite() && *px > 0.0)
          .unwrap_or(default_knob_diameter);
        let knob_height = thumb_style
          .and_then(|style| style.height.map(|len| resolve_px(style, len, padding_rect.height())))
          .filter(|px| px.is_finite() && *px > 0.0)
          .unwrap_or(default_knob_diameter);

        let knob_travel = (padding_rect.width() - knob_width).max(0.0);
        let knob_center_x = padding_rect.x() + knob_width / 2.0 + clamped * knob_travel;
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
          let track_height = track_style
            .and_then(|style| style.height.map(|len| resolve_px(style, len, padding_rect.height())))
            .filter(|px| px.is_finite() && *px > 0.0)
            .unwrap_or_else(|| 4.0_f32.min(padding_rect.height()));
          if track_height > 0.0 {
            let track_y = padding_rect.y() + (padding_rect.height() - track_height) / 2.0;
            let track_rect =
              Rect::from_xywh(padding_rect.x(), track_y, padding_rect.width(), track_height);

            if let Some(track_style) = track_style {
              self.emit_box_shadows_from_style(track_rect, track_style, false);
              self.emit_background_from_style(track_rect, track_style);
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
              let filled_rect = Rect::from_xywh(
                track_rect.x(),
                track_rect.y(),
                (knob_center_x - track_rect.x()).max(0.0),
                track_rect.height(),
              );
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
              self.emit_border_from_style(track_rect, track_style);
            }
          }
        }

        if let Some(thumb_style) = thumb_style {
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
          self.emit_background_from_style(knob_rect, style_for_thumb);
          self.emit_box_shadows_from_style(knob_rect, style_for_thumb, true);
          self.emit_border_from_style(knob_rect, style_for_thumb);
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
      FormControlKind::Unknown { label } => {
        if let Some(text) = label {
          let rect = content_rect;
          let mut unknown_style = style.clone();
          if control.invalid {
            unknown_style.color = accent;
          }
          if let Some(clip) = content_clip.as_ref() {
            self.list.push(DisplayItem::PushClip(clip.clone()));
          }
          let _ = self.emit_text_with_style(text, Some(&unknown_style), rect);
          if content_clip.is_some() {
            self.list.push(DisplayItem::PopClip);
          }
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
    let ok = self.emit_text_with_style(alt, style, content_rect);
    if clip_contents.is_some() {
      self.list.push(DisplayItem::PopClip);
    }
    ok
  }

  fn emit_text_with_style(
    &mut self,
    text: &str,
    style: Option<&ComputedStyle>,
    rect: Rect,
  ) -> bool {
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
    let line_height =
      compute_line_height_with_metrics_viewport(style, metrics_scaled.as_ref(), viewport);
    let metrics =
      InlineTextItem::metrics_from_runs(&self.font_ctx, &runs, line_height, style.font_size);
    let half_leading = (metrics.line_height - (metrics.ascent + metrics.descent)) / 2.0;
    let baseline = rect.y() + half_leading + metrics.baseline_offset;

    let shadows = Self::text_shadows_from_style(Some(style), self.viewport);
    self.emit_shaped_runs(
      &runs,
      style.color,
      baseline,
      rect.x(),
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
    let shadows = Self::text_shadows_from_style(style, self.viewport);
    let char_width = font_size * 0.6;
    let origin = Point::new(rect.x(), rect.y() + font_size * 0.8);
    let advance_width = text.len() as f32 * char_width;
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

    let item = TextItem {
      origin,
      cached_bounds: Some(cached_bounds),
      glyphs,
      color,
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

    let resolved_svg = if content.foreign_objects.is_empty() {
      None
    } else {
      crate::paint::svg_foreign_object::inline_svg_with_foreign_objects(
      &content.svg,
      &content.foreign_objects,
      &content.shared_css,
      &self.font_ctx,
      image_cache,
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

    let meta = match image_cache.probe_svg_content(svg, "inline-svg") {
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

    let injection = content.document_css_injection.as_ref();
    let (content_hash, content_len) = match injection {
      Some(injection) => match (
        svg.get(..injection.insert_pos),
        svg.get(injection.insert_pos..),
      ) {
        // Hash the final SVG markup (with injected CSS) without allocating it, matching `str::hash`
        // semantics (single 0xFF terminator byte).
        (Some(prefix), Some(suffix)) => {
          let style_element = injection.style_element.as_ref();
          let mut hasher = DefaultHasher::new();
          hasher.write(prefix.as_bytes());
          hasher.write(style_element.as_bytes());
          hasher.write(suffix.as_bytes());
          hasher.write_u8(0xff);
          (
            hasher.finish(),
            prefix.len() + style_element.len() + suffix.len(),
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
    let pixmap = match injection {
      Some(injection) => image_cache.render_svg_pixmap_at_size_with_injected_style(
        svg,
        injection.insert_pos,
        injection.style_element.as_ref(),
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

  fn decode_image(
    &self,
    src: &str,
    style: Option<&ComputedStyle>,
    decorative: bool,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
    reject_placeholder: bool,
  ) -> Option<Arc<ImageData>> {
    let image_cache = self.image_cache.as_ref()?;
    let trimmed = src.trim_start();
    let inline_svg = trimmed.starts_with('<');

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

    if reject_placeholder && !inline_svg && image_cache.is_placeholder_image(&image) {
      return None;
    }

    let image_resolution = style.map(|s| s.image_resolution).unwrap_or_default();
    let orientation = style
      .map(|s| s.image_orientation.resolve(image.orientation, decorative))
      .unwrap_or_else(|| ImageOrientation::default().resolve(image.orientation, decorative));
    let has_intrinsic_ratio = image.intrinsic_ratio(orientation).is_some();
    let used_resolution =
      image_resolution.used_resolution(None, image.resolution, self.device_pixel_ratio);
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
    let (css_w, css_h) = match image.css_dimensions(
      orientation,
      &image_resolution,
      self.device_pixel_ratio,
      None,
    ) {
      Some(dimensions) => dimensions,
      None => {
        if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
          breakdown.record_image_decode(start.elapsed());
        }
        return None;
      }
    };
    let rgba = image.to_oriented_rgba(orientation);
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
      if let (Some(breakdown), Some(start)) = (self.build_breakdown.as_ref(), decode_timer) {
        breakdown.record_image_decode(start.elapsed());
      }
      return None;
    }
    let mut image_data = ImageData::new(w, h, css_w, css_h, rgba.into_raw());
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
) -> Vec<(f32, f32)> {
  crate::paint::text_decoration_skip_ink::collect_underline_exclusions(
    runs,
    0.0,
    baseline_y,
    band_top,
    band_bottom,
    skip_all,
    1.0,
  )
}

fn collect_underline_exclusions_vertical(
  runs: &[ShapedRun],
  block_baseline: f32,
  band_left: f32,
  band_right: f32,
  skip_all: bool,
) -> Vec<(f32, f32)> {
  crate::paint::text_decoration_skip_ink::collect_underline_exclusions_vertical(
    runs,
    0.0,
    block_baseline,
    band_left,
    band_right,
    skip_all,
    1.0,
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
  use crate::image_loader::ImageCache;
  use crate::paint::display_list::text_bounds;
  use crate::paint::display_list::ResolvedMaskImage;
  use crate::paint::display_list_renderer::DisplayListRenderer;
  use crate::paint::stacking::StackingContext;
  use crate::paint::stacking::StackingContextReason;
  use crate::render_control::RenderDeadline;
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
  use crate::style::types::BackgroundLayer;
  use crate::style::types::BackgroundRepeat;
  use crate::style::types::BackgroundRepeatKeyword;
  use crate::style::types::BackgroundSize;
  use crate::style::types::BackgroundSizeComponent;
  use crate::style::types::BasicShape;
  use crate::style::types::ClipComponent;
  use crate::style::types::ClipPath;
  use crate::style::types::ClipRect;
  use crate::style::types::Containment;
  use crate::style::types::ImageOrientation;
  use crate::style::types::ImageRendering;
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
  use crate::tree::box_tree::{CrossOriginAttribute, ReplacedType};
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
  fn clip_rect_from_style_resolves_rem_against_root_font_size() {
    let mut style = ComputedStyle::default();
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
      &layer,
      10.0,
      10.0,
      None,
      100.0,
      100.0,
      0.0,
      0.0,
      false,
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
      false,
    );
    assert!((w - 20.0).abs() < 1e-6);
    assert!((h - 10.0).abs() < 1e-6);
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
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
      .write_image(&color, 1, 1, ColorType::Rgba8.into())
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
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: crate::text::pipeline::RunRotation::None,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    }
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

  fn create_image_fragment(x: f32, y: f32, width: f32, height: f32, src: &str) -> FragmentNode {
    FragmentNode::new_replaced(
      Rect::from_xywh(x, y, width, height),
      ReplacedType::Image {
        src: src.to_string(),
        alt: None,
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
    let root = FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

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
    let root = FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

    let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

    let child_context = list.items().iter().find_map(|item| match item {
      DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
      _ => None,
    });
    let child_context = child_context.expect("expected a stacking context for backdrop-filter:url()");

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
    std::env::set_var("FASTR_DISPLAY_LIST_PARALLEL_MIN", "1");
    std::env::set_var("FASTR_DISPLAY_LIST_PARALLEL", "1");

    let child1 = create_text_fragment(0.0, 0.0, 20.0, 10.0, "One");
    let child2 = create_text_fragment(0.0, 10.0, 20.0, 10.0, "Two");
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![child1, child2]);

    let parallel = DisplayListBuilder::new().build(&root);

    std::env::set_var("FASTR_DISPLAY_LIST_PARALLEL", "0");
    let sequential = DisplayListBuilder::new().build(&root);

    std::env::remove_var("FASTR_DISPLAY_LIST_PARALLEL_MIN");
    std::env::remove_var("FASTR_DISPLAY_LIST_PARALLEL");

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
    assert_eq!(img.image.width, 1);
    assert_eq!(img.image.height, 1);
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
    root.compute_bounds(None);

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
    layer.image = Some(BackgroundImage::Url(data_url_for_color([0, 0, 0, 0])));
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

    let push_opacity = items
      .iter()
      .position(|item| matches!(item, DisplayItem::PushOpacity(_)))
      .expect("expected push opacity");
    let pop_opacity = items
      .iter()
      .rposition(|item| matches!(item, DisplayItem::PopOpacity))
      .expect("expected pop opacity");

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
        .filter(|item| matches!(item, DisplayItem::PushOpacity(_)))
        .count(),
      1
    );
    assert_eq!(
      items
        .iter()
        .filter(|item| matches!(item, DisplayItem::PopOpacity))
        .count(),
      1
    );
    assert!(push_opacity < background && background < text && text < pop_opacity);
    if let DisplayItem::PushOpacity(opacity) = &items[push_opacity] {
      assert!((opacity.opacity - 0.5).abs() < f32::EPSILON);
    }
  }

  #[test]
  fn stacking_context_plane_rect_uses_root_fragment_bounds() {
    let mut style = ComputedStyle::default();
    style
      .transform
      .push(Transform::Translate(Length::px(0.0), Length::px(0.0)));

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
    assert_eq!(
      sc_bounds,
      Some(Rect::from_xywh(5.0, 25.0, 55.0, 50.0)),
      "stacking context bounds should include text-shadow paint overflow"
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
            position: Some(0.0),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(1.0),
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
  fn root_background_extension_preserves_gradient_tile_size() {
    // When the canvas is taller than the root stacking context bounds, we extend the root
    // background paint rect to the viewport. The background image tile size should still be
    // derived from the original element bounds so repeat defaults match Chrome.
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
            position: Some(0.0),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLUE),
            position: Some(1.0),
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
      (extended.tile_size.height - 10.0).abs() < 0.01,
      "expected root background tile height to remain 10px, got {}",
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
            position: Some(0.0),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::RED),
            position: Some(1.0),
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
    let base_url = format!("file://{}", dir.display());

    let mut style = ComputedStyle::default();
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::Url(
        path.file_name().unwrap().to_str().unwrap().to_string(),
      )),
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

    let chosen = match content {
      ContentValue::Items(items) if items.len() == 1 => match &items[0] {
        ContentItem::Url(url) => url.clone(),
        other => panic!("unexpected content item: {other:?}"),
      },
      other => panic!("unexpected content value: {other:?}"),
    };

    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      ReplacedType::Image {
        src: chosen,
        alt: None,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
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
    let low = data_url_for_color([255, 0, 0, 255]);
    let high = data_url_for_color([0, 255, 0, 255]);

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

    let chosen = match &style.list_style_image {
      crate::style::types::ListStyleImage::Url(url) => url.clone(),
      crate::style::types::ListStyleImage::None => panic!("unexpected list-style-image: None"),
    };

    let mut fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      ReplacedType::Image {
        src: chosen,
        alt: None,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
    );
    fragment.style = Some(Arc::new(style));

    let list = DisplayListBuilder::with_image_cache(ImageCache::new()).build(&fragment);
    let image = list
      .items()
      .iter()
      .find_map(|item| match item {
        DisplayItem::Image(img) => Some(img),
        _ => None,
      })
      .expect("marker image should emit an image item");

    assert_eq!(&image.image.pixels[..4], &[0, 255, 0, 255]);
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
    assert_eq!(img.image.width, 1);
    assert_eq!(img.image.height, 1);
    let pixels = img.image.pixels.as_ref();
    assert_eq!(pixels.len(), 4);
    assert_eq!(pixels, &[255, 0, 0, 255]);
  }

  #[test]
  fn decode_image_cache_reuses_converted_pixels() {
    let src = data_url_for_color([5, 6, 7, 8]);
    let mut builder = DisplayListBuilder::new();

    let first = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false)
      .expect("first decode");
    let second = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false)
      .expect("cached decode");
    assert!(Arc::ptr_eq(&first, &second));

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
      )
      .expect("rotated decode");
    assert!(!Arc::ptr_eq(&first, &rotated));

    builder.set_device_pixel_ratio(2.0);
    let hidpi = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false)
      .expect("hi-dpi decode");
    assert!(!Arc::ptr_eq(&first, &hidpi));
    let hidpi_cached = builder
      .decode_image(&src, None, false, CrossOriginAttribute::None, None, false)
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
            .decode_image(url, None, false, CrossOriginAttribute::None, None, false)
            .is_some(),
          "expected no-cors decode to succeed"
        );

        // Crossorigin loads should not be able to reuse the no-cors decoded pixels when CORS
        // enforcement is enabled; missing ACAO should surface as a load failure.
        assert!(
          builder
            .decode_image(url, None, false, CrossOriginAttribute::Anonymous, None, false)
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
    assert_eq!(embed_img.image.width, 2);
    assert_eq!(embed_img.image.height, 2);

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
    assert_eq!(object_img.image.width, 2);
    assert_eq!(object_img.image.height, 2);
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
          referrer_policy: None,
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
          referrer_policy: None,
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
      image: Some(BackgroundImage::Url(url)),
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
      image: Some(BackgroundImage::Url(url)),
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
    let DisplayItem::Text(text) = &list.items()[0] else {
      panic!("Expected text item for alt fallback");
    };
    assert!(text.advance_width > 0.0);
  }

  #[test]
  fn missing_image_without_alt_emits_placeholder() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 40.0, 20.0),
      ReplacedType::Image {
        src: String::new(),
        alt: None,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
    );
    let builder = DisplayListBuilder::new();
    let list = builder.build(&fragment);

    assert_eq!(list.len(), 2, "expected fill + stroke placeholder items");
    assert!(matches!(list.items()[0], DisplayItem::FillRect(_)));
    assert!(matches!(list.items()[1], DisplayItem::StrokeRect(_)));
  }

  #[test]
  fn video_without_poster_emits_no_placeholder() {
    let fragment = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 40.0, 20.0),
      ReplacedType::Video {
        src: String::new(),
        poster: None,
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
      let _ = collect_underline_exclusions(&runs, 0.0, -2.0, 2.0, false);
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
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: crate::text::pipeline::RunRotation::None,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: run_scale,
    };

    let intervals = collect_underline_exclusions(&[run], 0.0, -1.0, 1.0, true);
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
  fn background_tiling_aborts_on_deadline() {
    struct EnvVarGuard {
      key: &'static str,
      prev: Option<String>,
    }

    impl EnvVarGuard {
      fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prev }
      }
    }

    impl Drop for EnvVarGuard {
      fn drop(&mut self) {
        match &self.prev {
          Some(prev) => std::env::set_var(self.key, prev),
          None => std::env::remove_var(self.key),
        }
      }
    }

    // Make each deadline check take 10ms so we can deterministically force the render deadline to
    // expire mid-way through background tiling (as opposed to at fragment traversal boundaries).
    let _delay_guard = EnvVarGuard::set("FASTR_TEST_RENDER_DELAY_MS", "10");
    let deadline = RenderDeadline::new(Some(Duration::from_millis(35)), None);

    let mut style = ComputedStyle::default();
    style.background_color = Rgba::TRANSPARENT;
    style.set_background_layers(vec![BackgroundLayer {
      image: Some(BackgroundImage::LinearGradient {
        angle: 0.0,
        stops: vec![
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::BLACK),
            position: Some(0.0),
          },
          crate::css::types::ColorStop {
            color: Color::Rgba(Rgba::WHITE),
            position: Some(1.0),
          },
        ],
      }),
      repeat: BackgroundRepeat {
        x: BackgroundRepeatKeyword::Repeat,
        y: BackgroundRepeatKeyword::Repeat,
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
}
