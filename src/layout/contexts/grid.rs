//! Grid Formatting Context - CSS Grid Layout via Taffy
//!
//! This module implements CSS Grid layout by wrapping the Taffy layout library.
//! It converts between fastrender's box/fragment tree representation and Taffy's
//! internal representation, delegating the actual grid algorithm to Taffy.
//!
//! # Architecture
//!
//! 1. **BoxNode → Taffy Tree**: Convert fastrender BoxNode tree to Taffy nodes
//! 2. **ComputedStyle → Taffy Style**: Map CSS properties to Taffy style values
//! 3. **Taffy Layout**: Run Taffy's grid layout algorithm
//! 4. **Taffy → FragmentNode**: Convert Taffy layout results to fragments
//!
//! # CSS Grid Support
//!
//! Supports core CSS Grid features:
//! - grid-template-columns/rows with track sizing functions
//! - grid-auto-columns/rows for implicit tracks
//! - grid-auto-flow (row, column, dense variants)
//! - gap (row-gap, column-gap)
//! - grid-column/row placement (line numbers, spans, auto)
//! - align-content, justify-content, align-items, justify-items
//! - align-self, justify-self
//!
//! # References
//!
//! - CSS Grid Layout Module Level 2: <https://www.w3.org/TR/css-grid-2/>
//! - Taffy: <https://github.com/DioxusLabs/taffy>

use crate::error::{RenderError, RenderStage};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::layout::axis::{FragmentAxes, PhysicalAxis};
use crate::layout::constraints::AvailableSpace as CrateAvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::factory::FormattingContextFactory;
use crate::layout::contexts::flex_cache::ShardedFlexCache;
use crate::layout::engine::LayoutParallelism;
use crate::layout::formatting_context::fragmentainer_block_size_hint;
use crate::layout::formatting_context::intrinsic_block_cache_lookup;
use crate::layout::formatting_context::intrinsic_block_cache_store;
#[cfg(not(test))]
use crate::layout::formatting_context::intrinsic_cache_epoch;
use crate::layout::formatting_context::intrinsic_cache_lookup;
use crate::layout::formatting_context::intrinsic_cache_store;
use crate::layout::formatting_context::layout_cache_lookup;
use crate::layout::formatting_context::layout_cache_store;
use crate::layout::formatting_context::remembered_size_cache_lookup;
use crate::layout::formatting_context::remembered_size_cache_store;
use crate::layout::formatting_context::FormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::layout::formatting_context::LayoutError;
use crate::layout::fragment_clone_profile::{self, CloneSite};
use crate::layout::profile::layout_timer;
use crate::layout::profile::LayoutKind;
use crate::layout::running_elements::clear_running_position_in_box_tree;
use crate::layout::style_override::{push_style_override, style_override_for, StyleOverrideGuard};
use crate::layout::taffy_integration::{
  record_taffy_compute, record_taffy_invocation, record_taffy_measure_call,
  record_taffy_node_cache_hit, record_taffy_node_cache_miss, record_taffy_style_cache_hit,
  record_taffy_style_cache_miss, taffy_grid_container_style_fingerprint,
  taffy_grid_item_style_fingerprint, taffy_template_cache_limit, CachedTaffyTemplate,
  SendSyncStyle, TaffyAdapterKind, TaffyNodeCache, TaffyNodeCacheKey, TAFFY_ABORT_CHECK_STRIDE,
};
use crate::layout::utils::border_size_from_box_sizing;
use crate::layout::utils::clamp_with_order;
use crate::layout::utils::resolve_length_with_percentage_metrics;
use crate::layout::utils::resolve_scrollbar_width;
use crate::render_control::{
  active_deadline, active_stage, check_active, check_active_periodic, with_deadline, StageGuard,
};
use crate::style::display::Display as CssDisplay;
use crate::style::display::FormattingContextType;
use crate::style::grid::validate_area_rectangles;
use crate::style::types::AlignContent;
use crate::style::types::AlignItems;
use crate::style::types::AspectRatio;
use crate::style::types::BoxSizing;
use crate::style::types::Direction;
use crate::style::types::GridAutoFlow;
use crate::style::types::GridTrack;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::JustifyContent;
use crate::style::types::Overflow as CssOverflow;
use crate::style::types::WhiteSpace;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{
  FragmentContent, FragmentNode, GridFragmentationInfo, GridItemFragmentationData, GridTrackRanges,
};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use taffy::geometry::Line;
use taffy::prelude::TaffyFitContent;
use taffy::prelude::TaffyMaxContent;
use taffy::prelude::TaffyMinContent;
use taffy::style::AlignContent as TaffyAlignContent;
use taffy::style::Dimension;
use taffy::style::Display;
use taffy::style::GridPlacement as TaffyGridPlacement;
use taffy::style::GridTemplateArea;
use taffy::style::GridTemplateComponent;
use taffy::style::GridTemplateRepetition;
use taffy::style::LengthPercentage;
use taffy::style::LengthPercentageAuto;
use taffy::style::MaxTrackSizingFunction;
use taffy::style::MinTrackSizingFunction;
use taffy::style::Overflow as TaffyOverflow;
use taffy::style::RepetitionCount;
use taffy::style::Style as TaffyStyle;
use taffy::style::TrackSizingFunction;
use taffy::style_helpers::TaffyAuto;
use taffy::tree::DetailedLayoutInfo;
use taffy::tree::Layout as TaffyLayout;
use taffy::tree::NodeId as TaffyNodeId;
use taffy::tree::TaffyTree;
use taffy::DetailedGridTracksInfo;

const MAX_MEASURED_KEYS_PER_NODE: usize = 32;
const GRID_DEADLINE_CHECK_STRIDE: usize = 64;
const GRID_CONTENT_VISIBILITY_AUTO_MAX_PASSES: usize = 4;

struct StyleOverrideStack {
  guards: Vec<StyleOverrideGuard>,
}

impl StyleOverrideStack {
  fn new() -> Self {
    Self { guards: Vec::new() }
  }

  fn push(&mut self, guard: StyleOverrideGuard) {
    self.guards.push(guard);
  }
}

impl Drop for StyleOverrideStack {
  fn drop(&mut self) {
    while self.guards.pop().is_some() {}
  }
}

#[inline]
fn check_layout_deadline(counter: &mut usize) -> Result<(), LayoutError> {
  if let Err(RenderError::Timeout { elapsed, .. }) =
    check_active_periodic(counter, GRID_DEADLINE_CHECK_STRIDE, RenderStage::Layout)
  {
    return Err(LayoutError::Timeout { elapsed });
  }
  Ok(())
}

type FingerprintHasher = FxHasher;

#[derive(Clone, Copy)]
enum Axis {
  Horizontal,
  Vertical,
}

#[derive(Clone, Copy, Debug)]
struct GridAxisStyle {
  writing_mode: WritingMode,
  direction: Direction,
}

impl GridAxisStyle {
  fn from_style(style: &ComputedStyle) -> Self {
    Self {
      writing_mode: style.writing_mode,
      direction: style.direction,
    }
  }

  fn effective_for_grid_container(style: &ComputedStyle, parent_axis: Option<Self>) -> Self {
    // Subgrid track definitions are inherited from the parent grid and stay in the parent grid's
    // axis space (even when the subgrid specifies a different `writing-mode`). To keep line names,
    // gaps, and placement coordinates consistent, treat subgrids as using the parent grid's
    // writing-mode when mapping CSS grid axes into Taffy's fixed horizontal/vertical axes.
    //
    // Directionality (`direction`) only needs to be inherited when the inline axis is inherited
    // (`grid-template-columns: subgrid` / `grid_column_subgrid`), otherwise the subgrid's own
    // direction continues to affect its locally-defined columns.
    if let Some(parent_axis) = parent_axis {
      if style.grid_row_subgrid || style.grid_column_subgrid {
        return Self {
          writing_mode: parent_axis.writing_mode,
          direction: if style.grid_column_subgrid {
            parent_axis.direction
          } else {
            style.direction
          },
        };
      }
    }

    Self::from_style(style)
  }

  fn inline_is_horizontal(self) -> bool {
    matches!(self.writing_mode, WritingMode::HorizontalTb)
  }

  fn inline_positive(self) -> bool {
    self.direction != Direction::Rtl
  }

  fn block_positive(self) -> bool {
    match self.writing_mode {
      WritingMode::VerticalRl | WritingMode::SidewaysRl => false,
      _ => true,
    }
  }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum MeasureAvailKey {
  Definite(u32),
  /// Represents "effectively indefinite" available space.
  ///
  /// `constraints_from_taffy` treats very small definite sizes (`<= 1px`) as
  /// `AvailableSpace::Indefinite` to avoid pathological 0px probes. Encode that
  /// normalization in the cache key so near-zero definites coalesce.
  Indefinite,
  MinContent,
  MaxContent,
  /// Marker used when the corresponding `known_dimensions` axis is definite.
  ///
  /// When Taffy provides a known size, `constraints_from_taffy` ignores the
  /// `AvailableSpace` value for that axis. Including the raw `AvailableSpace`
  /// in the cache key would therefore create redundant entries (and redundant
  /// `fc.layout` calls) for identical measurements.
  Ignored,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct MeasureKey {
  /// Stable identifier for the measured box.
  ///
  /// Most nodes have a deterministic `BoxNode::id`; however, callers can construct `BoxNode`s
  /// directly without running them through `BoxTree::new` (which assigns ids). For those ephemeral
  /// nodes we fall back to a pointer-derived sentinel id (see `ensure_box_id`).
  box_id: usize,
  /// Pointer identity for the base style `Arc<ComputedStyle>`.
  ///
  /// Style overrides are applied via TLS without changing the `Arc` pointer on `BoxNode`, so the
  /// active override fingerprint is tracked separately.
  style_ptr: usize,
  /// Fingerprint for the currently active style override (if any).
  ///
  /// `style_override` intentionally avoids allocating a new `BoxNode` subtree, so caching must
  /// account for override state to avoid reusing measurements computed under a different effective
  /// style.
  override_fingerprint: Option<u64>,
  known_width: Option<u32>,
  known_height: Option<u32>,
  available_width: MeasureAvailKey,
  available_height: MeasureAvailKey,
}

const EPHEMERAL_BOX_ID_BASE: usize = 1usize << (usize::BITS - 1);

#[cfg(not(test))]
const GRID_MEASURE_SIZE_CACHE_MAX_ENTRIES: usize = 262_144;

#[cfg(not(test))]
const GRID_MEASURE_SIZE_CACHE_EVICTION_BATCH: usize = 16_384;

fn grid_measure_size_cache_store_with_policy(
  cache: &mut FxHashMap<MeasureKey, taffy::tree::MeasureOutput>,
  key: MeasureKey,
  output: taffy::tree::MeasureOutput,
  max_entries: usize,
  eviction_batch: usize,
) {
  if max_entries == 0 {
    cache.clear();
    return;
  }
  if cache.len() >= max_entries && !cache.contains_key(&key) {
    // The grid measure cache can grow extremely large on pages with many grid items + probes.
    // Clearing the cache in this case causes avoidable thrash (re-measuring recently-seen nodes).
    // Instead, evict a bounded batch of entries to make room while keeping reuse stable.
    let eviction_batch = eviction_batch.max(1).min(max_entries).min(cache.len());
    if eviction_batch > 0 {
      let keys: Vec<_> = cache.keys().take(eviction_batch).cloned().collect();
      for key in keys {
        cache.remove(&key);
      }
    }
  }
  cache.insert(key, output);
}

#[cfg(not(test))]
thread_local! {
  /// Cross-invocation cache for grid item measurements keyed by quantized constraints.
  ///
  /// Grid containers can be laid out repeatedly during track sizing and intrinsic measurement (and
  /// each layout call triggers a fresh Taffy tree + measurement pass). Persisting measurements per
  /// render avoids re-running nested formatting-context layout for the same items across those
  /// invocations.
  static GRID_MEASURE_SIZE_CACHE: RefCell<FxHashMap<MeasureKey, taffy::tree::MeasureOutput>> =
    RefCell::new(FxHashMap::default());
  static GRID_MEASURE_SIZE_CACHE_EPOCH: Cell<usize> = const { Cell::new(0) };
}

#[cfg(not(test))]
fn grid_measure_size_cache_use_epoch() {
  let epoch = intrinsic_cache_epoch().max(1);
  GRID_MEASURE_SIZE_CACHE_EPOCH.with(|cell| {
    if cell.get() != epoch {
      cell.set(epoch);
      GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow_mut().clear());
    }
  });
}

#[cfg(not(test))]
fn grid_measure_size_cache_lookup(key: &MeasureKey) -> Option<taffy::tree::MeasureOutput> {
  if (key.box_id & EPHEMERAL_BOX_ID_BASE) != 0 {
    return None;
  }
  grid_measure_size_cache_use_epoch();
  GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow().get(key).copied())
}

#[cfg(test)]
fn grid_measure_size_cache_lookup(_key: &MeasureKey) -> Option<taffy::tree::MeasureOutput> {
  None
}

#[cfg(not(test))]
fn grid_measure_size_cache_store(key: MeasureKey, output: taffy::tree::MeasureOutput) {
  if (key.box_id & EPHEMERAL_BOX_ID_BASE) != 0 {
    return;
  }
  grid_measure_size_cache_use_epoch();
  GRID_MEASURE_SIZE_CACHE.with(|cache| {
    let mut cache = cache.borrow_mut();
    grid_measure_size_cache_store_with_policy(
      &mut cache,
      key,
      output,
      GRID_MEASURE_SIZE_CACHE_MAX_ENTRIES,
      GRID_MEASURE_SIZE_CACHE_EVICTION_BATCH,
    );
  });
}

#[cfg(test)]
fn grid_measure_size_cache_store(_key: MeasureKey, _output: taffy::tree::MeasureOutput) {}

impl MeasureKey {
  fn quantize(val: f32) -> f32 {
    let abs = val.abs();
    let step = if abs > 4096.0 {
      64.0
    } else if abs > 2048.0 {
      32.0
    } else if abs > 1024.0 {
      16.0
    } else if abs > 512.0 {
      8.0
    } else if abs > 256.0 {
      4.0
    } else {
      2.0
    };
    let quantized = (val / step).round() * step;
    if quantized == 0.0 {
      0.0
    } else {
      quantized
    }
  }

  fn quantize_to_bits(val: f32) -> u32 {
    Self::quantize(val).to_bits()
  }

  fn sanitize_definite(val: f32) -> f32 {
    if val.is_finite() {
      val.max(0.0)
    } else {
      0.0
    }
  }

  fn new(
    node: &BoxNode,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
    available_space: taffy::geometry::Size<taffy::style::AvailableSpace>,
    _viewport: Size,
    drop_available_height: bool,
  ) -> Self {
    fn avail_key(space: taffy::style::AvailableSpace) -> MeasureAvailKey {
      match space {
        taffy::style::AvailableSpace::Definite(v) => {
          let v = MeasureKey::sanitize_definite(v);
          if v <= 1.0 {
            MeasureAvailKey::Indefinite
          } else {
            MeasureAvailKey::Definite(MeasureKey::quantize_to_bits(v))
          }
        }
        taffy::style::AvailableSpace::MinContent => MeasureAvailKey::MinContent,
        taffy::style::AvailableSpace::MaxContent => MeasureAvailKey::MaxContent,
      }
    }

    let known_width = known_dimensions
      .width
      .map(|w| Self::quantize_to_bits(Self::sanitize_definite(w)));
    let known_height = known_dimensions
      .height
      .map(|h| Self::quantize_to_bits(Self::sanitize_definite(h)));
    let box_id = ensure_box_id(node);
    let override_fingerprint =
      crate::layout::style_override::style_override_fingerprint_for(box_id);

    Self {
      box_id,
      style_ptr: Arc::as_ptr(&node.style) as usize,
      override_fingerprint,
      known_width,
      known_height,
      available_width: if known_width.is_some() {
        MeasureAvailKey::Ignored
      } else {
        avail_key(available_space.width)
      },
      available_height: if known_height.is_some() {
        MeasureAvailKey::Ignored
      } else if drop_available_height {
        MeasureAvailKey::Indefinite
      } else {
        avail_key(available_space.height)
      },
    }
  }

  fn new_with_snapped_sizes(
    node: &BoxNode,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
    available_space: taffy::geometry::Size<taffy::style::AvailableSpace>,
    viewport: Size,
    drop_available_height: bool,
  ) -> (
    Self,
    taffy::geometry::Size<Option<f32>>,
    taffy::geometry::Size<taffy::style::AvailableSpace>,
  ) {
    let key = Self::new(
      node,
      known_dimensions,
      available_space,
      viewport,
      drop_available_height,
    );

    let known_dimensions = taffy::geometry::Size {
      width: key.known_width.map(f32::from_bits),
      height: key.known_height.map(f32::from_bits),
    };

    let available_width = match key.available_width {
      MeasureAvailKey::Definite(bits) => {
        taffy::style::AvailableSpace::Definite(f32::from_bits(bits))
      }
      MeasureAvailKey::Indefinite => taffy::style::AvailableSpace::Definite(0.0),
      MeasureAvailKey::MinContent => taffy::style::AvailableSpace::MinContent,
      MeasureAvailKey::MaxContent => taffy::style::AvailableSpace::MaxContent,
      MeasureAvailKey::Ignored => {
        taffy::style::AvailableSpace::Definite(key.known_width.map(f32::from_bits).unwrap_or(0.0))
      }
    };

    let available_height = match key.available_height {
      MeasureAvailKey::Definite(bits) => {
        taffy::style::AvailableSpace::Definite(f32::from_bits(bits))
      }
      MeasureAvailKey::Indefinite => taffy::style::AvailableSpace::Definite(0.0),
      MeasureAvailKey::MinContent => taffy::style::AvailableSpace::MinContent,
      MeasureAvailKey::MaxContent => taffy::style::AvailableSpace::MaxContent,
      MeasureAvailKey::Ignored => {
        taffy::style::AvailableSpace::Definite(key.known_height.map(f32::from_bits).unwrap_or(0.0))
      }
    };

    let available_space = taffy::geometry::Size {
      width: available_width,
      height: available_height,
    };

    (key, known_dimensions, available_space)
  }
}

fn height_depends_on_available_height(style: &ComputedStyle) -> bool {
  let height_depends = style.height.as_ref().is_some_and(Length::has_percentage)
    || style
      .height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));
  let min_depends = style
    .min_height
    .as_ref()
    .is_some_and(Length::has_percentage)
    || style
      .min_height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));
  let max_depends = style
    .max_height
    .as_ref()
    .is_some_and(Length::has_percentage)
    || style
      .max_height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));

  height_depends || min_depends || max_depends
}

#[inline]
fn physical_width_is_auto(style: &ComputedStyle) -> bool {
  style.width.is_none() && style.width_keyword.is_none()
}

#[inline]
fn physical_height_is_auto(style: &ComputedStyle) -> bool {
  style.height.is_none() && style.height_keyword.is_none()
}

fn node_or_in_flow_children_depend_on_available_height(node: &BoxNode) -> bool {
  if height_depends_on_available_height(&node.style) {
    return true;
  }

  // Absolute/fixed-positioned boxes don't contribute to a parent's intrinsic block size, so don't
  // force cache key separation based on their percentage heights.
  node.children.iter().any(|child| {
    child.style.position.is_in_flow() && height_depends_on_available_height(&child.style)
  })
}

fn push_measured_key(keys: &mut Vec<MeasureKey>, key: MeasureKey) -> Option<MeasureKey> {
  // Keep the list small, but de-dup and move recently seen keys to the end so we have a better
  // chance of reusing measured fragments when Taffy cycles through multiple sizing modes.
  if let Some(pos) = keys.iter().position(|existing| *existing == key) {
    keys.remove(pos);
  }
  let evicted = if keys.len() >= MAX_MEASURED_KEYS_PER_NODE {
    Some(keys.remove(0))
  } else {
    None
  };
  keys.push(key);
  evicted
}

#[cfg(test)]
thread_local! {
  // Grid measure callbacks can be triggered during many unrelated tests. Keep the counter
  // thread-local so tests can make assertions without racing other parallel test threads.
  static GRID_MEASURE_LAYOUT_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_grid_measure_layout_calls() {
  GRID_MEASURE_LAYOUT_CALLS.with(|counter| counter.set(0));
}

#[cfg(test)]
fn grid_measure_layout_calls() -> usize {
  GRID_MEASURE_LAYOUT_CALLS.with(|counter| counter.get())
}

#[cfg(test)]
fn record_measure_layout_call() {
  GRID_MEASURE_LAYOUT_CALLS.with(|counter| counter.set(counter.get() + 1));
}

#[cfg(not(test))]
fn record_measure_layout_call() {}

fn record_fragment_clone(site: CloneSite, fragment: &FragmentNode) {
  fragment_clone_profile::record_fragment_clone_from_fragment(site, fragment);
}

fn ensure_box_id(node: &BoxNode) -> usize {
  if node.id != 0 {
    return node.id;
  }
  EPHEMERAL_BOX_ID_BASE | (node as *const BoxNode as usize)
}

fn is_collapsible_ascii_whitespace_only(text: &str) -> bool {
  // Only treat the ASCII whitespace characters that participate in CSS white-space collapsing as
  // collapsible. In particular, do not consider NBSP collapsible.
  text.chars().all(|ch| {
    matches!(
      ch,
      '\r' | '\n' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' | ' ' | '\t'
    )
  })
}

fn is_collapsible_whitespace_grid_item(node: &BoxNode) -> bool {
  // Inter-element whitespace commonly appears in HTML source, but whitespace-only text nodes that
  // collapse to empty should not generate grid items (matching browser behavior). If we keep these
  // nodes, Taffy will place them into grid tracks, shifting real items and expanding the implicit
  // grid.
  if !matches!(
    node.style.white_space,
    WhiteSpace::Normal | WhiteSpace::Nowrap
  ) {
    return false;
  }

  fn subtree_is_whitespace_only(node: &BoxNode) -> bool {
    match &node.box_type {
      crate::tree::box_tree::BoxType::Text(text) => {
        is_collapsible_ascii_whitespace_only(&text.text)
      }
      crate::tree::box_tree::BoxType::Marker(marker) => match &marker.content {
        crate::tree::box_tree::MarkerContent::Text(text) => {
          is_collapsible_ascii_whitespace_only(text)
        }
        crate::tree::box_tree::MarkerContent::Image(_) => false,
      },
      crate::tree::box_tree::BoxType::Anonymous(_) => {
        node.children.iter().all(subtree_is_whitespace_only)
      }
      _ => false,
    }
  }

  match &node.box_type {
    crate::tree::box_tree::BoxType::Text(text) => is_collapsible_ascii_whitespace_only(&text.text),
    crate::tree::box_tree::BoxType::Marker(marker) => match &marker.content {
      crate::tree::box_tree::MarkerContent::Text(text) => {
        is_collapsible_ascii_whitespace_only(text)
      }
      crate::tree::box_tree::MarkerContent::Image(_) => false,
    },
    crate::tree::box_tree::BoxType::Anonymous(anon)
      if matches!(
        anon.anonymous_type,
        crate::tree::box_tree::AnonymousType::Block | crate::tree::box_tree::AnonymousType::Inline
      ) =>
    {
      subtree_is_whitespace_only(node)
    }
    _ => false,
  }
}

fn constraints_from_taffy(
  _viewport_size: crate::geometry::Size,
  mut known: taffy::geometry::Size<Option<f32>>,
  available: taffy::geometry::Size<taffy::style::AvailableSpace>,
  inline_percentage_base: Option<f32>,
) -> LayoutConstraints {
  // Taffy can probe leaf nodes with "tiny" definite constraints (0px/1px) when it really means
  // "unknown"/unconstrained. Our integration treats these as indefinite; normalize known sizes in
  // the same cases so we don't accidentally force a 0px layout and then cache it.
  if let Some(w) = known.width {
    if w <= 1.0
      && matches!(
        available.width,
        taffy::style::AvailableSpace::Definite(v) if v <= 1.0
      )
    {
      known.width = None;
    }
  }
  if let Some(h) = known.height {
    if h <= 1.0
      && matches!(
        available.height,
        taffy::style::AvailableSpace::Definite(v) if v <= 1.0
      )
    {
      known.height = None;
    }
  }

  let sanitize_definite = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
  let known_width = known.width.map(sanitize_definite);
  let known_height = known.height.map(sanitize_definite);
  let width = match (known_width, available.width) {
    (Some(w), _) => CrateAvailableSpace::Definite(w),
    (_, taffy::style::AvailableSpace::Definite(w)) => {
      if w <= 1.0 {
        CrateAvailableSpace::Indefinite
      } else {
        CrateAvailableSpace::Definite(sanitize_definite(w))
      }
    }
    (_, taffy::style::AvailableSpace::MinContent) => CrateAvailableSpace::MinContent,
    (_, taffy::style::AvailableSpace::MaxContent) => CrateAvailableSpace::MaxContent,
  };
  let height = match (known_height, available.height) {
    (Some(h), _) => CrateAvailableSpace::Definite(h),
    (_, taffy::style::AvailableSpace::Definite(h)) => {
      if h <= 1.0 {
        CrateAvailableSpace::Indefinite
      } else {
        CrateAvailableSpace::Definite(sanitize_definite(h))
      }
    }
    (_, taffy::style::AvailableSpace::MinContent) => CrateAvailableSpace::MinContent,
    (_, taffy::style::AvailableSpace::MaxContent) => CrateAvailableSpace::MaxContent,
  };

  let mut constraints = LayoutConstraints::new(width, height);
  constraints.used_border_box_width = known_width;
  constraints.used_border_box_height = known_height;
  constraints.inline_percentage_base = constraints
    .inline_percentage_base
    .or(inline_percentage_base)
    .or(match available.width {
      taffy::style::AvailableSpace::Definite(w) if w > 1.0 => Some(w),
      _ => None,
    });
  constraints
}

/// Grid Formatting Context
///
/// Implements CSS Grid layout by delegating to the Taffy library.
/// Each layout operation creates a fresh Taffy tree, performs layout,
/// and converts results back to FragmentNode.
///
/// # Thread Safety
///
/// GridFormattingContext is stateless and can be shared across threads.
/// Each layout operation is independent and creates its own Taffy tree.
///
/// # Example
///
/// ```ignore
/// use fastrender::layout::contexts::GridFormattingContext;
/// use fastrender::{FormattingContext, LayoutConstraints};
///
/// let fc = GridFormattingContext::new();
/// let fragment = fc.layout(&box_node, &constraints)?;
/// ```
#[derive(Clone)]
pub struct GridFormattingContext {
  /// Shared factory used to create child formatting contexts without losing shared caches.
  factory: FormattingContextFactory,
  viewport_size: crate::geometry::Size,
  font_context: crate::text::font_loader::FontContext,
  nearest_positioned_cb: crate::layout::contexts::positioned::ContainingBlock,
  nearest_fixed_cb: crate::layout::contexts::positioned::ContainingBlock,
  taffy_cache: std::sync::Arc<crate::layout::taffy_integration::TaffyNodeCache>,
  parallelism: LayoutParallelism,
}

impl GridFormattingContext {
  fn grid_item_continuation_available_block_size(
    &self,
    bounds: Rect,
    constraints: &LayoutConstraints,
    containing_grid_axis: Option<GridAxisStyle>,
  ) -> Option<f32> {
    let axis_style = containing_grid_axis?;
    if !axis_style.inline_is_horizontal() || !axis_style.block_positive() {
      return None;
    }

    // Only attempt continuation adjustments when we're in an actual fragmentation context.
    // Without a fragmentainer hint, the grid item is simply overflowing its containing block,
    // not flowing into a continuation fragment.
    let fragmentainer_block_size =
      fragmentainer_block_size_hint().filter(|size| size.is_finite() && *size > 0.0)?;
    let first_fragment_block_size = match constraints.available_height {
      CrateAvailableSpace::Definite(h) if h.is_finite() && h > 0.0 => {
        h.min(fragmentainer_block_size)
      }
      _ => fragmentainer_block_size,
    };

    let block_start = bounds.y();
    if !block_start.is_finite() {
      return None;
    }

    // Only adjust in continuation fragments. This is a best-effort heuristic based on the
    // box's flow position relative to the fragmentainer size.
    //
    // https://www.w3.org/TR/css-grid-2/#fragmentation
    const EPSILON: f32 = 0.01;
    if block_start + EPSILON < first_fragment_block_size {
      return None;
    }

    let relative = (block_start - first_fragment_block_size).max(0.0);
    let fragment_index =
      1usize.saturating_add((relative / fragmentainer_block_size).floor() as usize);
    let consumed = first_fragment_block_size
      + (fragment_index.saturating_sub(1) as f32) * fragmentainer_block_size;
    let remaining = (fragmentainer_block_size - consumed).max(0.0);
    Some(remaining)
  }

  fn is_simple_grid(
    &self,
    style: &ComputedStyle,
    children: &[&BoxNode],
    deadline_counter: &mut usize,
  ) -> Result<bool, LayoutError> {
    if !matches!(style.display, CssDisplay::Grid | CssDisplay::InlineGrid) {
      return Ok(false);
    }
    if style.grid_row_subgrid || style.grid_column_subgrid {
      return Ok(false);
    }
    if !style.grid_template_columns.is_empty() || !style.grid_template_rows.is_empty() {
      return Ok(false);
    }
    if !style.grid_template_areas.is_empty()
      || !style.grid_column_names.is_empty()
      || !style.grid_row_names.is_empty()
      || !style.grid_column_line_names.is_empty()
      || !style.grid_row_line_names.is_empty()
    {
      return Ok(false);
    }
    let auto_track =
      |tracks: &[GridTrack], deadline_counter: &mut usize| -> Result<bool, LayoutError> {
        for track in tracks {
          check_layout_deadline(deadline_counter)?;
          if !matches!(track, GridTrack::Auto) {
            return Ok(false);
          }
        }
        Ok(true)
      };
    if !auto_track(&style.grid_auto_rows, deadline_counter)?
      || !auto_track(&style.grid_auto_columns, deadline_counter)?
    {
      return Ok(false);
    }
    if style.grid_gap.value != 0.0
      || style.grid_row_gap.value != 0.0
      || style.grid_column_gap.value != 0.0
    {
      return Ok(false);
    }
    if style.grid_auto_flow != GridAutoFlow::Row {
      return Ok(false);
    }
    if style.align_items != AlignItems::Stretch
      || style.justify_items != AlignItems::Stretch
      || style.align_content != AlignContent::Stretch
      || style.justify_content != JustifyContent::FlexStart
    {
      return Ok(false);
    }

    for child in children {
      check_layout_deadline(deadline_counter)?;
      let cs = &child.style;
      if cs.grid_column_start != 0
        || cs.grid_column_end != 0
        || cs.grid_row_start != 0
        || cs.grid_row_end != 0
        || cs.align_self.is_some()
        || cs.justify_self.is_some()
      {
        return Ok(false);
      }
    }

    Ok(true)
  }

  /// Creates a new GridFormattingContext
  pub fn new() -> Self {
    let viewport_size = crate::geometry::Size::new(800.0, 600.0);
    Self::with_viewport_and_cb(
      viewport_size,
      crate::layout::contexts::positioned::ContainingBlock::viewport(viewport_size),
      crate::text::font_loader::FontContext::new(),
    )
  }

  // NOTE: Grid axis mapping logic (writing-mode + direction and subgrid inheritance) lives in
  // `GridAxisStyle`; avoid introducing parallel helpers in `GridFormattingContext`.

  pub fn with_viewport(viewport_size: crate::geometry::Size) -> Self {
    Self::with_viewport_and_cb(
      viewport_size,
      crate::layout::contexts::positioned::ContainingBlock::viewport(viewport_size),
      crate::text::font_loader::FontContext::new(),
    )
  }

  pub fn with_viewport_and_cb(
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: crate::layout::contexts::positioned::ContainingBlock,
    font_context: crate::text::font_loader::FontContext,
  ) -> Self {
    Self::with_viewport_cb_and_cache(
      viewport_size,
      nearest_positioned_cb,
      font_context,
      std::sync::Arc::new(TaffyNodeCache::new(taffy_template_cache_limit(
        TaffyAdapterKind::Grid,
      ))),
    )
  }

  pub fn with_viewport_cb_and_cache(
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: crate::layout::contexts::positioned::ContainingBlock,
    font_context: crate::text::font_loader::FontContext,
    taffy_cache: std::sync::Arc<crate::layout::taffy_integration::TaffyNodeCache>,
  ) -> Self {
    let factory = FormattingContextFactory::with_font_context_viewport_cb_and_cache(
      font_context.clone(),
      viewport_size,
      nearest_positioned_cb,
      std::sync::Arc::new(ShardedFlexCache::new_measure()),
      std::sync::Arc::new(ShardedFlexCache::new_layout()),
      std::sync::Arc::new(TaffyNodeCache::new(taffy_template_cache_limit(
        TaffyAdapterKind::Flex,
      ))),
      taffy_cache.clone(),
    );
    Self::with_factory(factory)
  }

  pub(crate) fn with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let parallelism = factory.parallelism();
    let taffy_cache = factory.grid_taffy_cache();
    Self {
      factory,
      viewport_size,
      font_context,
      nearest_positioned_cb,
      nearest_fixed_cb,
      taffy_cache,
      parallelism,
    }
  }

  pub fn with_parallelism(mut self, parallelism: LayoutParallelism) -> Self {
    self.parallelism = parallelism;
    self.factory = self.factory.clone().with_parallelism(parallelism);
    self.taffy_cache = self.factory.grid_taffy_cache();
    self
  }

  fn horizontal_edges_px(&self, style: &ComputedStyle) -> Option<f32> {
    let left = self.resolve_length_px(&style.padding_left, style)?;
    let right = self.resolve_length_px(&style.padding_right, style)?;
    let bl = self.resolve_length_px(&style.used_border_left_width(), style)?;
    let br = self.resolve_length_px(&style.used_border_right_width(), style)?;
    Some(left + right + bl + br)
  }

  fn vertical_edges_px(&self, style: &ComputedStyle) -> Option<f32> {
    let top = self.resolve_length_px(&style.padding_top, style)?;
    let bottom = self.resolve_length_px(&style.padding_bottom, style)?;
    let bt = self.resolve_length_px(&style.used_border_top_width(), style)?;
    let bb = self.resolve_length_px(&style.used_border_bottom_width(), style)?;
    Some(top + bottom + bt + bb)
  }

  fn resolve_length_for_width(
    &self,
    length: Length,
    percentage_base: f32,
    style: &ComputedStyle,
  ) -> f32 {
    let base = if percentage_base.is_finite() {
      Some(percentage_base)
    } else {
      None
    };
    resolve_length_with_percentage_metrics(
      length,
      base,
      self.viewport_size,
      style.font_size,
      style.root_font_size,
      Some(style),
      Some(&self.font_context),
    )
    .unwrap_or(0.0)
  }

  fn resolve_length_px_with_base(
    &self,
    length: Length,
    percentage_base: Option<f32>,
    style: &ComputedStyle,
  ) -> Option<f32> {
    resolve_length_with_percentage_metrics(
      length,
      percentage_base,
      self.viewport_size,
      style.font_size,
      style.root_font_size,
      Some(style),
      Some(&self.font_context),
    )
  }

  fn specified_size_for_border_box(
    &self,
    border_box: f32,
    edges: f32,
    box_sizing: BoxSizing,
  ) -> f32 {
    match box_sizing {
      BoxSizing::BorderBox => border_box.max(0.0),
      BoxSizing::ContentBox => (border_box - edges).max(0.0),
    }
  }

  fn intrinsic_keyword_size_from_min_max(
    &self,
    keyword: IntrinsicSizeKeyword,
    min: f32,
    max: f32,
    available: Option<f32>,
    style: &ComputedStyle,
    edges: f32,
  ) -> f32 {
    match keyword {
      IntrinsicSizeKeyword::MinContent => min,
      IntrinsicSizeKeyword::MaxContent => max,
      IntrinsicSizeKeyword::FillAvailable => {
        available.filter(|v| v.is_finite()).unwrap_or(max).max(0.0)
      }
      IntrinsicSizeKeyword::FitContent { limit } => {
        let preferred = limit
          .and_then(|limit| self.resolve_length_px_with_base(limit, available, style))
          .map(|px| border_size_from_box_sizing(px, edges, style.box_sizing));
        crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
          available, preferred, min, max,
        )
      }
    }
  }

  fn intrinsic_min_max_for_physical_axis(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Result<(f32, f32), LayoutError> {
    let fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let fc = self.factory.get(fc_type);
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let axis_is_inline = match axis {
      Axis::Horizontal => inline_is_horizontal,
      Axis::Vertical => !inline_is_horizontal,
    };

    if axis_is_inline {
      match fc.compute_intrinsic_inline_sizes(box_node) {
        Ok((min, max)) => Ok((min.max(0.0), max.max(0.0))),
        Err(err @ LayoutError::Timeout { .. }) => Err(err),
        Err(_) => Ok((0.0, 0.0)),
      }
    } else {
      // When resolving intrinsic sizing keywords we need the box's *content-driven* sizes, which
      // must not depend on the box's own intrinsic keyword constraints along the same physical
      // axis. In vertical writing modes, `width: max-content` maps to the block axis; querying the
      // block intrinsic size without clearing that keyword would re-enter grid layout and recurse
      // indefinitely (stack overflow).
      //
      // Clear any intrinsic keyword sizing on the physical axis we're measuring so the nested
      // layout pass treats it as `auto` instead of trying to resolve the same keyword again.
      let needs_keyword_clear = match axis {
        Axis::Horizontal => {
          style.width_keyword.is_some()
            || style.min_width_keyword.is_some()
            || style.max_width_keyword.is_some()
        }
        Axis::Vertical => {
          style.height_keyword.is_some()
            || style.min_height_keyword.is_some()
            || style.max_height_keyword.is_some()
        }
      };

      let compute = |mode| -> Result<f32, LayoutError> {
        if needs_keyword_clear && box_node.id != 0 {
          let mut cleared: ComputedStyle = style.clone();
          match axis {
            Axis::Horizontal => {
              cleared.width = None;
              cleared.width_keyword = None;
              cleared.min_width_keyword = None;
              cleared.max_width_keyword = None;
            }
            Axis::Vertical => {
              cleared.height = None;
              cleared.height_keyword = None;
              cleared.min_height_keyword = None;
              cleared.max_height_keyword = None;
            }
          }
          crate::layout::style_override::with_style_override(box_node.id, Arc::new(cleared), || {
            fc.compute_intrinsic_block_size(box_node, mode)
          })
        } else {
          fc.compute_intrinsic_block_size(box_node, mode)
        }
      };

      let min = match compute(IntrinsicSizingMode::MinContent) {
        Ok(value) => value.max(0.0),
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => 0.0,
      };
      let max = match compute(IntrinsicSizingMode::MaxContent) {
        Ok(value) => value.max(0.0),
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => min,
      };
      Ok((min, max))
    }
  }

  fn definite_physical_available_size(
    &self,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
    axis: Axis,
  ) -> Option<f32> {
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let space = match axis {
      Axis::Horizontal => {
        if inline_is_horizontal {
          constraints.available_width
        } else {
          constraints.available_height
        }
      }
      Axis::Vertical => {
        if inline_is_horizontal {
          constraints.available_height
        } else {
          constraints.available_width
        }
      }
    };
    match space {
      CrateAvailableSpace::Definite(value) if value.is_finite() && value >= 0.0 => Some(value),
      _ => None,
    }
  }

  fn resolve_intrinsic_sizing_keywords_for_node(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    available_width: Option<f32>,
    available_height: Option<f32>,
    resolve_fit_content: bool,
  ) -> Result<Option<Arc<ComputedStyle>>, LayoutError> {
    if box_node.id == 0 {
      return Ok(None);
    }

    // Resolving intrinsic sizing keywords (`min/max/fit-content`) requires querying the intrinsic
    // size of *this same node*. Those intrinsic size queries can re-enter grid layout, which in
    // turn will attempt to resolve intrinsic keywords again, causing infinite recursion
    // (manifesting as a stack overflow).
    //
    // Avoid this by clearing any intrinsic sizing keywords on the axis being measured while we
    // compute the node's intrinsic min/max sizes. We still apply the authored keyword constraints
    // after measuring by clamping against the computed intrinsic min/max values.
    let cleared_style_for_axis = |axis: Axis| -> Option<Arc<ComputedStyle>> {
      let mut next_style: ComputedStyle = style.clone();
      let mut changed = false;
      match axis {
        Axis::Horizontal => {
          if next_style.width_keyword.is_some() {
            next_style.width = None;
            next_style.width_keyword = None;
            changed = true;
          }
          if next_style.min_width_keyword.is_some() {
            next_style.min_width = None;
            next_style.min_width_keyword = None;
            changed = true;
          }
          if next_style.max_width_keyword.is_some() {
            next_style.max_width = None;
            next_style.max_width_keyword = None;
            changed = true;
          }
        }
        Axis::Vertical => {
          if next_style.height_keyword.is_some() {
            next_style.height = None;
            next_style.height_keyword = None;
            changed = true;
          }
          if next_style.min_height_keyword.is_some() {
            next_style.min_height = None;
            next_style.min_height_keyword = None;
            changed = true;
          }
          if next_style.max_height_keyword.is_some() {
            next_style.max_height = None;
            next_style.max_height_keyword = None;
            changed = true;
          }
        }
      }
      changed.then(|| Arc::new(next_style))
    };

    let should_resolve_keyword = |keyword: IntrinsicSizeKeyword| match keyword {
      IntrinsicSizeKeyword::FitContent { .. } => resolve_fit_content,
      _ => true,
    };

    let has_horizontal_keyword = style
      .width_keyword
      .is_some_and(|kw| should_resolve_keyword(kw))
      || style
        .min_width_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
      || style
        .max_width_keyword
        .is_some_and(|kw| should_resolve_keyword(kw));
    let has_vertical_keyword = style
      .height_keyword
      .is_some_and(|kw| should_resolve_keyword(kw))
      || style
        .min_height_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
      || style
        .max_height_keyword
        .is_some_and(|kw| should_resolve_keyword(kw));
    if !(has_horizontal_keyword || has_vertical_keyword) {
      return Ok(None);
    }

    let horizontal_edges = self.edges_px(style, Axis::Horizontal).unwrap_or(0.0);
    let vertical_edges = self.edges_px(style, Axis::Vertical).unwrap_or(0.0);

    let (intrinsic_min_w, intrinsic_max_w) = if has_horizontal_keyword {
      if let Some(cleared) = cleared_style_for_axis(Axis::Horizontal) {
        crate::layout::style_override::with_style_override(box_node.id, cleared.clone(), || {
          self.intrinsic_min_max_for_physical_axis(box_node, cleared.as_ref(), Axis::Horizontal)
        })?
      } else {
        self.intrinsic_min_max_for_physical_axis(box_node, style, Axis::Horizontal)?
      }
    } else {
      (0.0, 0.0)
    };
    let (intrinsic_min_h, intrinsic_max_h) = if has_vertical_keyword {
      if let Some(cleared) = cleared_style_for_axis(Axis::Vertical) {
        crate::layout::style_override::with_style_override(box_node.id, cleared.clone(), || {
          self.intrinsic_min_max_for_physical_axis(box_node, cleared.as_ref(), Axis::Vertical)
        })?
      } else {
        self.intrinsic_min_max_for_physical_axis(box_node, style, Axis::Vertical)?
      }
    } else {
      (0.0, 0.0)
    };

    let mut min_width_border_box = if let Some(keyword) = style
      .min_width_keyword
      .filter(|kw| should_resolve_keyword(*kw))
    {
      self.intrinsic_keyword_size_from_min_max(
        keyword,
        intrinsic_min_w,
        intrinsic_max_w,
        available_width,
        style,
        horizontal_edges,
      )
    } else if let Some(length) = style.min_width {
      self
        .resolve_length_px_with_base(length, available_width, style)
        .map(|px| border_size_from_box_sizing(px, horizontal_edges, style.box_sizing))
        .unwrap_or(0.0)
    } else {
      0.0
    };
    let mut max_width_border_box = if let Some(keyword) = style
      .max_width_keyword
      .filter(|kw| should_resolve_keyword(*kw))
    {
      self.intrinsic_keyword_size_from_min_max(
        keyword,
        intrinsic_min_w,
        intrinsic_max_w,
        available_width,
        style,
        horizontal_edges,
      )
    } else if let Some(length) = style.max_width {
      self
        .resolve_length_px_with_base(length, available_width, style)
        .map(|px| border_size_from_box_sizing(px, horizontal_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
    } else {
      f32::INFINITY
    };
    if max_width_border_box < min_width_border_box {
      max_width_border_box = min_width_border_box;
    }
    if !min_width_border_box.is_finite() {
      min_width_border_box = 0.0;
    }
    if !max_width_border_box.is_finite() {
      max_width_border_box = f32::INFINITY;
    }

    let mut min_height_border_box = if let Some(keyword) = style
      .min_height_keyword
      .filter(|kw| should_resolve_keyword(*kw))
    {
      self.intrinsic_keyword_size_from_min_max(
        keyword,
        intrinsic_min_h,
        intrinsic_max_h,
        available_height,
        style,
        vertical_edges,
      )
    } else if let Some(length) = style.min_height {
      self
        .resolve_length_px_with_base(length, available_height, style)
        .map(|px| border_size_from_box_sizing(px, vertical_edges, style.box_sizing))
        .unwrap_or(0.0)
    } else {
      0.0
    };
    let mut max_height_border_box = if let Some(keyword) = style
      .max_height_keyword
      .filter(|kw| should_resolve_keyword(*kw))
    {
      self.intrinsic_keyword_size_from_min_max(
        keyword,
        intrinsic_min_h,
        intrinsic_max_h,
        available_height,
        style,
        vertical_edges,
      )
    } else if let Some(length) = style.max_height {
      self
        .resolve_length_px_with_base(length, available_height, style)
        .map(|px| border_size_from_box_sizing(px, vertical_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
    } else {
      f32::INFINITY
    };
    if max_height_border_box < min_height_border_box {
      max_height_border_box = min_height_border_box;
    }
    if !min_height_border_box.is_finite() {
      min_height_border_box = 0.0;
    }
    if !max_height_border_box.is_finite() {
      max_height_border_box = f32::INFINITY;
    }

    let mut next_style: ComputedStyle = style.clone();
    let mut changed = false;

    if style
      .min_width_keyword
      .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified = self.specified_size_for_border_box(
        min_width_border_box,
        horizontal_edges,
        style.box_sizing,
      );
      next_style.min_width = Some(Length::px(specified));
      next_style.min_width_keyword = None;
      changed = true;
    }
    if style
      .max_width_keyword
      .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified = self.specified_size_for_border_box(
        max_width_border_box,
        horizontal_edges,
        style.box_sizing,
      );
      next_style.max_width = Some(Length::px(specified));
      next_style.max_width_keyword = None;
      changed = true;
    }
    if let Some(keyword) = style.width_keyword.filter(|kw| should_resolve_keyword(*kw)) {
      let base = self.intrinsic_keyword_size_from_min_max(
        keyword,
        intrinsic_min_w,
        intrinsic_max_w,
        available_width,
        style,
        horizontal_edges,
      );
      let border_box = clamp_with_order(base, min_width_border_box, max_width_border_box);
      let specified =
        self.specified_size_for_border_box(border_box, horizontal_edges, style.box_sizing);
      next_style.width = Some(Length::px(specified));
      next_style.width_keyword = None;
      changed = true;
    }

    if style
      .min_height_keyword
      .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified =
        self.specified_size_for_border_box(min_height_border_box, vertical_edges, style.box_sizing);
      next_style.min_height = Some(Length::px(specified));
      next_style.min_height_keyword = None;
      changed = true;
    }
    if style
      .max_height_keyword
      .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified =
        self.specified_size_for_border_box(max_height_border_box, vertical_edges, style.box_sizing);
      next_style.max_height = Some(Length::px(specified));
      next_style.max_height_keyword = None;
      changed = true;
    }
    if let Some(keyword) = style
      .height_keyword
      .filter(|kw| should_resolve_keyword(*kw))
    {
      let base = self.intrinsic_keyword_size_from_min_max(
        keyword,
        intrinsic_min_h,
        intrinsic_max_h,
        available_height,
        style,
        vertical_edges,
      );
      let border_box = clamp_with_order(base, min_height_border_box, max_height_border_box);
      let specified =
        self.specified_size_for_border_box(border_box, vertical_edges, style.box_sizing);
      next_style.height = Some(Length::px(specified));
      next_style.height_keyword = None;
      changed = true;
    }

    if changed {
      Ok(Some(Arc::new(next_style)))
    } else {
      Ok(None)
    }
  }

  fn resolve_intrinsic_sizing_keywords_for_taffy_tree(
    &self,
    box_node: &BoxNode,
    root_children: &[&BoxNode],
    constraints: &LayoutConstraints,
    resolve_root: bool,
  ) -> Result<StyleOverrideStack, LayoutError> {
    let style_override = style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
    let available_width =
      self.definite_physical_available_size(style, constraints, Axis::Horizontal);
    let available_height =
      self.definite_physical_available_size(style, constraints, Axis::Vertical);

    let mut stack = StyleOverrideStack::new();
    let mut deadline_counter = 0usize;
    self.resolve_intrinsic_sizing_keywords_for_taffy_tree_inner(
      box_node,
      Some(root_children),
      true,
      resolve_root,
      available_width,
      available_height,
      &mut deadline_counter,
      &mut stack,
    )?;
    Ok(stack)
  }

  #[allow(clippy::too_many_arguments)]
  fn resolve_intrinsic_sizing_keywords_for_taffy_tree_inner(
    &self,
    box_node: &BoxNode,
    children_override: Option<&[&BoxNode]>,
    is_root: bool,
    resolve_this_node: bool,
    available_width: Option<f32>,
    available_height: Option<f32>,
    deadline_counter: &mut usize,
    stack: &mut StyleOverrideStack,
  ) -> Result<(), LayoutError> {
    check_layout_deadline(deadline_counter)?;

    let style_override = style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    let is_grid_container = matches!(
      box_node.formatting_context(),
      Some(FormattingContextType::Grid)
    );

    let mut include_children = is_root;
    let mut children_iter: Vec<&BoxNode> = Vec::new();
    let mut has_subgrid_child = false;

    if is_grid_container {
      let children_source: Vec<&BoxNode> = match children_override {
        Some(children) => children.to_vec(),
        None => box_node.children.iter().collect(),
      };
      for child in children_source {
        check_layout_deadline(deadline_counter)?;
        match child.style.position {
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
            continue;
          }
          _ => {
            if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
              has_subgrid_child = true;
            }
            children_iter.push(child);
          }
        }
      }

      let is_subgrid = style.grid_row_subgrid || style.grid_column_subgrid;
      include_children |= is_subgrid || has_subgrid_child;
    } else if is_root {
      children_iter = match children_override {
        Some(children) => children.to_vec(),
        None => box_node.children.iter().collect(),
      };
    }

    if include_children {
      for child in children_iter.iter() {
        self.resolve_intrinsic_sizing_keywords_for_taffy_tree_inner(
          child,
          None,
          false,
          true,
          available_width,
          available_height,
          deadline_counter,
          stack,
        )?;
      }
    }

    if resolve_this_node {
      if let Some(resolved) = self.resolve_intrinsic_sizing_keywords_for_node(
        box_node,
        style,
        available_width,
        available_height,
        is_root,
      )? {
        stack.push(push_style_override(box_node.id, resolved));
      }
    }

    Ok(())
  }

  fn resolved_padding_border_for_measure(
    &self,
    style: &ComputedStyle,
    percentage_base: f32,
  ) -> (f32, f32, f32, f32, f32, f32, f32, f32) {
    #[inline]
    fn length_is_zero(length: Length) -> bool {
      length.calc.is_none() && length.value == 0.0
    }

    let reserve_vertical_gutter = matches!(style.overflow_y, CssOverflow::Scroll)
      || (style.scrollbar_gutter.stable
        && matches!(style.overflow_y, CssOverflow::Auto | CssOverflow::Scroll));
    let reserve_horizontal_gutter = matches!(style.overflow_x, CssOverflow::Scroll)
      || (style.scrollbar_gutter.stable
        && matches!(style.overflow_x, CssOverflow::Auto | CssOverflow::Scroll));
    let gutter_width = if reserve_vertical_gutter || reserve_horizontal_gutter {
      resolve_scrollbar_width(style)
    } else {
      0.0
    };
    let border_left_len = style.used_border_left_width();
    let border_right_len = style.used_border_right_width();
    let border_top_len = style.used_border_top_width();
    let border_bottom_len = style.used_border_bottom_width();

    // Fast-path the common case (no padding/border/gutters) to avoid repeated length resolution
    // work in tight grid measurement loops.
    if gutter_width == 0.0
      && length_is_zero(style.padding_left)
      && length_is_zero(style.padding_right)
      && length_is_zero(style.padding_top)
      && length_is_zero(style.padding_bottom)
      && length_is_zero(border_left_len)
      && length_is_zero(border_right_len)
      && length_is_zero(border_top_len)
      && length_is_zero(border_bottom_len)
    {
      return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    }

    let resolve = |length: Length| {
      if length_is_zero(length) {
        0.0
      } else {
        self.resolve_length_for_width(length, percentage_base, style)
      }
    };

    let mut padding_left = resolve(style.padding_left);
    let mut padding_right = resolve(style.padding_right);
    let mut padding_top = resolve(style.padding_top);
    let mut padding_bottom = resolve(style.padding_bottom);

    // Reserve space for scrollbars when overflow/scrollbar-gutter request stable gutters. This
    // mirrors the block formatting context behaviour so grid measurement returns the true content
    // box size (excluding scrollbar gutters).
    if reserve_vertical_gutter && gutter_width > 0.0 {
      if style.scrollbar_gutter.both_edges {
        padding_left += gutter_width;
        padding_right += gutter_width;
      } else {
        padding_right += gutter_width;
      }
    }

    if reserve_horizontal_gutter && gutter_width > 0.0 {
      if style.scrollbar_gutter.both_edges {
        padding_top += gutter_width;
      }
      padding_bottom += gutter_width;
    }

    let border_left = resolve(border_left_len);
    let border_right = resolve(border_right_len);
    let border_top = resolve(border_top_len);
    let border_bottom = resolve(border_bottom_len);

    (
      padding_left,
      padding_right,
      padding_top,
      padding_bottom,
      border_left,
      border_right,
      border_top,
      border_bottom,
    )
  }

  fn content_box_size(
    &self,
    fragment: &FragmentNode,
    style: &ComputedStyle,
    percentage_base: f32,
  ) -> Size {
    let (
      padding_left,
      padding_right,
      padding_top,
      padding_bottom,
      border_left,
      border_right,
      border_top,
      border_bottom,
    ) = self.resolved_padding_border_for_measure(style, percentage_base);

    let content_width =
      (fragment.bounds.width() - padding_left - padding_right - border_left - border_right)
        .max(0.0);
    let content_height =
      (fragment.bounds.height() - padding_top - padding_bottom - border_top - border_bottom)
        .max(0.0);

    Size::new(content_width, content_height)
  }

  fn resolve_root_fit_content_border_box_size(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
    axis: Axis,
    limit: Option<Length>,
  ) -> Result<Option<f32>, LayoutError> {
    let avail = match axis {
      Axis::Horizontal => constraints.available_width,
      Axis::Vertical => constraints.available_height,
    };
    let physical_axis = match axis {
      Axis::Horizontal => PhysicalAxis::X,
      Axis::Vertical => PhysicalAxis::Y,
    };

    let width_base = constraints
      .width()
      .or(constraints.inline_percentage_base)
      .unwrap_or(self.viewport_size.width.max(0.0));
    let (
      padding_left,
      padding_right,
      padding_top,
      padding_bottom,
      border_left,
      border_right,
      border_top,
      border_bottom,
    ) = self.resolved_padding_border_for_measure(style, width_base);
    let edges_w = padding_left + padding_right + border_left + border_right;
    let edges_h = padding_top + padding_bottom + border_top + border_bottom;
    let axis_edges = match axis {
      Axis::Horizontal => edges_w,
      Axis::Vertical => edges_h,
    };

    let intrinsic_range = |node: &BoxNode| -> Result<(f32, f32), LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
        self,
        node,
        physical_axis,
      )
    };

    // Avoid self-recursion when intrinsic sizing needs to re-enter layout by clearing any
    // fit-content keywords on the container before measuring its intrinsic contributions.
    let mut cleared_style: ComputedStyle = (*style).clone();
    if matches!(
      cleared_style.width_keyword,
      Some(IntrinsicSizeKeyword::FitContent { .. })
    ) {
      cleared_style.width = None;
      cleared_style.width_keyword = None;
    }
    if matches!(
      cleared_style.height_keyword,
      Some(IntrinsicSizeKeyword::FitContent { .. })
    ) {
      cleared_style.height = None;
      cleared_style.height_keyword = None;
    }
    let cleared_style = Arc::new(cleared_style);
    let (min_intrinsic, max_intrinsic) = if box_node.id != 0 {
      crate::layout::style_override::with_style_override(box_node.id, cleared_style, || {
        intrinsic_range(box_node)
      })
    } else {
      let mut cloned = box_node.clone();
      cloned.style = cleared_style;
      intrinsic_range(&cloned)
    }?;

    let min_intrinsic = min_intrinsic.max(0.0);
    let max_intrinsic = max_intrinsic.max(0.0);
    let available_border_box = match avail {
      CrateAvailableSpace::Definite(v) => v.max(0.0),
      CrateAvailableSpace::MinContent => min_intrinsic,
      CrateAvailableSpace::MaxContent => max_intrinsic,
      CrateAvailableSpace::Indefinite => max_intrinsic,
    };

    let percentage_base_opt = match axis {
      Axis::Horizontal => constraints.width().or(constraints.inline_percentage_base),
      Axis::Vertical => constraints.height(),
    };
    let resolve_length_px = |len: Length| -> Option<f32> {
      if len.has_percentage() && percentage_base_opt.is_none() {
        return None;
      }
      let base = match axis {
        Axis::Horizontal => percentage_base_opt.unwrap_or(self.viewport_size.width.max(0.0)),
        Axis::Vertical => percentage_base_opt.unwrap_or(self.viewport_size.height.max(0.0)),
      };
      Some(self.resolve_length_for_width(len, base, style).max(0.0))
    };
    let to_border_box = |value: f32| -> f32 {
      if style.box_sizing == BoxSizing::ContentBox {
        (value + axis_edges).max(0.0)
      } else {
        value.max(0.0)
      }
    };

    let preferred_border_box = match limit {
      None => None,
      Some(limit) => {
        let Some(resolved) = resolve_length_px(limit) else {
          return Ok(None);
        };
        Some(to_border_box(resolved))
      }
    };

    let mut border_box = crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
      Some(available_border_box),
      preferred_border_box,
      min_intrinsic,
      max_intrinsic,
    );

    // Apply authored min/max constraints on the axis, including intrinsic keyword constraints.
    let (author_min_len, author_max_len, author_min_kw, author_max_kw) = match axis {
      Axis::Horizontal => (
        style.min_width,
        style.max_width,
        style.min_width_keyword,
        style.max_width_keyword,
      ),
      Axis::Vertical => (
        style.min_height,
        style.max_height,
        style.min_height_keyword,
        style.max_height_keyword,
      ),
    };
    let keyword_to_bound = |kw: IntrinsicSizeKeyword| -> Option<f32> {
      match kw {
        IntrinsicSizeKeyword::MinContent => Some(min_intrinsic),
        IntrinsicSizeKeyword::MaxContent => Some(max_intrinsic),
        IntrinsicSizeKeyword::FillAvailable => None,
        IntrinsicSizeKeyword::FitContent { .. } => None,
      }
    };
    let author_min = author_min_kw.and_then(keyword_to_bound).or_else(|| {
      author_min_len
        .and_then(resolve_length_px)
        .map(to_border_box)
    });
    let author_max = author_max_kw.and_then(keyword_to_bound).or_else(|| {
      author_max_len
        .and_then(resolve_length_px)
        .map(to_border_box)
    });
    if author_min.is_some() || author_max.is_some() {
      let min_bound = author_min.unwrap_or(0.0);
      let mut max_bound = author_max.unwrap_or(f32::INFINITY);
      if max_bound < min_bound {
        max_bound = min_bound;
      }
      border_box = crate::layout::utils::clamp_with_order(border_box, min_bound, max_bound);
    }

    Ok(border_box.is_finite().then(|| border_box.max(0.0)))
  }

  fn apply_grid_intrinsic_size_keywords(
    &self,
    box_node: &BoxNode,
    is_root: bool,
    taffy_style: &mut TaffyStyle,
  ) -> Result<(), LayoutError> {
    if is_root {
      return Ok(());
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
    let keyword_to_mode = |kw: IntrinsicSizeKeyword| match kw {
      IntrinsicSizeKeyword::MinContent => Some(IntrinsicSizingMode::MinContent),
      IntrinsicSizeKeyword::MaxContent => Some(IntrinsicSizingMode::MaxContent),
      IntrinsicSizeKeyword::FillAvailable => None,
      IntrinsicSizeKeyword::FitContent { .. } => None,
    };
    let width_has_fit_content_max_constraint = style.max_width.is_none()
      && matches!(
        style.max_width_keyword,
        Some(IntrinsicSizeKeyword::FitContent { .. })
      );
    let height_has_fit_content_max_constraint = style.max_height.is_none()
      && matches!(
        style.max_height_keyword,
        Some(IntrinsicSizeKeyword::FitContent { .. })
      );

    let has_intrinsic_keyword = style.width_keyword.and_then(keyword_to_mode).is_some()
      || style.height_keyword.and_then(keyword_to_mode).is_some()
      || style.min_width_keyword.and_then(keyword_to_mode).is_some()
      || style.max_width_keyword.and_then(keyword_to_mode).is_some()
      || style.min_height_keyword.and_then(keyword_to_mode).is_some()
      || style.max_height_keyword.and_then(keyword_to_mode).is_some();
    if !has_intrinsic_keyword {
      return Ok(());
    }

    let item_fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let item_fc = self.factory.get(item_fc_type);
    let box_id = box_node.id();

    // When computing intrinsic sizes for an axis that is itself specified as an intrinsic keyword,
    // clear that size property to avoid self-recursion.
    let width_override = style.width_keyword.is_some().then(|| {
      let mut override_style: ComputedStyle = (*style).clone();
      override_style.width = None;
      override_style.width_keyword = None;
      Arc::new(override_style)
    });
    let height_override = style.height_keyword.is_some().then(|| {
      let mut override_style: ComputedStyle = (*style).clone();
      override_style.height = None;
      override_style.height_keyword = None;
      Arc::new(override_style)
    });

    let run_with_override = |override_style: Option<Arc<ComputedStyle>>,
                             f: &dyn Fn(&BoxNode) -> Result<f32, LayoutError>|
     -> Result<f32, LayoutError> {
      if let Some(style_override) = override_style {
        if box_id != 0 {
          crate::layout::style_override::with_style_override(box_id, style_override, || f(box_node))
        } else {
          let mut cloned = box_node.clone();
          cloned.style = style_override;
          f(&cloned)
        }
      } else {
        f(box_node)
      }
    };

    let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      run_with_override(width_override.clone(), &|node| {
        crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
          item_fc.as_ref(),
          node,
          PhysicalAxis::X,
          mode,
        )
      })
    };

    let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      run_with_override(height_override.clone(), &|node| {
        crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
          item_fc.as_ref(),
          node,
          PhysicalAxis::Y,
          mode,
        )
      })
    };

    if let Some(mode) = style.width_keyword.and_then(keyword_to_mode) {
      // `max-width: fit-content(...)` must be resolved using the available grid area size. Since
      // the Taffy template cache can't represent this dependency, keep the preferred size as `auto`
      // and let the measure callback compute the final used size for this axis.
      if !width_has_fit_content_max_constraint {
        match intrinsic_physical_width(mode) {
          Ok(border_box) => {
            if border_box.is_finite() {
              taffy_style.size.width = Dimension::length(border_box.max(0.0));
            }
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }
    }

    if let Some(mode) = style.height_keyword.and_then(keyword_to_mode) {
      if !height_has_fit_content_max_constraint {
        match intrinsic_physical_height(mode) {
          Ok(border_box) => {
            if border_box.is_finite() {
              taffy_style.size.height = Dimension::length(border_box.max(0.0));
            }
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }
    }

    if let Some(mode) = style.min_width_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_width(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            taffy_style.min_size.width = Dimension::length(border_box.max(0.0));
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    if let Some(mode) = style.min_height_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_height(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            taffy_style.min_size.height = Dimension::length(border_box.max(0.0));
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    if let Some(mode) = style.max_width_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_width(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            taffy_style.max_size.width = Dimension::length(border_box.max(0.0));
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    if let Some(mode) = style.max_height_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_height(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            taffy_style.max_size.height = Dimension::length(border_box.max(0.0));
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    Ok(())
  }

  fn content_visibility_auto_has_definite_placeholder(&self, node: &BoxNode) -> bool {
    let style = node.style.as_ref();
    let axis_is_width = crate::style::block_axis_is_horizontal(style.writing_mode);
    let explicit = if axis_is_width {
      style.width.as_ref()
    } else {
      style.height.as_ref()
    };
    if explicit
      .and_then(|l| self.resolve_length_px(l, style))
      .is_some()
    {
      return true;
    }

    let axis = if axis_is_width {
      style.contain_intrinsic_width
    } else {
      style.contain_intrinsic_height
    };
    if axis
      .length
      .as_ref()
      .and_then(|l| self.resolve_length_px(l, style))
      .is_some()
    {
      return true;
    }

    if axis.auto {
      if let Some(size) = remembered_size_cache_lookup(node) {
        let value = if axis_is_width {
          size.width
        } else {
          size.height
        };
        if value.is_finite() {
          return true;
        }
      }
    }

    false
  }

  fn content_visibility_placeholder_content_size(
    &self,
    node: &BoxNode,
    constraints: &LayoutConstraints,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
  ) -> Size {
    let style = node.style.as_ref();
    let sanitize = |value: f32| {
      if value.is_finite() && value >= 0.0 {
        value
      } else {
        0.0
      }
    };

    let remembered = remembered_size_cache_lookup(node);
    let remembered_width = remembered.map(|size| size.width).filter(|v| v.is_finite());
    let remembered_height = remembered.map(|size| size.height).filter(|v| v.is_finite());

    let width = known_dimensions.width.unwrap_or_else(|| {
      let base = constraints
        .inline_percentage_base
        .or_else(|| constraints.width())
        .filter(|b| b.is_finite());
      crate::layout::utils::resolve_contain_intrinsic_size_axis(
        style.contain_intrinsic_width,
        remembered_width,
        base,
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      )
    });

    let height = known_dimensions.height.unwrap_or_else(|| {
      let base = constraints.height().filter(|b| b.is_finite());
      crate::layout::utils::resolve_contain_intrinsic_size_axis(
        style.contain_intrinsic_height,
        remembered_height,
        base,
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      )
    });

    Size::new(sanitize(width), sanitize(height))
  }

  /// Builds a Taffy tree from a BoxNode tree
  ///
  /// Recursively converts the BoxNode tree to Taffy nodes, returning
  /// the root node ID and a mapping from Taffy nodes to BoxNodes.
  fn build_taffy_tree(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    root_style: &ComputedStyle,
    constraints: &LayoutConstraints,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
  ) -> Result<TaffyNodeId, LayoutError> {
    let root_children: Vec<&BoxNode> = box_node.children.iter().collect();
    self.build_taffy_tree_children(
      taffy,
      box_node,
      root_style,
      &root_children,
      constraints,
      positioned_children,
    )
  }

  /// Builds a Taffy tree using an explicit slice of root children (used to exclude out-of-flow boxes).
  fn build_taffy_tree_children(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    root_style: &ComputedStyle,
    root_children: &[&BoxNode],
    _constraints: &LayoutConstraints,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
  ) -> Result<TaffyNodeId, LayoutError> {
    let mut deadline_counter = 0usize;
    let mut in_flow_children: Vec<&BoxNode> = Vec::new();
    let mut positioned: Vec<*const BoxNode> = Vec::new();
    let mut child_has_subgrid = false;
    for child in root_children {
      check_layout_deadline(&mut deadline_counter)?;
      match child.style.position {
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
          positioned.push(*child as *const BoxNode)
        }
        _ => {
          if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
            child_has_subgrid = true;
          }
          in_flow_children.push(*child)
        }
      }
    }

    let has_subgrid = root_style.grid_row_subgrid || root_style.grid_column_subgrid;

    if !has_subgrid && !child_has_subgrid {
      let root_axis_style = GridAxisStyle::from_style(root_style);
      let child_fingerprint = grid_child_fingerprint(&in_flow_children, &mut deadline_counter)?;
      let root_style_fingerprint = taffy_grid_container_style_fingerprint(root_style);
      let cache_key = TaffyNodeCacheKey::new(
        TaffyAdapterKind::Grid,
        root_style_fingerprint,
        child_fingerprint,
        self.viewport_size,
      );
      let cached = self.taffy_cache.get(&cache_key);
      let template: std::sync::Arc<CachedTaffyTemplate> = if let Some(template) = cached {
        record_taffy_node_cache_hit(TaffyAdapterKind::Grid, template.node_count());
        record_taffy_style_cache_hit(TaffyAdapterKind::Grid, template.node_count());
        template
      } else {
        let mut child_styles = Vec::with_capacity(in_flow_children.len());
        for child in in_flow_children.iter() {
          check_layout_deadline(&mut deadline_counter)?;
          let child_style_override = style_override_for(child.id);
          let child_style: &ComputedStyle = child_style_override
            .as_deref()
            .unwrap_or_else(|| child.style.as_ref());
          child_styles.push(std::sync::Arc::new(SendSyncStyle(self.convert_style(
            child_style,
            Some(root_style),
            Some(root_axis_style),
            false,
            false,
          ))));
        }
        let simple_grid =
          self.is_simple_grid(root_style, &in_flow_children, &mut deadline_counter)?;
        let root_style = std::sync::Arc::new(SendSyncStyle(self.convert_style(
          root_style,
          None,
          None,
          simple_grid,
          true,
        )));
        let template = std::sync::Arc::new(CachedTaffyTemplate {
          root_style,
          child_styles,
        });
        self.taffy_cache.insert(cache_key, template.clone());
        record_taffy_node_cache_miss(TaffyAdapterKind::Grid, template.node_count());
        record_taffy_style_cache_miss(TaffyAdapterKind::Grid, template.node_count());
        template
      };

      let mut taffy_children = Vec::with_capacity(in_flow_children.len());
      for (child_style, child) in template.child_styles.iter().zip(in_flow_children.iter()) {
        check_layout_deadline(&mut deadline_counter)?;
        let child = *child;
        let mut resolved_style = child_style.0.clone();
        self.apply_grid_intrinsic_size_keywords(child, false, &mut resolved_style)?;
        let node = taffy
          .new_leaf_with_context(resolved_style, child as *const BoxNode)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        taffy_children.push(node);
      }

      let node_id = if taffy_children.is_empty() {
        taffy
          .new_leaf_with_context(template.root_style.0.clone(), box_node as *const BoxNode)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?
      } else {
        let node_id = taffy
          .new_with_children(template.root_style.0.clone(), &taffy_children)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        taffy
          .set_node_context(node_id, Some(box_node as *const BoxNode))
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        node_id
      };

      if !positioned.is_empty() {
        positioned_children.insert(node_id, positioned);
      }

      return Ok(node_id);
    }

    self.build_taffy_tree_inner(
      taffy,
      box_node,
      root_style,
      true,
      None,
      None,
      Some(root_children),
      positioned_children,
      &mut deadline_counter,
    )
  }

  fn build_taffy_tree_inner(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    style: &ComputedStyle,
    is_root: bool,
    containing_grid: Option<&ComputedStyle>,
    containing_grid_axis: Option<GridAxisStyle>,
    children_override: Option<&[&BoxNode]>,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
    deadline_counter: &mut usize,
  ) -> Result<TaffyNodeId, LayoutError> {
    let mut children_iter: Vec<&BoxNode> = Vec::new();
    let mut positioned: Vec<*const BoxNode> = Vec::new();
    let mut has_subgrid_child = false;

    // Determine whether this grid container should be represented in the Taffy tree.
    let is_grid_container = matches!(
      box_node.formatting_context(),
      Some(FormattingContextType::Grid)
    );
    // Non-grid descendants are represented as leaf nodes in the Taffy tree. Avoid collecting or
    // scanning their children here; their full subtree will be laid out later when we re-run the
    // child formatting context with the definite sizes resolved by Taffy.
    if !is_grid_container && !is_root {
      let mut taffy_style =
        self.convert_style(style, containing_grid, containing_grid_axis, false, false);
      self.apply_grid_intrinsic_size_keywords(box_node, false, &mut taffy_style)?;
      return taffy
        .new_leaf_with_context(taffy_style, box_node as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)));
    }
    let mut include_children = is_root;

    // Partition children into in-flow vs positioned for grid containers we expand in the tree.
    if is_grid_container {
      if let Some(children_override) = children_override {
        for child in children_override {
          check_layout_deadline(deadline_counter)?;
          if is_collapsible_whitespace_grid_item(child) {
            continue;
          }
          match child.style.position {
            crate::style::position::Position::Absolute
            | crate::style::position::Position::Fixed => positioned.push(*child as *const BoxNode),
            _ => {
              if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
                has_subgrid_child = true;
              }
              children_iter.push(*child)
            }
          }
        }
      } else {
        for child in box_node.children.iter() {
          check_layout_deadline(deadline_counter)?;
          if is_collapsible_whitespace_grid_item(child) {
            continue;
          }
          match child.style.position {
            crate::style::position::Position::Absolute
            | crate::style::position::Position::Fixed => positioned.push(child as *const BoxNode),
            _ => {
              if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
                has_subgrid_child = true;
              }
              children_iter.push(child)
            }
          }
        }
      }
    } else if is_root {
      children_iter = match children_override {
        Some(children_override) => children_override.to_vec(),
        None => box_node.children.iter().collect(),
      };
    }

    // Expand subgrids (and any grid that hosts a subgrid child) into the Taffy tree so tracks can be shared.
    if is_grid_container {
      let is_subgrid = style.grid_row_subgrid || style.grid_column_subgrid;
      include_children |= is_subgrid || has_subgrid_child;
    }

    let simple_grid = include_children
      && is_root
      && self.is_simple_grid(style, &children_iter, deadline_counter)?;
    let mut taffy_style = self.convert_style(
      style,
      containing_grid,
      containing_grid_axis,
      simple_grid,
      include_children,
    );
    self.apply_grid_intrinsic_size_keywords(box_node, is_root, &mut taffy_style)?;

    let axis_style_for_children = if is_grid_container {
      GridAxisStyle::effective_for_grid_container(style, containing_grid_axis)
    } else {
      GridAxisStyle::from_style(style)
    };

    let node_id = if include_children {
      let mut taffy_children = Vec::with_capacity(children_iter.len());
      for child in children_iter {
        check_layout_deadline(deadline_counter)?;
        let child_style_override = style_override_for(child.id);
        let child_style: &ComputedStyle = child_style_override
          .as_deref()
          .unwrap_or_else(|| child.style.as_ref());
        taffy_children.push(self.build_taffy_tree_inner(
          taffy,
          child,
          child_style,
          false,
          Some(style),
          Some(axis_style_for_children),
          None,
          positioned_children,
          deadline_counter,
        )?);
      }
      let node_id = taffy
        .new_with_children(taffy_style, &taffy_children)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      if !positioned.is_empty() {
        positioned_children.insert(node_id, positioned);
      }
      taffy
        .set_node_context(node_id, Some(box_node as *const BoxNode))
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      node_id
    } else {
      taffy
        .new_leaf_with_context(taffy_style, box_node as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?
    };

    Ok(node_id)
  }

  /// Converts ComputedStyle to Taffy Style
  fn convert_style(
    &self,
    style: &ComputedStyle,
    containing_grid: Option<&ComputedStyle>,
    containing_grid_axis: Option<GridAxisStyle>,
    simple_grid: bool,
    is_grid_node: bool,
  ) -> TaffyStyle {
    let mut taffy_style = TaffyStyle::default();
    let is_grid = is_grid_node && !simple_grid;
    let container_axis_style = if is_grid {
      GridAxisStyle::effective_for_grid_container(style, containing_grid_axis)
    } else {
      GridAxisStyle::from_style(style)
    };
    let inline_positive_container = container_axis_style.inline_positive();
    let block_positive_container = container_axis_style.block_positive();
    let inline_is_horizontal_container = container_axis_style.inline_is_horizontal();

    let reserve_scroll_x = style.scrollbar_gutter.stable
      && matches!(style.overflow_x, CssOverflow::Auto | CssOverflow::Scroll);
    let reserve_scroll_y = style.scrollbar_gutter.stable
      && matches!(style.overflow_y, CssOverflow::Auto | CssOverflow::Scroll);
    let map_overflow = |value: CssOverflow, reserve: bool| match value {
      // Taffy lacks a distinct `Auto` variant. CSS `overflow: auto` is still a scroll container
      // (automatic min size = 0), but it should only reserve scrollbar space when
      // `scrollbar-gutter: stable` (or `overflow: scroll`) requests it.
      CssOverflow::Visible => TaffyOverflow::Visible,
      CssOverflow::Clip => TaffyOverflow::Clip,
      CssOverflow::Hidden => TaffyOverflow::Hidden,
      CssOverflow::Scroll => TaffyOverflow::Scroll,
      CssOverflow::Auto => {
        if reserve {
          TaffyOverflow::Scroll
        } else {
          TaffyOverflow::Hidden
        }
      }
    };

    // Grid item axes follow the containing grid's writing mode, not the item's own.
    let item_axis_style = containing_grid_axis.unwrap_or_else(|| GridAxisStyle::from_style(style));
    let inline_positive_item = item_axis_style.inline_positive();
    let block_positive_item = item_axis_style.block_positive();
    let inline_is_horizontal_item = item_axis_style.inline_is_horizontal();
    // `self-start`/`self-end` resolve against the alignment subject's (item's) writing-mode and
    // direction, but on the physical axis being aligned. Compute the polarity for each physical
    // axis so we can map these keywords correctly even when writing modes differ.
    let self_axes =
      FragmentAxes::from_writing_mode_and_direction(style.writing_mode, style.direction);
    let self_axis_positive = |axis: PhysicalAxis| {
      if axis == self_axes.inline_axis() {
        self_axes.inline_positive()
      } else {
        self_axes.block_positive()
      }
    };
    let self_physical_x_positive = self_axis_positive(PhysicalAxis::X);
    let self_physical_y_positive = self_axis_positive(PhysicalAxis::Y);
    let container_physical_x_positive = if inline_is_horizontal_item {
      inline_positive_item
    } else {
      block_positive_item
    };
    let container_physical_y_positive = if inline_is_horizontal_item {
      block_positive_item
    } else {
      inline_positive_item
    };
    let convert_item_alignment = |align: AlignItems, axis: PhysicalAxis| {
      let (container_positive, self_positive) = match axis {
        PhysicalAxis::X => (container_physical_x_positive, self_physical_x_positive),
        PhysicalAxis::Y => (container_physical_y_positive, self_physical_y_positive),
      };
      let axis_positive = match align {
        AlignItems::SelfStart | AlignItems::SelfEnd => self_positive,
        _ => container_positive,
      };
      self.convert_align_items(&align, axis_positive)
    };

    // Display mode
    if is_grid {
      taffy_style.display = Display::Grid;
      taffy_style.axes_swapped = !inline_is_horizontal_container;
    } else {
      taffy_style.display = Display::Block;
    }

    // Size
    taffy_style.size = taffy::geometry::Size {
      width: self.convert_sizing_property_to_dimension_box_sizing(
        &style.width,
        &style.width_keyword,
        style,
        Axis::Horizontal,
      ),
      height: self.convert_sizing_property_to_dimension_box_sizing(
        &style.height,
        &style.height_keyword,
        style,
        Axis::Vertical,
      ),
    };

    // Min/Max size
    taffy_style.min_size = taffy::geometry::Size {
      width: self.convert_sizing_property_to_dimension_box_sizing(
        &style.min_width,
        &style.min_width_keyword,
        style,
        Axis::Horizontal,
      ),
      height: self.convert_sizing_property_to_dimension_box_sizing(
        &style.min_height,
        &style.min_height_keyword,
        style,
        Axis::Vertical,
      ),
    };
    taffy_style.max_size = taffy::geometry::Size {
      width: self.convert_sizing_property_to_dimension_box_sizing(
        &style.max_width,
        &style.max_width_keyword,
        style,
        Axis::Horizontal,
      ),
      height: self.convert_sizing_property_to_dimension_box_sizing(
        &style.max_height,
        &style.max_height_keyword,
        style,
        Axis::Vertical,
      ),
    };

    // Margin
    let margin_left_auto = style.margin_left.is_none();
    let margin_right_auto = style.margin_right.is_none();
    let margin_top_auto = style.margin_top.is_none();
    let margin_bottom_auto = style.margin_bottom.is_none();
    taffy_style.margin = taffy::geometry::Rect {
      left: self.convert_opt_length_to_lpa(&style.margin_left, style),
      right: self.convert_opt_length_to_lpa(&style.margin_right, style),
      top: self.convert_opt_length_to_lpa(&style.margin_top, style),
      bottom: self.convert_opt_length_to_lpa(&style.margin_bottom, style),
    };

    // Padding
    taffy_style.padding = taffy::geometry::Rect {
      left: self.convert_length_to_lp(&style.padding_left, style),
      right: self.convert_length_to_lp(&style.padding_right, style),
      top: self.convert_length_to_lp(&style.padding_top, style),
      bottom: self.convert_length_to_lp(&style.padding_bottom, style),
    };

    // Border
    taffy_style.border = taffy::geometry::Rect {
      left: self.convert_length_to_lp(&style.used_border_left_width(), style),
      right: self.convert_length_to_lp(&style.used_border_right_width(), style),
      top: self.convert_length_to_lp(&style.used_border_top_width(), style),
      bottom: self.convert_length_to_lp(&style.used_border_bottom_width(), style),
    };
    taffy_style.aspect_ratio = self.convert_aspect_ratio(style.aspect_ratio);

    taffy_style.overflow = taffy::geometry::Point {
      x: map_overflow(style.overflow_x, reserve_scroll_x),
      y: map_overflow(style.overflow_y, reserve_scroll_y),
    };
    taffy_style.scrollbar_width = resolve_scrollbar_width(style);

    // Grid container properties
    if is_grid {
      let has_parent_grid = containing_grid_axis.is_some();
      let css_row_subgrid = style.grid_row_subgrid && has_parent_grid;
      let css_column_subgrid = style.grid_column_subgrid && has_parent_grid;

      // Taffy always interprets columns as the physical X axis and rows as the physical Y axis.
      // CSS Grid axes, however, are defined in terms of the element's writing-mode (inline/block).
      // When the inline axis is vertical we transpose all axis-dependent grid properties so that
      // Taffy sees column data for the physical X axis and row data for the physical Y axis.
      let swap_grid_axes = !inline_is_horizontal_container;

      // Grid template columns/rows.
      if swap_grid_axes {
        taffy_style.grid_template_columns =
          self.convert_grid_template(&style.grid_template_rows, style);
        taffy_style.grid_template_rows =
          self.convert_grid_template(&style.grid_template_columns, style);
      } else {
        taffy_style.grid_template_columns =
          self.convert_grid_template(&style.grid_template_columns, style);
        taffy_style.grid_template_rows = self.convert_grid_template(&style.grid_template_rows, style);
      }

      // Subgrid flags + extra line names.
      if swap_grid_axes {
        taffy_style.subgrid_columns = css_row_subgrid;
        taffy_style.subgrid_rows = css_column_subgrid;
        if css_row_subgrid && !style.subgrid_row_line_names.is_empty() {
          taffy_style.subgrid_column_names = style.subgrid_row_line_names.clone();
        }
        if css_column_subgrid && !style.subgrid_column_line_names.is_empty() {
          taffy_style.subgrid_row_names = style.subgrid_column_line_names.clone();
        }
      } else {
        taffy_style.subgrid_columns = css_column_subgrid;
        taffy_style.subgrid_rows = css_row_subgrid;
        if css_column_subgrid && !style.subgrid_column_line_names.is_empty() {
          taffy_style.subgrid_column_names = style.subgrid_column_line_names.clone();
        }
        if css_row_subgrid && !style.subgrid_row_line_names.is_empty() {
          taffy_style.subgrid_row_names = style.subgrid_row_line_names.clone();
        }
      }

      // Line names.
      if swap_grid_axes {
        if !style.grid_row_line_names.is_empty() {
          taffy_style.grid_template_column_names = style.grid_row_line_names.clone();
        }
        if !style.grid_column_line_names.is_empty() {
          taffy_style.grid_template_row_names = style.grid_column_line_names.clone();
        }
      } else {
        if !style.grid_column_line_names.is_empty() {
          taffy_style.grid_template_column_names = style.grid_column_line_names.clone();
        }
        if !style.grid_row_line_names.is_empty() {
          taffy_style.grid_template_row_names = style.grid_row_line_names.clone();
        }
      }
      // Implicit track sizing
      if inline_is_horizontal_container {
        if !style.grid_auto_rows.is_empty() {
          taffy_style.grid_auto_rows = style
            .grid_auto_rows
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
        if !style.grid_auto_columns.is_empty() {
          taffy_style.grid_auto_columns = style
            .grid_auto_columns
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
      } else {
        if !style.grid_auto_columns.is_empty() {
          taffy_style.grid_auto_rows = style
            .grid_auto_columns
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
        if !style.grid_auto_rows.is_empty() {
          taffy_style.grid_auto_columns = style
            .grid_auto_rows
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
      }
      taffy_style.grid_auto_flow = match (style.grid_auto_flow, inline_is_horizontal_container) {
        (GridAutoFlow::Row, true) => taffy::style::GridAutoFlow::Row,
        (GridAutoFlow::RowDense, true) => taffy::style::GridAutoFlow::RowDense,
        (GridAutoFlow::Column, true) => taffy::style::GridAutoFlow::Column,
        (GridAutoFlow::ColumnDense, true) => taffy::style::GridAutoFlow::ColumnDense,
        (GridAutoFlow::Row, false) => taffy::style::GridAutoFlow::Column,
        (GridAutoFlow::RowDense, false) => taffy::style::GridAutoFlow::ColumnDense,
        (GridAutoFlow::Column, false) => taffy::style::GridAutoFlow::Row,
        (GridAutoFlow::ColumnDense, false) => taffy::style::GridAutoFlow::RowDense,
      };
      if !style.grid_template_areas.is_empty() {
        if let Some(mut bounds) = validate_area_rectangles(&style.grid_template_areas) {
          let mut entries: Vec<_> = bounds.drain().collect();
          entries.sort_by(|a, b| a.0.cmp(&b.0));
          let mut areas = Vec::with_capacity(entries.len());
          for (name, (top, bottom, left, right)) in entries {
            let (row_start, row_end, column_start, column_end) = if swap_grid_axes {
              ((left as u16) + 1, (right as u16) + 2, (top as u16) + 1, (bottom as u16) + 2)
            } else {
              ((top as u16) + 1, (bottom as u16) + 2, (left as u16) + 1, (right as u16) + 2)
            };
            areas.push(GridTemplateArea {
              name,
              row_start,
              row_end,
              column_start,
              column_end,
            });
          }
          taffy_style.grid_template_areas = areas;
        }
      }

      // Gap
      taffy_style.gap = if swap_grid_axes {
        taffy::geometry::Size {
          width: self.convert_length_to_lp(&style.grid_row_gap, style),
          height: self.convert_length_to_lp(&style.grid_column_gap, style),
        }
      } else {
        taffy::geometry::Size {
          width: self.convert_length_to_lp(&style.grid_column_gap, style),
          height: self.convert_length_to_lp(&style.grid_row_gap, style),
        }
      };
      taffy_style.subgrid_gap = taffy::geometry::Size {
        width: if inline_is_horizontal_container {
          self.convert_length_to_lp(&style.grid_column_gap, style)
        } else {
          self.convert_length_to_lp(&style.grid_row_gap, style)
        },
        height: if inline_is_horizontal_container {
          self.convert_length_to_lp(&style.grid_row_gap, style)
        } else {
          self.convert_length_to_lp(&style.grid_column_gap, style)
        },
      };
      taffy_style.subgrid_gap_is_normal = taffy::geometry::Size {
        width: if inline_is_horizontal_container {
          style.grid_column_gap_is_normal
        } else {
          style.grid_row_gap_is_normal
        },
        height: if inline_is_horizontal_container {
          style.grid_row_gap_is_normal
        } else {
          style.grid_column_gap_is_normal
        },
      };

      // Alignment
      if swap_grid_axes {
        taffy_style.align_content =
          Some(self.convert_justify_content(&style.justify_content, inline_positive_container));
        taffy_style.justify_content =
          Some(self.convert_align_content(&style.align_content, block_positive_container));
      } else {
        taffy_style.align_content =
          Some(self.convert_align_content(&style.align_content, block_positive_container));
        taffy_style.justify_content =
          Some(self.convert_justify_content(&style.justify_content, inline_positive_container));
      }
    }
    if inline_is_horizontal_container {
      taffy_style.align_items =
        Some(self.convert_align_items(&style.align_items, block_positive_container));
      taffy_style.justify_items =
        Some(self.convert_align_items(&style.justify_items, inline_positive_container));
    } else {
      taffy_style.align_items =
        Some(self.convert_align_items(&style.justify_items, inline_positive_container));
      taffy_style.justify_items =
        Some(self.convert_align_items(&style.align_items, block_positive_container));
    }

    if inline_is_horizontal_item {
      taffy_style.align_self = style
        .align_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::Y));
      taffy_style.justify_self = style
        .justify_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::X));
    } else {
      taffy_style.align_self = style
        .justify_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::Y));
      taffy_style.justify_self = style
        .align_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::X));
    }

    if let Some(containing_grid) = containing_grid {
      if inline_is_horizontal_item {
        if taffy_style.justify_self.is_none() {
          taffy_style.justify_self =
            Some(convert_item_alignment(containing_grid.justify_items, PhysicalAxis::X));
        }
        if taffy_style.align_self.is_none() {
          taffy_style.align_self =
            Some(convert_item_alignment(containing_grid.align_items, PhysicalAxis::Y));
        }
      } else {
        if taffy_style.align_self.is_none() {
          taffy_style.align_self =
            Some(convert_item_alignment(containing_grid.justify_items, PhysicalAxis::Y));
        }
        if taffy_style.justify_self.is_none() {
          taffy_style.justify_self =
            Some(convert_item_alignment(containing_grid.align_items, PhysicalAxis::X));
        }
      }
    }

    if containing_grid.is_some() {
      // Auto margins override alignment per-axis; map them to self-alignment to keep grid items centered or pushed.
      let inline_start_auto = if inline_is_horizontal_item {
        if inline_positive_item {
          margin_left_auto
        } else {
          margin_right_auto
        }
      } else {
        if inline_positive_item {
          margin_top_auto
        } else {
          margin_bottom_auto
        }
      };
      let inline_end_auto = if inline_is_horizontal_item {
        if inline_positive_item {
          margin_right_auto
        } else {
          margin_left_auto
        }
      } else {
        if inline_positive_item {
          margin_bottom_auto
        } else {
          margin_top_auto
        }
      };

      let block_start_auto = if inline_is_horizontal_item {
        if block_positive_item {
          margin_top_auto
        } else {
          margin_bottom_auto
        }
      } else {
        if block_positive_item {
          margin_left_auto
        } else {
          margin_right_auto
        }
      };
      let block_end_auto = if inline_is_horizontal_item {
        if block_positive_item {
          margin_bottom_auto
        } else {
          margin_top_auto
        }
      } else {
        if block_positive_item {
          margin_right_auto
        } else {
          margin_left_auto
        }
      };

      let justify_override = match (inline_start_auto, inline_end_auto) {
        (true, true) => Some(AlignItems::Center),
        (true, false) => Some(if inline_positive_item {
          AlignItems::FlexEnd
        } else {
          AlignItems::FlexStart
        }),
        (false, true) => Some(if inline_positive_item {
          AlignItems::FlexStart
        } else {
          AlignItems::FlexEnd
        }),
        _ => None,
      };
      if let Some(justify) = justify_override {
        let converted = Some(self.convert_align_items(&justify, inline_positive_item));
        if inline_is_horizontal_item {
          taffy_style.justify_self = converted;
        } else {
          taffy_style.align_self = converted;
        }
      }

      let align_override = match (block_start_auto, block_end_auto) {
        (true, true) => Some(AlignItems::Center),
        (true, false) => Some(if block_positive_item {
          AlignItems::FlexEnd
        } else {
          AlignItems::FlexStart
        }),
        (false, true) => Some(if block_positive_item {
          AlignItems::FlexStart
        } else {
          AlignItems::FlexEnd
        }),
        _ => None,
      };
      if let Some(align) = align_override {
        let converted = Some(self.convert_align_items(&align, block_positive_item));
        if inline_is_horizontal_item {
          taffy_style.align_self = converted;
        } else {
          taffy_style.justify_self = converted;
        }
      }

      // `fit-content` keywords in max-size properties can't be represented directly in cached
      // Taffy styles (they depend on available space). Let the measure callback compute the final
      // size by preventing Taffy from treating the preferred size as definite or stretching the
      // item to the full grid area.
      let width_has_fit_content_max_constraint = style.max_width.is_none()
        && matches!(
          style.max_width_keyword,
          Some(IntrinsicSizeKeyword::FitContent { .. })
        );
      let height_has_fit_content_max_constraint = style.max_height.is_none()
        && matches!(
          style.max_height_keyword,
          Some(IntrinsicSizeKeyword::FitContent { .. })
        );
      if width_has_fit_content_max_constraint {
        taffy_style.size.width = Dimension::auto();
      }
      if height_has_fit_content_max_constraint {
        taffy_style.size.height = Dimension::auto();
      }

      // Intrinsic size keywords act like non-auto preferred sizes, but we represent them as `auto`
      // in the cached Taffy template so they can be resolved per box instance (or per measure call
      // for fit-content). `stretch` alignment only stretches auto-sized items, so when an intrinsic
      // keyword is specified on an axis, force `stretch` to fall back to `start` to avoid
      // incorrectly stretching the item to fill the grid area.
      let physical_width_positive = if inline_is_horizontal_item {
        inline_positive_item
      } else {
        block_positive_item
      };
      let physical_height_positive = if inline_is_horizontal_item {
        block_positive_item
      } else {
        inline_positive_item
      };
      if (style.width_keyword.is_some() || width_has_fit_content_max_constraint)
        && matches!(
          taffy_style.justify_self,
          Some(taffy::style::AlignItems::Stretch)
        )
      {
        taffy_style.justify_self =
          Some(self.convert_align_items(&AlignItems::Start, physical_width_positive));
      }
      if (style.height_keyword.is_some() || height_has_fit_content_max_constraint)
        && matches!(
          taffy_style.align_self,
          Some(taffy::style::AlignItems::Stretch)
        )
      {
        taffy_style.align_self =
          Some(self.convert_align_items(&AlignItems::Start, physical_height_positive));
      }
    }

    // Grid item properties using raw line numbers.
    let css_grid_column = self.convert_grid_placement(
      style.grid_column_raw.as_deref(),
      style.grid_column_start,
      style.grid_column_end,
    );
    let css_grid_row = self.convert_grid_placement(
      style.grid_row_raw.as_deref(),
      style.grid_row_start,
      style.grid_row_end,
    );
    if inline_is_horizontal_item {
      taffy_style.grid_column = css_grid_column;
      taffy_style.grid_row = css_grid_row;
    } else {
      taffy_style.grid_column = css_grid_row;
      taffy_style.grid_row = css_grid_column;
    }

    taffy_style
  }

  fn convert_sizing_property_to_dimension_box_sizing(
    &self,
    length: &Option<Length>,
    keyword: &Option<IntrinsicSizeKeyword>,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Dimension {
    if length.is_some() {
      return self.convert_opt_length_to_dimension_box_sizing(length, style, axis);
    }
    if keyword.is_some() {
      // Intrinsic sizing keywords depend on the box's contents and/or available space. Represent
      // them as `auto` in the cached template and resolve per box instance (or inside the measure
      // callback for fit-content).
      return Dimension::auto();
    }
    Dimension::auto()
  }

  /// Converts Option<Length> to Taffy Dimension
  fn convert_opt_length_to_dimension_box_sizing(
    &self,
    length: &Option<Length>,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Dimension {
    match length {
      None => Dimension::auto(),
      Some(len) => self.dimension_for_box_sizing(len, style, axis),
    }
  }

  /// Converts Length to Taffy Dimension
  fn convert_length_to_dimension(&self, length: &Length, style: &ComputedStyle) -> Dimension {
    use crate::style::values::LengthUnit;
    match length.unit {
      LengthUnit::Percent => Dimension::percent(length.value / 100.0),
      _ => {
        if let Some(px) = self.resolve_length_px(length, style) {
          Dimension::length(px)
        } else {
          Dimension::length(length.to_px())
        }
      }
    }
  }

  /// Converts Option<Length> to Taffy LengthPercentageAuto
  fn convert_opt_length_to_lpa(
    &self,
    length: &Option<Length>,
    style: &ComputedStyle,
  ) -> LengthPercentageAuto {
    use crate::style::values::LengthUnit;
    match length {
      None => LengthPercentageAuto::auto(),
      Some(len) => match len.unit {
        LengthUnit::Percent => LengthPercentageAuto::percent(len.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(len, style) {
            LengthPercentageAuto::length(px)
          } else {
            LengthPercentageAuto::length(len.to_px())
          }
        }
      },
    }
  }

  /// Converts Length to Taffy LengthPercentage
  fn convert_length_to_lp(&self, length: &Length, style: &ComputedStyle) -> LengthPercentage {
    use crate::style::values::LengthUnit;
    match length.unit {
      LengthUnit::Percent => LengthPercentage::percent(length.value / 100.0),
      _ => {
        if let Some(px) = self.resolve_length_px(length, style) {
          LengthPercentage::length(px)
        } else {
          LengthPercentage::length(length.to_px())
        }
      }
    }
  }

  fn dimension_for_box_sizing(&self, len: &Length, style: &ComputedStyle, axis: Axis) -> Dimension {
    if style.box_sizing == BoxSizing::ContentBox {
      if let Some(edges) = self.edges_px(style, axis) {
        if let Some(px) = self.resolve_length_px(len, style) {
          return Dimension::length((px + edges).max(0.0));
        }
      }
    }
    self.convert_length_to_dimension(len, style)
  }

  fn edges_px(&self, style: &ComputedStyle, axis: Axis) -> Option<f32> {
    match axis {
      Axis::Horizontal => {
        let p1 = self.resolve_length_px(&style.padding_left, style)?;
        let p2 = self.resolve_length_px(&style.padding_right, style)?;
        let b1 = self.resolve_length_px(&style.used_border_left_width(), style)?;
        let b2 = self.resolve_length_px(&style.used_border_right_width(), style)?;
        Some(p1 + p2 + b1 + b2)
      }
      Axis::Vertical => {
        let p1 = self.resolve_length_px(&style.padding_top, style)?;
        let p2 = self.resolve_length_px(&style.padding_bottom, style)?;
        let b1 = self.resolve_length_px(&style.used_border_top_width(), style)?;
        let b2 = self.resolve_length_px(&style.used_border_bottom_width(), style)?;
        Some(p1 + p2 + b1 + b2)
      }
    }
  }

  fn resolve_length_px(&self, len: &Length, style: &ComputedStyle) -> Option<f32> {
    use crate::style::values::LengthUnit::Ch;
    use crate::style::values::LengthUnit::Cm;
    use crate::style::values::LengthUnit::Em;
    use crate::style::values::LengthUnit::Ex;
    use crate::style::values::LengthUnit::In;
    use crate::style::values::LengthUnit::Mm;
    use crate::style::values::LengthUnit::Pc;
    use crate::style::values::LengthUnit::Percent;
    use crate::style::values::LengthUnit::Pt;
    use crate::style::values::LengthUnit::Px;
    use crate::style::values::LengthUnit::Rem;
    match len.unit {
      Percent => None,
      Px | Pt | In | Cm | Mm | Pc => Some(len.to_px()),
      Rem => Some(len.value * style.root_font_size),
      Em => Some(len.value * style.font_size),
      Ex => Some(len.value * style.font_size * 0.5),
      Ch => Some(len.value * style.font_size * 0.5),
      unit if unit.is_viewport_relative() => {
        len.resolve_with_viewport(self.viewport_size.width, self.viewport_size.height)
      }
      _ => None,
    }
  }

  /// Converts GridTrack Vec to Taffy track list
  fn convert_grid_template(
    &self,
    tracks: &[GridTrack],
    style: &ComputedStyle,
  ) -> Vec<GridTemplateComponent<String>> {
    let mut components = Vec::new();
    for track in tracks {
      match track {
        GridTrack::RepeatAutoFill {
          tracks: inner,
          line_names,
        } => {
          let converted: Vec<TrackSizingFunction> = inner
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
          let repetition = GridTemplateRepetition {
            count: RepetitionCount::AutoFill,
            tracks: converted,
            line_names: line_names.clone(),
          };
          components.push(GridTemplateComponent::Repeat(repetition));
        }
        GridTrack::RepeatAutoFit {
          tracks: inner,
          line_names,
        } => {
          let converted: Vec<TrackSizingFunction> = inner
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
          let repetition = GridTemplateRepetition {
            count: RepetitionCount::AutoFit,
            tracks: converted,
            line_names: line_names.clone(),
          };
          components.push(GridTemplateComponent::Repeat(repetition));
        }
        _ => components.push(GridTemplateComponent::Single(
          self.convert_track_size(track, style),
        )),
      }
    }
    components
  }

  /// Converts a single GridTrack to TrackSizingFunction
  fn convert_track_size(&self, track: &GridTrack, style: &ComputedStyle) -> TrackSizingFunction {
    match track {
      GridTrack::Length(len) => {
        let lp = self.convert_length_to_lp(len, style);
        TrackSizingFunction::from(lp)
      }
      GridTrack::MinContent => TrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => TrackSizingFunction::MAX_CONTENT,
      GridTrack::FitContent(len) => {
        TrackSizingFunction::fit_content(self.convert_length_to_lp(len, style))
      }
      GridTrack::Fr(fr) => TrackSizingFunction {
        min: MinTrackSizingFunction::AUTO,
        max: MaxTrackSizingFunction::fr(*fr),
      },
      GridTrack::Auto => TrackSizingFunction::AUTO,
      GridTrack::MinMax(min, max) => {
        let min_fn = self.convert_min_track(min, style);
        let max_fn = self.convert_max_track(max, style);
        TrackSizingFunction {
          min: min_fn,
          max: max_fn,
        }
      }
      GridTrack::RepeatAutoFill { .. } | GridTrack::RepeatAutoFit { .. } => {
        TrackSizingFunction::AUTO
      }
    }
  }

  /// Converts GridTrack to MinTrackSizingFunction
  fn convert_min_track(&self, track: &GridTrack, style: &ComputedStyle) -> MinTrackSizingFunction {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => match len.unit {
        LengthUnit::Percent => MinTrackSizingFunction::percent(len.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(len, style) {
            MinTrackSizingFunction::length(px)
          } else {
            MinTrackSizingFunction::length(len.to_px())
          }
        }
      },
      GridTrack::MinContent => MinTrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => MinTrackSizingFunction::MAX_CONTENT,
      GridTrack::Auto => MinTrackSizingFunction::auto(),
      _ => MinTrackSizingFunction::auto(),
    }
  }

  /// Converts GridTrack to MaxTrackSizingFunction
  fn convert_max_track(&self, track: &GridTrack, style: &ComputedStyle) -> MaxTrackSizingFunction {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => match len.unit {
        LengthUnit::Percent => MaxTrackSizingFunction::percent(len.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(len, style) {
            MaxTrackSizingFunction::length(px)
          } else {
            MaxTrackSizingFunction::length(len.to_px())
          }
        }
      },
      GridTrack::Fr(fr) => MaxTrackSizingFunction::fr(*fr),
      GridTrack::MinContent => MaxTrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => MaxTrackSizingFunction::MAX_CONTENT,
      GridTrack::FitContent(len) => {
        MaxTrackSizingFunction::fit_content(self.convert_length_to_lp(len, style))
      }
      GridTrack::Auto
      | GridTrack::MinMax(..)
      | GridTrack::RepeatAutoFill { .. }
      | GridTrack::RepeatAutoFit { .. } => MaxTrackSizingFunction::auto(),
    }
  }

  /// Converts grid placements (with optional named lines) to Taffy Line<GridPlacement>
  fn convert_grid_placement(
    &self,
    raw: Option<&str>,
    start: i32,
    end: i32,
  ) -> Line<TaffyGridPlacement<String>> {
    if let Some(raw_str) = raw {
      return parse_grid_line_placement_raw(raw_str);
    }

    Line {
      start: if start == 0 {
        TaffyGridPlacement::Auto
      } else {
        TaffyGridPlacement::Line((start as i16).into())
      },
      end: if end == 0 {
        TaffyGridPlacement::Auto
      } else {
        TaffyGridPlacement::Line((end as i16).into())
      },
    }
  }

  /// Converts AlignContent to Taffy AlignContent
  fn convert_align_content(&self, align: &AlignContent, axis_positive: bool) -> TaffyAlignContent {
    match align {
      AlignContent::Start | AlignContent::FlexStart => {
        if axis_positive {
          TaffyAlignContent::Start
        } else {
          TaffyAlignContent::End
        }
      }
      AlignContent::End | AlignContent::FlexEnd => {
        if axis_positive {
          TaffyAlignContent::End
        } else {
          TaffyAlignContent::Start
        }
      }
      AlignContent::Center => TaffyAlignContent::Center,
      AlignContent::Stretch => TaffyAlignContent::Stretch,
      AlignContent::SpaceBetween => TaffyAlignContent::SpaceBetween,
      AlignContent::SpaceEvenly => TaffyAlignContent::SpaceEvenly,
      AlignContent::SpaceAround => TaffyAlignContent::SpaceAround,
    }
  }

  fn convert_justify_content(
    &self,
    justify: &JustifyContent,
    axis_positive: bool,
  ) -> TaffyAlignContent {
    match justify {
      JustifyContent::Start | JustifyContent::FlexStart => {
        if axis_positive {
          TaffyAlignContent::Start
        } else {
          TaffyAlignContent::End
        }
      }
      JustifyContent::End | JustifyContent::FlexEnd => {
        if axis_positive {
          TaffyAlignContent::End
        } else {
          TaffyAlignContent::Start
        }
      }
      JustifyContent::Center => TaffyAlignContent::Center,
      JustifyContent::SpaceBetween => TaffyAlignContent::SpaceBetween,
      JustifyContent::SpaceAround => TaffyAlignContent::SpaceAround,
      JustifyContent::SpaceEvenly => TaffyAlignContent::SpaceEvenly,
    }
  }

  fn convert_align_items(
    &self,
    align: &AlignItems,
    axis_positive: bool,
  ) -> taffy::style::AlignItems {
    match align {
      AlignItems::Start | AlignItems::SelfStart => {
        if axis_positive {
          taffy::style::AlignItems::Start
        } else {
          taffy::style::AlignItems::End
        }
      }
      AlignItems::End | AlignItems::SelfEnd => {
        if axis_positive {
          taffy::style::AlignItems::End
        } else {
          taffy::style::AlignItems::Start
        }
      }
      AlignItems::FlexStart => taffy::style::AlignItems::FlexStart,
      AlignItems::FlexEnd => taffy::style::AlignItems::FlexEnd,
      AlignItems::Center => taffy::style::AlignItems::Center,
      AlignItems::Baseline => taffy::style::AlignItems::Baseline,
      AlignItems::Stretch => taffy::style::AlignItems::Stretch,
    }
  }

  fn convert_aspect_ratio(&self, aspect_ratio: AspectRatio) -> Option<f32> {
    match aspect_ratio {
      AspectRatio::Auto => None,
      AspectRatio::Ratio(ratio) => Some(ratio),
      AspectRatio::AutoRatio(ratio) => Some(ratio),
    }
  }

  fn take_matching_measured_fragment(
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    keys: &[MeasureKey],
    width: f32,
    height: f32,
  ) -> Option<FragmentNode> {
    let matched_key = keys.iter().copied().find(|key| {
      measured_fragments.get(key).map_or(false, |fragment| {
        (fragment.bounds.width() - width).abs() < 0.1
          && (fragment.bounds.height() - height).abs() < 0.1
      })
    })?;
    measured_fragments.remove(&matched_key)
  }

  #[allow(clippy::too_many_arguments)]
  fn try_parallel_root_children_conversion(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    root_id: TaffyNodeId,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
    in_flow_children: &[&BoxNode],
    auto_unskipped: Option<&FxHashSet<*const BoxNode>>,
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    measured_node_keys: &FxHashMap<TaffyNodeId, Vec<MeasureKey>>,
  ) -> Option<Result<FragmentNode, LayoutError>> {
    let child_ids = match taffy.children(root_id) {
      Ok(children) => children,
      Err(e) => {
        return Some(Err(LayoutError::MissingContext(format!(
          "Taffy children error: {:?}",
          e
        ))));
      }
    };
    if child_ids.is_empty()
      || child_ids.len() != in_flow_children.len()
      || !self.parallelism.should_parallelize(child_ids.len())
    {
      return None;
    }

    let mut deadline_counter = 0usize;
    for child_id in child_ids.iter().copied() {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let Ok(grandchildren) = taffy.children(child_id) else {
        return None;
      };
      if !grandchildren.is_empty() {
        return None;
      }
    }

    let mut child_bounds: Vec<Rect> = Vec::with_capacity(child_ids.len());
    let mut reused_fragments: Vec<Option<FragmentNode>> = vec![None; child_ids.len()];
    let mut child_skipped: Vec<bool> = vec![false; child_ids.len()];
    let root_axis_style = GridAxisStyle::from_style(box_node.style.as_ref());
    let mut child_continuation_available: Vec<Option<f32>> = vec![None; child_ids.len()];
    for (idx, child_id) in child_ids.iter().copied().enumerate() {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let layout = match taffy.layout(child_id) {
        Ok(layout) => layout,
        Err(e) => {
          return Some(Err(LayoutError::MissingContext(format!(
            "Taffy layout error: {:?}",
            e
          ))));
        }
      };
      let bounds = Rect::from_xywh(
        layout.location.x,
        layout.location.y,
        layout.size.width,
        layout.size.height,
      );
      child_bounds.push(bounds);
      child_continuation_available[idx] = self.grid_item_continuation_available_block_size(
        bounds,
        constraints,
        Some(root_axis_style),
      );
      let child = in_flow_children[idx];
      let skip_contents = match child.style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          self.content_visibility_auto_has_definite_placeholder(child)
            && auto_unskipped
              .map(|set| !set.contains(&(child as *const BoxNode)))
              .unwrap_or(false)
        }
        crate::style::types::ContentVisibility::Visible => false,
      };
      child_skipped[idx] = skip_contents;
      if skip_contents {
        reused_fragments[idx] = Some(FragmentNode::new_with_style(
          bounds,
          FragmentContent::Block {
            box_id: Some(child.id),
          },
          vec![],
          child.style.clone(),
        ));
        continue;
      }
      if child_continuation_available[idx].is_some() {
        // Continuation fragments need to re-run layout under a reduced fragmentainer size; skip any
        // cached subtree captured during Taffy measurement.
        continue;
      }
      if let Some(keys) = measured_node_keys.get(&child_id) {
        if let Some(mut reused) = Self::take_matching_measured_fragment(
          measured_fragments,
          keys,
          bounds.width(),
          bounds.height(),
        ) {
          fragment_clone_profile::record_fragment_reuse_without_clone(CloneSite::GridMeasureReuse);
          let delta = Point::new(
            bounds.x() - reused.bounds.x(),
            bounds.y() - reused.bounds.y(),
          );
          if let Err(err) = translate_fragment_tree(&mut reused, delta, &mut deadline_counter) {
            return Some(Err(err));
          }
          reused_fragments[idx] = Some(reused);
        }
      }
    }

    let mut indices_to_layout: Vec<usize> = Vec::new();
    for (idx, reused) in reused_fragments.iter().enumerate() {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      if reused.is_none() {
        indices_to_layout.push(idx);
      }
    }

    let factory = std::sync::Arc::new(self.factory.clone());

    let deadline = active_deadline();
    let stage = active_stage();
    let child_results = indices_to_layout
      .par_iter()
      .map(|&idx| {
        with_deadline(deadline.as_ref(), || {
          let _stage_guard = StageGuard::install(stage);
          crate::layout::engine::debug_record_parallel_work();

          let child = in_flow_children[idx];
          let bounds = child_bounds[idx];
          let continuation_available = child_continuation_available[idx];
          let fc_type = child
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let parent_scroll = factory.viewport_scroll();
          let child_scroll = Point::new(parent_scroll.x - bounds.x(), parent_scroll.y - bounds.y());
          let child_factory = (*factory).clone().with_viewport_scroll(child_scroll);
          let fc = child_factory.get(fc_type);

          let available_height = continuation_available.unwrap_or(bounds.height());
          let child_constraints = LayoutConstraints::new(
            CrateAvailableSpace::Definite(bounds.width()),
            CrateAvailableSpace::Definite(available_height),
          )
          .with_inline_percentage_base(
            constraints
              .inline_percentage_base
              .or_else(|| Some(bounds.width())),
          );

          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );

          let force_height = continuation_available
            .map(|available| available + 0.01 >= bounds.height())
            .unwrap_or(true);

          let mut laid_out = if supports_used_border_box {
            let child_constraints = child_constraints.with_used_border_box_size(
              Some(bounds.width()),
              force_height.then_some(bounds.height()),
            );
            if continuation_available.is_some() {
              crate::layout::style_override::with_style_override(
                child.id,
                child.style.clone(),
                || fc.layout(child, &child_constraints),
              )?
            } else {
              fc.layout(child, &child_constraints)?
            }
          } else if matches!(
            fc_type,
            FormattingContextType::Flex | FormattingContextType::Grid
          ) {
            let mut layout_style = (*child.style).clone();
            layout_style.width = Some(Length::px(bounds.width()));
            if force_height {
              layout_style.height = Some(Length::px(bounds.height()));
            }
            layout_style.width_keyword = None;
            if force_height {
              layout_style.height_keyword = None;
            }
            crate::layout::style_override::with_style_override(
              child.id,
              Arc::new(layout_style),
              || fc.layout(child, &child_constraints),
            )?
          } else {
            let mut layout_style = (*child.style).clone();
            layout_style.width = Some(Length::px(bounds.width()));
            if force_height {
              layout_style.height = Some(Length::px(bounds.height()));
            }
            layout_style.width_keyword = None;
            if force_height {
              layout_style.height_keyword = None;
            }
            let layout_style = Arc::new(layout_style);
            if child.id != 0 {
              crate::layout::style_override::with_style_override(child.id, layout_style, || {
                fc.layout(child, &child_constraints)
              })?
            } else {
              let mut layout_child = (*child).clone();
              layout_child.style = layout_style;
              fc.layout(&layout_child, &child_constraints)?
            }
          };

          let mut translate_deadline_counter = 0usize;
          translate_fragment_tree(
            &mut laid_out,
            Point::new(bounds.x(), bounds.y()),
            &mut translate_deadline_counter,
          )?;
          laid_out.content = FragmentContent::Block {
            box_id: Some(child.id),
          };
          laid_out.style = Some(child.style.clone());
          Ok((idx, laid_out))
        })
      })
      .collect::<Result<Vec<_>, LayoutError>>();

    let mut child_results = match child_results {
      Ok(results) => results,
      Err(err) => return Some(Err(err)),
    };
    // `indices_to_layout` is collected in index order, and Rayon will usually preserve that order
    // when collecting into a Vec. Avoid an unconditional sort (which would allocate in
    // `sort_by_key`) unless we actually observe out-of-order results.
    let mut ordered = true;
    let mut prev_idx: Option<usize> = None;
    for (idx, _) in &child_results {
      if let Some(prev) = prev_idx {
        if *idx <= prev {
          ordered = false;
          break;
        }
      }
      prev_idx = Some(*idx);
    }
    if !ordered {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      child_results.sort_unstable_by_key(|(idx, _)| *idx);
    }

    let mut child_fragments = Vec::with_capacity(child_ids.len());
    let mut child_results = child_results.into_iter();
    let mut next_child = child_results.next();
    for idx in 0..child_ids.len() {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let child = in_flow_children[idx];
      let percentage_base = constraints
        .inline_percentage_base
        .filter(|base| base.is_finite())
        .unwrap_or(child_bounds[idx].width());
      if let Some(fragment) = reused_fragments[idx].take() {
        if !child_skipped[idx] {
          let content_size =
            self.content_box_size(&fragment, child.style.as_ref(), percentage_base);
          remembered_size_cache_store(child, content_size);
        }
        child_fragments.push(fragment);
        continue;
      }

      let Some((result_idx, fragment)) = next_child.take() else {
        return Some(Err(LayoutError::MissingContext(
          "Missing parallel grid child fragment".into(),
        )));
      };
      debug_assert_eq!(result_idx, idx, "parallel grid conversion index mismatch");
      if !child_skipped[idx] {
        let content_size = self.content_box_size(&fragment, child.style.as_ref(), percentage_base);
        remembered_size_cache_store(child, content_size);
      }
      child_fragments.push(fragment);
      next_child = child_results.next();
    }

    let root_layout = match taffy.layout(root_id) {
      Ok(layout) => layout,
      Err(e) => {
        return Some(Err(LayoutError::MissingContext(format!(
          "Taffy layout error: {:?}",
          e
        ))));
      }
    };
    let bounds = Rect::from_xywh(
      root_layout.location.x,
      root_layout.location.y,
      root_layout.size.width,
      root_layout.size.height,
    );
    let mut fragment = FragmentNode::new_with_style(
      bounds,
      FragmentContent::Block {
        box_id: Some(box_node.id),
      },
      child_fragments,
      box_node.style.clone(),
    );
    let has_in_flow_children = !fragment.children.is_empty();

    let container_style = taffy.style(root_id).ok();
    let is_grid_style = matches!(container_style.map(|style| style.display), Some(Display::Grid));
    if is_grid_style {
      let axis_style = GridAxisStyle::from_style(&box_node.style);
      if let Err(err) = self.apply_grid_baseline_alignment(
        taffy,
        root_id,
        root_layout,
        &child_ids,
        &mut fragment,
        &mut deadline_counter,
      ) {
        return Some(Err(err));
      }
      if let Err(err) = self.apply_grid_axis_mirroring(
        taffy,
        root_id,
        root_layout,
        &mut fragment,
        axis_style,
        &mut deadline_counter,
      ) {
        return Some(Err(err));
      }
      if let Some(container_style) = container_style {
        fragment.grid_tracks = grid_track_ranges_for_container(
          taffy,
          root_id,
          root_layout,
          container_style,
          axis_style,
          has_in_flow_children,
        )
        .map(Arc::new);
      }

      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
        if info.items.len() == in_flow_children.len() {
          let mut items = Vec::with_capacity(info.items.len());
          for (idx, placement) in info.items.iter().enumerate() {
            items.push(GridItemFragmentationData {
              box_id: in_flow_children[idx].id,
              row_start: placement.row_start,
              row_end: placement.row_end,
              column_start: placement.column_start,
              column_end: placement.column_end,
            });
          }
          fragment.grid_fragmentation = Some(Arc::new(GridFragmentationInfo { items }));
        }
      }
    }

    Some(Ok(fragment))
  }

  /// Converts Taffy layout results to FragmentNode tree
  fn convert_to_fragments(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    root_id: TaffyNodeId,
    constraints: &LayoutConstraints,
    containing_grid_axis: Option<GridAxisStyle>,
    auto_unskipped: Option<&FxHashSet<*const BoxNode>>,
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    measured_node_keys: &FxHashMap<TaffyNodeId, Vec<MeasureKey>>,
    positioned_children: &FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
    deadline_counter: &mut usize,
  ) -> Result<FragmentNode, LayoutError> {
    let layout = taffy
      .layout(node_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;

    let children = taffy
      .children(node_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy children error: {:?}", e)))?;

    let taffy_style = taffy.style(node_id).ok();
    let is_grid_style = matches!(taffy_style.as_ref().map(|s| s.display), Some(Display::Grid));
    let node_axis_style = if is_grid_style {
      taffy
        .get_node_context(node_id)
        .copied()
        .map(|ptr| unsafe { &*ptr })
        .map(|node| GridAxisStyle::effective_for_grid_container(&node.style, containing_grid_axis))
    } else {
      None
    };
    let child_axis_style = if is_grid_style {
      node_axis_style
    } else {
      containing_grid_axis
    };

    // Convert children recursively, propagating the effective grid axes to descendants.
    let mut child_fragments = Vec::with_capacity(children.len());
    for &child_id in children.iter() {
      check_layout_deadline(deadline_counter)?;
      child_fragments.push(self.convert_to_fragments(
        taffy,
        child_id,
        root_id,
        constraints,
        child_axis_style,
        auto_unskipped,
        measured_fragments,
        measured_node_keys,
        positioned_children,
        deadline_counter,
      )?);
    }

    // Create fragment bounds from Taffy layout.
    let bounds = Rect::from_xywh(
      layout.location.x,
      layout.location.y,
      layout.size.width,
      layout.size.height,
    );

    // Get style from node context if available
    if let Some(&box_node_ptr) = taffy.get_node_context(node_id) {
      let box_node = unsafe { &*box_node_ptr };
      let fc_type = box_node
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      if node_id == root_id || is_grid_style || !child_fragments.is_empty() {
        let mut fragment = FragmentNode::new_with_style(
          bounds,
          FragmentContent::Block {
            box_id: Some(box_node.id),
          },
          child_fragments,
          box_node.style.clone(),
        );
        let has_in_flow_children = !fragment.children.is_empty();
        if is_grid_style {
          self.apply_grid_baseline_alignment(
            taffy,
            node_id,
            layout,
            &children,
            &mut fragment,
            deadline_counter,
          )?;
          if let Some(axis_style) = node_axis_style {
            self.apply_grid_axis_mirroring(
              taffy,
              node_id,
              layout,
              &mut fragment,
              axis_style,
              deadline_counter,
            )?;
          }
          if let (Some(container_style), Some(axis_style)) = (taffy_style, node_axis_style) {
            fragment.grid_tracks = grid_track_ranges_for_container(
              taffy,
              node_id,
              layout,
              container_style,
              axis_style,
              has_in_flow_children,
            )
            .map(Arc::new);
          }

          if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
            if info.items.len() == children.len() {
              let mut items = Vec::with_capacity(info.items.len());
              for (idx, placement) in info.items.iter().enumerate() {
                let Some(&child_ptr) = taffy.get_node_context(children[idx]) else {
                  break;
                };
                let child_node = unsafe { &*child_ptr };
                items.push(GridItemFragmentationData {
                  box_id: child_node.id,
                  row_start: placement.row_start,
                  row_end: placement.row_end,
                  column_start: placement.column_start,
                  column_end: placement.column_end,
                });
              }
              if items.len() == children.len() {
                fragment.grid_fragmentation = Some(Arc::new(GridFragmentationInfo { items }));
              }
            }
          }
        }
        if let Some(positioned) = positioned_children.get(&node_id) {
          let mut abs_children = self
            .layout_positioned_children_for_node(taffy, node_id, box_node, bounds, positioned)?;
          fragment.children_mut().append(&mut abs_children);
        }
        return Ok(fragment);
      }

      let skip_contents = match box_node.style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          self.content_visibility_auto_has_definite_placeholder(box_node)
            && auto_unskipped
              .map(|set| !set.contains(&box_node_ptr))
              .unwrap_or(false)
        }
        crate::style::types::ContentVisibility::Visible => false,
      };
      if skip_contents {
        return Ok(FragmentNode::new_with_style(
          bounds,
          FragmentContent::Block {
            box_id: Some(box_node.id),
          },
          vec![],
          box_node.style.clone(),
        ));
      }

      let continuation_available = (taffy.parent(node_id) == Some(root_id))
        .then(|| {
          self.grid_item_continuation_available_block_size(
            bounds,
            constraints,
            containing_grid_axis,
          )
        })
        .flatten();

      if continuation_available.is_none() {
        if let Some(keys) = measured_node_keys.get(&node_id) {
          if let Some(mut reused) = Self::take_matching_measured_fragment(
            measured_fragments,
            keys,
            bounds.width(),
            bounds.height(),
          ) {
            fragment_clone_profile::record_fragment_reuse_without_clone(
              CloneSite::GridMeasureReuse,
            );
            let delta = Point::new(
              bounds.x() - reused.bounds.x(),
              bounds.y() - reused.bounds.y(),
            );
            translate_fragment_tree(&mut reused, delta, deadline_counter)?;
            let percentage_base = constraints
              .inline_percentage_base
              .filter(|base| base.is_finite())
              .unwrap_or(bounds.width());
            let content_size =
              self.content_box_size(&reused, box_node.style.as_ref(), percentage_base);
            remembered_size_cache_store(box_node, content_size);
            return Ok(reused);
          }
        }
      }

      let parent_scroll = self.factory.viewport_scroll();
      let child_scroll = Point::new(parent_scroll.x - bounds.x(), parent_scroll.y - bounds.y());
      let child_factory = self.factory.clone().with_viewport_scroll(child_scroll);
      let fc = child_factory.get(fc_type);

      let available_height = continuation_available.unwrap_or(bounds.height());
      let child_constraints = LayoutConstraints::new(
        CrateAvailableSpace::Definite(bounds.width()),
        CrateAvailableSpace::Definite(available_height),
      )
      .with_inline_percentage_base(
        constraints
          .inline_percentage_base
          .or_else(|| Some(bounds.width())),
      );

      let force_height = continuation_available
        .map(|available| available + 0.01 >= bounds.height())
        .unwrap_or(true);
      let supports_used_border_box = matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Flex
          | FormattingContextType::Grid
          | FormattingContextType::Inline
          | FormattingContextType::Table
      );

      let mut laid_out = if supports_used_border_box {
        let child_constraints = child_constraints.with_used_border_box_size(
          Some(bounds.width()),
          force_height.then_some(bounds.height()),
        );
        if continuation_available.is_some() {
          // Grid intrinsic sizing keywords are resolved against the initial fragmentainer size when
          // building the Taffy tree. When the grid container continues after a break, re-layout
          // the item using its original style so keywords like `fill-available` can be resolved
          // against the reduced available block size.
          crate::layout::style_override::with_style_override(
            box_node.id,
            box_node.style.clone(),
            || fc.layout(box_node, &child_constraints),
          )?
        } else {
          fc.layout(box_node, &child_constraints)?
        }
      } else if matches!(
        fc_type,
        FormattingContextType::Flex | FormattingContextType::Grid
      ) {
        let mut layout_style = (*box_node.style).clone();
        layout_style.width = Some(Length::px(bounds.width()));
        if force_height {
          layout_style.height = Some(Length::px(bounds.height()));
        }
        layout_style.width_keyword = None;
        if force_height {
          layout_style.height_keyword = None;
        }
        crate::layout::style_override::with_style_override(
          box_node.id,
          Arc::new(layout_style),
          || fc.layout(box_node, &child_constraints),
        )?
      } else {
        let mut layout_style = (*box_node.style).clone();
        layout_style.width = Some(Length::px(bounds.width()));
        if force_height {
          layout_style.height = Some(Length::px(bounds.height()));
        }
        layout_style.width_keyword = None;
        if force_height {
          layout_style.height_keyword = None;
        }
        let layout_style = Arc::new(layout_style);
        if box_node.id != 0 {
          crate::layout::style_override::with_style_override(box_node.id, layout_style, || {
            fc.layout(box_node, &child_constraints)
          })?
        } else {
          let mut layout_child = (*box_node).clone();
          layout_child.style = layout_style;
          fc.layout(&layout_child, &child_constraints)?
        }
      };
      translate_fragment_tree(
        &mut laid_out,
        Point::new(bounds.x(), bounds.y()),
        deadline_counter,
      )?;
      laid_out.content = FragmentContent::Block {
        box_id: Some(box_node.id),
      };
      laid_out.style = Some(box_node.style.clone());
      let percentage_base = constraints
        .inline_percentage_base
        .filter(|base| base.is_finite())
        .unwrap_or(bounds.width());
      let content_size = self.content_box_size(&laid_out, box_node.style.as_ref(), percentage_base);
      remembered_size_cache_store(box_node, content_size);
      Ok(laid_out)
    } else {
      Ok(FragmentNode::new_block(bounds, child_fragments))
    }
  }

  fn layout_positioned_children_for_node(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    box_node: &BoxNode,
    bounds: Rect,
    positioned_children: &[*const BoxNode],
  ) -> Result<Vec<FragmentNode>, LayoutError> {
    if positioned_children.is_empty() {
      return Ok(Vec::new());
    }

    let padding_left =
      self.resolve_length_for_width(box_node.style.padding_left, bounds.width(), &box_node.style);
    let padding_top =
      self.resolve_length_for_width(box_node.style.padding_top, bounds.width(), &box_node.style);
    let padding_right = self.resolve_length_for_width(
      box_node.style.padding_right,
      bounds.width(),
      &box_node.style,
    );
    let padding_bottom = self.resolve_length_for_width(
      box_node.style.padding_bottom,
      bounds.width(),
      &box_node.style,
    );
    let border_left = self.resolve_length_for_width(
      box_node.style.used_border_left_width(),
      bounds.width(),
      &box_node.style,
    );
    let border_top = self.resolve_length_for_width(
      box_node.style.used_border_top_width(),
      bounds.width(),
      &box_node.style,
    );
    let border_right = self.resolve_length_for_width(
      box_node.style.used_border_right_width(),
      bounds.width(),
      &box_node.style,
    );
    let border_bottom = self.resolve_length_for_width(
      box_node.style.used_border_bottom_width(),
      bounds.width(),
      &box_node.style,
    );
    // CSS 2.1 §10.1: the containing block for absolute positioned descendants is the padding box
    // of the nearest positioned ancestor, i.e. the rectangle bounded by the padding edge (border
    // box minus borders).
    let padding_origin = crate::geometry::Point::new(border_left, border_top);
    let padding_size = crate::geometry::Size::new(
      (bounds.width() - border_left - border_right).max(0.0),
      (bounds.height() - border_top - border_bottom).max(0.0),
    );
    let padding_rect = crate::geometry::Rect::new(padding_origin, padding_size);

    // Percentage sizes/offsets on absolutely positioned boxes resolve against the used size of the
    // containing block, even when the containing block's own height is `auto` (CSS 2.1 §10.5).
    let block_base = Some(padding_rect.size.height);
    let establishes_abs_cb = box_node.style.establishes_abs_containing_block();
    let establishes_fixed_cb = box_node.style.establishes_fixed_containing_block();
    let padding_cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
      padding_rect,
      self.viewport_size,
      Some(padding_rect.size.width),
      block_base,
    );
    let cb_for_absolute = if establishes_abs_cb {
      padding_cb
    } else {
      self.nearest_positioned_cb
    };
    let cb_for_fixed = if establishes_fixed_cb {
      padding_cb
    } else {
      self.nearest_fixed_cb
    };

    // `FormattingContextFactory::with_fixed_cb` / `with_positioned_cb` reset the per-factory cached
    // formatting contexts store. Build factory variants once so multiple positioned children can
    // reuse cached formatting contexts instead of rebuilding detached ones per child.
    let positioned_factory = self.factory.clone();
    let positioned_factory = if cb_for_fixed == positioned_factory.nearest_fixed_cb() {
      positioned_factory
    } else {
      positioned_factory.with_fixed_cb(cb_for_fixed)
    };
    let abs_factory = if cb_for_absolute == positioned_factory.nearest_positioned_cb() {
      positioned_factory.clone()
    } else {
      positioned_factory.with_positioned_cb(cb_for_absolute)
    };

    #[derive(Clone)]
    struct SubgridAxisContext {
      offsets: Vec<f32>,
      line_offset: u16,
      node_offset: f32,
    }

    let axes_swapped = {
      let mut effective_writing_mode = box_node.style.writing_mode;
      let mut current_id = node_id;
      let mut current_is_subgrid =
        box_node.style.grid_row_subgrid || box_node.style.grid_column_subgrid;
      while current_is_subgrid {
        let Some(parent_id) = taffy.parent(current_id) else {
          break;
        };
        let Some(parent_ptr) = taffy.get_node_context(parent_id).copied() else {
          break;
        };
        let parent_box_node = unsafe { &*parent_ptr };
        effective_writing_mode = parent_box_node.style.writing_mode;
        current_is_subgrid =
          parent_box_node.style.grid_row_subgrid || parent_box_node.style.grid_column_subgrid;
        current_id = parent_id;
      }
      !crate::style::inline_axis_is_horizontal(effective_writing_mode)
    };

    let mut row_offsets: Option<Vec<f32>> = None;
    let mut col_offsets: Option<Vec<f32>> = None;
    let mut row_subgrid_ctx: Option<SubgridAxisContext> = None;
    let mut col_subgrid_ctx: Option<SubgridAxisContext> = None;

    if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
      if let Ok(container_style) = taffy.style(node_id) {
        row_offsets = Some(compute_track_offsets(
          &info.rows,
          bounds.height(),
          padding_top,
          padding_bottom,
          border_top,
          border_bottom,
          container_style
            .align_content
            .unwrap_or(taffy::style::AlignContent::Stretch),
        ));
        col_offsets = Some(compute_track_offsets(
          &info.columns,
          bounds.width(),
          padding_left,
          padding_right,
          border_left,
          border_right,
          container_style
            .justify_content
            .unwrap_or(taffy::style::AlignContent::Stretch),
        ));
      } else if crate::debug::runtime::runtime_toggles().truthy("FASTR_LOG_GRID_STATIC_POS") {
        eprintln!(
          "[grid-static-pos] missing style for node {node_id:?} (box_id={})",
          box_node.id
        );
      }
    } else {
      // Subgrid nodes do not always expose per-node track info in Taffy. When that happens,
      // derive track offsets from the nearest ancestor grid that does provide them, then map
      // local grid line numbers into the ancestor grid's line space.
      let toggles = crate::debug::runtime::runtime_toggles();
      let maybe_axis_ctx = |axis_is_columns: bool| -> Option<SubgridAxisContext> {
        let axis_is_physical_x = if axes_swapped {
          !axis_is_columns
        } else {
          axis_is_columns
        };
        let mut line_offset: i32 = 0;
        let mut node_offset: f32 = 0.0;
        let mut current = node_id;

        loop {
          let current_ptr = *taffy.get_node_context(current)?;
          let current_box_node = unsafe { &*current_ptr };
          let is_subgrid_axis = if axis_is_columns {
            current_box_node.style.grid_column_subgrid
          } else {
            current_box_node.style.grid_row_subgrid
          };
          if !is_subgrid_axis {
            return None;
          }
          let start_line = if axis_is_columns {
            current_box_node.style.grid_column_start
          } else {
            current_box_node.style.grid_row_start
          };
          if start_line <= 0 {
            return None;
          }
          line_offset = line_offset.saturating_add(start_line.saturating_sub(1) as i32);

          let layout = taffy.layout(current).ok()?;
          node_offset += if axis_is_physical_x {
            layout.location.x
          } else {
            layout.location.y
          };

          let parent = taffy.parent(current)?;
          current = parent;

          if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(current) {
            let container_style = taffy.style(current).ok()?;
            let ancestor_layout = taffy.layout(current).ok()?;
            let ancestor_bounds = Rect::from_xywh(
              ancestor_layout.location.x,
              ancestor_layout.location.y,
              ancestor_layout.size.width,
              ancestor_layout.size.height,
            );

            let ancestor_ptr = *taffy.get_node_context(current)?;
            let ancestor_box_node = unsafe { &*ancestor_ptr };

            let ancestor_padding_left = self.resolve_length_for_width(
              ancestor_box_node.style.padding_left,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_padding_top = self.resolve_length_for_width(
              ancestor_box_node.style.padding_top,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_padding_right = self.resolve_length_for_width(
              ancestor_box_node.style.padding_right,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_padding_bottom = self.resolve_length_for_width(
              ancestor_box_node.style.padding_bottom,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_left = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_left_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_top = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_top_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_right = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_right_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_bottom = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_bottom_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );

            let offsets = if axis_is_physical_x {
              compute_track_offsets(
                &info.columns,
                ancestor_bounds.width(),
                ancestor_padding_left,
                ancestor_padding_right,
                ancestor_border_left,
                ancestor_border_right,
                container_style
                  .justify_content
                  .unwrap_or(taffy::style::AlignContent::Stretch),
              )
            } else {
              compute_track_offsets(
                &info.rows,
                ancestor_bounds.height(),
                ancestor_padding_top,
                ancestor_padding_bottom,
                ancestor_border_top,
                ancestor_border_bottom,
                container_style
                  .align_content
                  .unwrap_or(taffy::style::AlignContent::Stretch),
              )
            };

            let line_offset = u16::try_from(line_offset).ok()?;
            return Some(SubgridAxisContext {
              offsets,
              line_offset,
              node_offset,
            });
          }
        }
      };

      if box_node.style.grid_row_subgrid {
        row_subgrid_ctx = maybe_axis_ctx(false);
      }
      if box_node.style.grid_column_subgrid {
        col_subgrid_ctx = maybe_axis_ctx(true);
      }

      if toggles.truthy("FASTR_LOG_GRID_STATIC_POS")
        && row_subgrid_ctx.is_none()
        && col_subgrid_ctx.is_none()
      {
        eprintln!(
          "[grid-static-pos] missing grid track info for node {node_id:?} (box_id={})",
          box_node.id
        );
      }
    }

    let mut static_positions: FxHashMap<usize, Point> = FxHashMap::default();
    for &child_ptr in positioned_children {
      let child = unsafe { &*child_ptr };
      let mut pos = Point::ZERO;
      let (x_start, x_end, x_raw, x_ctx, x_line_count) = if axes_swapped {
        (
          child.style.grid_row_start,
          child.style.grid_row_end,
          child.style.grid_row_raw.as_deref(),
          row_subgrid_ctx.as_ref(),
          col_offsets
            .as_ref()
            .and_then(|offsets| grid_line_count_from_offsets(offsets))
            .or_else(|| {
              // Subgrid fallback: line numbers are local to the subgrid, which inherits the number
              // of tracks it spans in the parent axis.
              let start = box_node.style.grid_row_start;
              let end = box_node.style.grid_row_end;
              (start > 0 && end > start)
                .then(|| u16::try_from((end - start + 1).max(0)).ok())
                .flatten()
            }),
        )
      } else {
        (
          child.style.grid_column_start,
          child.style.grid_column_end,
          child.style.grid_column_raw.as_deref(),
          col_subgrid_ctx.as_ref(),
          col_offsets
            .as_ref()
            .and_then(|offsets| grid_line_count_from_offsets(offsets))
            .or_else(|| {
              let start = box_node.style.grid_column_start;
              let end = box_node.style.grid_column_end;
              (start > 0 && end > start)
                .then(|| u16::try_from((end - start + 1).max(0)).ok())
                .flatten()
            }),
        )
      };
      if let Some((start_line, end_line)) =
        resolve_grid_line_range_from_style(x_start, x_end, x_raw, x_line_count)
      {
        if let Some(col_offsets) = col_offsets.as_ref() {
          if let Some((start, _)) = grid_area_for_item(col_offsets, start_line, end_line) {
            pos.x = start - padding_origin.x;
          }
        } else if let Some(ctx) = x_ctx {
          let mapped_start = ctx.line_offset.saturating_add(start_line);
          let mapped_end = ctx.line_offset.saturating_add(end_line);
          if let Some((start, _)) = grid_area_for_item(&ctx.offsets, mapped_start, mapped_end) {
            pos.x = (start - ctx.node_offset) - padding_origin.x;
          }
        }
      }

      let (y_start, y_end, y_raw, y_ctx, y_line_count) = if axes_swapped {
        (
          child.style.grid_column_start,
          child.style.grid_column_end,
          child.style.grid_column_raw.as_deref(),
          col_subgrid_ctx.as_ref(),
          row_offsets
            .as_ref()
            .and_then(|offsets| grid_line_count_from_offsets(offsets))
            .or_else(|| {
              let start = box_node.style.grid_column_start;
              let end = box_node.style.grid_column_end;
              (start > 0 && end > start)
                .then(|| u16::try_from((end - start + 1).max(0)).ok())
                .flatten()
            }),
        )
      } else {
        (
          child.style.grid_row_start,
          child.style.grid_row_end,
          child.style.grid_row_raw.as_deref(),
          row_subgrid_ctx.as_ref(),
          row_offsets
            .as_ref()
            .and_then(|offsets| grid_line_count_from_offsets(offsets))
            .or_else(|| {
              let start = box_node.style.grid_row_start;
              let end = box_node.style.grid_row_end;
              (start > 0 && end > start)
                .then(|| u16::try_from((end - start + 1).max(0)).ok())
                .flatten()
            }),
        )
      };
      if let Some((start_line, end_line)) =
        resolve_grid_line_range_from_style(y_start, y_end, y_raw, y_line_count)
      {
        if let Some(row_offsets) = row_offsets.as_ref() {
          if let Some((start, _)) = grid_area_for_item(row_offsets, start_line, end_line) {
            pos.y = start - padding_origin.y;
          }
        } else if let Some(ctx) = y_ctx {
          let mapped_start = ctx.line_offset.saturating_add(start_line);
          let mapped_end = ctx.line_offset.saturating_add(end_line);
          if let Some((start, _)) = grid_area_for_item(&ctx.offsets, mapped_start, mapped_end) {
            pos.y = (start - ctx.node_offset) - padding_origin.y;
          }
        }
      }
      static_positions.insert(ensure_box_id(child), pos);
    }

    let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
      self.font_context.clone(),
    );
    let mut fragments = Vec::with_capacity(positioned_children.len());
    let mut deadline_counter = 0usize;
    for &child_ptr in positioned_children {
      check_layout_deadline(&mut deadline_counter)?;
      let child = unsafe { &*child_ptr };

      let cb = match child.style.position {
        crate::style::position::Position::Fixed => cb_for_fixed,
        _ => cb_for_absolute,
      };

      let fc_type = child
        .formatting_context()
        .unwrap_or(crate::style::display::FormattingContextType::Block);
      let fc = abs_factory.get(fc_type);
      let child_constraints = LayoutConstraints::new(
        CrateAvailableSpace::Definite(padding_rect.size.width),
        block_base
          .map(CrateAvailableSpace::Definite)
          .unwrap_or(CrateAvailableSpace::Indefinite),
      );

      let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
        &child.style,
        &cb,
        self.viewport_size,
        &self.font_context,
      );
      // Static position resolves to where the element would be in flow, relative to the containing
      // block origin (padding edge).
      let static_pos = static_positions
        .get(&ensure_box_id(child))
        .copied()
        .unwrap_or(crate::geometry::Point::ZERO);
      let width_keyword = child.style.width_keyword;
      let min_width_keyword = child.style.min_width_keyword;
      let max_width_keyword = child.style.max_width_keyword;
      let height_keyword = child.style.height_keyword;
      let min_height_keyword = child.style.min_height_keyword;
      let max_height_keyword = child.style.max_height_keyword;
      let has_inline_keyword =
        width_keyword.is_some() || min_width_keyword.is_some() || max_width_keyword.is_some();
      let has_block_keyword =
        height_keyword.is_some() || min_height_keyword.is_some() || max_height_keyword.is_some();
      let needs_inline_intrinsics = has_inline_keyword
        || (positioned_style.width.is_auto()
          && (positioned_style.left.is_auto()
            || positioned_style.right.is_auto()
            || child.is_replaced()));
      let needs_block_intrinsics = has_block_keyword
        || (positioned_style.height.is_auto()
          && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()));
      let mut static_style = (*child.style).clone();
      static_style.position = crate::style::position::Position::Relative;
      static_style.top = crate::style::types::InsetValue::Auto;
      static_style.right = crate::style::types::InsetValue::Auto;
      static_style.bottom = crate::style::types::InsetValue::Auto;
      static_style.left = crate::style::types::InsetValue::Auto;
      let static_style = Arc::new(static_style);

      let (
        mut child_fragment,
        preferred_min_inline,
        preferred_inline,
        preferred_min_block,
        preferred_block,
      ) = if child.id != 0 {
        crate::layout::style_override::with_style_override(child.id, static_style.clone(), || {
          let child_fragment = fc.layout(child, &child_constraints)?;
          let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
            match fc.compute_intrinsic_inline_sizes(child) {
              Ok((min, max)) => (Some(min), Some(max)),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {
                let min =
                  match fc.compute_intrinsic_inline_size(child, IntrinsicSizingMode::MinContent) {
                    Ok(size) => Some(size),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                let max =
                  match fc.compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent) {
                    Ok(size) => Some(size),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                (min, max)
              }
            }
          } else {
            (None, None)
          };
          let preferred_min_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MinContent) {
              Ok(size) => Some(size),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          let preferred_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MaxContent) {
              Ok(size) => Some(size),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          Ok((
            child_fragment,
            preferred_min_inline,
            preferred_inline,
            preferred_min_block,
            preferred_block,
          ))
        })?
      } else {
        let mut layout_child = child.clone();
        layout_child.style = static_style.clone();
        let child_fragment = fc.layout(&layout_child, &child_constraints)?;
        let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
          match fc.compute_intrinsic_inline_sizes(&layout_child) {
            Ok((min, max)) => (Some(min), Some(max)),
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {
              let min = match fc
                .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MinContent)
              {
                Ok(size) => Some(size),
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => None,
              };
              let max = match fc
                .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
              {
                Ok(size) => Some(size),
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => None,
              };
              (min, max)
            }
          }
        } else {
          (None, None)
        };
        let preferred_min_block = if needs_block_intrinsics {
          match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent) {
            Ok(size) => Some(size),
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => None,
          }
        } else {
          None
        };
        let preferred_block = if needs_block_intrinsics {
          match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent) {
            Ok(size) => Some(size),
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => None,
          }
        } else {
          None
        };
        (
          child_fragment,
          preferred_min_inline,
          preferred_inline,
          preferred_min_block,
          preferred_block,
        )
      };

      let actual_horizontal = positioned_style.padding.left
        + positioned_style.padding.right
        + positioned_style.border_width.left
        + positioned_style.border_width.right;
      let actual_vertical = positioned_style.padding.top
        + positioned_style.padding.bottom
        + positioned_style.border_width.top
        + positioned_style.border_width.bottom;
      let content_offset = Point::new(
        positioned_style.border_width.left + positioned_style.padding.left,
        positioned_style.border_width.top + positioned_style.padding.top,
      );
      let (intrinsic_horizontal, intrinsic_vertical) =
        crate::layout::absolute_positioning::intrinsic_edge_sizes(
          &child.style,
          self.viewport_size,
          &self.font_context,
        );
      let preferred_min_inline = preferred_min_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
      let preferred_inline = preferred_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
      let preferred_min_block = preferred_min_block.map(|v| (v - intrinsic_vertical).max(0.0));
      let preferred_block = preferred_block.map(|v| (v - intrinsic_vertical).max(0.0));
      let intrinsic_size = Size::new(
        (child_fragment.bounds.size.width - actual_horizontal).max(0.0),
        (child_fragment.bounds.size.height - actual_vertical).max(0.0),
      );

      let mut input = crate::layout::absolute_positioning::AbsoluteLayoutInput::new(
        positioned_style,
        intrinsic_size,
        static_pos,
      );
      input.style.width_keyword = width_keyword;
      input.style.min_width_keyword = min_width_keyword;
      input.style.max_width_keyword = max_width_keyword;
      input.style.height_keyword = height_keyword;
      input.style.min_height_keyword = min_height_keyword;
      input.style.max_height_keyword = max_height_keyword;
      input.is_replaced = child.is_replaced();
      input.preferred_min_inline_size = preferred_min_inline;
      input.preferred_inline_size = preferred_inline;
      input.preferred_min_block_size = preferred_min_block;
      input.preferred_block_size = preferred_block;
      let result = abs.layout_absolute(&input, &cb)?;
      let border_size = Size::new(
        result.size.width + actual_horizontal,
        result.size.height + actual_vertical,
      );
      let border_origin = Point::new(
        result.position.x - content_offset.x,
        result.position.y - content_offset.y,
      );
      let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
        || (border_size.height - child_fragment.bounds.height()).abs() > 0.01;
      if needs_relayout {
        let supports_used_border_box = matches!(
          fc_type,
          FormattingContextType::Block
            | FormattingContextType::Flex
            | FormattingContextType::Grid
            | FormattingContextType::Inline
            | FormattingContextType::Table
        );
        let relayout_constraints = child_constraints
          .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
        if child.id != 0 {
          if supports_used_border_box {
            child_fragment = crate::layout::style_override::with_style_override(
              child.id,
              static_style.clone(),
              || fc.layout(child, &relayout_constraints),
            )?;
          } else {
            let mut relayout_style = (*static_style).clone();
            relayout_style.width = Some(Length::px(border_size.width));
            relayout_style.height = Some(Length::px(border_size.height));
            relayout_style.width_keyword = None;
            relayout_style.height_keyword = None;
            child_fragment = crate::layout::style_override::with_style_override(
              child.id,
              Arc::new(relayout_style),
              || fc.layout(child, &relayout_constraints),
            )?;
          }
        } else {
          let mut relayout_child = child.clone();
          if supports_used_border_box {
            relayout_child.style = static_style.clone();
          } else {
            let mut relayout_style = (*static_style).clone();
            relayout_style.width = Some(Length::px(border_size.width));
            relayout_style.height = Some(Length::px(border_size.height));
            relayout_style.width_keyword = None;
            relayout_style.height_keyword = None;
            relayout_child.style = Arc::new(relayout_style);
          }
          child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
        }
      }
      child_fragment.bounds = crate::geometry::Rect::new(border_origin, border_size);
      child_fragment.style = Some(child.style.clone());
      fragments.push(child_fragment);
    }

    Ok(fragments)
  }

  fn alignment_for_axis(
    &self,
    axis: Axis,
    child_style: &taffy::style::Style,
    container_style: &taffy::style::Style,
  ) -> taffy::style::AlignItems {
    let fallback = match axis {
      Axis::Vertical => {
        if !child_style.size.height.is_auto() || child_style.aspect_ratio.is_some() {
          taffy::style::AlignItems::Start
        } else {
          taffy::style::AlignItems::Stretch
        }
      }
      Axis::Horizontal => {
        if !child_style.size.width.is_auto() {
          taffy::style::AlignItems::Start
        } else {
          taffy::style::AlignItems::Stretch
        }
      }
    };

    match axis {
      Axis::Vertical => child_style
        .align_self
        .or(container_style.align_items)
        .unwrap_or(fallback),
      Axis::Horizontal => child_style
        .justify_self
        .or(container_style.justify_items)
        .unwrap_or(fallback),
    }
  }

  fn baseline_offset_with_fallback(
    &self,
    fragment: &FragmentNode,
    axis: Axis,
    deadline_counter: &mut usize,
  ) -> Result<Option<f32>, LayoutError> {
    if let Some(offset) = first_baseline_offset(fragment, deadline_counter)? {
      return Ok(Some(offset));
    }

    let size = match axis {
      Axis::Vertical => fragment.bounds.height(),
      Axis::Horizontal => fragment.bounds.width(),
    };
    if size.is_finite() && size > 0.0 {
      Ok(Some(size))
    } else {
      Ok(None)
    }
  }

  fn apply_baseline_group(
    &self,
    axis: Axis,
    group: &[BaselineItem],
    fragment: &mut FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    // Baseline alignment only has an effect when two or more items participate in the same
    // baseline-sharing group.
    if group.len() <= 1 {
      return Ok(());
    }

    let debug_baseline =
      crate::debug::runtime::runtime_toggles().truthy("FASTR_DEBUG_GRID_BASELINE");

    let mut target = 0.0;
    for item in group {
      check_layout_deadline(deadline_counter)?;
      let area_size = (item.area_end - item.area_start).max(0.0);
      let clamped = item.baseline.min(area_size);
      if clamped > target {
        target = clamped;
      }
    }

    if debug_baseline {
      let axis_name = match axis {
        Axis::Horizontal => "horizontal",
        Axis::Vertical => "vertical",
      };
      eprintln!(
        "[grid-baseline] axis={} group_len={} target={:.2}",
        axis_name,
        group.len(),
        target
      );
    }

    for item in group {
      check_layout_deadline(deadline_counter)?;
      let area_size = (item.area_end - item.area_start).max(0.0);
      if area_size <= 0.0 {
        if debug_baseline {
          eprintln!(
            "[grid-baseline] idx={} non-positive area (area_start={:.2} area_end={:.2})",
            item.idx, item.area_start, item.area_end
          );
        }
        continue;
      }
      let upper = (item.area_end - item.size).max(item.area_start);
      let desired_start = (item.area_start + target - item.baseline).clamp(item.area_start, upper);
      let delta = desired_start - item.start;
      if delta.abs() > 0.01 {
        if debug_baseline {
          eprintln!(
            "[grid-baseline] idx={} area=({:.2},{:.2}) baseline={:.2} start={:.2} size={:.2} desired_start={:.2} delta={:.2}",
            item.idx,
            item.area_start,
            item.area_end,
            item.baseline,
            item.start,
            item.size,
            desired_start,
            delta
          );
        }
        if let Some(child) = fragment.children_mut().get_mut(item.idx) {
          translate_along_axis(child, axis, delta, deadline_counter)?;
        }
      } else if debug_baseline {
        eprintln!(
          "[grid-baseline] idx={} area=({:.2},{:.2}) baseline={:.2} start={:.2} size={:.2} desired_start={:.2} delta={:.4}",
          item.idx,
          item.area_start,
          item.area_end,
          item.baseline,
          item.start,
          item.size,
          desired_start,
          delta
        );
      }
    }
    Ok(())
  }

  fn apply_grid_baseline_alignment(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    layout: &TaffyLayout,
    child_ids: &[TaffyNodeId],
    fragment: &mut FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    let detailed = match taffy.detailed_layout_info(node_id) {
      DetailedLayoutInfo::Grid(info) => info,
      _ => return Ok(()),
    };
    if fragment.children.is_empty() {
      return Ok(());
    }
    if detailed.items.len() != fragment.children.len() || child_ids.len() != fragment.children.len()
    {
      return Ok(());
    }

    let container_style = match taffy.style(node_id) {
      Ok(style) => style,
      Err(_) => return Ok(()),
    };

    let row_offsets = compute_track_offsets(
      &detailed.rows,
      layout.size.height,
      layout.padding.top,
      layout.padding.bottom,
      layout.border.top,
      layout.border.bottom,
      container_style
        .align_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );
    let col_offsets = compute_track_offsets(
      &detailed.columns,
      layout.size.width,
      layout.padding.left,
      layout.padding.right,
      layout.border.left,
      layout.border.right,
      container_style
        .justify_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );

    let debug_baseline =
      crate::debug::runtime::runtime_toggles().truthy("FASTR_DEBUG_GRID_BASELINE");
    if debug_baseline {
      eprintln!(
        "[grid-baseline] rows sizes={:?} gutters={:?} offsets={:?}",
        detailed.rows.sizes, detailed.rows.gutters, row_offsets
      );
      eprintln!(
        "[grid-baseline] cols sizes={:?} gutters={:?} offsets={:?}",
        detailed.columns.sizes, detailed.columns.gutters, col_offsets
      );
    }

    let mut row_groups: FxHashMap<u16, Vec<BaselineItem>> = FxHashMap::default();

    for (idx, ((child_id, item_info), child_fragment)) in child_ids
      .iter()
      .zip(detailed.items.iter())
      .zip(fragment.children.iter())
      .enumerate()
    {
      check_layout_deadline(deadline_counter)?;
      let child_style = match taffy.style(*child_id) {
        Ok(style) => style,
        Err(_) => continue,
      };

      if self.alignment_for_axis(Axis::Vertical, child_style, container_style)
        == taffy::style::AlignItems::Baseline
      {
        let baseline =
          self.baseline_offset_with_fallback(child_fragment, Axis::Vertical, deadline_counter)?;
        if let (Some((area_start, area_end)), Some(baseline)) = (
          grid_area_for_item(&row_offsets, item_info.row_start, item_info.row_end),
          baseline,
        ) {
          if debug_baseline {
            eprintln!(
              "[grid-baseline] idx={} row=({},{}) area=({:.2},{:.2}) start={:.2} size={:.2} baseline={:.2}",
              idx,
              item_info.row_start,
              item_info.row_end,
              area_start,
              area_end,
              child_fragment.bounds.y(),
              child_fragment.bounds.height(),
              baseline
            );
          }
          row_groups
            .entry(item_info.row_start)
            .or_default()
            .push(BaselineItem {
              idx,
              area_start,
              area_end,
              baseline,
              start: child_fragment.bounds.y(),
              size: child_fragment.bounds.height(),
            });
        }
      }
    }

    for group in row_groups.values() {
      self.apply_baseline_group(Axis::Vertical, group, fragment, deadline_counter)?;
    }
    Ok(())
  }

  fn apply_grid_axis_mirroring(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    layout: &TaffyLayout,
    fragment: &mut FragmentNode,
    axis_style: GridAxisStyle,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    if fragment.children.is_empty() {
      return Ok(());
    }

    let inline_is_horizontal = axis_style.inline_is_horizontal();
    let mut mirror_x = false;
    let mut mirror_y = false;

    if !axis_style.inline_positive() {
      if inline_is_horizontal {
        mirror_x = true;
      } else {
        mirror_y = true;
      }
    }
    if !axis_style.block_positive() {
      if inline_is_horizontal {
        mirror_y = true;
      } else {
        mirror_x = true;
      }
    }

    if !mirror_x && !mirror_y {
      return Ok(());
    }

    let container_style = match taffy.style(node_id) {
      Ok(style) => style,
      Err(_) => return Ok(()),
    };

    let compute_region = |axis: Axis, children: &[FragmentNode]| -> Option<(f32, f32)> {
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        let offsets = match axis {
          Axis::Horizontal => compute_track_offsets(
            &info.columns,
            layout.size.width,
            layout.padding.left,
            layout.padding.right,
            layout.border.left,
            layout.border.right,
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
          ),
          Axis::Vertical => compute_track_offsets(
            &info.rows,
            layout.size.height,
            layout.padding.top,
            layout.padding.bottom,
            layout.border.top,
            layout.border.bottom,
            container_style
              .align_content
              .unwrap_or(TaffyAlignContent::Stretch),
          ),
        };
        if offsets.len() >= 2 {
          let start = *offsets.get(1)?;
          let end = *offsets.last()?;
          return Some((start, end));
        }
      }

      // Fallback: infer the span of the used grid tracks from child positions. This is less robust
      // when tracks are empty, but avoids direction bugs for subgrids that don't expose detailed
      // track info via Taffy.
      let mut min_start = f32::INFINITY;
      let mut max_end = f32::NEG_INFINITY;
      for child in children {
        match axis {
          Axis::Horizontal => {
            min_start = min_start.min(child.bounds.x());
            max_end = max_end.max(child.bounds.x() + child.bounds.width());
          }
          Axis::Vertical => {
            min_start = min_start.min(child.bounds.y());
            max_end = max_end.max(child.bounds.y() + child.bounds.height());
          }
        }
      }
      if min_start.is_finite() && max_end.is_finite() && max_end >= min_start {
        Some((min_start, max_end))
      } else {
        None
      }
    };

    let (x_region, y_region) = {
      let children = fragment.children.as_slice();
      (
        mirror_x
          .then(|| compute_region(Axis::Horizontal, children))
          .flatten(),
        mirror_y
          .then(|| compute_region(Axis::Vertical, children))
          .flatten(),
      )
    };

    let mut rtl_mirrored_x = false;
    if mirror_x && inline_is_horizontal && !axis_style.inline_positive() {
      let child_ids = match taffy.children(node_id) {
        Ok(children) => children,
        Err(_) => Vec::new(),
      };

      let apply_translation = |area_start: f32,
                               area_end: f32,
                               span_start: f32,
                               span_end: f32,
                               child_id: TaffyNodeId,
                               child_fragment: &mut FragmentNode,
                               deadline_counter: &mut usize|
       -> Result<(), LayoutError> {
        let child_style = match taffy.style(child_id) {
          Ok(style) => style,
          Err(_) => return Ok(()),
        };
        let width = child_fragment.bounds.width();
        if !width.is_finite() || width <= 0.0 {
          return Ok(());
        }

        let (area_start, area_end) = if area_start <= area_end {
          (area_start, area_end)
        } else {
          (area_end, area_start)
        };
        let (span_start, span_end) = if span_start <= span_end {
          (span_start, span_end)
        } else {
          (span_end, span_start)
        };
        let span_size = span_end - span_start;
        if !span_size.is_finite() || span_size <= 0.0 {
          return Ok(());
        }

        let mirrored_area_start = span_start + (span_end - area_end);
        let mirrored_area_end = span_start + (span_end - area_start);

        let old_start = child_fragment.bounds.x();
        let offset_from_start = old_start - area_start;
        let offset_from_end = area_end - (old_start + width);

        let alignment = self.alignment_for_axis(Axis::Horizontal, child_style, container_style);
        let unclamped_new_start = match alignment {
          taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => {
            mirrored_area_end - width - offset_from_end
          }
          _ => mirrored_area_start + offset_from_start,
        };

        let upper = (mirrored_area_end - width).max(mirrored_area_start);
        let new_start = unclamped_new_start.clamp(mirrored_area_start, upper);
        let delta = new_start - old_start;
        translate_along_axis(child_fragment, Axis::Horizontal, delta, deadline_counter)
      };

      // Prefer using detailed track/item info from Taffy when available.
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        if info.items.len() == fragment.children.len()
          && child_ids.len() == fragment.children.len()
          && !info.columns.sizes.is_empty()
        {
          let col_offsets = compute_track_offsets(
            &info.columns,
            layout.size.width,
            layout.padding.left,
            layout.padding.right,
            layout.border.left,
            layout.border.right,
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let track_count = info.columns.sizes.len() as u16;
          if let Some((span_start, span_end)) =
            grid_area_for_item(&col_offsets, 1, track_count.saturating_add(1))
          {
            let children = fragment.children_mut();
            for idx in 0..children.len() {
              check_layout_deadline(deadline_counter)?;
              let item = &info.items[idx];
              if let Some((area_start, area_end)) =
                grid_area_for_item(&col_offsets, item.column_start, item.column_end)
              {
                apply_translation(
                  area_start,
                  area_end,
                  span_start,
                  span_end,
                  child_ids[idx],
                  &mut children[idx],
                  deadline_counter,
                )?;
              } else {
                let child = &mut children[idx];
                let width = child.bounds.width();
                let old_start = child.bounds.x();
                let new_start = span_start + (span_end - (old_start + width));
                translate_along_axis(
                  child,
                  Axis::Horizontal,
                  new_start - old_start,
                  deadline_counter,
                )?;
              }
            }
            rtl_mirrored_x = true;
          }
        }
      }

      // Subgrid nodes sometimes don't expose detailed track info. When possible, derive the
      // inherited column offsets from the nearest ancestor grid and map local line numbers into that
      // line space.
      if !rtl_mirrored_x {
        if let Some(&box_node_ptr) = taffy.get_node_context(node_id) {
          let box_node = unsafe { &*box_node_ptr };
          if box_node.style.grid_column_subgrid {
            #[derive(Clone)]
            struct SubgridAxisContext {
              offsets: Vec<f32>,
              line_offset: u16,
              node_offset: f32,
            }

            let mut line_offset: i32 = 0;
            let mut node_offset: f32 = 0.0;
            let mut current = node_id;
            let mut ctx: Option<SubgridAxisContext> = None;
            loop {
              let Some(&current_ptr) = taffy.get_node_context(current) else {
                break;
              };
              let current_box_node = unsafe { &*current_ptr };
              if !current_box_node.style.grid_column_subgrid {
                break;
              }
              let start_line = current_box_node.style.grid_column_start;
              if start_line <= 0 {
                break;
              }
              line_offset = line_offset.saturating_add(start_line.saturating_sub(1));
              let layout = match taffy.layout(current) {
                Ok(layout) => layout,
                Err(_) => break,
              };
              node_offset += layout.location.x;

              let Some(parent) = taffy.parent(current) else {
                break;
              };
              current = parent;

              if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(current) {
                let Ok(ancestor_style) = taffy.style(current) else {
                  break;
                };
                let Ok(ancestor_layout) = taffy.layout(current) else {
                  break;
                };
                let offsets = compute_track_offsets(
                  &info.columns,
                  ancestor_layout.size.width,
                  ancestor_layout.padding.left,
                  ancestor_layout.padding.right,
                  ancestor_layout.border.left,
                  ancestor_layout.border.right,
                  ancestor_style
                    .justify_content
                    .unwrap_or(TaffyAlignContent::Stretch),
                );
                let line_offset = match u16::try_from(line_offset) {
                  Ok(offset) => offset,
                  Err(_) => break,
                };
                ctx = Some(SubgridAxisContext {
                  offsets,
                  line_offset,
                  node_offset,
                });
                break;
              }
            }

            if let Some(ctx) = ctx {
              let span_tracks =
                (box_node.style.grid_column_end - box_node.style.grid_column_start).max(0);
              if span_tracks > 0 {
                let local_end_line = (span_tracks as u16).saturating_add(1);
                let mapped_start = ctx.line_offset.saturating_add(1);
                let mapped_end = ctx.line_offset.saturating_add(local_end_line);
                if let Some((span_start, span_end)) =
                  grid_area_for_item(&ctx.offsets, mapped_start, mapped_end)
                {
                  let span_start = span_start - ctx.node_offset;
                  let span_end = span_end - ctx.node_offset;
                  let children = fragment.children_mut();
                  for idx in 0..children.len() {
                    check_layout_deadline(deadline_counter)?;
                    let child_id = child_ids.get(idx).copied().unwrap_or(node_id);
                    let mut translated = false;
                    if let Some(&child_ptr) = taffy.get_node_context(child_id) {
                      let child_node = unsafe { &*child_ptr };
                      if child_node.style.grid_column_start > 0
                        && child_node.style.grid_column_end > 0
                      {
                        let child_start = child_node.style.grid_column_start as u16;
                        let child_end = child_node.style.grid_column_end as u16;
                        let mapped_child_start = ctx.line_offset.saturating_add(child_start);
                        let mapped_child_end = ctx.line_offset.saturating_add(child_end);
                        if let Some((area_start, area_end)) =
                          grid_area_for_item(&ctx.offsets, mapped_child_start, mapped_child_end)
                        {
                          let area_start = area_start - ctx.node_offset;
                          let area_end = area_end - ctx.node_offset;
                          apply_translation(
                            area_start,
                            area_end,
                            span_start,
                            span_end,
                            child_id,
                            &mut children[idx],
                            deadline_counter,
                          )?;
                          translated = true;
                        }
                      }
                    }

                    if !translated {
                      let child = &mut children[idx];
                      let width = child.bounds.width();
                      let old_start = child.bounds.x();
                      let new_start = span_start + (span_end - (old_start + width));
                      translate_along_axis(
                        child,
                        Axis::Horizontal,
                        new_start - old_start,
                        deadline_counter,
                      )?;
                    }
                  }
                  rtl_mirrored_x = true;
                }
              }
            }
          }
        }
      }
    }

    let mut block_mirrored_x = false;
    if mirror_x && !inline_is_horizontal && !axis_style.block_positive() {
      // For vertical writing modes with a negative block axis (e.g. `writing-mode: vertical-rl`),
      // we mirror the horizontal axis. The style conversion already maps `start`/`end` alignment
      // values into Taffy's coordinate system, so the mirroring step must preserve an item's
      // alignment within its grid area (rather than blindly mirroring the child bounding box).
      //
      // This mirrors the RTL-specific handling above, but applies when the block axis is the one
      // running right-to-left.
      let child_ids = match taffy.children(node_id) {
        Ok(children) => children,
        Err(_) => Vec::new(),
      };

      let apply_translation = |area_start: f32,
                               area_end: f32,
                               span_start: f32,
                               span_end: f32,
                               child_id: TaffyNodeId,
                               child_fragment: &mut FragmentNode,
                               deadline_counter: &mut usize|
       -> Result<(), LayoutError> {
        let child_style = match taffy.style(child_id) {
          Ok(style) => style,
          Err(_) => return Ok(()),
        };
        let width = child_fragment.bounds.width();
        if !width.is_finite() || width <= 0.0 {
          return Ok(());
        }

        let (area_start, area_end) = if area_start <= area_end {
          (area_start, area_end)
        } else {
          (area_end, area_start)
        };
        let (span_start, span_end) = if span_start <= span_end {
          (span_start, span_end)
        } else {
          (span_end, span_start)
        };
        let span_size = span_end - span_start;
        if !span_size.is_finite() || span_size <= 0.0 {
          return Ok(());
        }

        let mirrored_area_start = span_start + (span_end - area_end);
        let mirrored_area_end = span_start + (span_end - area_start);

        let old_start = child_fragment.bounds.x();
        let offset_from_start = old_start - area_start;
        let offset_from_end = area_end - (old_start + width);

        let alignment = self.alignment_for_axis(Axis::Horizontal, child_style, container_style);
        let unclamped_new_start = match alignment {
          taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => {
            mirrored_area_end - width - offset_from_end
          }
          _ => mirrored_area_start + offset_from_start,
        };

        let upper = (mirrored_area_end - width).max(mirrored_area_start);
        let new_start = unclamped_new_start.clamp(mirrored_area_start, upper);
        let delta = new_start - old_start;
        translate_along_axis(child_fragment, Axis::Horizontal, delta, deadline_counter)
      };

      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        if info.items.len() == fragment.children.len()
          && child_ids.len() == fragment.children.len()
          && !info.columns.sizes.is_empty()
        {
          let col_offsets = compute_track_offsets(
            &info.columns,
            layout.size.width,
            layout.padding.left,
            layout.padding.right,
            layout.border.left,
            layout.border.right,
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let track_count = info.columns.sizes.len() as u16;
          if let Some((span_start, span_end)) =
            grid_area_for_item(&col_offsets, 1, track_count.saturating_add(1))
          {
            let children = fragment.children_mut();
            for idx in 0..children.len() {
              check_layout_deadline(deadline_counter)?;
              let item = &info.items[idx];
              if let Some((area_start, area_end)) =
                grid_area_for_item(&col_offsets, item.column_start, item.column_end)
              {
                apply_translation(
                  area_start,
                  area_end,
                  span_start,
                  span_end,
                  child_ids[idx],
                  &mut children[idx],
                  deadline_counter,
                )?;
              } else {
                let child = &mut children[idx];
                let width = child.bounds.width();
                let old_start = child.bounds.x();
                let new_start = span_start + (span_end - (old_start + width));
                translate_along_axis(child, Axis::Horizontal, new_start - old_start, deadline_counter)?;
              }
            }
            block_mirrored_x = true;
          }
        }
      }
    }

    let mut rtl_mirrored_y = false;
    if mirror_y && !inline_is_horizontal && !axis_style.inline_positive() {
      let child_ids = match taffy.children(node_id) {
        Ok(children) => children,
        Err(_) => Vec::new(),
      };

      let apply_translation = |area_start: f32,
                               area_end: f32,
                               span_start: f32,
                               span_end: f32,
                               child_id: TaffyNodeId,
                               child_fragment: &mut FragmentNode,
                               deadline_counter: &mut usize|
       -> Result<(), LayoutError> {
        let child_style = match taffy.style(child_id) {
          Ok(style) => style,
          Err(_) => return Ok(()),
        };
        let height = child_fragment.bounds.height();
        if !height.is_finite() || height <= 0.0 {
          return Ok(());
        }

        let (area_start, area_end) = if area_start <= area_end {
          (area_start, area_end)
        } else {
          (area_end, area_start)
        };
        let (span_start, span_end) = if span_start <= span_end {
          (span_start, span_end)
        } else {
          (span_end, span_start)
        };
        let span_size = span_end - span_start;
        if !span_size.is_finite() || span_size <= 0.0 {
          return Ok(());
        }

        let mirrored_area_start = span_start + (span_end - area_end);
        let mirrored_area_end = span_start + (span_end - area_start);

        let old_start = child_fragment.bounds.y();
        let offset_from_start = old_start - area_start;
        let offset_from_end = area_end - (old_start + height);

        let alignment = self.alignment_for_axis(Axis::Vertical, child_style, container_style);
        let unclamped_new_start = match alignment {
          taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => {
            mirrored_area_end - height - offset_from_end
          }
          _ => mirrored_area_start + offset_from_start,
        };

        let upper = (mirrored_area_end - height).max(mirrored_area_start);
        let new_start = unclamped_new_start.clamp(mirrored_area_start, upper);
        let delta = new_start - old_start;
        translate_along_axis(child_fragment, Axis::Vertical, delta, deadline_counter)
      };

      // Prefer using detailed track/item info from Taffy when available.
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        if info.items.len() == fragment.children.len()
          && child_ids.len() == fragment.children.len()
          && !info.rows.sizes.is_empty()
        {
          let row_offsets = compute_track_offsets(
            &info.rows,
            layout.size.height,
            layout.padding.top,
            layout.padding.bottom,
            layout.border.top,
            layout.border.bottom,
            container_style
              .align_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let track_count = info.rows.sizes.len() as u16;
          if let Some((span_start, span_end)) =
            grid_area_for_item(&row_offsets, 1, track_count.saturating_add(1))
          {
            let children = fragment.children_mut();
            for idx in 0..children.len() {
              check_layout_deadline(deadline_counter)?;
              let item = &info.items[idx];
              if let Some((area_start, area_end)) =
                grid_area_for_item(&row_offsets, item.row_start, item.row_end)
              {
                apply_translation(
                  area_start,
                  area_end,
                  span_start,
                  span_end,
                  child_ids[idx],
                  &mut children[idx],
                  deadline_counter,
                )?;
              } else {
                let child = &mut children[idx];
                let height = child.bounds.height();
                let old_start = child.bounds.y();
                let new_start = span_start + (span_end - (old_start + height));
                translate_along_axis(child, Axis::Vertical, new_start - old_start, deadline_counter)?;
              }
            }
            rtl_mirrored_y = true;
          }
        }
      }

      // Subgrid nodes sometimes don't expose detailed track info. When possible, derive the
      // inherited inline axis offsets from the nearest ancestor grid and map local line numbers
      // into that line space.
      if !rtl_mirrored_y {
        if let Some(&box_node_ptr) = taffy.get_node_context(node_id) {
          let box_node = unsafe { &*box_node_ptr };
          if box_node.style.grid_column_subgrid {
            #[derive(Clone)]
            struct SubgridAxisContext {
              offsets: Vec<f32>,
              line_offset: u16,
              node_offset: f32,
            }

            let mut line_offset: i32 = 0;
            let mut node_offset: f32 = 0.0;
            let mut current = node_id;
            let mut ctx: Option<SubgridAxisContext> = None;
            loop {
              let Some(&current_ptr) = taffy.get_node_context(current) else {
                break;
              };
              let current_box_node = unsafe { &*current_ptr };
              if !current_box_node.style.grid_column_subgrid {
                break;
              }
              let start_line = current_box_node.style.grid_column_start;
              if start_line <= 0 {
                break;
              }
              line_offset = line_offset.saturating_add(start_line.saturating_sub(1));
              let layout = match taffy.layout(current) {
                Ok(layout) => layout,
                Err(_) => break,
              };
              node_offset += layout.location.y;

              let Some(parent) = taffy.parent(current) else {
                break;
              };
              current = parent;

              if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(current) {
                let Ok(ancestor_style) = taffy.style(current) else {
                  break;
                };
                let Ok(ancestor_layout) = taffy.layout(current) else {
                  break;
                };
                let offsets = compute_track_offsets(
                  &info.rows,
                  ancestor_layout.size.height,
                  ancestor_layout.padding.top,
                  ancestor_layout.padding.bottom,
                  ancestor_layout.border.top,
                  ancestor_layout.border.bottom,
                  ancestor_style
                    .align_content
                    .unwrap_or(TaffyAlignContent::Stretch),
                );
                let line_offset = match u16::try_from(line_offset) {
                  Ok(offset) => offset,
                  Err(_) => break,
                };
                ctx = Some(SubgridAxisContext {
                  offsets,
                  line_offset,
                  node_offset,
                });
                break;
              }
            }

            if let Some(ctx) = ctx {
              let span_tracks =
                (box_node.style.grid_column_end - box_node.style.grid_column_start).max(0);
              if span_tracks > 0 {
                let local_end_line = (span_tracks as u16).saturating_add(1);
                let mapped_start = ctx.line_offset.saturating_add(1);
                let mapped_end = ctx.line_offset.saturating_add(local_end_line);
                if let Some((span_start, span_end)) =
                  grid_area_for_item(&ctx.offsets, mapped_start, mapped_end)
                {
                  let span_start = span_start - ctx.node_offset;
                  let span_end = span_end - ctx.node_offset;
                  let children = fragment.children_mut();
                  for idx in 0..children.len() {
                    check_layout_deadline(deadline_counter)?;
                    let child_id = child_ids.get(idx).copied().unwrap_or(node_id);
                    let mut translated = false;
                    if let Some(&child_ptr) = taffy.get_node_context(child_id) {
                      let child_node = unsafe { &*child_ptr };
                      if child_node.style.grid_column_start > 0 && child_node.style.grid_column_end > 0 {
                        let child_start = child_node.style.grid_column_start as u16;
                        let child_end = child_node.style.grid_column_end as u16;
                        let mapped_child_start = ctx.line_offset.saturating_add(child_start);
                        let mapped_child_end = ctx.line_offset.saturating_add(child_end);
                        if let Some((area_start, area_end)) = grid_area_for_item(
                          &ctx.offsets,
                          mapped_child_start,
                          mapped_child_end,
                        ) {
                          let area_start = area_start - ctx.node_offset;
                          let area_end = area_end - ctx.node_offset;
                          apply_translation(
                            area_start,
                            area_end,
                            span_start,
                            span_end,
                            child_id,
                            &mut children[idx],
                            deadline_counter,
                          )?;
                          translated = true;
                        }
                      }
                    }

                    if !translated {
                      let child = &mut children[idx];
                      let height = child.bounds.height();
                      let old_start = child.bounds.y();
                      let new_start = span_start + (span_end - (old_start + height));
                      translate_along_axis(
                        child,
                        Axis::Vertical,
                        new_start - old_start,
                        deadline_counter,
                      )?;
                    }
                  }
                  rtl_mirrored_y = true;
                }
              }
            }
          }
        }
      }
    }

    if mirror_x && !rtl_mirrored_x && !block_mirrored_x {
      if let Some((region_start, region_end)) = x_region {
        if region_end > region_start {
          for child in fragment.children_mut().iter_mut() {
            check_layout_deadline(deadline_counter)?;
            let child_end = child.bounds.x() + child.bounds.width();
            let new_start = region_start + region_end - child_end;
            let delta = new_start - child.bounds.x();
            translate_along_axis(child, Axis::Horizontal, delta, deadline_counter)?;
          }
        }
      }
    }

    if mirror_y && !rtl_mirrored_y {
      if let Some((region_start, region_end)) = y_region {
        if region_end > region_start {
          for child in fragment.children_mut().iter_mut() {
            check_layout_deadline(deadline_counter)?;
            let child_end = child.bounds.y() + child.bounds.height();
            let new_start = region_start + region_end - child_end;
            let delta = new_start - child.bounds.y();
            translate_along_axis(child, Axis::Vertical, delta, deadline_counter)?;
          }
        }
      }
    }

    Ok(())
  }
  /// Computes intrinsic size using Taffy
  fn compute_intrinsic_size(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    debug_assert!(
      matches!(
        box_node.formatting_context(),
        Some(FormattingContextType::Grid)
      ),
      "GridFormattingContext must only query grid containers",
    );
    if let Some(cached) = intrinsic_cache_lookup(box_node, mode) {
      return Ok(cached);
    }
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override.as_deref().unwrap_or(style);
    if style.containment.isolates_inline_size() {
      let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
      let edges = if inline_is_horizontal {
        self.horizontal_edges_px(style).unwrap_or(0.0)
      } else {
        self.vertical_edges_px(style).unwrap_or(0.0)
      };
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      let fallback = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        None,
        Some(0.0),
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
      let size = (edges + fallback).max(0.0);
      intrinsic_cache_store(box_node, mode, size);
      return Ok(size);
    }

    // Use a pooled Taffy tree to avoid repeated allocation churn during intrinsic sizing probes.
    let mut taffy = crate::layout::taffy_integration::PooledTaffyTree::new();
    let mut positioned_children: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> = FxHashMap::default();
    let intrinsic_constraints = LayoutConstraints::new(
      match mode {
        IntrinsicSizingMode::MinContent => CrateAvailableSpace::MinContent,
        IntrinsicSizingMode::MaxContent => CrateAvailableSpace::MaxContent,
      },
      CrateAvailableSpace::Indefinite,
    );

    let in_flow_children: Vec<&BoxNode> = box_node
      .children
      .iter()
      .filter(|child| {
        !matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      })
      .collect();

    // Resolve intrinsic sizing keywords for descendants while computing this grid container's
    // intrinsic inline sizes. Skip resolving the root itself to avoid recursion: resolving
    // `width: max-content` requires asking for the max-content size of the same box.
    let _intrinsic_keyword_overrides = self.resolve_intrinsic_sizing_keywords_for_taffy_tree(
      box_node,
      &in_flow_children,
      &intrinsic_constraints,
      false,
    )?;

    let root_id = self.build_taffy_tree_children(
      &mut taffy,
      box_node,
      style,
      &in_flow_children,
      &intrinsic_constraints,
      &mut positioned_children,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      let mut style_deadline_counter = 0usize;
      let simple_grid = self.is_simple_grid(
        style_override,
        &in_flow_children,
        &mut style_deadline_counter,
      )?;
      let override_taffy_style = self.convert_style(style_override, None, None, simple_grid, true);
      taffy
        .set_style(root_id, override_taffy_style)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    // Use appropriate available space for intrinsic sizing
    let available_space = match mode {
      IntrinsicSizingMode::MinContent => taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MinContent,
        height: taffy::style::AvailableSpace::MinContent,
      },
      IntrinsicSizingMode::MaxContent => taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MaxContent,
        height: taffy::style::AvailableSpace::MaxContent,
      },
    };

    record_taffy_invocation(TaffyAdapterKind::Grid);
    let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
    let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
    // Render pipeline always installs a deadline guard (even when disabled), so only enable
    // the Taffy cancellation path when the active deadline is actually configured.
    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
    let compute_result = taffy.compute_layout_with_measure_and_cancel(
      root_id,
      available_space,
      {
        let this = self.clone();
        let factory = self.factory.clone();
        let viewport_size = self.viewport_size;
        let mut cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
        move |known_dimensions,
              available_space,
              node_id,
              node_context,
              taffy_style: &taffy::style::Style| {
          if taffy_perf_enabled {
            record_taffy_measure_call(TaffyAdapterKind::Grid);
          }
          if node_id == root_id {
            return taffy::tree::MeasureOutput::ZERO;
          }
          let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
            known.unwrap_or(match avail_dim {
              taffy::style::AvailableSpace::Definite(v) => v,
              _ => 0.0,
            })
          };
          let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
            return taffy::tree::MeasureOutput::ZERO;
          };
          let box_node = unsafe { &*node_ptr };
          let style_override = style_override_for(box_node.id);
          let style: &ComputedStyle = style_override
            .as_deref()
            .unwrap_or_else(|| box_node.style.as_ref());
          let wants_baseline_y = taffy_style.align_self == Some(taffy::style::AlignItems::Baseline);
          let wants_baseline_x = taffy_style.justify_self
            == Some(taffy::style::AlignItems::Baseline)
            && crate::style::block_axis_is_horizontal(style.writing_mode);
          let mut available_space = available_space;
          if known_dimensions.width.is_none()
            && matches!(
              available_space.width,
              taffy::style::AvailableSpace::Definite(_)
            )
            && physical_width_is_auto(style)
          {
            available_space.width = taffy::style::AvailableSpace::MaxContent;
          }

          let fc_type = box_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let drop_available_height = matches!(
            available_space.height,
            taffy::style::AvailableSpace::Definite(_)
          ) && (known_dimensions.height.is_some()
            || matches!(
              fc_type,
              FormattingContextType::Block
                | FormattingContextType::Inline
                | FormattingContextType::Table
            ) && !node_or_in_flow_children_depend_on_available_height(box_node));
          let (key, known_dimensions, available_space) = MeasureKey::new_with_snapped_sizes(
            box_node,
            known_dimensions,
            available_space,
            viewport_size,
            drop_available_height,
          );
          if let Some(size) = cache.get(&key) {
            return *size;
          }
          if let Some(output) = grid_measure_size_cache_lookup(&key) {
            cache.insert(key, output);
            return output;
          }
          let fc = factory.get(fc_type);
          let inline_is_horizontal =
            crate::style::inline_axis_is_horizontal(box_node.style.writing_mode);
          let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_inline_size(box_node, mode)
            } else {
              fc.compute_intrinsic_block_size(box_node, mode)
            }
          };
          let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_block_size(box_node, mode)
            } else {
              fc.compute_intrinsic_inline_size(box_node, mode)
            }
          };

          let mut intrinsic_width: Option<f32> = None;
          if inline_is_horizontal && known_dimensions.width.is_none() {
            intrinsic_width = match available_space.width {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          let mut intrinsic_height: Option<f32> = None;
          if !inline_is_horizontal && known_dimensions.height.is_none() {
            intrinsic_height = match available_space.height {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          if !(wants_baseline_y || wants_baseline_x)
            && (intrinsic_width.is_some() || intrinsic_height.is_some())
          {
            let percentage_base = match available_space.width {
              taffy::style::AvailableSpace::Definite(w) => w,
              _ => 0.0,
            };
            let (
              padding_left,
              padding_right,
              padding_top,
              padding_bottom,
              border_left,
              border_right,
              border_top,
              border_bottom,
            ) = this.resolved_padding_border_for_measure(style, percentage_base);

            let width = intrinsic_width
              .map(|border_width| {
                (border_width - padding_left - padding_right - border_left - border_right).max(0.0)
              })
              .unwrap_or_else(|| {
                fallback_size(known_dimensions.width, available_space.width).max(0.0)
              });

            let height = intrinsic_height
              .map(|border_height| {
                (border_height - padding_top - padding_bottom - border_top - border_bottom).max(0.0)
              })
              .unwrap_or_else(|| {
                fallback_size(known_dimensions.height, available_space.height).max(0.0)
              });
            let size = taffy::geometry::Size { width, height };
            let output = taffy::tree::MeasureOutput::from_size(size);
            cache.insert(key, output);
            return output;
          }
          let constraints =
            constraints_from_taffy(viewport_size, known_dimensions, available_space, None);
          let fragment = match fc.layout(box_node, &constraints) {
            Ok(fragment) => fragment,
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => return taffy::tree::MeasureOutput::ZERO,
          };
          let percentage_base = match available_space.width {
            taffy::style::AvailableSpace::Definite(w) => w,
            _ => constraints
              .width()
              .unwrap_or_else(|| fragment.bounds.width()),
          };
          let content_size = this.content_box_size(&fragment, style, percentage_base);
          let size = taffy::geometry::Size {
            width: content_size.width.max(0.0),
            height: content_size.height.max(0.0),
          };
          let mut baseline_deadline_counter = 0usize;
          let baseline_y = if wants_baseline_y {
            match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let baseline_x = if wants_baseline_x {
            match first_baseline_offset_x(
              &fragment,
              style.writing_mode,
              &mut baseline_deadline_counter,
            ) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let output = taffy::tree::MeasureOutput::from_size_and_baselines(
            size,
            taffy::geometry::Point {
              x: baseline_x,
              y: baseline_y,
            },
          );
          grid_measure_size_cache_store(key, output);
          cache.insert(key, output);
          output
        }
      },
      cancel,
      TAFFY_ABORT_CHECK_STRIDE,
    );
    if let Some(start) = taffy_compute_start {
      record_taffy_compute(TaffyAdapterKind::Grid, start.elapsed());
    }
    compute_result.map_err(|e| match e {
      taffy::TaffyError::LayoutAborted => match active_deadline() {
        Some(deadline) => LayoutError::Timeout {
          elapsed: deadline.elapsed(),
        },
        None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
      },
      _ => LayoutError::MissingContext(format!("Taffy compute error: {:?}", e)),
    })?;

    let layout = taffy
      .layout(root_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let result = if inline_is_horizontal {
      layout.size.width
    } else {
      layout.size.height
    }
    .max(0.0);
    intrinsic_cache_store(box_node, mode, result);
    Ok(result)
  }

  #[allow(clippy::too_many_arguments)]
  fn measure_grid_item(
    &self,
    node_ptr: *const BoxNode,
    node_id: TaffyNodeId,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
    available_space: taffy::geometry::Size<taffy::style::AvailableSpace>,
    parent_inline_base: Option<f32>,
    taffy_style: &taffy::style::Style,
    auto_unskipped: &FxHashSet<*const BoxNode>,
    factory: &crate::layout::contexts::factory::FormattingContextFactory,
    measure_cache: &mut FxHashMap<MeasureKey, taffy::tree::MeasureOutput>,
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    measured_node_keys: &mut FxHashMap<TaffyNodeId, Vec<MeasureKey>>,
  ) -> taffy::tree::MeasureOutput {
    let box_node = unsafe { &*node_ptr };
    let style_override = style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    let skip_contents = match style.content_visibility {
      crate::style::types::ContentVisibility::Hidden => true,
      crate::style::types::ContentVisibility::Auto => {
        self.content_visibility_auto_has_definite_placeholder(box_node)
          && !auto_unskipped.contains(&node_ptr)
      }
      crate::style::types::ContentVisibility::Visible => false,
    };
    let fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let wants_baseline_y = taffy_style.align_self == Some(taffy::style::AlignItems::Baseline);
    let wants_baseline_x = taffy_style.justify_self == Some(taffy::style::AlignItems::Baseline)
      && crate::style::block_axis_is_horizontal(style.writing_mode);
    let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
      known.unwrap_or(match avail_dim {
        taffy::style::AvailableSpace::Definite(v) => v,
        _ => 0.0,
      })
    };

    let drop_available_height = matches!(
      available_space.height,
      taffy::style::AvailableSpace::Definite(_)
    ) && (known_dimensions.height.is_some()
      || matches!(
        fc_type,
        FormattingContextType::Block | FormattingContextType::Inline | FormattingContextType::Table
      ) && !node_or_in_flow_children_depend_on_available_height(box_node));
    let (key, known_dimensions, available_space) = MeasureKey::new_with_snapped_sizes(
      box_node,
      known_dimensions,
      available_space,
      self.viewport_size,
      drop_available_height,
    );

    if skip_contents {
      let constraints = constraints_from_taffy(
        self.viewport_size,
        known_dimensions,
        available_space,
        parent_inline_base,
      );
      let placeholder =
        self.content_visibility_placeholder_content_size(box_node, &constraints, known_dimensions);
      let size = taffy::geometry::Size {
        width: placeholder.width.max(0.0),
        height: placeholder.height.max(0.0),
      };
      let output = taffy::tree::MeasureOutput::from_size(size);
      measure_cache.insert(key, output);
      return output;
    }

    if let Some(output) = measure_cache.get(&key) {
      return *output;
    }
    if let Some(output) = grid_measure_size_cache_lookup(&key) {
      measure_cache.insert(key, output);
      return output;
    }
    let mut constraints = constraints_from_taffy(
      self.viewport_size,
      known_dimensions,
      available_space,
      parent_inline_base,
    );
    if constraints.used_border_box_width.is_none()
      && known_dimensions.width.is_none()
      && physical_width_is_auto(style)
      && taffy_style.justify_self == Some(taffy::style::AlignItems::Stretch)
    {
      if let taffy::style::AvailableSpace::Definite(w) = available_space.width {
        if w.is_finite() && w > 1.0 {
          constraints.used_border_box_width = Some(w.max(0.0));
        }
      }
    }
    if constraints.used_border_box_height.is_none()
      && known_dimensions.height.is_none()
      && physical_height_is_auto(style)
      && taffy_style.align_self == Some(taffy::style::AlignItems::Stretch)
    {
      if let taffy::style::AvailableSpace::Definite(h) = available_space.height {
        if h.is_finite() && h > 1.0 {
          constraints.used_border_box_height = Some(h.max(0.0));
        }
      }
    }

    // Taffy frequently probes min/max-content sizes during track sizing.
    // Avoid running full layout for these probes; use intrinsic sizing APIs instead.
    #[cfg(test)]
    if let Some(mut fragment) =
      GRID_TEST_MEASURE_HOOK.with(|hook| hook.borrow().as_ref().and_then(|hook| hook(box_node)))
    {
      let mut normalize_deadline_counter = 0usize;
      fragment = match normalize_fragment_origin(fragment, &mut normalize_deadline_counter) {
        Ok(fragment) => fragment,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => return taffy::tree::MeasureOutput::ZERO,
      };
      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => constraints
          .width()
          .unwrap_or_else(|| fragment.bounds.width()),
      };
      fragment.content = FragmentContent::Block {
        box_id: Some(box_node.id),
      };
      fragment.style = Some(box_node.style.clone());
      let content_size = self.content_box_size(&fragment, style, percentage_base);
      let size = taffy::geometry::Size {
        width: content_size.width.max(0.0),
        height: content_size.height.max(0.0),
      };
      let mut baseline_deadline_counter = 0usize;
      let baseline_y = if wants_baseline_y {
        match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
          Ok(baseline) => baseline,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };
      let baseline_x = if wants_baseline_x {
        match first_baseline_offset_x(&fragment, style.writing_mode, &mut baseline_deadline_counter)
        {
          Ok(baseline) => baseline,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };
      let output = taffy::tree::MeasureOutput::from_size_and_baselines(
        size,
        taffy::geometry::Point {
          x: baseline_x,
          y: baseline_y,
        },
      );
      grid_measure_size_cache_store(key, output);
      if let Some(evicted) = push_measured_key(measured_node_keys.entry(node_id).or_default(), key)
      {
        measured_fragments.remove(&evicted);
      }
      measured_fragments.insert(key, fragment);
      measure_cache.insert(key, output);
      return output;
    }

    let fc = factory.get(fc_type);

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(box_node.style.writing_mode);
    let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
        fc.as_ref(),
        box_node,
        PhysicalAxis::X,
        mode,
      )
    };
    let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
        fc.as_ref(),
        box_node,
        PhysicalAxis::Y,
        mode,
      )
    };

    // Fit-content depends on the available space passed by Taffy. Resolve it here (per measure call)
    // instead of during style conversion so cached templates remain valid.
    let fit_width_limit = match (known_dimensions.width, style.width_keyword) {
      (None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let fit_height_limit = match (known_dimensions.height, style.height_keyword) {
      (None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let fit_max_width_limit = match (
      known_dimensions.width,
      style.max_width,
      style.max_width_keyword,
    ) {
      (None, None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let fit_max_height_limit = match (
      known_dimensions.height,
      style.max_height,
      style.max_height_keyword,
    ) {
      (None, None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let resolve_fit_max_width = fit_max_width_limit.is_some() && !physical_width_is_auto(style);
    let resolve_fit_max_height = fit_max_height_limit.is_some() && !physical_height_is_auto(style);
    let mut fit_border_box_width: Option<f32> = None;
    let mut fit_border_box_height: Option<f32> = None;

    if fit_width_limit.is_some()
      || fit_height_limit.is_some()
      || resolve_fit_max_width
      || resolve_fit_max_height
    {
      fn run_with_override<T, F>(
        box_node: &BoxNode,
        override_style: Option<Arc<ComputedStyle>>,
        f: F,
      ) -> Result<T, LayoutError>
      where
        F: FnOnce(&BoxNode) -> Result<T, LayoutError>,
      {
        if let Some(style_override) = override_style {
          if box_node.id() != 0 {
            crate::layout::style_override::with_style_override(
              box_node.id(),
              style_override,
              || f(box_node),
            )
          } else {
            let mut cloned = box_node.clone();
            cloned.style = style_override;
            f(&cloned)
          }
        } else {
          f(box_node)
        }
      }

      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => parent_inline_base.unwrap_or(0.0),
      };
      let (
        padding_left,
        padding_right,
        padding_top,
        padding_bottom,
        border_left,
        border_right,
        border_top,
        border_bottom,
      ) = self.resolved_padding_border_for_measure(style, percentage_base);
      let fit_inset_w = padding_left + padding_right + border_left + border_right;
      let fit_inset_h = padding_top + padding_bottom + border_top + border_bottom;

      // Avoid self-recursion when computing intrinsic sizes for a fit-content axis by clearing the
      // corresponding preferred size property before calling into intrinsic APIs.
      let fit_width_override = (fit_width_limit.is_some() || resolve_fit_max_width).then(|| {
        let mut override_style: ComputedStyle = style.clone();
        override_style.width = None;
        override_style.width_keyword = None;
        if resolve_fit_max_width {
          override_style.max_width = None;
          override_style.max_width_keyword = None;
        }
        Arc::new(override_style)
      });
      let fit_height_override = (fit_height_limit.is_some() || resolve_fit_max_height).then(|| {
        let mut override_style: ComputedStyle = style.clone();
        override_style.height = None;
        override_style.height_keyword = None;
        if resolve_fit_max_height {
          override_style.max_height = None;
          override_style.max_height_keyword = None;
        }
        Arc::new(override_style)
      });

      let override_for_axis = |axis: Axis| match axis {
        Axis::Horizontal => fit_width_override.clone(),
        Axis::Vertical => fit_height_override.clone(),
      };

      let intrinsic_range_for_physical_axis = |axis: Axis| -> Result<(f32, f32), LayoutError> {
        let physical_axis = match axis {
          Axis::Horizontal => PhysicalAxis::X,
          Axis::Vertical => PhysicalAxis::Y,
        };
        run_with_override(box_node, override_for_axis(axis), |node| {
          crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
            fc.as_ref(),
            node,
            physical_axis,
          )
        })
      };

      let compute_fit_border_box = |axis: Axis,
                                    limit: Option<Length>,
                                    avail_dim: taffy::style::AvailableSpace|
       -> Result<f32, LayoutError> {
        let (min_intrinsic, max_intrinsic) = intrinsic_range_for_physical_axis(axis)?;
        let min_intrinsic = min_intrinsic.max(0.0);
        let max_intrinsic = max_intrinsic.max(0.0);
        let axis_inset = match axis {
          Axis::Horizontal => fit_inset_w,
          Axis::Vertical => fit_inset_h,
        };

        let available_border_box = match avail_dim {
          taffy::style::AvailableSpace::Definite(v) => (v + axis_inset).max(0.0),
          taffy::style::AvailableSpace::MinContent => min_intrinsic,
          taffy::style::AvailableSpace::MaxContent => max_intrinsic,
        };

        let preferred_border_box = match limit {
          None => None,
          Some(arg) => {
            let base_content = match avail_dim {
              taffy::style::AvailableSpace::Definite(v) => v.max(0.0),
              _ => (available_border_box - axis_inset).max(0.0),
            };
            let resolved = self
              .resolve_length_for_width(arg, base_content, style)
              .max(0.0);
            Some(if style.box_sizing == BoxSizing::ContentBox {
              (resolved + axis_inset).max(0.0)
            } else {
              resolved
            })
          }
        };
        let mut border_box =
          crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
            Some(available_border_box),
            preferred_border_box,
            min_intrinsic,
            max_intrinsic,
          );

        // Apply authored min/max constraints on the axis, including intrinsic keyword constraints.
        // These clamp the fit-content result so the subtree is laid out at the same size Taffy will
        // ultimately use.
        let (author_min_len, author_max_len, author_min_kw, author_max_kw) = match axis {
          Axis::Horizontal => (
            style.min_width,
            style.max_width,
            style.min_width_keyword,
            style.max_width_keyword,
          ),
          Axis::Vertical => (
            style.min_height,
            style.max_height,
            style.min_height_keyword,
            style.max_height_keyword,
          ),
        };
        let keyword_to_bound = |kw: IntrinsicSizeKeyword| -> Option<f32> {
          match kw {
            IntrinsicSizeKeyword::MinContent => Some(min_intrinsic),
            IntrinsicSizeKeyword::MaxContent => Some(max_intrinsic),
            IntrinsicSizeKeyword::FillAvailable => None,
            IntrinsicSizeKeyword::FitContent { .. } => None,
          }
        };
        let percentage_base_opt = match avail_dim {
          taffy::style::AvailableSpace::Definite(v) => Some(v.max(0.0)),
          _ => None,
        };
        let resolve_length_px = |len: Length| -> Option<f32> {
          if len.has_percentage() && percentage_base_opt.is_none() {
            return None;
          }
          let base = percentage_base_opt.unwrap_or(self.viewport_size.width.max(0.0));
          Some(self.resolve_length_for_width(len, base, style).max(0.0))
        };
        let to_border_box = |value: f32| -> f32 {
          if style.box_sizing == BoxSizing::ContentBox {
            (value + axis_inset).max(0.0)
          } else {
            value.max(0.0)
          }
        };

        let author_min = author_min_kw.and_then(keyword_to_bound).or_else(|| {
          author_min_len
            .and_then(resolve_length_px)
            .map(to_border_box)
        });
        let author_max = author_max_kw.and_then(keyword_to_bound).or_else(|| {
          author_max_len
            .and_then(resolve_length_px)
            .map(to_border_box)
        });
        if author_min.is_some() || author_max.is_some() {
          let min_bound = author_min.unwrap_or(0.0);
          let mut max_bound = author_max.unwrap_or(f32::INFINITY);
          if max_bound < min_bound {
            max_bound = min_bound;
          }
          border_box = crate::layout::utils::clamp_with_order(border_box, min_bound, max_bound);
        }

        Ok(border_box)
      };

      if let Some(limit) = fit_width_limit {
        match compute_fit_border_box(Axis::Horizontal, limit, available_space.width) {
          Ok(border_box) if border_box.is_finite() => {
            let border_box = border_box.max(0.0);
            fit_border_box_width = Some(border_box);
            constraints.used_border_box_width = Some(border_box);
          }
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => {}
          _ => {}
        }
      }

      if let Some(limit) = fit_height_limit {
        match compute_fit_border_box(Axis::Vertical, limit, available_space.height) {
          Ok(border_box) if border_box.is_finite() => {
            let border_box = border_box.max(0.0);
            fit_border_box_height = Some(border_box);
            constraints.used_border_box_height = Some(border_box);
          }
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => {}
          _ => {}
        }
      }

      if resolve_fit_max_width {
        if let Some(limit) = fit_max_width_limit {
          match compute_fit_border_box(Axis::Horizontal, limit, available_space.width) {
            Ok(max_border_box) if max_border_box.is_finite() => {
              let max_border_box = max_border_box.max(0.0);
              if let Some(existing) = constraints.used_border_box_width {
                constraints.used_border_box_width = Some(existing.min(max_border_box));
              } else if let Some(width) = style.width {
                let base = constraints
                  .inline_percentage_base
                  .unwrap_or(percentage_base)
                  .max(0.0);
                let resolved = self.resolve_length_for_width(width, base, style).max(0.0);
                let border_box = if style.box_sizing == BoxSizing::ContentBox {
                  (resolved + fit_inset_w).max(0.0)
                } else {
                  resolved.max(0.0)
                };
                constraints.used_border_box_width = Some(border_box.min(max_border_box));
              } else if let Some(keyword) = style.width_keyword {
                match keyword {
                  IntrinsicSizeKeyword::MinContent | IntrinsicSizeKeyword::MaxContent => {
                    let use_min = matches!(keyword, IntrinsicSizeKeyword::MinContent);
                    match intrinsic_range_for_physical_axis(Axis::Horizontal) {
                      Ok((min_intrinsic, max_intrinsic)) => {
                        let preferred = if use_min {
                          min_intrinsic.max(0.0)
                        } else {
                          max_intrinsic.max(0.0)
                        };
                        constraints.used_border_box_width = Some(preferred.min(max_border_box));
                      }
                      Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                      Err(_) => {}
                    }
                  }
                  IntrinsicSizeKeyword::FillAvailable => {
                    let base = constraints
                      .inline_percentage_base
                      .unwrap_or(percentage_base)
                      .max(0.0);
                    let border_box = if style.box_sizing == BoxSizing::ContentBox {
                      (base + fit_inset_w).max(0.0)
                    } else {
                      base.max(0.0)
                    };
                    constraints.used_border_box_width = Some(border_box.min(max_border_box));
                  }
                  IntrinsicSizeKeyword::FitContent { .. } => {}
                }
              }
            }
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => {}
            _ => {}
          }
        }
      }

      if resolve_fit_max_height {
        if let Some(limit) = fit_max_height_limit {
          match compute_fit_border_box(Axis::Vertical, limit, available_space.height) {
            Ok(max_border_box) if max_border_box.is_finite() => {
              let max_border_box = max_border_box.max(0.0);
              if let Some(existing) = constraints.used_border_box_height {
                constraints.used_border_box_height = Some(existing.min(max_border_box));
              } else if let Some(height) = style.height {
                let base = match available_space.height {
                  taffy::style::AvailableSpace::Definite(v) => v,
                  _ => 0.0,
                }
                .max(0.0);
                let resolved = self.resolve_length_for_width(height, base, style).max(0.0);
                let border_box = if style.box_sizing == BoxSizing::ContentBox {
                  (resolved + fit_inset_h).max(0.0)
                } else {
                  resolved.max(0.0)
                };
                constraints.used_border_box_height = Some(border_box.min(max_border_box));
              } else if let Some(keyword) = style.height_keyword {
                match keyword {
                  IntrinsicSizeKeyword::MinContent | IntrinsicSizeKeyword::MaxContent => {
                    let use_min = matches!(keyword, IntrinsicSizeKeyword::MinContent);
                    match intrinsic_range_for_physical_axis(Axis::Vertical) {
                      Ok((min_intrinsic, max_intrinsic)) => {
                        let preferred = if use_min {
                          min_intrinsic.max(0.0)
                        } else {
                          max_intrinsic.max(0.0)
                        };
                        constraints.used_border_box_height = Some(preferred.min(max_border_box));
                      }
                      Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                      Err(_) => {}
                    }
                  }
                  IntrinsicSizeKeyword::FillAvailable => {
                    let base = match available_space.height {
                      taffy::style::AvailableSpace::Definite(v) => v,
                      _ => 0.0,
                    }
                    .max(0.0);
                    let border_box = if style.box_sizing == BoxSizing::ContentBox {
                      (base + fit_inset_h).max(0.0)
                    } else {
                      base.max(0.0)
                    };
                    constraints.used_border_box_height = Some(border_box.min(max_border_box));
                  }
                  IntrinsicSizeKeyword::FitContent { .. } => {}
                }
              }
            }
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => {}
            _ => {}
          }
        }
      }
    }

    let mut intrinsic_width: Option<f32> = None;
    if inline_is_horizontal && known_dimensions.width.is_none() {
      intrinsic_width = match available_space.width {
        taffy::style::AvailableSpace::MinContent => fit_border_box_width.or_else(|| {
          Some(
            match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
              Ok(size) => size,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => 0.0,
            },
          )
        }),
        taffy::style::AvailableSpace::MaxContent => fit_border_box_width.or_else(|| {
          Some(
            match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
              Ok(size) => size,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => 0.0,
            },
          )
        }),
        _ => None,
      };
    }

    let mut intrinsic_height: Option<f32> = None;
    if !inline_is_horizontal && known_dimensions.height.is_none() {
      intrinsic_height = match available_space.height {
        taffy::style::AvailableSpace::MinContent => fit_border_box_height.or_else(|| {
          Some(
            match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
              Ok(size) => size,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => 0.0,
            },
          )
        }),
        taffy::style::AvailableSpace::MaxContent => fit_border_box_height.or_else(|| {
          Some(
            match intrinsic_physical_height(IntrinsicSizingMode::MaxContent) {
              Ok(size) => size,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => 0.0,
            },
          )
        }),
        _ => None,
      };
    }

    if !(wants_baseline_y || wants_baseline_x)
      && (intrinsic_width.is_some() || intrinsic_height.is_some())
    {
      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => parent_inline_base.unwrap_or(0.0),
      };
      let (
        padding_left,
        padding_right,
        padding_top,
        padding_bottom,
        border_left,
        border_right,
        border_top,
        border_bottom,
      ) = self.resolved_padding_border_for_measure(style, percentage_base);

      let width = intrinsic_width
        .map(|border_width| {
          (border_width - padding_left - padding_right - border_left - border_right).max(0.0)
        })
        .unwrap_or_else(|| fallback_size(known_dimensions.width, available_space.width).max(0.0));

      let height = if let Some(border_height) = intrinsic_height {
        (border_height - padding_top - padding_bottom - border_top - border_bottom).max(0.0)
      } else {
        let normalize_known =
          |known: Option<f32>, avail: taffy::style::AvailableSpace| match (known, avail) {
            (Some(value), taffy::style::AvailableSpace::Definite(avail))
              if value <= 1.0 && avail <= 1.0 =>
            {
              None
            }
            _ => known,
          };
        let known_height = normalize_known(known_dimensions.height, available_space.height);
        match (known_height, available_space.height) {
          (Some(h), _) => h.max(0.0),
          (_, taffy::style::AvailableSpace::Definite(h)) if h > 1.0 => h.max(0.0),
          _ => {
            let mode = match available_space.width {
              taffy::style::AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
              taffy::style::AvailableSpace::MaxContent => IntrinsicSizingMode::MaxContent,
              _ => IntrinsicSizingMode::MaxContent,
            };
            let border_block_size = match fc.compute_intrinsic_block_size(box_node, mode) {
              Ok(size) => size,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => 0.0,
            };
            (border_block_size - padding_top - padding_bottom - border_top - border_bottom).max(0.0)
          }
        }
      };

      let size = taffy::geometry::Size { width, height };
      let output = taffy::tree::MeasureOutput::from_size(size);
      grid_measure_size_cache_store(key, output);
      measure_cache.insert(key, output);
      return output;
    }

    if known_dimensions.width.is_none()
      && matches!(
        available_space.width,
        taffy::style::AvailableSpace::Definite(_)
      )
      && physical_width_is_auto(style)
    {
      if let taffy::style::AvailableSpace::Definite(area_width) = available_space.width {
        let justify = taffy_style
          .justify_self
          .unwrap_or(taffy::style::AlignItems::Stretch);
        if justify != taffy::style::AlignItems::Stretch {
          match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
            Ok(intrinsic_width) => {
              let used_width = intrinsic_width.max(0.0).min(area_width.max(0.0));
              constraints.available_width = CrateAvailableSpace::Definite(used_width);
              constraints.inline_percentage_base = Some(area_width);
            }
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => {}
          }
        }
      }
    }

    record_measure_layout_call();
    let mut fragment = match fc.layout(box_node, &constraints) {
      Ok(fragment) => fragment,
      Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
      Err(_) => return taffy::tree::MeasureOutput::ZERO,
    };
    let percentage_base = match available_space.width {
      taffy::style::AvailableSpace::Definite(w) => w,
      _ => constraints
        .width()
        .unwrap_or_else(|| fragment.bounds.width()),
    };
    fragment.content = FragmentContent::Block {
      box_id: Some(box_node.id),
    };
    fragment.style = Some(box_node.style.clone());
    let content_size = self.content_box_size(&fragment, style, percentage_base);
    let size = taffy::geometry::Size {
      width: content_size.width.max(0.0),
      height: content_size.height.max(0.0),
    };
    let mut baseline_deadline_counter = 0usize;
    let baseline_y = if wants_baseline_y {
      match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
        Ok(baseline) => baseline,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => None,
      }
    } else {
      None
    };
    let baseline_x = if wants_baseline_x {
      match first_baseline_offset_x(&fragment, style.writing_mode, &mut baseline_deadline_counter) {
        Ok(baseline) => baseline,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => None,
      }
    } else {
      None
    };
    let output = taffy::tree::MeasureOutput::from_size_and_baselines(
      size,
      taffy::geometry::Point {
        x: baseline_x,
        y: baseline_y,
      },
    );
    grid_measure_size_cache_store(key, output);
    if let Some(evicted) = push_measured_key(measured_node_keys.entry(node_id).or_default(), key) {
      measured_fragments.remove(&evicted);
    }
    measured_fragments.insert(key, fragment);
    measure_cache.insert(key, output);
    output
  }
}

fn grid_child_fingerprint(
  children: &[&BoxNode],
  deadline_counter: &mut usize,
) -> Result<u64, LayoutError> {
  use std::hash::Hash;
  use std::hash::Hasher;
  let mut h = FingerprintHasher::default();
  children.len().hash(&mut h);
  for child in children {
    check_layout_deadline(deadline_counter)?;
    // The cached Taffy template stores converted styles only; box type/formatting context affects
    // measurement/layout callbacks, not the style conversion output. Hash only style fingerprints so
    // we maximize template reuse across repeated component trees.
    let child_style_override = style_override_for(child.id);
    let child_style: &ComputedStyle = child_style_override
      .as_deref()
      .unwrap_or_else(|| child.style.as_ref());
    taffy_grid_item_style_fingerprint(child_style).hash(&mut h);
  }
  Ok(h.finish())
}

fn translate_fragment_tree(
  fragment: &mut FragmentNode,
  delta: Point,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  check_layout_deadline(deadline_counter)?;
  fragment.bounds = Rect::new(
    Point::new(fragment.bounds.x() + delta.x, fragment.bounds.y() + delta.y),
    fragment.bounds.size,
  );
  if let Some(logical) = fragment.logical_override {
    fragment.logical_override = Some(Rect::new(
      Point::new(logical.x() + delta.x, logical.y() + delta.y),
      logical.size,
    ));
  }
  Ok(())
}

fn normalize_fragment_origin(
  mut fragment: FragmentNode,
  deadline_counter: &mut usize,
) -> Result<FragmentNode, LayoutError> {
  let origin = fragment.bounds.origin;
  if origin.x != 0.0 || origin.y != 0.0 {
    translate_fragment_tree(
      &mut fragment,
      Point::new(-origin.x, -origin.y),
      deadline_counter,
    )?;
  }
  Ok(fragment)
}

#[cfg(test)]
thread_local! {
  static GRID_TEST_MEASURE_HOOK: RefCell<Option<Box<dyn Fn(&BoxNode) -> Option<FragmentNode>>>> =
    RefCell::new(None);
}

#[cfg(test)]
struct GridTestMeasureHookGuard;

#[cfg(test)]
fn set_grid_test_measure_hook(
  hook: impl Fn(&BoxNode) -> Option<FragmentNode> + 'static,
) -> GridTestMeasureHookGuard {
  GRID_TEST_MEASURE_HOOK.with(|slot| {
    *slot.borrow_mut() = Some(Box::new(hook));
  });
  GridTestMeasureHookGuard
}

#[cfg(test)]
impl Drop for GridTestMeasureHookGuard {
  fn drop(&mut self) {
    GRID_TEST_MEASURE_HOOK.with(|slot| {
      slot.borrow_mut().take();
    });
  }
}

fn find_first_baseline_offset(
  fragment: &FragmentNode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  check_layout_deadline(deadline_counter)?;
  if let Some(baseline) = fragment.baseline {
    return Ok(Some(baseline));
  }
  match &fragment.content {
    FragmentContent::Line { baseline } => return Ok(Some(*baseline)),
    FragmentContent::Text {
      baseline_offset, ..
    } => return Ok(Some(*baseline_offset)),
    _ => {}
  }

  for child in fragment.children.iter() {
    if let Some(b) = find_first_baseline_offset(child, deadline_counter)? {
      return Ok(Some(child.bounds.y() + b));
    }
  }

  Ok(None)
}

fn find_first_baseline_offset_x(
  fragment: &FragmentNode,
  block_positive: bool,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  check_layout_deadline(deadline_counter)?;

  let resolve_from_block_start = |offset: f32, extent: f32| -> f32 {
    if block_positive {
      offset
    } else if extent.is_finite() && extent > 0.0 {
      (extent - offset).max(0.0)
    } else {
      offset
    }
  };

  let extent = fragment.bounds.width();
  if let Some(baseline) = fragment.baseline {
    return Ok(Some(resolve_from_block_start(baseline, extent)));
  }
  match &fragment.content {
    FragmentContent::Line { baseline } => {
      return Ok(Some(resolve_from_block_start(*baseline, extent)));
    }
    FragmentContent::Text {
      baseline_offset, ..
    } => {
      return Ok(Some(resolve_from_block_start(*baseline_offset, extent)));
    }
    _ => {}
  }

  for child in fragment.children.iter() {
    if let Some(b) = find_first_baseline_offset_x(child, block_positive, deadline_counter)? {
      return Ok(Some(child.bounds.x() + b));
    }
  }

  Ok(None)
}

fn first_baseline_offset(
  fragment: &FragmentNode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  find_first_baseline_offset(fragment, deadline_counter)
}

fn first_baseline_offset_x(
  fragment: &FragmentNode,
  writing_mode: WritingMode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  if !crate::style::block_axis_is_horizontal(writing_mode) {
    return Ok(None);
  }
  find_first_baseline_offset_x(
    fragment,
    crate::style::block_axis_positive(writing_mode),
    deadline_counter,
  )
}

fn apply_alignment_fallback_for_grid(
  free_space: f32,
  num_items: usize,
  alignment_mode: TaffyAlignContent,
) -> TaffyAlignContent {
  // Mirrors taffy's alignment fallback, but scoped locally for grid post-processing.
  if num_items <= 1 || free_space <= 0.0 {
    return match alignment_mode {
      TaffyAlignContent::Stretch => TaffyAlignContent::FlexStart,
      TaffyAlignContent::SpaceBetween => TaffyAlignContent::FlexStart,
      TaffyAlignContent::SpaceAround => TaffyAlignContent::Center,
      TaffyAlignContent::SpaceEvenly => TaffyAlignContent::Center,
      other => other,
    };
  }

  alignment_mode
}

fn compute_alignment_offset_for_grid(
  free_space: f32,
  num_items: usize,
  alignment_mode: TaffyAlignContent,
  is_first: bool,
) -> f32 {
  if is_first {
    match alignment_mode {
      TaffyAlignContent::Start => 0.0,
      TaffyAlignContent::FlexStart => 0.0,
      TaffyAlignContent::End => free_space,
      TaffyAlignContent::FlexEnd => free_space,
      TaffyAlignContent::Center => free_space / 2.0,
      TaffyAlignContent::Stretch => 0.0,
      TaffyAlignContent::SpaceBetween => 0.0,
      TaffyAlignContent::SpaceAround => {
        if free_space >= 0.0 {
          (free_space / num_items as f32) / 2.0
        } else {
          free_space / 2.0
        }
      }
      TaffyAlignContent::SpaceEvenly => {
        if free_space >= 0.0 {
          free_space / (num_items + 1) as f32
        } else {
          free_space / 2.0
        }
      }
    }
  } else {
    let free_space = free_space.max(0.0);
    match alignment_mode {
      TaffyAlignContent::Start
      | TaffyAlignContent::FlexStart
      | TaffyAlignContent::End
      | TaffyAlignContent::FlexEnd
      | TaffyAlignContent::Center
      | TaffyAlignContent::Stretch => 0.0,
      TaffyAlignContent::SpaceBetween => free_space / (num_items.saturating_sub(1).max(1) as f32),
      TaffyAlignContent::SpaceAround => free_space / (num_items.max(1) as f32),
      TaffyAlignContent::SpaceEvenly => free_space / (num_items + 1) as f32,
    }
  }
}

fn grid_track_ranges_for_container(
  taffy: &TaffyTree<*const BoxNode>,
  node_id: TaffyNodeId,
  layout: &TaffyLayout,
  container_style: &taffy::style::Style,
  axis_style: GridAxisStyle,
  apply_axis_mirroring: bool,
) -> Option<GridTrackRanges> {
  let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) else {
    return None;
  };

  if info.rows.sizes.is_empty() && info.columns.sizes.is_empty() {
    return None;
  }

  let row_offsets = compute_track_offsets(
    &info.rows,
    layout.size.height,
    layout.padding.top,
    layout.padding.bottom,
    layout.border.top,
    layout.border.bottom,
    container_style
      .align_content
      .unwrap_or(TaffyAlignContent::Stretch),
  );
  let col_offsets = compute_track_offsets(
    &info.columns,
    layout.size.width,
    layout.padding.left,
    layout.padding.right,
    layout.border.left,
    layout.border.right,
    container_style
      .justify_content
      .unwrap_or(TaffyAlignContent::Stretch),
  );

  let mut rows = Vec::with_capacity(info.rows.sizes.len());
  for (idx, size) in info.rows.sizes.iter().copied().enumerate() {
    let start_idx = 1usize + 2usize * idx;
    let Some(&start) = row_offsets.get(start_idx) else {
      break;
    };
    let end = start + size;
    if start.is_finite() && end.is_finite() && end > start {
      rows.push((start, end));
    }
  }

  let mut columns = Vec::with_capacity(info.columns.sizes.len());
  for (idx, size) in info.columns.sizes.iter().copied().enumerate() {
    let start_idx = 1usize + 2usize * idx;
    let Some(&start) = col_offsets.get(start_idx) else {
      break;
    };
    let end = start + size;
    if start.is_finite() && end.is_finite() && end > start {
      columns.push((start, end));
    }
  }

  if apply_axis_mirroring && (!rows.is_empty() || !columns.is_empty()) {
    let inline_is_horizontal = axis_style.inline_is_horizontal();
    let mut mirror_x = false;
    let mut mirror_y = false;

    if !axis_style.inline_positive() {
      if inline_is_horizontal {
        mirror_x = true;
      } else {
        mirror_y = true;
      }
    }
    if !axis_style.block_positive() {
      if inline_is_horizontal {
        mirror_y = true;
      } else {
        mirror_x = true;
      }
    }

    if mirror_y && !rows.is_empty() {
      let track_count = info.rows.sizes.len();
      if track_count > 0 && row_offsets.len() >= 2 {
        let span_start = row_offsets[1];
        let span_end = row_offsets
          .get(track_count * 2)
          .copied()
          .or_else(|| row_offsets.last().copied())
          .unwrap_or(span_start);
        if span_end > span_start {
          for range in rows.iter_mut() {
            let (start, end) = *range;
            let new_start = span_start + (span_end - end);
            let new_end = span_start + (span_end - start);
            *range = (new_start, new_end);
          }
        }
      }
    }

    if mirror_x && !columns.is_empty() {
      let track_count = info.columns.sizes.len();
      if track_count > 0 && col_offsets.len() >= 2 {
        let span_start = col_offsets[1];
        let span_end = col_offsets
          .get(track_count * 2)
          .copied()
          .or_else(|| col_offsets.last().copied())
          .unwrap_or(span_start);
        if span_end > span_start {
          for range in columns.iter_mut() {
            let (start, end) = *range;
            let new_start = span_start + (span_end - end);
            let new_end = span_start + (span_end - start);
            *range = (new_start, new_end);
          }
        }
      }
    }
  }

  if rows.is_empty() && columns.is_empty() {
    None
  } else {
    Some(GridTrackRanges { rows, columns })
  }
}

fn compute_track_offsets(
  tracks: &DetailedGridTracksInfo,
  axis_size: f32,
  padding_start: f32,
  padding_end: f32,
  border_start: f32,
  border_end: f32,
  alignment: TaffyAlignContent,
) -> Vec<f32> {
  let track_count = tracks.sizes.len();
  if track_count == 0 {
    return Vec::new();
  }

  let mut gutters = tracks.gutters.clone();
  if gutters.len() < track_count + 1 {
    gutters.resize(track_count + 1, 0.0);
  }

  #[derive(Clone, Copy)]
  struct TrackEntry {
    size: f32,
    is_gutter: bool,
  }

  let mut entries: Vec<TrackEntry> = Vec::with_capacity(track_count * 2 + 1);
  entries.push(TrackEntry {
    size: gutters.get(0).copied().unwrap_or(0.0),
    is_gutter: true,
  });
  for i in 0..track_count {
    entries.push(TrackEntry {
      size: tracks.sizes[i],
      is_gutter: false,
    });
    entries.push(TrackEntry {
      size: gutters.get(i + 1).copied().unwrap_or(0.0),
      is_gutter: true,
    });
  }

  let used_size: f32 = entries.iter().map(|t| t.size).sum();
  let content_size = axis_size - padding_start - padding_end - border_start - border_end;
  let free_space = content_size - used_size;
  let aligned = apply_alignment_fallback_for_grid(free_space, track_count, alignment);

  let mut offsets = Vec::with_capacity(entries.len());
  let mut total_offset = padding_start + border_start;
  let mut first_track_seen = false;
  for entry in entries {
    let is_first_track = !first_track_seen && !entry.is_gutter;
    if is_first_track {
      first_track_seen = true;
    }
    let offset_within = if entry.is_gutter {
      0.0
    } else {
      compute_alignment_offset_for_grid(free_space, track_count.max(1), aligned, is_first_track)
    };
    offsets.push(total_offset + offset_within);
    total_offset += offset_within + entry.size;
  }

  offsets
}

fn grid_area_for_item(offsets: &[f32], start_line: u16, end_line: u16) -> Option<(f32, f32)> {
  let start_idx = (start_line.saturating_sub(1) as usize).saturating_mul(2);
  let end_idx = (end_line.saturating_sub(1) as usize).saturating_mul(2);
  if start_idx + 1 >= offsets.len() || end_idx >= offsets.len() {
    return None;
  }
  Some((offsets[start_idx + 1], offsets[end_idx]))
}

#[derive(Clone, Copy, Debug)]
enum ResolvedGridPlacementComponent {
  Auto,
  Line(i32),
  Span(u16),
  Unsupported,
}

fn grid_placement_component_from_taffy(
  placement: &TaffyGridPlacement<String>,
) -> ResolvedGridPlacementComponent {
  match placement {
    TaffyGridPlacement::Auto => ResolvedGridPlacementComponent::Auto,
    TaffyGridPlacement::Line(line) => ResolvedGridPlacementComponent::Line(line.as_i16() as i32),
    TaffyGridPlacement::Span(span) => ResolvedGridPlacementComponent::Span(*span),
    // Named line resolution is handled by Taffy for in-flow items. For static positioning of
    // out-of-flow abspos/fixed items, fall back to the axis default (0) when named resolution
    // would be required.
    TaffyGridPlacement::NamedLine(..) | TaffyGridPlacement::NamedSpan(..) => {
      ResolvedGridPlacementComponent::Unsupported
    }
  }
}

fn grid_line_count_from_offsets(offsets: &[f32]) -> Option<u16> {
  // Offsets is (track_count * 2 + 1) entries: [gutter, track, gutter, track, ... gutter].
  if offsets.len() < 3 || offsets.len() % 2 == 0 {
    return None;
  }
  let track_count = (offsets.len() - 1) / 2;
  u16::try_from(track_count.saturating_add(1)).ok()
}

fn resolve_css_grid_line_to_u16(line: i32, line_count: Option<u16>) -> Option<u16> {
  match line.cmp(&0) {
    std::cmp::Ordering::Equal => None,
    std::cmp::Ordering::Greater => u16::try_from(line).ok(),
    std::cmp::Ordering::Less => {
      let line_count = line_count? as i32;
      let resolved = line_count + line + 1;
      if resolved >= 1 && resolved <= line_count {
        Some(resolved as u16)
      } else {
        None
      }
    }
  }
}

fn resolve_grid_line_range_from_style(
  start: i32,
  end: i32,
  raw: Option<&str>,
  line_count: Option<u16>,
) -> Option<(u16, u16)> {
  let parsed = raw
    .filter(|_| start == 0 || end == 0)
    .map(parse_grid_line_placement_raw);

  let start_component = if start != 0 {
    ResolvedGridPlacementComponent::Line(start)
  } else {
    parsed
      .as_ref()
      .map(|line| grid_placement_component_from_taffy(&line.start))
      .unwrap_or(ResolvedGridPlacementComponent::Auto)
  };
  let end_component = if end != 0 {
    ResolvedGridPlacementComponent::Line(end)
  } else {
    parsed
      .as_ref()
      .map(|line| grid_placement_component_from_taffy(&line.end))
      .unwrap_or(ResolvedGridPlacementComponent::Auto)
  };

  use ResolvedGridPlacementComponent as Comp;
  match (start_component, end_component) {
    (Comp::Line(start), Comp::Line(end)) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      (end > start).then_some((start, end))
    }
    (Comp::Line(start), Comp::Span(span)) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = start.checked_add(span)?;
      (end > start).then_some((start, end))
    }
    (Comp::Span(span), Comp::Line(end)) => {
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      let start = end.checked_sub(span)?;
      (end > start).then_some((start, end))
    }
    (Comp::Line(start), Comp::Auto) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = start.checked_add(1)?;
      (end > start).then_some((start, end))
    }
    (Comp::Auto, Comp::Line(end)) => {
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      let start = end.checked_sub(1)?;
      (end > start).then_some((start, end))
    }
    _ => None,
  }
}

#[derive(Clone, Copy)]
struct BaselineItem {
  idx: usize,
  area_start: f32,
  area_end: f32,
  baseline: f32,
  start: f32,
  size: f32,
}

fn translate_along_axis(
  fragment: &mut FragmentNode,
  axis: Axis,
  delta: f32,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  if delta == 0.0 {
    return Ok(());
  }
  let delta_point = match axis {
    Axis::Horizontal => Point::new(delta, 0.0),
    Axis::Vertical => Point::new(0.0, delta),
  };
  translate_fragment_tree(fragment, delta_point, deadline_counter)
}

fn is_css_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_css_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_css_ascii_whitespace)
}

fn split_css_ascii_whitespace(value: &str) -> impl Iterator<Item = &str> {
  value
    .split(is_css_ascii_whitespace)
    .filter(|part| !part.is_empty())
}

fn parse_grid_line_placement_raw(raw: &str) -> Line<TaffyGridPlacement<String>> {
  let mut parts = raw.splitn(2, '/').map(trim_css_ascii_whitespace);
  let start_str = parts.next().unwrap_or("auto");
  let end_str = parts.next().unwrap_or("auto");
  Line {
    start: parse_grid_line_component(start_str),
    end: parse_grid_line_component(end_str),
  }
}

fn parse_grid_line_component(token: &str) -> TaffyGridPlacement<String> {
  let trimmed = trim_css_ascii_whitespace(token);
  if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
    return TaffyGridPlacement::Auto;
  }

  let parts: Vec<&str> = split_css_ascii_whitespace(trimmed).collect();
  if parts.is_empty() {
    return TaffyGridPlacement::Auto;
  }

  // Span syntax: span && (<integer> || <custom-ident>) in any order
  //
  // The `span` keyword can appear anywhere within the component, not just as the first token.
  // See: https://www.w3.org/TR/css-grid-2/#typedef-grid-line
  if parts.iter().any(|part| part.eq_ignore_ascii_case("span")) {
    let mut name: Option<String> = None;
    let mut count: Option<u16> = None;

    for part in parts.iter().filter(|part| !part.eq_ignore_ascii_case("span")) {
      if let Ok(n) = part.parse::<i32>() {
        if n > 0 && count.is_none() {
          count = Some(n as u16);
        }
        continue;
      }

      if name.is_none() {
        name = Some((*part).to_string());
      }
    }

    let count = count.unwrap_or(1).max(1);
    return match name {
      Some(name) => TaffyGridPlacement::NamedSpan(name, count),
      None => TaffyGridPlacement::Span(count),
    };
  }

  // Non-span grammar: <custom-ident>? <integer>? in any order (integer controls the nth occurrence)
  let mut number: Option<i16> = None;
  let mut name: Option<String> = None;
  for part in &parts {
    if number.is_none() {
      if let Ok(n) = part.parse::<i16>() {
        number = Some(n);
        continue;
      }
    }
    if name.is_none() {
      name = Some((*part).to_string());
    }
  }

  match (name, number) {
    (Some(name), Some(idx)) => TaffyGridPlacement::NamedLine(name, idx),
    (Some(name), None) => TaffyGridPlacement::NamedLine(name, 1),
    (None, Some(idx)) => TaffyGridPlacement::Line(idx.into()),
    (None, None) => TaffyGridPlacement::Auto,
  }
}

impl Default for GridFormattingContext {
  fn default() -> Self {
    Self::new()
  }
}

impl FormattingContext for GridFormattingContext {
  fn layout(
    &self,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
  ) -> Result<FragmentNode, LayoutError> {
    debug_assert!(
      matches!(
        box_node.formatting_context(),
        Some(FormattingContextType::Grid)
      ),
      "GridFormattingContext must only layout grid containers",
    );
    let _profile = layout_timer(LayoutKind::Grid);
    if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
      return Err(LayoutError::Timeout { elapsed });
    }
    let mut has_running_children = false;
    let mut has_running_deadline_counter = 0usize;
    for child in &box_node.children {
      check_layout_deadline(&mut has_running_deadline_counter)?;
      if child.style.running_position.is_some() {
        has_running_children = true;
        break;
      }
    }
    let trace_grid_layout =
      crate::debug::runtime::runtime_toggles().truthy("FASTR_TRACE_GRID_LAYOUT");
    let grid_trace_start = trace_grid_layout.then(std::time::Instant::now);

    // Create a fresh Taffy tree for this layout.
    //
    // Use a pooled instance to reduce allocation churn when many grid containers are laid out
    // during a single render.
    let mut taffy = crate::layout::taffy_integration::PooledTaffyTree::new();
    let mut positioned_children_map: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> =
      FxHashMap::default();

    // Partition children into running, in-flow vs. out-of-flow positioned.
    let mut in_flow_children: Vec<&BoxNode> = Vec::new();
    let mut positioned_children: Vec<&BoxNode> = Vec::new();
    let mut running_children: Vec<(usize, BoxNode)> = Vec::new();
    let mut deadline_counter = 0usize;
    let mut child_has_subgrid = false;
    for (idx, child) in box_node.children.iter().enumerate() {
      check_layout_deadline(&mut deadline_counter)?;
      if child.style.running_position.is_some() {
        // Running elements do not participate in grid layout; instead, capture a snapshot at the
        // position the element would have occupied in flow.
        running_children.push((idx, child.clone()));
        continue;
      }
      match child.style.position {
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
          positioned_children.push(child);
        }
        _ => {
          if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
            child_has_subgrid = true;
          }
          in_flow_children.push(child)
        }
      }
    }

    let intrinsic_keyword_overrides = self.resolve_intrinsic_sizing_keywords_for_taffy_tree(
      box_node,
      &in_flow_children,
      constraints,
      true,
    )?;

    let style_override = style_override_for(box_node.id);
    // Do not cache grid containers that contain running elements: running anchors are synthesized
    // based on in-flow position, so reusing cached fragments can capture the wrong snapshot.
    let taffy_counters_enabled = crate::layout::taffy_integration::taffy_counters_enabled();
    if !has_running_children && !taffy_counters_enabled {
      if let Some(cached) = layout_cache_lookup(
        box_node,
        FormattingContextType::Grid,
        constraints,
        self.factory.viewport_scroll(),
        self.viewport_size,
      ) {
        drop(intrinsic_keyword_overrides);
        return Ok(cached);
      }
    }

    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    // Grid containers that establish a fixed containing block (`transform`, `perspective`, filters,
    // or `contain: paint/layout`) need to thread that containing block into the factories used for
    // laying out descendants. Otherwise `position: fixed` descendants inside in-flow grid items can
    // incorrectly fall back to the viewport.
    let mut ctx = self.clone();
    if style.establishes_fixed_containing_block() {
      let percentage_base = constraints
        .inline_percentage_base
        .or_else(|| constraints.width())
        .unwrap_or(self.viewport_size.width);
      let padding_left = ctx.resolve_length_for_width(style.padding_left, percentage_base, style);
      let padding_right = ctx.resolve_length_for_width(style.padding_right, percentage_base, style);
      let padding_top = ctx.resolve_length_for_width(style.padding_top, percentage_base, style);
      let padding_bottom =
        ctx.resolve_length_for_width(style.padding_bottom, percentage_base, style);
      let border_left =
        ctx.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
      let border_top =
        ctx.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
      let padding_origin = Point::new(border_left, border_top);
      let content_width = constraints.width().unwrap_or(0.0).max(0.0);
      let content_height = constraints.height().unwrap_or(0.0).max(0.0);
      let padding_size = Size::new(
        content_width + padding_left + padding_right,
        content_height + padding_top + padding_bottom,
      );
      let padding_rect = Rect::new(padding_origin, padding_size);
      let padding_cb =
        crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
          padding_rect,
          ctx.viewport_size,
          Some(padding_rect.size.width),
          constraints.height().map(|_| padding_rect.size.height),
        );
      ctx.nearest_fixed_cb = padding_cb;
      ctx.factory = ctx.factory.with_fixed_cb(padding_cb);
    }
    let ctx = &ctx;

    let has_subgrid = style.grid_row_subgrid || style.grid_column_subgrid;

    // Build Taffy tree from in-flow children
    let root_id = ctx.build_taffy_tree_children(
      &mut taffy,
      box_node,
      style,
      &in_flow_children,
      constraints,
      &mut positioned_children_map,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      // Patch the root style in-place so we can avoid deep-cloning box subtrees when sizing hints
      // are temporarily overridden (e.g. flex/grid intrinsic probes or final item sizing).
      let mut style_deadline_counter = 0usize;
      let simple_grid = ctx.is_simple_grid(
        style_override,
        &in_flow_children,
        &mut style_deadline_counter,
      )?;
      let override_taffy_style = self.convert_style(style_override, None, None, simple_grid, true);
      taffy
        .set_style(root_id, override_taffy_style)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    if trace_grid_layout {
      let selector = box_node
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anon>".to_string());
      eprintln!(
        "[grid-layout] start id={} in_flow_children={} selector={}",
        box_node.id,
        in_flow_children.len(),
        selector
      );
    }

    // Block-level grid containers with `width: auto` should stretch to the available inline size.
    // Taffy treats `size: auto` as shrink-to-fit in some grid cases, so force a definite width
    // when we have one from the parent constraints.
    if let CrateAvailableSpace::Definite(outer_width) = constraints.available_width {
      if outer_width.is_finite()
        && physical_width_is_auto(style)
        && box_node.is_block_level()
        && crate::style::inline_axis_is_horizontal(style.writing_mode)
      {
        if let Ok(existing) = taffy.style(root_id) {
          if existing.size.width.is_auto() {
            let mut updated = existing.clone();
            // Taffy treats the `size` property as the border-box width, so use the available
            // border-box width directly (CSS 2.1 §10.3.3).
            updated.size.width = Dimension::length(outer_width.max(0.0));
            taffy
              .set_style(root_id, updated)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
        }
      }
    }

    // If a parent layout mode (flex/grid) already resolved a definite used border-box size for
    // this grid item, force that size on the root node without cloning/mutating styles.
    if constraints.used_border_box_width.is_some() || constraints.used_border_box_height.is_some() {
      if let Ok(existing) = taffy.style(root_id) {
        let mut updated = existing.clone();
        let mut changed = false;
        if let Some(w) = constraints
          .used_border_box_width
          .filter(|w| w.is_finite() && *w >= 0.0)
        {
          updated.size.width = Dimension::length(w);
          changed = true;
        }
        if let Some(h) = constraints
          .used_border_box_height
          .filter(|h| h.is_finite() && *h >= 0.0)
        {
          updated.size.height = Dimension::length(h);
          changed = true;
        }
        if changed {
          taffy
            .set_style(root_id, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // `width/height: fit-content` clamps the used size between the box's min- and max-content
    // contributions. When this grid formatting context is invoked without a parent precomputing
    // a used border-box size (notably at the root of the layout tree), resolve it here so Taffy
    // runs track sizing against the correct container size.
    if (constraints.used_border_box_width.is_none()
      && matches!(
        style.width_keyword,
        Some(IntrinsicSizeKeyword::FitContent { .. })
      ))
      || (constraints.used_border_box_height.is_none()
        && matches!(
          style.height_keyword,
          Some(IntrinsicSizeKeyword::FitContent { .. })
        ))
    {
      if let Ok(existing) = taffy.style(root_id) {
        let mut updated = existing.clone();
        let mut changed = false;

        if constraints.used_border_box_width.is_none() {
          if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.width_keyword {
            match self.resolve_root_fit_content_border_box_size(
              box_node,
              style,
              constraints,
              Axis::Horizontal,
              limit,
            ) {
              Ok(Some(width)) if width.is_finite() && width >= 0.0 => {
                updated.size.width = Dimension::length(width);
                changed = true;
              }
              Ok(_) => {}
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {}
            }
          }
        }

        if constraints.used_border_box_height.is_none() {
          if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.height_keyword {
            match self.resolve_root_fit_content_border_box_size(
              box_node,
              style,
              constraints,
              Axis::Vertical,
              limit,
            ) {
              Ok(Some(height)) if height.is_finite() && height >= 0.0 => {
                updated.size.height = Dimension::length(height);
                changed = true;
              }
              Ok(_) => {}
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {}
            }
          }
        }

        if changed {
          taffy
            .set_style(root_id, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // Resolve intrinsic sizing keywords used in root min/max size properties. The cached Taffy
    // template represents these keywords as `auto`, but the root node is never resolved via the
    // per-item keyword path.
    if (constraints.used_border_box_width.is_none()
      && (style.min_width_keyword.is_some() || style.max_width_keyword.is_some()))
      || (constraints.used_border_box_height.is_none()
        && (style.min_height_keyword.is_some() || style.max_height_keyword.is_some()))
    {
      if let Ok(existing) = taffy.style(root_id) {
        let mut updated = existing.clone();
        let mut changed = false;

        let keyword_to_mode = |kw: IntrinsicSizeKeyword| match kw {
          IntrinsicSizeKeyword::MinContent => Some(IntrinsicSizingMode::MinContent),
          IntrinsicSizeKeyword::MaxContent => Some(IntrinsicSizingMode::MaxContent),
          IntrinsicSizeKeyword::FillAvailable => None,
          IntrinsicSizeKeyword::FitContent { .. } => None,
        };

        let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
        let run_with_override = |override_style: Arc<ComputedStyle>,
                                 f: &dyn Fn(&BoxNode) -> Result<f32, LayoutError>|
         -> Result<f32, LayoutError> {
          if box_node.id() != 0 {
            crate::layout::style_override::with_style_override(
              box_node.id(),
              override_style,
              || f(box_node),
            )
          } else {
            let mut cloned = box_node.clone();
            cloned.style = override_style;
            f(&cloned)
          }
        };

        let intrinsic_physical_width =
          |node: &BoxNode, mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              self.compute_intrinsic_inline_size(node, mode)
            } else {
              self.compute_intrinsic_block_size(node, mode)
            }
          };
        let intrinsic_physical_height =
          |node: &BoxNode, mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              self.compute_intrinsic_block_size(node, mode)
            } else {
              self.compute_intrinsic_inline_size(node, mode)
            }
          };

        if constraints.used_border_box_width.is_none() {
          let width_override_needed =
            style.min_width_keyword.is_some() || style.max_width_keyword.is_some();
          if width_override_needed {
            let mut override_style: ComputedStyle = (*style).clone();
            override_style.width = None;
            override_style.width_keyword = None;
            override_style.min_width = None;
            override_style.min_width_keyword = None;
            override_style.max_width = None;
            override_style.max_width_keyword = None;
            let override_style = Arc::new(override_style);

            if style.min_width.is_none() {
              if let Some(mode) = style.min_width_keyword.and_then(keyword_to_mode) {
                match run_with_override(override_style.clone(), &|node| {
                  intrinsic_physical_width(node, mode)
                }) {
                  Ok(border_box) if border_box.is_finite() => {
                    updated.min_size.width = Dimension::length(border_box.max(0.0));
                    changed = true;
                  }
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {}
                  _ => {}
                }
              }
            }
            if style.max_width.is_none() {
              if let Some(mode) = style.max_width_keyword.and_then(keyword_to_mode) {
                match run_with_override(override_style.clone(), &|node| {
                  intrinsic_physical_width(node, mode)
                }) {
                  Ok(border_box) if border_box.is_finite() => {
                    updated.max_size.width = Dimension::length(border_box.max(0.0));
                    changed = true;
                  }
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {}
                  _ => {}
                }
              }
            }
          }
        }

        if constraints.used_border_box_height.is_none() {
          let height_override_needed =
            style.min_height_keyword.is_some() || style.max_height_keyword.is_some();
          if height_override_needed {
            let mut override_style: ComputedStyle = (*style).clone();
            override_style.height = None;
            override_style.height_keyword = None;
            override_style.min_height = None;
            override_style.min_height_keyword = None;
            override_style.max_height = None;
            override_style.max_height_keyword = None;
            let override_style = Arc::new(override_style);

            if style.min_height.is_none() {
              if let Some(mode) = style.min_height_keyword.and_then(keyword_to_mode) {
                match run_with_override(override_style.clone(), &|node| {
                  intrinsic_physical_height(node, mode)
                }) {
                  Ok(border_box) if border_box.is_finite() => {
                    updated.min_size.height = Dimension::length(border_box.max(0.0));
                    changed = true;
                  }
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {}
                  _ => {}
                }
              }
            }
            if style.max_height.is_none() {
              if let Some(mode) = style.max_height_keyword.and_then(keyword_to_mode) {
                match run_with_override(override_style.clone(), &|node| {
                  intrinsic_physical_height(node, mode)
                }) {
                  Ok(border_box) if border_box.is_finite() => {
                    updated.max_size.height = Dimension::length(border_box.max(0.0));
                    changed = true;
                  }
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {}
                  _ => {}
                }
              }
            }
          }
        }

        if changed {
          taffy
            .set_style(root_id, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // Convert constraints to Taffy available space
    let mut available_space = taffy::geometry::Size {
      width: match constraints.available_width {
        CrateAvailableSpace::Definite(w) => taffy::style::AvailableSpace::Definite(w),
        CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
        CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
      },
      height: match constraints.available_height {
        CrateAvailableSpace::Definite(h) => taffy::style::AvailableSpace::Definite(h),
        CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
        CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
      },
    };
    // If the grid container itself is sized with intrinsic keywords, map the available space we
    // pass into Taffy so it performs the corresponding intrinsic probe. Fit-content needs a
    // definite available size (fill-available), so only map min-/max-content here.
    if constraints.used_border_box_width.is_none() {
      match style.width_keyword {
        Some(IntrinsicSizeKeyword::MinContent) => {
          available_space.width = taffy::style::AvailableSpace::MinContent
        }
        Some(IntrinsicSizeKeyword::MaxContent) => {
          available_space.width = taffy::style::AvailableSpace::MaxContent
        }
        _ => {}
      }
    }
    if constraints.used_border_box_height.is_none() {
      match style.height_keyword {
        Some(IntrinsicSizeKeyword::MinContent) => {
          available_space.height = taffy::style::AvailableSpace::MinContent
        }
        Some(IntrinsicSizeKeyword::MaxContent) => {
          available_space.height = taffy::style::AvailableSpace::MaxContent
        }
        _ => {}
      }
    }

    let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
    // Render pipeline always installs a deadline guard (even when disabled), so only enable
    // the Taffy cancellation path when the active deadline is actually configured.
    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
      FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> =
      FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> =
      FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
    let mut auto_item_nodes: Vec<(*const BoxNode, TaffyNodeId)> = Vec::new();
    if !in_flow_children.is_empty() {
      let mut auto_deadline_counter = 0usize;
      let mut stack = vec![root_id];
      while let Some(node_id) = stack.pop() {
        check_layout_deadline(&mut auto_deadline_counter)?;
        let children = taffy
          .children(node_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy children error: {:?}", e)))?;
        if children.is_empty() && node_id != root_id {
          if let Some(ptr) = taffy.get_node_context(node_id).copied() {
            let node = unsafe { &*ptr };
            if matches!(
              node.style.content_visibility,
              crate::style::types::ContentVisibility::Auto
            ) && self.content_visibility_auto_has_definite_placeholder(node)
            {
              auto_item_nodes.push((ptr, node_id));
            }
          }
        } else {
          stack.extend(children.iter().copied());
        }
      }
    }
    let auto_item_count = auto_item_nodes.len();
    let auto_all_nodes: FxHashSet<*const BoxNode> =
      auto_item_nodes.iter().map(|(ptr, _)| *ptr).collect();
    let mut auto_unskipped_nodes: FxHashSet<*const BoxNode> = FxHashSet::default();
    let max_passes = if auto_item_count == 0 {
      1
    } else {
      GRID_CONTENT_VISIBILITY_AUTO_MAX_PASSES
    };

    let parent_inline_base = constraints.inline_percentage_base;
    for pass_idx in 0..max_passes {
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      let is_last_pass = pass_idx + 1 == max_passes;
      if is_last_pass
        && auto_item_count > 0
        && auto_unskipped_nodes.len() < auto_all_nodes.len()
        && pass_idx > 0
      {
        // If we hit the pass cap without reaching a stable viewport set, fall back to fully
        // laying out all `content-visibility:auto` items so in-viewport content is never skipped.
        auto_unskipped_nodes = auto_all_nodes.clone();
        for (_, node_id) in auto_item_nodes.iter() {
          taffy
            .mark_dirty(*node_id)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
        taffy
          .mark_dirty(root_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }

      measure_cache.clear();
      record_taffy_invocation(TaffyAdapterKind::Grid);
      let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
      let auto_unskipped_for_pass = &auto_unskipped_nodes;

      let compute_result = {
        let this = self.clone();
        let cache = &mut measure_cache;
        let measured = &mut measured_fragments;
        let measured_keys = &mut measured_node_keys;
        taffy.compute_layout_with_measure_and_cancel(
          root_id,
          available_space,
          move |known_dimensions,
                available_space,
                node_id,
                node_context,
                taffy_style: &taffy::style::Style| {
            if taffy_perf_enabled {
              record_taffy_measure_call(TaffyAdapterKind::Grid);
            }
            let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
              known.unwrap_or(match avail_dim {
                taffy::style::AvailableSpace::Definite(v) => v,
                _ => 0.0,
              })
            };

            if node_id == root_id {
              let outer_width = fallback_size(known_dimensions.width, available_space.width);
              let outer_height = known_dimensions.height.unwrap_or(0.0);
              let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
                return taffy::tree::MeasureOutput::from_size(taffy::geometry::Size {
                  width: outer_width,
                  height: outer_height,
                });
              };
              let root_box_node = unsafe { &*node_ptr };
              let percentage_base = outer_width;
              let padding_left = this.resolve_length_for_width(
                root_box_node.style.padding_left,
                percentage_base,
                &root_box_node.style,
              );
              let padding_right = this.resolve_length_for_width(
                root_box_node.style.padding_right,
                percentage_base,
                &root_box_node.style,
              );
              let padding_top = this.resolve_length_for_width(
                root_box_node.style.padding_top,
                percentage_base,
                &root_box_node.style,
              );
              let padding_bottom = this.resolve_length_for_width(
                root_box_node.style.padding_bottom,
                percentage_base,
                &root_box_node.style,
              );
              let border_left = this.resolve_length_for_width(
                root_box_node.style.used_border_left_width(),
                percentage_base,
                &root_box_node.style,
              );
              let border_right = this.resolve_length_for_width(
                root_box_node.style.used_border_right_width(),
                percentage_base,
                &root_box_node.style,
              );
              let border_top = this.resolve_length_for_width(
                root_box_node.style.used_border_top_width(),
                percentage_base,
                &root_box_node.style,
              );
              let border_bottom = this.resolve_length_for_width(
                root_box_node.style.used_border_bottom_width(),
                percentage_base,
                &root_box_node.style,
              );
              let content_width =
                (outer_width - padding_left - padding_right - border_left - border_right).max(0.0);
              let content_height =
                (outer_height - padding_top - padding_bottom - border_top - border_bottom).max(0.0);
              return taffy::tree::MeasureOutput::from_size(taffy::geometry::Size {
                width: content_width,
                height: content_height,
              });
            }

            let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
              return taffy::tree::MeasureOutput::ZERO;
            };
            this.measure_grid_item(
              node_ptr,
              node_id,
              known_dimensions,
              available_space,
              parent_inline_base,
              taffy_style,
              auto_unskipped_for_pass,
              &this.factory,
              cache,
              measured,
              measured_keys,
            )
          },
          cancel.clone(),
          TAFFY_ABORT_CHECK_STRIDE,
        )
      };
      if let Some(start) = taffy_compute_start {
        record_taffy_compute(TaffyAdapterKind::Grid, start.elapsed());
      }
      compute_result.map_err(|e| match e {
        taffy::TaffyError::LayoutAborted => match active_deadline() {
          Some(deadline) => LayoutError::Timeout {
            elapsed: deadline.elapsed(),
          },
          None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
        },
        _ => LayoutError::MissingContext(format!("Taffy compute error: {:?}", e)),
      })?;

      if auto_item_count == 0 || is_last_pass {
        break;
      }

      let mut changed = false;
      for (ptr, node_id) in auto_item_nodes.iter().copied() {
        if auto_unskipped_nodes.contains(&ptr) {
          continue;
        }
        let mut top = 0.0;
        let mut current = node_id;
        while current != root_id {
          let layout = taffy.layout(current).map_err(|e| {
            LayoutError::MissingContext(format!("Failed to get Taffy layout: {:?}", e))
          })?;
          top += layout.location.y;
          current = match taffy.parent(current) {
            Some(parent) => parent,
            None => break,
          };
        }
        let node = unsafe { &*ptr };
        let should_unskip = if crate::style::block_axis_is_horizontal(node.style.writing_mode) {
          true
        } else {
          top < self.viewport_size.height
        };
        if should_unskip {
          auto_unskipped_nodes.insert(ptr);
          changed = true;
        }
      }

      if !changed {
        break;
      }

      for (_, node_id) in auto_item_nodes.iter() {
        taffy
          .mark_dirty(*node_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }
      taffy
        .mark_dirty(root_id)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    if let Some(start) = grid_trace_start {
      let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
      eprintln!("[grid-layout] done id={} ms={elapsed_ms:.2}", box_node.id);
    }

    // Convert back to FragmentNode tree and layout each in-flow child using its formatting context.
    let auto_unskipped = (auto_item_count > 0).then_some(&auto_unskipped_nodes);
    let mut deadline_counter = 0usize;
    let mut fragment = if !has_subgrid && !child_has_subgrid {
      if let Some(result) = ctx.try_parallel_root_children_conversion(
        &taffy,
        root_id,
        box_node,
        constraints,
        &in_flow_children,
        auto_unskipped,
        &mut measured_fragments,
        &measured_node_keys,
      ) {
        result?
      } else {
        ctx.convert_to_fragments(
          &taffy,
          root_id,
          root_id,
          &constraints,
          None,
          auto_unskipped,
          &mut measured_fragments,
          &measured_node_keys,
          &positioned_children_map,
          &mut deadline_counter,
        )?
      }
    } else {
      ctx.convert_to_fragments(
        &taffy,
        root_id,
        root_id,
        &constraints,
        None,
        auto_unskipped,
        &mut measured_fragments,
        &measured_node_keys,
        &positioned_children_map,
        &mut deadline_counter,
      )?
    };
    if let Some(trace_id) = crate::debug::runtime::runtime_toggles().usize("FASTR_TRACE_GRID_TEXT")
    {
      if trace_id == box_node.id {
        fn count_texts(node: &FragmentNode) -> usize {
          let mut count = 0;
          fn walk(node: &FragmentNode, count: &mut usize) {
            if matches!(node.content, FragmentContent::Text { .. }) {
              *count += 1;
            }
            for child in node.children.iter() {
              walk(child, count);
            }
          }
          walk(node, &mut count);
          count
        }
        eprintln!(
          "[grid-text] id={} selector={:?} text_fragments={}",
          box_node.id,
          box_node.debug_info.as_ref().map(|d| d.to_selector()),
          count_texts(&fragment)
        );
      }
    }

    // Position out-of-flow children against the appropriate containing block.
    if !positioned_children.is_empty() {
      let padding_left = ctx.resolve_length_for_width(
        box_node.style.padding_left,
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let padding_top = ctx.resolve_length_for_width(
        box_node.style.padding_top,
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let padding_right = ctx.resolve_length_for_width(
        box_node.style.padding_right,
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let padding_bottom = ctx.resolve_length_for_width(
        box_node.style.padding_bottom,
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_left = ctx.resolve_length_for_width(
        box_node.style.used_border_left_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_top = ctx.resolve_length_for_width(
        box_node.style.used_border_top_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_right = ctx.resolve_length_for_width(
        box_node.style.used_border_right_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_bottom = ctx.resolve_length_for_width(
        box_node.style.used_border_bottom_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      // CSS 2.1 §10.1: the containing block for absolute positioned descendants is the padding box
      // of the nearest positioned ancestor, i.e. the rectangle bounded by the padding edge (border
      // box minus borders).
      let padding_origin = crate::geometry::Point::new(border_left, border_top);
      let padding_size = crate::geometry::Size::new(
        (fragment.bounds.width() - border_left - border_right).max(0.0),
        (fragment.bounds.height() - border_top - border_bottom).max(0.0),
      );
      let padding_rect = crate::geometry::Rect::new(padding_origin, padding_size);

      // Percentage sizes/offsets on absolutely positioned boxes resolve against the used size of the
      // containing block, even when the containing block's own height is `auto` (CSS 2.1 §10.5).
      let block_base = Some(padding_rect.size.height);
      let establishes_abs_cb = box_node.style.establishes_abs_containing_block();
      let establishes_fixed_cb = box_node.style.establishes_fixed_containing_block();
      let padding_cb =
        crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
          padding_rect,
          ctx.viewport_size,
          Some(padding_rect.size.width),
          block_base,
        );
      let root_box_id = ensure_box_id(box_node);
      let mut anchor_index =
        crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
          fragment.children_ref(),
          root_box_id,
          &box_node.style.anchor_scope,
          ctx.viewport_size,
        );
      anchor_index.insert_names_for_box(
        root_box_id,
        &box_node.style.anchor_names,
        crate::layout::anchor_positioning::AnchorBox {
          rect: crate::geometry::Rect::new(crate::geometry::Point::ZERO, fragment.bounds.size),
          writing_mode: box_node.style.writing_mode,
          direction: box_node.style.direction,
        },
      );
      let cb_for_absolute = if establishes_abs_cb {
        padding_cb
      } else {
        ctx.nearest_positioned_cb
      };
      let cb_for_fixed = if establishes_fixed_cb {
        padding_cb
      } else {
        ctx.nearest_fixed_cb
      };

      // `FormattingContextFactory::with_fixed_cb` / `with_positioned_cb` reset the per-factory cached
      // formatting contexts store. Build factory variants once so multiple positioned children can
      // reuse cached formatting contexts instead of rebuilding detached ones per child.
      let positioned_factory = ctx.factory.clone();
      let positioned_factory = if cb_for_fixed == positioned_factory.nearest_fixed_cb() {
        positioned_factory
      } else {
        positioned_factory.with_fixed_cb(cb_for_fixed)
      };
      let abs_factory = if cb_for_absolute == positioned_factory.nearest_positioned_cb() {
        positioned_factory.clone()
      } else {
        positioned_factory.with_positioned_cb(cb_for_absolute)
      };

      let axes_swapped = !crate::style::inline_axis_is_horizontal(box_node.style.writing_mode);

      let mut static_positions: FxHashMap<usize, Point> = FxHashMap::default();
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
        if let Ok(container_style) = taffy.style(root_id) {
          let row_offsets = compute_track_offsets(
            &info.rows,
            fragment.bounds.height(),
            padding_top,
            padding_bottom,
            border_top,
            border_bottom,
            container_style
              .align_content
              .unwrap_or(taffy::style::AlignContent::Stretch),
          );
          let col_offsets = compute_track_offsets(
            &info.columns,
            fragment.bounds.width(),
            padding_left,
            padding_right,
            border_left,
            border_right,
            container_style
              .justify_content
              .unwrap_or(taffy::style::AlignContent::Stretch),
          );

          let col_line_count = grid_line_count_from_offsets(&col_offsets);
          let row_line_count = grid_line_count_from_offsets(&row_offsets);

          for child in &positioned_children {
            let mut pos = Point::ZERO;
            let (x_start, x_end, x_raw) = if axes_swapped {
              (
                child.style.grid_row_start,
                child.style.grid_row_end,
                child.style.grid_row_raw.as_deref(),
              )
            } else {
              (
                child.style.grid_column_start,
                child.style.grid_column_end,
                child.style.grid_column_raw.as_deref(),
              )
            };
            if let Some((start_line, end_line)) =
              resolve_grid_line_range_from_style(x_start, x_end, x_raw, col_line_count)
            {
              if let Some((start, _)) = grid_area_for_item(&col_offsets, start_line, end_line) {
                pos.x = start - padding_origin.x;
              }
            }

            let (y_start, y_end, y_raw) = if axes_swapped {
              (
                child.style.grid_column_start,
                child.style.grid_column_end,
                child.style.grid_column_raw.as_deref(),
              )
            } else {
              (
                child.style.grid_row_start,
                child.style.grid_row_end,
                child.style.grid_row_raw.as_deref(),
              )
            };
            if let Some((start_line, end_line)) =
              resolve_grid_line_range_from_style(y_start, y_end, y_raw, row_line_count)
            {
              if let Some((start, _)) = grid_area_for_item(&row_offsets, start_line, end_line) {
                pos.y = start - padding_origin.y;
              }
            }
            static_positions.insert(ensure_box_id(child), pos);
          }
        }
      }

      let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
        ctx.font_context.clone(),
      );
      let mut abs_deadline_counter = 0usize;
      for child in &positioned_children {
        check_layout_deadline(&mut abs_deadline_counter)?;

        let cb = match child.style.position {
          crate::style::position::Position::Fixed => cb_for_fixed,
          _ => cb_for_absolute,
        };

        let fc_type = child
          .formatting_context()
          .unwrap_or(crate::style::display::FormattingContextType::Block);
        let fc = abs_factory.get(fc_type);
        let child_constraints = LayoutConstraints::new(
          CrateAvailableSpace::Definite(padding_rect.size.width),
          block_base
            .map(CrateAvailableSpace::Definite)
            .unwrap_or(CrateAvailableSpace::Indefinite),
        );

        let anchors_for_cb = Some(&anchor_index);
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
          &child.style,
          &cb,
          ctx.viewport_size,
          &ctx.font_context,
          anchors_for_cb,
          Some(root_box_id),
        );
        // Static position resolves to where the element would be in flow, relative to the containing
        // block origin (padding edge).
        let static_pos = static_positions
          .get(&ensure_box_id(child))
          .copied()
          .unwrap_or(crate::geometry::Point::ZERO);
        let width_keyword = child.style.width_keyword;
        let min_width_keyword = child.style.min_width_keyword;
        let max_width_keyword = child.style.max_width_keyword;
        let height_keyword = child.style.height_keyword;
        let min_height_keyword = child.style.min_height_keyword;
        let max_height_keyword = child.style.max_height_keyword;
        let has_inline_keyword =
          width_keyword.is_some() || min_width_keyword.is_some() || max_width_keyword.is_some();
        let has_block_keyword =
          height_keyword.is_some() || min_height_keyword.is_some() || max_height_keyword.is_some();
        let needs_inline_intrinsics = has_inline_keyword
          || (positioned_style.width.is_auto()
            && (positioned_style.left.is_auto()
              || positioned_style.right.is_auto()
              || child.is_replaced()));
        let needs_block_intrinsics = has_block_keyword
          || (positioned_style.height.is_auto()
            && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()));
        let mut static_style = (*child.style).clone();
        static_style.position = crate::style::position::Position::Relative;
        static_style.top = crate::style::types::InsetValue::Auto;
        static_style.right = crate::style::types::InsetValue::Auto;
        static_style.bottom = crate::style::types::InsetValue::Auto;
        static_style.left = crate::style::types::InsetValue::Auto;
        let static_style = Arc::new(static_style);

        let (
          mut child_fragment,
          preferred_min_inline,
          preferred_inline,
          preferred_min_block,
          preferred_block,
        ) = if child.id != 0 {
          crate::layout::style_override::with_style_override(
            child.id,
            static_style.clone(),
            || {
              let child_fragment = fc.layout(child, &child_constraints)?;
              let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
                match fc.compute_intrinsic_inline_sizes(child) {
                  Ok((min, max)) => (Some(min), Some(max)),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {
                    let min = match fc
                      .compute_intrinsic_inline_size(child, IntrinsicSizingMode::MinContent)
                    {
                      Ok(size) => Some(size),
                      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                      Err(_) => None,
                    };
                    let max = match fc
                      .compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent)
                    {
                      Ok(size) => Some(size),
                      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                      Err(_) => None,
                    };
                    (min, max)
                  }
                }
              } else {
                (None, None)
              };
              let preferred_min_block = if needs_block_intrinsics {
                match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MinContent) {
                  Ok(size) => Some(size),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                }
              } else {
                None
              };
              let preferred_block = if needs_block_intrinsics {
                match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MaxContent) {
                  Ok(size) => Some(size),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                }
              } else {
                None
              };
              Ok((
                child_fragment,
                preferred_min_inline,
                preferred_inline,
                preferred_min_block,
                preferred_block,
              ))
            },
          )?
        } else {
          let mut layout_child = (*child).clone();
          layout_child.style = static_style.clone();
          let child_fragment = fc.layout(&layout_child, &child_constraints)?;
          let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
            match fc.compute_intrinsic_inline_sizes(&layout_child) {
              Ok((min, max)) => (Some(min), Some(max)),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {
                let min = match fc
                  .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MinContent)
                {
                  Ok(size) => Some(size),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                };
                let max = match fc
                  .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
                {
                  Ok(size) => Some(size),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                };
                (min, max)
              }
            }
          } else {
            (None, None)
          };
          let preferred_min_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent) {
              Ok(size) => Some(size),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          let preferred_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent) {
              Ok(size) => Some(size),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          (
            child_fragment,
            preferred_min_inline,
            preferred_inline,
            preferred_min_block,
            preferred_block,
          )
        };

        let actual_horizontal = positioned_style.padding.left
          + positioned_style.padding.right
          + positioned_style.border_width.left
          + positioned_style.border_width.right;
        let actual_vertical = positioned_style.padding.top
          + positioned_style.padding.bottom
          + positioned_style.border_width.top
          + positioned_style.border_width.bottom;
        let content_offset = Point::new(
          positioned_style.border_width.left + positioned_style.padding.left,
          positioned_style.border_width.top + positioned_style.padding.top,
        );
        let (intrinsic_horizontal, intrinsic_vertical) =
          crate::layout::absolute_positioning::intrinsic_edge_sizes(
            &child.style,
            self.viewport_size,
            &self.font_context,
          );
        let preferred_min_inline =
          preferred_min_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_inline = preferred_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_min_block = preferred_min_block.map(|v| (v - intrinsic_vertical).max(0.0));
        let preferred_block = preferred_block.map(|v| (v - intrinsic_vertical).max(0.0));
        let intrinsic_size = Size::new(
          (child_fragment.bounds.size.width - actual_horizontal).max(0.0),
          (child_fragment.bounds.size.height - actual_vertical).max(0.0),
        );

        let mut input = crate::layout::absolute_positioning::AbsoluteLayoutInput::new(
          positioned_style,
          intrinsic_size,
          static_pos,
        );
        input.style.width_keyword = width_keyword;
        input.style.min_width_keyword = min_width_keyword;
        input.style.max_width_keyword = max_width_keyword;
        input.style.height_keyword = height_keyword;
        input.style.min_height_keyword = min_height_keyword;
        input.style.max_height_keyword = max_height_keyword;
        input.is_replaced = child.is_replaced();
        input.preferred_min_inline_size = preferred_min_inline;
        input.preferred_inline_size = preferred_inline;
        input.preferred_min_block_size = preferred_min_block;
        input.preferred_block_size = preferred_block;
        let result = abs.layout_absolute(&input, &cb)?;
        let border_size = Size::new(
          result.size.width + actual_horizontal,
          result.size.height + actual_vertical,
        );
        let border_origin = Point::new(
          result.position.x - content_offset.x,
          result.position.y - content_offset.y,
        );
        let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          || (border_size.height - child_fragment.bounds.height()).abs() > 0.01;
        if needs_relayout {
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );
          let relayout_constraints = child_constraints
            .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
          if child.id != 0 {
            if supports_used_border_box {
              child_fragment = crate::layout::style_override::with_style_override(
                child.id,
                static_style.clone(),
                || fc.layout(child, &relayout_constraints),
              )?;
            } else {
              let mut relayout_style = (*static_style).clone();
              relayout_style.width = Some(Length::px(border_size.width));
              relayout_style.height = Some(Length::px(border_size.height));
              relayout_style.width_keyword = None;
              relayout_style.height_keyword = None;
              child_fragment = crate::layout::style_override::with_style_override(
                child.id,
                Arc::new(relayout_style),
                || fc.layout(child, &relayout_constraints),
              )?;
            }
          } else {
            let mut relayout_child = (*child).clone();
            if supports_used_border_box {
              relayout_child.style = static_style.clone();
            } else {
              let mut relayout_style = (*static_style).clone();
              relayout_style.width = Some(Length::px(border_size.width));
              relayout_style.height = Some(Length::px(border_size.height));
              relayout_style.width_keyword = None;
              relayout_style.height_keyword = None;
              relayout_child.style = Arc::new(relayout_style);
            }
            child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
          }
        }
        child_fragment.bounds = crate::geometry::Rect::new(border_origin, border_size);
        child_fragment.style = Some(child.style.clone());
        let child_box_id = ensure_box_id(child);
        match &mut child_fragment.content {
          FragmentContent::Block { box_id }
          | FragmentContent::Inline { box_id, .. }
          | FragmentContent::Text { box_id, .. }
          | FragmentContent::Replaced { box_id, .. } => *box_id = Some(child_box_id),
          FragmentContent::Line { .. }
          | FragmentContent::RunningAnchor { .. }
          | FragmentContent::FootnoteAnchor { .. } => {}
        }
        fragment.children_mut().push(child_fragment);
      }
    }

    if !running_children.is_empty() {
      let mut id_to_bounds: FxHashMap<usize, Rect> =
        FxHashMap::with_capacity_and_hasher(fragment.children.len(), Default::default());
      let mut deadline_counter = 0usize;
      for child in fragment.children.iter() {
        check_layout_deadline(&mut deadline_counter)?;
        let Some(box_id) = (match &child.content {
          FragmentContent::Block { box_id }
          | FragmentContent::Inline { box_id, .. }
          | FragmentContent::Text { box_id, .. }
          | FragmentContent::Replaced { box_id, .. } => *box_id,
          FragmentContent::Line { .. }
          | FragmentContent::RunningAnchor { .. }
          | FragmentContent::FootnoteAnchor { .. } => None,
        }) else {
          continue;
        };
        id_to_bounds.entry(box_id).or_insert(child.bounds);
      }

      let mut row_offsets: Option<Vec<f32>> = None;
      let mut col_offsets: Option<Vec<f32>> = None;
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
        if let Ok(container_style) = taffy.style(root_id) {
          let percentage_base = constraints.width().unwrap_or(fragment.bounds.width());
          let padding_left =
            ctx.resolve_length_for_width(style.padding_left, percentage_base, style);
          let padding_right =
            ctx.resolve_length_for_width(style.padding_right, percentage_base, style);
          let padding_top = ctx.resolve_length_for_width(style.padding_top, percentage_base, style);
          let padding_bottom =
            ctx.resolve_length_for_width(style.padding_bottom, percentage_base, style);
          let border_left =
            ctx.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
          let border_right =
            ctx.resolve_length_for_width(style.used_border_right_width(), percentage_base, style);
          let border_top =
            ctx.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
          let border_bottom =
            ctx.resolve_length_for_width(style.used_border_bottom_width(), percentage_base, style);

          row_offsets = Some(compute_track_offsets(
            &info.rows,
            fragment.bounds.height(),
            padding_top,
            padding_bottom,
            border_top,
            border_bottom,
            container_style
              .align_content
              .unwrap_or(taffy::style::AlignContent::Stretch),
          ));
          col_offsets = Some(compute_track_offsets(
            &info.columns,
            fragment.bounds.width(),
            padding_left,
            padding_right,
            border_left,
            border_right,
            container_style
              .justify_content
              .unwrap_or(taffy::style::AlignContent::Stretch),
          ));
        }
      }

      let axes = FragmentAxes::from_writing_mode_and_direction(style.writing_mode, style.direction);
      let snapshot_factory = ctx.factory.clone();

      let parse_explicit_single_track =
        |raw: Option<&str>, start: i32, end: i32| -> Option<(u16, u16)> {
          if start > 0 && end > 0 && end == start + 1 {
            return Some((start as u16, end as u16));
          }
          let raw = raw?;
          let placement = parse_grid_line_placement_raw(raw);
          let TaffyGridPlacement::Line(start_line) = placement.start else {
            return None;
          };
          let TaffyGridPlacement::Line(end_line) = placement.end else {
            return None;
          };
          let start = start_line.as_i16();
          let end = end_line.as_i16();
          if start > 0 && end > 0 && end == start + 1 {
            Some((start as u16, end as u16))
          } else {
            None
          }
        };

      for (order, (running_idx, running_child)) in running_children.into_iter().enumerate() {
        check_layout_deadline(&mut deadline_counter)?;
        let Some(name) = running_child.style.running_position.clone() else {
          continue;
        };

        let mut anchor_x = 0.0f32;
        let mut anchor_y = 0.0f32;

        if let Some((_, next_child)) = box_node
          .children
          .iter()
          .enumerate()
          .filter(|(idx, child)| {
            *idx > running_idx
              && child.style.running_position.is_none()
              && !matches!(
                child.style.position,
                crate::style::position::Position::Absolute
                  | crate::style::position::Position::Fixed
              )
          })
          .min_by_key(|(idx, _)| *idx)
        {
          if let Some(bounds) = id_to_bounds.get(&ensure_box_id(next_child)) {
            anchor_x = bounds.x();
            anchor_y = bounds.y();
          }
        } else if let Some((_, last_child)) = box_node
          .children
          .iter()
          .enumerate()
          .filter(|(_, child)| {
            child.style.running_position.is_none()
              && !matches!(
                child.style.position,
                crate::style::position::Position::Absolute
                  | crate::style::position::Position::Fixed
              )
          })
          .max_by_key(|(idx, _)| *idx)
        {
          if let Some(bounds) = id_to_bounds.get(&ensure_box_id(last_child)) {
            match axes.block_axis() {
              PhysicalAxis::Y => {
                anchor_x = bounds.x();
                anchor_y = bounds.max_y();
              }
              PhysicalAxis::X => {
                anchor_y = bounds.y();
                anchor_x = if axes.block_positive() {
                  bounds.max_x()
                } else {
                  bounds.x()
                };
              }
            }
          }
        } else {
          match axes.block_axis() {
            PhysicalAxis::Y => {
              anchor_y = fragment.bounds.height();
            }
            PhysicalAxis::X => {
              anchor_x = if axes.block_positive() {
                fragment.bounds.width()
              } else {
                0.0
              };
            }
          }
        }

        let mut area_w: Option<f32> = None;
        let mut area_h: Option<f32> = None;

        if let Some(col_offsets) = col_offsets.as_ref() {
          if let Some((start_line, end_line)) = parse_explicit_single_track(
            running_child.style.grid_column_raw.as_deref(),
            running_child.style.grid_column_start,
            running_child.style.grid_column_end,
          ) {
            if let Some((start, end)) = grid_area_for_item(col_offsets, start_line, end_line) {
              anchor_x = start;
              area_w = Some((end - start).max(0.0));
            }
          }
        }

        if let Some(row_offsets) = row_offsets.as_ref() {
          if let Some((start_line, end_line)) = parse_explicit_single_track(
            running_child.style.grid_row_raw.as_deref(),
            running_child.style.grid_row_start,
            running_child.style.grid_row_end,
          ) {
            if let Some((start, end)) = grid_area_for_item(row_offsets, start_line, end_line) {
              anchor_y = start;
              area_h = Some((end - start).max(0.0));
            }
          }
        }

        let mut snapshot_node = running_child.clone();
        let mut snapshot_style = snapshot_node.style.as_ref().clone();
        snapshot_style.running_position = None;
        snapshot_style.position = crate::style::position::Position::Static;
        snapshot_node.style = Arc::new(snapshot_style);
        clear_running_position_in_box_tree(&mut snapshot_node);

        let fc_type = snapshot_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = snapshot_factory.get(fc_type);

        let snapshot_constraints = if let (Some(w), Some(h)) = (area_w, area_h) {
          LayoutConstraints::new(
            CrateAvailableSpace::Definite(w.max(0.0)),
            CrateAvailableSpace::Definite(h.max(0.0)),
          )
          .with_used_border_box_size(Some(w.max(0.0)), Some(h.max(0.0)))
          .with_inline_percentage_base(Some(w.max(0.0)))
        } else {
          let container_w = fragment.bounds.width().max(0.0);
          LayoutConstraints::new(
            CrateAvailableSpace::Definite(container_w),
            CrateAvailableSpace::Indefinite,
          )
        };

        match fc.layout(&snapshot_node, &snapshot_constraints) {
          Ok(snapshot_fragment) => {
            let eps = (order as f32) * 1e-4;
            let offset = axes.block_offset(eps);
            let anchor_bounds =
              Rect::from_xywh(anchor_x + offset.x, anchor_y + offset.y, 0.0, 0.01);
            let mut anchor =
              FragmentNode::new_running_anchor(anchor_bounds, name, snapshot_fragment);
            anchor.style = Some(running_child.style.clone());
            fragment.children_mut().push(anchor);
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }
    }

    if !has_running_children {
      layout_cache_store(
        box_node,
        FormattingContextType::Grid,
        constraints,
        &fragment,
        self.factory.viewport_scroll(),
        self.viewport_size,
      );
    }

    Ok(fragment)
  }

  fn compute_intrinsic_inline_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    self.compute_intrinsic_size(box_node, box_node.style.as_ref(), mode)
  }

  fn compute_intrinsic_block_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    debug_assert!(
      matches!(
        box_node.formatting_context(),
        Some(FormattingContextType::Grid)
      ),
      "GridFormattingContext must only query grid containers",
    );

    if let Some(cached) = intrinsic_block_cache_lookup(box_node, mode) {
      return Ok(cached);
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    if style.containment.isolates_block_size() {
      let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
      let edges = if inline_is_horizontal {
        self.vertical_edges_px(style).unwrap_or(0.0)
      } else {
        self.horizontal_edges_px(style).unwrap_or(0.0)
      };
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_height
      } else {
        style.contain_intrinsic_width
      };
      let fallback = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        None,
        Some(0.0),
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
      let size = (edges + fallback).max(0.0);
      intrinsic_block_cache_store(box_node, mode, size);
      return Ok(size);
    }

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let intrinsic_inline_space = match mode {
      IntrinsicSizingMode::MinContent => CrateAvailableSpace::MinContent,
      IntrinsicSizingMode::MaxContent => CrateAvailableSpace::MaxContent,
    };
    let intrinsic_constraints = if inline_is_horizontal {
      LayoutConstraints::new(intrinsic_inline_space, CrateAvailableSpace::Indefinite)
    } else {
      LayoutConstraints::new(CrateAvailableSpace::Indefinite, intrinsic_inline_space)
    };

    // Use a pooled Taffy tree to avoid repeated allocation churn during intrinsic sizing probes.
    let mut taffy = crate::layout::taffy_integration::PooledTaffyTree::new();
    let mut positioned_children: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> = FxHashMap::default();

    let in_flow_children: Vec<&BoxNode> = box_node
      .children
      .iter()
      .filter(|child| {
        !matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      })
      .collect();

    // Resolve intrinsic sizing keywords for descendants while computing this grid container's
    // intrinsic block sizes. Skip resolving the root itself to avoid recursion: resolving
    // `height/width: max-content` requires asking for the same intrinsic block-size we are
    // computing.
    let _intrinsic_keyword_overrides = self.resolve_intrinsic_sizing_keywords_for_taffy_tree(
      box_node,
      &in_flow_children,
      &intrinsic_constraints,
      false,
    )?;

    let root_id = self.build_taffy_tree_children(
      &mut taffy,
      box_node,
      style,
      &in_flow_children,
      &intrinsic_constraints,
      &mut positioned_children,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      let mut style_deadline_counter = 0usize;
      let simple_grid = self.is_simple_grid(
        style_override,
        &in_flow_children,
        &mut style_deadline_counter,
      )?;
      let override_taffy_style = self.convert_style(style_override, None, None, simple_grid, true);
      taffy
        .set_style(root_id, override_taffy_style)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    // Convert constraints to Taffy available space. The intrinsic sizing probes for block-size
    // mirror the default `FormattingContext::compute_intrinsic_block_size` implementation:
    // intrinsic available space in the inline axis, indefinite in the block axis.
    let available_space = taffy::geometry::Size {
      width: match intrinsic_constraints.available_width {
        CrateAvailableSpace::Definite(w) => taffy::style::AvailableSpace::Definite(w),
        CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
        CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
      },
      height: match intrinsic_constraints.available_height {
        CrateAvailableSpace::Definite(h) => taffy::style::AvailableSpace::Definite(h),
        CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
        CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
      },
    };

    record_taffy_invocation(TaffyAdapterKind::Grid);
    let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
    let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
    let compute_result = taffy.compute_layout_with_measure_and_cancel(
      root_id,
      available_space,
      {
        let this = self.clone();
        let factory = self.factory.clone();
        let viewport_size = self.viewport_size;
        let mut cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
        move |known_dimensions,
              available_space,
              node_id,
              node_context,
              taffy_style: &taffy::style::Style| {
          if taffy_perf_enabled {
            record_taffy_measure_call(TaffyAdapterKind::Grid);
          }
          if node_id == root_id {
            return taffy::tree::MeasureOutput::ZERO;
          }
          let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
            known.unwrap_or(match avail_dim {
              taffy::style::AvailableSpace::Definite(v) => v,
              _ => 0.0,
            })
          };
          let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
            return taffy::tree::MeasureOutput::ZERO;
          };
          let box_node = unsafe { &*node_ptr };
          let style_override = style_override_for(box_node.id);
          let style: &ComputedStyle = style_override
            .as_deref()
            .unwrap_or_else(|| box_node.style.as_ref());
          let wants_baseline_y = taffy_style.align_self == Some(taffy::style::AlignItems::Baseline);
          let wants_baseline_x = taffy_style.justify_self
            == Some(taffy::style::AlignItems::Baseline)
            && crate::style::block_axis_is_horizontal(style.writing_mode);
          let mut available_space = available_space;
          if known_dimensions.width.is_none()
            && matches!(
              available_space.width,
              taffy::style::AvailableSpace::Definite(_)
            )
            && physical_width_is_auto(style)
          {
            available_space.width = taffy::style::AvailableSpace::MaxContent;
          }

          let fc_type = box_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let drop_available_height = matches!(
            available_space.height,
            taffy::style::AvailableSpace::Definite(_)
          ) && (known_dimensions.height.is_some()
            || matches!(
              fc_type,
              FormattingContextType::Block
                | FormattingContextType::Inline
                | FormattingContextType::Table
            ) && !node_or_in_flow_children_depend_on_available_height(box_node));
          let (key, known_dimensions, available_space) = MeasureKey::new_with_snapped_sizes(
            box_node,
            known_dimensions,
            available_space,
            viewport_size,
            drop_available_height,
          );
          if let Some(output) = cache.get(&key) {
            return *output;
          }
          if let Some(output) = grid_measure_size_cache_lookup(&key) {
            cache.insert(key, output);
            return output;
          }
          let fc = factory.get(fc_type);
          let inline_is_horizontal =
            crate::style::inline_axis_is_horizontal(box_node.style.writing_mode);
          let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_inline_size(box_node, mode)
            } else {
              fc.compute_intrinsic_block_size(box_node, mode)
            }
          };
          let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_block_size(box_node, mode)
            } else {
              fc.compute_intrinsic_inline_size(box_node, mode)
            }
          };

          let mut intrinsic_width: Option<f32> = None;
          if inline_is_horizontal && known_dimensions.width.is_none() {
            intrinsic_width = match available_space.width {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          let mut intrinsic_height: Option<f32> = None;
          if !inline_is_horizontal && known_dimensions.height.is_none() {
            intrinsic_height = match available_space.height {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          if !(wants_baseline_y || wants_baseline_x)
            && (intrinsic_width.is_some() || intrinsic_height.is_some())
          {
            let percentage_base = match available_space.width {
              taffy::style::AvailableSpace::Definite(w) => w,
              _ => 0.0,
            };
            let (
              padding_left,
              padding_right,
              padding_top,
              padding_bottom,
              border_left,
              border_right,
              border_top,
              border_bottom,
            ) = this.resolved_padding_border_for_measure(style, percentage_base);

            let width = intrinsic_width
              .map(|border_width| {
                (border_width - padding_left - padding_right - border_left - border_right).max(0.0)
              })
              .unwrap_or_else(|| {
                fallback_size(known_dimensions.width, available_space.width).max(0.0)
              });

            let height = intrinsic_height
              .map(|border_height| {
                (border_height - padding_top - padding_bottom - border_top - border_bottom).max(0.0)
              })
              .unwrap_or_else(|| {
                fallback_size(known_dimensions.height, available_space.height).max(0.0)
              });
            let size = taffy::geometry::Size { width, height };
            let output = taffy::tree::MeasureOutput::from_size(size);
            cache.insert(key, output);
            return output;
          }
          let constraints =
            constraints_from_taffy(viewport_size, known_dimensions, available_space, None);
          let fragment = match fc.layout(box_node, &constraints) {
            Ok(fragment) => fragment,
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => return taffy::tree::MeasureOutput::ZERO,
          };
          let percentage_base = match available_space.width {
            taffy::style::AvailableSpace::Definite(w) => w,
            _ => constraints
              .width()
              .unwrap_or_else(|| fragment.bounds.width()),
          };
          let content_size = this.content_box_size(&fragment, style, percentage_base);
          let size = taffy::geometry::Size {
            width: content_size.width.max(0.0),
            height: content_size.height.max(0.0),
          };
          let mut baseline_deadline_counter = 0usize;
          let baseline_y = if wants_baseline_y {
            match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let baseline_x = if wants_baseline_x {
            match first_baseline_offset_x(
              &fragment,
              style.writing_mode,
              &mut baseline_deadline_counter,
            ) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let output = taffy::tree::MeasureOutput::from_size_and_baselines(
            size,
            taffy::geometry::Point {
              x: baseline_x,
              y: baseline_y,
            },
          );
          grid_measure_size_cache_store(key, output);
          cache.insert(key, output);
          output
        }
      },
      cancel,
      TAFFY_ABORT_CHECK_STRIDE,
    );
    if let Some(start) = taffy_compute_start {
      record_taffy_compute(TaffyAdapterKind::Grid, start.elapsed());
    }
    compute_result.map_err(|e| match e {
      taffy::TaffyError::LayoutAborted => match active_deadline() {
        Some(deadline) => LayoutError::Timeout {
          elapsed: deadline.elapsed(),
        },
        None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
      },
      _ => LayoutError::MissingContext(format!("Taffy compute error: {:?}", e)),
    })?;

    let layout = taffy
      .layout(root_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;

    let result = if inline_is_horizontal {
      layout.size.height
    } else {
      layout.size.width
    }
    .max(0.0);
    intrinsic_block_cache_store(box_node, mode, result);
    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{DiagnosticsLevel, FastRender, FastRenderConfig, RenderOptions};
  use crate::debug::runtime;
  use crate::style::display::FormattingContextType;
  use crate::style::properties::apply_content_visibility_implied_containment;
  use crate::style::types::AlignItems;
  use crate::style::types::AspectRatio;
  use crate::style::types::BorderStyle;
  use crate::style::types::ContentVisibility;
  use crate::style::types::GridAutoFlow;
  use crate::style::types::GridTrack;
  use crate::style::types::Overflow;
  use crate::style::types::ScrollbarWidth;
  use crate::style::types::WhiteSpace;
  use crate::style::types::WordBreak;
  use crate::style::types::WritingMode;
  use crate::text::font_db::FontConfig;
  use crate::tree::box_tree::BoxTree;
  use std::collections::HashMap;
  use std::sync::Arc;

  fn make_grid_style() -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    Arc::new(style)
  }

  fn make_item_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  fn make_grid_style_with_tracks(cols: Vec<GridTrack>, rows: Vec<GridTrack>) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = cols;
    style.grid_template_rows = rows;
    Arc::new(style)
  }

  fn make_text_item(text: &str, font_size: f32) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.font_size = font_size;
    let style = Arc::new(style);
    let text_child = BoxNode::new_text(style.clone(), text.to_string());
    BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child])
  }

  fn make_text_item_with_writing_mode(
    text: &str,
    font_size: f32,
    writing_mode: WritingMode,
    width: f32,
  ) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.font_size = font_size;
    style.writing_mode = writing_mode;
    style.width = Some(Length::px(width));
    let style = Arc::new(style);
    let text_child = BoxNode::new_text(style.clone(), text.to_string());
    BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child])
  }

  fn find_block_fragment<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
    fn walk<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
      if matches!(
        node.content,
        FragmentContent::Block {
          box_id: Some(id), ..
        } if id == box_id
      ) {
        return Some(node);
      }
      node.children.iter().find_map(|child| walk(child, box_id))
    }

    walk(fragment, box_id).unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
  }

  fn content_visibility_test_guard() -> runtime::ThreadRuntimeTogglesGuard {
    runtime::set_thread_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(HashMap::from(
      [(
        "FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX".to_string(),
        "0".to_string(),
      )],
    ))))
  }

  #[test]
  fn grid_content_visibility_auto_in_view_does_not_skip() {
    fn has_text(fragment: &FragmentNode) -> bool {
      matches!(fragment.content, FragmentContent::Text { .. })
        || fragment.children.iter().any(has_text)
    }

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    container_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];
    let container_style = Arc::new(container_style);

    let mut item_style = ComputedStyle::default();
    item_style.content_visibility = ContentVisibility::Auto;
    item_style.contain_intrinsic_height.length = Some(Length::px(30.0));
    crate::style::properties::apply_content_visibility_implied_containment(&mut item_style);
    let item_style = Arc::new(item_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = CssDisplay::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let item = BoxNode::new_block(
      item_style,
      FormattingContextType::Block,
      vec![BoxNode::new_text(text_style, "hello".to_string())],
    );
    let container = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![item]);

    let gc = GridFormattingContext::with_viewport(Size::new(200.0, 200.0));
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = gc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(fragment.children.len(), 1);
    assert!(
      has_text(&fragment.children[0]),
      "content-visibility:auto in viewport must not skip layout"
    );
  }

  #[test]
  fn contain_intrinsic_size_auto_uses_remembered_size_when_skipped_in_grid() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let _toggles = content_visibility_test_guard();

    fn has_text(fragment: &FragmentNode) -> bool {
      matches!(fragment.content, FragmentContent::Text { .. })
        || fragment.children.iter().any(has_text)
    }

    let spacer_id = 301usize;
    let auto_id = 302usize;
    let after_id = 303usize;

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    container_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto, GridTrack::Auto];
    let container_style = Arc::new(container_style);

    let mut spacer_style = ComputedStyle::default();
    spacer_style.height = Some(Length::px(200.0));
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = spacer_id;

    let padding_top = 5.0;
    let padding_bottom = 7.0;
    let border_top = 2.0;
    let border_bottom = 3.0;

    let mut auto_style = ComputedStyle::default();
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.padding_top = Length::px(padding_top);
    auto_style.padding_bottom = Length::px(padding_bottom);
    auto_style.border_top_width = Length::px(border_top);
    auto_style.border_bottom_width = Length::px(border_bottom);
    auto_style.border_top_style = BorderStyle::Solid;
    auto_style.border_bottom_style = BorderStyle::Solid;
    apply_content_visibility_implied_containment(&mut auto_style);
    let auto_style = Arc::new(auto_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = CssDisplay::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let mut auto_item = BoxNode::new_block(
      auto_style,
      FormattingContextType::Block,
      vec![BoxNode::new_text(text_style, "remembered".to_string())],
    );
    auto_item.id = auto_id;

    let mut after_style = ComputedStyle::default();
    after_style.height = Some(Length::px(50.0));
    let mut after_item =
      BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]);
    after_item.id = after_id;

    let container = BoxNode::new_block(
      container_style,
      FormattingContextType::Grid,
      vec![spacer, auto_item, after_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 1000.0);

    let gc_large = GridFormattingContext::with_viewport(Size::new(200.0, 400.0))
      .with_parallelism(LayoutParallelism::disabled());
    let fragment_large = gc_large
      .layout(&container, &constraints)
      .expect("layout should succeed");
    let auto_fragment_large = find_block_fragment(&fragment_large, auto_id);
    assert!(
      has_text(auto_fragment_large),
      "expected first pass to fully lay out the content-visibility:auto item"
    );
    let remembered_border_box_height = auto_fragment_large.bounds.height();
    let after_y_large = find_block_fragment(&fragment_large, after_id).bounds.y();

    let gc_small = GridFormattingContext::with_viewport(Size::new(200.0, 100.0))
      .with_parallelism(LayoutParallelism::disabled());
    let fragment_small = gc_small
      .layout(&container, &constraints)
      .expect("layout should succeed");
    let auto_fragment_small = find_block_fragment(&fragment_small, auto_id);
    assert!(
      !has_text(auto_fragment_small),
      "expected second pass to skip the offscreen content-visibility:auto item"
    );
    assert!(
      auto_fragment_small.children.is_empty(),
      "expected skipped grid item to be represented by a placeholder fragment"
    );
    assert!(
      (auto_fragment_small.bounds.height() - remembered_border_box_height).abs() < 0.5,
      "expected placeholder height {:.1}, got {:.1}",
      remembered_border_box_height,
      auto_fragment_small.bounds.height()
    );

    let after_y_small = find_block_fragment(&fragment_small, after_id).bounds.y();
    assert!(
      (after_y_small - after_y_large).abs() < 0.5,
      "expected following grid item y to remain stable (first={after_y_large:.1}, second={after_y_small:.1})"
    );
  }

  #[test]
  fn grid_border_box_width_accounts_for_padding_and_border_with_explicit_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = Some(Length::px(100.0));
    style.width_keyword = None;
    style.padding_left = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.border_right_style = crate::style::types::BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![]);

    // CSS `width` applies to the content box by default (`box-sizing: content-box`), so the used
    // border-box width includes padding and borders.
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(1000.0, 200.0))
      .unwrap();
    assert_eq!(fragment.bounds.width(), 124.0);
  }

  #[test]
  fn grid_auto_width_stretches_border_box_including_padding_and_border() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = None;
    style.width_keyword = None;
    style.padding_left = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.border_right_style = crate::style::types::BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![]);

    // With `width: auto`, block-level boxes should stretch to fill the available inline size. The
    // used width is the border-box width, so padding/borders must be included in the total size.
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(124.0, 200.0))
      .unwrap();
    assert_eq!(fragment.bounds.width(), 124.0);
  }

  #[test]
  fn grid_measure_key_uses_box_id_when_available() {
    use taffy::style::AvailableSpace;

    let mut node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    node.id = 123;
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let available = taffy::geometry::Size {
      width: AvailableSpace::MaxContent,
      height: AvailableSpace::MaxContent,
    };

    let key = MeasureKey::new(&node, known, available, viewport, false);
    assert_eq!(key.box_id, 123);
  }

  #[test]
  fn grid_measure_key_canonicalizes_negative_zero() {
    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let available = taffy::geometry::Size {
      width: taffy::style::AvailableSpace::MaxContent,
      height: taffy::style::AvailableSpace::MaxContent,
    };

    let key_zero = MeasureKey::new(
      &node,
      taffy::geometry::Size {
        width: Some(0.0),
        height: None,
      },
      available,
      viewport,
      false,
    );
    let key_neg_zero = MeasureKey::new(
      &node,
      taffy::geometry::Size {
        width: Some(-0.0),
        height: None,
      },
      available,
      viewport,
      false,
    );

    assert_eq!(key_zero, key_neg_zero);
  }

  #[test]
  fn grid_intrinsic_width_probe_returns_nonzero_height() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut item = make_text_item("Flipped side", 16.0);
    item.id = 1;
    let node_ptr: *const BoxNode = &item;

    // Use a dummy node id for the measured item (only used for per-node key tracking).
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let node_id = taffy
      .new_leaf(taffy::style::Style::default())
      .expect("create leaf node");

    let known_dimensions = taffy::geometry::Size {
      width: None,
      height: Some(0.0),
    };
    let available_space = taffy::geometry::Size {
      width: taffy::style::AvailableSpace::MaxContent,
      height: taffy::style::AvailableSpace::Definite(0.0),
    };

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
    let auto_unskipped: FxHashSet<*const BoxNode> = FxHashSet::default();

    let size = fc.measure_grid_item(
      node_ptr,
      node_id,
      known_dimensions,
      available_space,
      Some(260.0),
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      size.size.height > 0.1,
      "expected intrinsic width probes to compute a non-zero height (got {size:?})"
    );
  }

  #[test]
  fn grid_respects_used_border_box_size_overrides() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::InlineGrid;
    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(10.0));
    item_style.height = Some(Length::px(10.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;

    let container = BoxNode::new_inline_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![BoxNode::new_block(
        Arc::new(item_style),
        FormattingContextType::Block,
        vec![],
      )],
    );

    let constraints =
      LayoutConstraints::definite(500.0, 100.0).with_used_border_box_size(Some(500.0), Some(100.0));
    let fragment = fc
      .layout(&container, &constraints)
      .expect("inline-grid layout should succeed");

    assert_eq!(fragment.bounds.width(), 500.0);
    assert_eq!(fragment.bounds.height(), 100.0);
  }

  #[test]
  fn grid_respects_style_override_for_root() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut base_style = ComputedStyle::default();
    base_style.display = CssDisplay::InlineGrid;
    base_style.width = Some(Length::px(50.0));
    base_style.width_keyword = None;

    let mut grid = BoxNode::new_inline_block(
      Arc::new(base_style.clone()),
      FormattingContextType::Grid,
      vec![],
    );
    grid.id = 1;

    let constraints = LayoutConstraints::new(
      CrateAvailableSpace::MaxContent,
      CrateAvailableSpace::Indefinite,
    );
    let fragment = fc.layout(&grid, &constraints).expect("inline-grid layout");
    assert_eq!(fragment.bounds.width(), 50.0);

    let mut override_style = base_style;
    override_style.width = Some(Length::px(100.0));
    override_style.width_keyword = None;
    let fragment =
      crate::layout::style_override::with_style_override(grid.id, Arc::new(override_style), || {
        fc.layout(&grid, &constraints)
      })
      .expect("layout with style override");

    assert_eq!(fragment.bounds.width(), 100.0);
  }

  #[test]
  fn grid_root_width_keyword_min_content_is_narrower_than_max_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let make_item = || make_text_item("hello world goodbye", 16.0);

    let make_container = |keyword: IntrinsicSizeKeyword| {
      let mut style = ComputedStyle::default();
      style.display = CssDisplay::Grid;
      style.grid_template_columns = vec![GridTrack::Auto];
      style.grid_template_rows = vec![GridTrack::Auto];
      style.width = None;
      style.width_keyword = Some(keyword);
      BoxNode::new_block(
        Arc::new(style),
        FormattingContextType::Grid,
        vec![make_item()],
      )
    };

    let constraints = LayoutConstraints::definite(1000.0, 200.0);
    let min_fragment = fc
      .layout(
        &make_container(IntrinsicSizeKeyword::MinContent),
        &constraints,
      )
      .unwrap();
    let max_fragment = fc
      .layout(
        &make_container(IntrinsicSizeKeyword::MaxContent),
        &constraints,
      )
      .unwrap();
    assert!(
      min_fragment.bounds.width() + 0.5 < max_fragment.bounds.width(),
      "expected min-content width ({:.2}) < max-content width ({:.2})",
      min_fragment.bounds.width(),
      max_fragment.bounds.width(),
    );
  }

  #[test]
  fn grid_root_width_keyword_fit_content_clamps_between_min_and_max_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_item = || make_text_item("fit content prefers available", 16.0);
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Auto];
    style.grid_template_rows = vec![GridTrack::Auto];
    style.width = None;
    style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

    let container = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Grid,
      vec![make_item()],
    );

    let (min_intrinsic, max_intrinsic) = fc
      .compute_intrinsic_inline_sizes(&container)
      .expect("intrinsic inline sizes");
    assert!(
      max_intrinsic > min_intrinsic + 1.0,
      "expected distinct min/max intrinsic widths (min={min_intrinsic:.2}, max={max_intrinsic:.2})"
    );
    let available_width = (min_intrinsic + (max_intrinsic - min_intrinsic) / 2.0)
      .clamp(min_intrinsic + 1.0, max_intrinsic - 1.0);

    let fragment = fc
      .layout(
        &container,
        &LayoutConstraints::definite(available_width, 200.0),
      )
      .unwrap();
    assert!(
      (fragment.bounds.width() - available_width).abs() < 1.0,
      "expected fit-content width {:.2} to match available width {:.2}",
      fragment.bounds.width(),
      available_width
    );
    assert!(
      fragment.bounds.width() + 0.5 < max_intrinsic,
      "expected fit-content width {:.2} < max-content width {:.2}",
      fragment.bounds.width(),
      max_intrinsic,
    );
  }

  #[test]
  fn grid_root_max_content_width_does_not_stretch_to_available_space() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(500.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected max-content grid width to equal the sum of fixed tracks, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_root_max_width_keyword_max_content_clamps_auto_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = None;
    style.width_keyword = None;
    style.max_width = None;
    style.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(500.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected max-width:max-content grid width to equal the sum of fixed tracks, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_root_min_width_keyword_max_content_overflows_available_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = None;
    style.width_keyword = None;
    style.min_width = None;
    style.min_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(100.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected min-width:max-content grid width to equal the sum of fixed tracks, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_root_max_width_keyword_max_content_clamps_explicit_width_in_vertical_writing_mode() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.width = Some(Length::px(500.0));
    style.width_keyword = None;
    style.max_width = None;
    style.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    // In vertical writing modes, CSS grid rows map to the physical horizontal axis.
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_columns = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(1000.0, 1000.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected max-width:max-content to clamp explicit width, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let max_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let max_width = max_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      max_width + 0.5 < auto_width,
      "expected max-content grid item width ({max_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_width_keyword_fit_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let fit_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None })),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(fit_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let fit_width = fit_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      fit_width + 0.5 < auto_width,
      "expected fit-content grid item width ({fit_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_fit_content_with_max_width_affects_row_sizing() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width: Option<Length>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
      item_style.max_width = max_width;
      item_style.max_width_keyword = None;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let unconstrained = fc.layout(&make_grid(None), &constraints).unwrap();
    let constrained = fc
      .layout(&make_grid(Some(Length::px(50.0))), &constraints)
      .unwrap();

    assert_eq!(unconstrained.children.len(), 1);
    assert_eq!(constrained.children.len(), 1);
    let unconstrained_bounds = unconstrained.children[0].bounds;
    let constrained_bounds = constrained.children[0].bounds;

    assert!(
      constrained_bounds.width() + 0.5 < unconstrained_bounds.width(),
      "expected max-width to narrow fit-content width (unconstrained={:.2}, constrained={:.2})",
      unconstrained_bounds.width(),
      constrained_bounds.width()
    );
    assert!(
      constrained_bounds.height() > unconstrained_bounds.height() + 5.0,
      "expected narrower fit-content width to increase row height (unconstrained={:.2}, constrained={:.2})",
      unconstrained_bounds.height(),
      constrained_bounds.height()
    );
  }

  #[test]
  fn grid_item_max_width_keyword_max_content_clamps_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = None;
      item_style.max_width = None;
      item_style.max_width_keyword = max_width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let clamped_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(clamped_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let clamped_width = clamped_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      clamped_width + 0.5 < auto_width,
      "expected max-width:max-content grid item width ({clamped_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_max_width_keyword_fit_content_clamps_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width_keyword: Option<IntrinsicSizeKeyword>,
                     explicit_width: Option<Length>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.display = CssDisplay::Block;
      item_style.font_size = 16.0;
      item_style.width = explicit_width;
      item_style.width_keyword = None;
      item_style.max_width = None;
      item_style.max_width_keyword = max_width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let auto_fragment = fc
      .layout(&make_grid(None, None), &constraints)
      .expect("auto grid should layout");
    let clamped_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None }), None),
        &constraints,
      )
      .expect("fit-content clamped grid should layout");
    let explicit_fragment = fc
      .layout(
        &make_grid(
          Some(IntrinsicSizeKeyword::FitContent { limit: None }),
          Some(Length::px(500.0)),
        ),
        &constraints,
      )
      .expect("explicit width grid should layout");

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(clamped_fragment.children.len(), 1);
    assert_eq!(explicit_fragment.children.len(), 1);

    let auto_width = auto_fragment.children[0].bounds.width();
    let clamped_width = clamped_fragment.children[0].bounds.width();
    let explicit_width = explicit_fragment.children[0].bounds.width();

    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      clamped_width + 0.5 < auto_width,
      "expected max-width:fit-content to cap below stretched width (auto={auto_width:.2}, clamped={clamped_width:.2})",
    );
    assert!(
      (explicit_width - clamped_width).abs() < 0.5,
      "expected max-width:fit-content to clamp explicit widths the same as auto (clamped={clamped_width:.2}, explicit={explicit_width:.2})",
    );
  }

  #[test]
  fn grid_item_min_width_keyword_fit_content_does_not_prevent_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = None;
    item_style.min_width = None;
    item_style.min_width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let width = fragment.children[0].bounds.width();
    assert!(
      (width - 200.0).abs() < 0.1,
      "expected min-width:fit-content item to still stretch to 200px, got {width:.2}"
    );
  }

  #[test]
  fn grid_item_min_height_keyword_fit_content_does_not_prevent_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.align_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.height = None;
    item_style.height_keyword = None;
    item_style.min_height = None;
    item_style.min_height_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let height = fragment.children[0].bounds.height();
    assert!(
      (height - 200.0).abs() < 0.1,
      "expected min-height:fit-content item to still stretch to 200px, got {height:.2}"
    );
  }

  #[test]
  fn grid_item_width_keyword_min_content_is_narrower_than_max_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: IntrinsicSizeKeyword| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = Some(width_keyword);
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let min_fragment = fc
      .layout(&make_grid(IntrinsicSizeKeyword::MinContent), &constraints)
      .unwrap();
    let max_fragment = fc
      .layout(&make_grid(IntrinsicSizeKeyword::MaxContent), &constraints)
      .unwrap();

    assert_eq!(min_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let min_width = min_fragment.children[0].bounds.width();
    let max_width = max_fragment.children[0].bounds.width();
    assert!(
      min_width + 0.5 < max_width,
      "expected min-content width ({min_width:.2}) < max-content width ({max_width:.2})"
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_prevents_stretch_in_vertical_writing_mode() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.writing_mode = WritingMode::VerticalRl;
      // In vertical writing modes, grid rows are the physical horizontal axis. Use a fixed row
      // track to create a definite 200px inline size for stretching.
      grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.align_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let max_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let max_width = max_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      max_width + 0.5 < auto_width,
      "expected max-content grid item width ({max_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_is_clamped_by_max_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    item_style.max_width = Some(Length::px(50.0));
    item_style.max_width_keyword = None;
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let width = fragment.children[0].bounds.width();
    assert!(
      (width - 50.0).abs() < 0.5,
      "expected max-content width to be clamped to 50px, got {width:.2}"
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_is_clamped_by_max_width_keyword_fit_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // Make the max-content width wider than the grid area, but keep min-content smaller so
    // fit-content clamps to the available width.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(80.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    item_style.max_width = None;
    item_style.max_width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(80.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let width = fragment.children[0].bounds.width();
    assert!(
      (width - 80.0).abs() < 0.5,
      "expected max-width:fit-content to clamp max-content width to available 80px, got {width:.2}",
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_with_max_width_keyword_fit_content_can_overflow() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(80.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.white_space = WhiteSpace::Nowrap;
      item_style.width = None;
      item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
      item_style.max_width = None;
      item_style.max_width_keyword = max_width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(80.0, 200.0);
    let unconstrained = fc
      .layout(&make_grid(None), &constraints)
      .expect("unconstrained grid should layout");
    let fit_content_max = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None })),
        &constraints,
      )
      .expect("fit-content max grid should layout");

    assert_eq!(unconstrained.children.len(), 1);
    assert_eq!(fit_content_max.children.len(), 1);
    let base_width = unconstrained.children[0].bounds.width();
    let fit_width = fit_content_max.children[0].bounds.width();

    assert!(
      base_width > 80.0 + 0.5,
      "expected max-content width to overflow the 80px grid area under nowrap text, got {base_width:.2}",
    );
    assert!(
      (fit_width - base_width).abs() < 0.5,
      "expected max-width:fit-content to match max-content width when min-content exceeds available (base={base_width:.2}, fit={fit_width:.2})",
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_uses_start_alignment_in_rtl() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.direction = Direction::Rtl;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child = &fragment.children[0];
    let width = child.bounds.width();
    assert!(
      width + 0.5 < 200.0,
      "expected max-content item width to be smaller than 200px, got {width:.2}"
    );

    let expected_x = 200.0 - width;
    assert!(
      (child.bounds.x() - expected_x).abs() < 1.0,
      "expected RTL start alignment to place item at x≈{expected_x:.2}, got {:.2} (width={width:.2})",
      child.bounds.x()
    );
  }

  #[test]
  fn grid_item_height_keyword_max_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |height_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(300.0))];
      grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.align_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.height = None;
      item_style.height_keyword = height_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(300.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let max_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let auto_height = auto_fragment.children[0].bounds.height();
    let max_height = max_fragment.children[0].bounds.height();
    assert!(
      (auto_height - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_height:.2}"
    );
    assert!(
      max_height + 0.5 < auto_height,
      "expected max-content grid item height ({max_height:.2}) to be smaller than stretched auto height ({auto_height:.2})",
    );
  }

  #[test]
  fn grid_item_height_keyword_fit_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |height_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(300.0))];
      grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.align_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.height = None;
      item_style.height_keyword = height_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(300.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let fit_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None })),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(fit_fragment.children.len(), 1);
    let auto_height = auto_fragment.children[0].bounds.height();
    let fit_height = fit_fragment.children[0].bounds.height();
    assert!(
      (auto_height - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_height:.2}"
    );
    assert!(
      fit_height + 0.5 < auto_height,
      "expected fit-content grid item height ({fit_height:.2}) to be smaller than stretched auto height ({auto_height:.2})",
    );
  }

  #[test]
  fn taffy_template_cache_reuses_grid_templates_with_equal_styles() {
    use crate::layout::taffy_integration::{
      taffy_grid_container_style_fingerprint, TaffyNodeCacheKey,
    };
    use taffy::TaffyTree;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(20.0)), GridTrack::Fr(1.0)];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.grid_column_gap = Length::px(8.0);
    grid_style.grid_row_gap = Length::px(4.0);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(10.0));
    item_style.height = Some(Length::px(10.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    item_style.grid_column_start = 1;
    item_style.grid_row_start = 1;

    let grid_style_a = Arc::new(grid_style.clone());
    let grid_style_b = Arc::new(grid_style);
    assert!(
      !Arc::ptr_eq(&grid_style_a, &grid_style_b),
      "expected distinct Arc pointers for grid container styles"
    );
    let item_style_a = Arc::new(item_style.clone());
    let item_style_b = Arc::new(item_style);
    assert!(
      !Arc::ptr_eq(&item_style_a, &item_style_b),
      "expected distinct Arc pointers for grid item styles"
    );

    let mut container_a = BoxNode::new_block(
      grid_style_a,
      FormattingContextType::Grid,
      vec![
        BoxNode::new_block(item_style_a.clone(), FormattingContextType::Block, vec![]),
        BoxNode::new_block(item_style_a, FormattingContextType::Block, vec![]),
      ],
    );
    container_a.id = 1;
    container_a.children[0].id = 10;
    container_a.children[1].id = 11;

    let mut container_b = BoxNode::new_block(
      grid_style_b,
      FormattingContextType::Grid,
      vec![
        BoxNode::new_block(item_style_b.clone(), FormattingContextType::Block, vec![]),
        BoxNode::new_block(item_style_b, FormattingContextType::Block, vec![]),
      ],
    );
    container_b.id = 2;
    container_b.children[0].id = 20;
    container_b.children[1].id = 21;

    let gc = GridFormattingContext::new();
    assert_eq!(gc.taffy_cache.template_count(), 0);

    let children_a: Vec<&BoxNode> = container_a.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_a = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Grid,
      taffy_grid_container_style_fingerprint(container_a.style.as_ref()),
      super::grid_child_fingerprint(&children_a, &mut deadline_counter).expect("fingerprint"),
      gc.viewport_size,
    );

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut positioned_children: FxHashMap<taffy::prelude::NodeId, Vec<*const BoxNode>> =
      FxHashMap::default();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    gc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_a,
      container_a.style.as_ref(),
      &children_a,
      &constraints,
      &mut positioned_children,
    )
    .expect("build taffy tree");

    assert_eq!(
      gc.taffy_cache.template_count(),
      1,
      "first build should insert a single cached template"
    );
    let template_a = gc
      .taffy_cache
      .get(&key_a)
      .expect("template should be cached after first build");

    let children_b: Vec<&BoxNode> = container_b.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_b = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Grid,
      taffy_grid_container_style_fingerprint(container_b.style.as_ref()),
      super::grid_child_fingerprint(&children_b, &mut deadline_counter).expect("fingerprint"),
      gc.viewport_size,
    );
    assert_eq!(
      key_a, key_b,
      "cache keys should match for identical style values regardless of ids/pointers"
    );

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut positioned_children: FxHashMap<taffy::prelude::NodeId, Vec<*const BoxNode>> =
      FxHashMap::default();
    gc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_b,
      container_b.style.as_ref(),
      &children_b,
      &constraints,
      &mut positioned_children,
    )
    .expect("build taffy tree");

    assert_eq!(
      gc.taffy_cache.template_count(),
      1,
      "second build should hit the template cache instead of inserting a new entry"
    );
    let template_b = gc
      .taffy_cache
      .get(&key_b)
      .expect("template should be cached after second build");
    assert!(
      Arc::ptr_eq(&template_a, &template_b),
      "expected second build to reuse existing cached template"
    );
  }

  #[test]
  fn grid_tree_build_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let children: Vec<BoxNode> = (0..64)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let container = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let gc = GridFormattingContext::new();
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    let mut positioned_children = FxHashMap::default();
    let result = gc.build_taffy_tree_children(
      &mut taffy,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut positioned_children,
    );

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_subgrid_tree_build_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = CssDisplay::Grid;
    subgrid_style.grid_row_subgrid = true;
    let subgrid_style = Arc::new(subgrid_style);

    let mut root = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    for _ in 0..(GRID_DEADLINE_CHECK_STRIDE + 16) {
      root = BoxNode::new_block(
        subgrid_style.clone(),
        FormattingContextType::Grid,
        vec![root],
      );
    }
    let container = root;
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let gc = GridFormattingContext::new();
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    let mut positioned_children = FxHashMap::default();
    let result = gc.build_taffy_tree_children(
      &mut taffy,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut positioned_children,
    );

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_baseline_traversal_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let leaf = FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), "x", 0.5);
    let mut fragment = leaf;
    // Ensure the baseline walk hits GRID_DEADLINE_CHECK_STRIDE checks before reaching the leaf.
    for _ in 0..(GRID_DEADLINE_CHECK_STRIDE + 16) {
      fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![fragment]);
    }

    let mut deadline_counter = 0usize;
    let result = find_first_baseline_offset(&fragment, &mut deadline_counter);
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_translate_fragment_tree_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let mut deadline_counter = 0usize;
    let mut fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![]);
    let mut result = Ok(());
    for _ in 0..(GRID_DEADLINE_CHECK_STRIDE + 16) {
      result = translate_fragment_tree(&mut fragment, Point::new(0.0, 0.0), &mut deadline_counter);
      if result.is_err() {
        break;
      }
    }
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_translate_fragment_tree_moves_root_only() {
    let child = FragmentNode::new_block(Rect::from_xywh(5.0, 7.0, 1.0, 1.0), vec![]);
    let mut fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![child]);

    let mut deadline_counter = 0usize;
    translate_fragment_tree(&mut fragment, Point::new(10.0, 5.0), &mut deadline_counter).unwrap();

    assert_eq!(fragment.bounds.x(), 10.0);
    assert_eq!(fragment.bounds.y(), 5.0);
    assert_eq!(fragment.children[0].bounds.x(), 5.0);
    assert_eq!(fragment.children[0].bounds.y(), 7.0);
  }

  #[test]
  fn grid_taffy_abort_surfaces_as_timeout() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Ensure the deadline is not tripped by the initial check, but is tripped during Taffy.
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let deadline = RenderDeadline::new(
      None,
      Some(Arc::new(move || {
        let prev = counter_clone.fetch_add(1, Ordering::SeqCst);
        prev == 1
      })),
    );
    let _guard = DeadlineGuard::install(Some(&deadline));

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let container = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let gc = GridFormattingContext::new();
    let result = gc.layout(&container, &constraints);

    match result {
      Err(LayoutError::Timeout { elapsed }) => {
        assert!(elapsed >= Duration::from_secs(0));
      }
      other => panic!("expected LayoutError::Timeout from Taffy abort, got {other:?}"),
    }
  }

  #[test]
  fn measure_key_quantizes_definite_sizes() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known_a = taffy::geometry::Size {
      width: Some(300.3),
      height: Some(150.7),
    };
    let avail_a = taffy::geometry::Size {
      width: AvailableSpace::Definite(500.2),
      height: AvailableSpace::Definite(600.4),
    };
    let known_b = taffy::geometry::Size {
      width: Some(300.6),
      height: Some(150.4),
    };
    let avail_b = taffy::geometry::Size {
      width: AvailableSpace::Definite(500.6),
      height: AvailableSpace::Definite(600.2),
    };

    let key_a = MeasureKey::new(&node, known_a, avail_a, viewport, false);
    let key_b = MeasureKey::new(&node, known_b, avail_b, viewport, false);
    assert_eq!(
      key_a, key_b,
      "near-identical definite sizes should quantize to the same key"
    );

    let intrinsic_known = taffy::geometry::Size {
      width: None,
      height: known_a.height,
    };
    let min_key = MeasureKey::new(
      &node,
      intrinsic_known,
      taffy::geometry::Size {
        width: AvailableSpace::MinContent,
        height: avail_a.height,
      },
      viewport,
      false,
    );
    let max_key = MeasureKey::new(
      &node,
      intrinsic_known,
      taffy::geometry::Size {
        width: AvailableSpace::MaxContent,
        height: avail_a.height,
      },
      viewport,
      false,
    );
    assert_ne!(
      min_key.available_width, max_key.available_width,
      "min/max-content probes should remain distinct when width is unknown"
    );
  }

  #[test]
  fn grid_measurement_is_deterministic_within_quantized_measure_key() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();

    let mut style = ComputedStyle::default();
    style.width = Some(Length::percent(50.0));
    let node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    let node_ptr = &node as *const _;
    let node_id = TaffyNodeId::from(1u64);
    let viewport = gc.viewport_size;

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();

    let avail_a = taffy::geometry::Size {
      width: AvailableSpace::Definite(100.4),
      height: AvailableSpace::MaxContent,
    };
    let avail_b = taffy::geometry::Size {
      width: AvailableSpace::Definite(100.6),
      height: AvailableSpace::MaxContent,
    };

    let key_a = MeasureKey::new(&node, known, avail_a, viewport, false);
    let key_b = MeasureKey::new(&node, known, avail_b, viewport, false);
    assert_eq!(
      key_a, key_b,
      "expected probes to map to the same quantized MeasureKey"
    );

    let size_a = {
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::default();
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
      gc.measure_grid_item(
        node_ptr,
        node_id,
        known,
        avail_a,
        None,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      )
    };

    let size_b = {
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::default();
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
      gc.measure_grid_item(
        node_ptr,
        node_id,
        known,
        avail_b,
        None,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      )
    };

    assert_eq!(
      size_a.size.width.to_bits(),
      size_b.size.width.to_bits(),
      "measured widths should match exactly after snapping to the quantized key"
    );
    assert_eq!(
      size_a.size.height.to_bits(),
      size_b.size.height.to_bits(),
      "measured heights should match exactly after snapping to the quantized key"
    );
  }

  #[test]
  fn measure_key_ignores_available_width_when_known_width_is_some() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: Some(200.0),
      height: None,
    };

    let key_a = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(10.0),
      },
      viewport,
      false,
    );
    let key_b = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(400.0),
        height: AvailableSpace::Definite(10.0),
      },
      viewport,
      false,
    );

    assert_eq!(key_a.available_width, MeasureAvailKey::Ignored);
    assert_eq!(key_b.available_width, MeasureAvailKey::Ignored);
    assert_eq!(
      key_a, key_b,
      "when Taffy supplies a known width, varying AvailableSpace::width should not change the key"
    );

    let key_c = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(50.0),
      },
      viewport,
      false,
    );
    assert_ne!(
      key_a, key_c,
      "unknown height should remain sensitive to AvailableSpace::height"
    );
  }

  #[test]
  fn measure_key_ignores_available_height_when_known_height_is_some() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: None,
      height: Some(200.0),
    };

    let key_a = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(10.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    let key_b = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(10.0),
        height: AvailableSpace::Definite(400.0),
      },
      viewport,
      false,
    );

    assert_eq!(key_a.available_height, MeasureAvailKey::Ignored);
    assert_eq!(key_b.available_height, MeasureAvailKey::Ignored);
    assert_eq!(
      key_a, key_b,
      "when Taffy supplies a known height, varying AvailableSpace::height should not change the key"
    );

    let key_c = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(50.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    assert_ne!(
      key_a, key_c,
      "unknown width should remain sensitive to AvailableSpace::width"
    );
  }

  #[test]
  fn grid_constraints_from_taffy_does_not_clamp_definite_width_to_viewport() {
    use taffy::style::AvailableSpace;

    let viewport = Size::new(200.0, 200.0);
    let known = taffy::geometry::Size {
      width: Some(1000.0),
      height: None,
    };
    let available = taffy::geometry::Size {
      width: AvailableSpace::Definite(1000.0),
      height: AvailableSpace::MaxContent,
    };

    let constraints = constraints_from_taffy(viewport, known, available, None);
    assert_eq!(
      constraints.available_width,
      CrateAvailableSpace::Definite(1000.0)
    );
  }

  #[test]
  fn measure_key_does_not_clamp_definite_widths_to_viewport() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);

    let known_a = taffy::geometry::Size {
      width: Some(2000.0),
      height: Some(100.0),
    };
    let known_b = taffy::geometry::Size {
      width: Some(3000.0),
      height: Some(100.0),
    };
    let avail_a = taffy::geometry::Size {
      width: AvailableSpace::Definite(2000.0),
      height: AvailableSpace::Definite(400.0),
    };
    let avail_b = taffy::geometry::Size {
      width: AvailableSpace::Definite(3000.0),
      height: AvailableSpace::Definite(400.0),
    };

    let key_a = MeasureKey::new(&node, known_a, avail_a, viewport, false);
    let key_b = MeasureKey::new(&node, known_b, avail_b, viewport, false);
    assert_ne!(
      key_a, key_b,
      "definite widths larger than the viewport should remain distinct cache keys"
    );
  }

  #[test]
  fn measure_key_treats_tiny_definite_available_space_as_indefinite() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    let zero_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(0.0),
        height: AvailableSpace::Definite(0.0),
      },
      viewport,
      false,
    );
    let one_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(1.0),
        height: AvailableSpace::Definite(1.0),
      },
      viewport,
      false,
    );
    assert_eq!(zero_key.available_width, MeasureAvailKey::Indefinite);
    assert_eq!(zero_key.available_height, MeasureAvailKey::Indefinite);
    assert_eq!(
      zero_key, one_key,
      "tiny definite available sizes should coalesce to the same key"
    );

    let two_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(2.0),
        height: AvailableSpace::Definite(2.0),
      },
      viewport,
      false,
    );
    assert_ne!(zero_key.available_width, two_key.available_width);
    assert_ne!(zero_key.available_height, two_key.available_height);
  }

  #[test]
  fn measured_node_keys_are_capped_per_node() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let node_ptr = &node as *const _;
    let node_id = TaffyNodeId::from(1u64);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();

    for i in 0..(MAX_MEASURED_KEYS_PER_NODE + 5) {
      let avail = taffy::geometry::Size {
        width: AvailableSpace::Definite(20.0 + (i as f32 * 10.0)),
        height: AvailableSpace::Definite(40.0 + (i as f32 * 5.0)),
      };
      let _ = gc.measure_grid_item(
        node_ptr,
        node_id,
        known,
        avail,
        None,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      );
    }

    let node_keys = measured_node_keys.get(&node_id).expect("measured keys");
    assert!(
      node_keys.len() <= MAX_MEASURED_KEYS_PER_NODE,
      "measured keys should be capped per node"
    );
    let first_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(20.0),
        height: AvailableSpace::Definite(40.0),
      },
      gc.viewport_size,
      false,
    );
    assert!(
      !node_keys.contains(&first_key),
      "oldest measured keys should be evicted when exceeding the cap"
    );
  }

  #[test]
  fn grid_measure_size_cache_policy_evicts_instead_of_clearing() {
    let mut cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let max_entries = 4;
    let eviction_batch = 2;
    let output: taffy::tree::MeasureOutput = taffy::geometry::Size {
      width: 1.0,
      height: 2.0,
    }
    .into();
    for i in 0..max_entries {
      let key = MeasureKey {
        box_id: i + 1,
        style_ptr: 0,
        override_fingerprint: None,
        known_width: None,
        known_height: None,
        available_width: MeasureAvailKey::Indefinite,
        available_height: MeasureAvailKey::Indefinite,
      };
      grid_measure_size_cache_store_with_policy(&mut cache, key, output, max_entries, eviction_batch);
    }
    assert_eq!(cache.len(), max_entries);

    let key = MeasureKey {
      box_id: 999,
      style_ptr: 0,
      override_fingerprint: None,
      known_width: None,
      known_height: None,
      available_width: MeasureAvailKey::Indefinite,
      available_height: MeasureAvailKey::Indefinite,
    };
    grid_measure_size_cache_store_with_policy(&mut cache, key, output, max_entries, eviction_batch);
    assert_eq!(cache.len(), max_entries - eviction_batch + 1);
    assert!(
      cache.len() > 1,
      "eviction should preserve cache reuse instead of clearing everything"
    );

    let before = cache.len();
    grid_measure_size_cache_store_with_policy(&mut cache, key, output, max_entries, eviction_batch);
    assert_eq!(
      cache.len(),
      before,
      "updating an existing key should not evict"
    );
  }

  #[test]
  fn grid_measure_quantization_limits_layout_calls() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let widths = [300.2, 300.6, 300.1, 300.8, 300.4];
    let heights = [150.7, 150.4, 150.9];

    let nodes: Vec<BoxNode> = (0..12)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for width in widths {
        for height in heights {
          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(width + (i as f32 * 0.01)),
            height: AvailableSpace::Definite(height),
          };
          let _ = gc.measure_grid_item(
            node_ptr,
            node_id,
            known,
            avail,
            None,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );
        }
      }
    }

    let calls = grid_measure_layout_calls();
    assert!(
      calls > 0 && calls <= nodes.len() * 3,
      "quantized keys should coalesce near-identical probes (calls={calls})"
    );
  }

  #[test]
  fn grid_measure_ignores_definite_available_height_when_safe() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let width = 300.0;
    let heights = [150.0, 400.0, 650.0];

    let nodes: Vec<BoxNode> = (0..12)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for height in heights {
        let avail = taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(height),
        };
        let _ = gc.measure_grid_item(
          node_ptr,
          node_id,
          known,
          avail,
          None,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
      }
    }

    let calls = grid_measure_layout_calls();
    assert_eq!(
      calls,
      nodes.len(),
      "available height differences should be ignored when measurement doesn't depend on them (calls={calls})"
    );
  }

  #[test]
  fn grid_measure_respects_definite_available_height_when_percentage_children_present() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let width = 300.0;
    let heights = [150.0, 400.0, 650.0];

    let mut child_style = ComputedStyle::default();
    child_style.height = Some(Length::percent(50.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let nodes: Vec<BoxNode> = (0..4)
      .map(|_| {
        BoxNode::new_block(
          make_item_style(),
          FormattingContextType::Block,
          vec![child.clone()],
        )
      })
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for height in heights {
        let avail = taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(height),
        };
        let _ = gc.measure_grid_item(
          node_ptr,
          node_id,
          known,
          avail,
          None,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
      }
    }

    let calls = grid_measure_layout_calls();
    assert_eq!(
      calls,
      nodes.len() * heights.len(),
      "percentage-height children should force distinct cache keys per definite height probe (calls={calls})"
    );
  }

  #[test]
  fn measure_grid_item_height_probes_respect_known_inline_size() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        gc.font_context.clone(),
        gc.viewport_size,
        gc.nearest_positioned_cb,
      );
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    for (probe_height, label) in [
      (AvailableSpace::MinContent, "min-content"),
      (AvailableSpace::MaxContent, "max-content"),
    ] {
      let mut style = ComputedStyle::default();
      style.font_size = 16.0;
      style.word_break = WordBreak::BreakAll;
      let style = Arc::new(style);
      let text_child = BoxNode::new_text(style.clone(), "a".repeat(64));
      let mut node = BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child]);
      node.id = 1;
      let node_ptr = &node as *const _;
      let taffy_style: taffy::style::Style = taffy::style::Style::default();

      let wide_width = gc.viewport_size.width;
      reset_grid_measure_layout_calls();
      let wide = gc.measure_grid_item(
        node_ptr,
        TaffyNodeId::from(1u64),
        taffy::geometry::Size {
          width: Some(wide_width),
          height: None,
        },
        taffy::geometry::Size {
          width: AvailableSpace::Definite(wide_width),
          height: probe_height,
        },
        Some(wide_width),
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      );
      assert!(
        wide.size.height > 0.0,
        "{label} height probe should return a non-zero intrinsic height"
      );
      assert_eq!(
        grid_measure_layout_calls(),
        1,
        "{label} height probe should measure by laying out the item with the known inline size"
      );

      let narrow_width = 50.0;
      reset_grid_measure_layout_calls();
      let narrow = gc.measure_grid_item(
        node_ptr,
        TaffyNodeId::from(1u64),
        taffy::geometry::Size {
          width: Some(narrow_width),
          height: None,
        },
        taffy::geometry::Size {
          width: AvailableSpace::Definite(narrow_width),
          height: probe_height,
        },
        Some(narrow_width),
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      );
      assert!(
        narrow.size.height > wide.size.height,
        "{label} height probe should respect the known inline size (narrow={:.2}, wide={:.2})",
        narrow.size.height,
        wide.size.height
      );
      assert_eq!(
        grid_measure_layout_calls(),
        1,
        "{label} height probe should measure by laying out the item with the known inline size"
      );
    }
  }

  #[test]
  fn grid_auto_rows_min_and_max_content_measure_text_height() {
    let gc = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite_width(320.0);

    for (track, label) in [
      (GridTrack::MinContent, "min-content"),
      (GridTrack::MaxContent, "max-content"),
    ] {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_auto_rows = vec![track].into();
      let grid_style = Arc::new(grid_style);

      let item = make_text_item(
        "This grid item should contribute a non-zero intrinsic height",
        16.0,
      );

      let tree = BoxTree::new(BoxNode::new_block(
        grid_style,
        FormattingContextType::Grid,
        vec![item],
      ));

      let fragment = gc.layout(&tree.root, &constraints).expect("grid layout");
      assert_eq!(fragment.children.len(), 1);
      assert!(
        fragment.children[0].bounds.height() > 0.0,
        "{label} auto row should be sized from inline text height"
      );
    }
  }

  #[test]
  fn convert_style_sets_overflow_and_scrollbar_width() {
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.overflow_x = Overflow::Scroll;
    style.overflow_y = Overflow::Clip;
    style.scrollbar_width = ScrollbarWidth::Thin;

    let node = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![]);
    let gc = GridFormattingContext::new();
    let taffy_style = gc.convert_style(&node.style, None, None, false, true);

    assert_eq!(taffy_style.overflow.x, TaffyOverflow::Scroll);
    assert_eq!(taffy_style.overflow.y, TaffyOverflow::Clip);
    assert_eq!(
      taffy_style.scrollbar_width,
      resolve_scrollbar_width(&node.style)
    );
  }

  #[test]
  fn convert_style_sets_axes_swapped_for_vertical_writing_modes() {
    let gc = GridFormattingContext::new();
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;

    style.writing_mode = WritingMode::HorizontalTb;
    let taffy_style = gc.convert_style(&style, None, None, false, true);
    assert!(
      !taffy_style.axes_swapped,
      "horizontal-tb should not transpose inline/block axes"
    );

    style.writing_mode = WritingMode::VerticalRl;
    let taffy_style = gc.convert_style(&style, None, None, false, true);
    assert!(
      taffy_style.axes_swapped,
      "vertical writing-modes should transpose inline/block axes into physical axes"
    );
  }

  #[test]
  fn convert_style_subgrids_use_own_writing_mode_for_axes_swapped() {
    let gc = GridFormattingContext::new();

    let mut parent_style = ComputedStyle::default();
    parent_style.display = CssDisplay::Grid;
    parent_style.writing_mode = WritingMode::HorizontalTb;
    let parent_axis = GridAxisStyle::from_style(&parent_style);

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = CssDisplay::Grid;
    subgrid_style.writing_mode = WritingMode::VerticalRl;
    subgrid_style.grid_column_subgrid = true;

    let taffy_style = gc.convert_style(
      &subgrid_style,
      Some(&parent_style),
      Some(parent_axis),
      false,
      true,
    );
    assert!(
      taffy_style.axes_swapped,
      "subgrids should transpose axes based on their own writing-mode"
    );

    parent_style.writing_mode = WritingMode::VerticalRl;
    let parent_axis = GridAxisStyle::from_style(&parent_style);
    subgrid_style.writing_mode = WritingMode::HorizontalTb;
    let taffy_style = gc.convert_style(
      &subgrid_style,
      Some(&parent_style),
      Some(parent_axis),
      false,
      true,
    );
    assert!(
      !taffy_style.axes_swapped,
      "horizontal-tb subgrids should not transpose axes even when the parent is vertical"
    );
  }

  #[test]
  fn simple_grids_use_block_fast_path() {
    // Grid with default implicit tracks and a single child should use the simple fast path.
    let mut parent = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Grid,
      vec![],
    );
    let child1 = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    parent.children.push(child1);
    parent.children.push(child2);

    let gc = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let _root_id = gc
      .build_taffy_tree(
        &mut TaffyTree::new(),
        &parent,
        parent.style.as_ref(),
        &constraints,
        &mut FxHashMap::default(),
      )
      .expect("grid conversion");
    // If the fast path is taken, the parent container style should have been converted to block.
    let taffy_style = gc.convert_style(&parent.style, None, None, true, true);
    assert_eq!(taffy_style.display, Display::Block);
  }

  // Test 1: Basic grid container creation
  #[test]
  fn test_grid_fc_creation() {
    let fc = GridFormattingContext::new();
    let default_fc = GridFormattingContext::default();
    assert_eq!(
      std::mem::size_of_val(&fc),
      std::mem::size_of_val(&default_fc)
    );
  }

  // Test 2: Empty grid layout
  #[test]
  fn test_empty_grid_layout() {
    let fc = GridFormattingContext::new();
    let box_node = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![]);
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = fc.layout(&box_node, &constraints).unwrap();
    assert!(fragment.bounds.width() >= 0.0);
    assert!(fragment.bounds.height() >= 0.0);
  }

  // Test 3: Grid with single child
  #[test]
  fn test_grid_single_child() {
    let fc = GridFormattingContext::new();

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  #[test]
  fn intrinsic_block_size_does_not_recurse_for_vertical_writing_mode_intrinsic_width() {
    let fc = GridFormattingContext::with_viewport(Size::new(200.0, 200.0));

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

    let mut child_style = ComputedStyle::default();
    child_style.width = Some(Length::px(40.0));
    child_style.height = Some(Length::px(20.0));
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Grid,
      vec![child],
    );

    let size = fc
      .compute_intrinsic_block_size(&container, IntrinsicSizingMode::MaxContent)
      .expect("compute intrinsic block size");

    assert!(size.is_finite() && size > 0.0);
  }

  // Test 4: Grid with multiple children
  #[test]
  fn test_grid_multiple_children() {
    let fc = GridFormattingContext::new();

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child3 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      make_grid_style(),
      FormattingContextType::Grid,
      vec![child1, child2, child3],
    );

    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 3);
  }

  #[test]
  fn measured_fragments_are_reused_without_cloning() {
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PROFILE_FRAGMENT_CLONES".to_string(),
        "1".to_string(),
      )]))),
      || {
        fragment_clone_profile::reset_fragment_clone_profile();

        let fc = GridFormattingContext::new();

        let mut item_style = ComputedStyle::default();
        item_style.font_size = 12.0;
        item_style.grid_column_start = 2;
        let item_style = Arc::new(item_style);
        let text_child = BoxNode::new_text(item_style.clone(), "reuse-me".to_string());
        let mut item = BoxNode::new_block(
          item_style.clone(),
          FormattingContextType::Inline,
          vec![text_child],
        );
        item.id = 2;

        let mut grid_style = ComputedStyle::default();
        grid_style.display = CssDisplay::Grid;
        grid_style.grid_template_columns =
          vec![GridTrack::Length(Length::px(50.0)), GridTrack::Auto];
        let grid_style = Arc::new(grid_style);
        let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
        grid.id = 1;

        let constraints = LayoutConstraints::definite(180.0, 100.0);

        let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
        let mut positioned_children_map: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> =
          FxHashMap::default();
        let in_flow_children: Vec<&BoxNode> = grid.children.iter().collect();
        let root_id = fc
          .build_taffy_tree_children(
            &mut taffy,
            &grid,
            grid.style.as_ref(),
            &in_flow_children,
            &constraints,
            &mut positioned_children_map,
          )
          .expect("build taffy tree");

        let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
          FxHashMap::default();
        let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
        let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

        let available_space = taffy::geometry::Size {
          width: taffy::style::AvailableSpace::Definite(constraints.width().unwrap()),
          height: taffy::style::AvailableSpace::Definite(constraints.height().unwrap()),
        };

        let factory = fc.factory.clone();
        let viewport_size = fc.viewport_size;
        let parent_inline_base = constraints.inline_percentage_base;
        let cache = &mut measure_cache;
        let measured = &mut measured_fragments;
        let measured_keys = &mut measured_node_keys;
        let this = fc.clone();
        taffy
          .compute_layout_with_measure(
            root_id,
            available_space,
              move |known_dimensions,
                    available_space,
                    node_id,
                    node_context,
                    _style: &taffy::style::Style| {
                if node_id == root_id {
                  let fallback_size =
                    |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
                    known.unwrap_or(match avail_dim {
                      taffy::style::AvailableSpace::Definite(v) => v,
                      _ => 0.0,
                    })
                  };
                return taffy::geometry::Size {
                  width: fallback_size(known_dimensions.width, available_space.width),
                  height: fallback_size(known_dimensions.height, available_space.height),
                }
                .into();
              }

              let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
                return taffy::tree::MeasureOutput::ZERO;
              };
              let box_node = unsafe { &*node_ptr };

              let key = MeasureKey::new(
                box_node,
                known_dimensions,
                available_space,
                viewport_size,
                false,
              );
              if let Some(size) = cache.get(&key) {
                return *size;
              }
              let fc_type = box_node
                .formatting_context()
                .unwrap_or(FormattingContextType::Block);
              let fc = factory.create(fc_type);

              let child_constraints = constraints_from_taffy(
                viewport_size,
                known_dimensions,
                available_space,
                parent_inline_base,
              );

              let mut fragment = match fc.layout(box_node, &child_constraints) {
                Ok(fragment) => fragment,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => return taffy::tree::MeasureOutput::ZERO,
              };
              let percentage_base = match available_space.width {
                taffy::style::AvailableSpace::Definite(w) => w,
                _ => child_constraints
                  .width()
                  .unwrap_or_else(|| fragment.bounds.width()),
              };
              fragment.content = FragmentContent::Block {
                box_id: Some(box_node.id),
              };
              fragment.style = Some(box_node.style.clone());
              let content_size = this.content_box_size(&fragment, &box_node.style, percentage_base);
              let size = taffy::geometry::Size {
                width: content_size.width.max(0.0),
                height: content_size.height.max(0.0),
              };
              let output = taffy::tree::MeasureOutput::from_size(size);
              measured_keys.entry(node_id).or_default().push(key);
              measured.insert(key, fragment);
              cache.insert(key, output);
              output
            },
          )
          .expect("taffy layout");

        let child_id = *taffy.children(root_id).unwrap().first().unwrap();
        let child_layout = taffy.layout(child_id).unwrap();
        let before_len = measured_fragments.len();
        assert!(before_len > 0, "expected measured fragments to be recorded");
        let matched_key = measured_node_keys
          .get(&child_id)
          .and_then(|keys| {
            keys.iter().copied().find(|key| {
              measured_fragments.get(key).map_or(false, |fragment| {
                (fragment.bounds.width() - child_layout.size.width).abs() < 0.1
                  && (fragment.bounds.height() - child_layout.size.height).abs() < 0.1
              })
            })
          })
          .expect("matching measured fragment should exist");

        let mut deadline_counter = 0usize;
        let mut fragment = fc
          .convert_to_fragments(
            &taffy,
            root_id,
            root_id,
            &constraints,
            None,
            None,
            &mut measured_fragments,
            &measured_node_keys,
            &positioned_children_map,
            &mut deadline_counter,
          )
          .expect("convert fragments");

        assert!(
          !measured_fragments.contains_key(&matched_key),
          "reused fragments should be removed from the cache"
        );
        assert!(
          measured_fragments.len() < before_len,
          "reusing a fragment should reduce the cache size"
        );
        assert_eq!(fragment.children.len(), 1);
        let child_fragment = fragment.children.pop().unwrap();
        match &child_fragment.content {
          FragmentContent::Block { box_id } => assert_eq!(*box_id, Some(2)),
          other => panic!("unexpected fragment content: {other:?}"),
        }
        assert!(child_fragment.style.is_some());
        assert!(
          (child_fragment.bounds.x() - child_layout.location.x).abs() < 0.01,
          "expected translated x to match layout"
        );
        assert!(
          (child_fragment.bounds.y() - child_layout.location.y).abs() < 0.01,
          "expected translated y to match layout"
        );

        fn has_text(node: &FragmentNode) -> bool {
          if matches!(node.content, FragmentContent::Text { .. }) {
            return true;
          }
          node.children.iter().any(has_text)
        }
        assert!(has_text(&child_fragment));

        let stats = fragment_clone_profile::fragment_clone_profile_stats();
        assert_eq!(
          stats.grid_measure_reuse.nodes, 0,
          "reuse should not record cloned nodes"
        );
        assert!(
          stats.grid_measure_reuse.events > 0,
          "reuse should be recorded when clone profiling is enabled"
        );
        fragment_clone_profile::reset_fragment_clone_profile();
      },
    );
  }

  // Test 5: Grid with explicit columns
  #[test]
  fn test_grid_explicit_columns() {
    let fc = GridFormattingContext::new();

    let style = make_grid_style_with_tracks(
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)],
      vec![],
    );

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  // Test 6: Grid with explicit rows
  #[test]
  fn test_grid_explicit_rows() {
    let fc = GridFormattingContext::new();

    let style = make_grid_style_with_tracks(
      vec![],
      vec![
        GridTrack::Length(Length::px(50.0)),
        GridTrack::Length(Length::px(100.0)),
      ],
    );

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(400.0, 300.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  #[test]
  fn absolute_child_inherits_positioned_cb_into_grid() {
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = CssDisplay::Block;
    abs_style.position = crate::style::position::Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    abs_style.width = Some(Length::px(12.0));
    abs_style.height = Some(Length::px(9.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![abs_child],
    );

    let viewport = crate::geometry::Size::new(300.0, 300.0);
    let cb_rect = crate::geometry::Rect::from_xywh(20.0, 30.0, 150.0, 150.0);
    let cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport(cb_rect, viewport);
    let fc = GridFormattingContext::with_viewport_and_cb(
      viewport,
      cb,
      crate::text::font_loader::FontContext::new(),
    );
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let abs_fragment = &fragment.children[0];
    assert_eq!(abs_fragment.bounds.x(), 25.0);
    assert_eq!(abs_fragment.bounds.y(), 37.0);
    assert_eq!(abs_fragment.bounds.width(), 12.0);
    assert_eq!(abs_fragment.bounds.height(), 9.0);
  }

  #[test]
  fn subgrid_content_visibility_establishes_positioned_cb_for_absolute_children() {
    fn find_by_box_id<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(box_id) {
        return Some(node);
      }
      node
        .children
        .iter()
        .find_map(|child| find_by_box_id(child, box_id))
    }

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = CssDisplay::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.content_visibility = ContentVisibility::Auto;
    apply_content_visibility_implied_containment(&mut subgrid_style);

    let mut abs_style = ComputedStyle::default();
    abs_style.display = CssDisplay::Block;
    abs_style.position = crate::style::position::Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(10.0));

    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = 3;

    let mut subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![abs_child],
    );
    subgrid.id = 2;

    let mut grid = BoxNode::new_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );
    grid.id = 1;

    let viewport = crate::geometry::Size::new(300.0, 300.0);
    let cb_rect = crate::geometry::Rect::from_xywh(20.0, 30.0, 150.0, 150.0);
    let cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport(cb_rect, viewport);
    let fc = GridFormattingContext::with_viewport_and_cb(
      viewport,
      cb,
      crate::text::font_loader::FontContext::new(),
    )
    .with_parallelism(LayoutParallelism::disabled());
    let constraints = LayoutConstraints::definite(50.0, 50.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let subgrid_fragment = find_by_box_id(&fragment, 2).expect("subgrid fragment should exist");
    let abs_fragment = subgrid_fragment
      .children
      .iter()
      .find(|child| child.box_id() == Some(3))
      .expect("absolute child fragment should be nested under subgrid fragment");

    // Content-visibility's implied containment should establish the containing block for the
    // absolutely positioned child. This prevents it from being offset by the outer positioned CB.
    assert_eq!(abs_fragment.bounds.x(), 0.0);
    assert_eq!(abs_fragment.bounds.y(), 0.0);
  }

  // Test 7: Grid with gap
  #[test]
  fn test_grid_with_gap() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_column_gap = Length::px(10.0);
    style.grid_row_gap = Length::px(20.0);
    style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    let style = Arc::new(style);

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(410.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  // Test 8: Grid with multiple rows
  #[test]
  fn test_grid_multiple_rows() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    let style = Arc::new(style);

    let children: Vec<BoxNode> = (0..4)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 4);
  }

  // Test 9: Grid item placement with line numbers
  #[test]
  fn test_grid_item_placement() {
    let fc = GridFormattingContext::new();

    let grid_style = make_grid_style_with_tracks(
      vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)],
      vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)],
    );

    let mut item_style = ComputedStyle::default();
    item_style.grid_column_start = 2;
    item_style.grid_row_start = 1;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  // Test 10: Intrinsic sizing - min content
  #[test]
  fn test_intrinsic_min_content() {
    let fc = GridFormattingContext::new();

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(30.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let size = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();

    assert!(size >= 0.0);
  }

  // Test 11: Intrinsic sizing - max content
  #[test]
  fn test_intrinsic_max_content() {
    let fc = GridFormattingContext::new();

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(100.0));
    item_style.height = Some(Length::px(50.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let size = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MaxContent)
      .unwrap();

    assert!(size >= 0.0);
  }

  #[test]
  fn grid_intrinsic_inline_size_reuses_intrinsic_cache() {
    let _lock = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::intrinsic_cache_clear();

    let _taffy_guard = crate::layout::taffy_integration::enable_taffy_counters(true);
    crate::layout::taffy_integration::reset_taffy_counters();

    let fc = GridFormattingContext::new();
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let mut grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);
    grid.id = 1;
    grid.children[0].id = 2;

    let first = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();
    assert_eq!(
      crate::layout::taffy_integration::taffy_counters().grid,
      1,
      "expected first intrinsic call to invoke Taffy"
    );

    let second = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();
    assert_eq!(second, first);
    assert_eq!(
      crate::layout::taffy_integration::taffy_counters().grid,
      1,
      "expected repeated intrinsic calls to reuse intrinsic_cache without re-running Taffy"
    );
  }

  #[test]
  fn grid_intrinsic_size_does_not_double_count_padding_and_border() {
    let fc = GridFormattingContext::new();

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(100.0));
    item_style.width_keyword = None;
    item_style.padding_left = Length::px(10.0);
    item_style.padding_right = Length::px(10.0);
    item_style.border_left_style = BorderStyle::Solid;
    item_style.border_right_style = BorderStyle::Solid;
    item_style.border_left_width = Length::px(5.0);
    item_style.border_right_width = Length::px(5.0);
    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      make_grid_style_with_tracks(vec![GridTrack::Auto], vec![GridTrack::Auto]),
      FormattingContextType::Grid,
      vec![child],
    );

    let expected = 100.0 + 10.0 + 10.0 + 5.0 + 5.0;
    let min = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();
    let max = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MaxContent)
      .unwrap();

    assert!(
      (min - expected).abs() < 0.01,
      "min-content intrinsic width should match the grid item's border-box width (got {:.2}, expected {:.2})",
      min,
      expected
    );
    assert!(
      (max - expected).abs() < 0.01,
      "max-content intrinsic width should match the grid item's border-box width (got {:.2}, expected {:.2})",
      max,
      expected
    );
  }

  // Test 12: Grid with minmax track
  #[test]
  fn test_grid_minmax_track() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::MinMax(
      Box::new(GridTrack::Length(Length::px(100.0))),
      Box::new(GridTrack::Fr(1.0)),
    )];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(500.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  // Test 13: Grid indefinite constraints
  #[test]
  fn test_grid_indefinite_constraints() {
    let fc = GridFormattingContext::new();

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::indefinite();
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert!(fragment.bounds.width() >= 0.0);
    assert!(fragment.bounds.height() >= 0.0);
  }

  // Test 14: Grid with align-content
  #[test]
  fn test_grid_align_content() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.align_content = AlignContent::Center;
    style.grid_template_columns = vec![GridTrack::Fr(1.0)];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(400.0, 400.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  #[test]
  fn grid_justify_items_centers_children() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
    style.justify_items = AlignItems::Center;
    style.align_items = AlignItems::FlexStart;
    let style = Arc::new(style);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(20.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.x(), 75.0);
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
  }

  #[test]
  fn grid_align_self_and_justify_self_override_container_alignment() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
    style.justify_items = AlignItems::FlexStart;
    style.align_items = AlignItems::FlexStart;
    let style = Arc::new(style);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(30.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    item_style.justify_self = Some(AlignItems::FlexEnd);
    item_style.align_self = Some(AlignItems::FlexEnd);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.x(), 150.0);
    assert_eq!(fragment.children[0].bounds.y(), 70.0);
  }

  #[test]
  fn grid_item_aspect_ratio_sets_height_from_width() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.align_items = AlignItems::FlexStart;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(80.0));
    item_style.width_keyword = None;
    item_style.aspect_ratio = AspectRatio::Ratio(2.0);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.width(), 80.0);
    assert_eq!(fragment.children[0].bounds.height(), 40.0);
  }

  #[test]
  fn grid_item_aspect_ratio_sets_width_from_height() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.align_items = AlignItems::FlexStart;
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(60.0))];
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.height = Some(Length::px(60.0));
    item_style.height_keyword = None;
    item_style.aspect_ratio = AspectRatio::Ratio(1.5);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.height(), 60.0);
    assert_eq!(fragment.children[0].bounds.width(), 90.0);
  }

  // Test 15: Grid with nested grid
  #[test]
  fn test_nested_grid() {
    let fc = GridFormattingContext::new();

    let inner_child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let inner_grid = BoxNode::new_block(
      make_grid_style(),
      FormattingContextType::Grid,
      vec![inner_child],
    );
    let outer_grid = BoxNode::new_block(
      make_grid_style(),
      FormattingContextType::Grid,
      vec![inner_grid],
    );

    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&outer_grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    assert_eq!(fragment.children[0].children.len(), 1);
  }

  // Test 16: FormattingContext trait is Send + Sync
  #[test]
  fn test_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<GridFormattingContext>();
  }

  // Test 17: Grid with percentage widths
  #[test]
  fn test_grid_percentage_track() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::percent(50.0)),
      GridTrack::Length(Length::percent(50.0)),
    ];
    let style = Arc::new(style);

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  // Test 18: Grid auto track
  #[test]
  fn test_grid_auto_track() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Auto];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  #[test]
  fn vertical_writing_mode_swaps_tracks_for_template_sizes() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(20.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(30.0))];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    // Inline axis vertical: column tracks become rows (height = 20+20), row tracks become columns (width = 30).
    assert_eq!(fragment.bounds.width(), 30.0);
    assert_eq!(fragment.bounds.height(), 40.0);
    assert_eq!(fragment.children[0].bounds.width(), 30.0);
    assert_eq!(fragment.children[0].bounds.height(), 20.0);
  }

  #[test]
  fn vertical_writing_mode_swaps_placements_to_physical_axes() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(30.0)),
      GridTrack::Length(Length::px(40.0)),
    ];
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(200.0)),
    ];
    let style = Arc::new(style);

    let mut inline_item_style = ComputedStyle::default();
    inline_item_style.grid_column_start = 2;
    inline_item_style.grid_column_end = 3;
    inline_item_style.grid_row_start = 1;
    inline_item_style.grid_row_end = 2;
    let inline_item = BoxNode::new_block(
      Arc::new(inline_item_style),
      FormattingContextType::Block,
      vec![],
    );

    let mut block_item_style = ComputedStyle::default();
    block_item_style.grid_row_start = 2;
    block_item_style.grid_row_end = 3;
    block_item_style.grid_column_start = 1;
    block_item_style.grid_column_end = 2;
    let block_item = BoxNode::new_block(
      Arc::new(block_item_style),
      FormattingContextType::Block,
      vec![],
    );

    let grid = BoxNode::new_block(
      style,
      FormattingContextType::Grid,
      vec![inline_item, block_item],
    );

    let constraints = LayoutConstraints::definite(500.0, 500.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    // Tracks transpose: block tracks become Taffy columns (width), inline tracks become Taffy rows (height).
    assert_eq!(fragment.bounds.width(), 300.0);
    assert_eq!(fragment.bounds.height(), 70.0);

    // grid-column maps to the inline axis (vertical), so it should affect y/height.
    assert_eq!(fragment.children[0].bounds.x(), 200.0);
    assert_eq!(fragment.children[0].bounds.y(), 30.0);
    assert_eq!(fragment.children[0].bounds.width(), 100.0);
    assert_eq!(fragment.children[0].bounds.height(), 40.0);

    // grid-row maps to the block axis (horizontal), so it should affect x/width.
    assert_eq!(fragment.children[1].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.width(), 200.0);
    assert_eq!(fragment.children[1].bounds.height(), 30.0);
  }

  #[test]
  fn vertical_writing_mode_row_autoflow_fills_inline_axis_first() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_auto_flow = GridAutoFlow::Row;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(20.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(40.0)),
      GridTrack::Length(Length::px(40.0)),
    ];
    let style = Arc::new(style);

    let children: Vec<_> = (0..3)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 80.0);
    assert_eq!(fragment.bounds.height(), 40.0);

    // Row auto-flow maps to column auto-flow when inline is vertical: fill inline tracks top→bottom, then start a new block track.
    assert_eq!(fragment.children[0].bounds.x(), 40.0);
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 40.0);
    assert_eq!(fragment.children[1].bounds.y(), 20.0);
    assert_eq!(fragment.children[2].bounds.x(), 0.0);
    assert_eq!(fragment.children[2].bounds.y(), 0.0);
  }

  #[test]
  fn vertical_writing_mode_column_autoflow_fills_block_axis_first() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_auto_flow = GridAutoFlow::Column;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(20.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(40.0)),
      GridTrack::Length(Length::px(40.0)),
    ];
    let style = Arc::new(style);

    let children: Vec<_> = (0..3)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 80.0);
    assert_eq!(fragment.bounds.height(), 40.0);

    // Column auto-flow maps to row auto-flow when inline is vertical: fill block tracks first, then wrap inline.
    assert_eq!(fragment.children[0].bounds.x(), 40.0);
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 0.0);
    assert_eq!(fragment.children[2].bounds.x(), 40.0);
    assert_eq!(fragment.children[2].bounds.y(), 20.0);
  }

  #[test]
  fn test_grid_fixed_and_fr() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Fr(1.0),
      GridTrack::Fr(2.0),
    ];
    let style = Arc::new(style);

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child3 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      style,
      FormattingContextType::Grid,
      vec![child1, child2, child3],
    );

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 3);
  }

  #[test]
  fn grid_reuses_normalized_measured_fragments() {
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child_id = child.id;
    let grid = BoxNode::new_block(
      make_grid_style_with_tracks(vec![GridTrack::Auto], vec![GridTrack::Auto]),
      FormattingContextType::Grid,
      vec![child],
    );

    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fc = GridFormattingContext::new();
    let _guard = set_grid_test_measure_hook(move |node| {
      (node.id == child_id)
        .then(|| FragmentNode::new_block(Rect::from_xywh(5.0, 7.0, 30.0, 10.0), vec![]))
    });

    let fragment = fc.layout(&grid, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_fragment = &fragment.children[0];
    assert_eq!(child_fragment.bounds.x(), 0.0);
    assert_eq!(child_fragment.bounds.y(), 0.0);
    assert_eq!(child_fragment.bounds.width(), 30.0);
    assert_eq!(child_fragment.bounds.height(), 10.0);
  }

  #[test]
  fn parses_named_line_with_integer_in_any_order() {
    let placement = parse_grid_line_placement_raw("foo 2");
    match placement.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, "foo");
        assert_eq!(idx, 2);
      }
      other => panic!("expected named line, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("2 foo");
    match placement_rev.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, "foo");
        assert_eq!(idx, 2);
      }
      other => panic!("expected named line, got {:?}", other),
    }
  }

  #[test]
  fn parses_grid_line_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let placement = parse_grid_line_placement_raw(&format!("{nbsp}auto"));
    match placement.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, format!("{nbsp}auto"));
        assert_eq!(idx, 1);
      }
      other => panic!("expected named line, got {:?}", other),
    }
  }

  #[test]
  fn parses_span_with_integer_in_any_order() {
    let placement = parse_grid_line_placement_raw("2 span");
    match placement.start {
      TaffyGridPlacement::Span(count) => assert_eq!(count, 2),
      other => panic!("expected span, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("span 2");
    match placement_rev.start {
      TaffyGridPlacement::Span(count) => assert_eq!(count, 2),
      other => panic!("expected span, got {:?}", other),
    }
  }

  #[test]
  fn parses_named_span_in_any_order() {
    let placement = parse_grid_line_placement_raw("span foo 3");
    match placement.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("span 3 foo");
    match placement_rev.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement = parse_grid_line_placement_raw("foo span");
    match placement.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 1);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement = parse_grid_line_placement_raw("foo 3 span");
    match placement.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("3 foo span");
    match placement_rev.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }
  }

  // Test 20: Grid with both row and column gaps
  #[test]
  fn test_grid_both_gaps() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    style.grid_template_rows = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    style.grid_column_gap = Length::px(10.0);
    style.grid_row_gap = Length::px(10.0);
    let style = Arc::new(style);

    let children: Vec<BoxNode> = (0..4)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(410.0, 210.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 4);
  }

  #[test]
  fn grid_align_items_baseline_aligns_children() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.align_items = AlignItems::Baseline;
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(140.0)),
      GridTrack::Length(Length::px(140.0)),
    ];
    let grid_style = Arc::new(grid_style);

    let item_small = make_text_item("small", 14.0);
    let item_large = make_text_item("large", 28.0);

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_small, item_large],
    );

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let mut deadline_counter = 0usize;
    let baseline0_offset =
      super::first_baseline_offset(&fragment.children[0], &mut deadline_counter)
        .expect("baseline computation")
        .expect("baseline");
    let baseline0 = fragment.children[0].bounds.y() + baseline0_offset;
    let mut deadline_counter = 0usize;
    let baseline1_offset =
      super::first_baseline_offset(&fragment.children[1], &mut deadline_counter)
        .expect("baseline computation")
        .expect("baseline");
    let baseline1 = fragment.children[1].bounds.y() + baseline1_offset;

    assert!(
      (baseline0 - baseline1).abs() < 0.05,
      "baselines should align: {:.2} vs {:.2}",
      baseline0,
      baseline1
    );
    assert!(
      fragment.children[0].bounds.y() > fragment.children[1].bounds.y(),
      "smaller baseline item should be offset to align"
    );
  }

  #[test]
  fn grid_justify_items_baseline_aligns_columns() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.justify_items = AlignItems::Baseline;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(180.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
    let grid_style = Arc::new(grid_style);

    let item_writing_mode = WritingMode::VerticalLr;
    let fixed_width = 60.0;
    let item_small =
      make_text_item_with_writing_mode("small", 12.0, item_writing_mode, fixed_width);
    let item_large =
      make_text_item_with_writing_mode("large", 24.0, item_writing_mode, fixed_width);

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_small, item_large],
    );

    let constraints = LayoutConstraints::definite(300.0, 300.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let mut deadline_counter = 0usize;
    let baseline_offset0 = super::first_baseline_offset_x(
      &fragment.children[0],
      item_writing_mode,
      &mut deadline_counter,
    )
    .expect("baseline computation")
    .expect("baseline");
    let mut deadline_counter = 0usize;
    let baseline_offset1 = super::first_baseline_offset_x(
      &fragment.children[1],
      item_writing_mode,
      &mut deadline_counter,
    )
    .expect("baseline computation")
    .expect("baseline");

    let baseline0 = fragment.children[0].bounds.x() + baseline_offset0;
    let baseline1 = fragment.children[1].bounds.x() + baseline_offset1;

    assert!(
      (baseline0 - baseline1).abs() < 0.05,
      "inline-axis baselines should align: {:.2} vs {:.2}",
      baseline0,
      baseline1
    );
    assert!(
      fragment.children[0].bounds.x() >= 0.0 || fragment.children[1].bounds.x() >= 0.0,
      "baseline alignment should not move items outside the column"
    );
    assert!(
      fragment.children[0].bounds.x() > fragment.children[1].bounds.x(),
      "smaller baseline item should be offset horizontally to align"
    );
  }

  #[test]
  fn taffy_perf_counters_record_grid_measure_and_compute_time() {
    let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
    let mut renderer = FastRender::with_config(config).expect("renderer");
    let html = r#"<!doctype html>
      <html>
        <body>
          <div style="display:grid">
            <div>hello</div>
          </div>
        </body>
      </html>"#;
    let options = RenderOptions::default()
      .with_viewport(200, 200)
      .with_diagnostics_level(DiagnosticsLevel::Basic);
    let result = renderer
      .render_html_with_diagnostics(html, options)
      .expect("render");
    let stats = result
      .diagnostics
      .stats
      .as_ref()
      .expect("diagnostics stats should be captured");
    let measure_calls = stats
      .layout
      .taffy_grid_measure_calls
      .expect("grid measure call count should be recorded");
    assert!(measure_calls > 0, "expected grid measure calls > 0");
    let compute_ms = stats
      .layout
      .taffy_grid_compute_cpu_ms
      .expect("grid compute_cpu_ms should be recorded");
    assert!(compute_ms >= 0.0, "expected grid compute_cpu_ms >= 0");
  }
}
