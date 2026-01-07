//! Flexbox Formatting Context (via Taffy)
//!
//! This module implements the Flexbox layout algorithm by delegating to the Taffy library.
//! Taffy is a battle-tested layout library that implements the CSS Flexbox specification.
//!
//! # Design
//!
//! The FlexFormattingContext acts as a thin wrapper around Taffy's flexbox implementation:
//! 1. Convert BoxNode tree to Taffy tree (with Taffy styles)
//! 2. Run Taffy's `compute_layout()` algorithm
//! 3. Convert Taffy's layout results back to FragmentNode tree
//!
//! # Why Taffy?
//!
//! - Complete CSS Flexbox spec compliance
//! - Well-tested against Web Platform Tests
//! - Active maintenance by Dioxus team
//! - Saves months of implementation time
//!
//! # References
//!
//! - CSS Flexible Box Layout Module Level 1: <https://www.w3.org/TR/css-flexbox-1/>
//! - Taffy documentation: <https://docs.rs/taffy/>

use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::layout::absolute_positioning::resolve_positioned_style;
use crate::layout::absolute_positioning::resolve_positioned_style_with_anchors;
use crate::layout::absolute_positioning::AbsoluteLayout;
use crate::layout::absolute_positioning::AbsoluteLayoutInput;
use crate::layout::axis::{FragmentAxes, PhysicalAxis};
use crate::layout::constraints::AvailableSpace as CrateAvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::contexts::factory::FormattingContextFactory;
use crate::layout::contexts::flex_cache::{
  find_layout_cache_fragment, FlexCacheEntry, FlexCacheKey, ShardedFlexCache,
};
use crate::layout::contexts::positioned::ContainingBlock;
use crate::layout::contexts::positioned::PositionedLayout;
use crate::layout::engine::LayoutParallelism;
use crate::layout::flex_profile::record_node_measure_hit;
use crate::layout::flex_profile::record_node_measure_store;
use crate::layout::flex_profile::DimState;
use crate::layout::flex_profile::{self};
use crate::layout::formatting_context::count_flex_intrinsic_call;
use crate::layout::formatting_context::intrinsic_block_cache_lookup;
use crate::layout::formatting_context::intrinsic_block_cache_store;
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
use crate::layout::taffy_integration::{
  record_taffy_compute, record_taffy_invocation, record_taffy_measure_call,
  record_taffy_node_cache_hit, record_taffy_node_cache_miss, record_taffy_style_cache_hit,
  record_taffy_style_cache_miss, taffy_flex_style_fingerprint, taffy_template_cache_limit,
  CachedTaffyTemplate, SendSyncStyle, TaffyAdapterKind, TaffyNodeCache, TaffyNodeCacheKey,
  TAFFY_ABORT_CHECK_STRIDE,
};
use crate::layout::utils::resolve_length_with_percentage_metrics;
use crate::layout::utils::resolve_scrollbar_width;
use crate::render_control::{
  active_deadline, active_stage, check_active, check_active_periodic, with_deadline, StageGuard,
};
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::position::Position;
use crate::style::types::AlignContent;
use crate::style::types::AlignItems;
use crate::style::types::AspectRatio;
use crate::style::types::BoxSizing;
use crate::style::types::Direction;
use crate::style::types::FlexBasis;
use crate::style::types::FlexDirection;
use crate::style::types::FlexWrap;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::JustifyContent;
use crate::style::types::Overflow as CssOverflow;
use crate::style::types::WritingMode;
use crate::style::values::CalcLength;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::style::ComputedStyle;
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::{error::RenderError, error::RenderStage};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::hash::Hash;
use std::hash::Hasher;
use std::mem;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

static LOG_CHILD_IDS: std::sync::OnceLock<Vec<usize>> = std::sync::OnceLock::new();

thread_local! {
  /// Memoized results for flexbox's content-based automatic minimum size computation.
  ///
  /// The Taffy template cache memoizes style → Taffy conversion, but flex auto minimum sizing
  /// (`min-width/height:auto` on flex items) is content-dependent. Still, within a single layout run
  /// the content size suggestion for a box is stable, and it is frequently requested repeatedly as
  /// flex/grid layout re-enters nested formatting contexts.
  ///
  /// Scope this cache to the intrinsic/layout cache epoch so we do not retain stale pointers across
  /// renders.
  static FLEX_AUTO_MIN_CACHE_EPOCH: Cell<usize> = const { Cell::new(0) };
  static FLEX_AUTO_MIN_CACHE: RefCell<FxHashMap<(usize, usize, bool, bool), Option<f32>>> =
    RefCell::new(FxHashMap::default());
}

const FLEX_AUTO_MIN_CACHE_MAX_ENTRIES: usize = 32_768;

#[inline]
fn ensure_flex_auto_min_cache_epoch() {
  let epoch = crate::layout::formatting_context::intrinsic_cache_epoch();
  FLEX_AUTO_MIN_CACHE_EPOCH.with(|cell| {
    if cell.get() == epoch {
      return;
    }
    cell.set(epoch);
    FLEX_AUTO_MIN_CACHE.with(|cache| cache.borrow_mut().clear());
  });
}

#[inline]
fn flex_auto_min_cache_lookup(
  box_id: usize,
  style_ptr: usize,
  main_axis_is_horizontal: bool,
  skip_contents: bool,
) -> Option<Option<f32>> {
  if box_id == 0 {
    return None;
  }
  ensure_flex_auto_min_cache_epoch();
  FLEX_AUTO_MIN_CACHE.with(|cache| {
    cache
      .borrow()
      .get(&(box_id, style_ptr, main_axis_is_horizontal, skip_contents))
      .cloned()
  })
}

#[inline]
fn flex_auto_min_cache_store(
  box_id: usize,
  style_ptr: usize,
  main_axis_is_horizontal: bool,
  skip_contents: bool,
  value: Option<f32>,
) {
  if box_id == 0 {
    return;
  }
  ensure_flex_auto_min_cache_epoch();
  FLEX_AUTO_MIN_CACHE.with(|cache| {
    let mut map = cache.borrow_mut();
    if map.len() >= FLEX_AUTO_MIN_CACHE_MAX_ENTRIES
      && !map.contains_key(&(box_id, style_ptr, main_axis_is_horizontal, skip_contents))
    {
      map.clear();
    }
    map.insert((box_id, style_ptr, main_axis_is_horizontal, skip_contents), value);
  });
}

#[cfg(test)]
thread_local! {
  // Sorting can happen during many unrelated tests, so keep this counter thread-local to avoid
  // cross-test races when the test runner executes tests in parallel.
  static FLEX_ORDER_SORT_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_flex_order_sort_calls() {
  FLEX_ORDER_SORT_CALLS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn flex_order_sort_calls() -> usize {
  FLEX_ORDER_SORT_CALLS.with(|cell| cell.get())
}

#[cfg(test)]
fn record_flex_order_sort_call() {
  FLEX_ORDER_SORT_CALLS.with(|cell| cell.set(cell.get() + 1));
}

use taffy::prelude::*;
use taffy::style::Overflow as TaffyOverflow;
use taffy::TaffyTree;

type FingerprintHasher = FxHasher;

#[cfg(test)]
thread_local! {
  static FLEX_MEASURE_INTRINSIC_INLINE_HINT_COUNTING: Cell<bool> = const { Cell::new(false) };
  static FLEX_MEASURE_INTRINSIC_INLINE_HINT_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
struct FlexMeasureInlineHintCounterGuard;

#[cfg(test)]
impl Drop for FlexMeasureInlineHintCounterGuard {
  fn drop(&mut self) {
    FLEX_MEASURE_INTRINSIC_INLINE_HINT_COUNTING.with(|flag| flag.set(false));
  }
}

#[cfg(test)]
fn start_flex_measure_inline_hint_counter() -> FlexMeasureInlineHintCounterGuard {
  FLEX_MEASURE_INTRINSIC_INLINE_HINT_CALLS.with(|cell| cell.set(0));
  FLEX_MEASURE_INTRINSIC_INLINE_HINT_COUNTING.with(|flag| flag.set(true));
  FlexMeasureInlineHintCounterGuard
}

#[cfg(test)]
fn flex_measure_inline_hint_calls() -> usize {
  FLEX_MEASURE_INTRINSIC_INLINE_HINT_CALLS.with(|cell| cell.get())
}

#[cfg(test)]
fn record_flex_measure_inline_hint_call() {
  FLEX_MEASURE_INTRINSIC_INLINE_HINT_COUNTING.with(|enabled| {
    if enabled.get() {
      FLEX_MEASURE_INTRINSIC_INLINE_HINT_CALLS.with(|cell| cell.set(cell.get() + 1));
    }
  });
}

#[cfg(test)]
thread_local! {
  // Track how often the flex measure callback falls back to the expensive `FormattingContext::layout`
  // path for each flex item.
  static FLEX_MEASURE_LAYOUT_CALLS: RefCell<FxHashMap<usize, usize>> =
    RefCell::new(FxHashMap::default());
}

#[cfg(test)]
fn reset_flex_measure_layout_calls() {
  FLEX_MEASURE_LAYOUT_CALLS.with(|map| map.borrow_mut().clear());
}

#[cfg(test)]
fn flex_measure_layout_calls_for(box_id: usize) -> usize {
  FLEX_MEASURE_LAYOUT_CALLS.with(|map| map.borrow().get(&box_id).copied().unwrap_or(0))
}

#[cfg(test)]
fn flex_measure_layout_total_calls() -> usize {
  FLEX_MEASURE_LAYOUT_CALLS.with(|map| map.borrow().values().sum())
}

#[cfg(test)]
fn record_flex_measure_layout_call(box_id: usize) {
  FLEX_MEASURE_LAYOUT_CALLS.with(|map| {
    let mut map = map.borrow_mut();
    *map.entry(box_id).or_insert(0) += 1;
  });
}

fn translate_fragment_tree(
  fragment: &mut FragmentNode,
  delta: Point,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  check_layout_deadline(deadline_counter)?;
  crate::tree::fragment_tree::record_fragment_traversal(1);
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

fn subtree_contains_content_visibility_auto(
  root: &BoxNode,
  deadline_counter: &mut usize,
) -> Result<bool, LayoutError> {
  let mut stack: Vec<&BoxNode> = vec![root];
  while let Some(node) = stack.pop() {
    check_layout_deadline(deadline_counter)?;
    if matches!(
      node.style.content_visibility,
      crate::style::types::ContentVisibility::Auto
    ) {
      return Ok(true);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  Ok(false)
}

fn normalize_fragment_origin(
  fragment: &mut FragmentNode,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  let origin = fragment.bounds.origin;
  if origin.x != 0.0 || origin.y != 0.0 {
    translate_fragment_tree(fragment, Point::new(-origin.x, -origin.y), deadline_counter)?;
  }
  Ok(())
}

const FLEX_DEADLINE_CHECK_STRIDE: usize = 64;
const FLEX_CONTENT_VISIBILITY_AUTO_MAX_PASSES: usize = 4;

#[inline]
fn check_layout_deadline(counter: &mut usize) -> Result<(), LayoutError> {
  if let Err(RenderError::Timeout { elapsed, .. }) =
    check_active_periodic(counter, FLEX_DEADLINE_CHECK_STRIDE, RenderStage::Layout)
  {
    return Err(LayoutError::Timeout { elapsed });
  }
  Ok(())
}

#[derive(Clone)]
struct CachedFragmentTemplate {
  template: Arc<FragmentNode>,
}

impl CachedFragmentTemplate {
  fn new(template: Arc<FragmentNode>) -> Self {
    Self { template }
  }

  fn fragment(&self) -> &FragmentNode {
    &self.template
  }

  fn place(&self, bounds: Rect) -> PlacedFragment {
    PlacedFragment::new(self.template.clone(), bounds)
  }
}

#[derive(Clone)]
struct PlacedFragment {
  bounds: Rect,
  template: Arc<FragmentNode>,
}

impl PlacedFragment {
  fn new(template: Arc<FragmentNode>, bounds: Rect) -> Self {
    Self { bounds, template }
  }

  fn materialize(&self) -> FragmentNode {
    let mut cloned = (*self.template).clone();
    cloned.bounds = self.bounds;
    flex_profile::record_fragment_materialize();
    cloned
  }
}

struct PositionedCandidate {
  child_id: usize,
  original_style: Arc<ComputedStyle>,
  layout_child: BoxNode,
  cb: ContainingBlock,
  fragment: FragmentNode,
  positioned_style: crate::style::computed::PositionedStyle,
  preferred_min_inline: Option<f32>,
  preferred_inline: Option<f32>,
  preferred_min_block: Option<f32>,
  preferred_block: Option<f32>,
  is_replaced: bool,
}

fn ensure_box_id(node: &BoxNode) -> usize {
  if node.id != 0 {
    return node.id;
  }
  const EPHEMERAL_ID_BASE: usize = 1usize << (usize::BITS - 1);
  EPHEMERAL_ID_BASE | (node as *const BoxNode as usize)
}

fn trace_flex_text_ids() -> Vec<usize> {
  crate::debug::runtime::runtime_toggles()
    .usize_list("FASTR_TRACE_FLEX_TEXT")
    .unwrap_or_default()
}

fn record_fragment_clone(site: CloneSite, fragment: &FragmentNode) {
  fragment_clone_profile::record_fragment_clone_from_fragment(site, fragment);
}

fn fragment_first_baseline(
  fragment: &FragmentNode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  check_layout_deadline(deadline_counter)?;
  if let Some(baseline) = fragment.baseline {
    return Ok(Some(baseline));
  }

  match &fragment.content {
    FragmentContent::Line { baseline } => Ok(Some(*baseline)),
    FragmentContent::Text {
      baseline_offset, ..
    } => Ok(Some(*baseline_offset)),
    FragmentContent::Replaced { .. } => Ok(Some(fragment.bounds.height())),
    _ => {
      for child in fragment.children.iter() {
        if let Some(baseline) = fragment_first_baseline(child, deadline_counter)? {
          return Ok(Some(child.bounds.y() + baseline));
        }
      }
      Ok(None)
    }
  }
}

#[derive(Clone, Copy)]
enum Axis {
  Horizontal,
  Vertical,
}

#[derive(Clone, Copy)]
enum FitContentAvailable {
  Definite(f32),
  MinContent,
  MaxContent,
}

impl FitContentAvailable {
  fn available_border_box(self, min: f32, max: f32) -> f32 {
    match self {
      Self::Definite(v) => v.max(0.0),
      Self::MinContent => min.max(0.0),
      Self::MaxContent => max.max(0.0),
    }
  }
}

/// Flexbox Formatting Context
///
/// Delegates layout to Taffy's flexbox algorithm. This is a stateless struct
/// that creates a fresh Taffy tree for each layout operation to avoid state issues.
///
/// # Thread Safety
///
/// This struct is `Send + Sync` as required by the `FormattingContext` trait.
/// Each layout operation creates its own TaffyTree instance, ensuring thread safety.
///
/// # Example
///
/// ```ignore
/// use fastrender::layout::contexts::FlexFormattingContext;
/// use fastrender::LayoutConstraints;
/// use fastrender::tree::BoxNode;
///
/// let fc = FlexFormattingContext::new();
/// let constraints = LayoutConstraints::definite(800.0, 600.0);
/// let fragment = fc.layout(&box_node, &constraints)?;
/// ```
#[derive(Clone)]
pub struct FlexFormattingContext {
  /// Shared factory used to create child formatting contexts without losing shared caches.
  factory: FormattingContextFactory,
  /// Viewport size used for resolving viewport-relative units inside Taffy conversion.
  viewport_size: Size,
  font_context: FontContext,
  nearest_positioned_cb: ContainingBlock,
  nearest_fixed_cb: ContainingBlock,
  parallelism: LayoutParallelism,
  measured_fragments: Arc<ShardedFlexCache>,
  layout_fragments: Arc<ShardedFlexCache>,
  taffy_cache: Arc<crate::layout::taffy_integration::TaffyNodeCache>,
}

const MAX_MEASURE_CACHE_PER_NODE: usize = 256;
const MAX_LAYOUT_CACHE_PER_NODE: usize = 128;

impl FlexFormattingContext {
  /// Creates a new FlexFormattingContext
  pub fn new() -> Self {
    let viewport = Size::new(800.0, 600.0);
    Self::with_viewport_and_cb(
      viewport,
      ContainingBlock::viewport(viewport),
      FontContext::new(),
      Arc::new(ShardedFlexCache::new_measure()),
      Arc::new(ShardedFlexCache::new_layout()),
    )
  }

  pub fn with_viewport(viewport_size: Size) -> Self {
    Self::with_viewport_and_cb(
      viewport_size,
      ContainingBlock::viewport(viewport_size),
      FontContext::new(),
      Arc::new(ShardedFlexCache::new_measure()),
      Arc::new(ShardedFlexCache::new_layout()),
    )
  }

  pub fn with_viewport_and_cb(
    viewport_size: Size,
    nearest_positioned_cb: ContainingBlock,
    font_context: FontContext,
    measured_fragments: Arc<ShardedFlexCache>,
    layout_fragments: Arc<ShardedFlexCache>,
  ) -> Self {
    let flex_taffy_cache = Arc::new(TaffyNodeCache::new(taffy_template_cache_limit(
      TaffyAdapterKind::Flex,
    )));
    let grid_taffy_cache = Arc::new(TaffyNodeCache::new(taffy_template_cache_limit(
      TaffyAdapterKind::Grid,
    )));
    let factory = FormattingContextFactory::with_font_context_viewport_cb_and_cache(
      font_context.clone(),
      viewport_size,
      nearest_positioned_cb,
      measured_fragments.clone(),
      layout_fragments.clone(),
      flex_taffy_cache.clone(),
      grid_taffy_cache,
    );
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    Self {
      factory,
      viewport_size,
      font_context,
      nearest_positioned_cb,
      nearest_fixed_cb,
      parallelism: LayoutParallelism::default(),
      measured_fragments,
      layout_fragments,
      taffy_cache: flex_taffy_cache,
    }
  }

  pub(crate) fn with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let measured_fragments = factory.flex_measure_cache();
    let layout_fragments = factory.flex_layout_cache();
    let parallelism = factory.parallelism();
    let taffy_cache = factory.flex_taffy_cache();
    Self {
      factory,
      viewport_size,
      font_context,
      nearest_positioned_cb,
      nearest_fixed_cb,
      parallelism,
      measured_fragments,
      layout_fragments,
      taffy_cache,
    }
  }

  pub fn with_parallelism(mut self, parallelism: LayoutParallelism) -> Self {
    self.parallelism = parallelism;
    self.factory = self.factory.clone().with_parallelism(parallelism);
    self.taffy_cache = self.factory.flex_taffy_cache();
    self
  }

  fn child_factory(&self) -> FormattingContextFactory {
    self.factory.clone()
  }

  fn child_factory_for_cb(&self, cb: ContainingBlock) -> FormattingContextFactory {
    self.factory.with_positioned_cb(cb)
  }
}

impl Default for FlexFormattingContext {
  fn default() -> Self {
    Self::new()
  }
}

impl std::fmt::Debug for FlexFormattingContext {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FlexFormattingContext")
      .finish_non_exhaustive()
  }
}

impl FormattingContext for FlexFormattingContext {
  /// Lays out a flex container and its children using Taffy
  ///
  /// # Process
  ///
  /// 1. Build a Taffy tree from the BoxNode tree
  /// 2. Set available space constraints
  /// 3. Run Taffy's compute_layout()
  /// 4. Convert Taffy layout results back to FragmentNode tree
  fn layout(
    &self,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
  ) -> Result<FragmentNode, LayoutError> {
    debug_assert!(
      matches!(
        box_node.formatting_context(),
        Some(FormattingContextType::Flex)
      ),
      "FlexFormattingContext must only layout flex containers",
    );
    let _profile = layout_timer(LayoutKind::Flex);
    if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
      return Err(LayoutError::Timeout { elapsed });
    }
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let override_active = style_override.is_some();
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
    let build_timer = flex_profile::timer();
    let mut constraints = *constraints;
    let container_inline_base = constraints
      .inline_percentage_base
      .or_else(|| constraints.width());
    if matches!(constraints.available_width, CrateAvailableSpace::Indefinite) {
      let fallback = container_inline_base.unwrap_or(self.viewport_size.width);
      constraints.available_width = CrateAvailableSpace::Definite(fallback);
      if constraints.inline_percentage_base.is_none() {
        constraints.inline_percentage_base = container_inline_base.or(Some(fallback));
      }
    }
    // Keep block axis as provided; many flex containers legitimately size-to-content.

    let mut deadline_counter = 0usize;
    let mut has_running_children = false;
    for child in box_node.children.iter() {
      check_layout_deadline(&mut deadline_counter)?;
      if child.style.running_position.is_some() {
        has_running_children = true;
        break;
      }
    }
    // Do not cache flex containers that contain running elements: running anchors are synthesized
    // based on in-flow position, so reusing cached fragments can capture the wrong snapshot.
    let toggles = crate::debug::runtime::runtime_toggles();
    let disable_global_layout_cache =
      toggles.truthy("FASTR_DISABLE_FLEX_CACHE") || has_running_children;
    if !disable_global_layout_cache {
      if let Some(cached) = layout_cache_lookup(
        box_node,
        FormattingContextType::Flex,
        &constraints,
        self.factory.viewport_scroll(),
        self.viewport_size,
      ) {
        return Ok(cached);
      }
    }

    // Reuse full layout fragments when the same flex container is laid out repeatedly with
    // identical available sizes (common on carousel-heavy pages). This is scoped per layout
    // run via the factory cache reset.
    let disable_cache = disable_global_layout_cache || override_active;
    let viewport_scroll = sanitize_viewport_scroll(self.factory.viewport_scroll());
    let layout_cache_entry = if disable_cache {
      None
    } else {
      layout_cache_key(&constraints, self.viewport_size).map(|k| {
        (
          flex_cache_key_with_style_and_scroll(box_node, style, viewport_scroll),
          k,
        )
      })
    };

    let _trace_text_ids = trace_flex_text_ids();
    if let Some((cache_key, key)) = layout_cache_entry {
      if let Some(cached) = self.layout_fragments.get(cache_key, &key) {
        let fragment = cached.fragment;
        flex_profile::record_layout_cache_hit();
        flex_profile::record_layout_cache_clone();
        record_fragment_clone(CloneSite::FlexLayoutCacheHit, fragment.as_ref());
        return Ok((*fragment).clone());
      }
    }

    // Create a fresh Taffy tree for this layout.
    //
    // Use a pooled instance to avoid repeatedly allocating slotmap backing storage on pages with
    // many nested flex/grid containers.
    let mut taffy_tree = crate::layout::taffy_integration::PooledTaffyTree::new();

    // Partition children: out-of-flow abs/fixed are handled after flex layout per CSS positioning.
    let mut in_flow_children: Vec<(usize, &BoxNode)> = Vec::new();
    let mut positioned_children: Vec<&BoxNode> = Vec::new();
    let mut running_children: Vec<(usize, BoxNode)> = Vec::new();
    let mut in_flow_children_need_sort = false;
    let mut last_in_flow_order: Option<i32> = None;
    for (idx, child) in box_node.children.iter().enumerate() {
      check_layout_deadline(&mut deadline_counter)?;
      if child.style.running_position.is_some() {
        // Running elements do not participate in flex layout; instead, capture a snapshot at the
        // position the element would have occupied in flow.
        running_children.push((idx, child.clone()));
        continue;
      }
      match child.style.position {
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
          positioned_children.push(child);
        }
        _ => {
          if let Some(prev) = last_in_flow_order {
            if child.style.order < prev {
              in_flow_children_need_sort = true;
            }
          }
          last_in_flow_order = Some(child.style.order);
          in_flow_children.push((idx, child))
        }
      }
    }
    if in_flow_children_need_sort {
      // `check_layout_deadline()` is periodic; ensure we still perform a definite check before doing
      // potentially expensive sort work.
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      #[cfg(test)]
      record_flex_order_sort_call();
      in_flow_children.sort_by(|(a_idx, a), (b_idx, b)| {
        a.style
          .order
          .cmp(&b.style.order)
          .then_with(|| a_idx.cmp(b_idx))
      });
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
    }
    let in_flow_children: Vec<&BoxNode> = in_flow_children
      .into_iter()
      .map(|(_, child)| child)
      .collect();

    // Phase 1: Build Taffy tree from in-flow children
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::with_capacity_and_hasher(
      in_flow_children.len().saturating_add(1),
      Default::default(),
    );
    let root_node = self.build_taffy_tree_children(
      &mut taffy_tree,
      box_node,
      style,
      &in_flow_children,
      &constraints,
      &mut node_map,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      // When a style override is active, update the root Taffy style in-place so we can keep
      // reusing cached Taffy templates without deep-cloning the box subtree.
      let override_style = self.computed_style_to_taffy_base(style_override, true, None)?;
      taffy_tree
        .set_style(root_node, override_style)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }
    flex_profile::record_build_time(build_timer);

    // Block-level flex containers with `width:auto` fill the available inline space. Root flex
    // nodes have no parent for percentage resolution, so translate the definite available width
    // into an explicit Taffy size to ensure flex-shrink/grow runs against the correct line size.
    if physical_width_is_auto(style) && matches!(style.display, Display::Flex) {
      if let CrateAvailableSpace::Definite(w) = constraints.available_width {
        if let Ok(existing) = taffy_tree.style(root_node) {
          let mut updated = existing.clone();
          updated.size.width = Dimension::length(w.max(0.0));
          taffy_tree
            .set_style(root_node, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // When a parent layout mode (e.g. flex/grid) has already resolved a definite used border-box
    // size for this grid/flex item, force that size on the root node without mutating the box
    // tree's style. This avoids deep-cloning box subtrees just to inject synthetic `width/height`
    // values while still keeping Taffy sizing consistent with the parent algorithm.
    if constraints.used_border_box_width.is_some() || constraints.used_border_box_height.is_some() {
      if let Ok(existing) = taffy_tree.style(root_node) {
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
          taffy_tree
            .set_style(root_node, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // `width/height: fit-content` clamps the used size between the box's min- and max-content
    // contributions. Outer layout usually resolves this for block-level boxes and passes the
    // result via `used_border_box_*`, but when the flex formatting context is invoked without
    // such an override (notably at the root of the layout tree) we must compute it here so Taffy
    // uses the correct line size.
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
      if let Ok(existing) = taffy_tree.style(root_node) {
        let mut updated = existing.clone();
        let mut changed = false;

        if constraints.used_border_box_width.is_none() {
          if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.width_keyword {
            match self.resolve_root_fit_content_border_box_size(
              box_node,
              style,
              &constraints,
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
              &constraints,
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
          taffy_tree
            .set_style(root_node, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // Phase 2: Compute layout using Taffy
    let mut available_space = self.constraints_to_available_space(&constraints);
    // If the flex container itself is sized with intrinsic keywords, map the available space we
    // pass into Taffy so it performs the corresponding intrinsic probe. Fit-content needs a
    // definite available size (fill-available), so only map min-/max-content here.
    if constraints.used_border_box_width.is_none() {
      match style.width_keyword {
        Some(IntrinsicSizeKeyword::MinContent) => {
          available_space.width = AvailableSpace::MinContent
        }
        Some(IntrinsicSizeKeyword::MaxContent) => {
          available_space.width = AvailableSpace::MaxContent
        }
        _ => {}
      }
    }
    if constraints.used_border_box_height.is_none() {
      match style.height_keyword {
        Some(IntrinsicSizeKeyword::MinContent) => {
          available_space.height = AvailableSpace::MinContent
        }
        Some(IntrinsicSizeKeyword::MaxContent) => {
          available_space.height = AvailableSpace::MaxContent
        }
        _ => {}
      }
    }
    let viewport_size = self.viewport_size;
    let measured_fragments = self.measured_fragments.clone();

    let establishes_fixed_cb = style.has_transform()
      || style.perspective.is_some()
      || style.containment.layout
      || style.containment.paint;
    let descendant_nearest_fixed_cb = if establishes_fixed_cb {
      let percentage_base = container_inline_base.unwrap_or(viewport_size.width);
      let padding_left = self.resolve_length_for_width(style.padding_left, percentage_base, style);
      let padding_right = self.resolve_length_for_width(style.padding_right, percentage_base, style);
      let padding_top = self.resolve_length_for_width(style.padding_top, percentage_base, style);
      let padding_bottom = self.resolve_length_for_width(style.padding_bottom, percentage_base, style);
      let border_left =
        self.resolve_length_for_width(style.border_left_width, percentage_base, style);
      let border_top = self.resolve_length_for_width(style.border_top_width, percentage_base, style);

      let padding_origin = Point::new(border_left + padding_left, border_top + padding_top);
      let content_width = constraints.width().unwrap_or(0.0).max(0.0);
      let content_height = constraints.height().unwrap_or(0.0).max(0.0);
      let padding_size = Size::new(
        content_width + padding_left + padding_right,
        content_height + padding_top + padding_bottom,
      );
      let padding_rect = Rect::new(padding_origin, padding_size);
      ContainingBlock::with_viewport_and_bases(
        padding_rect,
        viewport_size,
        Some(padding_rect.size.width),
        constraints.height().map(|_| padding_rect.size.height),
      )
    } else {
      self.nearest_fixed_cb
    };

    let base_factory = if descendant_nearest_fixed_cb == self.factory.nearest_fixed_cb() {
      self.child_factory()
    } else {
      self.factory.with_fixed_cb(descendant_nearest_fixed_cb)
    };
    let factory = base_factory.clone();
    let viewport_scroll = sanitize_viewport_scroll(factory.viewport_scroll());
    let mut scroll_sensitive_items: FxHashSet<*const BoxNode> = FxHashSet::default();
    for child in in_flow_children.iter() {
      if subtree_contains_content_visibility_auto(child, &mut deadline_counter)? {
        scroll_sensitive_items.insert(*child as *const BoxNode);
      }
    }
    let flex_item_block_fc: Arc<dyn FormattingContext> = Arc::new(
      BlockFormattingContext::for_flex_item_with_factory(base_factory.clone())
        .with_parallelism(self.parallelism),
    );
    let this = self.clone();

    let auto_item_nodes: Vec<(&BoxNode, NodeId)> = in_flow_children
      .iter()
      .filter(|child| {
        matches!(
          child.style.content_visibility,
          crate::style::types::ContentVisibility::Auto
        )
      })
      .filter(|child| self.content_visibility_auto_has_definite_placeholder(child))
      .filter_map(|child| {
        node_map
          .get(&(*child as *const BoxNode))
          .copied()
          .map(|node_id| (*child, node_id))
      })
      .collect();
    let auto_item_count = auto_item_nodes.len();
    let auto_all_nodes: FxHashSet<*const BoxNode> = auto_item_nodes
      .iter()
      .map(|(child, _)| *child as *const BoxNode)
      .collect();
    let mut auto_unskipped_nodes: FxHashSet<*const BoxNode> = FxHashSet::default();
    let compute_timer = flex_profile::timer();
    let log_root = toggles.truthy("FASTR_LOG_FLEX_ROOT");
    if log_root {
      eprintln!(
        "[flex-root] id={} selector={} avail=({:?},{:?}) known=({:?},{:?}) viewport=({:.1},{:.1})",
        box_node.id,
        box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string()),
        available_space.width,
        available_space.height,
        constraints.width(),
        constraints.height(),
        self.viewport_size.width,
        self.viewport_size.height,
      );
    }
    let log_constraint_raw = toggles
      .get("FASTR_LOG_FLEX_CONSTRAINTS")
      .map(|v| v.to_string());
    let log_constraint_ids = toggles
      .usize_list("FASTR_LOG_FLEX_CONSTRAINTS")
      .unwrap_or_default();
    let log_constraint_limit = toggles.usize_with_default("FASTR_LOG_FLEX_CONSTRAINTS_MAX", 10);
    let log_first_n = toggles.usize_with_default("FASTR_LOG_FLEX_FIRST_N", 0);
    let abort_after_first = toggles.truthy("FASTR_ABORT_FLEX_AFTER_FIRST_N");
    if log_constraint_raw.is_some() {
      eprintln!(
        "[flex-constraints-env] raw={:?} ids={:?} max={}",
        log_constraint_raw, log_constraint_ids, log_constraint_limit
      );
    }
    if log_first_n > 0 {
      eprintln!(
        "[flex-first-env] n={} abort={}",
        log_first_n, abort_after_first
      );
    }
    let log_skinny = toggles.truthy("FASTR_LOG_SKINNY_FLEX");
    let log_small_avail = toggles.truthy("FASTR_LOG_SMALL_FLEX");
    let log_measure_ids = toggles
      .usize_list("FASTR_LOG_FLEX_MEASURE_IDS")
      .unwrap_or_default();
    let log_node_keys = toggles
      .usize_list("FASTR_LOG_FLEX_NODE_KEYS")
      .unwrap_or_default();
    let log_node_keys_max = toggles.usize_with_default("FASTR_LOG_FLEX_NODE_KEYS_MAX", 10);
    let log_large_avail = toggles.f64("FASTR_LOG_LARGE_FLEX").map(|v| v as f32);
    static LOG_NODE_KEYS_COUNTS: std::sync::OnceLock<
      std::sync::Mutex<std::collections::HashMap<usize, usize>>,
    > = std::sync::OnceLock::new();
    static LOG_LARGE_AVAIL_COUNTS: std::sync::OnceLock<
      std::sync::Mutex<std::collections::HashMap<usize, usize>>,
    > = std::sync::OnceLock::new();
    static LOG_CONSTRAINT_COUNTS: std::sync::OnceLock<
      std::sync::Mutex<std::collections::HashMap<usize, usize>>,
    > = std::sync::OnceLock::new();
    static LOG_FIRST_N_COUNTER: std::sync::OnceLock<std::sync::Mutex<usize>> =
      std::sync::OnceLock::new();
    let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
    // Render pipeline always installs a deadline guard (even when disabled), so only enable
    // the Taffy cancellation path when the active deadline is actually configured.
    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
    let max_passes = if auto_item_count == 0 {
      1
    } else {
      FLEX_CONTENT_VISIBILITY_AUTO_MAX_PASSES
    };
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
        for (child, node_id) in auto_item_nodes.iter() {
          let mut resolved_style = self.computed_style_to_taffy_base(
            child.style.as_ref(),
            false,
            Some(style),
          )?;
          self.apply_flex_intrinsic_size_keywords(
            child,
            false,
            Some(style),
            Some(&constraints),
            &mut resolved_style,
          )?;
          let skip_contents = self.flex_item_should_skip_contents(child, &auto_unskipped_nodes);
          self.apply_flex_auto_min_size(
            child,
            false,
            Some(style),
            skip_contents,
            &mut resolved_style,
          )?;
          self.apply_flex_fit_content_keywords(
            child,
            false,
            Some(style),
            &constraints,
            &mut resolved_style,
          )?;
          taffy_tree
            .set_style(*node_id, resolved_style)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
        for (_, node_id) in auto_item_nodes.iter() {
          taffy_tree
            .mark_dirty(*node_id)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
        taffy_tree
          .mark_dirty(root_node)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }

      let auto_unskipped_for_pass = &auto_unskipped_nodes;
      let mut pass_cache: FxHashMap<u64, FlexCacheEntry> =
        FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
      let measure_toggles = toggles.clone();
      let container_inline_positive = self.inline_axis_positive(style);
      let container_block_positive = self.block_axis_positive(style);
      let container_flex_direction = self.flex_direction_to_taffy(
        style,
        container_inline_positive,
        container_block_positive,
      );
      let container_main_axis_is_horizontal = matches!(
        container_flex_direction,
        taffy::style::FlexDirection::Row | taffy::style::FlexDirection::RowReverse
      );
      record_taffy_invocation(TaffyAdapterKind::Flex);
      let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
      let compute_result = taffy_tree.compute_layout_with_measure_and_cancel(
        root_node,
        available_space,
        |mut known_dimensions, mut avail, _node_id, node_context, _style| {
                    if taffy_perf_enabled {
                      record_taffy_measure_call(TaffyAdapterKind::Flex);
                    }
                    let toggles = measure_toggles.as_ref();
                    // Treat zero/near-zero definite sizes as absent to avoid pathological
                    // measurement probes when Taffy propagates a 0px available size. This
                    // aligns with constraints_from_taffy, which promotes tiny definites to
                    // Indefinite/MaxContent.
                    if let Some(w) = known_dimensions.width {
                        if w <= 1.0 && matches!(avail.width, AvailableSpace::Definite(v) if v <= 1.0) {
                            known_dimensions.width = None;
                            avail.width = AvailableSpace::MaxContent;
                        }
                    }
                    if let AvailableSpace::Definite(w) = avail.width {
                        if w <= 1.0 {
                            avail.width = AvailableSpace::MaxContent;
                        }
                    }
                    if let Some(h) = known_dimensions.height {
                        if h <= 1.0 && matches!(avail.height, AvailableSpace::Definite(v) if v <= 1.0) {
                            known_dimensions.height = None;
                            avail.height = AvailableSpace::MaxContent;
                        }
                    }
                    if let AvailableSpace::Definite(h) = avail.height {
                        if h <= 1.0 {
                            avail.height = AvailableSpace::MaxContent;
                        }
                    }

                    flex_profile::record_measure_lookup();
                    let measure_timer = flex_profile::timer();
                    let mut force_full_measure = false;
                    if let Some(node_ptr) = node_context.as_ref().map(|p| **p) {
                        let box_node = unsafe { &*node_ptr };
                        force_full_measure = !log_measure_ids.is_empty()
                          && log_measure_ids.contains(&box_node.id);
                        if known_dimensions.width == Some(0.0)
                            && matches!(avail.width, AvailableSpace::Definite(0.0))
                            && physical_width_is_auto(box_node.style.as_ref())
                        {
                            known_dimensions.width = None;
                        }
                        if known_dimensions.height == Some(0.0)
                            && matches!(avail.height, AvailableSpace::Definite(0.0))
                            && physical_height_is_auto(box_node.style.as_ref())
                        {
                            known_dimensions.height = None;
                        }
                        if matches!(avail.width, AvailableSpace::Definite(v) if v == 0.0)
                            && known_dimensions.width.is_none()
                            && physical_width_is_auto(box_node.style.as_ref())
                        {
                            avail.width = AvailableSpace::MaxContent;
                        }
                        if matches!(avail.height, AvailableSpace::Definite(v) if v == 0.0)
                            && known_dimensions.height.is_none()
                            && physical_height_is_auto(box_node.style.as_ref())
                        {
                            avail.height = AvailableSpace::MaxContent;
                        }
                        if log_small_avail {
                            if let AvailableSpace::Definite(w) = avail.width {
                                if w > 0.0 && w <= 100.0 {
                                    let selector = box_node
                                        .debug_info
                                        .as_ref()
                                        .map(|d| d.to_selector())
                                        .unwrap_or_else(|| "<anon>".to_string());
                                    eprintln!(
                                        "[flex-avail-small] id={} selector={} known_w={:?} known_h={:?} avail_w={:?} avail_h={:?} width_decl={:?} min_w={:?} max_w={:?}",
                                        box_node.id,
                                        selector,
                                        known_dimensions.width,
                                        known_dimensions.height,
                                        avail.width,
                                        avail.height,
                                        box_node.style.width,
                                        box_node.style.min_width,
                                        box_node.style.max_width,
                                    );
                                }
                            }
                        }

                        // For content-based flex base sizing, prefer intrinsic max-content sizing
                        // instead of forcing the container's definite width into the measurement
                        // constraints. This matches CSS Flexbox §4.5 (auto main size uses max-content)
                        // and the `flex-basis: content` override.
                        if known_dimensions.width.is_none()
                            && matches!(avail.width, AvailableSpace::Definite(_))
                            && ((matches!(
                                box_node.style.flex_basis,
                                crate::style::types::FlexBasis::Auto
                              ) && physical_width_is_auto(box_node.style.as_ref()))
                              || matches!(
                                box_node.style.flex_basis,
                                crate::style::types::FlexBasis::Content
                              ))
                        {
                            avail.width = AvailableSpace::MaxContent;
                        }
                    }
                    // Fast path: when both dimensions are already known (typically from definite
                    // authored sizes or a previous cached measurement), we don't need any cache-key
                    // bookkeeping or intrinsic/layout work. This is a very hot path on large pages
                    // where most flex items resolve to fixed sizes.
                    if !force_full_measure {
                      if let (Some(w), Some(h)) = (known_dimensions.width, known_dimensions.height) {
                        let size = taffy::geometry::Size { width: w, height: h };
                        flex_profile::record_measure_time(measure_timer);
                        return size;
                      }
                    }
                    let w_state = if known_dimensions.width.is_some() {
                        DimState::Known
                    } else if matches!(
                        avail.width,
                        AvailableSpace::Definite(_) | AvailableSpace::MinContent | AvailableSpace::MaxContent
                    ) {
                        DimState::Definite
                    } else {
                        DimState::Other
                    };
                    let h_state = if known_dimensions.height.is_some() {
                        DimState::Known
                    } else if matches!(
                        avail.height,
                        AvailableSpace::Definite(_) | AvailableSpace::MinContent | AvailableSpace::MaxContent
                    ) {
                        DimState::Definite
                    } else {
                        DimState::Other
                    };
                    flex_profile::record_measure_bucket(w_state, h_state);
                    let drop_available_height = known_dimensions.height.is_some()
                        || node_context
                            .as_ref()
                            .map(|ptr| {
                                let box_ptr: *const BoxNode = **ptr;
                                let box_node = unsafe { &*box_ptr };
                                !height_depends_on_available_height(&box_node.style)
                            })
                            .unwrap_or(false);
                    let (key, snapped_known_dimensions, snapped_avail) =
                        measure_cache_key_and_snap(&known_dimensions, &avail, viewport_size, drop_available_height);
                    known_dimensions = snapped_known_dimensions;
                    avail = snapped_avail;
                    let bucket = match (w_state, h_state) {
                        (DimState::Known, DimState::Known) => 0,
                        (DimState::Known, DimState::Definite) => 1,
                        (DimState::Known, DimState::Other) => 2,
                        (DimState::Definite, DimState::Known) => 3,
                        (DimState::Definite, DimState::Definite) => 4,
                        (DimState::Definite, DimState::Other) => 5,
                        (DimState::Other, DimState::Known) => 6,
                        (DimState::Other, DimState::Definite) => 7,
                        (DimState::Other, DimState::Other) => 8,
                    };
                    flex_profile::record_histogram(bucket, key);
                    let node_ptr = node_context.as_ref().map(|p| **p);
                    if let Some(ptr) = node_ptr {
                        let box_node = unsafe { &*ptr };
                        flex_profile::record_node_lookup(box_node.id, key);
                        if !log_node_keys.is_empty() && log_node_keys.contains(&box_node.id) {
                            let counts = LOG_NODE_KEYS_COUNTS
                                .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
                            if let Ok(mut guard) = counts.lock() {
                                let entry = guard.entry(box_node.id).or_insert(0);
                                if *entry < log_node_keys_max {
                                    *entry += 1;
                                    let selector = box_node
                                        .debug_info
                                        .as_ref()
                                        .map(|d| d.to_selector())
                                        .unwrap_or_else(|| "<anon>".to_string());
                                    eprintln!(
                                        "[flex-node-key] id={} selector={} lookup={} bucket={} known=({:?},{:?}) avail=({:?},{:?}) key=({:?},{:?})",
                                        box_node.id,
                                        selector,
                                        *entry,
                                        bucket,
                                        known_dimensions.width,
                                        known_dimensions.height,
                                        avail.width,
                                        avail.height,
                                        key.0,
                                        key.1
                                    );
                                    if *entry == log_node_keys_max {
                                        eprintln!(
                                            "[flex-node-key-cap] id={} selector={} cap_reached={}",
                                            box_node.id, selector, log_node_keys_max
                                        );
                                    }
                                }
                            }
                        }
                        if let Some(threshold) = log_large_avail {
                            let mut log = false;
                            if let AvailableSpace::Definite(w) = avail.width {
                                if w > threshold {
                                    log = true;
                                }
                            }
                            if let AvailableSpace::Definite(h) = avail.height {
                                if h > threshold {
                                    log = true;
                                }
                            }
                            if log {
                                let counts = LOG_LARGE_AVAIL_COUNTS
                                    .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
                                if let Ok(mut guard) = counts.lock() {
                                    let entry = guard.entry(box_node.id).or_insert(0);
                                    if *entry < 5 {
                                        *entry += 1;
                                        let selector = box_node
                                            .debug_info
                                            .as_ref()
                                            .map(|d| d.to_selector())
                                            .unwrap_or_else(|| "<anon>".to_string());
                                        eprintln!(
                                            "[flex-large-avail] id={} selector={} lookup={} known=({:?},{:?}) avail=({:?},{:?}) key=({:?},{:?}) threshold={}",
                                            box_node.id,
                                            selector,
                                            *entry,
                                            known_dimensions.width,
                                            known_dimensions.height,
                                            avail.width,
                                            avail.height,
                                            key.0,
                                            key.1,
                                            threshold
                                        );
                                    }
                                }
                            }
                        }
                        let mut logged_first = false;
                        if log_first_n > 0 {
                            let counter = LOG_FIRST_N_COUNTER
                                .get_or_init(|| std::sync::Mutex::new(0));
                            if let Ok(mut guard) = counter.lock() {
                                if *guard < log_first_n {
                                    *guard += 1;
                                    logged_first = true;
                                    let selector = box_node
                                        .debug_info
                                        .as_ref()
                                        .map(|d| d.to_selector())
                                        .unwrap_or_else(|| "<anon>".to_string());
                                    eprintln!(
                                        "[flex-first] seq={} id={} selector={} known=({:?},{:?}) avail=({:?},{:?}) key=({:?},{:?})",
                                        *guard,
                                        box_node.id,
                                        selector,
                                        known_dimensions.width,
                                        known_dimensions.height,
                                        avail.width,
                                        avail.height,
                                        key.0,
                                        key.1
                                    );
                                    assert!(
                                        !(abort_after_first && *guard >= log_first_n),
                                        "[flex-first-abort] seq={}",
                                        *guard
                                    );
                                }
                            }
                        }
                        if logged_first || (!log_constraint_ids.is_empty() && log_constraint_ids.contains(&box_node.id)) {
                            let counts = LOG_CONSTRAINT_COUNTS
                                .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
                            if let Ok(mut guard) = counts.lock() {
                                let entry = guard.entry(box_node.id).or_insert(0);
                                if *entry < log_constraint_limit {
                                    *entry += 1;
                                    let selector = box_node
                                        .debug_info
                                        .as_ref()
                                        .map(|d| d.to_selector())
                                        .unwrap_or_else(|| "<anon>".to_string());
                                    eprintln!(
                                        "[flex-constraints] id={} selector={} lookup={} known=({:?},{:?}) avail=({:?},{:?}) key=({:?},{:?})",
                                        box_node.id,
                                        selector,
                                        *entry,
                                        known_dimensions.width,
                                        known_dimensions.height,
                                        avail.width,
                                        avail.height,
                                        key.0,
                                        key.1
                                    );
                                    if *entry == log_constraint_limit {
                                        eprintln!(
                                            "[flex-constraints-cap] id={} selector={} cap_reached={}",
                                            box_node.id, selector, log_constraint_limit
                                        );
                                    }
                                }
                            }
                        }
                    }
                    static TRACE_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
                    static LOG_MEASURE_COUNTS: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
                    let trace_enabled = toggles.truthy("FASTR_TRACE_FLEX");
                    static LOG_MEASURE_FIRST_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
                    let log_measure_first =
                        toggles.usize_with_default("FASTR_LOG_FLEX_MEASURE_FIRST_N", 0);
                    let log_measure_first_above =
                        toggles.u128("FASTR_LOG_FLEX_MEASURE_FIRST_N_MS");

                    let fallback_size = |known: Option<f32>, avail_dim: AvailableSpace| {
                        known.unwrap_or(match avail_dim {
                            AvailableSpace::Definite(v) => v,
                            _ => 0.0,
                        })
                    };
                    let Some(box_ptr) = node_ptr else {
                        let size = taffy::geometry::Size {
                            width: fallback_size(known_dimensions.width, avail.width),
                            height: fallback_size(known_dimensions.height, avail.height),
                        };
                        flex_profile::record_measure_time(measure_timer);
                        return size;
                    };
                    let box_node = unsafe { &*box_ptr };
                    if log_measure_first > 0 {
                        if let Some(threshold) = log_measure_first_above {
                            let elapsed = measure_timer.map(|s| s.elapsed().as_millis()).unwrap_or(0);
                            if elapsed >= threshold {
                                 let seq = {
                                     let mut count =
                                         LOG_MEASURE_FIRST_COUNT
                                           .get_or_init(|| Mutex::new(0))
                                           .lock()
                                           .unwrap_or_else(|poisoned| poisoned.into_inner());
                                     (*count < log_measure_first).then(|| {
                                         *count += 1;
                                         *count
                                     })
                                };
                                if let Some(seq) = seq {
                                    let selector = box_node
                                        .debug_info
                                        .as_ref()
                                        .map(|d| d.to_selector())
                                        .unwrap_or_else(|| "<anon>".to_string());
                                    eprintln!(
                                        "[flex-measure-first] seq={} id={} selector={} elapsed_ms={} known=({:?},{:?}) avail=({:?},{:?})",
                                        seq,
                                        box_node.id,
                                        selector,
                                        elapsed,
                                        known_dimensions.width,
                                        known_dimensions.height,
                                        avail.width,
                                        avail.height,
                                    );
                                }
                            }
                        }
                     }
                      if trace_enabled {
                          let mut remaining = TRACE_COUNT
                            .get_or_init(|| Mutex::new(50))
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                          if *remaining > 0 {
                              let selector = box_node
                                .debug_info
                                .as_ref()
                                .map(|d| d.to_selector())
                                .unwrap_or_else(|| "<anon>".to_string());
                               eprintln!(
                                   "flex-trace selector={} display={:?} known=({:?},{:?}) avail=({:?},{:?}) flex=({}, {}, {:?})",
                                  selector,
                                 box_node.style.display,
                                 known_dimensions.width,
                                 known_dimensions.height,
                                 avail.width,
                                avail.height,
                                box_node.style.flex_grow,
                                box_node.style.flex_shrink,
                                box_node.style.flex_basis
                            );
                            *remaining -= 1;
                         }
                     }

                    let constraints = this.constraints_from_taffy(known_dimensions, avail, None);
                    let skip_contents =
                      this.flex_item_should_skip_contents(box_node, auto_unskipped_for_pass);
                    if skip_contents {
                      let placeholder = this.content_visibility_placeholder_content_size(
                        box_node,
                        &constraints,
                        known_dimensions,
                      );
                      flex_profile::record_measure_time(measure_timer);
                      return taffy::geometry::Size {
                        width: placeholder.width,
                        height: placeholder.height,
                      };
                    }

                    // When Taffy asks for the min-content contribution of a flex item, ignore
                    // author-specified widths so auto min-size falls back to the content-driven
                    // minimum (per CSS Flexbox §4.5). Keep min-width/max-width intact so explicit
                    // constraints still apply.
                    let mut cloned_style: Option<ComputedStyle> = None;
                    if matches!(box_node.style.flex_basis, crate::style::types::FlexBasis::Content) {
                      // `flex-basis: content` must ignore the preferred main size (width/height) when
                      // determining the flex base size, even if it is a definite length. Our
                      // intrinsic sizing APIs otherwise honor specified widths, so temporarily clear
                      // the preferred main-size property for this probe.
                      let style = cloned_style.get_or_insert_with(|| (*box_node.style).clone());
                      if container_main_axis_is_horizontal {
                        style.width = None;
                        style.width_keyword = None;
                      } else {
                        style.height = None;
                        style.height_keyword = None;
                      }
                    }
                    // When the available inline size is intrinsic (min-/max-content), percentage
                    // widths/min/max can't be resolved (§10.5). Treat them as auto so intrinsic
                    // sizing uses content-driven sizes instead of forcing 100% of an unknown base.
                    let avail_is_intrinsic =
                        matches!(avail.width, AvailableSpace::MinContent | AvailableSpace::MaxContent);
                    if avail_is_intrinsic {
                        let style = cloned_style.get_or_insert_with(|| (*box_node.style).clone());
                        if matches!(style.width, Some(len) if len.unit.is_percentage()) {
                            style.width = None;
                            style.width_keyword = None;
                        }
                        if matches!(style.min_width, Some(len) if len.unit.is_percentage()) {
                            style.min_width = None;
                            style.min_width_keyword = None;
                        }
                        if matches!(style.max_width, Some(len) if len.unit.is_percentage()) {
                            style.max_width = None;
                            style.max_width_keyword = None;
                        }
                    }
                    // Flexbox automatic minimum sizes use the min-content size suggestion, which is
                    // content-driven (specified sizes are handled separately by Taffy). When Taffy
                    // requests a min-content measurement, clear authored sizes on that axis so the
                    // formatting context can compute the content size suggestion instead of
                    // echoing a fixed `width`/`height`.
                    if matches!(avail.width, AvailableSpace::MinContent) && known_dimensions.width.is_none() {
                        let style = cloned_style.get_or_insert_with(|| (*box_node.style).clone());
                        style.width = None;
                        style.min_width = None;
                        style.max_width = None;
                        style.width_keyword = None;
                        style.min_width_keyword = None;
                        style.max_width_keyword = None;
                    }
                    if matches!(avail.height, AvailableSpace::MinContent | AvailableSpace::MaxContent) {
                        let style = cloned_style.get_or_insert_with(|| (*box_node.style).clone());
                        if matches!(style.height, Some(len) if len.unit.is_percentage()) {
                            style.height = None;
                            style.height_keyword = None;
                        }
                        if matches!(style.min_height, Some(len) if len.unit.is_percentage()) {
                            style.min_height = None;
                            style.min_height_keyword = None;
                        }
                        if matches!(style.max_height, Some(len) if len.unit.is_percentage()) {
                            style.max_height = None;
                            style.max_height_keyword = None;
                        }
                    }
                    if matches!(avail.height, AvailableSpace::MinContent) && known_dimensions.height.is_none() {
                        let style = cloned_style.get_or_insert_with(|| (*box_node.style).clone());
                        style.height = None;
                        style.min_height = None;
                        style.max_height = None;
                        style.height_keyword = None;
                        style.min_height_keyword = None;
                        style.max_height_keyword = None;
                    }
                    if known_dimensions.width.is_none()
                        && matches!(avail.width, AvailableSpace::MinContent)
                        && matches!(box_node.style.flex_basis, crate::style::types::FlexBasis::Auto)
                        && matches!(box_node.style.width, Some(w) if !w.unit.is_absolute() && !w.unit.is_viewport_relative())
                    {
                        // For auto min-size with non-definite widths (percent/font-relative),
                        // remeasure without the authored width so the intrinsic content drives
                        // the min-content contribution. Keep definite lengths/viewport units
                        // intact so fixed/viewport-spanning items preserve their authored size.
                        let style = cloned_style.get_or_insert_with(|| (*box_node.style).clone());
                        style.width = None;
                        style.width_keyword = None;
                    }
                    let override_style = cloned_style.map(Arc::new);
                    // When probing intrinsic sizes we may temporarily override the root style.
                    // Use the thread-local override mechanism rather than cloning the entire box
                    // subtree just to swap the style pointer.
                    let measure_box: &BoxNode = box_node;
                    let measure_style: &ComputedStyle = override_style
                      .as_deref()
                      .unwrap_or_else(|| measure_box.style.as_ref());
                    let cache_key = if scroll_sensitive_items.contains(&box_ptr) {
                      flex_cache_key_with_style_and_scroll(
                        measure_box,
                        measure_style,
                        viewport_scroll,
                      )
                    } else {
                      flex_cache_key_with_style(measure_box, measure_style)
                    };
                    if let Some(cached) = pass_cache.get(&cache_key).and_then(|m| m.get(&key)).cloned() {
                        record_node_measure_hit(measure_box.id);
                        flex_profile::record_measure_hit();
                        flex_profile::record_measure_bucket_hit(w_state, h_state);
                        flex_profile::record_measure_time(measure_timer);
                        return taffy::geometry::Size {
                            width: cached.measured_size.width,
                            height: cached.measured_size.height,
                        };
                    }
                    if let Some(entry) = pass_cache.get(&cache_key) {
                        let target_w = fallback_size(known_dimensions.width, avail.width);
                        let target_h = fallback_size(known_dimensions.height, avail.height);
                        if let Some(cached) =
                            find_layout_cache_fragment(entry, Size::new(target_w, target_h))
                        {
                            record_node_measure_hit(measure_box.id);
                            flex_profile::record_measure_hit();
                            flex_profile::record_measure_bucket_hit(w_state, h_state);
                            flex_profile::record_measure_time(measure_timer);
                            pass_cache
                                .entry(cache_key)
                                .or_default()
                                .entry(key)
                                .or_insert_with(|| cached.clone());
                            return taffy::geometry::Size {
                                width: cached.measured_size.width,
                                height: cached.measured_size.height,
                            };
                        }
                    }
                    if let Some(cached) = measured_fragments.get(cache_key, &key) {
                        pass_cache
                            .entry(cache_key)
                            .or_default()
                            .entry(key)
                            .or_insert_with(|| cached.clone());
                        record_node_measure_hit(measure_box.id);
                        flex_profile::record_measure_hit();
                        flex_profile::record_measure_time(measure_timer);
                        return taffy::geometry::Size {
                            width: cached.measured_size.width,
                            height: cached.measured_size.height,
                        };
                    }
                    let fc_type = measure_box.formatting_context().unwrap_or(FormattingContextType::Block);
                    if !log_measure_ids.is_empty() && log_measure_ids.contains(&measure_box.id) {
                        let seq = LOG_MEASURE_COUNTS
                            .get_or_init(|| Mutex::new(HashMap::new()))
                            .lock()
                            .ok()
                            .and_then(|mut counts| {
                                let entry = counts.entry(measure_box.id).or_insert(0);
                                (*entry < 3).then(|| {
                                    *entry += 1;
                                    *entry
                                })
                            });
                        if let Some(seq) = seq {
                            let selector = measure_box
                                .debug_info
                                .as_ref()
                                .map(|d| d.to_selector())
                                .unwrap_or_else(|| "<anon>".to_string());
                            eprintln!(
                                "[flex-measure] seq={} id={} selector={} display={:?} basis={:?} width_decl={:?} avail_w={:?} known_w={:?} avail_after={:?}",
                                seq,
                                measure_box.id,
                                selector,
                                measure_style.display,
                                measure_style.flex_basis,
                                measure_style.width,
                                avail.width,
                                known_dimensions.width,
                                avail.width,
                            );
                        }
                    }
                    let mut constraints = this.constraints_from_taffy(known_dimensions, avail, None);
                    if log_small_avail {
                        if let CrateAvailableSpace::Definite(w) = constraints.available_width {
                            if w > 0.0 && w <= 100.0 {
                                let selector = measure_box
                                    .debug_info
                                    .as_ref()
                                    .map(|d| d.to_selector())
                                    .unwrap_or_else(|| "<anon>".to_string());
                                eprintln!(
                                    "[flex-avail-small] id={} selector={} known_w={:?} known_h={:?} avail_w={:?} avail_h={:?} constraint_w={:?} constraint_h={:?} width_decl={:?} min_w={:?} max_w={:?} fc={:?}",
                                    measure_box.id,
                                    selector,
                                    known_dimensions.width,
                                    known_dimensions.height,
                                    avail.width,
                                    avail.height,
                                    constraints.available_width,
                                    constraints.available_height,
                                    measure_style.width,
                                    measure_style.min_width,
                                    measure_style.max_width,
                                    fc_type,
                                );
                            }
                        }
                    }
                    if log_skinny {
                        let mut cw_log = None;
                        if let CrateAvailableSpace::Definite(w) = constraints.available_width {
                            if w <= 1.0 {
                                cw_log = Some(w);
                            }
                        }
                        if let Some(w) = cw_log {
                            let selector = measure_box
                                .debug_info
                                .as_ref()
                                .map(|d| d.to_selector())
                                .unwrap_or_else(|| "<anon>".to_string());
                            eprintln!(
                                "[skinny-flex-root-constraint] id={} selector={} known_w={:?} avail_w={:?} constraint_w={:.1} width_decl={:?}",
                                measure_box.id, selector, known_dimensions.width, avail.width, w, measure_style.width
                            );
                        }
                    }

                    // Replaced elements don't establish a formatting context; compute their
                    // intrinsic/used size directly to avoid block layout inflating widths.
                    if let crate::tree::box_tree::BoxType::Replaced(replaced_box) = &measure_box.box_type {
                        let avail_width = known_dimensions.width.or(match avail.width {
                            AvailableSpace::Definite(w) => Some(w),
                            _ => None,
                        }).unwrap_or(this.viewport_size.width);
                        let avail_height = known_dimensions.height.or(match avail.height {
                            AvailableSpace::Definite(h) => Some(h),
                            _ => None,
                        }).unwrap_or(this.viewport_size.height);
                        let percentage_base = Some(Size::new(avail_width, avail_height));
                        let size = crate::layout::utils::compute_replaced_size(
                            measure_style,
                            replaced_box,
                            percentage_base,
                            this.viewport_size,
                        );
                        flex_profile::record_measure_time(measure_timer);
                        return taffy::geometry::Size {
                            width: size.width,
                            height: size.height,
                        };
                    }

                    let fc: Arc<dyn FormattingContext> = if matches!(fc_type, FormattingContextType::Block) {
                        flex_item_block_fc.clone()
                    } else {
                        factory.get(fc_type)
                    };
                    // Fit-content depends on the available space passed by Taffy. Resolve it here (per
                    // measure call) instead of during style conversion so cached templates remain valid.
                    let mut fit_border_box_width: Option<f32> = None;
                    let mut fit_border_box_height: Option<f32> = None;
                    // Border-box insets used to convert a resolved border-box length to the content-box
                    // size that Taffy expects from the measure callback.
                    let mut fit_inset_w: f32 = 0.0;
                    let mut fit_inset_h: f32 = 0.0;

                    let fit_width_limit = match (known_dimensions.width, measure_style.width_keyword) {
                      (None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
                      _ => None,
                    };
                    let fit_height_limit = match (known_dimensions.height, measure_style.height_keyword) {
                      (None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
                      _ => None,
                    };
                    let width_is_fit_content = fit_width_limit.is_some();
                    let height_is_fit_content = fit_height_limit.is_some();

                    if fit_width_limit.is_some() || fit_height_limit.is_some() {
                      let percentage_base = this.viewport_size.width.max(0.0);
                      let reserve_scroll_x = matches!(measure_style.overflow_x, CssOverflow::Scroll)
                        || (measure_style.scrollbar_gutter.stable
                          && matches!(measure_style.overflow_x, CssOverflow::Auto | CssOverflow::Scroll));
                      let reserve_scroll_y = matches!(measure_style.overflow_y, CssOverflow::Scroll)
                        || (measure_style.scrollbar_gutter.stable
                          && matches!(measure_style.overflow_y, CssOverflow::Auto | CssOverflow::Scroll));
                      let scrollbar_width = resolve_scrollbar_width(measure_style);

                      let padding_left = this.resolve_length_for_width(
                        measure_style.padding_left,
                        percentage_base,
                        measure_style,
                      );
                      let padding_right = this.resolve_length_for_width(
                        measure_style.padding_right,
                        percentage_base,
                        measure_style,
                      );
                      let padding_top = this.resolve_length_for_width(
                        measure_style.padding_top,
                        percentage_base,
                        measure_style,
                      );
                      let padding_bottom = this.resolve_length_for_width(
                        measure_style.padding_bottom,
                        percentage_base,
                        measure_style,
                      );
                      let border_left = this.resolve_length_for_width(
                        measure_style.used_border_left_width(),
                        percentage_base,
                        measure_style,
                      );
                      let border_right = this.resolve_length_for_width(
                        measure_style.used_border_right_width(),
                        percentage_base,
                        measure_style,
                      );
                      let border_top = this.resolve_length_for_width(
                        measure_style.used_border_top_width(),
                        percentage_base,
                        measure_style,
                      );
                      let border_bottom = this.resolve_length_for_width(
                        measure_style.used_border_bottom_width(),
                        percentage_base,
                        measure_style,
                      );
                      let edges_w = padding_left + padding_right + border_left + border_right;
                      let edges_h = padding_top + padding_bottom + border_top + border_bottom;
                      fit_inset_w = edges_w + if reserve_scroll_y { scrollbar_width } else { 0.0 };
                      fit_inset_h = edges_h + if reserve_scroll_x { scrollbar_width } else { 0.0 };

                      // Avoid self-recursion when computing intrinsic sizes for a fit-content axis by
                      // clearing the corresponding preferred size property before calling into
                      // intrinsic APIs.
                      let fit_width_override = fit_width_limit.is_some().then(|| {
                        let mut override_style: ComputedStyle = (*measure_style).clone();
                        override_style.width = None;
                        override_style.width_keyword = None;
                        Arc::new(override_style)
                      });
                      let fit_height_override = fit_height_limit.is_some().then(|| {
                        let mut override_style: ComputedStyle = (*measure_style).clone();
                        override_style.height = None;
                        override_style.height_keyword = None;
                        Arc::new(override_style)
                      });

                      let override_for_axis = |axis: Axis| -> Option<Arc<ComputedStyle>> {
                        match axis {
                          Axis::Horizontal => fit_width_override.clone().or_else(|| override_style.clone()),
                          Axis::Vertical => fit_height_override.clone().or_else(|| override_style.clone()),
                        }
                      };

                      let intrinsic_range_for_physical_axis =
                        |axis: Axis| -> Result<(f32, f32), LayoutError> {
                          let physical_axis = match axis {
                            Axis::Horizontal => PhysicalAxis::X,
                            Axis::Vertical => PhysicalAxis::Y,
                          };
                          let axis_override = override_for_axis(axis);
                          if let Some(style) = axis_override {
                            crate::layout::style_override::with_style_override(
                              measure_box.id,
                              style,
                              || {
                                crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
                                  fc.as_ref(),
                                  measure_box,
                                  physical_axis,
                                )
                              },
                            )
                          } else {
                            crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
                              fc.as_ref(),
                              measure_box,
                              physical_axis,
                            )
                          }
                        };

                      let compute_fit_border_box =
                        |axis: Axis, limit: Option<Length>, avail_dim: AvailableSpace| -> Result<f32, LayoutError> {
                          let (min_intrinsic, max_intrinsic) =
                            intrinsic_range_for_physical_axis(axis)?;
                          let min_intrinsic = min_intrinsic.max(0.0);
                          let max_intrinsic = max_intrinsic.max(0.0);
                          let axis_inset = match axis {
                            Axis::Horizontal => fit_inset_w,
                            Axis::Vertical => fit_inset_h,
                          };

                          let available_border_box = match avail_dim {
                            AvailableSpace::Definite(v) => (v + axis_inset).max(0.0),
                            AvailableSpace::MinContent => min_intrinsic,
                            AvailableSpace::MaxContent => max_intrinsic,
                          };

                          let preferred_border_box = match limit {
                            None => None,
                            Some(arg) => {
                              let base_content = match avail_dim {
                                AvailableSpace::Definite(v) => v.max(0.0),
                                _ => (available_border_box - axis_inset).max(0.0),
                              };
                              let resolved =
                                this.resolve_length_for_width(arg, base_content, measure_style).max(0.0);
                              Some(if measure_style.box_sizing == BoxSizing::ContentBox {
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

                          // Apply authored min/max constraints on the axis, including intrinsic keyword
                          // constraints. These clamp the fit-content result.
                          let percentage_base_opt = match avail_dim {
                            AvailableSpace::Definite(v) => Some(v.max(0.0)),
                            _ => None,
                          };
                          let resolve_length_px = |len: Length| -> Option<f32> {
                            if len.has_percentage() && percentage_base_opt.is_none() {
                              return None;
                            }
                            let base = percentage_base_opt.unwrap_or(this.viewport_size.width.max(0.0));
                            Some(this.resolve_length_for_width(len, base, measure_style).max(0.0))
                          };
                          let to_border_box = |value: f32| -> f32 {
                            if measure_style.box_sizing == BoxSizing::ContentBox {
                              (value + axis_inset).max(0.0)
                            } else {
                              value.max(0.0)
                            }
                          };

                          let (author_min_len, author_max_len, author_min_kw, author_max_kw) =
                            match axis {
                              Axis::Horizontal => (
                                measure_style.min_width,
                                measure_style.max_width,
                                measure_style.min_width_keyword,
                                measure_style.max_width_keyword,
                              ),
                              Axis::Vertical => (
                                measure_style.min_height,
                                measure_style.max_height,
                                measure_style.min_height_keyword,
                                measure_style.max_height_keyword,
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

                          let author_min = author_min_kw
                            .and_then(keyword_to_bound)
                            .or_else(|| author_min_len.and_then(resolve_length_px).map(to_border_box));
                          let author_max = author_max_kw
                            .and_then(keyword_to_bound)
                            .or_else(|| author_max_len.and_then(resolve_length_px).map(to_border_box));
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
                        match compute_fit_border_box(Axis::Horizontal, limit, avail.width) {
                          Ok(border_box) if border_box.is_finite() => {
                            fit_border_box_width = Some(border_box);
                            constraints.used_border_box_width = Some(border_box);
                          }
                          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                          Err(_) => {}
                          _ => {}
                        }
                      }

                      if let Some(limit) = fit_height_limit {
                        match compute_fit_border_box(Axis::Vertical, limit, avail.height) {
                          Ok(border_box) if border_box.is_finite() => {
                            fit_border_box_height = Some(border_box);
                            constraints.used_border_box_height = Some(border_box);
                          }
                          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                          Err(_) => {}
                          _ => {}
                        }
                      }
                    }

                    // Intrinsic width probes (`AvailableSpace::{MinContent,MaxContent}`) are used by
                    // Taffy during flex base-size resolution. They do not need a fully laid out
                    // fragment tree (we can't reuse those fragments for final placement anyway),
                    // and laying out large subtrees purely to answer an intrinsic query is
                    // disproportionately expensive on real-world pages (notably stripe.com).
                    //
                    // When possible, satisfy these probes via the formatting context's intrinsic
                    // sizing API, which is heavily cached and avoids constructing fragment trees.
                    let width_is_intrinsic_probe = known_dimensions.width.is_none()
                      && matches!(
                        avail.width,
                        AvailableSpace::MinContent | AvailableSpace::MaxContent
                      );
                    if width_is_intrinsic_probe
                      && crate::style::inline_axis_is_horizontal(measure_style.writing_mode)
                    {
                      let mode = match avail.width {
                        AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
                        AvailableSpace::MaxContent => IntrinsicSizingMode::MaxContent,
                        _ => unreachable!("width_is_intrinsic_probe guarded avail.width"),
                      };
                      if let Some(border_box_width) = fit_border_box_width {
                        // Fit-content sizing is already resolved in border-box terms. Convert to
                        // the content-box size that Taffy expects from the measure callback.
                        let mut content_w = (border_box_width - fit_inset_w).max(0.0);
                        // Mirror the clamp behavior from the intrinsic fast path to avoid runaway
                        // widths propagating through flex sizing.
                        content_w = content_w.min(this.viewport_size.width.max(0.0));

                        // Preserve the existing block-size hint behavior so baseline alignment
                        // doesn't collapse when Taffy asks for intrinsic inline sizes.
                        let mut content_h = fallback_size(known_dimensions.height, avail.height);
                        let eps = 0.01;
                        if content_h <= eps {
                          let intrinsic_block_result = if let Some(style) = override_style.clone() {
                            crate::layout::style_override::with_style_override(
                              measure_box.id,
                              style,
                              || fc.compute_intrinsic_block_size(measure_box, mode),
                            )
                          } else {
                            fc.compute_intrinsic_block_size(measure_box, mode)
                          };
                          match intrinsic_block_result {
                            Ok(border_box_block) => {
                              content_h = (border_box_block - fit_inset_h).max(0.0);
                            }
                            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                            Err(_) => {}
                          }
                        }

                        let max_h_bound = match avail.height {
                          AvailableSpace::Definite(h) => h,
                          _ => this.viewport_size.height,
                        };
                        if content_h.is_finite() {
                          content_h = content_h.min(max_h_bound.max(0.0));
                        } else {
                          content_h = max_h_bound.max(0.0);
                        }

                        flex_profile::record_measure_time(measure_timer);
                        return taffy::geometry::Size {
                          width: content_w.max(0.0),
                          height: content_h.max(0.0),
                        };
                      }
                      #[cfg(test)]
                      if !log_measure_ids.is_empty() && log_measure_ids.contains(&measure_box.id) {
                        // Mirror the debug-only max-content hint accounting used by the full
                        // layout path so unit tests can detect when intrinsic sizing work is
                        // performed specifically for flex-measure logging.
                        record_flex_measure_inline_hint_call();
                      }
                      let intrinsic_result = if let Some(style) = override_style.clone() {
                        crate::layout::style_override::with_style_override(
                          measure_box.id,
                          style,
                          || fc.compute_intrinsic_inline_size(measure_box, mode),
                        )
                      } else {
                        fc.compute_intrinsic_inline_size(measure_box, mode)
                      };
                      match intrinsic_result {
                        Ok(border_box_width) => {
                          // `compute_intrinsic_inline_size` returns a border-box size. Convert to a
                          // content-box size because Taffy adds padding/border/scrollbars from the
                          // style when computing the used border-box size for the flex item.
                          let percentage_base = this.viewport_size.width.max(0.0);
                          let reserve_scroll_x = matches!(measure_style.overflow_x, CssOverflow::Scroll)
                            || (measure_style.scrollbar_gutter.stable
                              && matches!(
                                measure_style.overflow_x,
                                CssOverflow::Auto | CssOverflow::Scroll
                              ));
                          let reserve_scroll_y = matches!(measure_style.overflow_y, CssOverflow::Scroll)
                            || (measure_style.scrollbar_gutter.stable
                              && matches!(
                                measure_style.overflow_y,
                                CssOverflow::Auto | CssOverflow::Scroll
                              ));
                          let scrollbar_width = resolve_scrollbar_width(measure_style);

                          // `compute_intrinsic_inline_size` and `compute_intrinsic_block_size`
                          // both return *border-box* sizes. Convert to a content-box size because
                          // Taffy applies padding/border/scrollbar gutters from the style when it
                          // computes the used border-box for flex items.
                          let padding_left = this.resolve_length_for_width(
                            measure_style.padding_left,
                            percentage_base,
                            measure_style,
                          );
                          let padding_right = this.resolve_length_for_width(
                            measure_style.padding_right,
                            percentage_base,
                            measure_style,
                          );
                          let padding_top = this.resolve_length_for_width(
                            measure_style.padding_top,
                            percentage_base,
                            measure_style,
                          );
                          let padding_bottom = this.resolve_length_for_width(
                            measure_style.padding_bottom,
                            percentage_base,
                            measure_style,
                          );
                          let border_left = this.resolve_length_for_width(
                            measure_style.used_border_left_width(),
                            percentage_base,
                            measure_style,
                          );
                          let border_right = this.resolve_length_for_width(
                            measure_style.used_border_right_width(),
                            percentage_base,
                            measure_style,
                          );
                          let border_top = this.resolve_length_for_width(
                            measure_style.used_border_top_width(),
                            percentage_base,
                            measure_style,
                          );
                          let border_bottom = this.resolve_length_for_width(
                            measure_style.used_border_bottom_width(),
                            percentage_base,
                            measure_style,
                          );
                          let extra_w = padding_left
                            + padding_right
                            + border_left
                            + border_right
                            + if reserve_scroll_y {
                              scrollbar_width
                            } else {
                              0.0
                            };
                          let extra_h = padding_top
                            + padding_bottom
                            + border_top
                            + border_bottom
                            + if reserve_scroll_x {
                              scrollbar_width
                            } else {
                              0.0
                            };
                          let mut content_w = (border_box_width - extra_w).max(0.0);
                          // Mirror the clamp behavior from the full layout path to avoid runaway
                          // intrinsic sizes propagating through flex sizing.
                          content_w = content_w.min(this.viewport_size.width.max(0.0));

                          // The intrinsic-width probe is primarily about the inline size, but Taffy
                          // still consumes the returned block size when resolving the flex line's
                          // cross size (notably for baseline alignment). Returning 0 here can
                          // collapse the line height, so compute an intrinsic block size when the
                          // caller hasn't provided a definite height.
                          let mut content_h = fallback_size(known_dimensions.height, avail.height);
                          let eps = 0.01;
                          if content_h <= eps {
                            let intrinsic_block_result = if let Some(style) = override_style.clone() {
                              crate::layout::style_override::with_style_override(
                                measure_box.id,
                                style,
                                || fc.compute_intrinsic_block_size(measure_box, mode),
                              )
                            } else {
                              fc.compute_intrinsic_block_size(measure_box, mode)
                            };
                            match intrinsic_block_result {
                              Ok(border_box_block) => {
                                content_h = (border_box_block - extra_h).max(0.0);
                              }
                              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                              Err(_) => {}
                            }
                          }

                          let max_h_bound = match avail.height {
                            AvailableSpace::Definite(h) => h,
                            _ => this.viewport_size.height,
                          };
                          if content_h.is_finite() {
                            content_h = content_h.min(max_h_bound.max(0.0));
                          } else {
                            content_h = max_h_bound.max(0.0);
                          }

                          flex_profile::record_measure_time(measure_timer);
                          return taffy::geometry::Size {
                            width: content_w.max(0.0),
                            height: content_h.max(0.0),
                          };
                        }
                        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                        Err(_) => {
                          // Fall through to the full layout path below.
                        }
                      }
                    }
                    let node_timer = flex_profile::node_timer();
                    let selector_for_profile = node_timer
                      .as_ref()
                      .and_then(|_| measure_box.debug_info.as_ref().map(|d| d.to_selector()));
                    #[cfg(test)]
                    record_flex_measure_layout_call(measure_box.id);
                    let layout_result = if let Some(style) = override_style.clone() {
                      crate::layout::style_override::with_style_override(measure_box.id, style, || {
                        fc.layout(measure_box, &constraints)
                      })
                    } else {
                      fc.layout(measure_box, &constraints)
                    };
                    let fragment = match layout_result {
                         Ok(f) => {
                               flex_profile::record_node_layout(
                                     measure_box.id,
                                    selector_for_profile.as_deref(),
                                   node_timer,
                              );
                             f
                         }
                         Err(LayoutError::Timeout { .. }) => {
                             flex_profile::record_node_layout(
                                 measure_box.id,
                                 selector_for_profile.as_deref(),
                                 node_timer,
                             );
                             flex_profile::record_measure_time(measure_timer);
                             taffy::abort_layout_now();
                         }
                         Err(_) => {
                             flex_profile::record_node_layout(
                                 measure_box.id,
                                 selector_for_profile.as_deref(),
                                node_timer,
                            );
                            let size = taffy::geometry::Size {
                                width: fallback_size(known_dimensions.width, avail.width),
                                height: fallback_size(known_dimensions.height, avail.height),
                            };
                            flex_profile::record_measure_time(measure_timer);
                            return size;
                        }
                    };

                    let percentage_base = match avail.width {
                        AvailableSpace::Definite(w) => w,
                        _ => constraints.width().unwrap_or_else(|| fragment.bounds.width()),
                    };
                    let mut content_size = this.content_box_size(&fragment, &box_node.style, percentage_base);
                    // For fit-content, the fit computation operates on border-box sizes. Override the
                    // measured content-box size so that Taffy sees the content size corresponding to
                    // the clamped fit-content border-box size (subtracting padding/border/scrollbar
                    // gutters).
                    if let Some(border_box) = fit_border_box_width {
                      content_size.width = (border_box - fit_inset_w).max(0.0);
                    }
                    if let Some(border_box) = fit_border_box_height {
                      content_size.height = (border_box - fit_inset_h).max(0.0);
                    }
                    let eps = 0.01;
                    let mut subtree_deadline_counter = 0usize;
                    let mut intrinsic_size = Size::new(0.0, 0.0);
                    let needs_intrinsic_size = content_size.width <= eps
                      || content_size.height <= eps
                      || log_skinny
                      || (!log_measure_ids.is_empty() && log_measure_ids.contains(&measure_box.id));
                    if needs_intrinsic_size {
                      // Guard against zero-sized measurements when the fragment actually has content.
                      intrinsic_size =
                        match Self::fragment_subtree_size(&fragment, &mut subtree_deadline_counter) {
                          Ok(size) => size,
                          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                          Err(_) => Size::new(0.0, 0.0),
                        };
                      if content_size.width <= eps && intrinsic_size.width > eps {
                        content_size.width = intrinsic_size.width;
                      }
                      if content_size.height <= eps && intrinsic_size.height > eps {
                        content_size.height = intrinsic_size.height;
                      }
                    }
                    if matches!(
                        constraints.available_width,
                        CrateAvailableSpace::MaxContent | CrateAvailableSpace::MinContent
                    ) && physical_width_is_auto(measure_style)
                        && physical_min_width_is_auto(measure_style)
                        && physical_max_width_is_auto(measure_style)
                    {
                        let descendant_span =
                          match Self::fragment_descendant_span(&fragment, &mut subtree_deadline_counter) {
                            Ok(span) => span,
                            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                            Err(_) => None,
                          };
                        if let Some(span) = descendant_span {
                            if span.width > eps
                                && content_size.width > span.width + 0.5
                                && span.width < this.viewport_size.width - eps
                            {
                                content_size.width = span.width;
                            }
                            if span.height > eps && content_size.height > span.height + 0.5 {
                                content_size.height = span.height;
                            }
                        }
                    }

                    // Respect author min/max sizing when available, and clamp runaway intrinsic
                    // sizes when Taffy requests max-content/min-content space without a definite
                    // constraint. This prevents flex items from ballooning to multi-thousand-pixel
                    // widths that then propagate through min-content sizing.
                    let percentage_base_w = match avail.width {
                        AvailableSpace::Definite(w) => Some(w),
                        _ => known_dimensions.width,
                    };
                    let percentage_base_h = match avail.height {
                        AvailableSpace::Definite(h) => Some(h),
                        _ => known_dimensions.height,
                    };
                    let resolve_if_base = |len: &Length, base: Option<f32>| {
                        base.map(|b| this.resolve_length_for_width(*len, b, measure_style))
                    };
                    let resolved_max_w = measure_style
                        .max_width
                        .as_ref()
                        .and_then(|l| resolve_if_base(l, percentage_base_w));
                    let resolved_min_w = measure_style
                        .min_width
                        .as_ref()
                        .and_then(|l| resolve_if_base(l, percentage_base_w));
                    let resolved_max_h = measure_style
                        .max_height
                        .as_ref()
                        .and_then(|l| resolve_if_base(l, percentage_base_h));
                    let resolved_min_h = measure_style
                        .min_height
                        .as_ref()
                        .and_then(|l| resolve_if_base(l, percentage_base_h));
                    let mut max_w_bound = resolved_max_w.unwrap_or_else(|| match avail.width {
                        AvailableSpace::Definite(w) => {
                          if width_is_fit_content {
                            this.viewport_size.width
                          } else {
                            w.min(this.viewport_size.width)
                          }
                        }
                        _ => this.viewport_size.width,
                    });
                    let min_w_bound = resolved_min_w.unwrap_or(0.0);
                    if max_w_bound < min_w_bound {
                        max_w_bound = min_w_bound;
                    }
                    content_size.width = crate::layout::utils::clamp_with_order(
                        content_size.width,
                        min_w_bound,
                        max_w_bound,
                    );

                    let mut max_h_bound = resolved_max_h.unwrap_or(match avail.height {
                        AvailableSpace::Definite(h) => {
                          if height_is_fit_content {
                            this.viewport_size.height
                          } else {
                            h
                          }
                        }
                        _ => this.viewport_size.height,
                    });
                    let min_h_bound = resolved_min_h.unwrap_or(0.0);
                    if max_h_bound < min_h_bound {
                        max_h_bound = min_h_bound;
                    }
                    content_size.height = crate::layout::utils::clamp_with_order(
                        content_size.height,
                        min_h_bound,
                        max_h_bound,
                    );

                    if log_skinny
                        && (content_size.width <= 1.0
                            || intrinsic_size.width <= 1.0
                            || matches!(avail.width, AvailableSpace::Definite(w) if w <= 1.0))
                    {
                        let selector = measure_box
                            .debug_info
                            .as_ref()
                            .map(|d| d.to_selector())
                            .unwrap_or_else(|| "<anon>".to_string());
                        eprintln!(
                            "[skinny-flex-measure] id={} selector={} known=({:?},{:?}) avail=({:?},{:?}) content=({:.2},{:.2}) intrinsic=({:.2},{:.2}) min=({:.2},{:.2}) max=({:.2},{:.2})",
                            measure_box.id,
                            selector,
                            known_dimensions.width,
                            known_dimensions.height,
                            avail.width,
                            avail.height,
                            content_size.width,
                            content_size.height,
                            intrinsic_size.width,
                            intrinsic_size.height,
                            min_w_bound,
                            min_h_bound,
                            max_w_bound,
                            max_h_bound,
                        );
                    }

                     if !log_measure_ids.is_empty() && log_measure_ids.contains(&measure_box.id) {
                         let seq = LOG_MEASURE_COUNTS
                             .get_or_init(|| Mutex::new(HashMap::new()))
                             .lock()
                            .ok()
                            .and_then(|mut counts| {
                                let entry = counts.entry(measure_box.id).or_insert(0);
                                (*entry < 3).then(|| {
                                    *entry += 1;
                                    *entry
                                })
                            });
                         if let Some(seq) = seq {
                             let selector = measure_box
                                 .debug_info
                                 .as_ref()
                                 .map(|d| d.to_selector())
                                 .unwrap_or_else(|| "<anon>".to_string());
                             let intrinsic_inline_hint = if matches!(
                               constraints.available_width,
                               CrateAvailableSpace::MaxContent | CrateAvailableSpace::MinContent
                             ) {
                               #[cfg(test)]
                               record_flex_measure_inline_hint_call();
                               match fc.compute_intrinsic_inline_size(
                                 measure_box,
                                 IntrinsicSizingMode::MaxContent,
                               ) {
                                 Ok(size) => Some(size),
                                 Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                                 Err(_) => None,
                               }
                             } else {
                               None
                             };
                             eprintln!(
                                 "[flex-measure-result] seq={} id={} selector={} avail=({:?},{:?}) known=({:?},{:?}) constraints=({:?},{:?}) content=({:.2},{:.2}) intrinsic=({:.2},{:.2}) min=({:.2},{:.2}) max=({:.2},{:.2}) inline_hint={:?}",
                                 seq,
                                 measure_box.id,
                                 selector,
                                avail.width,
                                avail.height,
                                known_dimensions.width,
                                known_dimensions.height,
                                constraints.available_width,
                                constraints.available_height,
                                content_size.width,
                                content_size.height,
                                intrinsic_size.width,
                                intrinsic_size.height,
                                min_w_bound,
                                min_h_bound,
                                 max_w_bound,
                                 max_h_bound,
                                 intrinsic_inline_hint,
                              );
                          }
                      }

                    let measured_size =
                      Size::new(content_size.width.max(0.0), content_size.height.max(0.0));
                    let border_size = {
                      let reserve_scroll_x = matches!(measure_style.overflow_x, CssOverflow::Scroll)
                        || (measure_style.scrollbar_gutter.stable
                          && matches!(
                            measure_style.overflow_x,
                            CssOverflow::Auto | CssOverflow::Scroll
                          ));
                      let reserve_scroll_y = matches!(measure_style.overflow_y, CssOverflow::Scroll)
                        || (measure_style.scrollbar_gutter.stable
                          && matches!(
                            measure_style.overflow_y,
                            CssOverflow::Auto | CssOverflow::Scroll
                          ));
                      let scrollbar_width = resolve_scrollbar_width(measure_style);
                      let padding_left = this.resolve_length_for_width(measure_style.padding_left, percentage_base, measure_style);
                      let padding_right = this.resolve_length_for_width(measure_style.padding_right, percentage_base, measure_style);
                      let padding_top = this.resolve_length_for_width(measure_style.padding_top, percentage_base, measure_style);
                      let padding_bottom =
                        this.resolve_length_for_width(measure_style.padding_bottom, percentage_base, measure_style);
                      let border_left = this.resolve_length_for_width(measure_style.used_border_left_width(), percentage_base, measure_style);
                      let border_right =
                        this.resolve_length_for_width(measure_style.used_border_right_width(), percentage_base, measure_style);
                      let border_top = this.resolve_length_for_width(measure_style.used_border_top_width(), percentage_base, measure_style);
                      let border_bottom =
                        this.resolve_length_for_width(measure_style.used_border_bottom_width(), percentage_base, measure_style);
                      let extra_w = padding_left + padding_right + border_left + border_right + if reserve_scroll_y { scrollbar_width } else { 0.0 };
                      let extra_h = padding_top + padding_bottom + border_top + border_bottom + if reserve_scroll_x { scrollbar_width } else { 0.0 };
                      Size::new((measured_size.width + extra_w).max(0.0), (measured_size.height + extra_h).max(0.0))
                    };
                    let pass_entry_vacant = node_ptr.is_some()
                      && !pass_cache
                        .get(&cache_key)
                        .map(|entry| entry.contains_key(&key))
                        .unwrap_or(false);
                    let mut fragment = Some(fragment);
                    let mut normalized_fragment: Option<std::sync::Arc<FragmentNode>> = None;
                    let normalize_for_cache = |fragment: &mut Option<FragmentNode>,
                                                   normalized_fragment: &mut Option<
                        std::sync::Arc<FragmentNode>,
                      >| {
                        normalized_fragment
                           .get_or_insert_with(|| {
                             let mut fragment = fragment.take().expect("fragment already normalized");
                             let mut deadline_counter = 0usize;
                             if let Err(err) = normalize_fragment_origin(&mut fragment, &mut deadline_counter) {
                               if matches!(err, LayoutError::Timeout { .. }) {
                                 taffy::abort_layout_now();
                               }
                             }
                             std::sync::Arc::new(fragment)
                           })
                           .clone()
                       };

                    if measured_fragments.get(cache_key, &key).is_none() {
                      let normalized_fragment =
                        normalize_for_cache(&mut fragment, &mut normalized_fragment);
                      let inserted = measured_fragments.insert(
                        cache_key,
                        key,
                        crate::layout::contexts::flex_cache::FlexCacheValue {
                          measured_size,
                          border_size,
                          fragment: normalized_fragment,
                        },
                        MAX_MEASURE_CACHE_PER_NODE,
                      );
                      flex_profile::record_measure_store(inserted);
                      if inserted {
                        record_node_measure_store(measure_box.id);
                      }
                    } else {
                      flex_profile::record_measure_store(false);
                    }
                    if pass_entry_vacant {
                      let entry = pass_cache.entry(cache_key).or_default();
                        if let Entry::Vacant(e) = entry.entry(key) {
                          let normalized_fragment =
                            normalize_for_cache(&mut fragment, &mut normalized_fragment);
                          e.insert(crate::layout::contexts::flex_cache::FlexCacheValue {
                            measured_size,
                            border_size,
                            fragment: normalized_fragment,
                          });
                          record_node_measure_store(measure_box.id);
                        }
                      }

                    let size = taffy::geometry::Size {
                        width: measured_size.width,
                        height: measured_size.height,
                    };
                    flex_profile::record_measure_time(measure_timer);
                    size
                },
        cancel.clone(),
        TAFFY_ABORT_CHECK_STRIDE,
      );
      if let Some(start) = taffy_compute_start {
        record_taffy_compute(TaffyAdapterKind::Flex, start.elapsed());
      }
      compute_result.map_err(|e| match e {
        taffy::TaffyError::LayoutAborted => match active_deadline() {
          Some(deadline) => LayoutError::Timeout {
            elapsed: deadline.elapsed(),
          },
          None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
        },
        _ => LayoutError::MissingContext(format!("Taffy layout failed: {:?}", e)),
      })?;

      if auto_item_count == 0 || is_last_pass {
        break;
      }

      let root_layout = taffy_tree
        .layout(root_node)
        .map_err(|e| LayoutError::MissingContext(format!("Failed to get Taffy layout: {:?}", e)))?;
      let root_origin_x = root_layout.location.x;
      let root_origin_y = root_layout.location.y;

      // Resolve the viewport rectangle in the flex container's coordinate system. Nested
      // formatting contexts translate the factory's viewport scroll offset so we can keep this
      // intersection test local.
      let scroll = self.factory.viewport_scroll();
      let scroll = if scroll.x.is_finite() && scroll.y.is_finite() {
        scroll
      } else {
        Point::ZERO
      };
      let activation_margin = toggles
        .f64("FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX")
        .unwrap_or(0.0)
        .max(0.0) as f32;
      let mut viewport_rect = Rect::from_xywh(
        scroll.x,
        scroll.y,
        viewport_size.width,
        viewport_size.height,
      );
      if activation_margin > 0.0 {
        viewport_rect = viewport_rect.inflate(activation_margin);
      }

      let cb_width = if root_layout.size.width.is_finite() {
        root_layout.size.width.max(0.0)
      } else {
        viewport_size.width
      };
      let cb_height = if root_layout.size.height.is_finite() {
        root_layout.size.height.max(0.0)
      } else {
        viewport_size.height
      };

      // Relative positioning offsets should be included in the viewport intersection decision:
      // a relative shift can move an otherwise in-flow box offscreen while preserving its space in
      // layout.
      let border_left =
        self.resolve_length_for_width(box_node.style.used_border_left_width(), cb_width, style);
      let border_right =
        self.resolve_length_for_width(box_node.style.used_border_right_width(), cb_width, style);
      let border_top =
        self.resolve_length_for_width(box_node.style.used_border_top_width(), cb_width, style);
      let border_bottom =
        self.resolve_length_for_width(box_node.style.used_border_bottom_width(), cb_width, style);
      let padding_left =
        self.resolve_length_for_width(box_node.style.padding_left, cb_width, style);
      let padding_right =
        self.resolve_length_for_width(box_node.style.padding_right, cb_width, style);
      let padding_top = self.resolve_length_for_width(box_node.style.padding_top, cb_width, style);
      let padding_bottom =
        self.resolve_length_for_width(box_node.style.padding_bottom, cb_width, style);
      let content_width =
        (cb_width - border_left - border_right - padding_left - padding_right).max(0.0);
      let content_height =
        (cb_height - border_top - border_bottom - padding_top - padding_bottom).max(0.0);
      let block_base = box_node.style.height.is_some().then_some(content_height);
      let containing_block = ContainingBlock::with_viewport_and_bases(
        Rect::new(Point::ZERO, Size::new(content_width, content_height)),
        viewport_size,
        Some(content_width),
        block_base,
      );
      let positioned_layout = PositionedLayout::with_font_context(self.font_context.clone());

      let mut changed = false;
      let mut newly_unskipped_nodes: Vec<(&BoxNode, NodeId)> = Vec::new();
      for (child, node_id) in auto_item_nodes.iter() {
        let ptr = *child as *const BoxNode;
        if auto_unskipped_nodes.contains(&ptr) {
          continue;
        }
        let child_layout = taffy_tree.layout(*node_id).map_err(|e| {
          LayoutError::MissingContext(format!("Failed to get Taffy layout: {:?}", e))
        })?;
        let mut child_bounds = Rect::from_xywh(
          child_layout.location.x - root_origin_x,
          child_layout.location.y - root_origin_y,
          child_layout.size.width,
          child_layout.size.height,
        );
        if child.style.position.is_relative() {
          let positioned_style = resolve_positioned_style(
            &child.style,
            &containing_block,
            viewport_size,
            &self.font_context,
          );
          let dummy = FragmentNode::new_block(child_bounds, vec![]);
          child_bounds = positioned_layout
            .apply_relative_positioning(&dummy, &positioned_style, &containing_block)?
            .bounds;
        }
        let should_unskip = viewport_rect.intersects(child_bounds);
        if should_unskip {
          auto_unskipped_nodes.insert(ptr);
          newly_unskipped_nodes.push((*child, *node_id));
          changed = true;
        }
      }

      if !changed {
        break;
      }

      for (child, node_id) in newly_unskipped_nodes {
        let mut resolved_style = self.computed_style_to_taffy_base(
          child.style.as_ref(),
          false,
          Some(style),
        )?;
        self.apply_flex_intrinsic_size_keywords(
          child,
          false,
          Some(style),
          Some(&constraints),
          &mut resolved_style,
        )?;
        let skip_contents = self.flex_item_should_skip_contents(child, &auto_unskipped_nodes);
        self.apply_flex_auto_min_size(
          child,
          false,
          Some(style),
          skip_contents,
          &mut resolved_style,
        )?;
        self.apply_flex_fit_content_keywords(
          child,
          false,
          Some(style),
          &constraints,
          &mut resolved_style,
        )?;
        taffy_tree
          .set_style(node_id, resolved_style)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }

      for (_, node_id) in auto_item_nodes.iter() {
        taffy_tree
          .mark_dirty(*node_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }
      taffy_tree
        .mark_dirty(root_node)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }
    flex_profile::record_compute_time(compute_timer);
    if toggles.truthy("FASTR_DEBUG_FLEX_CHILD") {
      if let Ok(style) = taffy_tree.style(root_node) {
        eprintln!(
          "[flex-taffy-root-style] size=({:?},{:?}) min=({:?},{:?}) max=({:?},{:?})",
          style.size.width,
          style.size.height,
          style.min_size.width,
          style.min_size.height,
          style.max_size.width,
          style.max_size.height,
        );
      }
      if let Ok(layout) = taffy_tree.layout(root_node) {
        eprintln!(
          "[flex-taffy-root-layout] size=({:.2},{:.2}) loc=({:.2},{:.2})",
          layout.size.width, layout.size.height, layout.location.x, layout.location.y,
        );
      }
    }

    // Phase 3: Convert Taffy layout back to FragmentNode
    let convert_timer = flex_profile::timer();
    let auto_unskipped = (auto_item_count > 0).then_some(&auto_unskipped_nodes);
    let mut fragment = self.taffy_to_fragment(
      &taffy_tree,
      root_node,
      root_node,
      box_node,
      &node_map,
      &constraints,
      auto_unskipped,
      &scroll_sensitive_items,
    )?;
    // Baseline alignment post-pass.
    //
    // Taffy does not know how to compute baselines from our fragment trees, so we translate
    // baseline-aligned items within each flex line here.
    //
    // IMPORTANT: Do not overwrite the cross-axis origin for non-baseline items. Taffy's positions
    // already include per-line cross offsets for wrapping; replacing them with a container-global
    // coordinate would collapse wrapped lines onto each other.
    if matches!(box_node.style.display, Display::Flex | Display::InlineFlex)
      && !fragment.children.is_empty()
    {
      let mut deadline_counter = 0usize;

      // Taffy's `flex-wrap: wrap-reverse` handling reverses the cross-axis direction when placing
      // flex lines. Our adapter maps flexbox into physical coordinates (taking writing-mode into
      // account), but downstream fragment construction expects positions in the normal top-left
      // coordinate system. Mirror the cross-axis positions inside the content box so wrapped lines
      // stack in the correct (reversed) order.
      if matches!(box_node.style.flex_wrap, FlexWrap::WrapReverse) {
        let inline_is_horizontal = matches!(box_node.style.writing_mode, WritingMode::HorizontalTb);
        let block_is_horizontal = !inline_is_horizontal;
        let main_is_inline = matches!(
          box_node.style.flex_direction,
          FlexDirection::Row | FlexDirection::RowReverse
        );
        let cross_is_horizontal = if main_is_inline {
          block_is_horizontal
        } else {
          inline_is_horizontal
        };
        let cross_size = if cross_is_horizontal {
          fragment.bounds.width()
        } else {
          fragment.bounds.height()
        };

        if cross_size.is_finite() && cross_size > 0.0 {
          let cb_width = fragment.bounds.width();
          let border_left = self.resolve_length_for_width(
            box_node.style.used_border_left_width(),
            cb_width,
            &box_node.style,
          );
          let border_right = self.resolve_length_for_width(
            box_node.style.used_border_right_width(),
            cb_width,
            &box_node.style,
          );
          let border_top = self.resolve_length_for_width(
            box_node.style.used_border_top_width(),
            cb_width,
            &box_node.style,
          );
          let border_bottom = self.resolve_length_for_width(
            box_node.style.used_border_bottom_width(),
            cb_width,
            &box_node.style,
          );
          let padding_left =
            self.resolve_length_for_width(box_node.style.padding_left, cb_width, &box_node.style);
          let padding_right =
            self.resolve_length_for_width(box_node.style.padding_right, cb_width, &box_node.style);
          let padding_top =
            self.resolve_length_for_width(box_node.style.padding_top, cb_width, &box_node.style);
          let padding_bottom = self.resolve_length_for_width(
            box_node.style.padding_bottom,
            cb_width,
            &box_node.style,
          );

          let (cross_content_start, cross_content_end) = if cross_is_horizontal {
            (border_left + padding_left, border_right + padding_right)
          } else {
            (border_top + padding_top, border_bottom + padding_bottom)
          };
          let cross_inner_size = (cross_size - cross_content_start - cross_content_end).max(0.0);

          if cross_inner_size.is_finite() && cross_inner_size > 0.0 {
            for child in fragment.children_mut() {
              check_layout_deadline(&mut deadline_counter)?;
              let (cross_pos, child_cross) = if cross_is_horizontal {
                (child.bounds.x(), child.bounds.width())
              } else {
                (child.bounds.y(), child.bounds.height())
              };
              if !cross_pos.is_finite() || !child_cross.is_finite() {
                continue;
              }
              let rel = cross_pos - cross_content_start;
              let new_pos = cross_content_start + (cross_inner_size - child_cross - rel);
              let delta = new_pos - cross_pos;
              if delta.abs() <= 1e-6 {
                continue;
              }
              if cross_is_horizontal {
                translate_fragment_tree(child, Point::new(delta, 0.0), &mut deadline_counter)?;
              } else {
                translate_fragment_tree(child, Point::new(0.0, delta), &mut deadline_counter)?;
              }
            }
          }
        }
      }

      let mut needs_baseline = matches!(box_node.style.align_items, AlignItems::Baseline);
      if !needs_baseline {
        for child in in_flow_children.iter() {
          check_layout_deadline(&mut deadline_counter)?;
          if matches!(child.style.align_self, Some(AlignItems::Baseline)) {
            needs_baseline = true;
            break;
          }
        }
      }

      if needs_baseline {
        let inline_is_horizontal = matches!(box_node.style.writing_mode, WritingMode::HorizontalTb);
        let block_is_horizontal = !inline_is_horizontal;
        let main_is_inline = matches!(
          box_node.style.flex_direction,
          FlexDirection::Row | FlexDirection::RowReverse
        );
        let main_is_horizontal = if main_is_inline {
          inline_is_horizontal
        } else {
          block_is_horizontal
        };
        let cross_is_horizontal = if main_is_inline {
          block_is_horizontal
        } else {
          inline_is_horizontal
        };
        let inline_positive = self.inline_axis_positive(&box_node.style);
        let block_positive = self.block_axis_positive(&box_node.style);
        let taffy_dir =
          self.flex_direction_to_taffy(&box_node.style, inline_positive, block_positive);
        let main_grows_positive = matches!(
          taffy_dir,
          taffy::style::FlexDirection::Row | taffy::style::FlexDirection::Column
        );

        let item_count = fragment.children.len().min(in_flow_children.len());

        let mut line_indices: Vec<usize> = Vec::with_capacity(item_count);
        if matches!(box_node.style.flex_wrap, FlexWrap::NoWrap) {
          line_indices.resize(item_count, 0);
        } else {
          let mut current_line = 0usize;
          let mut prev_main: Option<f32> = None;
          let wrap_break_eps = 0.5;
          for child in fragment.children.iter().take(item_count) {
            check_layout_deadline(&mut deadline_counter)?;
            let main_pos = if main_is_horizontal {
              child.bounds.x()
            } else {
              child.bounds.y()
            };
            if let Some(prev) = prev_main {
              let delta = main_pos - prev;
              if (main_grows_positive && delta < -wrap_break_eps)
                || (!main_grows_positive && delta > wrap_break_eps)
              {
                current_line += 1;
              }
            }
            line_indices.push(current_line);
            prev_main = Some(main_pos);
          }
        }

        #[derive(Clone, Copy, Default)]
        struct LineBaselineData {
          cross_start: f32,
          max_above: f32,
          max_below: f32,
          baseline: f32,
          has_baseline: bool,
        }

        #[derive(Clone, Copy)]
        struct BaselineItemMetrics {
          line_index: usize,
          baseline_pos: f32,
          baseline_offset: f32,
          cross_size: f32,
        }

        let mut baseline_items: Vec<Option<BaselineItemMetrics>> = vec![None; item_count];
        let line_count = line_indices.iter().copied().max().unwrap_or(0) + 1;
        let mut line_cross_starts = vec![f32::INFINITY; line_count];

        for idx in 0..item_count {
          check_layout_deadline(&mut deadline_counter)?;
          let child_node = &in_flow_children[idx];
          let align = child_node
            .style
            .align_self
            .unwrap_or(box_node.style.align_items);

          if matches!(align, AlignItems::Baseline) {
            let child_fragment = &fragment.children[idx];
            let cross_start = if cross_is_horizontal {
              child_fragment.bounds.x()
            } else {
              child_fragment.bounds.y()
            };
            let cross_size_child = if cross_is_horizontal {
              child_fragment.bounds.width()
            } else {
              child_fragment.bounds.height()
            };
            let mut baseline_offset =
              fragment_first_baseline(child_fragment, &mut deadline_counter)?
                .unwrap_or(cross_size_child);
            if !baseline_offset.is_finite() {
              baseline_offset = cross_size_child;
            }
            let baseline_offset = baseline_offset.clamp(0.0, cross_size_child);
            let baseline_pos = cross_start + baseline_offset;
            let line_idx = *line_indices.get(idx).unwrap_or(&0);
            if line_idx >= line_cross_starts.len() {
              line_cross_starts.resize(line_idx + 1, f32::INFINITY);
            }
            line_cross_starts[line_idx] = line_cross_starts[line_idx].min(cross_start);
            baseline_items[idx] = Some(BaselineItemMetrics {
              line_index: line_idx,
              baseline_pos,
              baseline_offset,
              cross_size: cross_size_child,
            });
          }
        }

        let mut line_baselines = vec![LineBaselineData::default(); line_cross_starts.len()];
        for (idx, start) in line_cross_starts.iter().enumerate() {
          check_layout_deadline(&mut deadline_counter)?;
          line_baselines[idx].cross_start = if start.is_finite() { *start } else { 0.0 };
        }

        for metrics in baseline_items.iter().flatten() {
          check_layout_deadline(&mut deadline_counter)?;
          let line_idx = metrics.line_index;
          if line_idx >= line_baselines.len() {
            line_baselines.resize(line_idx + 1, LineBaselineData::default());
          }
          let line_start = line_baselines[line_idx].cross_start;
          let above = metrics.baseline_pos - line_start;
          let below = metrics.cross_size - metrics.baseline_offset;
          let line = &mut line_baselines[line_idx];
          line.max_above = line.max_above.max(above);
          line.max_below = line.max_below.max(below);
          line.has_baseline = true;
        }

        for line in line_baselines.iter_mut() {
          check_layout_deadline(&mut deadline_counter)?;
          if line.has_baseline {
            line.cross_start = line.cross_start.max(0.0);
            line.baseline = line.cross_start + line.max_above;
          }
        }

        for (idx, child_fragment) in fragment.children_mut().iter_mut().take(item_count).enumerate()
        {
          check_layout_deadline(&mut deadline_counter)?;
          let Some(metrics) = baseline_items[idx] else {
            continue;
          };
          let Some(line) = line_baselines.get(metrics.line_index) else {
            continue;
          };
          if !line.has_baseline {
            continue;
          }
          let delta = line.baseline - metrics.baseline_pos;
          if cross_is_horizontal {
            translate_fragment_tree(
              child_fragment,
              Point::new(delta, 0.0),
              &mut deadline_counter,
            )?;
          } else {
            translate_fragment_tree(
              child_fragment,
              Point::new(0.0, delta),
              &mut deadline_counter,
            )?;
          }
        }
      }
      if toggles.truthy("FASTR_DEBUG_FLEX_CHILD") {
        eprintln!(
          "[flex-container] bounds=({:.2},{:.2},{:.2},{:.2})",
          fragment.bounds.x(),
          fragment.bounds.y(),
          fragment.bounds.width(),
          fragment.bounds.height()
        );
        for (idx, child) in fragment.children.iter().enumerate() {
          check_layout_deadline(&mut deadline_counter)?;
          eprintln!(
            "[flex-child-after-align] idx={} bounds=({:.2},{:.2},{:.2},{:.2})",
            idx,
            child.bounds.x(),
            child.bounds.y(),
            child.bounds.width(),
            child.bounds.height()
          );
        }
      }
    }
    // Clamp the container width to the definite available width when provided. A block-level
    // flex container with auto width should not expand to its max-content size; it fills the
    // containing block width. This prevents oversized flex containers from ballooning when
    // child intrinsic sizes are large.
    if let CrateAvailableSpace::Definite(w) = constraints.available_width {
      let clamped_w = w.min(self.viewport_size.width);
      fragment.bounds = Rect::new(
        fragment.bounds.origin,
        Size::new(clamped_w, fragment.bounds.height()),
      );
    } else if fragment.bounds.width() > self.viewport_size.width {
      // When width is indefinite, default to filling the viewport rather than expanding to
      // the max-content of children (which can explode with wide carousels). Block-level
      // flex containers with auto width should behave as width: auto in normal flow, i.e.
      // fill the containing block (viewport at root).
      fragment.bounds = Rect::new(
        fragment.bounds.origin,
        Size::new(self.viewport_size.width, fragment.bounds.height()),
      );
    }
    // Keep child layout positions intact even when they overflow the container; overflow handling
    // is a paint concern (via overflow clipping). Only sanitize clearly invalid or runaway values.
    if fragment.bounds.width().is_finite() && fragment.bounds.height().is_finite() {
      let max_w = fragment.bounds.width().max(0.0);
      let max_h = fragment.bounds.height().max(0.0);
      let runaway_x = max_w.max(1.0) * 20.0;
      let runaway_y = max_h.max(1.0) * 20.0;
      let mut deadline_counter = 0usize;
      for child in fragment.children_mut() {
        check_layout_deadline(&mut deadline_counter)?;
        let mut x = child.bounds.x();
        let mut y = child.bounds.y();
        let mut w = child.bounds.width();
        let mut h = child.bounds.height();
        let mut changed = false;

        if !x.is_finite() {
          x = 0.0;
          changed = true;
        }
        if !y.is_finite() {
          y = 0.0;
          changed = true;
        }
        if !w.is_finite() || w < 0.0 {
          w = 0.0;
          changed = true;
        }
        if !h.is_finite() || h < 0.0 {
          h = 0.0;
          changed = true;
        }

        let max_x = x + w;
        let max_y = y + h;
        if x.abs() > runaway_x || max_x.abs() > runaway_x {
          w = w.min(max_w);
          x = x.clamp(0.0, (max_w - w).max(0.0));
          changed = true;
        }
        if y.abs() > runaway_y || max_y.abs() > runaway_y {
          h = h.min(max_h);
          y = y.clamp(0.0, (max_h - h).max(0.0));
          changed = true;
        }

        if changed {
          child.bounds = Rect::new(Point::new(x, y), Size::new(w, h));
        }
      }
    }

    // Apply relative positioning after flex layout resolves the normal positions
    // (CSS 2.1 §9.4.3). Flex item fragments have their bounds origins overwritten
    // by the container placement logic, so the child's own formatting context
    // cannot preserve the offset.
    if in_flow_children.len() == fragment.children.len() {
      let cb_width = fragment.bounds.width();
      let cb_height = fragment.bounds.height();
      let border_left = self.resolve_length_for_width(
        box_node.style.used_border_left_width(),
        cb_width,
        &box_node.style,
      );
      let border_right = self.resolve_length_for_width(
        box_node.style.used_border_right_width(),
        cb_width,
        &box_node.style,
      );
      let border_top = self.resolve_length_for_width(
        box_node.style.used_border_top_width(),
        cb_width,
        &box_node.style,
      );
      let border_bottom = self.resolve_length_for_width(
        box_node.style.used_border_bottom_width(),
        cb_width,
        &box_node.style,
      );
      let padding_left =
        self.resolve_length_for_width(box_node.style.padding_left, cb_width, &box_node.style);
      let padding_right =
        self.resolve_length_for_width(box_node.style.padding_right, cb_width, &box_node.style);
      let padding_top =
        self.resolve_length_for_width(box_node.style.padding_top, cb_width, &box_node.style);
      let padding_bottom =
        self.resolve_length_for_width(box_node.style.padding_bottom, cb_width, &box_node.style);
      let content_width =
        (cb_width - border_left - border_right - padding_left - padding_right).max(0.0);
      let content_height =
        (cb_height - border_top - border_bottom - padding_top - padding_bottom).max(0.0);
      let block_base = box_node.style.height.is_some().then_some(content_height);
      let containing_block = ContainingBlock::with_viewport_and_bases(
        Rect::new(Point::ZERO, Size::new(content_width, content_height)),
        self.viewport_size,
        Some(content_width),
        block_base,
      );
      let positioned_layout = PositionedLayout::with_font_context(self.font_context.clone());

      for (child_node, child_fragment) in in_flow_children
        .iter()
        .zip(fragment.children_mut().iter_mut())
      {
        if !child_node.style.position.is_relative() {
          continue;
        }
        let positioned_style = resolve_positioned_style(
          &child_node.style,
          &containing_block,
          self.viewport_size,
          &self.font_context,
        );
        *child_fragment = positioned_layout.apply_relative_positioning(
          child_fragment,
          &positioned_style,
          &containing_block,
        )?;
      }
    }

    let log_wide = toggles.truthy("FASTR_LOG_WIDE_FLEX");
    let log_skinny_frag = toggles.truthy("FASTR_LOG_SKINNY_FLEX");
    let log_target_ids = toggles.usize_list("FASTR_LOG_FLEX_IDS").unwrap_or_default();
    if log_wide || log_skinny_frag || !log_target_ids.is_empty() {
      let avail_w = constraints.width();
      if fragment.bounds.width() > self.viewport_size.width + 0.5
        || avail_w
          .map(|w| w > self.viewport_size.width + 0.5)
          .unwrap_or(false)
      {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string());
        let child_ids: Vec<usize> = box_node.children.iter().take(5).map(|c| c.id).collect();
        let style = &box_node.style;
        eprintln!(
                    "[flex-wide] box_id={:?} selector={} avail_w={:?} avail_h={:?} frag_w={:.1} frag_h={:.1} viewport_w={:.1} display={:?} width={:?} min_w={:?} max_w={:?} margins=({:.1},{:.1}) children_first5={:?}",
                    box_node.id,
                    selector,
                    avail_w,
                    constraints.height(),
                    fragment.bounds.width(),
                    fragment.bounds.height(),
                    self.viewport_size.width,
                    style.display,
                    style.width,
                    style.min_width,
                    style.max_width,
                    style.margin_left.map(|l| l.to_px()).unwrap_or(0.0),
                    style.margin_right.map(|l| l.to_px()).unwrap_or(0.0),
                    child_ids,
                );
        if log_target_ids.contains(&box_node.id) {
          eprintln!(
                        "[flex-target] id={} selector={} avail_w={:?} avail_h={:?} frag_w={:.1} frag_h={:.1} bounds=({:.1},{:.1}) display={:?} width={:?} min_w={:?} max_w={:?} margins=({:.1},{:.1})",
                        box_node.id,
                        selector,
                        constraints.width(),
                        constraints.height(),
                        fragment.bounds.width(),
                        fragment.bounds.height(),
                        fragment.bounds.x(),
                        fragment.bounds.y(),
                        box_node.style.display,
                        box_node.style.width,
                        box_node.style.min_width,
                        box_node.style.max_width,
                        box_node.style.margin_left.map(|l| l.to_px()).unwrap_or(0.0),
                        box_node.style.margin_right.map(|l| l.to_px()).unwrap_or(0.0),
                    );
        }
      }
      if log_skinny_frag && fragment.bounds.width() <= 1.0 {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string());
        eprintln!(
                    "[skinny-flex-frag] box_id={:?} selector={} avail_w={:?} avail_h={:?} frag_w={:.2} frag_h={:.2} display={:?}",
                    box_node.id,
                    selector,
                    avail_w,
                    constraints.height(),
                    fragment.bounds.width(),
                    fragment.bounds.height(),
                    box_node.style.display
                );
      }
      if log_target_ids.contains(&box_node.id) {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string());
        eprintln!(
                    "[flex-target] id={} selector={} avail_w={:?} avail_h={:?} frag_w={:.1} frag_h={:.1} bounds=({:.1},{:.1}) display={:?} width={:?} min_w={:?} max_w={:?} margins=({:.1},{:.1})",
                    box_node.id,
                    selector,
                    constraints.width(),
                    constraints.height(),
                    fragment.bounds.width(),
                    fragment.bounds.height(),
                    fragment.bounds.x(),
                    fragment.bounds.y(),
                    box_node.style.display,
                    box_node.style.width,
                    box_node.style.min_width,
                    box_node.style.max_width,
                    box_node.style.margin_left.map(|l| l.to_px()).unwrap_or(0.0),
                    box_node.style.margin_right.map(|l| l.to_px()).unwrap_or(0.0),
                );
      }
    }
    flex_profile::record_convert_time(convert_timer);

    // Phase 4: Position out-of-flow abs/fixed children against this flex container.
    if !positioned_children.is_empty() {
      let positioned_factory = base_factory.clone();
      let abs = AbsoluteLayout::with_font_context(self.font_context.clone());
      let border_left = self.resolve_length_for_width(
        box_node.style.used_border_left_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_top = self.resolve_length_for_width(
        box_node.style.used_border_top_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_right = self.resolve_length_for_width(
        box_node.style.used_border_right_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );
      let border_bottom = self.resolve_length_for_width(
        box_node.style.used_border_bottom_width(),
        constraints.width().unwrap_or(0.0),
        &box_node.style,
      );

      // CSS 2.1 §10.1: the containing block for absolute positioned descendants is the
      // padding box of the nearest positioned ancestor, i.e. the rectangle bounded by the
      // padding edge (border box minus borders).
      let padding_origin = Point::new(border_left, border_top);
      let padding_size = Size::new(
        fragment.bounds.width() - border_left - border_right,
        fragment.bounds.height() - border_top - border_bottom,
      );
      let padding_rect = Rect::new(padding_origin, padding_size);

      // Percentage sizes/offsets on absolutely positioned boxes resolve against the used size
      // of the containing block, even when the containing block's own height is `auto`
      // (CSS 2.1 §10.5). Use the computed padding box height as the percentage base.
      let block_base = Some(padding_rect.size.height);
      let establishes_abs_cb = box_node.style.establishes_abs_containing_block();
      let establishes_fixed_cb = box_node.style.establishes_fixed_containing_block();
      let padding_cb = ContainingBlock::with_viewport_and_bases(
        padding_rect,
        self.viewport_size,
        Some(padding_rect.size.width),
        block_base,
      );
      let mut anchor_index =
        crate::layout::anchor_positioning::AnchorIndex::from_fragments(fragment.children_ref());
      anchor_index.insert_names(
        &box_node.style.anchor_names,
        Rect::new(Point::ZERO, fragment.bounds.size),
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

      let mut positioned_candidates: Vec<PositionedCandidate> = Vec::new();
      let mut deadline_counter = 0usize;
      for child in positioned_children {
        check_layout_deadline(&mut deadline_counter)?;
        let child_id = ensure_box_id(child);
        let original_style = child.style.clone();
        let is_replaced = child.is_replaced();
        let cb = match child.style.position {
          Position::Fixed => cb_for_fixed,
          Position::Absolute => cb_for_absolute,
          _ => cb_for_absolute,
        };

        // Layout child as static to obtain intrinsic size.
        let mut layout_child = child.clone();
        let mut style = (*layout_child.style).clone();
        style.position = crate::style::position::Position::Relative;
        style.top = crate::style::types::InsetValue::Auto;
        style.right = crate::style::types::InsetValue::Auto;
        style.bottom = crate::style::types::InsetValue::Auto;
        style.left = crate::style::types::InsetValue::Auto;
        // Keep a distinct style Arc so cache keys that hash the style fingerprint do not share
        // entries with the real positioned variant.
        layout_child.style = Arc::new(style);

        let fc_type = layout_child
          .formatting_context()
          .unwrap_or(crate::style::display::FormattingContextType::Block);
        let fc = abs_factory.get(fc_type);
        let child_constraints = LayoutConstraints::new(
          CrateAvailableSpace::Definite(padding_rect.size.width),
          block_base
            .map(CrateAvailableSpace::Definite)
            .unwrap_or(CrateAvailableSpace::Indefinite),
        );
        let child_fragment = fc.layout(&layout_child, &child_constraints)?;

        let anchors_for_cb = (cb == padding_cb).then_some(&anchor_index);
        let positioned_style = resolve_positioned_style_with_anchors(
          &original_style,
          &cb,
          self.viewport_size,
          &self.font_context,
          anchors_for_cb,
        );
        let has_inline_keyword = positioned_style.width_keyword.is_some()
          || positioned_style.min_width_keyword.is_some()
          || positioned_style.max_width_keyword.is_some();
        let has_block_keyword = positioned_style.height_keyword.is_some()
          || positioned_style.min_height_keyword.is_some()
          || positioned_style.max_height_keyword.is_some();
        let needs_inline_intrinsics = has_inline_keyword
          || (positioned_style.width.is_auto()
            && (positioned_style.left.is_auto()
              || positioned_style.right.is_auto()
              || is_replaced));
        let needs_block_intrinsics = has_block_keyword
          || (positioned_style.height.is_auto()
            && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()));
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

        positioned_candidates.push(PositionedCandidate {
          child_id,
          original_style,
          layout_child,
          cb,
          fragment: child_fragment,
          positioned_style,
          preferred_min_inline,
          preferred_inline,
          preferred_min_block,
          preferred_block,
          is_replaced,
        });
      }

      let static_positions = match self.compute_static_positions_for_abs_children(
        box_node,
        &fragment,
        &in_flow_children,
        &positioned_candidates,
        auto_unskipped,
        padding_origin,
      ) {
        Ok(positions) => positions,
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => FxHashMap::default(),
      };

      let child_constraints = LayoutConstraints::new(
        CrateAvailableSpace::Definite(padding_rect.size.width),
        block_base
          .map(CrateAvailableSpace::Definite)
          .unwrap_or(CrateAvailableSpace::Indefinite),
      );

      for candidate in positioned_candidates {
        check_layout_deadline(&mut deadline_counter)?;
        let actual_horizontal = candidate.positioned_style.padding.left
          + candidate.positioned_style.padding.right
          + candidate.positioned_style.border_width.left
          + candidate.positioned_style.border_width.right;
        let actual_vertical = candidate.positioned_style.padding.top
          + candidate.positioned_style.padding.bottom
          + candidate.positioned_style.border_width.top
          + candidate.positioned_style.border_width.bottom;
        let content_offset = Point::new(
          candidate.positioned_style.border_width.left + candidate.positioned_style.padding.left,
          candidate.positioned_style.border_width.top + candidate.positioned_style.padding.top,
        );
        let (intrinsic_horizontal, intrinsic_vertical) =
          crate::layout::absolute_positioning::intrinsic_edge_sizes(
            &candidate.original_style,
            self.viewport_size,
            &self.font_context,
          );
        let preferred_min_inline = candidate
          .preferred_min_inline
          .map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_inline = candidate
          .preferred_inline
          .map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_min_block = candidate
          .preferred_min_block
          .map(|v| (v - intrinsic_vertical).max(0.0));
        let preferred_block = candidate
          .preferred_block
          .map(|v| (v - intrinsic_vertical).max(0.0));
        let intrinsic_size = Size::new(
          (candidate.fragment.bounds.size.width - actual_horizontal).max(0.0),
          (candidate.fragment.bounds.size.height - actual_vertical).max(0.0),
        );
        let mut input = AbsoluteLayoutInput::new(
          candidate.positioned_style,
          intrinsic_size,
          static_positions
            .get(&candidate.child_id)
            .copied()
            .unwrap_or(Point::ZERO),
        );
        input.is_replaced = candidate.is_replaced;
        input.preferred_min_inline_size = preferred_min_inline;
        input.preferred_inline_size = preferred_inline;
        input.preferred_min_block_size = preferred_min_block;
        input.preferred_block_size = preferred_block;
        let result = abs.layout_absolute(&input, &candidate.cb)?;
        let mut child_fragment = candidate.fragment;
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
          let fc_type = candidate
            .layout_child
            .formatting_context()
            .unwrap_or(crate::style::display::FormattingContextType::Block);
          let fc = abs_factory.get(fc_type);
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
          );
          let relayout_constraints = child_constraints
            .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
          if supports_used_border_box {
            child_fragment = fc.layout(&candidate.layout_child, &relayout_constraints)?;
          } else {
            let mut relayout_child = candidate.layout_child.clone();
            let mut relayout_style = (*relayout_child.style).clone();
            relayout_style.width = Some(crate::style::values::Length::px(border_size.width));
            relayout_style.height = Some(crate::style::values::Length::px(border_size.height));
            relayout_style.width_keyword = None;
            relayout_style.height_keyword = None;
            relayout_child.style = Arc::new(relayout_style);
            child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
          }
        }
        child_fragment.bounds = Rect::new(border_origin, border_size);
        child_fragment.style = Some(candidate.original_style.clone());
        match &mut child_fragment.content {
          FragmentContent::Block { box_id }
          | FragmentContent::Inline { box_id, .. }
          | FragmentContent::Text { box_id, .. }
          | FragmentContent::Replaced { box_id, .. } => *box_id = Some(candidate.child_id),
          FragmentContent::Line { .. } | FragmentContent::RunningAnchor { .. } => {}
        }
        fragment.children_mut().push(child_fragment);
      }
    }

    if !running_children.is_empty() {
      // Running elements are removed from the flex layout tree, but still need anchors positioned
      // as-if they were in-flow. Unlike block layout, flexbox can reorder items (`order`) and even
      // reverse the main axis, so we synthesize anchors based on flex ordering instead of DOM
      // sibling position.
      let axes = FragmentAxes::from_writing_mode_and_direction(style.writing_mode, style.direction);
      let container_block_size = axes.block_size(&fragment.logical_bounds());

      let mut id_to_bounds: FxHashMap<usize, Rect> =
        FxHashMap::with_capacity_and_hasher(fragment.children.len(), Default::default());
      let mut end_of_flow_block_end = 0.0f32;

      let mut deadline_counter = 0usize;
      for child in fragment.children.iter() {
        check_layout_deadline(&mut deadline_counter)?;
        let Some(box_id) = (match &child.content {
          FragmentContent::Block { box_id }
          | FragmentContent::Inline { box_id, .. }
          | FragmentContent::Text { box_id, .. }
          | FragmentContent::Replaced { box_id, .. } => *box_id,
          FragmentContent::Line { .. } | FragmentContent::RunningAnchor { .. } => None,
        }) else {
          continue;
        };
        id_to_bounds.entry(box_id).or_insert(child.bounds);

        if let Some(style) = child.style.as_deref() {
          if style.running_position.is_none()
            && !matches!(style.position, Position::Absolute | Position::Fixed)
          {
            let child_end = axes.block_end(&child.bounds, container_block_size);
            if child_end.is_finite() {
              end_of_flow_block_end = end_of_flow_block_end.max(child_end);
            }
          }
        }
      }

      #[derive(Clone, Copy)]
      struct OrderedFlexChild {
        dom_index: usize,
        order: i32,
        id: usize,
        is_running: bool,
      }

      let mut ordered_children: Vec<OrderedFlexChild> = Vec::new();
      for (dom_index, child) in box_node.children.iter().enumerate() {
        check_layout_deadline(&mut deadline_counter)?;
        if matches!(child.style.position, Position::Absolute | Position::Fixed) {
          continue;
        }
        ordered_children.push(OrderedFlexChild {
          dom_index,
          order: child.style.order,
          id: child.id,
          is_running: child.style.running_position.is_some(),
        });
      }

      // Deterministically order by CSS `order` then DOM index tiebreak.
      ordered_children.sort_by(|a, b| {
        a.order
          .cmp(&b.order)
          .then_with(|| a.dom_index.cmp(&b.dom_index))
      });

      // Map running children by box id so we can process them in flex order without depending on
      // their DOM siblings.
      let mut running_by_id: FxHashMap<usize, BoxNode> =
        FxHashMap::with_capacity_and_hasher(running_children.len(), Default::default());
      for (_, child) in running_children.into_iter() {
        running_by_id.insert(child.id, child);
      }

      // Precompute the next non-running item in flex order for each index.
      let mut next_non_running: Option<usize> = None;
      let mut next_non_running_for_index: Vec<Option<usize>> = vec![None; ordered_children.len()];
      for (idx, entry) in ordered_children.iter().enumerate().rev() {
        if entry.is_running {
          next_non_running_for_index[idx] = next_non_running;
        } else {
          next_non_running = Some(entry.id);
        }
      }

      let snapshot_factory = base_factory.clone();
      let mut running_sequence = 0usize;
      for (idx, entry) in ordered_children.iter().enumerate() {
        if !entry.is_running {
          continue;
        }
        check_layout_deadline(&mut deadline_counter)?;

        let Some(running_child) = running_by_id.get(&entry.id) else {
          continue;
        };
        let Some(name) = running_child.style.running_position.clone() else {
          continue;
        };

        let (anchor_ref_bounds, mut anchor_block_start) = next_non_running_for_index
          .get(idx)
          .and_then(|next| next.as_ref())
          .and_then(|id| id_to_bounds.get(id))
          .map(|bounds| {
            let anchor_block = if matches!(style.flex_direction, FlexDirection::ColumnReverse) {
              axes.block_end(bounds, container_block_size)
            } else {
              axes.block_start(bounds, container_block_size)
            };
            (Some(*bounds), anchor_block)
          })
          .unwrap_or((None, end_of_flow_block_end));

        // Ensure deterministic ordering for multiple running elements at the same anchor position.
        anchor_block_start += (running_sequence as f32) * 1e-4;
        running_sequence += 1;

        let base_anchor_rect = anchor_ref_bounds
          .map(|b| Rect::from_xywh(b.x(), b.y(), 0.0, 0.0))
          .unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 0.0, 0.0));
        let anchor_bounds = axes.set_block_start_and_size(
          base_anchor_rect,
          container_block_size,
          anchor_block_start,
          0.01,
        );

        let mut snapshot_node = running_child.clone();
        let mut snapshot_style = snapshot_node.style.as_ref().clone();
        snapshot_style.running_position = None;
        snapshot_style.position = Position::Static;
        snapshot_node.style = Arc::new(snapshot_style);

        let fc_type = snapshot_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = snapshot_factory.get(fc_type);
        let snapshot_constraints = LayoutConstraints::new(
          CrateAvailableSpace::Definite(fragment.bounds.width()),
          CrateAvailableSpace::Indefinite,
        );
        match fc.layout(&snapshot_node, &snapshot_constraints) {
          Ok(snapshot_fragment) => {
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

    if !disable_cache {
      if let Some((cache_key, key)) = layout_cache_entry {
        let size = fragment.bounds.size;
        self.layout_fragments.insert(
          cache_key,
          key,
          crate::layout::contexts::flex_cache::FlexCacheValue {
            measured_size: size,
            border_size: size,
            fragment: std::sync::Arc::new(fragment.clone()),
          },
          MAX_LAYOUT_CACHE_PER_NODE,
        );
        flex_profile::record_layout_cache_store();
      }
    }

    if !disable_global_layout_cache {
      layout_cache_store(
        box_node,
        FormattingContextType::Flex,
        &constraints,
        &fragment,
        self.factory.viewport_scroll(),
        self.viewport_size,
      );
    }

    Ok(fragment)
  }

  /// Computes intrinsic size by running Taffy with appropriate constraints
  fn compute_intrinsic_inline_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    count_flex_intrinsic_call();
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    if let Some(cached) = intrinsic_cache_lookup(box_node, mode) {
      return Ok(cached);
    }
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
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

    // Approximate intrinsic inline size from flex items per CSS flexbox intrinsic sizing rules:
    // - Row axis: sum of item min/max-content contributions
    // - Column axis: max of item contributions
    let factory = Arc::new(self.child_factory());
    let is_row_axis = matches!(
      style.flex_direction,
      FlexDirection::Row | FlexDirection::RowReverse
    );

    let compute_child_contribution = |child: &BoxNode| -> Result<Option<f32>, LayoutError> {
      if matches!(child.style.position, Position::Absolute | Position::Fixed) {
        return Ok(None);
      }

      let fc_type = child
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let fc = factory.get(fc_type);
      let child_inline = fc.compute_intrinsic_inline_size(child, mode)?;

      // Include horizontal margins when they resolve without a containing block.
      let style = &child.style;
      let margin_left = style
        .margin_left
        .as_ref()
        .map(|l| self.resolve_length_for_width(*l, 0.0, style))
        .unwrap_or(0.0);
      let margin_right = style
        .margin_right
        .as_ref()
        .map(|l| self.resolve_length_for_width(*l, 0.0, style))
        .unwrap_or(0.0);
      let child_total = child_inline + margin_left + margin_right;
      Ok(Some(child_total))
    };

    let mut deadline_counter = 0usize;
    let contributions = if self.parallelism.should_parallelize(box_node.children.len()) {
      let deadline = active_deadline();
      let stage = active_stage();
      let mut child_results = box_node
        .children
        .par_iter()
        .enumerate()
        .map_init(
          || 0usize,
          |deadline_counter, (idx, child)| {
            with_deadline(deadline.as_ref(), || {
              let _stage_guard = StageGuard::install(stage);
              crate::layout::engine::debug_record_parallel_work();
              check_layout_deadline(deadline_counter)?;
              compute_child_contribution(child).map(|value| (idx, value))
            })
          },
        )
        .collect::<Result<Vec<_>, _>>()?;

      // `par_iter().enumerate()` is indexed, but collecting through `Result` does not guarantee
      // stable ordering. Ensure that the intrinsic inline-size summation uses DOM order so float
      // rounding stays deterministic under Rayon parallelism.
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
        child_results.sort_unstable_by_key(|(idx, _)| *idx);
      }

      child_results
        .into_iter()
        .enumerate()
        .map(|(expected_idx, (idx, value))| {
          debug_assert_eq!(idx, expected_idx, "parallel flex intrinsic index mismatch");
          value
        })
        .collect()
    } else {
      box_node
        .children
        .iter()
        .map(|child| {
          check_layout_deadline(&mut deadline_counter)?;
          compute_child_contribution(child)
        })
        .collect::<Result<Vec<_>, _>>()?
    };

    let mut contribution = 0.0f32;
    for child_total in contributions.into_iter().flatten() {
      check_layout_deadline(&mut deadline_counter)?;
      if is_row_axis {
        contribution += child_total;
      } else {
        contribution = contribution.max(child_total);
      }
    }

    let edges = self.horizontal_edges_px(style).unwrap_or(0.0);
    let width = (contribution + edges).max(0.0);
    intrinsic_cache_store(box_node, mode, width);
    Ok(width)
  }

  fn compute_intrinsic_block_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    count_flex_intrinsic_call();
    if let Some(cached) = intrinsic_block_cache_lookup(box_node, mode) {
      return Ok(cached);
    }

    let fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    if fc_type != FormattingContextType::Flex {
      return self
        .child_factory()
        .get(fc_type)
        .compute_intrinsic_block_size(box_node, mode);
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let writing_mode = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref())
      .writing_mode;
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(writing_mode);
    let intrinsic_inline_space = match mode {
      IntrinsicSizingMode::MinContent => CrateAvailableSpace::MinContent,
      IntrinsicSizingMode::MaxContent => CrateAvailableSpace::MaxContent,
    };
    let constraints = if inline_is_horizontal {
      LayoutConstraints::new(intrinsic_inline_space, CrateAvailableSpace::Indefinite)
    } else {
      LayoutConstraints::new(CrateAvailableSpace::Indefinite, intrinsic_inline_space)
    };
    let fragment = self.layout(box_node, &constraints)?;
    let block_size = if inline_is_horizontal {
      fragment.bounds.height()
    } else {
      fragment.bounds.width()
    };
    intrinsic_block_cache_store(box_node, mode, block_size);
    Ok(block_size)
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
  let flex_basis_depends =
    matches!(style.flex_basis, FlexBasis::Length(len) if len.has_percentage());

  height_depends || min_depends || max_depends || flex_basis_depends
}

#[inline]
fn physical_width_is_auto(style: &ComputedStyle) -> bool {
  style.width.is_none() && style.width_keyword.is_none()
}

#[inline]
fn physical_height_is_auto(style: &ComputedStyle) -> bool {
  style.height.is_none() && style.height_keyword.is_none()
}

#[inline]
fn physical_min_width_is_auto(style: &ComputedStyle) -> bool {
  style.min_width.is_none() && style.min_width_keyword.is_none()
}

#[inline]
fn physical_max_width_is_auto(style: &ComputedStyle) -> bool {
  style.max_width.is_none() && style.max_width_keyword.is_none()
}

fn measure_cache_key_and_snap(
  known: &taffy::geometry::Size<Option<f32>>,
  avail: &taffy::geometry::Size<AvailableSpace>,
  viewport: Size,
  drop_available_height: bool,
) -> (
  FlexCacheKey,
  taffy::geometry::Size<Option<f32>>,
  taffy::geometry::Size<AvailableSpace>,
) {
  fn quantize(val: f32) -> f32 {
    // Quantize measure keys to merge near-duplicate probes without visibly affecting layout.
    // Use progressively coarser steps as sizes grow to curb key cardinality on large pages
    // while keeping typical sizes precise. Thresholds favor tighter precision for small
    // items while aggressively merging large, near-identical probes common on wide carousels.
    let abs = val.abs();
    let step = if abs > 32768.0 {
      512.0
    } else if abs > 16384.0 {
      256.0
    } else if abs > 8192.0 {
      128.0
    } else if abs > 4096.0 {
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

  fn clamp_width_for_constraints(w: f32, viewport: Size) -> f32 {
    // Match `constraints_from_taffy`, which clamps all definite inline sizes to the viewport.
    // This means any `AvailableSpace::Definite(w)` above the viewport will produce the same
    // downstream layout result, so we can safely coalesce cache keys for those probes.
    w.min(viewport.width.max(0.0))
  }

  fn normalize_available_width(space: AvailableSpace, viewport: Size) -> AvailableSpace {
    match space {
      AvailableSpace::Definite(w) if w <= 1.0 => AvailableSpace::MaxContent,
      AvailableSpace::Definite(w) => {
        AvailableSpace::Definite(quantize(clamp_width_for_constraints(w, viewport)))
      }
      other => other,
    }
  }

  fn normalize_available_height(space: AvailableSpace) -> AvailableSpace {
    match space {
      AvailableSpace::Definite(h) if h <= 1.0 => AvailableSpace::MaxContent,
      AvailableSpace::Definite(h) => AvailableSpace::Definite(quantize(h)),
      other => other,
    }
  }

  let mut snapped_known = known.clone();
  let mut snapped_avail = taffy::geometry::Size {
    width: normalize_available_width(avail.width, viewport),
    height: normalize_available_height(avail.height),
  };
  if let Some(w) = snapped_known.width {
    if w > 1.0 {
      snapped_known.width = Some(clamp_width_for_constraints(w, viewport));
    }
  }
  if let Some(w) = snapped_known.width {
    if w <= 1.0 && matches!(snapped_avail.width, AvailableSpace::MaxContent) {
      snapped_known.width = None;
    }
  }
  if let Some(h) = snapped_known.height {
    if h <= 1.0 && matches!(snapped_avail.height, AvailableSpace::MaxContent) {
      snapped_known.height = None;
    }
  }

  let width_is_intrinsic = snapped_known.width.is_none()
    && matches!(
      snapped_avail.width,
      AvailableSpace::MinContent | AvailableSpace::MaxContent
    );

  let width = if let Some(w) = snapped_known.width {
    quantize(w)
  } else {
    match snapped_avail.width {
      AvailableSpace::Definite(w) => quantize(w),
      AvailableSpace::MinContent => -viewport.width.max(0.0) - 1.0,
      AvailableSpace::MaxContent => -viewport.width.max(0.0) - 2.0,
    }
  };

  if snapped_known.width.is_some() {
    snapped_known.width = Some(width);
  }
  if matches!(snapped_avail.width, AvailableSpace::Definite(_)) {
    snapped_avail.width = AvailableSpace::Definite(width);
  }

  let ignore_height = drop_available_height || width_is_intrinsic;
  let height = if let Some(h) = snapped_known.height {
    Some(quantize(h))
  } else if ignore_height {
    // Ignore available block-size differences when the measurement does not depend on the
    // containing block height (or when probing intrinsic inline sizes).
    None
  } else {
    Some(match snapped_avail.height {
      AvailableSpace::Definite(h) => quantize(h),
      AvailableSpace::MinContent => -viewport.height.max(0.0) - 1.0,
      AvailableSpace::MaxContent => -viewport.height.max(0.0) - 2.0,
    })
  };

  match height {
    Some(h) => {
      if snapped_known.height.is_some() {
        snapped_known.height = Some(h);
      }
      if matches!(snapped_avail.height, AvailableSpace::Definite(_)) {
        snapped_avail.height = AvailableSpace::Definite(h);
      }
    }
    None => {
      snapped_known.height = None;
      snapped_avail.height = AvailableSpace::MaxContent;
    }
  }

  (
    (
      Some(f32_to_canonical_bits(width)),
      height.map(f32_to_canonical_bits),
    ),
    snapped_known,
    snapped_avail,
  )
}

fn measure_cache_key(
  known: &taffy::geometry::Size<Option<f32>>,
  avail: &taffy::geometry::Size<AvailableSpace>,
  viewport: Size,
  drop_available_height: bool,
) -> FlexCacheKey {
  measure_cache_key_and_snap(known, avail, viewport, drop_available_height).0
}

fn hash_enum_discriminant<T>(value: &T, hasher: &mut FingerprintHasher) {
  mem::discriminant(value).hash(hasher);
}

fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

fn hash_calc_length(calc: &CalcLength, hasher: &mut FingerprintHasher) {
  let terms = calc.terms();
  (terms.len() as u8).hash(hasher);
  for term in terms {
    hash_enum_discriminant(&term.unit, hasher);
    f32_to_canonical_bits(term.value).hash(hasher);
  }
}

fn hash_length(len: &Length, hasher: &mut FingerprintHasher) {
  hash_enum_discriminant(&len.unit, hasher);
  f32_to_canonical_bits(len.value).hash(hasher);
  match &len.calc {
    Some(calc) => {
      1u8.hash(hasher);
      hash_calc_length(calc, hasher);
    }
    None => 0u8.hash(hasher),
  }
}

fn hash_option_length(len: &Option<Length>, hasher: &mut FingerprintHasher) {
  match len {
    Some(l) => {
      1u8.hash(hasher);
      hash_length(l, hasher);
    }
    None => 0u8.hash(hasher),
  }
}

fn hash_intrinsic_size_keyword(value: &IntrinsicSizeKeyword, hasher: &mut FingerprintHasher) {
  match value {
    IntrinsicSizeKeyword::MinContent => 0u8.hash(hasher),
    IntrinsicSizeKeyword::MaxContent => 1u8.hash(hasher),
    IntrinsicSizeKeyword::FillAvailable => 2u8.hash(hasher),
    IntrinsicSizeKeyword::FitContent { limit } => {
      3u8.hash(hasher);
      match limit {
        Some(limit) => {
          1u8.hash(hasher);
          hash_length(limit, hasher);
        }
        None => 0u8.hash(hasher),
      }
    }
  }
}

fn hash_option_intrinsic_size_keyword(
  value: &Option<IntrinsicSizeKeyword>,
  hasher: &mut FingerprintHasher,
) {
  match value {
    Some(value) => {
      1u8.hash(hasher);
      hash_intrinsic_size_keyword(value, hasher);
    }
    None => 0u8.hash(hasher),
  }
}

fn hash_sizing_property(
  length: &Option<Length>,
  keyword: &Option<IntrinsicSizeKeyword>,
  hasher: &mut FingerprintHasher,
) {
  hash_option_length(length, hasher);
  hash_option_intrinsic_size_keyword(keyword, hasher);
}

fn hash_flex_basis(basis: &crate::style::types::FlexBasis, hasher: &mut FingerprintHasher) {
  match basis {
    crate::style::types::FlexBasis::Auto => 0u8.hash(hasher),
    crate::style::types::FlexBasis::Content => 2u8.hash(hasher),
    crate::style::types::FlexBasis::Length(l) => {
      1u8.hash(hasher);
      hash_length(l, hasher);
    }
  }
}

fn flex_style_fingerprint(style: &ComputedStyle) -> u64 {
  let mut h = FingerprintHasher::default();
  hash_enum_discriminant(&style.display, &mut h);
  hash_enum_discriminant(&style.position, &mut h);
  hash_enum_discriminant(&style.box_sizing, &mut h);
  hash_enum_discriminant(&style.content_visibility, &mut h);
  style.contain_intrinsic_width.auto.hash(&mut h);
  hash_option_length(&style.contain_intrinsic_width.length, &mut h);
  style.contain_intrinsic_height.auto.hash(&mut h);
  hash_option_length(&style.contain_intrinsic_height.length, &mut h);
  hash_sizing_property(&style.width, &style.width_keyword, &mut h);
  hash_sizing_property(&style.height, &style.height_keyword, &mut h);
  hash_sizing_property(&style.min_width, &style.min_width_keyword, &mut h);
  hash_sizing_property(&style.max_width, &style.max_width_keyword, &mut h);
  hash_sizing_property(&style.min_height, &style.min_height_keyword, &mut h);
  hash_sizing_property(&style.max_height, &style.max_height_keyword, &mut h);
  hash_option_length(&style.margin_top, &mut h);
  hash_option_length(&style.margin_right, &mut h);
  hash_option_length(&style.margin_bottom, &mut h);
  hash_option_length(&style.margin_left, &mut h);
  hash_length(&style.padding_top, &mut h);
  hash_length(&style.padding_right, &mut h);
  hash_length(&style.padding_bottom, &mut h);
  hash_length(&style.padding_left, &mut h);
  hash_length(&style.used_border_top_width(), &mut h);
  hash_length(&style.used_border_right_width(), &mut h);
  hash_length(&style.used_border_bottom_width(), &mut h);
  hash_length(&style.used_border_left_width(), &mut h);
  hash_enum_discriminant(&style.overflow_x, &mut h);
  hash_enum_discriminant(&style.overflow_y, &mut h);
  hash_enum_discriminant(&style.scrollbar_width, &mut h);
  hash_enum_discriminant(&style.flex_direction, &mut h);
  hash_enum_discriminant(&style.flex_wrap, &mut h);
  hash_enum_discriminant(&style.justify_content, &mut h);
  hash_enum_discriminant(&style.align_items, &mut h);
  hash_enum_discriminant(&style.align_content, &mut h);
  match style.align_self {
    Some(v) => {
      1u8.hash(&mut h);
      hash_enum_discriminant(&v, &mut h);
    }
    None => 0u8.hash(&mut h),
  }
  hash_enum_discriminant(&style.justify_items, &mut h);
  match style.justify_self {
    Some(v) => {
      1u8.hash(&mut h);
      hash_enum_discriminant(&v, &mut h);
    }
    None => 0u8.hash(&mut h),
  }
  f32_to_canonical_bits(style.flex_grow).hash(&mut h);
  f32_to_canonical_bits(style.flex_shrink).hash(&mut h);
  hash_flex_basis(&style.flex_basis, &mut h);
  hash_enum_discriminant(&style.aspect_ratio, &mut h);
  // Intrinsic/text sizing influences: include font size/line height basics.
  f32_to_canonical_bits(style.font_size).hash(&mut h);
  f32_to_canonical_bits(style.root_font_size).hash(&mut h);
  hash_enum_discriminant(&style.line_height, &mut h);
  h.finish()
}

fn flex_cache_key_with_style(box_node: &BoxNode, style: &ComputedStyle) -> u64 {
  let mut h = FingerprintHasher::default();
  box_node.styled_node_id.hash(&mut h);
  // Include semantic pseudo-element identity so `::before`/`::after` boxes don't collide with the
  // originating element in cache keys. `debug_info` is optional (often `None` in release builds)
  // and must never influence layout/caching decisions.
  box_node.generated_pseudo.hash(&mut h);
  // Anonymous boxes (no originating styled node) must not share cached fragments across
  // different instances: their descendants may differ (e.g. different `<img src>`), and
  // reusing would clone the wrong subtree.
  if box_node.styled_node_id.is_none() {
    box_node.id.hash(&mut h);
  }
  let fingerprint = flex_style_fingerprint(style);
  fingerprint.hash(&mut h);
  h.finish()
}

fn sanitize_viewport_scroll(scroll: Point) -> Point {
  if scroll.x.is_finite() && scroll.y.is_finite() {
    scroll
  } else {
    Point::ZERO
  }
}

fn flex_cache_key_with_style_and_scroll(
  box_node: &BoxNode,
  style: &ComputedStyle,
  viewport_scroll: Point,
) -> u64 {
  let base = flex_cache_key_with_style(box_node, style);
  let viewport_scroll = sanitize_viewport_scroll(viewport_scroll);
  if viewport_scroll == Point::ZERO {
    return base;
  }

  let mut h = FingerprintHasher::default();
  base.hash(&mut h);
  f32_to_canonical_bits(viewport_scroll.x).hash(&mut h);
  f32_to_canonical_bits(viewport_scroll.y).hash(&mut h);
  h.finish()
}

fn flex_cache_key_with_scroll(box_node: &BoxNode, viewport_scroll: Point) -> u64 {
  flex_cache_key_with_style_and_scroll(box_node, box_node.style.as_ref(), viewport_scroll)
}

fn flex_cache_key(box_node: &BoxNode) -> u64 {
  flex_cache_key_with_style(box_node, box_node.style.as_ref())
}

fn layout_cache_key(
  constraints: &LayoutConstraints,
  viewport: Size,
) -> Option<(Option<u32>, Option<u32>)> {
  let map_space = |space: CrateAvailableSpace, vp: f32, neg_offset: f32| -> Option<u32> {
    match space {
      // Do *not* quantize definite sizes here.
      //
      // The flex layout cache stores a fragment computed with the original constraints. If we
      // quantize sizes in the cache key, multiple distinct constraint values may collide to the
      // same key. Then whichever layout runs first determines the stored fragment, and later
      // layouts can reuse an incompatible fragment, making output order-dependent (and therefore
      // non-deterministic under parallel traversal).
      CrateAvailableSpace::Definite(v) => Some(f32_to_canonical_bits(v)),
      CrateAvailableSpace::MinContent => Some(f32_to_canonical_bits(-vp - neg_offset)),
      CrateAvailableSpace::MaxContent => Some(f32_to_canonical_bits(-vp - (neg_offset + 1.0))),
      CrateAvailableSpace::Indefinite => None,
    }
  };

  // The flex container's layout is primarily determined by the available space, but when a parent
  // layout mode provides an explicit used border-box size (e.g., sizing a nested inline-flex as a
  // flex/grid item), that override is the effective input for the container's outer size.
  let width_space = constraints
    .used_border_box_width
    .map(CrateAvailableSpace::Definite)
    .unwrap_or(constraints.available_width);
  let height_space = constraints
    .used_border_box_height
    .map(CrateAvailableSpace::Definite)
    .unwrap_or(constraints.available_height);

  let w = map_space(width_space, viewport.width.max(0.0), 1.0);
  let h = map_space(height_space, viewport.height.max(0.0), 1.0);
  Some((w, h))
}

fn flex_child_fingerprint(
  children: &[&BoxNode],
  deadline_counter: &mut usize,
) -> Result<u64, LayoutError> {
  let mut h = FingerprintHasher::default();
  children.len().hash(&mut h);
  for child in children {
    check_layout_deadline(deadline_counter)?;
    // The cached Taffy template only memoizes the style-to-Taffy conversion, which depends on the
    // computed style (and the parent flex container style) but not on box type or formatting
    // context. Including those would unnecessarily fragment the template cache and hurt reuse on
    // component-heavy pages.
    taffy_flex_style_fingerprint(child.style.as_ref()).hash(&mut h);
  }
  Ok(h.finish())
}

impl FlexFormattingContext {
  /// Builds a Taffy tree from a BoxNode tree
  ///
  /// Returns the root NodeId and populates the node_map for later lookups.
  #[allow(dead_code)]
  fn build_taffy_tree(
    &self,
    taffy_tree: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
    node_map: &mut FxHashMap<*const BoxNode, NodeId>,
  ) -> Result<NodeId, LayoutError> {
    let children: Vec<&BoxNode> = box_node.children.iter().collect();
    self.build_taffy_tree_children(
      taffy_tree,
      box_node,
      box_node.style.as_ref(),
      &children,
      constraints,
      node_map,
    )
  }

  /// Builds a Taffy tree from a BoxNode tree using an explicit set of root children
  /// (used to exclude out-of-flow children).
  fn build_taffy_tree_children(
    &self,
    taffy_tree: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    root_style: &ComputedStyle,
    root_children: &[&BoxNode],
    constraints: &LayoutConstraints,
    node_map: &mut FxHashMap<*const BoxNode, NodeId>,
  ) -> Result<NodeId, LayoutError> {
    let mut deadline_counter = 0usize;
    let child_fingerprint = flex_child_fingerprint(root_children, &mut deadline_counter)?;
    let root_style_fingerprint = taffy_flex_style_fingerprint(root_style);
    let cache_key = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Flex,
      root_style_fingerprint,
      child_fingerprint,
      self.viewport_size,
    );
    let cached = self.taffy_cache.get(&cache_key);
    let template: std::sync::Arc<CachedTaffyTemplate> = if let Some(template) = cached {
      record_taffy_node_cache_hit(TaffyAdapterKind::Flex, template.node_count());
      record_taffy_style_cache_hit(TaffyAdapterKind::Flex, template.node_count());
      template
    } else {
      let mut child_styles = Vec::with_capacity(root_children.len());
      for child in root_children {
        check_layout_deadline(&mut deadline_counter)?;
        child_styles.push(std::sync::Arc::new(SendSyncStyle(
          self.computed_style_to_taffy_base(child.style.as_ref(), false, Some(root_style))?,
        )));
      }
      let root_style = std::sync::Arc::new(SendSyncStyle(
        self.computed_style_to_taffy_base(root_style, true, None)?,
      ));
      let template = std::sync::Arc::new(CachedTaffyTemplate {
        root_style,
        child_styles,
      });
      self.taffy_cache.insert(cache_key, template.clone());
      record_taffy_node_cache_miss(TaffyAdapterKind::Flex, template.node_count());
      record_taffy_style_cache_miss(TaffyAdapterKind::Flex, template.node_count());
      template
    };

    let auto_unskipped_empty: FxHashSet<*const BoxNode> = FxHashSet::default();
    let mut taffy_children = Vec::with_capacity(root_children.len());
    for (child_style, child) in template.child_styles.iter().zip(root_children.iter()) {
      check_layout_deadline(&mut deadline_counter)?;
      let child = *child;
      let mut resolved_style = child_style.0.clone();
      self.apply_flex_intrinsic_size_keywords(
        child,
        false,
        Some(root_style),
        Some(constraints),
        &mut resolved_style,
      )?;
      let skip_contents = self.flex_item_should_skip_contents(child, &auto_unskipped_empty);
      self.apply_flex_auto_min_size(
        child,
        false,
        Some(root_style),
        skip_contents,
        &mut resolved_style,
      )?;
      self.apply_flex_fit_content_keywords(
        child,
        false,
        Some(root_style),
        constraints,
        &mut resolved_style,
      )?;
      let node = taffy_tree
        .new_leaf_with_context(resolved_style, child as *const BoxNode)
        .map_err(|e| {
          LayoutError::MissingContext(format!("Failed to create Taffy leaf: {:?}", e))
        })?;
      node_map.insert(child as *const BoxNode, node);
      taffy_children.push(node);
    }

    let taffy_node = if taffy_children.is_empty() {
      taffy_tree
        .new_leaf(template.root_style.0.clone())
        .map_err(|e| LayoutError::MissingContext(format!("Failed to create Taffy leaf: {:?}", e)))?
    } else {
      taffy_tree
        .new_with_children(template.root_style.0.clone(), &taffy_children)
        .map_err(|e| LayoutError::MissingContext(format!("Failed to create Taffy node: {:?}", e)))?
    };

    node_map.insert(box_node as *const BoxNode, taffy_node);

    Ok(taffy_node)
  }

  /// Internal tree builder that tracks whether we're at the root
  #[allow(dead_code)]
  fn build_taffy_tree_inner(
    &self,
    taffy_tree: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    node_map: &mut FxHashMap<*const BoxNode, NodeId>,
    is_root: bool,
    containing_flex: Option<&ComputedStyle>,
    root_children: Option<&[&BoxNode]>,
  ) -> Result<NodeId, LayoutError> {
    let auto_unskipped_empty: FxHashSet<*const BoxNode> = FxHashSet::default();
    // Convert style to Taffy style
    let taffy_style =
      self.computed_style_to_taffy(box_node, is_root, containing_flex, &auto_unskipped_empty)?;

    // Create Taffy node
    let children_iter: Vec<&BoxNode> = if is_root {
      root_children
        .map(|c| c.to_vec())
        .unwrap_or_else(|| box_node.children.iter().collect())
    } else {
      Vec::new()
    };

    let taffy_node = if is_root {
      let mut taffy_children = Vec::with_capacity(children_iter.len());
      for child in children_iter {
        let next_containing_flex =
          if is_root || matches!(box_node.style.display, Display::Flex | Display::InlineFlex) {
            Some(&box_node.style)
          } else {
            None
          };
        let child_node = self.build_taffy_tree_inner(
          taffy_tree,
          child,
          node_map,
          false,
          next_containing_flex.map(|s| &**s),
          None,
        )?;
        taffy_children.push(child_node);
      }

      if taffy_children.is_empty() {
        taffy_tree.new_leaf(taffy_style).map_err(|e| {
          LayoutError::MissingContext(format!("Failed to create Taffy leaf: {:?}", e))
        })?
      } else {
        taffy_tree
          .new_with_children(taffy_style, &taffy_children)
          .map_err(|e| {
            LayoutError::MissingContext(format!("Failed to create Taffy node: {:?}", e))
          })?
      }
    } else {
      taffy_tree
        .new_leaf_with_context(taffy_style, box_node as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Failed to create Taffy leaf: {:?}", e)))?
    };

    // Record mapping for later fragment conversion
    node_map.insert(box_node as *const BoxNode, taffy_node);

    Ok(taffy_node)
  }

  /// Converts our ComputedStyle to Taffy's Style
  ///
  /// The `is_root` flag indicates if this is the root flex container.
  /// For the root, we use Flex display; for children, we use Block.
  fn computed_style_to_taffy(
    &self,
    box_node: &BoxNode,
    is_root: bool,
    containing_flex: Option<&ComputedStyle>,
    auto_unskipped_for_pass: &FxHashSet<*const BoxNode>,
  ) -> Result<taffy::style::Style, LayoutError> {
    let mut style =
      self.computed_style_to_taffy_base(box_node.style.as_ref(), is_root, containing_flex)?;
    self.apply_flex_intrinsic_size_keywords(
      box_node,
      is_root,
      containing_flex,
      None,
      &mut style,
    )?;
    let skip_contents = self.flex_item_should_skip_contents(box_node, auto_unskipped_for_pass);
    self.apply_flex_auto_min_size(box_node, is_root, containing_flex, skip_contents, &mut style)?;
    Ok(style)
  }

  /// Converts our `ComputedStyle` to Taffy's `Style` without any content-dependent sizing work.
  ///
  /// In particular, this does **not** apply Flexbox's content-based automatic minimum size
  /// (`min-width/height: auto` on flex items). That content-derived step must run per box instance
  /// (e.g. when instantiating cached Taffy templates) so template caching remains correct.
  fn computed_style_to_taffy_base(
    &self,
    style: &ComputedStyle,
    is_root: bool,
    containing_flex: Option<&ComputedStyle>,
  ) -> Result<taffy::style::Style, LayoutError> {
    let inline_positive_container = self.inline_axis_positive(style);
    let block_positive_container = self.block_axis_positive(style);
    let inline_is_horizontal_container = matches!(style.writing_mode, WritingMode::HorizontalTb);
    let container_main_is_inline = matches!(
      style.flex_direction,
      FlexDirection::Row | FlexDirection::RowReverse
    );
    let cross_positive_container = if container_main_is_inline {
      block_positive_container
    } else {
      inline_positive_container
    };

    // Flex items align to the parent flex container's axes, not their own writing-mode/direction.
    let axis_source = containing_flex.unwrap_or(style);
    let inline_positive_item = self.inline_axis_positive(axis_source);
    let block_positive_item = self.block_axis_positive(axis_source);
    let axis_main_is_inline = matches!(
      axis_source.flex_direction,
      FlexDirection::Row | FlexDirection::RowReverse
    );
    let cross_positive_item = if axis_main_is_inline {
      block_positive_item
    } else {
      inline_positive_item
    };

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

    let min_width_dimension =
      self.length_option_to_dimension_box_sizing(style.min_width.as_ref(), style, Axis::Horizontal);
    let min_height_dimension =
      self.length_option_to_dimension_box_sizing(style.min_height.as_ref(), style, Axis::Vertical);

    let mut taffy_style = taffy::style::Style {
      // Display mode - only root is Flex, children are Block (flex items)
      display: self.display_to_taffy(style, is_root),

      // Flex container properties
      flex_direction: self.flex_direction_to_taffy(
        style,
        inline_positive_container,
        block_positive_container,
      ),
      flex_wrap: self.flex_wrap_to_taffy(style.flex_wrap),
      justify_content: self.justify_content_to_taffy(style.justify_content),
      align_items: self.align_items_to_taffy(style.align_items, cross_positive_container),
      align_content: self.align_content_to_taffy(style.align_content, cross_positive_container),
      align_self: self.align_self_to_taffy(style.align_self, cross_positive_item),
      justify_self: self.align_self_to_taffy(style.justify_self, inline_positive_item),
      justify_items: self.align_items_to_taffy(style.justify_items, inline_positive_container),

      // Gap
      gap: taffy::geometry::Size {
        // Column gap follows the inline axis; row gap follows the block axis.
        width: if inline_is_horizontal_container {
          self.length_to_taffy_lp(&style.grid_column_gap, style)
        } else {
          self.length_to_taffy_lp(&style.grid_row_gap, style)
        },
        height: if inline_is_horizontal_container {
          self.length_to_taffy_lp(&style.grid_row_gap, style)
        } else {
          self.length_to_taffy_lp(&style.grid_column_gap, style)
        },
      },

      // Flex item properties
      flex_grow: style.flex_grow,
      flex_shrink: style.flex_shrink,
      flex_basis: self.flex_basis_to_taffy(&style.flex_basis, style),

      // Sizing - for root flex container without explicit size, use 100%
      // to fill the available space (block-level behavior)
      size: self.compute_size(style, is_root),
      min_size: taffy::geometry::Size {
        width: min_width_dimension,
        height: min_height_dimension,
      },
      max_size: taffy::geometry::Size {
        width: self.length_option_to_dimension_box_sizing(
          style.max_width.as_ref(),
          style,
          Axis::Horizontal,
        ),
        height: self.length_option_to_dimension_box_sizing(
          style.max_height.as_ref(),
          style,
          Axis::Vertical,
        ),
      },

      // Spacing
      padding: taffy::geometry::Rect {
        left: self.length_to_taffy_lp(&style.padding_left, style),
        right: self.length_to_taffy_lp(&style.padding_right, style),
        top: self.length_to_taffy_lp(&style.padding_top, style),
        bottom: self.length_to_taffy_lp(&style.padding_bottom, style),
      },
      margin: taffy::geometry::Rect {
        left: self.length_option_to_lpa(style.margin_left.as_ref(), style),
        right: self.length_option_to_lpa(style.margin_right.as_ref(), style),
        top: self.length_option_to_lpa(style.margin_top.as_ref(), style),
        bottom: self.length_option_to_lpa(style.margin_bottom.as_ref(), style),
      },
      border: taffy::geometry::Rect {
        left: self.length_to_taffy_lp(&style.used_border_left_width(), style),
        right: self.length_to_taffy_lp(&style.used_border_right_width(), style),
        top: self.length_to_taffy_lp(&style.used_border_top_width(), style),
        bottom: self.length_to_taffy_lp(&style.used_border_bottom_width(), style),
      },
      aspect_ratio: self.aspect_ratio_to_taffy(style.aspect_ratio),

      overflow: taffy::geometry::Point {
        x: map_overflow(style.overflow_x, reserve_scroll_x),
        y: map_overflow(style.overflow_y, reserve_scroll_y),
      },
      scrollbar_width: resolve_scrollbar_width(style),

      ..Default::default()
    };

    if !is_root && matches!(style.flex_basis, FlexBasis::Content) {
      // CSS `flex-basis: content` explicitly requests content-based flex base sizing even when a
      // preferred main-size (`width`/`height`) is specified. Taffy models this behaviour by using
      // `flex_basis: auto` *and* a non-definite main size, causing the base size calculation to
      // enter its measurement step.
      taffy_style.flex_basis = Dimension::auto();

      let parent_style = containing_flex.unwrap_or(style);
      let parent_inline_positive = self.inline_axis_positive(parent_style);
      let parent_block_positive = self.block_axis_positive(parent_style);
      let parent_direction = self.flex_direction_to_taffy(
        parent_style,
        parent_inline_positive,
        parent_block_positive,
      );
      let main_axis_is_horizontal = matches!(
        parent_direction,
        taffy::style::FlexDirection::Row | taffy::style::FlexDirection::RowReverse
      );
      if main_axis_is_horizontal {
        taffy_style.size.width = Dimension::auto();
      } else {
        taffy_style.size.height = Dimension::auto();
      }
    }

    Ok(taffy_style)
  }

  /// Resolves intrinsic sizing keywords (`min-content` / `max-content`) for a specific flex item.
  ///
  /// These keyword values depend on the box's contents, so the resolution must happen per box
  /// instance (both in the non-template path and when instantiating cached Taffy templates).
  fn apply_flex_intrinsic_size_keywords(
    &self,
    box_node: &BoxNode,
    is_root: bool,
    containing_flex: Option<&ComputedStyle>,
    constraints: Option<&LayoutConstraints>,
    taffy_style: &mut taffy::style::Style,
  ) -> Result<(), LayoutError> {
    if is_root {
      return Ok(());
    }

    let style = box_node.style.as_ref();
    let item_fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let item_fc = self.factory.get(item_fc_type);
    let box_id = box_node.id();
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);

    // Intrinsic size computations treat percentage padding/borders as 0px because the containing
    // block width is not yet known. When applying an intrinsic keyword to a flex item in a
    // definite-width flex container, rebase those percentage edges against the actual flex
    // container content box width (CSS Sizing L3 §4.5 / §4.7).
    //
    // This mirrors the rebasing behavior in the block formatting context and ensures
    // `width/max-width/min-width: max-content|min-content` include percentage padding/borders.
    let mut edges_base0_w: Option<f32> = None;
    let mut edges_base0_h: Option<f32> = None;
    let mut edges_actual_w: Option<f32> = None;
    let mut edges_actual_h: Option<f32> = None;
    if let (Some(container_style), Some(constraints)) = (containing_flex, constraints) {
      let content_base = match self.fit_content_available_for_axis(
        Axis::Horizontal,
        Some(container_style),
        constraints,
      ) {
        FitContentAvailable::Definite(v) if v.is_finite() => Some(v.max(0.0)),
        _ => None,
      };
      if let Some(content_base) = content_base {
        let axis_edges = |axis: Axis, percentage_base: f32| -> f32 {
          let padding_left =
            self.resolve_length_for_width(style.padding_left, percentage_base, style);
          let padding_right =
            self.resolve_length_for_width(style.padding_right, percentage_base, style);
          let padding_top =
            self.resolve_length_for_width(style.padding_top, percentage_base, style);
          let padding_bottom =
            self.resolve_length_for_width(style.padding_bottom, percentage_base, style);
          let border_left =
            self.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
          let border_right =
            self.resolve_length_for_width(style.used_border_right_width(), percentage_base, style);
          let border_top =
            self.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
          let border_bottom =
            self.resolve_length_for_width(style.used_border_bottom_width(), percentage_base, style);
          match axis {
            Axis::Horizontal => padding_left + padding_right + border_left + border_right,
            Axis::Vertical => padding_top + padding_bottom + border_top + border_bottom,
          }
        };

        edges_base0_w = Some(axis_edges(Axis::Horizontal, 0.0));
        edges_base0_h = Some(axis_edges(Axis::Vertical, 0.0));
        edges_actual_w = Some(axis_edges(Axis::Horizontal, content_base));
        edges_actual_h = Some(axis_edges(Axis::Vertical, content_base));
      }
    }
    let rebase_intrinsic_border_box = |border_box: f32, axis: Axis| -> f32 {
      let (edges_base0, edges_actual) = match axis {
        Axis::Horizontal => (edges_base0_w, edges_actual_w),
        Axis::Vertical => (edges_base0_h, edges_actual_h),
      };
      if let (Some(edges_base0), Some(edges_actual)) = (edges_base0, edges_actual) {
        (border_box - edges_base0 + edges_actual).max(0.0)
      } else {
        border_box.max(0.0)
      }
    };

    // When computing intrinsic sizes for an axis that is itself specified as an intrinsic keyword,
    // clear that size property to avoid self-recursion.
    let width_override = style.width_keyword.is_some().then(|| {
      let mut override_style: ComputedStyle = (*box_node.style).clone();
      override_style.width = None;
      override_style.width_keyword = None;
      Arc::new(override_style)
    });
    let height_override = style.height_keyword.is_some().then(|| {
      let mut override_style: ComputedStyle = (*box_node.style).clone();
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
      if inline_is_horizontal {
        run_with_override(width_override.clone(), &|node| {
          item_fc.compute_intrinsic_inline_size(node, mode)
        })
      } else {
        run_with_override(width_override.clone(), &|node| {
          item_fc.compute_intrinsic_block_size(node, mode)
        })
      }
    };

    let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      if inline_is_horizontal {
        run_with_override(height_override.clone(), &|node| {
          item_fc.compute_intrinsic_block_size(node, mode)
        })
      } else {
        run_with_override(height_override.clone(), &|node| {
          item_fc.compute_intrinsic_inline_size(node, mode)
        })
      }
    };

    let keyword_to_mode = |kw: IntrinsicSizeKeyword| match kw {
      IntrinsicSizeKeyword::MinContent => Some(IntrinsicSizingMode::MinContent),
      IntrinsicSizeKeyword::MaxContent => Some(IntrinsicSizingMode::MaxContent),
      IntrinsicSizeKeyword::FillAvailable => None,
      IntrinsicSizeKeyword::FitContent { .. } => None,
    };

    if let Some(mode) = style.width_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_width(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            let rebased = rebase_intrinsic_border_box(border_box, Axis::Horizontal);
            taffy_style.size.width = Dimension::length(rebased);
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    if let Some(mode) = style.height_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_height(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            let rebased = rebase_intrinsic_border_box(border_box, Axis::Vertical);
            taffy_style.size.height = Dimension::length(rebased);
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    if let Some(mode) = style.min_width_keyword.and_then(keyword_to_mode) {
      match intrinsic_physical_width(mode) {
        Ok(border_box) => {
          if border_box.is_finite() {
            let rebased = rebase_intrinsic_border_box(border_box, Axis::Horizontal);
            taffy_style.min_size.width = Dimension::length(rebased);
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
            let rebased = rebase_intrinsic_border_box(border_box, Axis::Vertical);
            taffy_style.min_size.height = Dimension::length(rebased);
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
            let rebased = rebase_intrinsic_border_box(border_box, Axis::Horizontal);
            taffy_style.max_size.width = Dimension::length(rebased);
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
            let rebased = rebase_intrinsic_border_box(border_box, Axis::Vertical);
            taffy_style.max_size.height = Dimension::length(rebased);
          }
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }

    Ok(())
  }

  /// Resolves `fit-content` sizing keywords for a flex item.
  ///
  /// Taffy asks leaf nodes for their flex base size with `AvailableSpace::MaxContent`. If we rely
  /// solely on the measure callback, `width/height: fit-content` collapses to max-content during
  /// flex sizing and does not clamp against the flex container. Pre-resolve fit-content against
  /// the container's content-box size so flex shrink/grow uses the correct base size.
  fn apply_flex_fit_content_keywords(
    &self,
    box_node: &BoxNode,
    is_root: bool,
    containing_flex: Option<&ComputedStyle>,
    constraints: &LayoutConstraints,
    taffy_style: &mut taffy::style::Style,
  ) -> Result<(), LayoutError> {
    if is_root {
      return Ok(());
    }

    let style = box_node.style.as_ref();
    let needs = style
      .width_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
      || style
        .height_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
      || style
        .min_width_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
      || style
        .min_height_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
      || style
        .max_width_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
      || style
        .max_height_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));
    if !needs {
      return Ok(());
    }

    let item_fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let item_fc = self.factory.get(item_fc_type);

    let avail_w =
      self.fit_content_available_for_axis(Axis::Horizontal, containing_flex, constraints);
    let avail_h = self.fit_content_available_for_axis(Axis::Vertical, containing_flex, constraints);

    // Padding/border percentages resolve against the containing block's physical width.
    let inline_base = match avail_w {
      FitContentAvailable::Definite(v) => v.max(0.0),
      _ => constraints
        .inline_percentage_base
        .or(constraints.width())
        .unwrap_or(self.viewport_size.width)
        .max(0.0),
    };

    let axis_edges = |axis: Axis| -> f32 {
      let padding_left = self.resolve_length_for_width(style.padding_left, inline_base, style);
      let padding_right = self.resolve_length_for_width(style.padding_right, inline_base, style);
      let padding_top = self.resolve_length_for_width(style.padding_top, inline_base, style);
      let padding_bottom = self.resolve_length_for_width(style.padding_bottom, inline_base, style);
      let border_left =
        self.resolve_length_for_width(style.used_border_left_width(), inline_base, style);
      let border_right =
        self.resolve_length_for_width(style.used_border_right_width(), inline_base, style);
      let border_top =
        self.resolve_length_for_width(style.used_border_top_width(), inline_base, style);
      let border_bottom =
        self.resolve_length_for_width(style.used_border_bottom_width(), inline_base, style);
      match axis {
        Axis::Horizontal => padding_left + padding_right + border_left + border_right,
        Axis::Vertical => padding_top + padding_bottom + border_top + border_bottom,
      }
    };

    let mut intrinsic_w: Option<(f32, f32)> = None;
    let mut intrinsic_h: Option<(f32, f32)> = None;
    let resolve_fit_content = |axis: Axis,
                               limit: Option<Length>,
                               available: FitContentAvailable,
                               cache: &mut Option<(f32, f32)>|
     -> Result<f32, LayoutError> {
      let (min_intrinsic, max_intrinsic) = match cache {
        Some(pair) => *pair,
        None => {
          let sizes = self.compute_intrinsic_sizes_for_axis(box_node, style, &item_fc, axis)?;
          *cache = Some(sizes);
          sizes
        }
      };
      let min_intrinsic = min_intrinsic.max(0.0);
      let max_intrinsic = max_intrinsic.max(0.0);
      let available_border_box = available.available_border_box(min_intrinsic, max_intrinsic);

      let preferred_border_box = match limit {
        None => None,
        Some(limit) => {
          let resolved = self
            .resolve_length_for_width(limit, available_border_box, style)
            .max(0.0);
          Some(if style.box_sizing == BoxSizing::ContentBox {
            (resolved + axis_edges(axis)).max(0.0)
          } else {
            resolved
          })
        }
      };

      Ok(
        crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
          Some(available_border_box),
          preferred_border_box,
          min_intrinsic,
          max_intrinsic,
        )
        .max(0.0),
      )
    };

    if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.min_width_keyword {
      let resolved = resolve_fit_content(Axis::Horizontal, limit, avail_w, &mut intrinsic_w)?;
      taffy_style.min_size.width = Dimension::length(resolved);
    }
    if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.max_width_keyword {
      let resolved = resolve_fit_content(Axis::Horizontal, limit, avail_w, &mut intrinsic_w)?;
      taffy_style.max_size.width = Dimension::length(resolved);
    }
    if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.width_keyword {
      let resolved = resolve_fit_content(Axis::Horizontal, limit, avail_w, &mut intrinsic_w)?;
      taffy_style.size.width = Dimension::length(resolved);
    }

    if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.min_height_keyword {
      let resolved = resolve_fit_content(Axis::Vertical, limit, avail_h, &mut intrinsic_h)?;
      taffy_style.min_size.height = Dimension::length(resolved);
    }
    if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.max_height_keyword {
      let resolved = resolve_fit_content(Axis::Vertical, limit, avail_h, &mut intrinsic_h)?;
      taffy_style.max_size.height = Dimension::length(resolved);
    }
    if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.height_keyword {
      let resolved = resolve_fit_content(Axis::Vertical, limit, avail_h, &mut intrinsic_h)?;
      taffy_style.size.height = Dimension::length(resolved);
    }

    Ok(())
  }

  fn clear_intrinsic_size_keywords(style: &mut ComputedStyle) {
    style.width_keyword = None;
    style.height_keyword = None;
    style.min_width_keyword = None;
    style.min_height_keyword = None;
    style.max_width_keyword = None;
    style.max_height_keyword = None;
  }

  fn compute_intrinsic_sizes_for_axis(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    fc: &Arc<dyn FormattingContext>,
    axis: Axis,
  ) -> Result<(f32, f32), LayoutError> {
    let physical_axis = match axis {
      Axis::Horizontal => PhysicalAxis::X,
      Axis::Vertical => PhysicalAxis::Y,
    };

    let compute_sizes = |node: &BoxNode| -> Result<(f32, f32), LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
        fc.as_ref(),
        node,
        physical_axis,
      )
    };

    let needs_override = style.width_keyword.is_some()
      || style.height_keyword.is_some()
      || style.min_width_keyword.is_some()
      || style.min_height_keyword.is_some()
      || style.max_width_keyword.is_some()
      || style.max_height_keyword.is_some();
    if !needs_override {
      return compute_sizes(box_node);
    }

    let mut override_style = style.clone();
    Self::clear_intrinsic_size_keywords(&mut override_style);
    let override_style = Arc::new(override_style);
    let box_id = box_node.id();
    if box_id != 0 {
      return crate::layout::style_override::with_style_override(box_id, override_style, || {
        compute_sizes(box_node)
      });
    }

    // Some tests use `id=0` for all boxes, which would collide in the override stack. Deep clone
    // in that case.
    let mut cloned = box_node.clone();
    cloned.style = override_style;
    compute_sizes(&cloned)
  }

  fn fit_content_available_for_axis(
    &self,
    axis: Axis,
    containing_flex: Option<&ComputedStyle>,
    constraints: &LayoutConstraints,
  ) -> FitContentAvailable {
    let used_border_box = match axis {
      Axis::Horizontal => constraints.used_border_box_width,
      Axis::Vertical => constraints.used_border_box_height,
    };

    // Prefer the flex container's resolved border-box size for computing the available size that
    // `fit-content` clamps against. In many call sites (notably the root formatting context),
    // `constraints.available_width` is the *containing block* width, while the flex container
    // itself has a definite `width`/`height` smaller than that.
    //
    // Using the containing block width here would cause `fit-content` flex items to clamp against
    // the wrong size (effectively behaving like `fit-content(<containing-block>)`).
    let mut space = match (axis, used_border_box) {
      (_, Some(v)) => CrateAvailableSpace::Definite(v),
      (Axis::Horizontal, None) => constraints.available_width,
      (Axis::Vertical, None) => constraints.available_height,
    };

    if used_border_box.is_none() {
      if let Some(container_style) = containing_flex {
        // Percentages on `width` and padding resolve against the flex container's containing block
        // inline size; percentages on `height` resolve against the containing block block size.
        let inline_base = constraints
          .inline_percentage_base
          .or(constraints.width())
          .unwrap_or(self.viewport_size.width)
          .max(0.0);
        let block_base = constraints
          .height()
          .unwrap_or(self.viewport_size.height)
          .max(0.0);

        let resolve_edges = |axis: Axis| -> f32 {
          let padding_left = self.resolve_length_for_width(
            container_style.padding_left,
            inline_base,
            container_style,
          );
          let padding_right = self.resolve_length_for_width(
            container_style.padding_right,
            inline_base,
            container_style,
          );
          let padding_top = self.resolve_length_for_width(
            container_style.padding_top,
            inline_base,
            container_style,
          );
          let padding_bottom = self.resolve_length_for_width(
            container_style.padding_bottom,
            inline_base,
            container_style,
          );
          let border_left = self.resolve_length_for_width(
            container_style.used_border_left_width(),
            inline_base,
            container_style,
          );
          let border_right = self.resolve_length_for_width(
            container_style.used_border_right_width(),
            inline_base,
            container_style,
          );
          let border_top = self.resolve_length_for_width(
            container_style.used_border_top_width(),
            inline_base,
            container_style,
          );
          let border_bottom = self.resolve_length_for_width(
            container_style.used_border_bottom_width(),
            inline_base,
            container_style,
          );
          match axis {
            Axis::Horizontal => padding_left + padding_right + border_left + border_right,
            Axis::Vertical => padding_top + padding_bottom + border_top + border_bottom,
          }
        };

        let border_box_size = match axis {
          Axis::Horizontal => container_style.width.map(|len| {
            let resolved = self
              .resolve_length_for_width(len, inline_base, container_style)
              .max(0.0);
            if container_style.box_sizing == BoxSizing::ContentBox {
              (resolved + resolve_edges(Axis::Horizontal)).max(0.0)
            } else {
              resolved
            }
          }),
          Axis::Vertical => container_style.height.map(|len| {
            let resolved = self
              .resolve_length_for_width(len, block_base, container_style)
              .max(0.0);
            if container_style.box_sizing == BoxSizing::ContentBox {
              (resolved + resolve_edges(Axis::Vertical)).max(0.0)
            } else {
              resolved
            }
          }),
        };

        if let Some(border_box) = border_box_size {
          space = CrateAvailableSpace::Definite(border_box);
        }
      }
    }

    let mut value = match space {
      CrateAvailableSpace::Definite(v) => FitContentAvailable::Definite(v.max(0.0)),
      CrateAvailableSpace::MinContent => FitContentAvailable::MinContent,
      CrateAvailableSpace::MaxContent => FitContentAvailable::MaxContent,
      CrateAvailableSpace::Indefinite => FitContentAvailable::MaxContent,
    };

    // For flex items, `fit-content` clamps against the flex container's content box size.
    if let (FitContentAvailable::Definite(mut definite), Some(container_style)) =
      (value, containing_flex)
    {
      // Padding percentages always resolve against the containing block's physical width
      // (CSS2.1 §8.1), even for vertical edges.
      let percentage_base = constraints
        .inline_percentage_base
        .or(constraints.used_border_box_width)
        .or(constraints.width())
        .unwrap_or_else(|| match axis {
          Axis::Horizontal => definite,
          Axis::Vertical => self.viewport_size.width,
        })
        .max(0.0);
      let border_left = self.resolve_length_for_width(
        container_style.used_border_left_width(),
        percentage_base,
        container_style,
      );
      let border_right = self.resolve_length_for_width(
        container_style.used_border_right_width(),
        percentage_base,
        container_style,
      );
      let border_top = self.resolve_length_for_width(
        container_style.used_border_top_width(),
        percentage_base,
        container_style,
      );
      let border_bottom = self.resolve_length_for_width(
        container_style.used_border_bottom_width(),
        percentage_base,
        container_style,
      );
      let padding_left = self.resolve_length_for_width(
        container_style.padding_left,
        percentage_base,
        container_style,
      );
      let padding_right = self.resolve_length_for_width(
        container_style.padding_right,
        percentage_base,
        container_style,
      );
      let padding_top = self.resolve_length_for_width(
        container_style.padding_top,
        percentage_base,
        container_style,
      );
      let padding_bottom = self.resolve_length_for_width(
        container_style.padding_bottom,
        percentage_base,
        container_style,
      );

      let reserve_scroll_x = matches!(container_style.overflow_x, CssOverflow::Scroll)
        || (container_style.scrollbar_gutter.stable
          && matches!(
            container_style.overflow_x,
            CssOverflow::Auto | CssOverflow::Scroll
          ));
      let reserve_scroll_y = matches!(container_style.overflow_y, CssOverflow::Scroll)
        || (container_style.scrollbar_gutter.stable
          && matches!(
            container_style.overflow_y,
            CssOverflow::Auto | CssOverflow::Scroll
          ));
      let scrollbar_width = resolve_scrollbar_width(container_style);

      let inset = match axis {
        Axis::Horizontal => {
          let mut inset = border_left + border_right + padding_left + padding_right;
          if reserve_scroll_y {
            inset += scrollbar_width;
          }
          inset
        }
        Axis::Vertical => {
          let mut inset = border_top + border_bottom + padding_top + padding_bottom;
          if reserve_scroll_x {
            inset += scrollbar_width;
          }
          inset
        }
      };

      definite = (definite - inset).max(0.0);
      value = FitContentAvailable::Definite(definite);
    }

    value
  }

  /// Applies Flexbox's automatic minimum size (min-width/height:auto) for a specific box instance.
  ///
  /// This is content-dependent: it may run intrinsic sizing probes and therefore must *not* be
  /// cached across different boxes that share the same styles/structure.
  fn apply_flex_auto_min_size(
    &self,
    box_node: &BoxNode,
    is_root: bool,
    containing_flex: Option<&ComputedStyle>,
    skip_contents: bool,
    taffy_style: &mut taffy::style::Style,
  ) -> Result<(), LayoutError> {
    if is_root {
      return Ok(());
    }

    let Some(container) = containing_flex else {
      return Ok(());
    };

    // Flexbox automatic minimum size (min-width/min-height: auto) applies on the flex item's
    // *main* axis (driven by the containing flex container), not the item's own flex-direction.
    // Taffy treats `auto` as zero, so compute the content-based minimum size to prevent
    // shrink-to-zero flex items. Flexbox specifies that scroll containers use a 0 automatic
    // minimum, so only apply this when overflow is `visible`.
    let container_inline_is_horizontal =
      matches!(container.writing_mode, WritingMode::HorizontalTb);
    let container_main_is_inline = matches!(
      container.flex_direction,
      FlexDirection::Row | FlexDirection::RowReverse
    );
    let container_main_is_horizontal = if container_inline_is_horizontal {
      container_main_is_inline
    } else {
      !container_main_is_inline
    };

    let style = &box_node.style;
    let style_ptr = Arc::as_ptr(&box_node.style) as usize;
    let box_id = box_node.id();

    if container_main_is_horizontal {
      if taffy_style.min_size.width == Dimension::AUTO
        && matches!(style.overflow_x, CssOverflow::Visible)
      {
        if let Some(cached) = flex_auto_min_cache_lookup(box_id, style_ptr, true, skip_contents) {
          if let Some(min_candidate) = cached {
            if min_candidate.is_finite() && min_candidate > 0.0 {
              taffy_style.min_size.width = Dimension::length(min_candidate);
            }
          }
          return Ok(());
        }
        let specified_size_suggestion = style.width.as_ref().and_then(|len| {
          let px = self.resolve_length_px(len, style)?;
          if px <= 0.0 {
            return None;
          }
          let mut value = px;
          if style.box_sizing == BoxSizing::ContentBox {
            value += self.horizontal_edges_px(style)?;
          }
          Some(value.max(0.0))
        });
        let transferred_size_suggestion = match style.aspect_ratio {
          AspectRatio::Ratio(ratio) if ratio > 0.0 && ratio.is_finite() => style.height.as_ref().and_then(|len| {
            let cross_px = self.resolve_length_px(len, style)?;
            if !cross_px.is_finite() {
              return None;
            }
            let mut cross_border_box = cross_px.max(0.0);
            if style.box_sizing == BoxSizing::ContentBox {
              cross_border_box += self.vertical_edges_px(style)?;
            }
            if !cross_border_box.is_finite() {
              return None;
            }
            let transferred = cross_border_box * ratio;
            if transferred.is_finite() {
              Some(transferred.max(0.0))
            } else {
              None
            }
          }),
          _ => None,
        };
        let max_main_size = style.max_width.as_ref().and_then(|len| {
          let px = self.resolve_length_px(len, style)?;
          if !px.is_finite() {
            return None;
          }
          let mut value = px.max(0.0);
          if style.box_sizing == BoxSizing::ContentBox {
            value += self.horizontal_edges_px(style)?;
          }
          value = value.max(0.0);
          value.is_finite().then_some(value)
        });

        if skip_contents {
          let mut content_size_suggestion = style
            .contain_intrinsic_width
            .length
            .as_ref()
            .and_then(|len| self.resolve_length_px(len, style))
            .unwrap_or(0.0);
          if style.box_sizing == BoxSizing::ContentBox {
            content_size_suggestion += self.horizontal_edges_px(style).unwrap_or(0.0);
          }
          content_size_suggestion = content_size_suggestion.max(0.0);

          let mut min_candidate = content_size_suggestion;
          if let Some(transferred) = transferred_size_suggestion {
            if box_node.is_replaced() {
              min_candidate = min_candidate.min(transferred);
            } else {
              min_candidate = min_candidate.max(transferred);
            }
          }
          if let Some(specified) = specified_size_suggestion {
            min_candidate = min_candidate.min(specified);
          }
          if let Some(max_main) = max_main_size {
            min_candidate = min_candidate.min(max_main);
          }
          if min_candidate.is_finite() && min_candidate > 0.0 {
            taffy_style.min_size.width = Dimension::length(min_candidate);
          }
          flex_auto_min_cache_store(
            box_id,
            style_ptr,
            true,
            skip_contents,
            (min_candidate.is_finite() && min_candidate > 0.0).then_some(min_candidate),
          );
          return Ok(());
        }

        let item_fc_type = box_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let item_fc = self.factory.get(item_fc_type);

        // `compute_intrinsic_inline_size` for block formatting contexts respects authored widths.
        // For flexbox auto minimum sizing we want the *content size suggestion* instead, which
        // ignores the preferred size (CSS Flexbox §4.5). When a definite preferred size exists,
        // the content-based minimum is the smaller of the preferred size suggestion and the
        // content size suggestion.
        let needs_override = style.width.is_some() || style.width_keyword.is_some();
        let intrinsic_result = if needs_override {
          let mut override_style: ComputedStyle = (*box_node.style).clone();
          override_style.width = None;
          override_style.width_keyword = None;
          if box_node.id != 0 {
            crate::layout::style_override::with_style_override(
              box_node.id,
              Arc::new(override_style),
              || item_fc.compute_intrinsic_inline_size(box_node, IntrinsicSizingMode::MinContent),
            )
          } else {
            // Tests and other ad-hoc callers sometimes build `BoxNode` trees without assigning
            // unique ids (they default to 0). Style overrides are keyed by id, so fall back to the
            // old cloning approach when ids are not initialized to avoid collisions.
            let mut cloned = box_node.clone();
            cloned.style = Arc::new(override_style);
            item_fc.compute_intrinsic_inline_size(&cloned, IntrinsicSizingMode::MinContent)
          }
        } else {
          item_fc.compute_intrinsic_inline_size(box_node, IntrinsicSizingMode::MinContent)
        };

        match intrinsic_result {
          Ok(content_size_suggestion) => {
            let mut min_candidate = content_size_suggestion;
            if let Some(transferred) = transferred_size_suggestion {
              if box_node.is_replaced() {
                min_candidate = min_candidate.min(transferred);
              } else {
                min_candidate = min_candidate.max(transferred);
              }
            }
            if let Some(specified) = specified_size_suggestion {
              min_candidate = min_candidate.min(specified);
            }
            if let Some(max_main) = max_main_size {
              min_candidate = min_candidate.min(max_main);
            }
            if min_candidate.is_finite() && min_candidate > 0.0 {
              taffy_style.min_size.width = Dimension::length(min_candidate);
            }
            flex_auto_min_cache_store(
              box_id,
              style_ptr,
              true,
              skip_contents,
              (min_candidate.is_finite() && min_candidate > 0.0).then_some(min_candidate),
            );
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {
            flex_auto_min_cache_store(box_id, style_ptr, true, skip_contents, None);
          }
        }
      }
    } else if taffy_style.min_size.height == Dimension::AUTO
      && matches!(style.overflow_y, CssOverflow::Visible)
    {
      if let Some(cached) = flex_auto_min_cache_lookup(box_id, style_ptr, false, skip_contents) {
        if let Some(min_candidate) = cached {
          if min_candidate.is_finite() && min_candidate > 0.0 {
            taffy_style.min_size.height = Dimension::length(min_candidate);
          }
        }
        return Ok(());
      }
      let specified_size_suggestion = style.height.as_ref().and_then(|len| {
        let px = self.resolve_length_px(len, style)?;
        if px <= 0.0 {
          return None;
        }
        let mut value = px;
        if style.box_sizing == BoxSizing::ContentBox {
          value += self.vertical_edges_px(style)?;
        }
        Some(value.max(0.0))
      });
      let transferred_size_suggestion = match style.aspect_ratio {
        AspectRatio::Ratio(ratio) if ratio > 0.0 && ratio.is_finite() => style.width.as_ref().and_then(|len| {
          let cross_px = self.resolve_length_px(len, style)?;
          if !cross_px.is_finite() {
            return None;
          }
          let mut cross_border_box = cross_px.max(0.0);
          if style.box_sizing == BoxSizing::ContentBox {
            cross_border_box += self.horizontal_edges_px(style)?;
          }
          if !cross_border_box.is_finite() {
            return None;
          }
          let transferred = cross_border_box / ratio;
          if transferred.is_finite() {
            Some(transferred.max(0.0))
          } else {
            None
          }
        }),
        _ => None,
      };
      let max_main_size = style.max_height.as_ref().and_then(|len| {
        let px = self.resolve_length_px(len, style)?;
        if !px.is_finite() {
          return None;
        }
        let mut value = px.max(0.0);
        if style.box_sizing == BoxSizing::ContentBox {
          value += self.vertical_edges_px(style)?;
        }
        value = value.max(0.0);
        value.is_finite().then_some(value)
      });

      if skip_contents {
        let mut content_size_suggestion = style
          .contain_intrinsic_height
          .length
          .as_ref()
          .and_then(|len| self.resolve_length_px(len, style))
          .unwrap_or(0.0);
        if style.box_sizing == BoxSizing::ContentBox {
          content_size_suggestion += self.vertical_edges_px(style).unwrap_or(0.0);
        }
        content_size_suggestion = content_size_suggestion.max(0.0);

        let mut min_candidate = content_size_suggestion;
        if let Some(transferred) = transferred_size_suggestion {
          if box_node.is_replaced() {
            min_candidate = min_candidate.min(transferred);
          } else {
            min_candidate = min_candidate.max(transferred);
          }
        }
        if let Some(specified) = specified_size_suggestion {
          min_candidate = min_candidate.min(specified);
        }
        if let Some(max_main) = max_main_size {
          min_candidate = min_candidate.min(max_main);
        }
        if min_candidate.is_finite() && min_candidate > 0.0 {
          taffy_style.min_size.height = Dimension::length(min_candidate);
        }
        flex_auto_min_cache_store(
          box_id,
          style_ptr,
          false,
          skip_contents,
          (min_candidate.is_finite() && min_candidate > 0.0).then_some(min_candidate),
        );
        return Ok(());
      }

      let item_fc_type = box_node
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let item_fc = self.factory.get(item_fc_type);
      let needs_override = style.height.is_some() || style.height_keyword.is_some();
      let intrinsic_result = if needs_override {
        let mut override_style: ComputedStyle = (*box_node.style).clone();
        override_style.height = None;
        override_style.height_keyword = None;
        if box_node.id != 0 {
          crate::layout::style_override::with_style_override(
            box_node.id,
            Arc::new(override_style),
            || item_fc.compute_intrinsic_block_size(box_node, IntrinsicSizingMode::MinContent),
          )
        } else {
          let mut cloned = box_node.clone();
          cloned.style = Arc::new(override_style);
          item_fc.compute_intrinsic_block_size(&cloned, IntrinsicSizingMode::MinContent)
        }
      } else {
        item_fc.compute_intrinsic_block_size(box_node, IntrinsicSizingMode::MinContent)
      };

      match intrinsic_result {
        Ok(content_size_suggestion) => {
          let mut min_candidate = content_size_suggestion;
          if let Some(transferred) = transferred_size_suggestion {
            if box_node.is_replaced() {
              min_candidate = min_candidate.min(transferred);
            } else {
              min_candidate = min_candidate.max(transferred);
            }
          }
          if let Some(specified) = specified_size_suggestion {
            min_candidate = min_candidate.min(specified);
          }
          if let Some(max_main) = max_main_size {
            min_candidate = min_candidate.min(max_main);
          }
          if min_candidate.is_finite() && min_candidate > 0.0 {
            taffy_style.min_size.height = Dimension::length(min_candidate);
          }
          flex_auto_min_cache_store(
            box_id,
            style_ptr,
            false,
            skip_contents,
            (min_candidate.is_finite() && min_candidate > 0.0).then_some(min_candidate),
          );
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {
          flex_auto_min_cache_store(box_id, style_ptr, false, skip_contents, None);
        }
      }
    }

    Ok(())
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
    // Padding percentages resolve against the physical width, even for vertical edges.
    let padding_left = self.resolve_length_for_width(style.padding_left, width_base, style);
    let padding_right = self.resolve_length_for_width(style.padding_right, width_base, style);
    let padding_top = self.resolve_length_for_width(style.padding_top, width_base, style);
    let padding_bottom = self.resolve_length_for_width(style.padding_bottom, width_base, style);
    let border_left = self.resolve_length_for_width(style.border_left_width, width_base, style);
    let border_right = self.resolve_length_for_width(style.border_right_width, width_base, style);
    let border_top = self.resolve_length_for_width(style.border_top_width, width_base, style);
    let border_bottom = self.resolve_length_for_width(style.border_bottom_width, width_base, style);
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

    // Avoid self-recursion when intrinsic sizing needs to re-enter layout (e.g. block-size probes)
    // by clearing any fit-content sizing keywords on the container.
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

    // Apply authored min/max constraints on the same axis. These clamp the fit-content result.
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

  /// Computes the size for a node
  ///
  /// For the root flex container without explicit size, we use 100% to fill
  /// available space (simulating block-level behavior).
  fn compute_size(&self, style: &ComputedStyle, is_root: bool) -> taffy::geometry::Size<Dimension> {
    let is_block_level_root = is_root && matches!(style.display, Display::Flex);
    let width = match (style.width.as_ref(), style.width_keyword) {
      (Some(len), _) => self.dimension_for_box_sizing(len, style, Axis::Horizontal),
      (None, Some(_)) => Dimension::auto(),
      (None, None) if is_block_level_root => {
        // Root flex container without explicit width: expand to fill
        // available space (100% of containing block).
        Dimension::percent(1.0)
      }
      (None, None) => Dimension::auto(),
    };

    let height = match (style.height.as_ref(), style.height_keyword) {
      (Some(len), _) => self.dimension_for_box_sizing(len, style, Axis::Vertical),
      (None, Some(_)) => Dimension::auto(),
      (None, None) => Dimension::auto(), // Height always auto unless specified
    };

    taffy::geometry::Size { width, height }
  }

  /// Converts Taffy layout back to FragmentNode tree
  #[allow(clippy::only_used_in_recursion)]
  fn taffy_to_fragment(
    &self,
    taffy_tree: &TaffyTree<*const BoxNode>,
    taffy_node: NodeId,
    root_id: NodeId,
    box_node: &BoxNode,
    node_map: &FxHashMap<*const BoxNode, NodeId>,
    constraints: &LayoutConstraints,
    auto_unskipped: Option<&FxHashSet<*const BoxNode>>,
    scroll_sensitive: &FxHashSet<*const BoxNode>,
  ) -> Result<FragmentNode, LayoutError> {
    // Get layout from Taffy
    let layout = taffy_tree
      .layout(taffy_node)
      .map_err(|e| LayoutError::MissingContext(format!("Failed to get Taffy layout: {:?}", e)))?;

    // Create fragment rect (Taffy uses relative coordinates)
    let mut rect = Rect::new(
      Point::new(layout.location.x, layout.location.y),
      Size::new(layout.size.width, layout.size.height),
    );
    if taffy_node == root_id {
      // When Taffy collapses the flex container to ~0px (often after a bad intrinsic probe),
      // fall back to the definite available width so children aren't clamped to a 0–1px line.
      let rect_eps = 0.01;
      if !rect.width().is_finite() || rect.width() <= rect_eps {
        if let Some(def_w) = constraints
          .width()
          .filter(|w| w.is_finite() && *w > rect_eps)
        {
          rect.size.width = def_w;
        } else if let Some(base) = constraints.inline_percentage_base.filter(|w| *w > rect_eps) {
          rect.size.width = base;
        } else {
          rect.size.width = self.viewport_size.width;
        }
      }
      // Clamp overly wide layouts back to the definite available inline size so children
      // are reflowed within the container instead of inheriting a runaway max-content span.
      if let Some(def_w) = constraints
        .width()
        .filter(|w| w.is_finite() && *w > rect_eps)
      {
        if rect.size.width > def_w + rect_eps {
          rect.size.width = def_w;
        }
      }
    }

    // Convert children by re-running layout with the definite sizes Taffy resolved.
    // This preserves the fully laid-out fragment trees (text, inline content) instead of
    // reconstructing empty boxes from the cached measure results.
    let mut children: Vec<FragmentNode>;
    let factory = self.child_factory();
    let measured_fragments = self.measured_fragments.clone();
    let main_axis_is_row = matches!(
      box_node.style.flex_direction,
      crate::style::types::FlexDirection::Row | crate::style::types::FlexDirection::RowReverse
    );
    // `flex-direction: row` follows the inline axis; `column` follows the block axis. In vertical
    // writing modes the inline axis is vertical, so many heuristics must use the *physical* main
    // axis orientation instead of assuming "row == x-axis".
    let inline_is_horizontal = matches!(box_node.style.writing_mode, WritingMode::HorizontalTb);
    let block_is_horizontal = !inline_is_horizontal;
    let main_axis_is_horizontal = if main_axis_is_row {
      inline_is_horizontal
    } else {
      block_is_horizontal
    };
    let allow_overflow_fallback = !matches!(box_node.style.flex_wrap, FlexWrap::NoWrap)
      && if main_axis_is_horizontal {
        matches!(box_node.style.overflow_x, CssOverflow::Visible)
      } else {
        matches!(box_node.style.overflow_y, CssOverflow::Visible)
      };
    let toggles = crate::debug::runtime::runtime_toggles();
    let wrap_eps = 0.5;
    #[derive(Clone, Copy)]
    struct ChildMetrics {
      child_loc_x: f32,
      child_loc_y: f32,
      layout_width: f32,
      layout_height: f32,
      target_width: f32,
      target_height: f32,
    }

    #[derive(Clone)]
    enum ChildPlan {
      Skip,
      ContentVisibilityPlaceholder,
      Reuse {
        stored_size: Size,
        fragment: std::sync::Arc<FragmentNode>,
      },
      Replaced,
      NeedsLayout,
    }

    struct ChildLayoutWorkItem<'a> {
      dom_idx: usize,
      child_box: &'a BoxNode,
      fc_type: FormattingContextType,
      /// The child's Taffy-resolved origin in the flex container's coordinate space.
      ///
      /// Used to translate the viewport scroll offset into the child's local coordinate system so
      /// nested formatting contexts can make correct `content-visibility:auto` culling decisions.
      origin: Point,
      constraints: LayoutConstraints,
      used_border_box_width: Option<f32>,
      used_border_box_height: Option<f32>,
      layout_child_storage: Option<BoxNode>,
      layout_width: f32,
      layout_height: f32,
    }

    struct ChildLayoutWorkOutput {
      fragment: FragmentNode,
      intrinsic_size: Size,
      max_content: Option<(FragmentNode, Size)>,
    }

    let child_count = box_node.children.len();
    let rect_w = rect.width();
    let rect_h = rect.height();
    let eps = 0.01;

    let mut child_metrics: Vec<Option<ChildMetrics>> = vec![None; child_count];
    let mut child_plans: Vec<ChildPlan> = vec![ChildPlan::Skip; child_count];
    let mut layout_work: Vec<ChildLayoutWorkItem<'_>> = Vec::new();
    let mut deadline_counter = 0usize;

    // Pre-pass to compute sizing/position inputs and decide which children require real layout.
    for (dom_idx, child_box) in box_node.children.iter().enumerate() {
      check_layout_deadline(&mut deadline_counter)?;
      let Some(&child_taffy) = node_map.get(&(child_box as *const BoxNode)) else {
        continue;
      };

      let child_layout = taffy_tree
        .layout(child_taffy)
        .map_err(|e| LayoutError::MissingContext(format!("Failed to get Taffy layout: {:?}", e)))?;
      let child_loc_x = child_layout.location.x - rect.origin.x;
      let child_loc_y = child_layout.location.y - rect.origin.y;
      let mut layout_width = child_layout.size.width;
      let mut layout_height = child_layout.size.height;
      let raw_layout_width = layout_width;
      let raw_layout_height = layout_height;
      let target_width = if raw_layout_width > eps {
        raw_layout_width
      } else if rect_w.is_finite() && rect_w > eps {
        rect_w
      } else {
        self.viewport_size.width
      };
      let target_height = if raw_layout_height > eps {
        raw_layout_height
      } else if rect_h.is_finite() && rect_h > eps {
        rect_h
      } else {
        self.viewport_size.height
      };
      if toggles.truthy("FASTR_DEBUG_FLEX_CHILD") {
        eprintln!(
                    "[flex-child-layout] id={} selector={} layout=({:.2},{:.2}) loc=({:.2},{:.2}) grow={} shrink={} basis={:?}",
                    child_box.id,
                    child_box
                        .debug_info
                        .as_ref()
                        .map(|d| d.to_selector())
                        .unwrap_or_else(|| "<anon>".to_string()),
                    layout_width,
                    layout_height,
                    child_loc_x,
                    child_loc_y,
                    child_box.style.flex_grow,
                    child_box.style.flex_shrink,
                    child_box.style.flex_basis
                );
      }

      // Guard against zero-sized cross axes coming from overly tight flex constraints
      // (e.g., items measured with 0px available space). When the flex container has
      // a real cross size, fall back to that (or the resolved specified size) so
      // percentage/auto widths don't collapse to zero.
      if !main_axis_is_row && layout_width <= eps {
        let explicit_zero_width = child_box
          .style
          .width
          .as_ref()
          .map(|l| l.unit.is_absolute() && l.value.abs() <= eps && !l.unit.is_percentage())
          .unwrap_or(false);
        if !explicit_zero_width {
          let base = if rect_w.is_finite() && rect_w > eps {
            rect_w
          } else {
            self.viewport_size.width
          };
          if let Some(specified) = child_box
            .style
            .width
            .as_ref()
            .map(|l| self.resolve_length_for_width(*l, base, &child_box.style))
          {
            if specified > eps {
              layout_width = specified;
            }
          }
          if layout_width <= eps && base > eps {
            layout_width = base;
          }
        }
      }
      if main_axis_is_row && layout_height <= eps {
        let explicit_zero_height = child_box
          .style
          .height
          .as_ref()
          .map(|l| l.unit.is_absolute() && l.value.abs() <= eps && !l.unit.is_percentage())
          .unwrap_or(false);
        if !explicit_zero_height {
          let base = if rect_h.is_finite() && rect_h > eps {
            rect_h
          } else {
            self.viewport_size.height
          };
          if let Some(specified) = child_box
            .style
            .height
            .as_ref()
            .map(|l| self.resolve_length_for_width(*l, base, &child_box.style))
          {
            if specified > eps {
              layout_height = specified;
            }
          }
          if layout_height <= eps && base > eps {
            layout_height = base;
          }
        }
      }
      let needs_intrinsic_main = (main_axis_is_row && raw_layout_width <= eps)
        || (!main_axis_is_row && raw_layout_height <= eps);

      child_metrics[dom_idx] = Some(ChildMetrics {
        child_loc_x,
        child_loc_y,
        layout_width,
        layout_height,
        target_width,
        target_height,
      });

      let skip_contents = match child_box.style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          self.content_visibility_auto_has_definite_placeholder(child_box)
            && auto_unskipped
              .map(|set| !set.contains(&(child_box as *const BoxNode)))
              .unwrap_or(false)
        }
        crate::style::types::ContentVisibility::Visible => false,
      };
      if skip_contents {
        child_plans[dom_idx] = ChildPlan::ContentVisibilityPlaceholder;
        continue;
      }

      if !needs_intrinsic_main {
        let child_ptr = child_box as *const BoxNode;
        let parent_scroll = sanitize_viewport_scroll(factory.viewport_scroll());
        let child_scroll = Point::new(parent_scroll.x - child_loc_x, parent_scroll.y - child_loc_y);
        let cache_key = if scroll_sensitive.contains(&child_ptr) {
          flex_cache_key_with_scroll(child_box, child_scroll)
        } else {
          flex_cache_key(child_box)
        };
        if let Some(cached) = measured_fragments
          .find_fragment_by_border_size_exact(cache_key, Size::new(target_width, target_height))
        {
          child_plans[dom_idx] = ChildPlan::Reuse {
            stored_size: cached.border_size,
            fragment: cached.fragment,
          };
          continue;
        }
      }

      if matches!(
        child_box.box_type,
        crate::tree::box_tree::BoxType::Replaced(_)
      ) {
        child_plans[dom_idx] = ChildPlan::Replaced;
        continue;
      }

      child_plans[dom_idx] = ChildPlan::NeedsLayout;

      let fc_type = child_box
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let size_eps = 0.01;
      let used_border_box_width =
        (raw_layout_width.is_finite() && raw_layout_width > size_eps).then_some(raw_layout_width);
      let used_border_box_height = (raw_layout_height.is_finite() && raw_layout_height > size_eps)
        .then_some(raw_layout_height);

      let supports_used_border_box = matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Flex
          | FormattingContextType::Grid
          | FormattingContextType::Inline
      );

      // Preserve the flex-resolved size. When the child formatting context can honor
      // `constraints.used_border_box_*`, avoid deep-cloning the entire box subtree just to inject
      // synthetic `width/height` declarations.
      let layout_child_storage = if fc_type == FormattingContextType::Block
        || (supports_used_border_box
          && used_border_box_width.is_some()
          && used_border_box_height.is_some())
      {
        None
      } else {
        let mut layout_child = child_box.clone();
        let mut layout_style = (*layout_child.style).clone();
        if used_border_box_width.is_some() {
          layout_style.width = Some(Length::px(raw_layout_width));
          layout_style.width_keyword = None;
        } else {
          layout_style.width = None;
          layout_style.min_width = None;
          layout_style.max_width = None;
          layout_style.width_keyword = None;
          layout_style.min_width_keyword = None;
          layout_style.max_width_keyword = None;
        }
        if used_border_box_height.is_some() {
          layout_style.height = Some(Length::px(raw_layout_height));
          layout_style.height_keyword = None;
        } else {
          layout_style.height = None;
          layout_style.min_height = None;
          layout_style.max_height = None;
          layout_style.height_keyword = None;
          layout_style.min_height_keyword = None;
          layout_style.max_height_keyword = None;
        }
        layout_child.style = Arc::new(layout_style);
        Some(layout_child)
      };

      let rect_main_def = if main_axis_is_row { rect_w } else { rect_h };
      let base_constraints = if needs_intrinsic_main {
        let width = if main_axis_is_row {
          if raw_layout_width > eps {
            CrateAvailableSpace::Definite(raw_layout_width)
          } else if rect_main_def.is_finite() && rect_main_def > eps {
            CrateAvailableSpace::Definite(rect_main_def)
          } else {
            CrateAvailableSpace::MaxContent
          }
        } else if raw_layout_width > eps {
          CrateAvailableSpace::Definite(layout_width)
        } else {
          CrateAvailableSpace::MaxContent
        };
        let height = if main_axis_is_row {
          if raw_layout_height > eps {
            CrateAvailableSpace::Definite(raw_layout_height)
          } else {
            CrateAvailableSpace::MaxContent
          }
        } else if layout_height > eps {
          CrateAvailableSpace::Definite(layout_height)
        } else if rect_main_def.is_finite() && rect_main_def > eps {
          CrateAvailableSpace::Definite(rect_main_def)
        } else {
          CrateAvailableSpace::MaxContent
        };
        LayoutConstraints::new(width, height)
      } else {
        LayoutConstraints::new(
          CrateAvailableSpace::Definite(layout_width),
          CrateAvailableSpace::Definite(layout_height),
        )
      };
      let constraints = if supports_used_border_box {
        base_constraints.with_used_border_box_size(used_border_box_width, used_border_box_height)
      } else {
        base_constraints
      };
      layout_work.push(ChildLayoutWorkItem {
        dom_idx,
        child_box,
        fc_type,
        origin: Point::new(child_loc_x, child_loc_y),
        constraints,
        used_border_box_width,
        used_border_box_height,
        layout_child_storage,
        layout_width,
        layout_height,
      });
    }

    let mut layout_results: Vec<Option<ChildLayoutWorkOutput>> =
      std::iter::repeat_with(|| None).take(child_count).collect();
    if !layout_work.is_empty() {
      let layout_work_count = layout_work.len();
      let should_parallel_layout = self.parallelism.should_parallelize(layout_work_count)
        && layout_work_count >= self.parallelism.min_fanout;
      let deadline = active_deadline();
      let run_layout = |deadline_counter: &mut usize,
                        work: &ChildLayoutWorkItem<'_>|
       -> Result<(usize, ChildLayoutWorkOutput), LayoutError> {
        // Translate the viewport scroll into the child's coordinate system so nested formatting
        // contexts can correctly decide which `content-visibility:auto` descendants intersect the
        // viewport. The flex container receives a scroll offset already translated into its own
        // coordinate space by its parent (block/grid/flex); do the same for each child formatting
        // context using the Taffy placement origin.
        let parent_scroll = factory.viewport_scroll();
        let parent_scroll = if parent_scroll.x.is_finite() && parent_scroll.y.is_finite() {
          parent_scroll
        } else {
          Point::ZERO
        };
        let child_scroll = Point::new(
          parent_scroll.x - work.origin.x,
          parent_scroll.y - work.origin.y,
        );
        let child_factory = factory.clone().with_viewport_scroll(child_scroll);
        let fc = child_factory.get(work.fc_type);
        let layout_node: &BoxNode = work.layout_child_storage.as_ref().unwrap_or(work.child_box);
        let node_timer = flex_profile::node_timer();
        let selector_for_profile = node_timer
          .as_ref()
          .and_then(|_| work.child_box.debug_info.as_ref().map(|d| d.to_selector()));
        let child_fragment = fc.layout(layout_node, &work.constraints)?;
        flex_profile::record_node_layout(
          work.child_box.id,
          selector_for_profile.as_deref(),
          node_timer,
        );
        let intrinsic_size = Self::fragment_subtree_size(&child_fragment, deadline_counter)?;

        if !trace_flex_text_ids().is_empty() && trace_flex_text_ids().contains(&work.child_box.id) {
          let mut text_count = 0;
          fn walk(node: &FragmentNode, count: &mut usize) {
            if matches!(node.content, FragmentContent::Text { .. }) {
              *count += 1;
            }
            for child in node.children.iter() {
              walk(child, count);
            }
          }
          walk(&child_fragment, &mut text_count);
          let selector = work
            .child_box
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          eprintln!(
            "[flex-child-text] id={} selector={} text_fragments={} size=({:.1},{:.1})",
            work.child_box.id,
            selector,
            text_count,
            child_fragment.bounds.width(),
            child_fragment.bounds.height()
          );
        }

        let mut max_content: Option<(FragmentNode, Size)> = None;
        if (main_axis_is_row && work.layout_width <= eps)
          || (!main_axis_is_row && work.layout_height <= eps)
        {
          let mc_constraints = if main_axis_is_row {
            LayoutConstraints::new(
              CrateAvailableSpace::MaxContent,
              CrateAvailableSpace::Definite(if work.layout_height > eps {
                work.layout_height
              } else {
                intrinsic_size.height
              }),
            )
          } else {
            LayoutConstraints::new(
              CrateAvailableSpace::Definite(if work.layout_width > eps {
                work.layout_width
              } else {
                intrinsic_size.width
              }),
              CrateAvailableSpace::MaxContent,
            )
          };
          let mc_timer = flex_profile::node_timer();
          let mc_selector = mc_timer
            .as_ref()
            .and_then(|_| work.child_box.debug_info.as_ref().map(|d| d.to_selector()));
          let mc_constraints = if work.layout_child_storage.is_none()
            && matches!(
              work.fc_type,
              FormattingContextType::Block
                | FormattingContextType::Flex
                | FormattingContextType::Grid
                | FormattingContextType::Inline
            ) {
            mc_constraints
              .with_used_border_box_size(work.used_border_box_width, work.used_border_box_height)
          } else {
            mc_constraints
          };
          match fc.layout(layout_node, &mc_constraints) {
            Ok(mc_fragment) => {
              flex_profile::record_node_layout(work.child_box.id, mc_selector.as_deref(), mc_timer);
              let mc_fragment = mc_fragment;
              let mut mc_size = Self::fragment_subtree_size(&mc_fragment, deadline_counter)?;
              if rect.width().is_finite() && rect.width() > 0.0 {
                mc_size.width = mc_size.width.min(rect.width());
              }
              if rect.height().is_finite() && rect.height() > 0.0 {
                mc_size.height = mc_size.height.min(rect.height());
              }
              max_content = Some((mc_fragment, mc_size));
            }
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {}
          }
        }

        Ok((
          work.dom_idx,
          ChildLayoutWorkOutput {
            fragment: child_fragment,
            intrinsic_size,
            max_content,
          },
        ))
      };

      let outputs = if should_parallel_layout {
        let stage = active_stage();
        layout_work
          .par_iter()
          .map_init(
            || 0usize,
            |thread_deadline_counter, work| {
              with_deadline(deadline.as_ref(), || {
                let _stage_guard = StageGuard::install(stage);
                crate::layout::engine::debug_record_parallel_work();
                run_layout(thread_deadline_counter, work)
              })
            },
          )
          .collect::<Result<Vec<_>, LayoutError>>()?
      } else {
        layout_work
          .iter()
          .map(|work| run_layout(&mut deadline_counter, work))
          .collect::<Result<Vec<_>, LayoutError>>()?
      };
      for (dom_idx, output) in outputs {
        layout_results[dom_idx] = Some(output);
      }
    }

    let mut fallback_cursor_x = 0.0;
    let mut fallback_cursor_y = 0.0;
    let mut last_layout_x: Option<f32> = None;
    let mut last_layout_y: Option<f32> = None;
    let mut manual_row_positions = false;
    let mut manual_col_positions = false;
    let mut unordered_children: Vec<(i32, usize, FragmentNode)> =
      Vec::with_capacity(box_node.children.len());
    let mut unordered_children_need_sort = false;
    let mut last_unordered_key: Option<(i32, usize)> = None;

    // Sequential assembly: apply placement/fallback logic in *flex order* (CSS `order` then DOM
    // index). Taffy computes layout positions using this ordering; iterating in DOM order can
    // observe non-monotonic main-axis positions (when `order` reorders items) and incorrectly trip
    // the "manual placement" fallback, shifting items down and leaving blank space at the start
    // of the container.
    let mut ordered_dom_indices: Vec<usize> = Vec::new();
    let mut needs_dom_sort = false;
    let mut last_order: Option<i32> = None;
    for (dom_idx, child_box) in box_node.children.iter().enumerate() {
      if child_metrics[dom_idx].is_none() {
        continue;
      }
      if let Some(prev) = last_order {
        if child_box.style.order < prev {
          needs_dom_sort = true;
        }
      }
      last_order = Some(child_box.style.order);
      ordered_dom_indices.push(dom_idx);
    }
    if needs_dom_sort {
      ordered_dom_indices.sort_by(|a_idx, b_idx| {
        let a = &box_node.children[*a_idx];
        let b = &box_node.children[*b_idx];
        a.style
          .order
          .cmp(&b.style.order)
          .then_with(|| a_idx.cmp(b_idx))
      });
    }

    for dom_idx in ordered_dom_indices {
      let child_box = &box_node.children[dom_idx];
      check_layout_deadline(&mut deadline_counter)?;
      let plan = mem::replace(&mut child_plans[dom_idx], ChildPlan::Skip);
      let Some(metrics) = child_metrics[dom_idx] else {
        continue;
      };

      let child_loc_x = metrics.child_loc_x;
      let child_loc_y = metrics.child_loc_y;
      let layout_width = metrics.layout_width;
      let layout_height = metrics.layout_height;
      let target_width = metrics.target_width;
      let target_height = metrics.target_height;

      let mut final_fragment: Option<FragmentNode> = None;
      let mut store_remembered_size = false;
      match plan {
        ChildPlan::Skip => {}
        ChildPlan::ContentVisibilityPlaceholder => {
          let bounds = Rect::new(
            Point::new(child_loc_x, child_loc_y),
            Size::new(layout_width, layout_height),
          );
          final_fragment = Some(FragmentNode::new_with_style(
            bounds,
            FragmentContent::Block {
              box_id: Some(child_box.id),
            },
            vec![],
            child_box.style.clone(),
          ));
        }
        ChildPlan::Reuse {
          stored_size,
          fragment,
        } => {
          store_remembered_size = true;
          record_fragment_clone(CloneSite::FlexMeasureReuse, fragment.as_ref());
          let template = CachedFragmentTemplate::new(fragment);
          let intrinsic_size =
            Self::fragment_subtree_size(template.fragment(), &mut deadline_counter)?;
          let mut resolved_width = layout_width;
          let mut resolved_height = layout_height;
          if resolved_width <= eps && intrinsic_size.width > eps {
            resolved_width = intrinsic_size.width;
          }
          if resolved_height <= eps && intrinsic_size.height > eps {
            resolved_height = intrinsic_size.height;
          }
          if resolved_width <= eps {
            resolved_width = stored_size.width.max(resolved_width);
          }
          if resolved_height <= eps {
            resolved_height = stored_size.height.max(resolved_height);
          }
          if resolved_width <= eps {
            resolved_width = target_width;
          }
          if resolved_height <= eps {
            resolved_height = target_height;
          }
          let mut origin_x = child_loc_x;
          let mut origin_y = child_loc_y;
          if main_axis_is_row && rect.height().is_finite() {
            let limit = rect.height().max(1.0) * 5.0;
            if origin_y.abs() > limit {
              origin_y = rect.origin.y;
            }
          }
          if resolved_width <= eps && main_axis_is_row {
            manual_row_positions = true;
          }
          if resolved_height <= eps && !main_axis_is_row {
            manual_col_positions = true;
          }
          if allow_overflow_fallback && main_axis_is_row && rect_w.is_finite() && rect_w > wrap_eps
          {
            let runaway = child_loc_x.abs() > rect_w * 2.0;
            if runaway {
              manual_row_positions = true;
              fallback_cursor_x = rect.origin.x;
            }
          }
          if main_axis_is_row {
            let same_row = last_layout_y
              .map(|prev_y| (child_loc_y - prev_y).abs() < wrap_eps)
              .unwrap_or(true);
            if same_row {
              if allow_overflow_fallback {
                if child_loc_x > rect.width() + wrap_eps {
                  manual_row_positions = true;
                  fallback_cursor_x = rect.origin.x;
                }
                if child_loc_x < rect.origin.x - rect.width().abs().max(wrap_eps) {
                  manual_row_positions = true;
                  fallback_cursor_x = rect.origin.x;
                }
                if let Some(prev) = last_layout_x {
                  if child_loc_x <= prev + 0.1 {
                    manual_row_positions = true;
                  }
                }
              }
              if last_layout_x.is_none() && !manual_row_positions {
                fallback_cursor_x = child_loc_x;
              }
            } else {
              manual_row_positions = false;
              fallback_cursor_x = child_loc_x;
            }
            let use_manual_row = allow_overflow_fallback && manual_row_positions;
            if use_manual_row {
              let cap_base = if rect.width().is_finite() && rect.width() > wrap_eps {
                rect.width()
              } else {
                self.viewport_size.width
              };
              let cap = cap_base.max(wrap_eps) * 2.0 + rect.origin.x;
              if fallback_cursor_x + resolved_width > cap {
                fallback_cursor_x = rect.origin.x;
                last_layout_x = None;
              }
            }
            if use_manual_row {
              origin_x = fallback_cursor_x;
              fallback_cursor_x += resolved_width;
            } else {
              fallback_cursor_x = child_loc_x + resolved_width;
              last_layout_x = Some(child_loc_x);
              last_layout_y = Some(child_loc_y);
            }
          } else {
            if let Some(prev) = last_layout_y {
              if child_loc_y <= prev + 0.1 {
                manual_col_positions = true;
              }
            } else {
              fallback_cursor_y = child_loc_y;
            }
            if manual_col_positions {
              origin_y = fallback_cursor_y;
              fallback_cursor_y += resolved_height;
            } else {
              fallback_cursor_y = child_loc_y + resolved_height;
              last_layout_y = Some(child_loc_y);
            }
          }
          let log_child_ids = toggles
            .usize_list("FASTR_LOG_FLEX_CHILD_IDS")
            .unwrap_or_default();
          let log_child = !log_child_ids.is_empty()
            && (log_child_ids.contains(&child_box.id) || log_child_ids.contains(&box_node.id));
          if log_child {
            let selector = child_box
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anon>".to_string());
            eprintln!(
                            "[flex-child-reuse] parent_id={} child_id={} selector={} layout=({:.2},{:.2}) loc=({:.2},{:.2}) resolved=({:.2},{:.2}) rect_w={:.2} manual_row={} cursor_x={:.2} flex=({:.2},{:.2},{:?}) width={:?} min_w={:?} max_w={:?}",
                            box_node.id,
                            child_box.id,
                            selector,
                            layout_width,
                            layout_height,
                            child_loc_x,
                            child_loc_y,
                            resolved_width,
                            resolved_height,
                            rect.width(),
                            manual_row_positions,
                            fallback_cursor_x,
                            child_box.style.flex_grow,
                            child_box.style.flex_shrink,
                            child_box.style.flex_basis,
                            child_box.style.width,
                            child_box.style.min_width,
                            child_box.style.max_width
                        );
          }
          let bounds = Rect::new(
            Point::new(origin_x, origin_y),
            Size::new(resolved_width, resolved_height),
          );
          final_fragment = Some(template.place(bounds).materialize());
        }
        ChildPlan::Replaced => {
          store_remembered_size = true;
          if let crate::tree::box_tree::BoxType::Replaced(replaced_box) = &child_box.box_type {
            let bounds = Rect::new(
              Point::new(child_loc_x, child_loc_y),
              Size::new(layout_width, layout_height),
            );
            let fragment = FragmentNode::new_with_style(
              bounds,
              crate::tree::fragment_tree::FragmentContent::Replaced {
                replaced_type: replaced_box.replaced_type.clone(),
                box_id: Some(child_box.id),
              },
              vec![],
              child_box.style.clone(),
            );
            final_fragment = Some(fragment);
          }
        }
        ChildPlan::NeedsLayout => {
          store_remembered_size = true;
          let Some(output) = layout_results[dom_idx].take() else {
            return Err(LayoutError::MissingContext(
              "Missing flex child layout output".to_string(),
            ));
          };
          let mut child_fragment = output.fragment;
          let intrinsic_size = output.intrinsic_size;

          if (main_axis_is_row && layout_width <= eps)
            || (!main_axis_is_row && layout_height <= eps)
          {
            if let Some((mut mc_fragment, mc_size)) = output.max_content {
              let mut origin = Point::new(child_loc_x, child_loc_y);
              if main_axis_is_row {
                let same_row = last_layout_y
                  .map(|prev_y| (child_loc_y - prev_y).abs() < wrap_eps)
                  .unwrap_or(true);
                if !same_row || last_layout_x.is_none() {
                  fallback_cursor_x = child_loc_x;
                }
                origin.x = fallback_cursor_x;
                fallback_cursor_x += mc_size.width;
                last_layout_x = Some(child_loc_x);
                last_layout_y = Some(child_loc_y);
              } else {
                let same_col = last_layout_x
                  .map(|prev_x| (child_loc_x - prev_x).abs() < wrap_eps)
                  .unwrap_or(true);
                if !same_col || last_layout_y.is_none() {
                  fallback_cursor_y = child_loc_y;
                }
                origin.y = fallback_cursor_y;
                fallback_cursor_y += mc_size.height;
                last_layout_x = Some(child_loc_x);
                last_layout_y = Some(child_loc_y);
              }
              if !main_axis_is_row && layout_width > eps {
                origin.x = child_loc_x;
              }
              mc_fragment.bounds = Rect::new(origin, mc_size);
              final_fragment = Some(mc_fragment);
            }
          }

          if final_fragment.is_none() {
            // Position the child using the Taffy-computed coordinates (relative to parent).
            let mut resolved_width = layout_width;
            let mut resolved_height = layout_height;
            if resolved_width <= eps && intrinsic_size.width > eps {
              resolved_width = intrinsic_size.width;
            }
            if resolved_height <= eps && intrinsic_size.height > eps {
              resolved_height = intrinsic_size.height;
            }
            let mut origin_x = child_loc_x;
            let mut origin_y = child_loc_y;
            if main_axis_is_row && rect.height().is_finite() {
              let limit = rect.height().max(1.0) * 5.0;
              if origin_y.abs() > limit {
                origin_y = rect.origin.y;
              }
            }
            if resolved_width <= eps && main_axis_is_row {
              manual_row_positions = true;
            }
            if resolved_height <= eps && !main_axis_is_row {
              manual_col_positions = true;
            }
            if main_axis_is_row && rect_w.is_finite() && rect_w > wrap_eps {
              let runaway = child_loc_x.abs() > rect_w * 2.0;
              if runaway {
                manual_row_positions = true;
                fallback_cursor_x = rect.origin.x;
              }
            }
            if main_axis_is_row {
              let same_row = last_layout_y
                .map(|prev_y| (child_loc_y - prev_y).abs() < wrap_eps)
                .unwrap_or(true);
              if same_row {
                if allow_overflow_fallback {
                  if child_loc_x > rect.width() + wrap_eps {
                    manual_row_positions = true;
                    fallback_cursor_x = rect.origin.x;
                  }
                  if child_loc_x < rect.origin.x - rect.width().abs().max(wrap_eps) {
                    manual_row_positions = true;
                    fallback_cursor_x = rect.origin.x;
                  }
                  if let Some(prev) = last_layout_x {
                    if child_loc_x <= prev + 0.1 {
                      manual_row_positions = true;
                    }
                  }
                }
                if last_layout_x.is_none() && !manual_row_positions {
                  fallback_cursor_x = child_loc_x;
                }
              } else {
                manual_row_positions = false;
                fallback_cursor_x = child_loc_x;
              }
              let use_manual_row = allow_overflow_fallback && manual_row_positions;
              if use_manual_row {
                let cap_base = if rect.width().is_finite() && rect.width() > wrap_eps {
                  rect.width()
                } else {
                  self.viewport_size.width
                };
                let cap = cap_base.max(wrap_eps) * 2.0 + rect.origin.x;
                if fallback_cursor_x + resolved_width > cap {
                  fallback_cursor_x = rect.origin.x;
                  last_layout_x = None;
                }
              }
              if use_manual_row {
                origin_x = fallback_cursor_x;
                fallback_cursor_x += resolved_width;
              } else {
                fallback_cursor_x = child_loc_x + resolved_width;
                last_layout_x = Some(child_loc_x);
                last_layout_y = Some(child_loc_y);
              }
            } else {
              if let Some(prev) = last_layout_y {
                if child_loc_y <= prev + 0.1 {
                  manual_col_positions = true;
                }
              } else {
                fallback_cursor_y = child_loc_y;
              }
              if manual_col_positions {
                origin_y = fallback_cursor_y;
                fallback_cursor_y += resolved_height;
              } else {
                fallback_cursor_y = child_loc_y + resolved_height;
                last_layout_y = Some(child_loc_y);
              }
            }
            let log_child_ids = toggles
              .usize_list("FASTR_LOG_FLEX_CHILD_IDS")
              .unwrap_or_default();
            let log_child = !log_child_ids.is_empty()
              && (log_child_ids.contains(&child_box.id) || log_child_ids.contains(&box_node.id));
            if log_child {
              let selector = child_box
                .debug_info
                .as_ref()
                .map(|d| d.to_selector())
                .unwrap_or_else(|| "<anon>".to_string());
              eprintln!(
                            "[flex-child-place] parent_id={} child_id={} selector={} layout=({:.2},{:.2}) loc=({:.2},{:.2}) resolved=({:.2},{:.2}) rect_w={:.2} manual_row={} cursor_x={:.2} flex=({:.2},{:.2},{:?}) width={:?} min_w={:?} max_w={:?}",
                            box_node.id,
                            child_box.id,
                            selector,
                            layout_width,
                            layout_height,
                            child_loc_x,
                            child_loc_y,
                            resolved_width,
                            resolved_height,
                            rect.width(),
                            manual_row_positions,
                            fallback_cursor_x,
                            child_box.style.flex_grow,
                            child_box.style.flex_shrink,
                            child_box.style.flex_basis,
                            child_box.style.width,
                            child_box.style.min_width,
                            child_box.style.max_width
                        );
            }
            child_fragment.bounds = Rect::new(
              Point::new(origin_x, origin_y),
              Size::new(resolved_width, resolved_height),
            );
            final_fragment = Some(child_fragment);
          }
        }
      }

      if let Some(fragment) = final_fragment {
        if store_remembered_size {
          let percentage_base = if rect_w.is_finite() && rect_w > eps {
            rect_w
          } else {
            constraints
              .inline_percentage_base
              .or_else(|| constraints.width())
              .filter(|base| base.is_finite() && *base > eps)
              .unwrap_or(self.viewport_size.width)
          };
          let content_size =
            self.content_box_size(&fragment, child_box.style.as_ref(), percentage_base);
          remembered_size_cache_store(child_box, content_size);
        }
        let key = (child_box.style.order, dom_idx);
        if let Some(prev) = last_unordered_key {
          if key < prev {
            unordered_children_need_sort = true;
          }
        }
        last_unordered_key = Some(key);
        unordered_children.push((child_box.style.order, dom_idx, fragment));
      }

      #[cfg(test)]
      if node_map.get(&(child_box as *const BoxNode)).is_none() {
        eprintln!(
          "[flex-debug-missing-child] box_id={} child_ptr={:p}",
          box_node.id, child_box
        );
      }
    }

    if unordered_children_need_sort {
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      unordered_children.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
    }
    children = unordered_children
      .into_iter()
      .map(|(_, _, frag)| frag)
      .collect();
    #[cfg(test)]
    if children.is_empty() {
      let keys: Vec<usize> = node_map.keys().map(|k| *k as usize).collect();
      let child_ptrs: Vec<usize> = box_node
        .children
        .iter()
        .map(|c| c as *const _ as usize)
        .collect();
      eprintln!(
        "[flex-debug-empty-children] box_id={} keys={:?} child_ptrs={:?}",
        box_node.id, keys, child_ptrs
      );
    }

    // If Taffy reported non-increasing positions along the main axis, fall back to a simple
    // manual placement using the resolved fragment widths/heights to avoid overlapping items.
    static OVERFLOW_COUNTS: OnceLock<Mutex<std::collections::HashMap<usize, usize>>> =
      OnceLock::new();
    let log_overflow = toggles.truthy("FASTR_LOG_FLEX_OVERFLOW");
    let log_overflow_ids = toggles
      .usize_list("FASTR_LOG_FLEX_OVERFLOW_IDS")
      .unwrap_or_default();
    if main_axis_is_horizontal {
      // Re-run manual row placement when Taffy positions items beyond the container width.
      // Even when overflow is expected (wide items), their starting position should remain
      // at the row origin rather than drifting far to the right.
      let container_w = if rect.width().is_finite() && rect.width() > wrap_eps {
        rect.width()
      } else {
        self.viewport_size.width
      };
      let mut max_child_x = 0.0f32;
      for child in &children {
        check_layout_deadline(&mut deadline_counter)?;
        max_child_x = max_child_x.max(child.bounds.max_x());
      }
      let log_shrink_ids = toggles
        .usize_list("FASTR_LOG_FLEX_SHRINK_IDS")
        .unwrap_or_default();
      let log_this_shrink = !log_shrink_ids.is_empty() && log_shrink_ids.contains(&box_node.id);

      if allow_overflow_fallback && max_child_x > container_w + 0.5 {
        let available = container_w.max(1.0).min(self.viewport_size.width);
        // Apply flex-shrink distribution to bring the total width back to the available span.
        let mut total_main = 0.0;
        let mut total_weight = 0.0;
        let mut child_count = 0usize;
        let mut box_lookup: FxHashMap<usize, &BoxNode> =
          FxHashMap::with_capacity_and_hasher(box_node.children.len(), Default::default());
        for child in &box_node.children {
          check_layout_deadline(&mut deadline_counter)?;
          box_lookup.insert(child.id, child);
        }
        let mut frag_box_ids: Vec<(usize, usize)> = Vec::new();
        for (idx, frag) in children.iter().enumerate() {
          check_layout_deadline(&mut deadline_counter)?;
          if let Some(box_id) = match &frag.content {
            FragmentContent::Block { box_id }
            | FragmentContent::Inline { box_id, .. }
            | FragmentContent::Text { box_id, .. }
            | FragmentContent::Replaced { box_id, .. } => *box_id,
            FragmentContent::RunningAnchor { .. } => None,
            FragmentContent::Line { .. } => None,
          } {
            frag_box_ids.push((idx, box_id));
          }
        }
        for (frag_idx, box_id) in &frag_box_ids {
          check_layout_deadline(&mut deadline_counter)?;
          if let Some(child_node) = box_lookup.get(box_id) {
            let child_fragment = &children[*frag_idx];
            let base = child_fragment.bounds.width();
            let weight = child_node.style.flex_shrink.max(0.0) * base;
            total_main += base;
            total_weight += weight;
            child_count += 1;
          }
        }
        if log_this_shrink {
          let widths: Vec<f32> = children.iter().map(|c| c.bounds.width()).collect();
          eprintln!(
            "[flex-shrink-pre] id={} avail_w={:.1} total={:.1} widths={:?}",
            box_node.id, available, total_main, widths
          );
        }

        if total_main > available + 0.01 {
          let deficit = total_main - available;
          let count = child_count.max(1) as f32;
          let mut cursor_x = 0.0;
          let mut cursor_y = 0.0;
          let mut row_h = 0.0;
          for (frag_idx, box_id) in &frag_box_ids {
            if let Some(child_node) = box_lookup.get(box_id) {
              let child_fragment = &mut children[*frag_idx];
              let base = child_fragment.bounds.width();
              let weight = child_node.style.flex_shrink.max(0.0) * base;
              let share = if total_weight > 0.0 {
                deficit * (weight / total_weight)
              } else {
                deficit / count
              };
              let w = (base - share).max(0.0).min(available);
              let h = child_fragment.bounds.height();
              if cursor_x + w > available + 0.1
                && cursor_x > 0.0
                && !matches!(box_node.style.flex_wrap, FlexWrap::NoWrap)
              {
                cursor_x = 0.0;
                cursor_y += row_h;
                row_h = 0.0;
              }
              child_fragment.bounds = Rect::new(Point::new(cursor_x, cursor_y), Size::new(w, h));
              cursor_x += w;
              row_h = row_h.max(h);
            }
          }
        } else {
          // Even without shrink, ensure sequential placement within the line.
          let mut cursor_x = 0.0;
          let mut cursor_y = 0.0;
          let mut row_h = 0.0;
          for child in &mut children {
            let w = child.bounds.width().min(available);
            let h = child.bounds.height();
            if cursor_x + w > available + 0.1
              && cursor_x > 0.0
              && !matches!(box_node.style.flex_wrap, FlexWrap::NoWrap)
            {
              cursor_x = 0.0;
              cursor_y += row_h;
              row_h = 0.0;
            }
            child.bounds = Rect::new(Point::new(cursor_x, cursor_y), Size::new(w, h));
            cursor_x += w;
            row_h = row_h.max(h);
          }
        }

        if log_this_shrink {
          let widths: Vec<f32> = children.iter().map(|c| c.bounds.width()).collect();
          eprintln!(
            "[flex-shrink-post] id={} avail_w={:.1} total={:.1} widths={:?}",
            box_node.id,
            available,
            widths.iter().sum::<f32>(),
            widths
          );
        }
      }
      // Log overflow after any redistribution/reflow so diagnostics reflect final placement.
      max_child_x = children
        .iter()
        .map(|c| c.bounds.max_x())
        .fold(0.0, f32::max);
      // If all children have been pushed far to the left (beyond 2× the container width),
      // shift them back so the leftmost child starts at the origin. This guards against
      // runaway negative positions from broken intrinsic sizing or cached fragments.
      let min_child_x = children
        .iter()
        .map(|c| c.bounds.x())
        .fold(f32::INFINITY, f32::min);
      if min_child_x.is_finite() {
        let clamp_width = container_w.max(self.viewport_size.width).max(1.0) * 2.0;
        if min_child_x < -clamp_width {
          let dx = -min_child_x;
          for child in &mut children {
            child.bounds = Rect::new(
              Point::new(child.bounds.x() + dx, child.bounds.y()),
              child.bounds.size,
            );
          }
          max_child_x = children
            .iter()
            .map(|c| c.bounds.max_x())
            .fold(0.0, f32::max);
        }
      }
      let should_log =
        log_overflow || (!log_overflow_ids.is_empty() && log_overflow_ids.contains(&box_node.id));
      if should_log && max_child_x > container_w + 0.5 {
        let log_allowed = {
          let mut counts = OVERFLOW_COUNTS
            .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
            .lock()
            .ok();
          counts
            .as_mut()
            .map(|map| {
              let counter = map.entry(box_node.id).or_insert(0);
              let allowed = *counter < 2;
              *counter += 1;
              allowed
            })
            .unwrap_or(true)
        };
        if log_allowed {
          let selector = box_node
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          eprintln!(
                        "[flex-overflow-row] id={} selector={} container_w={:.1} max_child_x={:.1} child_count={}",
                        box_node.id,
                        selector,
                        container_w,
                        max_child_x,
                        children.len()
                    );
          for (idx, child) in children.iter().enumerate().take(12) {
            let frag_box_id = match &child.content {
              crate::tree::fragment_tree::FragmentContent::Block { box_id }
              | crate::tree::fragment_tree::FragmentContent::Inline { box_id, .. } => {
                box_id.clone()
              }
              crate::tree::fragment_tree::FragmentContent::Replaced { box_id, .. } => {
                box_id.clone()
              }
              _ => None,
            };
            let child_sel = box_node
              .children
              .get(idx)
              .and_then(|b| b.debug_info.as_ref().map(|d| d.to_selector()))
              .unwrap_or_else(|| "<anon>".to_string());
            eprintln!(
                            "  [flex-overflow-row-child] parent_id={} idx={} child_id={:?} sel={} x={:.1} w={:.1} max_x={:.1}",
                            box_node.id,
                            idx,
                            frag_box_id.or_else(|| box_node.children.get(idx).map(|b| b.id)),
                            child_sel,
                            child.bounds.x(),
                            child.bounds.width(),
                            child.bounds.max_x(),
                        );
          }
        }
      }
    } else {
      let container_h = rect.height();
      let max_child_y = children
        .iter()
        .map(|c| c.bounds.max_y())
        .fold(0.0, f32::max);
      let should_log =
        log_overflow || (!log_overflow_ids.is_empty() && log_overflow_ids.contains(&box_node.id));
      if should_log && max_child_y > container_h * 1.5 {
        let log_allowed = {
          let mut counts = OVERFLOW_COUNTS
            .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
            .lock()
            .ok();
          counts
            .as_mut()
            .map(|map| {
              let counter = map.entry(box_node.id).or_insert(0);
              let allowed = *counter < 2;
              *counter += 1;
              allowed
            })
            .unwrap_or(true)
        };
        if log_allowed {
          let selector = box_node
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          eprintln!(
                        "[flex-overflow-col] id={} selector={} container_h={:.1} max_child_y={:.1} child_count={}",
                        box_node.id,
                        selector,
                        container_h,
                        max_child_y,
                        children.len()
                    );
          for (idx, child) in children.iter().enumerate().take(12) {
            let frag_box_id = match &child.content {
              crate::tree::fragment_tree::FragmentContent::Block { box_id }
              | crate::tree::fragment_tree::FragmentContent::Inline { box_id, .. } => {
                box_id.clone()
              }
              crate::tree::fragment_tree::FragmentContent::Replaced { box_id, .. } => {
                box_id.clone()
              }
              _ => None,
            };
            let child_sel = box_node
              .children
              .get(idx)
              .and_then(|b| b.debug_info.as_ref().map(|d| d.to_selector()))
              .unwrap_or_else(|| "<anon>".to_string());
            eprintln!(
                            "  [flex-overflow-col-child] parent_id={} idx={} child_id={:?} sel={} y={:.1} h={:.1} max_y={:.1}",
                            box_node.id,
                            idx,
                            frag_box_id.or_else(|| box_node.children.get(idx).map(|b| b.id)),
                            child_sel,
                            child.bounds.y(),
                            child.bounds.height(),
                            child.bounds.max_y(),
                        );
          }
        }
      }
      // Guard against cross-axis drift when column flex containers produce children far
      // outside the container width. Clamp children back to the start edge to avoid
      // dropping content offscreen when Taffy returns oversized x positions.
      let container_w = rect.width();
      let max_child_x = children
        .iter()
        .map(|c| c.bounds.max_x())
        .fold(0.0, f32::max);
      if max_child_x > container_w + 0.5 {
        let available = container_w.max(1.0).min(self.viewport_size.width);
        for child in &mut children {
          let w = child.bounds.width().min(available);
          child.bounds = Rect::new(
            Point::new(0.0, child.bounds.y()),
            Size::new(w, child.bounds.height()),
          );
        }
      }
    }

    if !children.is_empty() {
      if main_axis_is_horizontal {
        let mut non_increasing = false;
        let mut last_x = children[0].bounds.x();
        for child in children.iter().skip(1) {
          let x = child.bounds.x();
          if x <= last_x + 0.1 {
            non_increasing = true;
            break;
          }
          last_x = x;
        }
        if non_increasing {
          let mut cursor = children[0].bounds.x();
          for child in &mut children {
            child.bounds = Rect::new(Point::new(cursor, child.bounds.y()), child.bounds.size);
            cursor += child.bounds.width();
          }
        }
      } else {
        let mut non_increasing = false;
        let mut last_y = children[0].bounds.y();
        for child in children.iter().skip(1) {
          let y = child.bounds.y();
          if y <= last_y + 0.1 {
            non_increasing = true;
            break;
          }
          last_y = y;
        }
        if non_increasing {
          let mut cursor = children[0].bounds.y();
          for child in &mut children {
            child.bounds = Rect::new(Point::new(child.bounds.x(), cursor), child.bounds.size);
            cursor += child.bounds.height();
          }
        }
      }
    }

    // If a wrapping row still overflows the container (e.g., items sized to 100% that Taffy
    // keeps on the same line), reflow the children into sequential rows within the container
    // width. This mirrors flex-wrap behaviour when items exceed the line length.
    if main_axis_is_horizontal
      && !matches!(box_node.style.flex_wrap, FlexWrap::NoWrap)
      && rect.width().is_finite()
      && rect.width() > 0.0
    {
      let max_child_x = children
        .iter()
        .map(|c| c.bounds.max_x())
        .fold(f32::NEG_INFINITY, f32::max);
      let min_child_x = children
        .iter()
        .map(|c| c.bounds.x())
        .fold(f32::INFINITY, f32::min);
      if max_child_x > rect.width() + 0.5 || min_child_x < -0.5 {
        let avail = rect.width();
        let start_y = children.iter().map(|c| c.bounds.y()).fold(0.0, f32::min);
        let mut cursor_x = 0.0;
        let mut cursor_y = start_y;
        let mut row_height: f32 = 0.0;
        for child in &mut children {
          let mut w = child.bounds.width().min(avail);
          let h = child.bounds.height();
          if cursor_x + w > avail + 0.01 {
            cursor_y += row_height;
            cursor_x = 0.0;
            row_height = 0.0;
          }
          if w <= 0.0 {
            w = 0.0;
          }
          child.bounds = Rect::new(Point::new(cursor_x, cursor_y), Size::new(w, h));
          cursor_x += w;
          row_height = row_height.max(h);
        }
        let new_height = (cursor_y + row_height - rect.origin.y).max(rect.height());
        rect = Rect::new(rect.origin, Size::new(rect.width(), new_height));
      }
    }

    // Taffy occasionally returns row layouts where every child starts well past the
    // container's end (e.g., due to upstream constraint collapse). Flex rows, even
    // with nowrap, should still start at the line's start edge; the overflow should
    // come from the total span, not an initial offset. If all children are entirely
    // to the right of the container, translate them back so the first child starts
    // at the container origin.
    if main_axis_is_horizontal
      && matches!(box_node.style.flex_wrap, FlexWrap::NoWrap)
      && !children.is_empty()
    {
      let min_child_x = children
        .iter()
        .map(|c| c.bounds.x())
        .fold(f32::INFINITY, f32::min);
      if min_child_x.is_finite() && min_child_x > rect.width() {
        for child in &mut children {
          child.bounds = Rect::new(
            Point::new(child.bounds.x() - min_child_x, child.bounds.y()),
            child.bounds.size,
          );
        }
      }
    }

    // Guard against pathological horizontal drift: if every child in a row sits far beyond
    // the container (multiple widths away), translate them back so the leftmost child starts
    // at the origin while preserving relative spacing.
    if main_axis_is_horizontal && !children.is_empty() {
      let min_x = children
        .iter()
        .map(|c| c.bounds.x())
        .fold(f32::INFINITY, f32::min);
      let max_x = children
        .iter()
        .map(|c| c.bounds.max_x())
        .fold(f32::NEG_INFINITY, f32::max);
      let drift_limit = rect.width().max(1.0) * 4.0;
      if min_x.is_finite() && min_x > drift_limit && max_x.is_finite() {
        let dx = min_x;
        let log_drift = toggles.truthy("FASTR_LOG_FLEX_DRIFT");
        if log_drift {
          let selector = box_node
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          eprintln!(
                        "[flex-drift-clamp] id={} selector={} min_x={:.1} max_x={:.1} rect_w={:.1} dx={:.1} children={}",
                        box_node.id,
                        selector,
                        min_x,
                        max_x,
                        rect.width(),
                        dx,
                        children.len()
                    );
        }
        for child in &mut children {
          child.bounds = Rect::new(
            Point::new(child.bounds.x() - dx, child.bounds.y()),
            child.bounds.size,
          );
        }
      } else if min_x.is_finite()
        && max_x.is_finite()
        && rect.width().is_finite()
        && min_x > rect.width() * 0.5
        && (max_x - min_x) <= rect.width() * 1.5
      {
        // Children sit noticeably to the right but still span roughly the container width.
        // Re-center them within the container to avoid an empty viewport.
        let span = max_x - min_x;
        let target_min = ((rect.width() - span).max(0.0)) / 2.0;
        let dx = min_x - target_min;
        for child in &mut children {
          child.bounds = Rect::new(
            Point::new(child.bounds.x() - dx, child.bounds.y()),
            child.bounds.size,
          );
        }
      }
    }

    // Final clamp: avoid shifting children out of view when Taffy returns coordinates that do not
    // overlap the container at all. Do not clamp sizes—overflow is handled at paint-time.
    if rect.width() > 0.0 && rect.height() > 0.0 && !children.is_empty() {
      let eps = 0.5;
      let mut min_x = f32::INFINITY;
      let mut max_x = f32::NEG_INFINITY;
      let mut min_y = f32::INFINITY;
      let mut max_y = f32::NEG_INFINITY;
      for child in &children {
        let x = child.bounds.x();
        let y = child.bounds.y();
        let mx = child.bounds.max_x();
        let my = child.bounds.max_y();
        if x.is_finite() {
          min_x = min_x.min(x);
        }
        if y.is_finite() {
          min_y = min_y.min(y);
        }
        if mx.is_finite() {
          max_x = max_x.max(mx);
        }
        if my.is_finite() {
          max_y = max_y.max(my);
        }
      }

      let container_min_x = rect.origin.x;
      let container_max_x = rect.origin.x + rect.width();
      let container_min_y = rect.origin.y;
      let container_max_y = rect.origin.y + rect.height();
      let mut dx = 0.0f32;
      let mut dy = 0.0f32;

      if min_x.is_finite() && max_x.is_finite() {
        let outside_x = min_x > container_max_x + eps || max_x < container_min_x - eps;
        if outside_x {
          dx = container_min_x - min_x;
        }
      }
      if min_y.is_finite() && max_y.is_finite() {
        let outside_y = min_y > container_max_y + eps || max_y < container_min_y - eps;
        if outside_y {
          dy = container_min_y - min_y;
        }
      }

      if (dx != 0.0 || dy != 0.0) && dx.is_finite() && dy.is_finite() {
        for child in &mut children {
          child.bounds = Rect::new(
            Point::new(child.bounds.x() + dx, child.bounds.y() + dy),
            child.bounds.size,
          );
        }
      }
    }

    Ok(FragmentNode::new_with_style(
      rect,
      crate::tree::fragment_tree::FragmentContent::Block {
        box_id: Some(box_node.id),
      },
      children,
      box_node.style.clone(),
    ))
  }

  /// Converts layout constraints to Taffy available space
  fn constraints_to_available_space(
    &self,
    constraints: &LayoutConstraints,
  ) -> taffy::geometry::Size<AvailableSpace> {
    taffy::geometry::Size {
      width: match constraints.available_width {
        CrateAvailableSpace::Definite(w) => AvailableSpace::Definite(w),
        CrateAvailableSpace::MinContent => AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => AvailableSpace::MaxContent,
        CrateAvailableSpace::Indefinite => AvailableSpace::MaxContent,
      },
      height: match constraints.available_height {
        CrateAvailableSpace::Definite(h) => AvailableSpace::Definite(h),
        CrateAvailableSpace::MinContent => AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => AvailableSpace::MaxContent,
        CrateAvailableSpace::Indefinite => AvailableSpace::MaxContent,
      },
    }
  }

  /// Converts Taffy's available space and known dimensions into this crate's constraints.
  fn constraints_from_taffy(
    &self,
    mut known: taffy::geometry::Size<Option<f32>>,
    available: taffy::geometry::Size<AvailableSpace>,
    inline_percentage_base: Option<f32>,
  ) -> LayoutConstraints {
    // Taffy uses tiny definite probes (0px/1px) to represent "unknown" constraints during
    // intrinsic sizing. Treat these as indefinite so flex measurements can't be accidentally
    // forced to 0px and then cached/reused.
    if let Some(w) = known.width {
      if w <= 1.0 && matches!(available.width, AvailableSpace::Definite(v) if v <= 1.0) {
        known.width = None;
      }
    }
    if let Some(h) = known.height {
      if h <= 1.0 && matches!(available.height, AvailableSpace::Definite(v) if v <= 1.0) {
        known.height = None;
      }
    }

    let clamp_def_width = |w: f32| w.min(self.viewport_size.width);
    let width = match (known.width, available.width) {
      (Some(w), _) => CrateAvailableSpace::Definite(clamp_def_width(w)),
      (_, AvailableSpace::Definite(w)) => {
        if w <= 1.0 {
          CrateAvailableSpace::Indefinite
        } else {
          CrateAvailableSpace::Definite(clamp_def_width(w))
        }
      }
      (_, AvailableSpace::MinContent) => CrateAvailableSpace::MinContent,
      (_, AvailableSpace::MaxContent) => CrateAvailableSpace::MaxContent,
    };
    let height = match (known.height, available.height) {
      (Some(h), _) => CrateAvailableSpace::Definite(h),
      (_, AvailableSpace::Definite(h)) => {
        if h <= 1.0 {
          CrateAvailableSpace::Indefinite
        } else {
          CrateAvailableSpace::Definite(h)
        }
      }
      (_, AvailableSpace::MinContent) => CrateAvailableSpace::MinContent,
      (_, AvailableSpace::MaxContent) => CrateAvailableSpace::MaxContent,
    };

    let mut constraints = LayoutConstraints::new(width, height);
    constraints.inline_percentage_base = constraints
      .inline_percentage_base
      .or(inline_percentage_base)
      .or(match available.width {
        AvailableSpace::Definite(w) if w > 1.0 => Some(w),
        _ => None,
      });
    constraints
  }

  fn fragment_subtree_size(
    fragment: &FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<Size, LayoutError> {
    fn walk(
      node: &FragmentNode,
      offset: Point,
      min: &mut Point,
      max: &mut Point,
      deadline_counter: &mut usize,
    ) -> Result<(), LayoutError> {
      check_layout_deadline(deadline_counter)?;
      let origin = Point::new(node.bounds.x() + offset.x, node.bounds.y() + offset.y);
      let bounds = Rect::new(origin, node.bounds.size);
      min.x = min.x.min(bounds.x());
      min.y = min.y.min(bounds.y());
      max.x = max.x.max(bounds.max_x());
      max.y = max.y.max(bounds.max_y());
      for child in node.children.iter() {
        walk(child, origin, min, max, deadline_counter)?;
      }
      Ok(())
    }
    let mut min = Point::new(0.0, 0.0);
    let mut max = Point::new(0.0, 0.0);
    walk(fragment, Point::ZERO, &mut min, &mut max, deadline_counter)?;
    Ok(Size::new(
      (max.x - min.x).max(0.0),
      (max.y - min.y).max(0.0),
    ))
  }

  /// Returns the tight bounds of all descendants, excluding the root node’s own bounds.
  fn fragment_descendant_span(
    fragment: &FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<Option<Size>, LayoutError> {
    fn walk(
      node: &FragmentNode,
      offset: Point,
      min: &mut Point,
      max: &mut Point,
      found: &mut bool,
      deadline_counter: &mut usize,
    ) -> Result<(), LayoutError> {
      check_layout_deadline(deadline_counter)?;
      for child in node.children.iter() {
        let origin = Point::new(child.bounds.x() + offset.x, child.bounds.y() + offset.y);
        let bounds = Rect::new(origin, child.bounds.size);
        *found = true;
        min.x = min.x.min(bounds.x());
        min.y = min.y.min(bounds.y());
        max.x = max.x.max(bounds.max_x());
        max.y = max.y.max(bounds.max_y());
        walk(child, origin, min, max, found, deadline_counter)?;
      }
      Ok(())
    }
    let mut min = Point::new(f32::INFINITY, f32::INFINITY);
    let mut max = Point::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut found = false;
    walk(
      fragment,
      Point::ZERO,
      &mut min,
      &mut max,
      &mut found,
      deadline_counter,
    )?;
    let span = if !found {
      None
    } else {
      Some(Size::new(
        (max.x - min.x).max(0.0),
        (max.y - min.y).max(0.0),
      ))
    };
    Ok(span)
  }

  /// Returns the content-box size for a laid-out fragment, stripping padding and borders.
  fn content_box_size(
    &self,
    fragment: &FragmentNode,
    style: &ComputedStyle,
    percentage_base: f32,
  ) -> Size {
    let padding_left = self.resolve_length_for_width(style.padding_left, percentage_base, style);
    let padding_right = self.resolve_length_for_width(style.padding_right, percentage_base, style);
    let padding_top = self.resolve_length_for_width(style.padding_top, percentage_base, style);
    let padding_bottom =
      self.resolve_length_for_width(style.padding_bottom, percentage_base, style);

    let border_left =
      self.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
    let border_right =
      self.resolve_length_for_width(style.used_border_right_width(), percentage_base, style);
    let border_top =
      self.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
    let border_bottom =
      self.resolve_length_for_width(style.used_border_bottom_width(), percentage_base, style);

    let content_width =
      (fragment.bounds.width() - padding_left - padding_right - border_left - border_right)
        .max(0.0);
    let content_height =
      (fragment.bounds.height() - padding_top - padding_bottom - border_top - border_bottom)
        .max(0.0);

    Size::new(content_width, content_height)
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

  fn flex_item_should_skip_contents(
    &self,
    node: &BoxNode,
    auto_unskipped_for_pass: &FxHashSet<*const BoxNode>,
  ) -> bool {
    match node.style.content_visibility {
      crate::style::types::ContentVisibility::Hidden => true,
      crate::style::types::ContentVisibility::Auto => {
        self.content_visibility_auto_has_definite_placeholder(node)
          && !auto_unskipped_for_pass.contains(&(node as *const BoxNode))
      }
      crate::style::types::ContentVisibility::Visible => false,
    }
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

  fn compute_static_positions_for_abs_children(
    &self,
    box_node: &BoxNode,
    fragment: &FragmentNode,
    _in_flow_children: &[&BoxNode],
    positioned: &[PositionedCandidate],
    auto_unskipped: Option<&FxHashSet<*const BoxNode>>,
    padding_origin: Point,
  ) -> Result<FxHashMap<usize, Point>, LayoutError> {
    let mut deadline_counter = 0usize;
    if positioned.is_empty() {
      return Ok(FxHashMap::default());
    }

    let auto_unskipped_empty: FxHashSet<*const BoxNode> = FxHashSet::default();
    let auto_unskipped_for_pass = auto_unskipped.unwrap_or(&auto_unskipped_empty);
    let mut positions: FxHashMap<usize, Point> =
      FxHashMap::with_capacity_and_hasher(positioned.len(), Default::default());

    // CSS Flexbox § "Absolutely-Positioned Flex Children":
    // The main-axis static position must be calculated as if the abspos child were the *sole*
    // flex item. In particular it must not depend on siblings (in-flow or abspos), and abspos
    // children must not respond to `order`.
    //
    // We approximate this by running a tiny flex layout per abspos child, containing only that
    // child with its used border-box size fixed to the pre-laid-out fragment size.
    //
    // The spec defines the main-axis edges in terms of the child's *margin edges*. Our absolute
    // positioning implementation expects the static position to be at the hypothetical in-flow
    // margin edge and then applies the resolved margin as part of the constraint equation, so we
    // convert the Taffy output (border-box origin) back into a margin-edge point. Additionally,
    // for this purpose the child's `margin: auto` is treated as zero, so ensure auto margins do
    // not absorb free space (which would otherwise override justify-content/align-self).
    let container_width = fragment.bounds.width().max(0.0);
    let container_height = fragment.bounds.height().max(0.0);

    let mut root_style = self.computed_style_to_taffy(box_node, true, None, auto_unskipped_for_pass)?;
    root_style.size.width = Dimension::length(container_width);
    root_style.size.height = Dimension::length(container_height);

    let available_space = taffy::geometry::Size {
      width: AvailableSpace::Definite(container_width),
      height: AvailableSpace::Definite(container_height),
    };

    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);

    for candidate in positioned {
      check_layout_deadline(&mut deadline_counter)?;

      let mut taffy = crate::layout::taffy_integration::PooledTaffyTree::new();
      let mut style = self.computed_style_to_taffy(
        &candidate.layout_child,
        false,
        Some(&box_node.style),
        auto_unskipped_for_pass,
      )?;
      style.size.width = Dimension::length(candidate.fragment.bounds.width().max(0.0));
      style.size.height = Dimension::length(candidate.fragment.bounds.height().max(0.0));
      let zero_auto_margin = |value: &mut LengthPercentageAuto| {
        if value.is_auto() {
          *value = LengthPercentageAuto::length(0.0);
        }
      };
      zero_auto_margin(&mut style.margin.left);
      zero_auto_margin(&mut style.margin.right);
      zero_auto_margin(&mut style.margin.top);
      zero_auto_margin(&mut style.margin.bottom);
      let node = taffy.new_leaf(style).map_err(|e| {
        LayoutError::MissingContext(format!("Failed to create Taffy leaf: {:?}", e))
      })?;

      let root = taffy
        .new_with_children(root_style.clone(), &[node])
        .map_err(|e| LayoutError::MissingContext(format!("Failed to create Taffy root: {:?}", e)))?;

      taffy
        .compute_layout_with_measure_and_cancel(
          root,
          available_space,
          |_, _, _, _, _| taffy::geometry::Size::ZERO,
          cancel.clone(),
          TAFFY_ABORT_CHECK_STRIDE,
        )
        .map_err(|e| match e {
          taffy::TaffyError::LayoutAborted => match active_deadline() {
            Some(deadline) => LayoutError::Timeout {
              elapsed: deadline.elapsed(),
            },
            None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
          },
          _ => LayoutError::MissingContext(format!("Taffy layout failed: {:?}", e)),
        })?;

      if let Ok(layout) = taffy.layout(node) {
        positions.insert(
          candidate.child_id,
          Point::new(
            layout.location.x - layout.margin.left - padding_origin.x,
            layout.location.y - layout.margin.top - padding_origin.y,
          ),
        );
      }
    }

    Ok(positions)
  }

  // ==========================================================================
  // Type conversion helpers
  // ==========================================================================

  fn display_to_taffy(&self, style: &ComputedStyle, is_root: bool) -> taffy::style::Display {
    // Root container is always Flex (that's why we're using FlexFormattingContext)
    // Children use their actual display mode, defaulting to Block for flex items
    if is_root {
      taffy::style::Display::Flex
    } else {
      // For children within a flex container, check if they're nested flex/grid
      match style.display {
        Display::Flex | Display::InlineFlex => taffy::style::Display::Flex,
        Display::Grid | Display::InlineGrid => taffy::style::Display::Grid,
        Display::None => taffy::style::Display::None,
        // Regular items become flex items with block-level sizing
        _ => taffy::style::Display::Block,
      }
    }
  }

  fn flex_direction_to_taffy(
    &self,
    style: &ComputedStyle,
    inline_forward_positive: bool,
    block_forward_positive: bool,
  ) -> taffy::style::FlexDirection {
    let inline_is_horizontal = matches!(style.writing_mode, WritingMode::HorizontalTb);
    let block_is_horizontal = !inline_is_horizontal;

    match style.flex_direction {
      FlexDirection::Row => {
        if inline_is_horizontal {
          if inline_forward_positive {
            taffy::style::FlexDirection::Row
          } else {
            taffy::style::FlexDirection::RowReverse
          }
        } else if inline_forward_positive {
          taffy::style::FlexDirection::Column
        } else {
          taffy::style::FlexDirection::ColumnReverse
        }
      }
      FlexDirection::RowReverse => {
        if inline_is_horizontal {
          if inline_forward_positive {
            taffy::style::FlexDirection::RowReverse
          } else {
            taffy::style::FlexDirection::Row
          }
        } else if inline_forward_positive {
          taffy::style::FlexDirection::ColumnReverse
        } else {
          taffy::style::FlexDirection::Column
        }
      }
      FlexDirection::Column => {
        if block_is_horizontal {
          if block_forward_positive {
            taffy::style::FlexDirection::Row
          } else {
            taffy::style::FlexDirection::RowReverse
          }
        } else if block_forward_positive {
          taffy::style::FlexDirection::Column
        } else {
          taffy::style::FlexDirection::ColumnReverse
        }
      }
      FlexDirection::ColumnReverse => {
        if block_is_horizontal {
          if block_forward_positive {
            taffy::style::FlexDirection::RowReverse
          } else {
            taffy::style::FlexDirection::Row
          }
        } else if block_forward_positive {
          taffy::style::FlexDirection::ColumnReverse
        } else {
          taffy::style::FlexDirection::Column
        }
      }
    }
  }

  fn flex_wrap_to_taffy(&self, wrap: FlexWrap) -> taffy::style::FlexWrap {
    match wrap {
      FlexWrap::NoWrap => taffy::style::FlexWrap::NoWrap,
      FlexWrap::Wrap => taffy::style::FlexWrap::Wrap,
      // Taffy's wrap-reverse behavior is incomplete for multi-line containers (line stacking and
      // `align-content`). We emulate wrap-reverse by running Taffy with normal wrapping and then
      // mirroring item positions along the cross axis.
      FlexWrap::WrapReverse => taffy::style::FlexWrap::Wrap,
    }
  }

  fn inline_axis_positive(&self, style: &ComputedStyle) -> bool {
    match style.writing_mode {
      WritingMode::HorizontalTb => style.direction != Direction::Rtl,
      WritingMode::VerticalRl
      | WritingMode::VerticalLr
      | WritingMode::SidewaysRl
      | WritingMode::SidewaysLr => true,
    }
  }

  fn block_axis_positive(&self, style: &ComputedStyle) -> bool {
    match style.writing_mode {
      WritingMode::VerticalRl | WritingMode::SidewaysRl => false,
      _ => true,
    }
  }

  fn justify_content_to_taffy(
    &self,
    justify: JustifyContent,
  ) -> Option<taffy::style::JustifyContent> {
    Some(match justify {
      JustifyContent::FlexStart => taffy::style::JustifyContent::FlexStart,
      JustifyContent::FlexEnd => taffy::style::JustifyContent::FlexEnd,
      JustifyContent::Center => taffy::style::JustifyContent::Center,
      JustifyContent::SpaceBetween => taffy::style::JustifyContent::SpaceBetween,
      JustifyContent::SpaceAround => taffy::style::JustifyContent::SpaceAround,
      JustifyContent::SpaceEvenly => taffy::style::JustifyContent::SpaceEvenly,
    })
  }

  fn align_items_to_taffy(
    &self,
    align: AlignItems,
    axis_positive: bool,
  ) -> Option<taffy::style::AlignItems> {
    Some(match align {
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
    })
  }

  fn align_self_to_taffy(
    &self,
    align: Option<AlignItems>,
    axis_positive: bool,
  ) -> Option<taffy::style::AlignItems> {
    align.and_then(|a| self.align_items_to_taffy(a, axis_positive))
  }

  fn align_content_to_taffy(
    &self,
    align: AlignContent,
    axis_positive: bool,
  ) -> Option<taffy::style::AlignContent> {
    Some(match align {
      AlignContent::FlexStart => {
        if axis_positive {
          taffy::style::AlignContent::FlexStart
        } else {
          taffy::style::AlignContent::FlexEnd
        }
      }
      AlignContent::FlexEnd => {
        if axis_positive {
          taffy::style::AlignContent::FlexEnd
        } else {
          taffy::style::AlignContent::FlexStart
        }
      }
      AlignContent::Center => taffy::style::AlignContent::Center,
      AlignContent::SpaceBetween => taffy::style::AlignContent::SpaceBetween,
      AlignContent::SpaceEvenly => taffy::style::AlignContent::SpaceEvenly,
      AlignContent::SpaceAround => taffy::style::AlignContent::SpaceAround,
      AlignContent::Stretch => taffy::style::AlignContent::Stretch,
    })
  }

  fn flex_basis_to_taffy(&self, basis: &FlexBasis, style: &ComputedStyle) -> Dimension {
    match basis {
      FlexBasis::Auto | FlexBasis::Content => Dimension::auto(),
      FlexBasis::Length(len) => self.length_to_dimension(len, style),
    }
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

  fn resolve_length_px(&self, len: &Length, style: &ComputedStyle) -> Option<f32> {
    match len.unit {
      LengthUnit::Percent => None,
      _ if len.unit.is_absolute() => Some(len.to_px()),
      u if u.is_viewport_relative() => {
        len.resolve_with_viewport(self.viewport_size.width, self.viewport_size.height)
      }
      LengthUnit::Rem => Some(len.value * style.root_font_size),
      LengthUnit::Em => Some(len.value * style.font_size),
      LengthUnit::Ex => Some(len.value * style.font_size * 0.5),
      LengthUnit::Ch => Some(len.value * style.font_size * 0.5),
      _ => None,
    }
  }

  fn dimension_for_box_sizing(&self, len: &Length, style: &ComputedStyle, axis: Axis) -> Dimension {
    if style.box_sizing == BoxSizing::ContentBox {
      if let Some(edges) = match axis {
        Axis::Horizontal => self.horizontal_edges_px(style),
        Axis::Vertical => self.vertical_edges_px(style),
      } {
        if let Some(px) = self.resolve_length_px(len, style) {
          return Dimension::length((px + edges).max(0.0));
        }
      }
    }
    self.length_to_dimension(len, style)
  }

  fn length_to_dimension(&self, len: &Length, style: &ComputedStyle) -> Dimension {
    match len.unit {
      LengthUnit::Px => Dimension::length(len.to_px()),
      LengthUnit::Percent => Dimension::percent(len.value / 100.0),
      _ => {
        if let Some(px) = self.resolve_length_px(len, style) {
          Dimension::length(px)
        } else {
          Dimension::length(len.to_px())
        }
      }
    }
  }

  fn length_option_to_dimension_box_sizing(
    &self,
    len: Option<&Length>,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Dimension {
    match len {
      Some(l) => self.dimension_for_box_sizing(l, style, axis),
      None => Dimension::auto(),
    }
  }

  #[allow(dead_code)]
  fn length_option_to_dimension(&self, len: Option<&Length>, style: &ComputedStyle) -> Dimension {
    match len {
      Some(l) => self.length_to_dimension(l, style),
      None => Dimension::auto(),
    }
  }

  fn length_to_taffy_lp(&self, len: &Length, style: &ComputedStyle) -> LengthPercentage {
    match len.unit {
      LengthUnit::Percent => LengthPercentage::percent(len.value / 100.0),
      _ => {
        if let Some(px) = self.resolve_length_px(len, style) {
          LengthPercentage::length(px)
        } else {
          LengthPercentage::length(len.to_px())
        }
      }
    }
  }

  fn length_option_to_lpa(
    &self,
    len: Option<&Length>,
    style: &ComputedStyle,
  ) -> LengthPercentageAuto {
    match len {
      Some(l) => match l.unit {
        LengthUnit::Percent => LengthPercentageAuto::percent(l.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(l, style) {
            LengthPercentageAuto::length(px)
          } else {
            LengthPercentageAuto::length(l.to_px())
          }
        }
      },
      None => LengthPercentageAuto::auto(),
    }
  }

  fn aspect_ratio_to_taffy(&self, aspect_ratio: AspectRatio) -> Option<f32> {
    match aspect_ratio {
      AspectRatio::Auto => None,
      AspectRatio::Ratio(ratio) | AspectRatio::AutoRatio(ratio) => Some(ratio),
    }
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
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{DiagnosticsLevel, FastRender, FastRenderConfig, RenderOptions};
  use crate::style::display::Display;
  use crate::style::display::FormattingContextType;
  use crate::style::position::Position;
  use crate::style::types::AlignItems;
  use crate::style::types::AspectRatio;
  use crate::style::types::BorderStyle;
  use crate::style::types::ContainIntrinsicSizeAxis;
  use crate::style::types::ContentVisibility;
  use crate::style::types::FlexWrap;
  use crate::style::types::Overflow;
  use crate::style::types::ScrollbarWidth;
  use crate::style::values::Length;
  use crate::text::font_db::FontConfig;
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::debug::{track_to_selector_calls, DebugInfo};
  use std::sync::Arc;

  fn create_flex_style() -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Flex;
    style.flex_direction = FlexDirection::Row;
    Arc::new(style)
  }

  fn create_item_style(width: f32, height: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.width = Some(Length::px(width));
    style.height = Some(Length::px(height));
    style.width_keyword = None;
    style.height_keyword = None;
    Arc::new(style)
  }

  fn create_item_style_with_grow(width: f32, height: f32, grow: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.width = Some(Length::px(width));
    style.height = Some(Length::px(height));
    style.width_keyword = None;
    style.height_keyword = None;
    style.flex_grow = grow;
    Arc::new(style)
  }

  fn baseline_position(fragment: &FragmentNode) -> f32 {
    let mut deadline_counter = 0usize;
    let offset = fragment_first_baseline(fragment, &mut deadline_counter)
      .expect("baseline computation")
      .expect("fragment has no baseline");
    fragment.bounds.y() + offset
  }

  fn find_block_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
    fragment
      .children
      .iter()
      .find(|child| match &child.content {
        FragmentContent::Block {
          box_id: Some(child_id),
        } => *child_id == box_id,
        _ => false,
      })
      .unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
  }

  fn content_visibility_test_guard() -> crate::debug::runtime::ThreadRuntimeTogglesGuard {
    use crate::debug::runtime;
    use std::collections::HashMap;

    runtime::set_thread_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(HashMap::from(
      [(
        "FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX".to_string(),
        "0".to_string(),
      )],
    ))))
  }

  #[test]
  fn content_visibility_hidden_flex_item_skips_measure_layout() {
    reset_flex_measure_layout_calls();

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let visible_id = 11usize;
    let hidden_id = 22usize;

    let visible_text = BoxNode::new_text(text_style.clone(), "Visible".to_string());
    let visible_inline = BoxNode::new_inline(text_style.clone(), vec![visible_text]);
    let mut visible_item = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![visible_inline],
    );
    visible_item.id = visible_id;

    let contain_intrinsic_height = 40.0;
    let padding_top = 5.0;
    let padding_bottom = 7.0;
    let border_top = 2.0;
    let border_bottom = 3.0;

    let mut hidden_style = ComputedStyle::default();
    hidden_style.content_visibility = ContentVisibility::Hidden;
    hidden_style.contain_intrinsic_height.length = Some(Length::px(contain_intrinsic_height));
    hidden_style.padding_top = Length::px(padding_top);
    hidden_style.padding_bottom = Length::px(padding_bottom);
    hidden_style.border_top_width = Length::px(border_top);
    hidden_style.border_bottom_width = Length::px(border_bottom);
    hidden_style.border_top_style = BorderStyle::Solid;
    hidden_style.border_bottom_style = BorderStyle::Solid;

    let hidden_text = BoxNode::new_text(text_style.clone(), "Hidden".to_string());
    let hidden_inline = BoxNode::new_inline(text_style.clone(), vec![hidden_text]);
    let mut hidden_item = BoxNode::new_block(
      Arc::new(hidden_style),
      FormattingContextType::Block,
      vec![hidden_inline],
    );
    hidden_item.id = hidden_id;

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![visible_item, hidden_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);
    let fragment = fc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert!(
      flex_measure_layout_calls_for(visible_id) > 0,
      "expected visible flex item to trigger the expensive measure layout path"
    );
    assert_eq!(
      flex_measure_layout_calls_for(hidden_id),
      0,
      "content-visibility:hidden flex item must not be laid out during flex measurement"
    );
    assert_eq!(
      flex_measure_layout_total_calls(),
      flex_measure_layout_calls_for(visible_id),
      "only the visible flex item should reach the expensive measure layout path"
    );

    let hidden_fragment = find_block_child(&fragment, hidden_id);
    assert!(
      hidden_fragment.children.is_empty(),
      "content-visibility:hidden flex item must not generate descendant fragments"
    );
    let expected_height =
      contain_intrinsic_height + padding_top + padding_bottom + border_top + border_bottom;
    assert!(
      (hidden_fragment.bounds.height() - expected_height).abs() < 0.5,
      "expected hidden fragment height {:.1}, got {:.1}",
      expected_height,
      hidden_fragment.bounds.height()
    );
  }

  #[test]
  fn flex_auto_min_size_skips_intrinsic_probes_for_content_visibility_hidden() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let next_epoch = crate::layout::formatting_context::intrinsic_cache_epoch() + 1;
    crate::layout::formatting_context::intrinsic_cache_use_epoch(next_epoch, true);

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Flex;
    child_style.flex_direction = FlexDirection::Column;
    child_style.content_visibility = ContentVisibility::Hidden;
    child_style.contain_intrinsic_width.length = Some(Length::px(10.0));
    let mut child =
      BoxNode::new_block(Arc::new(child_style), FormattingContextType::Flex, vec![]);
    child.id = 10;

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    fc.layout(&container, &constraints).expect("layout");

    assert_eq!(
      crate::layout::formatting_context::intrinsic_cache_stats().4,
      0,
      "content-visibility:hidden flex item must not trigger flex intrinsic probes during auto min-size"
    );
  }

  #[test]
  fn flex_constraints_from_taffy_treats_tiny_known_sizes_as_indefinite() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let constraints = fc.constraints_from_taffy(
      taffy::geometry::Size {
        width: Some(0.0),
        height: Some(0.0),
      },
      taffy::geometry::Size {
        width: AvailableSpace::Definite(0.0),
        height: AvailableSpace::Definite(0.0),
      },
      None,
    );

    assert_eq!(constraints.available_width, CrateAvailableSpace::Indefinite);
    assert_eq!(
      constraints.available_height,
      CrateAvailableSpace::Indefinite
    );
    assert!(constraints.inline_percentage_base.is_none());
  }

  #[test]
  fn content_visibility_auto_flex_item_offscreen_skips_measure_layout() {
    let _toggles = content_visibility_test_guard();
    reset_flex_measure_layout_calls();

    let fc = FlexFormattingContext::with_viewport(Size::new(400.0, 200.0))
      .with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Column;

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let auto_visible_id = 101usize;
    let spacer_id = 102usize;
    let auto_offscreen_id = 103usize;

    let mut spacer_style = ComputedStyle::default();
    spacer_style.height = Some(Length::px(500.0));
    spacer_style.order = 1;
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = spacer_id;

    let mut auto_visible_style = ComputedStyle::default();
    auto_visible_style.content_visibility = ContentVisibility::Auto;
    auto_visible_style.contain_intrinsic_height.length = Some(Length::px(20.0));
    auto_visible_style.order = 0;
    let auto_visible_text = BoxNode::new_text(text_style.clone(), "Onscreen auto".to_string());
    let auto_visible_inline = BoxNode::new_inline(text_style.clone(), vec![auto_visible_text]);
    let mut auto_visible_item = BoxNode::new_block(
      Arc::new(auto_visible_style),
      FormattingContextType::Block,
      vec![auto_visible_inline],
    );
    auto_visible_item.id = auto_visible_id;

    let mut auto_offscreen_style = ComputedStyle::default();
    auto_offscreen_style.content_visibility = ContentVisibility::Auto;
    auto_offscreen_style.contain_intrinsic_height.length = Some(Length::px(60.0));
    auto_offscreen_style.order = 2;
    let auto_offscreen_text = BoxNode::new_text(text_style.clone(), "Offscreen auto".to_string());
    let auto_offscreen_inline = BoxNode::new_inline(text_style.clone(), vec![auto_offscreen_text]);
    let mut auto_offscreen_item = BoxNode::new_block(
      Arc::new(auto_offscreen_style),
      FormattingContextType::Block,
      vec![auto_offscreen_inline],
    );
    auto_offscreen_item.id = auto_offscreen_id;

    // Spacer is the first DOM child, but `order` brings the onscreen auto item to the top.
    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![spacer, auto_visible_item, auto_offscreen_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    let fragment = fc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert!(
      flex_measure_layout_calls_for(auto_visible_id) > 0,
      "expected in-viewport content-visibility:auto item to be laid out after multi-pass unskip"
    );
    assert_eq!(
      flex_measure_layout_calls_for(auto_offscreen_id),
      0,
      "expected offscreen content-visibility:auto item to remain skipped and never enter the expensive measure layout path"
    );

    let onscreen_fragment = find_block_child(&fragment, auto_visible_id);
    assert!(
      !onscreen_fragment.children.is_empty(),
      "expected onscreen auto item to generate descendant fragments"
    );

    let offscreen_fragment = find_block_child(&fragment, auto_offscreen_id);
    assert!(
      offscreen_fragment.children.is_empty(),
      "expected offscreen auto item to be represented by a placeholder fragment"
    );
    assert!(
      offscreen_fragment.bounds.y() > fc.viewport_size.height,
      "expected offscreen auto item y={} to be beyond viewport height={}",
      offscreen_fragment.bounds.y(),
      fc.viewport_size.height
    );
  }

  #[test]
  fn flex_auto_min_size_runs_intrinsic_probes_after_content_visibility_auto_unskip() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let next_epoch = crate::layout::formatting_context::intrinsic_cache_epoch() + 1;
    crate::layout::formatting_context::intrinsic_cache_use_epoch(next_epoch, true);
    let _toggles = content_visibility_test_guard();

    let fc = FlexFormattingContext::with_viewport(Size::new(400.0, 200.0))
      .with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Column;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Flex;
    auto_style.flex_direction = FlexDirection::Column;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a definite inline size so Taffy doesn't need to perform intrinsic inline probes
    // during flex base-size resolution (those would also increment FLEX_INTRINSIC_CALLS, masking
    // regressions in the auto-min-size path we're testing here).
    auto_style.width = Some(Length::px(100.0));
    // Ensure the item participates in the multi-pass unskip logic.
    auto_style.contain_intrinsic_height.length = Some(Length::px(20.0));
    let mut auto_item =
      BoxNode::new_block(Arc::new(auto_style), FormattingContextType::Flex, vec![]);
    auto_item.id = 20;

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![auto_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    fc.layout(&container, &constraints).expect("layout");

    assert!(
      crate::layout::formatting_context::intrinsic_cache_stats().4 > 0,
      "expected unskipped content-visibility:auto item to compute flex auto-min-size via intrinsic probes"
    );
  }

  #[test]
  fn flex_auto_min_size_skips_intrinsic_probes_for_offscreen_content_visibility_auto() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let next_epoch = crate::layout::formatting_context::intrinsic_cache_epoch() + 1;
    crate::layout::formatting_context::intrinsic_cache_use_epoch(next_epoch, true);
    let _toggles = content_visibility_test_guard();

    let fc = FlexFormattingContext::with_viewport(Size::new(400.0, 200.0))
      .with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;

    let mut spacer_style = ComputedStyle::default();
    spacer_style.width = Some(Length::px(500.0));
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = 30;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Flex;
    auto_style.flex_direction = FlexDirection::Column;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.contain_intrinsic_height.length = Some(Length::px(20.0));
    let mut auto_item =
      BoxNode::new_block(Arc::new(auto_style), FormattingContextType::Flex, vec![]);
    auto_item.id = 31;

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![spacer, auto_item],
    );
    let constraints = LayoutConstraints::definite(1000.0, 200.0);
    fc.layout(&container, &constraints).expect("layout");

    assert_eq!(
      crate::layout::formatting_context::intrinsic_cache_stats().4,
      0,
      "offscreen content-visibility:auto item must not trigger flex intrinsic probes during auto min-size"
    );
  }

  #[test]
  fn content_visibility_auto_nested_flex_in_block_accounts_for_parent_offset() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite(viewport.width, viewport.height);

    let mut spacer_style = ComputedStyle::default();
    spacer_style.display = Display::Block;
    spacer_style.height = Some(Length::px(300.0));
    spacer_style.height_keyword = None;
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = 1;

    let mut leaf_style = ComputedStyle::default();
    leaf_style.display = Display::Block;
    leaf_style.height = Some(Length::px(10.0));
    leaf_style.height_keyword = None;
    let mut leaf = BoxNode::new_block(Arc::new(leaf_style), FormattingContextType::Block, vec![]);
    leaf.id = 4;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_item = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_item.id = 3;

    let mut flex_style = ComputedStyle::default();
    flex_style.display = Display::Flex;
    flex_style.flex_direction = FlexDirection::Column;
    let mut flex_container = BoxNode::new_block(
      Arc::new(flex_style),
      FormattingContextType::Flex,
      vec![auto_item],
    );
    flex_container.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![spacer, flex_container],
    );
    root.id = 0;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let flex_fragment = find_block_child(&fragment, 2);
    let auto_fragment = find_block_child(flex_fragment, 3);
    assert!(
      auto_fragment.children.is_empty(),
      "expected nested flex content-visibility:auto item to remain skipped when the flex container is offscreen"
    );
  }

  #[test]
  fn contain_intrinsic_size_auto_uses_remembered_size_when_skipped_in_flex() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let _toggles = content_visibility_test_guard();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Column;
    let container_style = Arc::new(container_style);

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let spacer_id = 201usize;
    let auto_id = 202usize;
    let after_id = 203usize;

    let mut spacer_style = ComputedStyle::default();
    spacer_style.height = Some(Length::px(300.0));
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

    let auto_text = BoxNode::new_text(text_style.clone(), "Remember me".to_string());
    let auto_inline = BoxNode::new_inline(text_style.clone(), vec![auto_text]);
    let mut auto_item = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![auto_inline],
    );
    auto_item.id = auto_id;

    let mut after_style = ComputedStyle::default();
    after_style.height = Some(Length::px(50.0));
    let mut after_item =
      BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]);
    after_item.id = after_id;

    let container = BoxNode::new_block(
      container_style,
      FormattingContextType::Flex,
      vec![spacer, auto_item, after_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 1000.0);

    reset_flex_measure_layout_calls();
    let fc_large = FlexFormattingContext::with_viewport(Size::new(400.0, 800.0))
      .with_parallelism(LayoutParallelism::disabled());
    let fragment_large = fc_large
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert!(
      flex_measure_layout_calls_for(auto_id) > 0,
      "expected first pass to lay out the content-visibility:auto item to establish a remembered size"
    );

    let auto_fragment_large = find_block_child(&fragment_large, auto_id);
    assert!(
      !auto_fragment_large.children.is_empty(),
      "expected first pass to fully lay out the auto item"
    );
    let remembered_border_box_height = auto_fragment_large.bounds.height();
    let after_y_large = find_block_child(&fragment_large, after_id).bounds.y();

    reset_flex_measure_layout_calls();
    let fc_small = FlexFormattingContext::with_viewport(Size::new(400.0, 100.0))
      .with_parallelism(LayoutParallelism::disabled());
    let fragment_small = fc_small
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(
      flex_measure_layout_calls_for(auto_id),
      0,
      "expected second pass to skip the offscreen content-visibility:auto item"
    );

    let auto_fragment_small = find_block_child(&fragment_small, auto_id);
    assert!(
      auto_fragment_small.children.is_empty(),
      "expected second pass auto item fragment to be a placeholder"
    );
    assert!(
      (auto_fragment_small.bounds.height() - remembered_border_box_height).abs() < 0.5,
      "expected placeholder height {:.1}, got {:.1}",
      remembered_border_box_height,
      auto_fragment_small.bounds.height()
    );

    let after_y_small = find_block_child(&fragment_small, after_id).bounds.y();
    assert!(
      (after_y_small - after_y_large).abs() < 0.5,
      "expected following flex item y to remain stable (first={after_y_large:.1}, second={after_y_small:.1})"
    );
  }

  #[test]
  fn content_visibility_auto_nested_flex_in_flex_accounts_for_child_offset() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    // Give the flex container more block-axis space than the viewport so the spacer can push the
    // nested container offscreen without being flex-shrunk to fit the viewport height.
    let constraints = LayoutConstraints::definite(viewport.width, 1000.0);

    let fc = FlexFormattingContext::with_viewport(viewport)
      .with_parallelism(LayoutParallelism::disabled());

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Flex;
    root_style.flex_direction = FlexDirection::Column;

    let mut spacer_style = ComputedStyle::default();
    spacer_style.display = Display::Block;
    spacer_style.height = Some(Length::px(300.0));
    spacer_style.height_keyword = None;
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = 11;

    let mut leaf_style = ComputedStyle::default();
    leaf_style.display = Display::Block;
    leaf_style.height = Some(Length::px(10.0));
    leaf_style.height_keyword = None;
    let mut leaf = BoxNode::new_block(Arc::new(leaf_style), FormattingContextType::Block, vec![]);
    leaf.id = 33;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_item = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_item.id = 22;

    let mut nested_style = ComputedStyle::default();
    nested_style.display = Display::Flex;
    nested_style.flex_direction = FlexDirection::Column;
    let mut nested_container = BoxNode::new_block(
      Arc::new(nested_style),
      FormattingContextType::Flex,
      vec![auto_item],
    );
    nested_container.id = 12;

    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Flex,
      vec![spacer, nested_container],
    );
    root.id = 10;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let nested_fragment = find_block_child(&fragment, 12);
    let auto_fragment = find_block_child(nested_fragment, 22);
    assert!(
      auto_fragment.children.is_empty(),
      "expected nested flex content-visibility:auto item to remain skipped when its flex container is offscreen within the parent"
    );
  }

  #[test]
  fn content_visibility_auto_respects_vertical_writing_mode_in_flex_placeholder_gate() {
    let _toggles = content_visibility_test_guard();
    reset_flex_measure_layout_calls();

    let fc = FlexFormattingContext::with_viewport(Size::new(400.0, 100.0))
      .with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Column;
    let container_style = Arc::new(container_style);

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let spacer_id = 401usize;
    let auto_id = 402usize;

    let mut spacer_style = ComputedStyle::default();
    spacer_style.height = Some(Length::px(300.0));
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = spacer_id;

    let mut auto_style = ComputedStyle::default();
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.writing_mode = WritingMode::VerticalRl;
    auto_style.contain_intrinsic_width.length = Some(Length::px(40.0));

    let auto_text = BoxNode::new_text(text_style.clone(), "Offscreen".to_string());
    let auto_inline = BoxNode::new_inline(text_style.clone(), vec![auto_text]);
    let mut auto_item = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![auto_inline],
    );
    auto_item.id = auto_id;

    let container = BoxNode::new_block(
      container_style,
      FormattingContextType::Flex,
      vec![spacer, auto_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    let fragment = fc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(
      flex_measure_layout_calls_for(auto_id),
      0,
      "expected offscreen vertical-writing-mode content-visibility:auto item to skip layout when it has a definite contain-intrinsic-width"
    );

    let auto_fragment = find_block_child(&fragment, auto_id);
    assert!(
      auto_fragment.children.is_empty(),
      "expected offscreen auto fragment to be a placeholder"
    );
    assert!(
      auto_fragment.bounds.y() > fc.viewport_size.height,
      "expected offscreen auto item y={} to be beyond viewport height={}",
      auto_fragment.bounds.y(),
      fc.viewport_size.height
    );
  }

  #[test]
  fn flex_item_order_does_not_trigger_manual_main_axis_placement() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Column;

    let nav_id = 10usize;
    let banner_id = 11usize;

    let mut nav_style = ComputedStyle::default();
    nav_style.width = Some(Length::px(200.0));
    nav_style.height = Some(Length::px(175.0));
    nav_style.width_keyword = None;
    nav_style.height_keyword = None;
    nav_style.order = 0;
    let mut nav = BoxNode::new_block(Arc::new(nav_style), FormattingContextType::Block, vec![]);
    nav.id = nav_id;

    let mut banner_style = ComputedStyle::default();
    banner_style.width = Some(Length::px(200.0));
    banner_style.height = Some(Length::px(56.0));
    banner_style.width_keyword = None;
    banner_style.height_keyword = None;
    banner_style.order = -1;
    let mut banner =
      BoxNode::new_block(Arc::new(banner_style), FormattingContextType::Block, vec![]);
    banner.id = banner_id;

    // DOM order is nav then banner, but `order:-1` should place the banner at the top.
    let mut container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![nav, banner],
    );
    container.id = 1;

    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    let fragment = fc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    let banner_fragment = find_block_child(&fragment, banner_id);
    let nav_fragment = find_block_child(&fragment, nav_id);

    assert!(
      banner_fragment.bounds.y().abs() < 0.5,
      "expected banner y≈0, got {}",
      banner_fragment.bounds.y()
    );
    assert!(
      (nav_fragment.bounds.y() - 56.0).abs() < 0.5,
      "expected nav y≈56, got {}",
      nav_fragment.bounds.y()
    );
  }

  #[test]
  fn flex_style_fingerprint_canonicalizes_negative_zero() {
    let mut style_zero = ComputedStyle::default();
    style_zero.display = Display::Flex;
    style_zero.margin_left = Some(Length::px(0.0));
    let mut style_neg_zero = style_zero.clone();
    style_neg_zero.margin_left = Some(Length::px(-0.0));

    assert_eq!(
      flex_style_fingerprint(&style_zero),
      flex_style_fingerprint(&style_neg_zero)
    );
  }

  #[test]
  fn flex_style_fingerprint_includes_intrinsic_size_keywords() {
    let mut base = ComputedStyle::default();
    base.display = Display::Flex;

    let mut max_content = base.clone();
    max_content.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    assert_ne!(
      flex_style_fingerprint(&base),
      flex_style_fingerprint(&max_content)
    );

    let mut fit_content = base.clone();
    fit_content.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let mut fit_content_fn = base.clone();
    fit_content_fn.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::percent(50.0)),
    });
    assert_ne!(
      flex_style_fingerprint(&fit_content),
      flex_style_fingerprint(&fit_content_fn)
    );

    let mut max_width_none = base.clone();
    max_width_none.max_width = None;
    max_width_none.max_width_keyword = None;
    let mut max_width_max_content = base.clone();
    max_width_max_content.max_width = None;
    max_width_max_content.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    assert_ne!(
      flex_style_fingerprint(&max_width_none),
      flex_style_fingerprint(&max_width_max_content)
    );
  }

  #[test]
  fn flex_style_fingerprint_accounts_for_content_visibility() {
    let mut style_visible = ComputedStyle::default();
    style_visible.display = Display::Flex;
    style_visible.content_visibility = ContentVisibility::Visible;

    let mut style_hidden = style_visible.clone();
    style_hidden.content_visibility = ContentVisibility::Hidden;

    let mut style_auto = style_visible.clone();
    style_auto.content_visibility = ContentVisibility::Auto;

    let fp_visible = flex_style_fingerprint(&style_visible);
    let fp_hidden = flex_style_fingerprint(&style_hidden);
    let fp_auto = flex_style_fingerprint(&style_auto);

    assert_ne!(
      fp_visible, fp_hidden,
      "content-visibility should affect flex style fingerprint"
    );
    assert_ne!(
      fp_visible, fp_auto,
      "content-visibility should affect flex style fingerprint"
    );
    assert_ne!(
      fp_hidden, fp_auto,
      "content-visibility should affect flex style fingerprint"
    );
  }

  #[test]
  fn flex_style_fingerprint_includes_calc_lengths() {
    let mut base = ComputedStyle::default();
    base.display = Display::Flex;

    let percent = CalcLength::single(LengthUnit::Percent, 10.0);
    let calc_a = CalcLength::single(LengthUnit::Px, 10.0)
      .add_scaled(&percent, 1.0)
      .expect("calc terms");
    let calc_b = CalcLength::single(LengthUnit::Px, 20.0)
      .add_scaled(&percent, 1.0)
      .expect("calc terms");

    let mut style_a = base.clone();
    style_a.width = Some(Length::calc(calc_a));
    let mut style_b = base.clone();
    style_b.width = Some(Length::calc(calc_b));
    assert_ne!(
      flex_style_fingerprint(&style_a),
      flex_style_fingerprint(&style_b)
    );

    let mut fit_content_a = base.clone();
    fit_content_a.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::calc(calc_a)),
    });
    let mut fit_content_b = base;
    fit_content_b.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::calc(calc_b)),
    });
    assert_ne!(
      flex_style_fingerprint(&fit_content_a),
      flex_style_fingerprint(&fit_content_b)
    );
  }

  #[test]
  fn flex_style_fingerprint_accounts_for_contain_intrinsic_size() {
    let mut style_a = ComputedStyle::default();
    style_a.display = Display::Flex;
    style_a.contain_intrinsic_height = ContainIntrinsicSizeAxis {
      auto: true,
      length: Some(Length::px(10.0)),
    };

    let mut style_b = style_a.clone();
    style_b.contain_intrinsic_height.length = Some(Length::px(20.0));

    let fp_a = flex_style_fingerprint(&style_a);
    let fp_b = flex_style_fingerprint(&style_b);

    assert_ne!(
      fp_a, fp_b,
      "contain-intrinsic-height should affect flex style fingerprint"
    );
  }

  #[test]
  fn flex_layout_cache_key_canonicalizes_negative_zero() {
    let viewport = Size::new(800.0, 600.0);
    let constraints_zero = LayoutConstraints::new(
      CrateAvailableSpace::Definite(0.0),
      CrateAvailableSpace::Definite(100.0),
    );
    let mut constraints_neg_zero = constraints_zero;
    constraints_neg_zero.available_width = CrateAvailableSpace::Definite(-0.0);

    assert_eq!(
      layout_cache_key(&constraints_zero, viewport),
      layout_cache_key(&constraints_neg_zero, viewport)
    );
  }

  #[test]
  fn flex_layout_cache_is_order_independent_for_previously_quantized_constraints() {
    // Regresses: `layout_cache_key` used to quantize definite widths to 2px buckets. Layout
    // fragments were cached under that key, but computed using the original (unquantized)
    // constraints.
    //
    // Two distinct widths that rounded into the same bucket (e.g. 99px and 100px) could therefore
    // share a cache entry, and whichever layout happened to run first would determine the reused
    // fragment, making output depend on evaluation order.

    fn bounds_signature(fragment: &FragmentNode) -> Vec<(u32, u32, u32, u32)> {
      fn walk(node: &FragmentNode, out: &mut Vec<(u32, u32, u32, u32)>) {
        out.push((
          f32_to_canonical_bits(node.bounds.x()),
          f32_to_canonical_bits(node.bounds.y()),
          f32_to_canonical_bits(node.bounds.width()),
          f32_to_canonical_bits(node.bounds.height()),
        ));
        for child in node.children.iter() {
          walk(child, out);
        }
      }

      let mut out = Vec::new();
      walk(fragment, &mut out);
      out
    }

    fn layout_with_order_capture_width(
      container: &BoxNode,
      first: f32,
      second: f32,
      capture: f32,
    ) -> FragmentNode {
      let viewport = Size::new(200.0, 200.0);
      let measured_fragments = Arc::new(ShardedFlexCache::new_measure());
      let layout_fragments = Arc::new(ShardedFlexCache::new_layout());
      let fc = FlexFormattingContext::with_viewport_and_cb(
        viewport,
        ContainingBlock::viewport(viewport),
        FontContext::new(),
        measured_fragments,
        layout_fragments,
      )
      .with_parallelism(LayoutParallelism::disabled());

      // Indefinite height keeps the test focused on width-dependent wrapping, and matches common
      // block layout usage (where height is content-based).
      let c1 = LayoutConstraints::new(
        CrateAvailableSpace::Definite(first),
        CrateAvailableSpace::Indefinite,
      );
      let c2 = LayoutConstraints::new(
        CrateAvailableSpace::Definite(second),
        CrateAvailableSpace::Indefinite,
      );

      let first_fragment = fc.layout(container, &c1).expect("first layout");
      let second_fragment = fc.layout(container, &c2).expect("second layout");
      if first == capture {
        first_fragment
      } else {
        second_fragment
      }
    }

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(10.0));

    let mut item1 = BoxNode::new_block(
      Arc::new(item_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    item1.id = 2;
    let mut item2 = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    item2.id = 3;

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.flex_wrap = FlexWrap::Wrap;
    let mut container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item1, item2],
    );
    container.id = 1;

    let w1 = 99.0;
    let w2 = 100.0;

    // Capture the fragment produced for the *w2* layout after running the two constraints in
    // different orders.
    let w2_after_w1 = layout_with_order_capture_width(&container, w1, w2, w2);
    let w2_before_w1 = layout_with_order_capture_width(&container, w2, w1, w2);

    assert_eq!(
      w2_before_w1.children.len(),
      2,
      "expected two flex item fragments"
    );
    assert_eq!(
      w2_before_w1.children[1].bounds.y(),
      0.0,
      "at 100px wide the second item should remain on the first flex line"
    );

    assert_eq!(
      bounds_signature(&w2_after_w1),
      bounds_signature(&w2_before_w1)
    );
  }

  #[test]
  fn flex_layout_cache_is_pure_memoization_for_definite_widths() {
    // Regression test: the flex layout fragment cache must behave like pure memoization. When the
    // cache key is lossy (quantized) and/or the cache reuses "near" fragments, the fragment
    // returned for a given set of constraints can depend on which similar layout ran first.
    //
    // Use two widths that used to collide in the quantized key (5000/5020 -> 4992) and are also
    // within the old `find_fragment` tolerance band.
    let viewport = Size::new(10_000.0, 1_000.0);
    let measured_fragments = Arc::new(ShardedFlexCache::new_measure());
    let layout_fragments = Arc::new(ShardedFlexCache::new_layout());
    let fc = FlexFormattingContext::with_viewport_and_cb(
      viewport,
      ContainingBlock::viewport(viewport),
      FontContext::new(),
      measured_fragments.clone(),
      layout_fragments.clone(),
    )
    .with_parallelism(LayoutParallelism::disabled());

    let mut container =
      BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, vec![]);
    container.id = 1;

    let constraints_a = LayoutConstraints::definite(5000.0, 0.0);
    let constraints_b = LayoutConstraints::definite(5020.0, 0.0);

    // Layout A then B.
    let fragment_a = fc.layout(&container, &constraints_a).expect("layout A");
    assert_eq!(fragment_a.bounds.width(), 5000.0);
    let fragment_b = fc.layout(&container, &constraints_b).expect("layout B");
    assert_eq!(fragment_b.bounds.width(), 5020.0);

    // Reset caches and repeat in the opposite order.
    layout_fragments.clear();
    measured_fragments.clear();

    let fragment_b = fc
      .layout(&container, &constraints_b)
      .expect("layout B-first");
    assert_eq!(fragment_b.bounds.width(), 5020.0);
    let fragment_a = fc
      .layout(&container, &constraints_a)
      .expect("layout A-second");
    assert_eq!(fragment_a.bounds.width(), 5000.0);
  }

  #[test]
  fn flex_tree_build_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let children: Vec<BoxNode> = (0..64)
      .map(|_| {
        BoxNode::new_block(
          create_item_style(10.0, 10.0),
          FormattingContextType::Block,
          vec![],
        )
      })
      .collect();
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let fc = FlexFormattingContext::new();
    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    let result = fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    );

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn flex_tree_build_times_out_in_later_loops_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    // Use fewer children than FLEX_DEADLINE_CHECK_STRIDE so the initial fingerprint loop alone would
    // not trigger a periodic deadline check. The build should still time out because subsequent
    // loops (style conversion + Taffy node construction) also perform deadline checks.
    let child_count = FLEX_DEADLINE_CHECK_STRIDE / 3 + 1;
    let children: Vec<BoxNode> = (0..child_count)
      .map(|_| {
        BoxNode::new_block(
          create_item_style(10.0, 10.0),
          FormattingContextType::Block,
          vec![],
        )
      })
      .collect();
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let fc = FlexFormattingContext::new();
    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    let result = fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    );

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn flex_running_child_scan_times_out_before_cache_hit() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    let children: Vec<BoxNode> = (0..FLEX_DEADLINE_CHECK_STRIDE)
      .map(|_| {
        BoxNode::new_block(
          create_item_style(10.0, 10.0),
          FormattingContextType::Block,
          vec![],
        )
      })
      .collect();
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let fc = FlexFormattingContext::new();
    fc.layout(&container, &constraints)
      .expect("populate flex cache");

    let key = layout_cache_key(&constraints, fc.viewport_size).expect("cache key");
    let node_key = flex_cache_key(&container);
    assert!(
      fc.layout_fragments.get(node_key, &key).is_some(),
      "expected second layout to be able to hit the flex layout cache",
    );

    // Let the initial check_active() pass, then time out on the first periodic deadline check in
    // the running-children scan loop. If that loop fails to check deadlines, the layout cache hit
    // would return early and mask the timeout.
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

    let result = fc.layout(&container, &constraints);
    match result {
      Err(LayoutError::Timeout { elapsed }) => assert!(elapsed >= Duration::from_secs(0)),
      other => panic!("expected LayoutError::Timeout from running scan, got {other:?}"),
    }
  }

  #[test]
  fn flex_respects_used_border_box_size_overrides() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::InlineFlex;
    container_style.flex_direction = FlexDirection::Row;

    let children = vec![BoxNode::new_block(
      create_item_style(10.0, 10.0),
      FormattingContextType::Block,
      vec![],
    )];
    let container = BoxNode::new_inline_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      children,
    );

    let constraints =
      LayoutConstraints::definite(500.0, 100.0).with_used_border_box_size(Some(500.0), Some(100.0));
    let fragment = fc
      .layout(&container, &constraints)
      .expect("inline-flex layout should succeed");

    assert_eq!(fragment.bounds.width(), 500.0);
    assert_eq!(fragment.bounds.height(), 100.0);
  }

  #[test]
  fn flex_respects_style_override_for_root() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut base_style = ComputedStyle::default();
    base_style.display = Display::InlineFlex;
    base_style.width = Some(Length::px(50.0));
    base_style.width_keyword = None;

    let mut container = BoxNode::new_inline_block(
      Arc::new(base_style.clone()),
      FormattingContextType::Flex,
      vec![],
    );
    container.id = 1;

    let constraints = LayoutConstraints::new(
      CrateAvailableSpace::MaxContent,
      CrateAvailableSpace::Indefinite,
    );
    let fragment = fc
      .layout(&container, &constraints)
      .expect("inline-flex layout should succeed");
    assert_eq!(fragment.bounds.width(), 50.0);

    let mut override_style = base_style;
    override_style.width = Some(Length::px(100.0));
    override_style.width_keyword = None;
    let fragment = crate::layout::style_override::with_style_override(
      container.id,
      Arc::new(override_style),
      || fc.layout(&container, &constraints),
    )
    .expect("layout with style override should succeed");

    assert_eq!(fragment.bounds.width(), 100.0);
  }

  #[test]
  fn flex_order_sort_is_skipped_when_children_are_already_sorted() {
    reset_flex_order_sort_calls();

    let mut style_a = ComputedStyle::default();
    style_a.width = Some(Length::px(10.0));
    style_a.height = Some(Length::px(10.0));
    style_a.width_keyword = None;
    style_a.height_keyword = None;
    style_a.order = 0;

    let mut style_b = ComputedStyle::default();
    style_b.width = Some(Length::px(10.0));
    style_b.height = Some(Length::px(10.0));
    style_b.width_keyword = None;
    style_b.height_keyword = None;
    style_b.order = 1;

    let children = vec![
      BoxNode::new_block(Arc::new(style_a), FormattingContextType::Block, vec![]),
      BoxNode::new_block(Arc::new(style_b), FormattingContextType::Block, vec![]),
    ];
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    fc.layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(
      flex_order_sort_calls(),
      0,
      "expected already-sorted flex items to bypass the order sort",
    );
  }

  #[test]
  fn flex_order_sort_runs_when_children_are_out_of_order() {
    reset_flex_order_sort_calls();

    let mut style_a = ComputedStyle::default();
    style_a.width = Some(Length::px(10.0));
    style_a.height = Some(Length::px(10.0));
    style_a.width_keyword = None;
    style_a.height_keyword = None;
    style_a.order = 1;

    let mut style_b = ComputedStyle::default();
    style_b.width = Some(Length::px(10.0));
    style_b.height = Some(Length::px(10.0));
    style_b.width_keyword = None;
    style_b.height_keyword = None;
    style_b.order = 0;

    let children = vec![
      BoxNode::new_block(Arc::new(style_a), FormattingContextType::Block, vec![]),
      BoxNode::new_block(Arc::new(style_b), FormattingContextType::Block, vec![]),
    ];
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    fc.layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(
      flex_order_sort_calls(),
      1,
      "expected out-of-order flex items to trigger exactly one order sort",
    );
  }

  #[test]
  fn flex_order_sort_checks_deadline_before_sorting() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::atomic::{AtomicUsize, Ordering};

    reset_flex_order_sort_calls();

    let mut style_a = ComputedStyle::default();
    style_a.width = Some(Length::px(10.0));
    style_a.height = Some(Length::px(10.0));
    style_a.width_keyword = None;
    style_a.height_keyword = None;
    style_a.order = 1;

    let mut style_b = ComputedStyle::default();
    style_b.width = Some(Length::px(10.0));
    style_b.height = Some(Length::px(10.0));
    style_b.width_keyword = None;
    style_b.height_keyword = None;
    style_b.order = 0;

    let children = vec![
      BoxNode::new_block(Arc::new(style_a), FormattingContextType::Block, vec![]),
      BoxNode::new_block(Arc::new(style_b), FormattingContextType::Block, vec![]),
    ];
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    // Allow the initial check_active() at the start of flex layout to pass, then abort on the next
    // check_active() that runs before sorting.
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

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let result = fc.layout(&container, &constraints);
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
    assert_eq!(
      flex_order_sort_calls(),
      0,
      "expected deadline to abort flex layout before performing the order sort",
    );
  }

  #[test]
  fn flex_intrinsic_inline_size_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let children: Vec<BoxNode> = (0..64)
      .map(|_| {
        BoxNode::new_block(
          create_item_style(10.0, 10.0),
          FormattingContextType::Block,
          vec![],
        )
      })
      .collect();
    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let result = fc.compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent);

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn flex_baseline_traversal_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let leaf = FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), "x", 0.5);
    let mut fragment = leaf;
    // Ensure the baseline walk hits FLEX_DEADLINE_CHECK_STRIDE checks before reaching the leaf.
    for _ in 0..(FLEX_DEADLINE_CHECK_STRIDE + 16) {
      fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![fragment]);
    }

    let mut deadline_counter = 0usize;
    let result = fragment_first_baseline(&fragment, &mut deadline_counter);
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn flex_fragment_subtree_size_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let leaf = FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), "x", 0.5);
    let mut fragment = leaf;
    for _ in 0..(FLEX_DEADLINE_CHECK_STRIDE + 16) {
      fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![fragment]);
    }

    let mut deadline_counter = 0usize;
    let result = FlexFormattingContext::fragment_subtree_size(&fragment, &mut deadline_counter);
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn flex_taffy_abort_surfaces_as_timeout() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Ensure the deadline is not tripped by the first pre-layout check, but is tripped during Taffy.
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

    // Build a minimal flex container so layout reaches the Taffy compute call.
    let child = BoxNode::new_block(
      create_item_style(10.0, 10.0),
      FormattingContextType::Block,
      vec![],
    );
    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let fc = FlexFormattingContext::new();
    let result = fc.layout(&container, &constraints);

    match result {
      Err(LayoutError::Timeout { elapsed }) => {
        // Should have some elapsed time recorded by RenderDeadline, even if tiny.
        assert!(elapsed >= Duration::from_secs(0));
      }
      other => panic!("expected LayoutError::Timeout from Taffy abort, got {other:?}"),
    }
  }

  #[test]
  fn flex_auto_min_height_uses_item_formatting_context() {
    let mut flex_style = ComputedStyle::default();
    flex_style.display = Display::Flex;
    flex_style.flex_direction = FlexDirection::Column;

    let mut item_style = ComputedStyle::default();
    item_style.height = Some(Length::px(10.0));
    item_style.height_keyword = None;
    item_style.overflow_y = Overflow::Visible;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let container = BoxNode::new_block(
      Arc::new(flex_style),
      FormattingContextType::Flex,
      vec![child],
    );

    let fc = FlexFormattingContext::new();
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    fc.layout(&container, &constraints)
      .expect("flex layout should succeed without calling flex layout on the block item");
  }

  #[test]
  fn flex_auto_min_width_ignores_item_width_for_flex_items_with_flex_fc() {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Flex;
    item_style.flex_direction = FlexDirection::Row;
    item_style.width = Some(Length::px(100.0));
    item_style.width_keyword = None;
    item_style.overflow_x = Overflow::Visible;

    let mut item = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Flex,
      vec![BoxNode::new_block(
        create_item_style(10.0, 10.0),
        FormattingContextType::Block,
        vec![],
      )],
    );
    item.id = 2;
    item.children[0].id = 3;
    let mut container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item],
    );
    container.id = 1;

    let fc = FlexFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    )
    .expect("build taffy tree");

    let item_ptr: *const BoxNode = &container.children[0];
    let child_node = node_map
      .get(&item_ptr)
      .copied()
      .expect("item should exist in node_map");
    let style = taffy_tree
      .style(child_node)
      .expect("taffy item style should be available");
    let min_width = style.min_size.width;
    assert!(
      !min_width.is_auto(),
      "expected flex auto min-size to resolve to a min-content length"
    );
    assert_eq!(
      min_width.tag(),
      Dimension::length(0.0).tag(),
      "expected flex auto min-size to resolve to a length, got {min_width:?}"
    );
    let value = min_width.value();
    assert!(
      value > 0.0,
      "expected auto min-width for nested flex items to be non-zero"
    );
    assert!(
      value < 100.0,
      "expected auto min-width ({value}) to ignore item width=100px"
    );
  }

  #[test]
  fn taffy_style_maps_overflow_and_scrollbar_width() {
    let mut style = ComputedStyle::default();
    style.display = Display::Flex;
    style.overflow_x = Overflow::Scroll;
    style.overflow_y = Overflow::Hidden;
    style.scrollbar_width = ScrollbarWidth::Thin;

    let node = BoxNode::new_block(Arc::new(style), FormattingContextType::Flex, vec![]);
    let fc = FlexFormattingContext::new();
    let auto_unskipped_empty: FxHashSet<*const BoxNode> = FxHashSet::default();
    let taffy_style = fc
      .computed_style_to_taffy(&node, true, None, &auto_unskipped_empty)
      .expect("taffy style");

    assert_eq!(
      taffy_style.scrollbar_width,
      resolve_scrollbar_width(&node.style)
    );
    assert_eq!(taffy_style.overflow.x, TaffyOverflow::Scroll);
    assert_eq!(taffy_style.overflow.y, TaffyOverflow::Hidden);
  }

  #[test]
  fn flex_auto_min_size_timeout_propagates() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);
    let item_children = (0..32usize)
      .map(|idx| BoxNode::new_text(text_style.clone(), format!("x{idx}")))
      .collect::<Vec<_>>();

    let mut item_style = ComputedStyle::default();
    item_style.overflow_x = Overflow::Visible;
    let child = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      item_children,
    );

    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fc = FlexFormattingContext::new();

    // First build populates the cached Taffy template.
    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    )
    .expect("initial flex template build should succeed");

    // Trigger cancellation during the flex-item auto-min-size intrinsic probe. If template caching
    // skips that probe on cache hits, the build would incorrectly succeed.
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let deadline = RenderDeadline::new(
      None,
      Some(Arc::new(move || {
        let prev = counter_clone.fetch_add(1, Ordering::SeqCst);
        prev == 0
      })),
    );
    let _guard = DeadlineGuard::install(Some(&deadline));

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let result = fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    );
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn flex_style_fingerprint_accounts_for_scrollbar_and_overflow() {
    let mut style_a = ComputedStyle::default();
    style_a.display = Display::Flex;
    style_a.overflow_x = Overflow::Hidden;
    style_a.scrollbar_width = ScrollbarWidth::Auto;

    let mut style_b = style_a.clone();
    style_b.scrollbar_width = ScrollbarWidth::Thin;

    let mut style_c = style_a.clone();
    style_c.overflow_x = Overflow::Scroll;

    let fp_a = super::flex_style_fingerprint(&style_a);
    let fp_b = super::flex_style_fingerprint(&style_b);
    let fp_c = super::flex_style_fingerprint(&style_c);

    assert_ne!(
      fp_a, fp_b,
      "scrollbar width should affect flex style fingerprint"
    );
    assert_ne!(fp_a, fp_c, "overflow should affect flex style fingerprint");
  }

  #[test]
  fn taffy_template_cache_reuses_flex_templates_with_equal_styles() {
    use crate::layout::taffy_integration::{taffy_flex_style_fingerprint, TaffyNodeCacheKey};
    use taffy::TaffyTree;

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.grid_column_gap = Length::px(8.0);
    container_style.grid_row_gap = Length::px(4.0);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(10.0));
    item_style.height = Some(Length::px(10.0));
    item_style.min_width = Some(Length::px(0.0));
    item_style.min_height = Some(Length::px(0.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    item_style.min_width_keyword = None;
    item_style.min_height_keyword = None;

    let container_style_a = Arc::new(container_style.clone());
    let container_style_b = Arc::new(container_style);
    assert!(
      !Arc::ptr_eq(&container_style_a, &container_style_b),
      "expected distinct Arc pointers for container styles"
    );
    let item_style_a = Arc::new(item_style.clone());
    let item_style_b = Arc::new(item_style);
    assert!(
      !Arc::ptr_eq(&item_style_a, &item_style_b),
      "expected distinct Arc pointers for item styles"
    );

    let mut container_a = BoxNode::new_block(
      container_style_a,
      FormattingContextType::Flex,
      vec![
        BoxNode::new_block(item_style_a.clone(), FormattingContextType::Block, vec![]),
        BoxNode::new_block(item_style_a, FormattingContextType::Block, vec![]),
      ],
    );
    container_a.id = 1;
    container_a.children[0].id = 10;
    container_a.children[1].id = 11;

    let mut container_b = BoxNode::new_block(
      container_style_b,
      FormattingContextType::Flex,
      vec![
        BoxNode::new_block(item_style_b.clone(), FormattingContextType::Block, vec![]),
        BoxNode::new_block(item_style_b, FormattingContextType::Block, vec![]),
      ],
    );
    container_b.id = 2;
    container_b.children[0].id = 20;
    container_b.children[1].id = 21;

    let fc = FlexFormattingContext::new();
    assert_eq!(fc.taffy_cache.template_count(), 0);

    let children_a: Vec<&BoxNode> = container_a.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_a = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Flex,
      taffy_flex_style_fingerprint(container_a.style.as_ref()),
      super::flex_child_fingerprint(&children_a, &mut deadline_counter).expect("fingerprint"),
      fc.viewport_size,
    );

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_a,
      container_a.style.as_ref(),
      &children_a,
      &constraints,
      &mut node_map,
    )
    .expect("build taffy tree");

    assert_eq!(
      fc.taffy_cache.template_count(),
      1,
      "first build should insert a single cached template"
    );
    let template_a = fc
      .taffy_cache
      .get(&key_a)
      .expect("template should be cached after first build");

    let children_b: Vec<&BoxNode> = container_b.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_b = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Flex,
      taffy_flex_style_fingerprint(container_b.style.as_ref()),
      super::flex_child_fingerprint(&children_b, &mut deadline_counter).expect("fingerprint"),
      fc.viewport_size,
    );
    assert_eq!(
      key_a, key_b,
      "cache keys should match for identical style values regardless of ids/pointers"
    );

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_b,
      container_b.style.as_ref(),
      &children_b,
      &constraints,
      &mut node_map,
    )
    .expect("build taffy tree");

    assert_eq!(
      fc.taffy_cache.template_count(),
      1,
      "second build should hit the template cache instead of inserting a new entry"
    );
    let template_b = fc
      .taffy_cache
      .get(&key_b)
      .expect("template should be cached after second build");
    assert!(
      Arc::ptr_eq(&template_a, &template_b),
      "expected second build to reuse existing cached template"
    );
  }

  #[test]
  fn flex_auto_min_size_is_recomputed_on_taffy_template_cache_hits() {
    use crate::layout::taffy_integration::{taffy_flex_style_fingerprint, TaffyNodeCacheKey};

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;

    let mut item_style = ComputedStyle::default();
    item_style.overflow_x = Overflow::Visible;

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let short = BoxNode::new_text(text_style.clone(), "xxxxxxxx".to_string());
    let long = BoxNode::new_text(text_style, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".to_string());

    let container_a = BoxNode::new_block(
      Arc::new(container_style.clone()),
      FormattingContextType::Flex,
      vec![BoxNode::new_block(
        Arc::new(item_style.clone()),
        FormattingContextType::Block,
        vec![short],
      )],
    );
    let container_b = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![BoxNode::new_block(
        Arc::new(item_style),
        FormattingContextType::Block,
        vec![long],
      )],
    );

    let fc = FlexFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let length_tag = Dimension::length(0.0).tag();

    let extract_min_width = |tree: &TaffyTree<*const BoxNode>,
                             node_map: &FxHashMap<*const BoxNode, NodeId>,
                             child: &BoxNode|
     -> f32 {
      let child_node = node_map
        .get(&(child as *const BoxNode))
        .copied()
        .expect("child node should exist in node_map");
      let style = tree
        .style(child_node)
        .expect("taffy child style should be available");
      let min_width = style.min_size.width;
      assert!(
        !min_width.is_auto(),
        "expected flex auto min-size to resolve to a min-content length"
      );
      assert_eq!(
        min_width.tag(),
        length_tag,
        "expected flex auto min-size to resolve to a length, got {min_width:?}"
      );
      min_width.value()
    };

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let root_children: Vec<&BoxNode> = container_a.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_a = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Flex,
      taffy_flex_style_fingerprint(container_a.style.as_ref()),
      super::flex_child_fingerprint(&root_children, &mut deadline_counter).expect("fingerprint"),
      fc.viewport_size,
    );
    fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_a,
      container_a.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    )
    .expect("build taffy tree");
    let min_a = extract_min_width(&taffy_tree, &node_map, &container_a.children[0]);
    assert!(min_a > 0.0, "expected non-zero min-width for short text");
    let template_a = fc
      .taffy_cache
      .get(&key_a)
      .expect("template should be cached after first build");

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut node_map: FxHashMap<*const BoxNode, NodeId> = FxHashMap::default();
    let root_children: Vec<&BoxNode> = container_b.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_b = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Flex,
      taffy_flex_style_fingerprint(container_b.style.as_ref()),
      super::flex_child_fingerprint(&root_children, &mut deadline_counter).expect("fingerprint"),
      fc.viewport_size,
    );
    assert_eq!(key_a, key_b, "expected template cache keys to match");
    fc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_b,
      container_b.style.as_ref(),
      &root_children,
      &constraints,
      &mut node_map,
    )
    .expect("build taffy tree (template cache hit)");
    let min_b = extract_min_width(&taffy_tree, &node_map, &container_b.children[0]);
    assert!(min_b > 0.0, "expected non-zero min-width for long text");
    let template_b = fc
      .taffy_cache
      .get(&key_b)
      .expect("template should be cached after second build");
    assert!(
      Arc::ptr_eq(&template_a, &template_b),
      "expected second build to reuse cached template"
    );
    assert!(
      min_b > min_a,
      "expected long text min-width ({min_b}) to exceed short text min-width ({min_a})"
    );
  }

  #[test]
  fn flex_cache_key_distinguishes_anonymous_boxes() {
    let style = Arc::new(ComputedStyle::default());

    let mut a = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let mut b = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
    a.id = 1;
    b.id = 2;
    a.styled_node_id = None;
    b.styled_node_id = None;
    a.debug_info = None;
    b.debug_info = None;

    assert_ne!(
      super::flex_cache_key(&a),
      super::flex_cache_key(&b),
      "anonymous boxes must not share flex cache keys"
    );
  }

  #[test]
  fn flex_cache_key_avoids_selector_allocations() {
    let style = Arc::new(ComputedStyle::default());
    let node = BoxNode::new_block(style, FormattingContextType::Block, vec![]).with_debug_info(
      DebugInfo::new(
        Some("div".to_string()),
        Some("carousel".to_string()),
        vec!["item".to_string(), "active".to_string()],
      ),
    );
    let (_key, selector_calls) = track_to_selector_calls(|| super::flex_cache_key(&node));
    assert_eq!(
      selector_calls, 0,
      "flex cache key should not format debug selectors"
    );
  }

  #[test]
  fn flex_cache_key_includes_generated_pseudo() {
    let style = Arc::new(ComputedStyle::default());
    let mut base = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
    base.styled_node_id = Some(42);

    let mut before = base.clone();
    before.generated_pseudo = Some(crate::tree::box_tree::GeneratedPseudoElement::Before);

    let mut after = base.clone();
    after.generated_pseudo = Some(crate::tree::box_tree::GeneratedPseudoElement::After);

    assert_ne!(
      super::flex_cache_key(&base),
      super::flex_cache_key(&before),
      "expected ::before boxes to have distinct cache keys"
    );
    assert_ne!(
      super::flex_cache_key(&base),
      super::flex_cache_key(&after),
      "expected ::after boxes to have distinct cache keys"
    );
    assert_ne!(
      super::flex_cache_key(&before),
      super::flex_cache_key(&after),
      "expected ::before and ::after boxes to have distinct cache keys"
    );
  }

  #[test]
  fn flex_cache_key_ignores_debug_info() {
    let style = Arc::new(ComputedStyle::default());
    let mut a = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    a.styled_node_id = Some(5);
    let mut b = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
    b.styled_node_id = Some(5);
    b.debug_info = Some(DebugInfo::new(Some("div".to_string()), None, vec![]));

    assert_eq!(super::flex_cache_key(&a), super::flex_cache_key(&b));
  }

  #[test]
  fn flex_does_not_shrink_below_min_width_when_overflowing() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.flex_wrap = FlexWrap::NoWrap;
    container_style.overflow_x = Overflow::Scroll;
    container_style.width = Some(Length::px(400.0));
    container_style.width_keyword = None;

    let mut items = Vec::new();
    for _ in 0..5 {
      let mut item_style = ComputedStyle::default();
      item_style.width = Some(Length::px(300.0));
      item_style.height = Some(Length::px(50.0));
      item_style.min_width = Some(Length::px(152.0));
      item_style.width_keyword = None;
      item_style.height_keyword = None;
      item_style.min_width_keyword = None;
      item_style.flex_shrink = 1.0;
      items.push(BoxNode::new_block(
        Arc::new(item_style),
        FormattingContextType::Block,
        vec![],
      ));
    }

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      items,
    );
    let fragment = fc
      .layout(&container, &LayoutConstraints::definite(400.0, 200.0))
      .unwrap();

    for (idx, child) in fragment.children.iter().enumerate() {
      assert!(
        child.bounds.width() >= 151.9,
        "child {idx} shrank below min-width: {:.2}",
        child.bounds.width()
      );
    }
  }

  #[test]
  fn measure_cache_coerces_tiny_definite_to_max_content_key() {
    use crate::geometry::Size as GeoSize;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let tiny_known = taffy::geometry::Size {
      width: Some(0.5),
      height: None,
    };
    let tiny_avail = taffy::geometry::Size {
      width: AvailableSpace::Definite(0.5),
      height: AvailableSpace::Definite(100.0),
    };
    let tiny_key = super::measure_cache_key(&tiny_known, &tiny_avail, viewport, false);

    let max_key = super::measure_cache_key(
      &taffy::geometry::Size {
        width: None,
        height: None,
      },
      &taffy::geometry::Size {
        width: AvailableSpace::MaxContent,
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );

    assert_eq!(tiny_key.0, max_key.0);
  }

  #[test]
  fn measure_cache_quantizes_definite_available_sizes() {
    use crate::geometry::Size as GeoSize;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    let key_a = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(300.3),
        height: AvailableSpace::Definite(150.7),
      },
      viewport,
      false,
    );

    let key_b = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(300.6),
        height: AvailableSpace::Definite(150.4),
      },
      viewport,
      false,
    );

    assert_eq!(
      key_a, key_b,
      "quantized definite availables should reuse the same cache key"
    );

    // Also ensure a tiny tolerance exists so very similar targets can merge.
    let key_c = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(300.0),
        height: AvailableSpace::Definite(151.0),
      },
      viewport,
      false,
    );
    assert_eq!(key_a.0, key_c.0);
  }

  #[test]
  fn measure_cache_key_snaps_known_and_available_space_to_quantized_values() {
    use crate::geometry::Size as GeoSize;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let fc = FlexFormattingContext::with_viewport(viewport);

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    let (key_a, snapped_known_a, snapped_avail_a) = super::measure_cache_key_and_snap(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(250.3),
        height: AvailableSpace::Definite(150.7),
      },
      viewport,
      false,
    );

    let (key_b, snapped_known_b, snapped_avail_b) = super::measure_cache_key_and_snap(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(250.6),
        height: AvailableSpace::Definite(150.4),
      },
      viewport,
      false,
    );

    assert_eq!(key_a, key_b);
    assert_eq!(
      snapped_known_a, snapped_known_b,
      "snapped known dimensions should match for probes sharing a cache key"
    );
    assert_eq!(
      snapped_avail_a, snapped_avail_b,
      "snapped available space should match for probes sharing a cache key"
    );

    let constraints_a = fc.constraints_from_taffy(snapped_known_a, snapped_avail_a, None);
    let constraints_b = fc.constraints_from_taffy(snapped_known_b, snapped_avail_b, None);
    assert_eq!(
      constraints_a, constraints_b,
      "layout constraints should be identical once inputs are snapped"
    );
  }

  #[test]
  fn measure_cache_key_snaps_known_dimensions_that_contribute_to_the_key() {
    use crate::geometry::Size as GeoSize;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let fc = FlexFormattingContext::with_viewport(viewport);

    let avail = taffy::geometry::Size {
      width: AvailableSpace::Definite(250.0),
      height: AvailableSpace::Definite(100.0),
    };

    let (key_a, snapped_known_a, snapped_avail_a) = super::measure_cache_key_and_snap(
      &taffy::geometry::Size {
        width: Some(250.3),
        height: None,
      },
      &avail,
      viewport,
      false,
    );

    let (key_b, snapped_known_b, snapped_avail_b) = super::measure_cache_key_and_snap(
      &taffy::geometry::Size {
        width: Some(250.6),
        height: None,
      },
      &avail,
      viewport,
      false,
    );

    assert_eq!(key_a, key_b);
    assert_eq!(snapped_known_a, snapped_known_b);
    assert_eq!(snapped_avail_a, snapped_avail_b);

    let constraints_a = fc.constraints_from_taffy(snapped_known_a, snapped_avail_a, None);
    let constraints_b = fc.constraints_from_taffy(snapped_known_b, snapped_avail_b, None);
    assert_eq!(constraints_a, constraints_b);
  }

  #[test]
  fn measure_cache_key_distinguishes_min_and_max_content() {
    use crate::geometry::Size as GeoSize;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    let min_key = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::MinContent,
        height: AvailableSpace::Definite(200.0),
      },
      viewport,
      false,
    );
    let max_key = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::MaxContent,
        height: AvailableSpace::Definite(200.0),
      },
      viewport,
      false,
    );
    assert_ne!(
      min_key.0, max_key.0,
      "min-content and max-content width probes must not share a cache key"
    );

    // Height probes should also distinguish intrinsic variants when height is not ignored.
    let known_width = taffy::geometry::Size {
      width: Some(100.0),
      height: None,
    };
    let min_h_key = super::measure_cache_key(
      &known_width,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::MinContent,
      },
      viewport,
      false,
    );
    let max_h_key = super::measure_cache_key(
      &known_width,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::MaxContent,
      },
      viewport,
      false,
    );
    assert_ne!(
      min_h_key.1, max_h_key.1,
      "min-content and max-content height probes must not share a cache key"
    );
  }

  #[test]
  fn layout_cache_key_distinguishes_min_and_max_content() {
    use crate::geometry::Size as GeoSize;

    let viewport = GeoSize::new(1200.0, 800.0);
    let min_constraints = LayoutConstraints::new(
      CrateAvailableSpace::MinContent,
      CrateAvailableSpace::Indefinite,
    );
    let max_constraints = LayoutConstraints::new(
      CrateAvailableSpace::MaxContent,
      CrateAvailableSpace::Indefinite,
    );

    let min_key = super::layout_cache_key(&min_constraints, viewport).expect("layout cache key");
    let max_key = super::layout_cache_key(&max_constraints, viewport).expect("layout cache key");

    assert_ne!(
      min_key.0, max_key.0,
      "layout cache keys must distinguish min/max-content constraints"
    );
  }

  #[test]
  fn measure_cache_clamps_definite_widths_at_viewport() {
    use crate::geometry::Size as GeoSize;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    let key_viewport = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(1200.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );

    // Any available width larger than the viewport is clamped by `constraints_from_taffy`, so it
    // should also share the same cache key.
    let key_wider = super::measure_cache_key(
      &known,
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(1700.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    assert_eq!(key_viewport, key_wider);

    // Known dimensions should receive the same clamping treatment.
    let key_known_wider = super::measure_cache_key(
      &taffy::geometry::Size {
        width: Some(1700.0),
        height: None,
      },
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(1700.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    let key_known_viewport = super::measure_cache_key(
      &taffy::geometry::Size {
        width: Some(1200.0),
        height: None,
      },
      &taffy::geometry::Size {
        width: AvailableSpace::Definite(1200.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    assert_eq!(key_known_wider, key_known_viewport);
  }

  #[test]
  fn measure_cache_key_unique_count_is_bounded_for_jittery_large_widths() {
    use crate::geometry::Size as GeoSize;
    use std::collections::HashSet;
    use taffy::style::AvailableSpace;

    let viewport = GeoSize::new(1200.0, 800.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    // Simulate a series of measurement probes where the available width fluctuates slightly
    // above the viewport width (common when Taffy propagates intermediate, over-large widths).
    let mut keys = HashSet::new();
    for i in 0..256u32 {
      let w = 1200.0 + (i as f32) * 1.0;
      let key = super::measure_cache_key(
        &known,
        &taffy::geometry::Size {
          width: AvailableSpace::Definite(w),
          height: AvailableSpace::Definite(200.0),
        },
        viewport,
        false,
      );
      keys.insert(key);
    }

    assert_eq!(
      keys.len(),
      1,
      "expected jittery widths above the viewport to coalesce into a single cache key"
    );
  }

  #[test]
  fn measured_fragments_normalize_and_reuse_fragments() {
    use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
    use std::collections::HashMap;

    // This test relies on the flex measure callback executing (to populate and hit the measured
    // fragment cache). Use thread-local toggles so other unit tests running in parallel don't see
    // `FASTR_*` overrides (notably `FASTR_DISABLE_FLEX_CACHE`).
    with_thread_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        "FASTR_FLEX_PROFILE".to_string(),
        "1".to_string(),
      )]))),
      || {
        let measured_fragments = Arc::new(ShardedFlexCache::new_measure());
        let layout_fragments = Arc::new(ShardedFlexCache::new_layout());
        let viewport = Size::new(200.0, 200.0);
        let fc = FlexFormattingContext::with_viewport_and_cb(
          viewport,
          ContainingBlock::viewport(viewport),
          FontContext::new(),
          measured_fragments.clone(),
          layout_fragments.clone(),
        )
        .with_parallelism(LayoutParallelism::disabled());

        let mut child_style = ComputedStyle::default();
        child_style.display = Display::Block;
        child_style.position = Position::Relative;
        child_style.left = crate::style::types::InsetValue::Length(Length::px(9.0));
        child_style.top = crate::style::types::InsetValue::Length(Length::px(11.0));
        child_style.width = Some(Length::px(40.0));
        child_style.height = Some(Length::px(20.0));
        child_style.width_keyword = None;
        child_style.height_keyword = None;
        let grandchild = BoxNode::new_block(
          Arc::new(ComputedStyle::default()),
          FormattingContextType::Block,
          vec![],
        );
        let mut child = BoxNode::new_block(
          Arc::new(child_style),
          FormattingContextType::Block,
          vec![grandchild],
        );
        child.id = 2;

        let mut container_style = ComputedStyle::default();
        container_style.display = Display::Flex;
        container_style.flex_direction = FlexDirection::Row;
        container_style.width = Some(Length::px(120.0));
        container_style.height = Some(Length::px(60.0));
        container_style.width_keyword = None;
        container_style.height_keyword = None;
        let mut container = BoxNode::new_block(
          Arc::new(container_style),
          FormattingContextType::Flex,
          vec![child.clone()],
        );
        // Keep the container id as 0 so it is never eligible for the global layout cache.
        container.id = 0;

        let constraints = LayoutConstraints::definite(120.0, 60.0);
        let first_fragment = fc.layout(&container, &constraints).unwrap();
        let first_child = &first_fragment.children[0];
        assert!(
          first_child.bounds.x() != 0.0 || first_child.bounds.y() != 0.0,
          "relative positioning should offset the measured fragment"
        );
        let expected_origin = first_child.bounds.origin;

        let cache_key = flex_cache_key(&child);
        let cached = measured_fragments.find_fragment(
          cache_key,
          Size::new(first_child.bounds.width(), first_child.bounds.height()),
        );
        let cached_fragment = cached.expect("child fragment cached").fragment;
        assert_eq!(cached_fragment.bounds.origin, Point::new(0.0, 0.0));

        let shard_hits_before: u64 = measured_fragments
          .shard_stats()
          .into_iter()
          .map(|s| s.hits)
          .sum();

        // Avoid layout cache hits so reuse flows through the measured fragment cache.
        layout_fragments.clear();

        let second_fragment = fc.layout(&container, &constraints).unwrap();
        let second_child = &second_fragment.children[0];
        let shard_hits_after: u64 = measured_fragments
          .shard_stats()
          .into_iter()
          .map(|s| s.hits)
          .sum();

        assert!(
          shard_hits_after > shard_hits_before,
          "measurement cache should be hit on reuse"
        );
        assert_eq!(second_child.bounds.origin, expected_origin);
      },
    );
  }

  #[test]
  fn measured_fragments_reuse_for_offset_children_without_content_visibility_auto() {
    use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
    use std::collections::HashMap;
    use std::sync::Arc;

    with_thread_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        "FASTR_FLEX_PROFILE".to_string(),
        "1".to_string(),
      )]))),
      || {
        let measured_fragments = Arc::new(ShardedFlexCache::new_measure());
        let layout_fragments = Arc::new(ShardedFlexCache::new_layout());
        let viewport = Size::new(200.0, 200.0);
        let fc = FlexFormattingContext::with_viewport_and_cb(
          viewport,
          ContainingBlock::viewport(viewport),
          FontContext::new(),
          measured_fragments.clone(),
          layout_fragments,
        )
        .with_parallelism(LayoutParallelism::disabled());

        let mut item_style = ComputedStyle::default();
        item_style.display = Display::Block;
        item_style.width = Some(Length::px(40.0));
        item_style.height = Some(Length::px(20.0));
        item_style.width_keyword = None;
        item_style.height_keyword = None;
        let item_style = Arc::new(item_style);

        let mut item_a =
          BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
        item_a.id = 2;
        let mut item_b = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
        item_b.id = 3;

        let mut container_style = ComputedStyle::default();
        container_style.display = Display::Flex;
        container_style.flex_direction = FlexDirection::Row;
        container_style.width = Some(Length::px(120.0));
        container_style.height = Some(Length::px(40.0));
        container_style.width_keyword = None;
        container_style.height_keyword = None;
        let mut container = BoxNode::new_block(
          Arc::new(container_style),
          FormattingContextType::Flex,
          vec![item_a, item_b],
        );
        // Use id=0 to avoid hitting the global layout cache in this test.
        container.id = 0;

        let hits_before: u64 = measured_fragments
          .shard_stats()
          .into_iter()
          .map(|s| s.hits)
          .sum();

        let constraints = LayoutConstraints::definite(120.0, 40.0);
        let fragment = fc.layout(&container, &constraints).expect("layout");
        assert_eq!(fragment.children.len(), 2);

        let hits_after: u64 = measured_fragments
          .shard_stats()
          .into_iter()
          .map(|s| s.hits)
          .sum();

        assert!(
          hits_after.saturating_sub(hits_before) >= 2,
          "expected measured fragment cache hits for both children, got {}",
          hits_after.saturating_sub(hits_before)
        );
      },
    );
  }

  #[test]
  fn test_flex_context_creation() {
    let _fc = FlexFormattingContext::new();
    let _fc_default = FlexFormattingContext::default();
    // Both methods should create valid contexts
    // (PhantomData<()> is zero-sized, so we just verify creation works)
  }

  #[test]
  fn absolute_child_is_positioned_against_flex_padding_box() {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.border_left_width = Length::px(2.0);
    container_style.border_top_width = Length::px(3.0);
    container_style.border_right_width = Length::px(2.0);
    container_style.border_bottom_width = Length::px(3.0);
    container_style.border_left_style = BorderStyle::Solid;
    container_style.border_top_style = BorderStyle::Solid;
    container_style.border_right_style = BorderStyle::Solid;
    container_style.border_bottom_style = BorderStyle::Solid;
    container_style.padding_left = Length::px(10.0);
    container_style.padding_top = Length::px(8.0);
    container_style.padding_right = Length::px(10.0);
    container_style.padding_bottom = Length::px(8.0);

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    abs_style.width = Some(Length::px(20.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![abs_child],
    );

    let fc = FlexFormattingContext::with_viewport(Size::new(200.0, 200.0));
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let abs_fragment = &fragment.children[0];
    // The absolute containing block is the flex container's padding box. The padding edge sits
    // inside the border, so top/left offsets are applied from the border thickness, not the
    // content box origin.
    assert_eq!(abs_fragment.bounds.x(), 7.0);
    assert_eq!(abs_fragment.bounds.y(), 10.0);
    assert_eq!(abs_fragment.bounds.width(), 20.0);
    assert_eq!(abs_fragment.bounds.height(), 10.0);
    let abs_fragment_style = abs_fragment.style.as_ref().expect("abs style preserved");
    assert_eq!(abs_fragment_style.position, Position::Absolute);
    assert_eq!(
      abs_fragment_style.left,
      crate::style::types::InsetValue::Length(Length::px(5.0))
    );
    assert_eq!(
      abs_fragment_style.top,
      crate::style::types::InsetValue::Length(Length::px(7.0))
    );
  }

  #[test]
  fn absolute_child_inherits_positioned_containing_block_from_ancestor() {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Static;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(6.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![abs_child],
    );

    let cb_rect = Rect::from_xywh(20.0, 30.0, 150.0, 150.0);
    let viewport = Size::new(300.0, 300.0);
    let cb = ContainingBlock::with_viewport(cb_rect, viewport);
    let fc = FlexFormattingContext::with_viewport_and_cb(
      viewport,
      cb,
      FontContext::new(),
      std::sync::Arc::new(ShardedFlexCache::new_measure()),
      std::sync::Arc::new(ShardedFlexCache::new_layout()),
    );
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let abs_fragment = &fragment.children[0];
    assert_eq!(abs_fragment.bounds.x(), 25.0);
    assert_eq!(abs_fragment.bounds.y(), 37.0);
    let abs_fragment_style = abs_fragment.style.as_ref().expect("abs style preserved");
    assert_eq!(abs_fragment_style.position, Position::Absolute);
    assert_eq!(
      abs_fragment_style.left,
      crate::style::types::InsetValue::Length(Length::px(5.0))
    );
    assert_eq!(
      abs_fragment_style.top,
      crate::style::types::InsetValue::Length(Length::px(7.0))
    );
  }

  #[test]
  fn test_basic_flex_row_layout() {
    let fc = FlexFormattingContext::new();

    // Create flex container with 3 children
    let item1 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item3 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![item1, item2, item3],
    );

    let constraints = LayoutConstraints::definite(400.0, 600.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Check that children are laid out horizontally
    assert_eq!(fragment.children.len(), 3);

    // Items should be positioned at x=0, 100, 200
    assert_eq!(fragment.children[0].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 100.0);
    assert_eq!(fragment.children[2].bounds.x(), 200.0);

    // All items should have same y position
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 0.0);
    assert_eq!(fragment.children[2].bounds.y(), 0.0);
  }

  #[test]
  fn test_flex_column_layout() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Column;

    let item1 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(100.0, 75.0),
      FormattingContextType::Block,
      vec![],
    );
    let item3 = BoxNode::new_block(
      create_item_style(100.0, 25.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item1, item2, item3],
    );

    let constraints = LayoutConstraints::definite(400.0, 600.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Check that children are laid out vertically
    assert_eq!(fragment.children.len(), 3);

    // Items should be positioned at y=0, 50, 125
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 50.0);
    assert_eq!(fragment.children[2].bounds.y(), 125.0);

    // All items should have same x position
    assert_eq!(fragment.children[0].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 0.0);
    assert_eq!(fragment.children[2].bounds.x(), 0.0);
  }

  #[test]
  fn test_flex_grow() {
    let fc = FlexFormattingContext::new();

    // Two items: one with flex-grow: 1, one without
    let item1 = BoxNode::new_block(
      create_item_style_with_grow(100.0, 50.0, 1.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![item1, item2],
    );

    let constraints = LayoutConstraints::definite(400.0, 600.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // First item should grow to fill available space: base widths are both 100, one grows with flex-grow:1.
    // Remaining space is distributed according to flex-grow factors, so item1 ends up at 300 and item2 at 100.
    assert_eq!(fragment.children[0].bounds.width(), 300.0);
    assert_eq!(fragment.children[1].bounds.width(), 100.0);
  }

  #[test]
  fn test_flex_shrink() {
    let fc = FlexFormattingContext::new();

    // Two items, total wider than container
    let item1 = BoxNode::new_block(
      create_item_style(250.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(250.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![item1, item2],
    );

    // Container only 400px wide, but items total 500px
    let constraints = LayoutConstraints::definite(400.0, 600.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Items should shrink equally (default flex-shrink: 1)
    // Deficit: 500 - 400 = 100
    // Each shrinks by 50
    assert_eq!(fragment.children[0].bounds.width(), 200.0);
    assert_eq!(fragment.children[1].bounds.width(), 200.0);
  }

  #[test]
  fn test_justify_content_space_between() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.justify_content = JustifyContent::SpaceBetween;

    let item1 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item3 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item1, item2, item3],
    );

    let constraints = LayoutConstraints::definite(500.0, 600.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Space between: first at start, last at end, equal spacing between
    // Items: 100 + 100 + 100 = 300
    // Container: 500
    // Space: 200
    // Gaps: 2 (between 3 items)
    // Gap size: 100
    assert_eq!(fragment.children[0].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 200.0); // 100 + 100 gap
    assert_eq!(fragment.children[2].bounds.x(), 400.0); // 200 + 100 width + 100 gap
  }

  #[test]
  fn test_align_items_center() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.align_items = AlignItems::Center;
    container_style.height = Some(Length::px(100.0));
    container_style.height_keyword = None;

    // Different height items
    let item1 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(100.0, 100.0),
      FormattingContextType::Block,
      vec![],
    );
    let item3 = BoxNode::new_block(
      create_item_style(100.0, 74.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item1, item2, item3],
    );

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Container height is 100px, items should be vertically centered
    // Taffy may round to integer pixels, so we check approximate values
    assert_eq!(fragment.children[0].bounds.y(), 25.0); // (100 - 50) / 2
    assert_eq!(fragment.children[1].bounds.y(), 0.0); // Tallest, at top
    assert_eq!(fragment.children[2].bounds.y(), 13.0); // (100 - 74) / 2 = 13
  }

  #[test]
  fn flex_align_items_baseline_aligns_text() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.align_items = AlignItems::Baseline;
    container_style.height = Some(Length::px(80.0));
    container_style.height_keyword = None;

    let mut small_text_style = ComputedStyle::default();
    small_text_style.font_size = 12.0;
    let small_text_style = Arc::new(small_text_style);
    let small_text = BoxNode::new_text(small_text_style.clone(), "small".to_string());
    let small_inline = BoxNode::new_inline(small_text_style.clone(), vec![small_text]);
    let small_item = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![small_inline],
    );

    let mut large_text_style = ComputedStyle::default();
    large_text_style.font_size = 24.0;
    let large_text_style = Arc::new(large_text_style);
    let large_text = BoxNode::new_text(large_text_style.clone(), "Large".to_string());
    let large_inline = BoxNode::new_inline(large_text_style.clone(), vec![large_text]);
    let large_item = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![large_inline],
    );

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![small_item, large_item],
    );

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    let small_baseline = baseline_position(&fragment.children[0]);
    let large_baseline = baseline_position(&fragment.children[1]);
    assert!(
      (small_baseline - large_baseline).abs() < 0.5,
      "baselines misaligned: {:.2} vs {:.2}",
      small_baseline,
      large_baseline
    );
  }

  #[test]
  fn flex_align_items_baseline_handles_replaced_fallback() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.align_items = AlignItems::Baseline;

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);
    let text = BoxNode::new_text(text_style.clone(), "Text".to_string());
    let inline = BoxNode::new_inline(text_style.clone(), vec![text]);
    let text_item = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![inline],
    );

    let mut replaced_style = ComputedStyle::default();
    replaced_style.width = Some(Length::px(20.0));
    replaced_style.height = Some(Length::px(10.0));
    replaced_style.width_keyword = None;
    replaced_style.height_keyword = None;
    let replaced =
      BoxNode::new_replaced(Arc::new(replaced_style), ReplacedType::Canvas, None, None);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![text_item, replaced],
    );

    let constraints = LayoutConstraints::definite(200.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    let text_baseline = baseline_position(&fragment.children[0]);
    let replaced_baseline = baseline_position(&fragment.children[1]);
    assert!(
      (text_baseline - replaced_baseline).abs() < 0.5,
      "replaced baseline not aligned: {:.2} vs {:.2}",
      text_baseline,
      replaced_baseline
    );
  }

  #[test]
  fn flex_baseline_alignment_is_per_line() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.align_items = AlignItems::Baseline;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.width = Some(Length::px(120.0));
    container_style.width_keyword = None;

    let make_item = |font_size: f32, width: f32| {
      let mut text_style = ComputedStyle::default();
      text_style.font_size = font_size;
      let text_style = Arc::new(text_style);
      let text = BoxNode::new_text(text_style.clone(), "Wrap".to_string());
      let inline = BoxNode::new_inline(text_style.clone(), vec![text]);
      let mut item_style = ComputedStyle::default();
      item_style.width = Some(Length::px(width));
      item_style.width_keyword = None;
      BoxNode::new_block(
        Arc::new(item_style),
        FormattingContextType::Block,
        vec![inline],
      )
    };

    let item1 = make_item(12.0, 60.0);
    let item2 = make_item(18.0, 50.0);
    let item3 = make_item(16.0, 80.0);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item1, item2, item3],
    );

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert!(
      fragment.children.len() == 3,
      "expected three flex items, got {}",
      fragment.children.len()
    );
    let line1_first = baseline_position(&fragment.children[0]);
    let line1_second = baseline_position(&fragment.children[1]);
    assert!(
      (line1_first - line1_second).abs() < 0.5,
      "first line baselines differ: {:.2} vs {:.2}",
      line1_first,
      line1_second
    );

    let line2_baseline = baseline_position(&fragment.children[2]);
    assert!(
      (line2_baseline - line1_first).abs() > 0.5,
      "baselines leaked across lines: {:.2} vs {:.2}",
      line2_baseline,
      line1_first
    );
    assert!(
      fragment.children[2].bounds.y() > fragment.children[0].bounds.y(),
      "wrapped item should appear on a new line"
    );
  }

  #[test]
  fn flex_align_self_overrides_parent_align_items() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.align_items = AlignItems::Center;
    container_style.height = Some(Length::px(100.0));
    container_style.height_keyword = None;

    let mut item_style = ComputedStyle::default();
    item_style.height = Some(Length::px(20.0));
    item_style.height_keyword = None;
    item_style.align_self = Some(AlignItems::FlexEnd);

    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item],
    );

    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Parent would center to y=40; align-self:end should place it at y=80
    assert_eq!(fragment.children[0].bounds.y(), 80.0);
  }

  #[test]
  fn writing_mode_vertical_treats_row_as_column() {
    let fc = FlexFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = Display::Flex;
    style.flex_direction = FlexDirection::Row;
    style.writing_mode = crate::style::types::WritingMode::VerticalRl;

    let child1 = BoxNode::new_block(
      create_item_style(20.0, 10.0),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      create_item_style(20.0, 10.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Flex,
      vec![child1, child2],
    );
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 10.0);
  }

  #[test]
  fn writing_mode_vertical_align_start_maps_to_block_start() {
    let fc = FlexFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = Display::Flex;
    style.flex_direction = FlexDirection::Row;
    style.writing_mode = crate::style::types::WritingMode::VerticalRl;
    style.align_items = AlignItems::Start;
    style.width = Some(Length::px(100.0));
    style.width_keyword = None;

    let child = BoxNode::new_block(
      create_item_style(20.0, 10.0),
      FormattingContextType::Block,
      vec![],
    );
    let container = BoxNode::new_block(Arc::new(style), FormattingContextType::Flex, vec![child]);
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Block axis start for vertical-rl is the right edge, so x should be at 80
    assert_eq!(fragment.children[0].bounds.x(), 80.0);
  }

  #[test]
  fn writing_mode_vertical_row_justify_start_and_end_follow_inline_axis() {
    let fc = FlexFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = Display::Flex;
    style.flex_direction = FlexDirection::Row; // inline axis (vertical in vertical-rl)
    style.writing_mode = crate::style::types::WritingMode::VerticalRl;
    style.justify_content = JustifyContent::FlexStart;
    style.height = Some(Length::px(100.0));
    style.height_keyword = None;

    let mut end_style = style.clone();
    end_style.justify_content = JustifyContent::FlexEnd;

    let child1 = BoxNode::new_block(
      create_item_style(10.0, 10.0),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      create_item_style(10.0, 10.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Flex,
      vec![child1, child2],
    );

    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Inline axis is vertical; flex-start packs at the top.
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 10.0);

    // Now flex-end should pack to the bottom of the inline axis.
    let end_container = BoxNode::new_block(
      Arc::new(end_style),
      FormattingContextType::Flex,
      vec![
        BoxNode::new_block(
          create_item_style(10.0, 10.0),
          FormattingContextType::Block,
          vec![],
        ),
        BoxNode::new_block(
          create_item_style(10.0, 10.0),
          FormattingContextType::Block,
          vec![],
        ),
      ],
    );
    let end_fragment = fc.layout(&end_container, &constraints).unwrap();
    assert_eq!(end_fragment.children[0].bounds.y(), 80.0);
    assert_eq!(end_fragment.children[1].bounds.y(), 90.0);
  }

  #[test]
  fn flex_item_alignment_uses_parent_axes_not_item_writing_mode() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row; // becomes vertical under vertical-rl
    container_style.writing_mode = crate::style::types::WritingMode::VerticalRl;
    container_style.align_items = AlignItems::Center;
    container_style.width = Some(Length::px(100.0));
    container_style.width_keyword = None;

    let mut child1_style = ComputedStyle::default();
    child1_style.width = Some(Length::px(10.0));
    child1_style.height = Some(Length::px(10.0));
    child1_style.width_keyword = None;
    child1_style.height_keyword = None;
    child1_style.align_self = Some(AlignItems::Start);
    // Different writing mode should not affect axis interpretation
    child1_style.writing_mode = crate::style::types::WritingMode::HorizontalTb;
    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);

    let mut child2_style = ComputedStyle::default();
    child2_style.width = Some(Length::px(10.0));
    child2_style.height = Some(Length::px(10.0));
    child2_style.width_keyword = None;
    child2_style.height_keyword = None;
    child2_style.align_self = Some(AlignItems::End);
    child2_style.writing_mode = crate::style::types::WritingMode::HorizontalTb;
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![child1, child2],
    );

    let constraints = LayoutConstraints::definite(100.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    // Cross axis is horizontal with start at the right edge for vertical-rl.
    assert_eq!(fragment.children[0].bounds.x(), 90.0);
    assert_eq!(fragment.children[1].bounds.x(), 0.0);
    // Main axis is vertical; children stack along y.
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 10.0);
  }

  #[test]
  fn flex_item_aspect_ratio_sets_width_from_height() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.align_items = AlignItems::FlexStart;

    let mut item_style = ComputedStyle::default();
    item_style.height = Some(Length::px(40.0));
    item_style.height_keyword = None;
    item_style.aspect_ratio = AspectRatio::Ratio(2.0);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item],
    );

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.width(), 80.0);
    assert_eq!(fragment.children[0].bounds.height(), 40.0);
  }

  #[test]
  fn flex_item_aspect_ratio_sets_height_from_width() {
    let fc = FlexFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.align_items = AlignItems::FlexStart;

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(120.0));
    item_style.width_keyword = None;
    item_style.aspect_ratio = AspectRatio::Ratio(3.0);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item],
    );

    let constraints = LayoutConstraints::definite(300.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.width(), 120.0);
    assert_eq!(fragment.children[0].bounds.height(), 40.0);
  }

  #[test]
  fn test_intrinsic_sizing_max_content() {
    let fc = FlexFormattingContext::new();

    let item1 = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );
    let item2 = BoxNode::new_block(
      create_item_style(150.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![item1, item2],
    );

    let width = fc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
      .unwrap();

    // Max-content width should be sum of children widths (row direction)
    assert_eq!(width, 250.0);
  }

  #[test]
  fn flex_intrinsic_inline_size_is_deterministic_under_parallelism() {
    use rayon::ThreadPoolBuilder;

    // Use values that are sensitive to summation order:
    // - Large widths (2^26) make the f32 ULP 8+ so small contributions can be rounded away.
    // - Small widths + fractional margins are lost when added after the large values, but would
    //   affect the result if their summation order changes.
    let large_width = 67_108_864.0; // 2^26
    let large_count = 8usize;
    let small_count = 1024usize;

    let mut children = Vec::with_capacity(large_count + small_count);
    for _ in 0..large_count {
      let mut style = ComputedStyle::default();
      style.width = Some(Length::px(large_width));
      style.height = Some(Length::px(10.0));
      children.push(BoxNode::new_block(
        Arc::new(style),
        FormattingContextType::Block,
        vec![],
      ));
    }
    for _ in 0..small_count {
      let mut style = ComputedStyle::default();
      style.width = Some(Length::px(1.0));
      style.height = Some(Length::px(10.0));
      style.margin_left = Some(Length::px(0.25));
      style.margin_right = Some(Length::px(0.25));
      children.push(BoxNode::new_block(
        Arc::new(style),
        FormattingContextType::Block,
        vec![],
      ));
    }

    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, children);

    let sequential_fc =
      FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let expected_bits = sequential_fc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
      .expect("sequential intrinsic sizing")
      .to_bits();

    let parallel_fc = FlexFormattingContext::new()
      .with_parallelism(LayoutParallelism::enabled(1).with_max_threads(Some(4)));
    assert!(
      parallel_fc
        .parallelism
        .should_parallelize(container.children.len()),
      "expected the test container to trigger the parallel intrinsic sizing path"
    );

    let pool = ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let results = pool.install(|| {
      (0..64usize)
        .map(|_| {
          parallel_fc
            .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
            .expect("parallel intrinsic sizing")
            .to_bits()
        })
        .collect::<Vec<_>>()
    });

    for (run, bits) in results.into_iter().enumerate() {
      assert_eq!(
        bits, expected_bits,
        "parallel intrinsic inline size differed on run {run}"
      );
    }
  }

  #[test]
  fn test_nested_flex() {
    let fc = FlexFormattingContext::new();

    // Inner flex container with two items
    let inner_item1 = BoxNode::new_block(
      create_item_style(50.0, 30.0),
      FormattingContextType::Block,
      vec![],
    );
    let inner_item2 = BoxNode::new_block(
      create_item_style(50.0, 30.0),
      FormattingContextType::Block,
      vec![],
    );

    let inner_container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![inner_item1, inner_item2],
    );

    // Outer flex container
    let outer_item = BoxNode::new_block(
      create_item_style(100.0, 50.0),
      FormattingContextType::Block,
      vec![],
    );

    let outer_container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![inner_container, outer_item],
    );

    let constraints = LayoutConstraints::definite(400.0, 600.0);
    let fragment = fc.layout(&outer_container, &constraints).unwrap();

    // Outer container has 2 children
    assert_eq!(fragment.children.len(), 2);

    // First child (inner container) should have 2 children
    assert_eq!(fragment.children[0].children.len(), 2);

    // Inner items should be laid out horizontally within their container
    assert_eq!(fragment.children[0].children[0].bounds.x(), 0.0);
    assert_eq!(fragment.children[0].children[1].bounds.x(), 50.0);
  }

  #[test]
  fn test_empty_flex_container() {
    let fc = FlexFormattingContext::new();

    let container = BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, vec![]);

    let constraints = LayoutConstraints::definite(400.0, 600.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 0);
  }

  #[test]
  fn test_flex_formatting_context_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FlexFormattingContext>();
  }

  #[test]
  fn test_style_conversion_flex_direction() {
    let fc = FlexFormattingContext::new();

    assert_eq!(
      fc.flex_direction_to_taffy(&ComputedStyle::default(), true, true),
      taffy::style::FlexDirection::Row
    );
    let mut row_rev = ComputedStyle::default();
    row_rev.flex_direction = FlexDirection::RowReverse;
    assert_eq!(
      fc.flex_direction_to_taffy(&row_rev, true, true),
      taffy::style::FlexDirection::RowReverse
    );

    let mut col = ComputedStyle::default();
    col.flex_direction = FlexDirection::Column;
    assert_eq!(
      fc.flex_direction_to_taffy(&col, true, true),
      taffy::style::FlexDirection::Column
    );

    let mut col_rev = ComputedStyle::default();
    col_rev.flex_direction = FlexDirection::ColumnReverse;
    assert_eq!(
      fc.flex_direction_to_taffy(&col_rev, true, true),
      taffy::style::FlexDirection::ColumnReverse
    );
  }

  #[test]
  fn test_style_conversion_flex_wrap() {
    let fc = FlexFormattingContext::new();

    assert_eq!(
      fc.flex_wrap_to_taffy(FlexWrap::NoWrap),
      taffy::style::FlexWrap::NoWrap
    );
    assert_eq!(
      fc.flex_wrap_to_taffy(FlexWrap::Wrap),
      taffy::style::FlexWrap::Wrap
    );
    assert_eq!(
      fc.flex_wrap_to_taffy(FlexWrap::WrapReverse),
      taffy::style::FlexWrap::Wrap
    );
  }

  #[test]
  fn test_length_conversion() {
    let fc = FlexFormattingContext::new();
    let style = ComputedStyle::default();

    // Pixel values
    let len_px = Length::px(100.0);
    assert_eq!(
      fc.length_to_dimension(&len_px, &style),
      Dimension::length(100.0)
    );

    // Percentage values
    let len_percent = Length::percent(50.0);
    assert_eq!(
      fc.length_to_dimension(&len_percent, &style),
      Dimension::percent(0.5)
    ); // 50% = 0.5

    // Auto (None)
    assert_eq!(
      fc.length_option_to_dimension(None, &style),
      Dimension::auto()
    );
  }

  #[test]
  fn taffy_perf_counters_record_flex_measure_and_compute_time() {
    let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
    let mut renderer = FastRender::with_config(config).expect("renderer");
    let html = r#"<!doctype html>
      <html>
        <body>
          <div style="display:flex">
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
      .taffy_flex_measure_calls
      .expect("flex measure call count should be recorded");
    assert!(measure_calls > 0, "expected flex measure calls > 0");
    let compute_ms = stats
      .layout
      .taffy_flex_compute_cpu_ms
      .expect("flex compute_cpu_ms should be recorded");
    assert!(compute_ms >= 0.0, "expected flex compute_cpu_ms >= 0");
  }

  #[test]
  fn sharded_flex_cache_supports_parallel_layouts() {
    use crate::debug::runtime::{set_runtime_toggles, RuntimeToggles};
    let mut toggles = std::collections::HashMap::new();
    toggles.insert("FASTR_FLEX_PROFILE".to_string(), "1".to_string());
    let guard = set_runtime_toggles(std::sync::Arc::new(RuntimeToggles::from_map(toggles)));

    let measure_cache = Arc::new(ShardedFlexCache::new_measure());
    let layout_cache = Arc::new(ShardedFlexCache::new_layout());
    let viewport = Size::new(640.0, 480.0);
    let fc = FlexFormattingContext::with_viewport_and_cb(
      viewport,
      ContainingBlock::viewport(viewport),
      FontContext::new(),
      measure_cache.clone(),
      layout_cache.clone(),
    )
    .with_parallelism(LayoutParallelism::enabled(1));

    let item = |w, h| {
      BoxNode::new_block(
        create_item_style(w, h),
        FormattingContextType::Block,
        vec![],
      )
    };
    let container = BoxNode::new_block(
      create_flex_style(),
      FormattingContextType::Flex,
      vec![item(40.0, 20.0), item(60.0, 24.0), item(30.0, 18.0)],
    );
    let constraints = LayoutConstraints::definite(300.0, 200.0);

    let expected = fc.layout(&container, &constraints).unwrap();
    let expected_size = expected.bounds.size;
    let shared_fc = Arc::new(fc);

    let results: Vec<Size> = (0..24)
      .into_par_iter()
      .map(|_| {
        shared_fc
          .layout(&container, &constraints)
          .map(|frag| frag.bounds.size)
          .unwrap()
      })
      .collect();
    for size in results {
      assert_eq!(size.width, expected_size.width);
      assert_eq!(size.height, expected_size.height);
    }

    let layout_stats = layout_cache.shard_stats();
    let layout_hits: u64 = layout_stats.iter().map(|s| s.hits).sum();
    let layout_misses: u64 = layout_stats.iter().map(|s| s.misses).sum();
    assert!(layout_hits > 0, "layout cache should record shard hits");
    assert!(layout_misses > 0, "layout cache should record shard misses");

    let measure_stats = measure_cache.shard_stats();
    let measure_lookups: u64 = measure_stats.iter().map(|s| s.hits + s.misses).sum();
    assert!(measure_lookups > 0, "measure cache should see lookups");

    drop(guard);
  }

  #[test]
  fn flex_width_keyword_min_content_is_narrower_than_max_content() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.width = Some(Length::px(1000.0));
    container_style.width_keyword = None;

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let make_item = |id: usize, keyword: IntrinsicSizeKeyword| {
      let text = BoxNode::new_text(text_style.clone(), "hello world goodbye".to_string());
      let inline = BoxNode::new_inline(text_style.clone(), vec![text]);
      let mut item_style = ComputedStyle::default();
      item_style.width = None;
      item_style.width_keyword = Some(keyword);
      item_style.flex_shrink = 0.0;
      let mut item = BoxNode::new_block(
        Arc::new(item_style),
        FormattingContextType::Block,
        vec![inline],
      );
      item.id = id;
      item
    };

    let min_id = 71001usize;
    let max_id = 71002usize;
    let min_item = make_item(min_id, IntrinsicSizeKeyword::MinContent);
    let max_item = make_item(max_id, IntrinsicSizeKeyword::MaxContent);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![min_item, max_item],
    );
    let constraints = LayoutConstraints::definite(1000.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();

    let min_fragment = find_block_child(&fragment, min_id);
    let max_fragment = find_block_child(&fragment, max_id);
    assert!(
      min_fragment.bounds.width() + 0.5 < max_fragment.bounds.width(),
      "expected min-content width ({:.2}) < max-content width ({:.2})",
      min_fragment.bounds.width(),
      max_fragment.bounds.width(),
    );
  }

  #[test]
  fn flex_width_keyword_fit_content_shrinks_within_definite_container() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let block_fc = fc.factory.get(FormattingContextType::Block);

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    // Measure intrinsic sizes on a plain auto-sized box to pick a container width that is between
    // min- and max-content. This keeps the assertion robust across font metrics.
    let probe_id = 72001usize;
    let probe_text = BoxNode::new_text(
      text_style.clone(),
      "fit content prefers available".to_string(),
    );
    let probe_inline = BoxNode::new_inline(text_style.clone(), vec![probe_text]);
    let mut probe_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![probe_inline],
    );
    probe_box.id = probe_id;
    let (min_intrinsic, max_intrinsic) = block_fc
      .compute_intrinsic_inline_sizes(&probe_box)
      .expect("intrinsic inline sizes");
    assert!(
      max_intrinsic > min_intrinsic + 1.0,
      "expected text to have distinct min/max intrinsic widths (min={min_intrinsic:.2}, max={max_intrinsic:.2})"
    );
    let container_width = (min_intrinsic + (max_intrinsic - min_intrinsic) / 2.0)
      .clamp(min_intrinsic + 1.0, max_intrinsic - 1.0);

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.width = Some(Length::px(container_width));
    container_style.width_keyword = None;

    let item_id = 72002usize;
    let item_text = BoxNode::new_text(
      text_style.clone(),
      "fit content prefers available".to_string(),
    );
    let item_inline = BoxNode::new_inline(text_style.clone(), vec![item_text]);
    let mut item_style = ComputedStyle::default();
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    item_style.flex_shrink = 0.0;
    let mut item = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      vec![item_inline],
    );
    item.id = item_id;

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![item],
    );

    let constraints = LayoutConstraints::definite(container_width, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();
    let item_fragment = find_block_child(&fragment, item_id);
    let measured_width = item_fragment.bounds.width();
    assert!(
      (measured_width - container_width).abs() < 1.0,
      "expected fit-content width {:.2} to match container width {:.2}",
      measured_width,
      container_width
    );
    assert!(
      measured_width + 0.5 < max_intrinsic,
      "expected fit-content width {:.2} < max-content width {:.2}",
      measured_width,
      max_intrinsic,
    );
  }

  #[test]
  fn flex_root_width_keyword_fit_content_clamps_between_min_and_max_content() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let flex_fc = fc.factory.get(FormattingContextType::Flex);

    let mut text_style = ComputedStyle::default();
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let text = BoxNode::new_text(
      text_style.clone(),
      "fit content prefers wrapping".to_string(),
    );
    let inline = BoxNode::new_inline(text_style.clone(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![inline],
    );

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.width = None;
    container_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![child],
    );

    let (min_intrinsic, max_intrinsic) = flex_fc
      .compute_intrinsic_inline_sizes(&container)
      .expect("intrinsic inline sizes");
    assert!(
      max_intrinsic > min_intrinsic + 1.0,
      "expected distinct min/max intrinsic sizes (min={min_intrinsic:.2}, max={max_intrinsic:.2})"
    );

    // Pick an available width between min- and max-content so fit-content chooses the available size.
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
      "expected fit-content width {:.2} to be smaller than max-content {:.2}",
      fragment.bounds.width(),
      max_intrinsic
    );
  }

  #[test]
  fn flex_intrinsic_keyword_resolution_respects_physical_axes_in_vertical_writing_mode() {
    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let block_fc = fc.factory.get(FormattingContextType::Block);

    let replaced_id = 73001usize;
    let mut replaced_style = ComputedStyle::default();
    replaced_style.writing_mode = WritingMode::VerticalRl;
    replaced_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    replaced_style.height_keyword = None;

    let mut replaced = BoxNode::new_replaced(
      Arc::new(replaced_style),
      ReplacedType::Canvas,
      Some(Size::new(20.0, 80.0)),
      None,
    );
    replaced.id = replaced_id;

    let inline_size = block_fc
      .compute_intrinsic_inline_size(&replaced, IntrinsicSizingMode::MaxContent)
      .expect("inline intrinsic size");
    let block_size = block_fc
      .compute_intrinsic_block_size(&replaced, IntrinsicSizingMode::MaxContent)
      .expect("block intrinsic size");
    assert!(
      (inline_size - block_size).abs() > 1.0,
      "expected inline/block intrinsic sizes to differ in vertical writing mode (inline={inline_size:.2}, block={block_size:.2})"
    );

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.flex_direction = FlexDirection::Row;
    container_style.width = Some(Length::px(200.0));
    container_style.width_keyword = None;

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Flex,
      vec![replaced],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&container, &constraints).unwrap();
    let child_fragment = &fragment.children[0];
    assert!(
      (child_fragment.bounds.width() - block_size).abs() < 0.5,
      "expected physical width {:.2} to match intrinsic block size {:.2} (not inline {:.2})",
      child_fragment.bounds.width(),
      block_size,
      inline_size,
    );
  }

  #[test]
  fn flex_measure_inline_hint_only_computed_when_measure_logging_enabled() {
    use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
    use std::collections::HashMap;

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    // Avoid flex-item auto-min-size intrinsic probes; this test only cares about the
    // debug-only max-content hint that was previously computed unconditionally.
    item_style.overflow_x = Overflow::Hidden;
    let item_style = Arc::new(item_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let mut item = BoxNode::new_block(
      item_style,
      FormattingContextType::Inline,
      vec![BoxNode::new_text(text_style, "hello".to_string())],
    );
    item.id = 65001;

    let mut container =
      BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, vec![item]);
    // Keep the container id at 0 to ensure the global layout cache can't short-circuit the flex
    // measure callback, which this test observes.
    container.id = 0;

    let constraints = LayoutConstraints::definite(200.0, 200.0);

    // Without any flex-measure logging, we should not do the extra intrinsic sizing work.
    with_thread_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        "FASTR_DISABLE_FLEX_CACHE".to_string(),
        "1".to_string(),
      )]))),
      || {
        let guard = start_flex_measure_inline_hint_counter();
        let fc = FlexFormattingContext::new();
        fc.layout(&container, &constraints).unwrap();
        drop(guard);
        assert_eq!(
          flex_measure_inline_hint_calls(),
          0,
          "inline hint should not be computed when flex-measure logging is disabled",
        );
      },
    );

    // When measure logging is enabled for this node, compute the hint for log output.
    with_thread_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([
        (
          "FASTR_LOG_FLEX_MEASURE_IDS".to_string(),
          "65001".to_string(),
        ),
        ("FASTR_DISABLE_FLEX_CACHE".to_string(), "1".to_string()),
      ]))),
      || {
        let guard = start_flex_measure_inline_hint_counter();
        let fc = FlexFormattingContext::new();
        fc.layout(&container, &constraints).unwrap();
        drop(guard);
        assert!(
          flex_measure_inline_hint_calls() > 0,
          "expected inline hint computation when measure logging is enabled"
        );
      },
    );
  }

  #[test]
  fn flex_content_visibility_auto_in_view_does_not_skip() {
    fn has_text(fragment: &FragmentNode) -> bool {
      matches!(fragment.content, FragmentContent::Text { .. })
        || fragment.children.iter().any(has_text)
    }

    let mut item_style = ComputedStyle::default();
    item_style.content_visibility = crate::style::types::ContentVisibility::Auto;
    item_style.contain_intrinsic_height.length = Some(Length::px(30.0));
    crate::style::properties::apply_content_visibility_implied_containment(&mut item_style);
    let item_style = Arc::new(item_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let item = BoxNode::new_block(
      item_style,
      FormattingContextType::Block,
      vec![BoxNode::new_text(text_style, "hello".to_string())],
    );
    let container =
      BoxNode::new_block(create_flex_style(), FormattingContextType::Flex, vec![item]);

    let fc = FlexFormattingContext::with_viewport(Size::new(200.0, 200.0));
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(fragment.children.len(), 1);
    assert!(
      has_text(&fragment.children[0]),
      "content-visibility:auto in viewport must not skip layout"
    );
  }
}
