//! Block Formatting Context Layout
//!
//! This module implements the Block Formatting Context (BFC) layout algorithm
//! as specified in CSS 2.1 Section 9.4.1.
//!
//! # Block Formatting Context
//!
//! A BFC is a layout mode where block boxes are laid out vertically, one after
//! another, starting at the top of the containing block. The vertical distance
//! between boxes is determined by margins (which may collapse).
//!
//! # Key Features
//!
//! - **Vertical stacking**: Block boxes stack vertically
//! - **Full width**: By default, blocks stretch to fill containing block width
//! - **Margin collapsing**: Adjacent vertical margins collapse into one
//! - **Independent context**: Contents don't affect outside layout
//!
//! # Module Structure
//!
//! - `margin_collapse` - Margin collapsing algorithm (CSS 2.1 Section 8.3.1)
//! - `width` - Block width computation (CSS 2.1 Section 10.3.3)
//!
//! Reference: <https://www.w3.org/TR/CSS21/visuren.html#block-formatting>

pub mod margin_collapse;
pub mod width;

use crate::error::{RenderError, RenderStage};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::layout::axis::FragmentAxes;
use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::width::MarginValue;
use crate::layout::contexts::factory::FormattingContextFactory;
use crate::layout::contexts::inline::InlineFormattingContext;
use crate::layout::contexts::positioned::ContainingBlock;
use crate::layout::contexts::positioned::PositionedLayout;
use crate::layout::engine::LayoutParallelism;
use crate::layout::float_context::FloatContext;
use crate::layout::float_context::{resolve_clear_side, resolve_float_side, ClearSide, FloatSide};
use crate::layout::float_shape::build_float_shape;
use crate::layout::formatting_context::count_block_intrinsic_call;
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
use crate::layout::fragmentation::{
  clip_node_with_axes,
  collect_forced_boundaries_for_explicit_page_breaks_with_axes_and_page_progression,
  forces_break_between, normalize_fragment_margins_with_axes, propagate_fragment_metadata,
  propagate_fragmentainer_columns, ForcedBoundary, FragmentationAnalyzer, FragmentationContext,
};
use crate::layout::profile::layout_timer;
use crate::layout::profile::LayoutKind;
use crate::layout::utils::border_size_from_box_sizing;
use crate::layout::utils::compute_replaced_size;
use crate::layout::utils::content_size_from_box_sizing;
use crate::layout::utils::resolve_length_with_percentage_metrics;
use crate::layout::utils::resolve_length_with_percentage_metrics_and_root_font_metrics;
use crate::layout::utils::resolve_scrollbar_width;
use crate::render_control::{
  active_deadline, active_heartbeat, active_stage, check_active, check_active_periodic,
  with_deadline, StageGuard, StageHeartbeatGuard,
};
use crate::style::block_axis_is_horizontal;
use crate::style::block_axis_positive;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::float::Clear;
use crate::style::inline_axis_is_horizontal;
use crate::style::inline_axis_positive;
use crate::style::page::PageSide;
use crate::style::position::Position;
use crate::style::types::BorderStyle;
use crate::style::types::BreakBetween;
use crate::style::types::ColumnFill;
use crate::style::types::ColumnSpan;
use crate::style::types::Direction;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::Overflow;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::style::PhysicalSide;
use crate::style::RootFontMetrics;
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::AnonymousType;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::ReplacedBox;
use crate::tree::fragment_tree::BlockFragmentMetadata;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentationInfo;
use margin_collapse::establishes_bfc;
use margin_collapse::is_margin_collapsible_through;
use margin_collapse::should_collapse_with_first_child;
use margin_collapse::should_collapse_with_last_child;
use margin_collapse::CollapsibleMargin;
use margin_collapse::MarginCollapseContext;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use selectors::context::QuirksMode;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use width::compute_block_width;

#[cfg(test)]
thread_local! {
  static OVERFLOW_AUTO_CHILD_LAYOUT_PASSES: std::cell::Cell<usize> =
    const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_overflow_auto_child_layout_passes() {
  OVERFLOW_AUTO_CHILD_LAYOUT_PASSES.with(|cell| cell.set(0));
}

#[cfg(test)]
fn overflow_auto_child_layout_passes() -> usize {
  OVERFLOW_AUTO_CHILD_LAYOUT_PASSES.with(|cell| cell.get())
}

#[cfg(test)]
fn record_overflow_auto_child_layout_pass() {
  OVERFLOW_AUTO_CHILD_LAYOUT_PASSES.with(|cell| cell.set(cell.get() + 1));
}

#[cfg(test)]
thread_local! {
  static COLLAPSED_BLOCK_MARGINS_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
  static COLLAPSED_BLOCK_MARGINS_CALL_LIMIT: std::cell::Cell<Option<usize>> =
    const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn reset_collapsed_block_margins_call_tracking() {
  COLLAPSED_BLOCK_MARGINS_CALLS.with(|cell| cell.set(0));
  COLLAPSED_BLOCK_MARGINS_CALL_LIMIT.with(|cell| cell.set(None));
}

#[cfg(test)]
fn set_collapsed_block_margins_call_limit(limit: Option<usize>) {
  COLLAPSED_BLOCK_MARGINS_CALL_LIMIT.with(|cell| cell.set(limit));
}

#[cfg(test)]
fn collapsed_block_margins_calls() -> usize {
  COLLAPSED_BLOCK_MARGINS_CALLS.with(|cell| cell.get())
}

#[cfg(test)]
fn record_collapsed_block_margins_call() {
  COLLAPSED_BLOCK_MARGINS_CALLS.with(|count_cell| {
    let next = count_cell.get().saturating_add(1);
    count_cell.set(next);
    let limit = COLLAPSED_BLOCK_MARGINS_CALL_LIMIT.with(|limit_cell| limit_cell.get());
    if let Some(limit) = limit {
      assert!(
        next <= limit,
        "collapsed_block_margins call count exceeded limit ({} > {})",
        next,
        limit
      );
    }
  });
}

#[cfg(not(test))]
#[inline]
fn record_overflow_auto_child_layout_pass() {}

#[cfg(not(test))]
#[inline]
fn record_collapsed_block_margins_call() {}

fn is_ascii_whitespace_char(c: char) -> bool {
  matches!(
    c,
    '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'
  )
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_char)
}

fn is_ignorable_whitespace(node: &BoxNode) -> bool {
  // CSS 2.1 §8.3.1 / §9.4.2: A block can be considered "empty" for margin-collapsing purposes when
  // there is no in-flow content that generates line boxes. In HTML, inter-element whitespace
  // becomes text nodes that collapse away, but our box tree may wrap that text in anonymous inline
  // boxes. Treat anonymous inline wrappers containing only collapsible whitespace as ignorable so
  // empty blocks like `<p> </p>` collapse through correctly and whitespace-only runs don't advance
  // the block cursor (which would otherwise misplace subsequent floats).
  if matches!(
    node.style.white_space,
    crate::style::types::WhiteSpace::Pre | crate::style::types::WhiteSpace::PreWrap
  ) {
    return false;
  }

  match &node.box_type {
    BoxType::Text(text_box) => trim_ascii_whitespace(&text_box.text).is_empty(),
    BoxType::Anonymous(anon) if matches!(anon.anonymous_type, AnonymousType::Inline) => {
      !node.children.is_empty() && node.children.iter().all(is_ignorable_whitespace)
    }
    _ => false,
  }
}

fn is_collapsible_whitespace_run(node: &BoxNode) -> bool {
  is_ignorable_whitespace(node)
}

fn abspos_depends_on_cb_block_size(style: &crate::style::ComputedStyle) -> bool {
  use crate::style::types::InsetValue;

  let inset_has_percentage = |value: &InsetValue| match value {
    InsetValue::Length(length) => length.has_percentage(),
    InsetValue::Auto | InsetValue::Anchor(_) => false,
  };

  // Percentage heights/vertical insets resolve against the containing block's used height for
  // absolutely positioned elements (CSS 2.1 §10.5/§10.6.4).
  if style.height.is_some_and(|l| l.has_percentage())
    || style.min_height.is_some_and(|l| l.has_percentage())
    || style.max_height.is_some_and(|l| l.has_percentage())
    || inset_has_percentage(&style.top)
    || inset_has_percentage(&style.bottom)
  {
    return true;
  }

  // Intrinsic sizing keywords can also depend on the available block size (e.g.
  // `height: fill-available`), which is defined in terms of the containing block height.
  if style.height_keyword.is_some()
    || style.min_height_keyword.is_some()
    || style.max_height_keyword.is_some()
  {
    return true;
  }

  // Non-percentage insets can still require the containing block height for the absolute
  // positioning constraint equation. In particular, `bottom: 0` (or `top/bottom` with
  // `height:auto`) needs the *used* CB height to compute the block-start position and/or
  // shrink-to-fit size. If the CB is `height:auto`, that used height isn't known until after
  // in-flow layout.
  let top_is_auto = matches!(style.top, InsetValue::Auto);
  let bottom_is_auto = matches!(style.bottom, InsetValue::Auto);
  let top_specified = !top_is_auto;
  let bottom_specified = !bottom_is_auto;

  // Any specified `bottom` requires the CB height for one or more of:
  // - resolving `top:auto` from the constraint equation (most common),
  // - overconstraint handling when both insets are present,
  // - auto margin distribution.
  if bottom_specified {
    return true;
  }

  // When height is auto and a vertical inset is specified, the CSS2.1 algorithm shrink-to-fits the
  // used height against the available CB height (CSS 2.1 §10.6.4).
  let height_is_auto = style.height.is_none();
  height_is_auto && top_specified
}

fn has_abspos_descendant_needing_used_cb_height(root: &BoxNode) -> bool {
  fn walk(node: &BoxNode, under_inner_cb: bool, depth: usize) -> bool {
    let style = node.style.as_ref();
    let establishes_cb = style.establishes_abs_containing_block();

    if matches!(style.position, Position::Absolute)
      && !under_inner_cb
      && abspos_depends_on_cb_block_size(style)
    {
      // Block-level abspos children are collected by the block formatting context and laid out
      // after the containing block's used height is known, so they do not require relayout here.
      let is_direct_block_level_child = depth == 1 && !node.original_display.is_inline_level();
      if !is_direct_block_level_child {
        return true;
      }
    }

    // Once we're inside another abs containing block, descendants will resolve percentages (and
    // bottom/top constraint equations) against that inner block instead of the current one, so the
    // current block doesn't need to rerun layout for them.
    if under_inner_cb || establishes_cb {
      return false;
    }

    for child in &node.children {
      if walk(child, false, depth + 1) {
        return true;
      }
    }
    false
  }

  for child in &root.children {
    if walk(child, false, 1) {
      return true;
    }
  }
  false
}

#[derive(Clone)]
struct PositionedCandidate {
  node: BoxNode,
  source: ContainingBlockSource,
  static_position: Option<Point>,
  query_parent_id: usize,
  implicit_anchor_box_id: Option<usize>,
}

#[derive(Clone)]
enum ContainingBlockSource {
  ParentPadding,
  Explicit(ContainingBlock),
}

#[derive(Clone, Copy, Debug, Default)]
struct CollapsedBlockMargins {
  top: CollapsibleMargin,
  bottom: CollapsibleMargin,
  /// True when the box is empty for margin collapsing and its own block-start/block-end margins
  /// collapse together (CSS 2.1 §8.3.1).
  collapsible_through: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum CollapsedMarginMode {
  #[default]
  Normal,
  /// Quirks-mode behavior for the direct child of the root element: when collapsing with its
  /// descendants at the edges, user-agent default margins are ignored (Chrome compat).
  QuirksRootChild,
  /// Recursive helper for `QuirksRootChild`: treat UA margins on this subtree as zero so they do
  /// not contribute to the collapsed margin chain.
  QuirksSuppressUserAgent,
}

fn axis_sides(horizontal: bool, positive: bool) -> (PhysicalSide, PhysicalSide) {
  match (horizontal, positive) {
    (true, true) => (PhysicalSide::Left, PhysicalSide::Right),
    (true, false) => (PhysicalSide::Right, PhysicalSide::Left),
    (false, true) => (PhysicalSide::Top, PhysicalSide::Bottom),
    (false, false) => (PhysicalSide::Bottom, PhysicalSide::Top),
  }
}

fn inline_axis_sides(style: &ComputedStyle) -> (PhysicalSide, PhysicalSide) {
  if inline_axis_is_horizontal(style.writing_mode) {
    (PhysicalSide::Left, PhysicalSide::Right)
  } else {
    (PhysicalSide::Top, PhysicalSide::Bottom)
  }
}

fn block_axis_sides(style: &ComputedStyle) -> (PhysicalSide, PhysicalSide) {
  axis_sides(
    block_axis_is_horizontal(style.writing_mode),
    block_axis_positive(style.writing_mode),
  )
}

fn paint_viewport_for(
  writing_mode: WritingMode,
  _direction: Direction,
  viewport_size: Size,
) -> Rect {
  let inline_size = if inline_axis_is_horizontal(writing_mode) {
    viewport_size.width
  } else {
    viewport_size.height
  };
  let block_size = if inline_axis_is_horizontal(writing_mode) {
    viewport_size.height
  } else {
    viewport_size.width
  };
  Rect::from_xywh(0.0, 0.0, inline_size, block_size)
}

fn translate_containing_block(cb: ContainingBlock, delta: Point) -> ContainingBlock {
  if delta == Point::ZERO {
    return cb;
  }
  cb.translate(delta)
}

/// Block Formatting Context implementation
///
/// Implements the FormattingContext trait for block-level layout.
/// Handles vertical stacking, margin collapsing, and width computation.
#[derive(Clone)]
pub struct BlockFormattingContext {
  /// Shared factory used to create child formatting contexts without losing shared caches.
  factory: FormattingContextFactory,
  /// Shared inline formatting context used for intrinsic sizing (and for inline child layout when
  /// the nearest positioned containing block matches this block context's).
  ///
  /// This avoids rebuilding inline contexts (and their hyphenator/pipeline wiring) in hot loops.
  intrinsic_inline_fc: Arc<InlineFormattingContext>,
  font_context: FontContext,
  viewport_size: crate::geometry::Size,
  nearest_positioned_cb: ContainingBlock,
  nearest_fixed_cb: ContainingBlock,
  /// When true, treat the root box as a flex item for width resolution (auto margins resolve to
  /// 0 and specified margins stay fixed instead of being rebalanced to satisfy the block width
  /// equation). This is only meant for the flex-item root; descendants revert to normal block
  /// behavior.
  flex_item_mode: bool,
  /// When true, treat the root box as establishing an independent formatting context for margin
  /// collapsing (CSS Display 3), preventing parent/child vertical margin collapsing across the
  /// box boundary.
  independent_context_root_mode: bool,
  /// When set, treat the box with this id as establishing an independent formatting context for
  /// margin-collapsing purposes.
  ///
  /// This is threaded into descendant block contexts so `layout_flow_children` can tell when it is
  /// laying out children of an independent formatting context root.
  independent_context_root_id: Option<usize>,
  /// When true, avoid falling back to the viewport size when the containing block inline size is
  /// near-zero.
  ///
  /// Block layout includes a safeguard that replaces collapsed containing widths (≤ 1px) with the
  /// viewport inline size. This keeps percentage-based sizing usable when flex/grid measurement
  /// passes a 0px constraint during real layout.
  ///
  /// Intrinsic sizing probes (e.g. min-/max-content block sizes used by grid/flex track sizing)
  /// intentionally treat percentage bases as 0px; using the viewport fallback inflates intrinsic
  /// measurements (notably for aspect-ratio replaced content sized with `width: 100%`).
  suppress_near_zero_width_viewport_fallback: bool,
  parallelism: LayoutParallelism,
}

impl BlockFormattingContext {
  /// Creates a new BlockFormattingContext
  pub fn new() -> Self {
    let viewport = crate::geometry::Size::new(800.0, 600.0);
    Self::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    )
  }

  /// Creates a BlockFormattingContext backed by a specific font context so text
  /// measurement shares caches with the caller.
  pub fn with_font_context(font_context: FontContext) -> Self {
    let viewport = crate::geometry::Size::new(800.0, 600.0);
    Self::with_font_context_viewport_and_cb(
      font_context,
      viewport,
      ContainingBlock::viewport(viewport),
    )
  }

  pub fn with_font_context_and_viewport(
    font_context: FontContext,
    viewport_size: crate::geometry::Size,
  ) -> Self {
    let cb = ContainingBlock::viewport(viewport_size);
    Self::with_font_context_viewport_and_cb(font_context, viewport_size, cb)
  }

  pub fn with_font_context_viewport_and_cb(
    font_context: FontContext,
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: ContainingBlock,
  ) -> Self {
    let factory =
      FormattingContextFactory::with_font_context_and_viewport(font_context, viewport_size)
        .with_positioned_cb(nearest_positioned_cb);
    Self::with_factory(factory)
  }

  pub fn with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let parallelism = factory.parallelism();
    let intrinsic_inline_fc = Arc::new(InlineFormattingContext::with_factory(factory.clone()));
    Self {
      factory,
      intrinsic_inline_fc,
      font_context,
      viewport_size,
      nearest_positioned_cb,
      nearest_fixed_cb,
      flex_item_mode: false,
      independent_context_root_mode: false,
      independent_context_root_id: None,
      suppress_near_zero_width_viewport_fallback: false,
      parallelism,
    }
  }

  /// Creates a BlockFormattingContext configured for laying out a flex item root. Margin
  /// resolution follows the flexbox hypothetical size rules (auto margins → 0; specified margins
  /// remain as authored).
  pub fn for_flex_item_with_font_context_viewport_and_cb(
    font_context: FontContext,
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: ContainingBlock,
  ) -> Self {
    let factory =
      FormattingContextFactory::with_font_context_and_viewport(font_context, viewport_size)
        .with_positioned_cb(nearest_positioned_cb);
    Self::for_flex_item_with_factory(factory)
  }

  pub fn for_flex_item_with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let parallelism = factory.parallelism();
    let intrinsic_inline_fc = Arc::new(InlineFormattingContext::with_factory(factory.clone()));
    Self {
      factory,
      intrinsic_inline_fc,
      font_context,
      viewport_size,
      nearest_positioned_cb,
      nearest_fixed_cb,
      flex_item_mode: true,
      independent_context_root_mode: true,
      independent_context_root_id: None,
      suppress_near_zero_width_viewport_fallback: false,
      parallelism,
    }
  }

  pub fn for_independent_context_root_with_factory(factory: FormattingContextFactory) -> Self {
    let mut ctx = Self::with_factory(factory);
    ctx.independent_context_root_mode = true;
    ctx
  }

  pub fn with_parallelism(mut self, parallelism: LayoutParallelism) -> Self {
    self.parallelism = parallelism;
    self.factory = self.factory.clone().with_parallelism(parallelism);
    self.intrinsic_inline_fc =
      Arc::new(InlineFormattingContext::with_factory(self.factory.clone()));
    self
  }

  fn child_factory(&self) -> FormattingContextFactory {
    self.factory.clone()
  }

  fn child_factory_for_cb(&self, cb: ContainingBlock) -> FormattingContextFactory {
    if cb == self.nearest_positioned_cb {
      self.child_factory()
    } else {
      self.factory.with_positioned_cb(cb)
    }
  }

  fn intrinsic_inline_content_sizes_for_sizing_keywords(
    &self,
    node: &BoxNode,
    fc_type: FormattingContextType,
    factory: &FormattingContextFactory,
  ) -> Result<(f32, f32), LayoutError> {
    let style_override = crate::layout::style_override::style_override_for(node.id);
    let base_style = style_override.clone().unwrap_or_else(|| node.style.clone());
    let style: &ComputedStyle = base_style.as_ref();
    let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);
    let root_font_metrics = factory.root_font_metrics();
    let intrinsic_edges = if inline_is_horizontal {
      horizontal_padding_and_borders(
        style,
        0.0,
        self.viewport_size,
        &self.font_context,
        root_font_metrics,
      )
    } else {
      vertical_padding_and_borders(
        style,
        0.0,
        self.viewport_size,
        &self.font_context,
        root_font_metrics,
      )
    };
    let compute = || {
      let mut override_style = base_style.clone();
      {
        let s = Arc::make_mut(&mut override_style);
        s.width = None;
        s.width_keyword = None;
        s.min_width = None;
        s.min_width_keyword = None;
        s.max_width = None;
        s.max_width_keyword = None;
      }

      if node.id != 0 {
        crate::layout::style_override::with_style_override(node.id, override_style, || {
          if fc_type == FormattingContextType::Block {
            self.compute_intrinsic_inline_sizes(node)
          } else {
            factory.get(fc_type).compute_intrinsic_inline_sizes(node)
          }
        })
      } else {
        let mut cloned = node.clone();
        cloned.style = override_style;
        if fc_type == FormattingContextType::Block {
          self.compute_intrinsic_inline_sizes(&cloned)
        } else {
          factory.get(fc_type).compute_intrinsic_inline_sizes(&cloned)
        }
      }
    };
    let (min_border, max_border) = compute()?;
    Ok((
      (min_border - intrinsic_edges).max(0.0),
      (max_border - intrinsic_edges).max(0.0),
    ))
  }

  fn resolve_intrinsic_size_keyword_to_content_width(
    &self,
    keyword: IntrinsicSizeKeyword,
    min_content: f32,
    max_content: f32,
    available_content: f32,
    containing_width: f32,
    style: &ComputedStyle,
    inline_edges: f32,
  ) -> f32 {
    // The intrinsic sizing keywords (`min-content`, `max-content`, `fit-content(...)`) are defined
    // in terms of the element's intrinsic *border-box* sizes. Internally we carry content-box
    // dimensions in most of the block formatting context, so convert to border-box for the clamp
    // and then back to content-box at the end. This also naturally rebases percentage padding
    // because `min_content`/`max_content` are computed with a 0px percentage base.
    let min_border = min_content + inline_edges;
    let max_border = max_content + inline_edges;
    let available_border = available_content + inline_edges;
    let root_font_metrics = self.factory.root_font_metrics();

    let used_border = match keyword {
      IntrinsicSizeKeyword::MinContent => min_border,
      IntrinsicSizeKeyword::MaxContent => max_border,
      IntrinsicSizeKeyword::FillAvailable => available_border,
      IntrinsicSizeKeyword::FitContent { limit } => match limit {
        None => max_border.min(available_border.max(min_border)),
        Some(limit) => {
          let limit_border = resolve_length_for_width(
            limit,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          );
          let limit_border =
            border_size_from_box_sizing(limit_border, inline_edges, style.box_sizing);
          max_border.min(limit_border.max(min_border))
        }
      },
      IntrinsicSizeKeyword::CalcSize(calc) => {
        use crate::style::types::CalcSizeBasis;

        let basis_border = match calc.basis {
          CalcSizeBasis::Auto => match style.box_sizing {
            crate::style::types::BoxSizing::ContentBox => available_content + inline_edges,
            crate::style::types::BoxSizing::BorderBox => available_border,
          },
          CalcSizeBasis::MinContent => min_border,
          CalcSizeBasis::MaxContent => max_border,
          CalcSizeBasis::FillAvailable => available_border,
          CalcSizeBasis::FitContent { limit } => match limit {
            None => max_border.min(available_border.max(min_border)),
            Some(limit) => {
              let limit_border = resolve_length_for_width(
                limit,
                containing_width,
                style,
                &self.font_context,
                self.viewport_size,
                root_font_metrics,
              );
              let limit_border =
                border_size_from_box_sizing(limit_border, inline_edges, style.box_sizing);
              max_border.min(limit_border.max(min_border))
            }
          },
          CalcSizeBasis::Length(len) => {
            let specified = resolve_length_for_width(
              len,
              containing_width,
              style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            );
            border_size_from_box_sizing(specified, inline_edges, style.box_sizing)
          }
        };
        let basis_border = basis_border.max(0.0);
        let basis_content = (basis_border - inline_edges).max(0.0);
        let basis_specified = match style.box_sizing {
          crate::style::types::BoxSizing::ContentBox => basis_content,
          crate::style::types::BoxSizing::BorderBox => basis_border,
        };

        let resolved_border =
          crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
            .and_then(|expr_sum| crate::css::properties::parse_length(&format!("calc({expr_sum})")))
            .map(|expr_len| {
              let resolved_specified = resolve_length_for_width(
                expr_len,
                containing_width,
                style,
                &self.font_context,
                self.viewport_size,
                root_font_metrics,
              );
              border_size_from_box_sizing(resolved_specified, inline_edges, style.box_sizing)
                .max(0.0)
            })
            .unwrap_or(basis_border);
        resolved_border
      }
    };

    (used_border - inline_edges).max(0.0)
  }

  fn child_factory_for_cbs(
    &self,
    positioned_cb: ContainingBlock,
    fixed_cb: ContainingBlock,
  ) -> FormattingContextFactory {
    let factory = self.child_factory_for_cb(positioned_cb);
    if fixed_cb == factory.nearest_fixed_cb() {
      factory
    } else {
      factory.with_fixed_cb(fixed_cb)
    }
  }

  fn maybe_attach_footnote_anchor(
    &self,
    child: &BoxNode,
    containing_width: f32,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    fragment: &mut FragmentNode,
  ) -> Result<(), LayoutError> {
    let Some(body) = child.footnote_body.as_deref() else {
      return Ok(());
    };

    let snapshot_node = body.clone();
    let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
    let fc_type = snapshot_node.formatting_context().unwrap_or_else(|| {
      if snapshot_node.is_block_level() {
        FormattingContextType::Block
      } else {
        FormattingContextType::Inline
      }
    });
    let fc = factory.get(fc_type);
    let snapshot_constraints = LayoutConstraints::new(
      AvailableSpace::Definite(containing_width.max(0.0)),
      AvailableSpace::Indefinite,
    );
    let snapshot_fragment = fc.layout(&snapshot_node, &snapshot_constraints)?;
    let anchor_bounds = Rect::from_xywh(0.0, 0.0, 0.0, 0.01);
    let mut anchor = FragmentNode::new_footnote_anchor(
      anchor_bounds,
      snapshot_fragment,
      body.style.footnote_policy,
    );
    anchor.style = Some(child.style.clone());
    fragment.children_mut().push(anchor);
    Ok(())
  }

  /// Lays out a single block-level child and returns its fragment
  #[allow(clippy::cognitive_complexity)]
  fn layout_block_child(
    &self,
    parent: &BoxNode,
    child: &BoxNode,
    containing_width: f32,
    constraints: &LayoutConstraints,
    box_y: f32,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    external_float_ctx: Option<&mut FloatContext>,
    external_float_base_x: f32,
    external_float_base_y: f32,
    paint_viewport: Rect,
  ) -> Result<FragmentNode, LayoutError> {
    if crate::layout::auto_scrollbars::should_bypass(child) {
      let parent_offset = crate::layout::formatting_context::fragmentainer_block_offset_hint();
      let parent_offset = if parent_offset.is_finite() {
        parent_offset
      } else {
        0.0
      };
      let child_offset = if box_y.is_finite() { box_y } else { 0.0 };
      let _fragmentainer_offset_guard =
        crate::layout::formatting_context::fragmentainer_block_size_hint()
          .filter(|size| size.is_finite() && *size > 0.0)
          .map(|_| {
            crate::layout::formatting_context::set_fragmentainer_block_offset_hint(
              parent_offset + child_offset,
            )
          });
      let toggles = crate::debug::runtime::runtime_toggles();
      let dump_child_y = toggles.truthy("FASTR_DUMP_CELL_CHILD_Y");
      let log_wide_flex = toggles.truthy("FASTR_LOG_WIDE_FLEX");
      if let BoxType::Replaced(replaced_box) = &child.box_type {
        let mut fragment = self.layout_replaced_child(
          parent,
          child,
          replaced_box,
          containing_width,
          constraints,
          box_y,
          nearest_positioned_cb,
          nearest_fixed_cb,
        )?;
        self.maybe_attach_footnote_anchor(
          child,
          containing_width,
          nearest_positioned_cb,
          nearest_fixed_cb,
          &mut fragment,
        )?;
        return Ok(fragment);
      }

      let style_override = crate::layout::style_override::style_override_for(child.id);
      let style_arc = style_override.unwrap_or_else(|| child.style.clone());
      let style = style_arc.as_ref();
      let font_size = style.font_size; // Get font-size for resolving em units
      let root_font_metrics = self.factory.root_font_metrics();
      let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);
      // Map physical width/height inputs to the logical inline/block axes used by the block
      // formatting context.
      let (
        inline_length,
        inline_keyword,
        min_inline_length,
        min_inline_keyword,
        max_inline_length,
        max_inline_keyword,
      ) = if inline_is_horizontal {
        (
          style.width,
          style.width_keyword,
          style.min_width,
          style.min_width_keyword,
          style.max_width,
          style.max_width_keyword,
        )
      } else {
        (
          style.height,
          style.height_keyword,
          style.min_height,
          style.min_height_keyword,
          style.max_height,
          style.max_height_keyword,
        )
      };
      let (block_length, block_keyword, min_block_keyword, max_block_keyword) =
        if inline_is_horizontal {
          (
            style.height,
            style.height_keyword,
            style.min_height_keyword,
            style.max_height_keyword,
          )
        } else {
          (
            style.width,
            style.width_keyword,
            style.min_width_keyword,
            style.max_width_keyword,
          )
        };
      // CSS Sizing L3: min-/max-content keywords on the *block-size* axis behave like `auto`
      // (min-content block size is equivalent to max-content block size for block containers / inline
      // boxes). Treat them as `auto` so we don't force a "min-content inline-size" reflow to measure
      // height, which can spuriously inflate the used block-size due to extra wrapping.
      let block_keyword = match block_keyword {
        Some(crate::style::types::IntrinsicSizeKeyword::MinContent)
        | Some(crate::style::types::IntrinsicSizeKeyword::MaxContent) => None,
        other => other,
      };
      // Percentage block sizes resolve against the containing block's *definite* block-size
      // (CSS2.1 §10.5). Thread this through via `LayoutConstraints::block_percentage_base` so we can
      // keep the block-axis available space indefinite (in-flow content is allowed to overflow)
      // without incorrectly resolving percentages against an ancestor's available height (e.g. the
      // viewport).
      let containing_height = if inline_is_horizontal {
        constraints.height()
      } else {
        constraints.width()
      };
      // CSS2.1 §10.5: Percentage `height` values on in-flow elements compute to `auto` when the
      // containing block's height depends on content (i.e. it is not specified explicitly).
      //
      // Prefer an explicit percentage base provided by the parent layout pass (e.g. specified
      // height, flex/grid-used size, absolute-positioning inset sizing). This allows block layout to
      // carry a definite percentage base even when `available_height` is indefinite (the common
      // block-flow case), and avoids incorrectly resolving `height:100%` against a definite
      // *available* height inherited from an ancestor (e.g. viewport).
      let containing_height_for_percentages = constraints.block_percentage_base.or_else(|| {
        // Only allow percentage heights to resolve when this block container has a definite block
        // size, or when the parent layout algorithm has already forced a used border-box size.
        //
        // Percentage block sizes on in-flow children resolve against the used size of the containing
        // block’s content box (CSS2.1 §10.5). This matters even when the parent itself was laid out
        // with an indefinite available height (the common block-flow case): a fixed-height parent
        // like `height: 28px` must still provide a definite percentage base for descendants
        // (`height:100%`) even though its own *available* height is `auto`.
        let parent_block_size_length = if inline_is_horizontal {
          parent.style.height
        } else {
          parent.style.width
        };
        let parent_block_size_keyword = if inline_is_horizontal {
          parent.style.height_keyword
        } else {
          parent.style.width_keyword
        };
        let parent_block_size_is_auto =
          parent_block_size_length.is_none() && parent_block_size_keyword.is_none();
        // When the parent formatting context already computed a final used border-box size for this
        // containing block (e.g. flex/grid items or absolute positioning relayout), use that as the
        // percentage basis after converting it to a content-box size.
        let parent_used_border_box_block_size =
          if constraints.used_border_box_size_forces_block_percentage_base {
            if inline_is_horizontal {
              constraints.used_border_box_height
            } else {
              constraints.used_border_box_width
            }
          } else {
            None
          };

        // `aspect-ratio` can establish a definite used size even when the corresponding block-size
        // property is `auto`. Treat that ratio-derived size as a valid percentage basis so common
        // patterns like `aspect-ratio` + `height:100%` descendants don't collapse to 0px.
        let parent_aspect_ratio_content_block_size =
          if parent_block_size_is_auto && parent_used_border_box_block_size.is_none() {
            match parent.style.aspect_ratio {
              crate::style::types::AspectRatio::Ratio(ratio)
              | crate::style::types::AspectRatio::AutoRatio(ratio) => {
                if ratio > 0.0 && containing_width.is_finite() {
                  let raw = if inline_is_horizontal {
                    containing_width / ratio
                  } else {
                    containing_width * ratio
                  };
                  raw.is_finite().then_some(raw.max(0.0))
                } else {
                  None
                }
              }
              crate::style::types::AspectRatio::Auto => None,
            }
          } else {
            None
          };

        if parent_block_size_is_auto
          && parent_used_border_box_block_size.is_none()
          && parent_aspect_ratio_content_block_size.is_none()
        {
          return None;
        }

        let parent_axis_edges = if inline_is_horizontal {
          vertical_padding_and_borders(
            &parent.style,
            containing_width,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          )
        } else {
          horizontal_padding_and_borders(
            &parent.style,
            containing_width,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          )
        };
        let parent_content_block_size = if let Some(border_box) = parent_used_border_box_block_size
        {
          Some((border_box - parent_axis_edges).max(0.0))
        } else if let Some(block_len) = parent_block_size_length {
          resolve_length_with_percentage_metrics(
            block_len,
            containing_height,
            self.viewport_size,
            parent.style.font_size,
            parent.style.root_font_size,
            Some(&parent.style),
            Some(&self.font_context),
          )
          .map(|resolved| {
            let mut content =
              content_size_from_box_sizing(resolved, parent_axis_edges, parent.style.box_sizing);
            // Stable scrollbar gutters consume space from the content box even when
            // `box-sizing: content-box` (mirrors the adjustment in this function when computing
            // the parent's own used block size).
            if parent.style.box_sizing == crate::style::types::BoxSizing::ContentBox {
              let reserve_gutter = if inline_is_horizontal {
                parent.style.scrollbar_gutter.stable
                  && matches!(
                    parent.style.overflow_x,
                    Overflow::Hidden | Overflow::Auto | Overflow::Scroll
                  )
              } else {
                parent.style.scrollbar_gutter.stable
                  && matches!(
                    parent.style.overflow_y,
                    Overflow::Hidden | Overflow::Auto | Overflow::Scroll
                  )
              };
              if reserve_gutter {
                let gutter = resolve_scrollbar_width(&parent.style);
                if gutter > 0.0 {
                  let delta = if parent.style.scrollbar_gutter.both_edges {
                    gutter * 2.0
                  } else {
                    gutter
                  };
                  content = (content - delta).max(0.0);
                }
              }
            }
            content.max(0.0)
          })
        } else {
          parent_aspect_ratio_content_block_size
        };
        parent_content_block_size.filter(|value: &f32| value.is_finite())
      });

      // Handle block-axis margins (resolve em/rem units with font-size)
      let block_sides = block_axis_sides(style);
      let margin_top = resolve_margin_side(
        style,
        block_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let margin_bottom = resolve_margin_side(
        style,
        block_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      if dump_child_y && matches!(child.style.display, Display::Table) {
        eprintln!(
          "block child margins: display={:?} margin_top={:.2} box_y={:.2}",
          child.style.display, margin_top, box_y
        );
      }

      // Pre-resolve vertical edges so box-sizing and used-size overrides can convert border-box sizes
      // into content sizes before laying out descendants.
      let border_top = resolve_border_side(
        style,
        block_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let border_bottom = resolve_border_side(
        style,
        block_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let mut padding_top = resolve_padding_side(
        style,
        block_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let mut padding_bottom = resolve_padding_side(
        style,
        block_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let reserve_horizontal_gutter = style.scrollbar_gutter.stable
        && matches!(
          style.overflow_x,
          Overflow::Hidden | Overflow::Auto | Overflow::Scroll
        );
      let mut reserved_horizontal_gutter = 0.0;
      if reserve_horizontal_gutter {
        let gutter = resolve_scrollbar_width(style);
        if gutter > 0.0 {
          if style.scrollbar_gutter.both_edges {
            padding_top += gutter;
            reserved_horizontal_gutter += gutter;
          }
          padding_bottom += gutter;
          reserved_horizontal_gutter += gutter;
        }
      }
      let vertical_edges = border_top + padding_top + padding_bottom + border_bottom;

      // Create constraints for child layout.
      let block_keyword_is_content_based = inline_is_horizontal
        && matches!(
          block_keyword,
          Some(IntrinsicSizeKeyword::MinContent | IntrinsicSizeKeyword::MaxContent)
        );
      let height_auto =
        block_length.is_none() && (block_keyword.is_none() || block_keyword_is_content_based);
      let available_block_border_box = containing_height
        .map(|h| (h - margin_top - margin_bottom).max(0.0))
        .unwrap_or(f32::INFINITY);

      let intrinsic_block_sizes = if (block_keyword.is_some() && !block_keyword_is_content_based)
        || min_block_keyword.is_some()
        || max_block_keyword.is_some()
      {
        let factory = self.child_factory_for_cb(*nearest_positioned_cb);
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let (min_base0, max_base0) = if fc_type == FormattingContextType::Block {
          compute_intrinsic_block_sizes_without_block_size_constraints(self, child)?
        } else {
          let fc = factory.get(fc_type);
          compute_intrinsic_block_sizes_without_block_size_constraints(fc.as_ref(), child)?
        };

        let border_top_base0 = resolve_border_side(
          style,
          block_sides.0,
          0.0,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let border_bottom_base0 = resolve_border_side(
          style,
          block_sides.1,
          0.0,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let mut padding_top_base0 = resolve_padding_side(
          style,
          block_sides.0,
          0.0,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let mut padding_bottom_base0 = resolve_padding_side(
          style,
          block_sides.1,
          0.0,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        if reserve_horizontal_gutter {
          let gutter = resolve_scrollbar_width(style);
          if style.scrollbar_gutter.both_edges {
            padding_top_base0 += gutter;
          }
          padding_bottom_base0 += gutter;
        }
        let vertical_edges_base0 =
          border_top_base0 + padding_top_base0 + padding_bottom_base0 + border_bottom_base0;
        Some((
          rebase_intrinsic_border_box_size(min_base0, vertical_edges_base0, vertical_edges),
          rebase_intrinsic_border_box_size(max_base0, vertical_edges_base0, vertical_edges),
        ))
      } else {
        None
      };

      let mut specified_height = block_length.and_then(|h| {
        resolve_length_with_percentage_metrics_and_root_font_metrics(
          h,
          containing_height_for_percentages,
          self.viewport_size,
          font_size,
          style.root_font_size,
          Some(style),
          Some(&self.font_context),
          self.factory.root_font_metrics(),
        )
      });
      // Track whether the block-size we computed should establish a definite percentage base for
      // descendants (CSS2.1 §10.5).
      //
      // Intrinsic sizing keywords like `min-content` depend on the element's own contents and must
      // *not* be treated as definite bases for percentage heights, otherwise common patterns like
      // `height: min-content` + `img { height: 100% }` create circular dependencies.
      let mut block_size_definite_for_percentages = specified_height.is_some();
      specified_height =
        specified_height.map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing));
      if reserved_horizontal_gutter > 0.0
        && block_length.is_some()
        && style.box_sizing == crate::style::types::BoxSizing::ContentBox
      {
        specified_height = specified_height.map(|h| (h - reserved_horizontal_gutter).max(0.0));
      }
      if let Some(height_keyword) = block_keyword {
        if !block_keyword_is_content_based {
          let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
          let used_border_box = match height_keyword {
            crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
            crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
              if available_block_border_box.is_finite() {
                available_block_border_box
              } else {
                intrinsic_max
              }
            }
            crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
              let preferred_border = limit.and_then(|limit| {
                resolve_length_with_percentage_metrics_and_root_font_metrics(
                  limit,
                  containing_height_for_percentages,
                  self.viewport_size,
                  font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                  self.factory.root_font_metrics(),
                )
                .map(|resolved| {
                  border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                })
              });
              crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
                Some(available_block_border_box),
                preferred_border,
                intrinsic_min,
                intrinsic_max,
              )
            }
            crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
              use crate::style::types::BoxSizing;
              use crate::style::types::CalcSizeBasis;
              let basis_border = match calc.basis {
                CalcSizeBasis::Auto => intrinsic_max,
                CalcSizeBasis::MinContent => intrinsic_min,
                CalcSizeBasis::MaxContent => intrinsic_max,
                CalcSizeBasis::FillAvailable => {
                  if available_block_border_box.is_finite() {
                    available_block_border_box
                  } else {
                    intrinsic_max
                  }
                }
                CalcSizeBasis::FitContent { limit } => {
                  let basis_border = match limit {
                    Some(limit) => resolve_length_with_percentage_metrics(
                      limit,
                      containing_height_for_percentages,
                      self.viewport_size,
                      font_size,
                      style.root_font_size,
                      Some(style),
                      Some(&self.font_context),
                    )
                    .map(|resolved| {
                      border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                    })
                    .unwrap_or(f32::INFINITY),
                    None => available_block_border_box,
                  };
                  crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
                }
                CalcSizeBasis::Length(len) => resolve_length_with_percentage_metrics(
                  len,
                  containing_height_for_percentages,
                  self.viewport_size,
                  font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                )
                .map(|resolved| {
                  border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                })
                .unwrap_or(intrinsic_max),
              }
              .max(0.0);
              let basis_content = (basis_border - vertical_edges).max(0.0);
              let basis_specified = match style.box_sizing {
                BoxSizing::ContentBox => basis_content,
                BoxSizing::BorderBox => basis_border,
              };
              crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
                .and_then(|expr_sum| {
                  crate::css::properties::parse_length(&format!("calc({expr_sum})"))
                })
                .and_then(|expr_len| {
                  resolve_length_with_percentage_metrics(
                    expr_len,
                    containing_height_for_percentages,
                    self.viewport_size,
                    font_size,
                    style.root_font_size,
                    Some(style),
                    Some(&self.font_context),
                  )
                })
                .map(|resolved_specified| {
                  border_size_from_box_sizing(resolved_specified, vertical_edges, style.box_sizing)
                    .max(0.0)
                })
                .unwrap_or(basis_border)
            }
          };
          specified_height = Some((used_border_box - vertical_edges).max(0.0));
          block_size_definite_for_percentages = matches!(
            height_keyword,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable
          ) && available_block_border_box.is_finite();
        }
      }
      // `LayoutConstraints::used_border_box_*` describes the containing block's own used size (as
      // computed by its parent formatting context). It is not a sizing constraint for this in-flow
      // child: block-level in-flow boxes can overflow a definite-height containing block (CSS2.1
      // §10.6.3), and should not be forced to fill its used block size.
      if specified_height.is_none()
        && height_auto
        && block_axis_is_horizontal(style.writing_mode)
        && available_block_border_box.is_finite()
      {
        // When flowing in a vertical/sideways writing mode, the block axis maps to the physical
        // x-axis. `width: auto` should therefore stretch to fill the available block size of the
        // containing block (mirroring the auto-width behavior for block boxes in horizontal writing
        // modes) instead of collapsing to the content size.
        specified_height = Some((available_block_border_box - vertical_edges).max(0.0));
        block_size_definite_for_percentages = true;
      }
      // Compute inline size using CSS 2.1 Section 10.3.3 algorithm
      let inline_sides = inline_axis_sides(style);
      let inline_positive = inline_axis_positive(style.writing_mode, style.direction);
      let mut computed_width = compute_block_width(
        style,
        containing_width,
        self.viewport_size,
        root_font_metrics,
        inline_sides,
        inline_positive,
        &self.font_context,
      );
      let width_auto = inline_length.is_none() && inline_keyword.is_none();
      let inline_edges_for_fit = computed_width.border_left
        + computed_width.padding_left
        + computed_width.padding_right
        + computed_width.border_right;
      let available_inline_border_box = (containing_width
        - resolve_margin_side(
          style,
          inline_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        )
        - resolve_margin_side(
          style,
          inline_sides.1,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        ))
      .max(0.0);
      let available_content_for_fit = (available_inline_border_box - inline_edges_for_fit).max(0.0);
      let mut intrinsic_content_sizes = None;
      if inline_length.is_none() {
        if let Some(keyword) = inline_keyword {
          let factory = self.child_factory_for_cb(*nearest_positioned_cb);
          let fc_type = child
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let (min_content, max_content) =
            self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?;
          intrinsic_content_sizes = Some((min_content, max_content));
          let keyword_content = self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            style,
            inline_edges_for_fit,
          );
          let specified_width = match style.box_sizing {
            crate::style::types::BoxSizing::ContentBox => keyword_content,
            crate::style::types::BoxSizing::BorderBox => keyword_content + inline_edges_for_fit,
          };
          let mut width_style = style.clone();
          if inline_is_horizontal {
            width_style.width = Some(Length::px(specified_width));
            width_style.width_keyword = None;
          } else {
            width_style.height = Some(Length::px(specified_width));
            width_style.height_keyword = None;
          }
          computed_width = compute_block_width(
            &width_style,
            containing_width,
            self.viewport_size,
            root_font_metrics,
            inline_sides,
            inline_positive,
            &self.font_context,
          );
        }
      }
      if toggles.truthy("FASTR_LOG_BLOCK_WIDE")
        && computed_width.total_width() > containing_width + 0.5
      {
        let selector = child
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<child>".to_string());
        eprintln!(
                "[block-wide] id={} selector={} containing_w={:.1} content_w={:.1} total_w={:.1} width_decl={:?} min_w={:?} max_w={:?} margins=({:.1},{:.1})",
                child.id,
                selector,
                containing_width,
                computed_width.content_width,
                computed_width.total_width(),
                style.width,
                style.min_width,
                style.max_width,
                computed_width.margin_left,
                computed_width.margin_right,
            );
      }
      if width_auto {
        if let (Some(ratio), Some(h)) = (
          match style.aspect_ratio {
            crate::style::types::AspectRatio::Ratio(ratio)
            | crate::style::types::AspectRatio::AutoRatio(ratio) => Some(ratio),
            crate::style::types::AspectRatio::Auto => None,
          },
          specified_height,
        ) {
          if ratio > 0.0 {
            computed_width.content_width = if inline_is_horizontal {
              h * ratio
            } else {
              h / ratio
            };
          }
        }
      }

      // Tables use a shrink-to-fit inline size when `width` is `auto` (CSS 2.1 §17.5.2).
      // Without this, the block constraint equation would force auto-width tables to span the
      // containing block, which then makes `table-layout: fixed` distribute slack into authored
      // columns (CSS 2.1 §17.5.2.1) and breaks expected fixed-width column behavior.
      let shrink_to_fit =
        style.shrink_to_fit_inline_size || matches!(style.display, Display::Table);
      if shrink_to_fit && width_auto {
        let inline_edges = computed_width.border_left
          + computed_width.padding_left
          + computed_width.padding_right
          + computed_width.border_right;
        let margin_left = resolve_margin_side(
          style,
          inline_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let margin_right = resolve_margin_side(
          style,
          inline_sides.1,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );

        let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = factory.get(fc_type);
        let (preferred_min_content, preferred_content) =
          fc.compute_intrinsic_inline_sizes(child)?;

        let edges_base0 = inline_axis_padding_and_borders(
          style,
          0.0,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        let preferred_min =
          rebase_intrinsic_border_box_size(preferred_min_content, edges_base0, inline_edges);
        let preferred =
          rebase_intrinsic_border_box_size(preferred_content, edges_base0, inline_edges);
        let available = (containing_width - margin_left - margin_right).max(0.0);
        let shrink_border_box = preferred.min(available.max(preferred_min));
        let shrink_content = (shrink_border_box - inline_edges).max(0.0);
        let (margin_left, margin_right) = recompute_margins_for_width(
          style,
          containing_width,
          shrink_content,
          computed_width.border_left,
          computed_width.padding_left,
          computed_width.padding_right,
          computed_width.border_right,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        computed_width.content_width = shrink_content;
        computed_width.margin_left = margin_left;
        computed_width.margin_right = margin_right;
      }

      // CSS 2.1 §10.4: apply min/max inline-size constraints after computing the
      // tentative used width/margins. When clamping changes the used width, we
      // need to re-resolve auto margins so centering works (e.g. max-width +
      // margin: 0 auto).
      //
      // In-flow block children are laid out via `layout_block_child` (not via a recursive
      // BlockFormattingContext::layout call), so we must apply min/max sizing here to keep wrappers
      // like `max-width: 920px; margin: 0 auto` from inflating to the full containing block width.
      let horizontal_edges = computed_width.border_left
        + computed_width.padding_left
        + computed_width.padding_right
        + computed_width.border_right;
      let reserved_vertical_gutter =
        if style.box_sizing == crate::style::types::BoxSizing::ContentBox {
          let reservation = crate::layout::utils::scrollbar_reservation_for_style(style);
          (reservation.left + reservation.right).max(0.0)
        } else {
          0.0
        };
      let min_width = if let Some(keyword) = min_inline_keyword {
        if intrinsic_content_sizes.is_none() {
          let factory = self.child_factory_for_cb(*nearest_positioned_cb);
          let fc_type = child
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          intrinsic_content_sizes = Some(
            self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?,
          );
        }
        let (min_content, max_content) = intrinsic_content_sizes.unwrap();
        self.resolve_intrinsic_size_keyword_to_content_width(
          keyword,
          min_content,
          max_content,
          available_content_for_fit,
          containing_width,
          style,
          horizontal_edges,
        )
      } else {
        min_inline_length
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              containing_width,
              style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
          .unwrap_or(0.0)
      };
      let min_width = if reserved_vertical_gutter > 0.0
        && min_inline_keyword.is_none()
        && min_inline_length.is_some()
      {
        (min_width - reserved_vertical_gutter).max(0.0)
      } else {
        min_width
      };

      let max_width = if let Some(keyword) = max_inline_keyword {
        if intrinsic_content_sizes.is_none() {
          let factory = self.child_factory_for_cb(*nearest_positioned_cb);
          let fc_type = child
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          intrinsic_content_sizes = Some(
            self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?,
          );
        }
        let (min_content, max_content) = intrinsic_content_sizes.unwrap();
        self.resolve_intrinsic_size_keyword_to_content_width(
          keyword,
          min_content,
          max_content,
          available_content_for_fit,
          containing_width,
          style,
          horizontal_edges,
        )
      } else {
        max_inline_length
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              containing_width,
              style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
          .unwrap_or(f32::INFINITY)
      };
      let max_width = if reserved_vertical_gutter > 0.0
        && max_inline_keyword.is_none()
        && max_inline_length.is_some()
        && max_width.is_finite()
      {
        (max_width - reserved_vertical_gutter).max(0.0)
      } else {
        max_width
      };
      let max_width = if max_width.is_finite() && max_width < min_width {
        min_width
      } else {
        max_width
      };
      let clamped_content_width =
        crate::layout::utils::clamp_with_order(computed_width.content_width, min_width, max_width);
      if clamped_content_width != computed_width.content_width {
        let (margin_left, margin_right) = recompute_margins_for_width(
          style,
          containing_width,
          clamped_content_width,
          computed_width.border_left,
          computed_width.padding_left,
          computed_width.padding_right,
          computed_width.border_right,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        computed_width.content_width = clamped_content_width;
        computed_width.margin_left = margin_left;
        computed_width.margin_right = margin_right;
      }

      if width_auto {
        let used_border_box = if inline_is_horizontal {
          constraints.used_border_box_width
        } else {
          constraints.used_border_box_height
        };
        if let Some(used_border_box) = used_border_box {
          let horizontal_edges = computed_width.border_left
            + computed_width.padding_left
            + computed_width.padding_right
            + computed_width.border_right;
          let used_content = (used_border_box - horizontal_edges).max(0.0);
          let (margin_left, margin_right) = recompute_margins_for_width(
            style,
            containing_width,
            used_content,
            computed_width.border_left,
            computed_width.padding_left,
            computed_width.padding_right,
            computed_width.border_right,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          );
          computed_width.content_width = used_content;
          computed_width.margin_left = margin_left;
          computed_width.margin_right = margin_right;
        }
      }

      // CSS 2.1 §9.5.1: A block formatting context (BFC) root's border box must not overlap the
      // margin boxes of floats in the same formatting context.
      //
      // FastRender already performs float avoidance for BFC roots by shifting/clamping their
      // horizontal position, and (when the border box is too wide to fit next to floats) pushing the
      // box down below them.
      //
      // However, browsers allow *auto-sized* BFC roots to shrink to the available width next to
      // floats rather than always being pushed down. This is a common pattern for float-based
      // two-column layouts (e.g. `float:left` sidebar + `overflow:hidden` content column).
      //
      // Implement this behavior by clamping the used content inline-size when:
      // - The element is in-flow (not itself a float / abspos)
      // - The inline-size is `auto`
      // - There are overlapping floats that reduce the available width band at this Y position
      //
      // Min/max sizing has already been applied above; do not shrink below the computed `min-width`
      // content size. If the minimum cannot fit in the float band, later float avoidance will still
      // push the box down below floats.
      if width_auto
        && establishes_bfc(style)
        && !style.float.is_floating()
        && matches!(style.position, Position::Static | Position::Relative)
        && (if inline_is_horizontal {
          constraints.used_border_box_width.is_none()
        } else {
          constraints.used_border_box_height.is_none()
        })
      {
        if let Some(ctx) = external_float_ctx.as_deref() {
          let query_y = external_float_base_y + box_y;
          let (_, available_width) = ctx.available_width_at_y_in_containing_block(
            query_y,
            external_float_base_x,
            containing_width,
          );
          // Allow a small epsilon to avoid layout instability from tiny float rounding errors.
          const FLOAT_FIT_EPSILON: f32 = 0.01;
          let has_overlapping_floats = available_width + FLOAT_FIT_EPSILON < containing_width;
          if has_overlapping_floats {
            let inline_edges = computed_width.border_left
              + computed_width.padding_left
              + computed_width.padding_right
              + computed_width.border_right;
            let available_content = (available_width - inline_edges).max(0.0);
            if available_content.is_finite()
              && available_content + FLOAT_FIT_EPSILON < computed_width.content_width
              && available_content + FLOAT_FIT_EPSILON >= min_width
            {
              computed_width.content_width = available_content;
            }
          }
        }
      }

      let aspect_ratio_block_size_hint = if height_auto && specified_height.is_none() {
        match style.aspect_ratio {
          crate::style::types::AspectRatio::Ratio(ratio)
          | crate::style::types::AspectRatio::AutoRatio(ratio) => {
            if ratio > 0.0 && computed_width.content_width.is_finite() {
              let raw = if inline_is_horizontal {
                computed_width.content_width / ratio
              } else {
                computed_width.content_width * ratio
              };
              raw.is_finite().then_some(raw.max(0.0))
            } else {
              None
            }
          }
          crate::style::types::AspectRatio::Auto => None,
        }
      } else {
        None
      };

      // Definite block-size base used for descendant percentage resolution and containing block
      // percentage offsets. Prefer the explicitly resolved size, but fall back to the ratio-derived
      // size when `block-size:auto` + `aspect-ratio` yields a definite used size.
      let specified_height_base = block_size_definite_for_percentages
        .then(|| {
          specified_height
            .filter(|h| h.is_finite())
            .map(|h| h.max(0.0))
        })
        .flatten();
      let child_block_size_base = specified_height_base.or(aspect_ratio_block_size_hint);
      let child_height_space = specified_height_base
        .map(AvailableSpace::Definite)
        .unwrap_or(AvailableSpace::Indefinite);
      let mut block_percentage_base = child_block_size_base;
      if block_percentage_base.is_none()
        && parent.id == 1
        && child.generated_pseudo.is_none()
        && self.factory.quirks_mode() == QuirksMode::Quirks
      {
        // In quirks mode, browsers treat the `<body>` element as having a definite height for the
        // purpose of percentage resolution, even when its own `height` is `auto`. This allows common
        // legacy patterns like `.container { height: 100% }` (without `html, body { height: 100% }`)
        // to size to the viewport and enable `justify-content: center` vertical centering.
        //
        // The root element always receives a definite block percentage base from the initial
        // containing block; propagate that base to the principal root child so descendants can
        // resolve percentages against the viewport.
        let viewport_block_size = if inline_is_horizontal {
          self.viewport_size.height
        } else {
          self.viewport_size.width
        };
        // Treat the root child as if it fills the viewport *content* box (excluding its own block
        // margins) so `height: 100%` descendants match browser quirks behavior without forcing the
        // document to overflow by the UA default body margins.
        let viewport_block_size = viewport_block_size - margin_top - margin_bottom;
        block_percentage_base = viewport_block_size
          .is_finite()
          .then_some(viewport_block_size.max(0.0));
      }

      let child_constraints = if inline_is_horizontal {
        LayoutConstraints::new(
          AvailableSpace::Definite(computed_width.content_width),
          child_height_space,
        )
      } else {
        LayoutConstraints::new(
          child_height_space,
          AvailableSpace::Definite(computed_width.content_width),
        )
      }
      .with_inline_percentage_base(Some(computed_width.content_width))
      .with_block_percentage_base(block_percentage_base);

      // Check if this child establishes a different formatting context
      let fc_type = child.formatting_context();
      let log_flex_child = toggles.truthy("FASTR_LOG_FLEX_CHILD");
      let log_flex_child_ids = toggles
        .usize_list("FASTR_LOG_FLEX_CHILD_IDS")
        .unwrap_or_default();

      if matches!(
        fc_type,
        Some(FormattingContextType::Flex | FormattingContextType::Grid)
      ) {
        if log_flex_child || log_flex_child_ids.contains(&child.id) {
          let child_selector = child
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<child>".to_string());
          eprintln!(
                    "[flex-child-constraint] parent_id={} child_id={} child_sel={} containing={:.1} content_w={:.1} total_w={:.1} constraint_w={:?} margins=({:.1},{:.1}) width={:?} min_w={:?} max_w={:?} viewport_w={:.1} style_margins=({:?},{:?}) parent_style_width={:?} parent_min_w={:?} parent_max_w={:?}",
                    parent.id,
                    child.id,
                    child_selector,
                    containing_width,
                    computed_width.content_width,
                    computed_width.total_width(),
                    child_constraints.width(),
                    computed_width.margin_left,
                    computed_width.margin_right,
                    child.style.width,
                    child.style.min_width,
                    child.style.max_width,
                    self.viewport_size.width,
                    child.style.margin_left,
                    child.style.margin_right,
                    parent.style.width,
                    parent.style.min_width,
                    parent.style.max_width,
                );
        }
        if log_wide_flex {
          let content_w = computed_width.content_width;
          let total_w = computed_width.total_width();
          let constraint_w = child_constraints.width();
          if content_w > self.viewport_size.width + 0.5
            || total_w > self.viewport_size.width + 0.5
            || constraint_w
              .map(|w| w > self.viewport_size.width + 0.5)
              .unwrap_or(false)
            || content_w > containing_width + 0.5
            || total_w > containing_width + 0.5
          {
            let selector = child
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anonymous>".to_string());
            eprintln!(
                        "[flex-constraint-wide] parent_id={} child_id={:?} selector={} containing={:.1} content_w={:.1} total_w={:.1} constraint_w={:?} margins=({:.1},{:.1}) width={:?} min_w={:?} max_w={:?} viewport_w={:.1}",
                        parent.id,
                        child.id,
                        selector,
                        containing_width,
                        content_w,
                        total_w,
                    constraint_w,
                    computed_width.margin_left,
                    computed_width.margin_right,
                    child.style.width,
                    child.style.min_width,
                    child.style.max_width,
                    self.viewport_size.width,
                );
          }
        }
        if toggles.truthy("FASTR_LOG_NARROW_FLEX") && computed_width.content_width < 150.0 {
          // Compute how much auto margins and percentage padding/borders left for content.
          let horiz_edges = computed_width.border_left
            + computed_width.padding_left
            + computed_width.padding_right
            + computed_width.border_right;
          let selector = child
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anonymous>".to_string());
          eprintln!(
                    "[flex-constraint-narrow] child_id={:?} selector={} containing={:.1} content_w={:.1} total_w={:.1} constraint_w={:?} margins=({:.1},{:.1}) width={:?} min_w={:?} max_w={:?} viewport_w={:.1} edges={:.1} auto_width={:?}",
                    child.id,
                    selector,
                    containing_width,
                    computed_width.content_width,
                    computed_width.total_width(),
                    child_constraints.width(),
                    computed_width.margin_left,
                    computed_width.margin_right,
                    child.style.width,
                    child.style.min_width,
                    child.style.max_width,
                    self.viewport_size.width,
                    horiz_edges,
                    child.style.width.is_none(),
                );
        }
      }

      // If this block establishes a new containing block for absolute/fixed descendants (via
      // positioning, transforms, filters, containment, etc.), propagate that updated containing
      // block into the descendant layout call. Otherwise absolutely-positioned descendants inside
      // inline content can incorrectly resolve percentages against an ancestor CB (e.g. the
      // viewport).
      let establishes_positioned_cb = style.establishes_abs_containing_block();
      let establishes_fixed_cb = style.establishes_fixed_containing_block();
      let content_origin = Point::new(
        computed_width.border_left + computed_width.padding_left,
        border_top + padding_top,
      );
      // Child fragments are produced in the block's *content* coordinate space (0,0 at the content
      // edge). Represent this block's padding box (the containing block for abs/fixed descendants)
      // in that same coordinate space: the padding edge is offset from the content edge by the
      // negative padding amounts.
      let padding_origin = Point::new(-computed_width.padding_left, -padding_top);
      let content_height_base = child_block_size_base.unwrap_or(0.0);
      let padding_size = Size::new(
        computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
        content_height_base + padding_top + padding_bottom,
      );
      let cb_block_base = child_block_size_base.map(|h| h + padding_top + padding_bottom);
      let mut box_y = box_y;
      let should_relayout_abspos_descendants_for_cb_height = establishes_positioned_cb
        && cb_block_base.is_none()
        && has_abspos_descendant_needing_used_cb_height(child);
      let mut external_float_ctx = external_float_ctx;
      // CSS 2.1 §9.5.1: boxes that establish a new block formatting context must not overlap the
      // margin boxes of floats in the same formatting context. Real pages use this for clearfix
      // patterns (`overflow:hidden`, `display: table`, etc.). Without applying float avoidance here,
      // BFC roots can be laid out starting at x=0 and end up painting underneath floats.
      let mut float_avoidance_offset = 0.0;
      if establishes_bfc(style) {
        if let Some(ctx) = external_float_ctx.as_deref() {
          let border_box_width = computed_width.border_box_width();
          let border_box_width = if border_box_width.is_finite() {
            border_box_width.max(0.0)
          } else {
            0.0
          };

          // If the float context does not reduce the available width at this Y, there are no
          // overlapping floats to avoid, so do not clamp margins. In particular, Bootstrap gutters
          // rely on negative margins on flex containers (e.g. `.row { margin-left: -12px; }`) which
          // legitimately overflow the containing block even in the absence of floats.
          const FLOAT_FIT_EPSILON: f32 = 0.01;

          // If the block's border box cannot fit in the available band next to floats at the
          // computed y-position, push it down until it does (matching the float placement loop).
          if border_box_width > 0.0 {
            let min_y = external_float_base_y + box_y;
            let (mut left_edge, mut available_width) = ctx.available_width_at_y_in_containing_block(
              min_y,
              external_float_base_x,
              containing_width,
            );
            let mut has_overlapping_floats = available_width + FLOAT_FIT_EPSILON < containing_width;

            if has_overlapping_floats && available_width + FLOAT_FIT_EPSILON < border_box_width {
              let (fit_y, fit_left_edge, fit_right_edge) = ctx.find_fit_in_containing_block_with_edges(
                border_box_width,
                0.0,
                min_y,
                external_float_base_x,
                containing_width,
              );
              if fit_y.is_finite() && fit_y > min_y {
                box_y += fit_y - min_y;
                left_edge = fit_left_edge;
                available_width = (fit_right_edge - fit_left_edge).max(0.0);
                has_overlapping_floats = available_width + FLOAT_FIT_EPSILON < containing_width;
              }
            }

            if has_overlapping_floats {
              let band_left = (left_edge - external_float_base_x).max(0.0);
              let band_right = (band_left + available_width).max(band_left);

              let desired_x = computed_width.margin_left;
              let max_x = band_right - border_box_width;
              let clamped_x = if max_x >= band_left {
                desired_x.clamp(band_left, max_x)
              } else {
                band_left
              };
              float_avoidance_offset = clamped_x - desired_x;
            }
          }
        }
      }
      let child_border_origin =
        Point::new(float_avoidance_offset + computed_width.margin_left, box_y);
      // Translate viewport-relative containing blocks into the child's coordinate space. Without
      // this, absolute/fixed positioned descendants can mistakenly include the parent's placement
      // offset (e.g. after parent/child margin collapsing shifts the child).
      let cb_translation_origin =
        if child_border_origin.x.is_finite() && child_border_origin.y.is_finite() {
          child_border_origin
        } else {
          Point::ZERO
        };
      let child_cb_delta = Point::new(-cb_translation_origin.x, -cb_translation_origin.y);
      let inherited_positioned_cb = nearest_positioned_cb.translate(child_cb_delta);
      // Viewport-fixed positioned elements are represented in absolute (viewport) coordinates for
      // paint. Keep the initial containing block un-translated so fixed descendants keep absolute
      // bounds; fixed CB ancestors (e.g. transforms) still translate like normal CBs.
      let viewport_fixed_cb = ContainingBlock::viewport(self.viewport_size);
      let inherited_fixed_cb = if *nearest_fixed_cb == viewport_fixed_cb {
        *nearest_fixed_cb
      } else {
        nearest_fixed_cb.translate(child_cb_delta)
      };

      let mut descendant_nearest_positioned_cb = if establishes_positioned_cb {
        ContainingBlock::with_viewport_and_bases(
          Rect::new(padding_origin, padding_size),
          self.viewport_size,
          Some(padding_size.width),
          cb_block_base,
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction)
        .with_box_id(Some(child.id))
      } else {
        inherited_positioned_cb
      };
      let mut descendant_nearest_fixed_cb = if establishes_fixed_cb {
        ContainingBlock::with_viewport_and_bases(
          Rect::new(padding_origin, padding_size),
          self.viewport_size,
          Some(padding_size.width),
          cb_block_base,
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction)
        .with_box_id(Some(child.id))
      } else {
        inherited_fixed_cb
      };

      let box_width = computed_width.border_box_width();
      let box_width = if box_width.is_finite() {
        box_width.max(0.0)
      } else {
        0.0
      };
      let child_content_origin = Point::new(
        child_border_origin.x + content_origin.x,
        child_border_origin.y + content_origin.y,
      );
      let child_viewport =
        paint_viewport.translate(Point::new(-child_content_origin.x, -child_content_origin.y));

      let skip_contents = match style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          // A deterministic heuristic aligned with Chrome: if the element's border box does not
          // intersect the paint viewport, treat it as skipped content and size the box using
          // `contain-intrinsic-size` fallback rules.
          let activation_margin = toggles
            .f64("FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX")
            .unwrap_or(0.0)
            .max(0.0) as f32;
          let viewport = if activation_margin > 0.0 {
            paint_viewport.inflate(activation_margin)
          } else {
            paint_viewport
          };

          let estimated_border_box_block_size = specified_height
            .filter(|h| h.is_finite())
            .map(|h| h.max(0.0))
            .or_else(|| {
              let axis_is_width = block_axis_is_horizontal(style.writing_mode);
              let axis = if axis_is_width {
                style.contain_intrinsic_width
              } else {
                style.contain_intrinsic_height
              };
              axis
                .auto
                .then(|| {
                  remembered_size_cache_lookup(child).map(|size| {
                    if axis_is_width {
                      size.width
                    } else {
                      size.height
                    }
                  })
                })
                .flatten()
                .filter(|v| v.is_finite())
                .map(|v| v.max(0.0))
                .or_else(|| {
                  axis
                    .length
                    .and_then(|l| {
                      resolve_length_with_percentage_metrics_and_root_font_metrics(
                        l,
                        containing_height,
                        self.viewport_size,
                        style.font_size,
                        style.root_font_size,
                        Some(style),
                        Some(&self.font_context),
                        self.factory.root_font_metrics(),
                      )
                    })
                    .map(|v| v.max(0.0))
                })
            })
            .and_then(|content_estimate| {
              let border_box = content_estimate + vertical_edges;
              border_box.is_finite().then_some(border_box.max(0.0))
            });

          if let Some(block_size) = estimated_border_box_block_size {
            let border_box = Rect::from_xywh(child_border_origin.x, box_y, box_width, block_size);
            !viewport.intersects(border_box)
          } else {
            // Without a definite placeholder block-size (explicit height or a resolved
            // `contain-intrinsic-*` length), skipping layout would collapse the element to 0px (the
            // initial `contain-intrinsic-size: auto` has no fallback length) and pull later siblings
            // upward. In that case, keep laying out to determine sizing; paint skipping still
            // applies.
            false
          }
        }
        crate::style::types::ContentVisibility::Visible => false,
      };

      let use_columns = Self::is_multicol_container(style);

      // Child establishes a non-block formatting context (flex/grid/table). Delegate layout to the
      // appropriate formatting context and return its fragment directly.
      //
      // The block formatting context still owns margin collapsing and used width resolution for
      // block-level boxes. Provide the resolved border-box size via `used_border_box_*` so the child
      // formatting context doesn't re-run block wrapper logic (which would double-apply
      // padding/borders and can generate duplicate fragments for the same box).
      if !skip_contents {
        if let Some(fc_type) = fc_type {
          if fc_type != FormattingContextType::Block {
            // Translate viewport-relative state (scroll offset + positioned/fixed containing blocks)
            // into the child's coordinate space so nested formatting contexts can correctly resolve
            // absolute/fixed positioning and `content-visibility:auto` decisions.
            let factory = self
              .child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb)
              .translated_for_child(child_border_origin);
            // Layout skipping (`content-visibility:auto`) is viewport-relative, so translate the
            // viewport scroll offset into the child’s local coordinate space before invoking layout.
            // Note: `viewport_scroll()` consults the thread-local override stack, so compute the
            // translated scroll explicitly rather than reading it back from `factory`.
            let parent_scroll = self.factory.viewport_scroll();
            let parent_scroll = if parent_scroll.x.is_finite() && parent_scroll.y.is_finite() {
              parent_scroll
            } else {
              Point::ZERO
            };
            let child_scroll = Point::new(
              parent_scroll.x - child_border_origin.x,
              parent_scroll.y - child_border_origin.y,
            );
            let fc = factory.get(fc_type);

            let used_border_box_inline = computed_width.border_box_width();
            let used_border_box_block =
              specified_height.map(|h| (h.max(0.0) + vertical_edges).max(0.0));
            // Block layout performs sizing/margin resolution in logical inline/block coordinates, but
            // flex/grid/table formatting contexts operate in the physical width/height axes. Map the
            // resolved border-box size into physical space and then convert the resulting fragment
            // tree back into the block formatting context’s logical space so the parent's
            // axis-conversion step applies exactly once.
            let (used_border_box_width, used_border_box_height) = if inline_is_horizontal {
              (Some(used_border_box_inline), used_border_box_block)
            } else {
              (used_border_box_block, Some(used_border_box_inline))
            };
            // When delegating to a non-block formatting context (flex/grid/table), keep the
            // *available size* in the block axis consistent with what block layout would pass to a
            // normal child: only constrain it when the element has a definite block-size
            // (`height`/`width` in the logical block axis, or a used-size override).
            //
            // Passing the parent's available height here (e.g. the viewport height for the root
            // element) incorrectly forces auto-sized flex/grid containers to fill the viewport,
            // pulling later siblings upward (notably visible on `walmart.com` where the footer would
            // appear in the initial viewport).
            let fc_constraints = if inline_is_horizontal {
              // Block formatting contexts do not constrain in-flow children in the block axis.
              // A definite available height may still be present (e.g. the viewport height), but
              // treating it as an actual sizing constraint causes nested flex/grid/table layout
              // to incorrectly stretch auto-sized containers (notably when `align-content: stretch`,
              // which is common and the initial value for grid containers).
              //
              // Use the same block-axis available size we'd pass to a normal block child
              // (`child_height_space`) when we're in scrollable layout. Percentage heights still
              // resolve via `block_percentage_base` (see `containing_height_for_percentages`), so we
              // can drop the definite available height without losing percentage resolution.
              let mut available_height = constraints.available_height;
              if crate::layout::formatting_context::fragmentainer_block_size_hint().is_none()
                && matches!(available_height, AvailableSpace::Definite(_))
              {
                available_height = child_height_space;
              }
              LayoutConstraints::new(AvailableSpace::Definite(containing_width), available_height)
            } else {
              LayoutConstraints::new(
                child_height_space,
                AvailableSpace::Definite(containing_width),
              )
            }
            .with_inline_percentage_base(Some(containing_width))
            .with_block_percentage_base(containing_height_for_percentages)
            .with_used_border_box_size(used_border_box_width, used_border_box_height);
            let mut fragment =
              FormattingContextFactory::with_viewport_scroll_override(child_scroll, || {
                fc.layout(child, &fc_constraints)
              })?;
            let physical_width = fragment.bounds.width();
            let physical_height = fragment.bounds.height();
            // Non-block formatting contexts (grid/flex/table) return fragments in physical
            // coordinates. The block formatting context keeps fragments in logical coordinates
            // until `convert_fragment_axes` runs at the end of `layout`, so convert the subtree
            // back into logical space here to avoid double-applying the writing-mode transform.
            fragment = unconvert_fragment_axes_root(fragment);
            let desired_origin = child_border_origin;
            let offset = Point::new(
              desired_origin.x - fragment.bounds.x(),
              desired_origin.y - fragment.bounds.y(),
            );
            if offset != Point::ZERO {
              fragment.translate_root_in_place(offset);
            }
            let child_block_is_horizontal = block_axis_is_horizontal(style.writing_mode);
            let border_box_block = if child_block_is_horizontal {
              physical_width
            } else {
              physical_height
            };
            let remembered_block = (border_box_block - vertical_edges).max(0.0);
            let remembered_inline = computed_width.content_width;
            let remembered = if block_axis_is_horizontal(style.writing_mode) {
              Size::new(remembered_block, remembered_inline)
            } else {
              Size::new(remembered_inline, remembered_block)
            };
            remembered_size_cache_store(child, remembered);
            let parent_block_is_horizontal = block_axis_is_horizontal(parent.style.writing_mode);
            let (inline_size_in_parent, block_size_in_parent) = if parent_block_is_horizontal {
              (physical_height, physical_width)
            } else {
              (physical_width, physical_height)
            };
            fragment.bounds = Rect::from_xywh(
              fragment.bounds.x(),
              fragment.bounds.y(),
              inline_size_in_parent,
              block_size_in_parent,
            );
            fragment.block_metadata = Some(BlockFragmentMetadata {
              margin_top,
              margin_bottom,
              ..BlockFragmentMetadata::default()
            });
            self.maybe_attach_footnote_anchor(
              child,
              containing_width,
              nearest_positioned_cb,
              nearest_fixed_cb,
              &mut fragment,
            )?;

            return Ok(fragment);
          }
        }
      }

      let mut float_ctx_snapshot = if should_relayout_abspos_descendants_for_cb_height {
        external_float_ctx.as_deref().cloned()
      } else {
        None
      };

      let (mut child_fragments, mut content_height, mut positioned_children, mut column_info) =
        if skip_contents {
          (Vec::new(), 0.0, Vec::new(), None)
        } else if use_columns {
          let (frags, height, positioned, info) = self.layout_multicolumn(
            child,
            &child_constraints,
            &descendant_nearest_positioned_cb,
            &descendant_nearest_fixed_cb,
            computed_width.content_width,
            child_viewport,
          )?;
          (frags, height, positioned, info)
        } else {
          let (frags, height, positioned) = self.layout_children_with_external_floats(
            child,
            &child_constraints,
            &descendant_nearest_positioned_cb,
            &descendant_nearest_fixed_cb,
            child_viewport,
            external_float_ctx.as_deref_mut(),
            external_float_base_x + child_content_origin.x,
            external_float_base_y + child_content_origin.y,
          )?;
          (frags, height, positioned, None)
        };

      if skip_contents || style.containment.size {
        let axis_is_width = block_axis_is_horizontal(style.writing_mode);
        let axis = if axis_is_width {
          style.contain_intrinsic_width
        } else {
          style.contain_intrinsic_height
        };
        let remembered = axis
          .auto
          .then(|| {
            remembered_size_cache_lookup(child).map(|size| {
              if axis_is_width {
                size.width
              } else {
                size.height
              }
            })
          })
          .flatten();
        content_height = crate::layout::utils::resolve_contain_intrinsic_size_axis(
          axis,
          remembered,
          containing_height,
          self.viewport_size,
          style.font_size,
          style.root_font_size,
        );
      }

      // Child fragments are produced in the block's content coordinate space (0,0 at the content
      // box). Translate them into the fragment's local coordinate space (border box) so padding and
      // borders correctly offset in-flow content.
      if content_origin.x != 0.0 || content_origin.y != 0.0 {
        for fragment in child_fragments.iter_mut() {
          fragment.translate_root_in_place(content_origin);
        }
      }

      // Height computation (CSS 2.1 Section 10.6.3) with aspect-ratio adjustment (CSS Sizing L4)
      let mut height = specified_height.unwrap_or(content_height);
      if specified_height.is_none() {
        if let crate::style::types::AspectRatio::Ratio(ratio)
        | crate::style::types::AspectRatio::AutoRatio(ratio) = style.aspect_ratio
        {
          if ratio > 0.0 && computed_width.content_width.is_finite() {
            let ratio_height = if inline_is_horizontal {
              computed_width.content_width / ratio
            } else {
              computed_width.content_width * ratio
            };
            // Do not shrink below content-based height
            height = height.max(ratio_height);
          }
        }
      }

      // Apply min/max height constraints
      let min_height_keyword = if inline_is_horizontal {
        style.min_height_keyword
      } else {
        style.min_width_keyword
      };
      let min_height = if let Some(keyword) = min_height_keyword {
        let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
        let min_border = match keyword {
          crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
          crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
          crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
            if available_block_border_box.is_finite() {
              available_block_border_box
            } else {
              intrinsic_max
            }
          }
          crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
            let basis_border = match limit {
              Some(limit) => resolve_length_with_percentage_metrics_and_root_font_metrics(
                limit,
                containing_height_for_percentages,
                self.viewport_size,
                font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
                self.factory.root_font_metrics(),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(f32::INFINITY),
              None => available_block_border_box,
            };
            crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
          }
          crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
            use crate::style::types::BoxSizing;
            use crate::style::types::CalcSizeBasis;
            let basis_border = match calc.basis {
              CalcSizeBasis::Auto => intrinsic_max,
              CalcSizeBasis::MinContent => intrinsic_min,
              CalcSizeBasis::MaxContent => intrinsic_max,
              CalcSizeBasis::FillAvailable => {
                if available_block_border_box.is_finite() {
                  available_block_border_box
                } else {
                  intrinsic_max
                }
              }
              CalcSizeBasis::FitContent { limit } => {
                let basis_border = match limit {
                  Some(limit) => resolve_length_with_percentage_metrics(
                    limit,
                    containing_height_for_percentages,
                    self.viewport_size,
                    font_size,
                    style.root_font_size,
                    Some(style),
                    Some(&self.font_context),
                  )
                  .map(|resolved| {
                    border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                  })
                  .unwrap_or(f32::INFINITY),
                  None => available_block_border_box,
                };
                crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
              }
              CalcSizeBasis::Length(len) => resolve_length_with_percentage_metrics(
                len,
                containing_height_for_percentages,
                self.viewport_size,
                font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(intrinsic_max),
            }
            .max(0.0);
            let basis_content = (basis_border - vertical_edges).max(0.0);
            let basis_specified = match style.box_sizing {
              BoxSizing::ContentBox => basis_content,
              BoxSizing::BorderBox => basis_border,
            };
            crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
              .and_then(|expr_sum| {
                crate::css::properties::parse_length(&format!("calc({expr_sum})"))
              })
              .and_then(|expr_len| {
                resolve_length_with_percentage_metrics(
                  expr_len,
                  containing_height_for_percentages,
                  self.viewport_size,
                  font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                )
              })
              .map(|resolved_specified| {
                border_size_from_box_sizing(resolved_specified, vertical_edges, style.box_sizing)
                  .max(0.0)
              })
              .unwrap_or(basis_border)
          }
        };
        (min_border - vertical_edges).max(0.0)
      } else {
        (if inline_is_horizontal {
          style.min_height
        } else {
          style.min_width
        })
        .as_ref()
        .and_then(|l| {
          resolve_length_with_percentage_metrics_and_root_font_metrics(
            *l,
            containing_height_for_percentages,
            self.viewport_size,
            font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
            self.factory.root_font_metrics(),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
        .unwrap_or(0.0)
      };
      let min_height = if reserved_horizontal_gutter > 0.0
        && style.box_sizing == crate::style::types::BoxSizing::ContentBox
        && min_height_keyword.is_none()
        && (if inline_is_horizontal {
          style.min_height.is_some()
        } else {
          style.min_width.is_some()
        }) {
        (min_height - reserved_horizontal_gutter).max(0.0)
      } else {
        min_height
      };
      let max_height_keyword = if inline_is_horizontal {
        style.max_height_keyword
      } else {
        style.max_width_keyword
      };
      let max_height = if let Some(keyword) = max_height_keyword {
        let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
        let max_border = match keyword {
          crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
          crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
          crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
            if available_block_border_box.is_finite() {
              available_block_border_box
            } else {
              intrinsic_max
            }
          }
          crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
            let basis_border = match limit {
              Some(limit) => resolve_length_with_percentage_metrics_and_root_font_metrics(
                limit,
                containing_height_for_percentages,
                self.viewport_size,
                font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
                self.factory.root_font_metrics(),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(f32::INFINITY),
              None => available_block_border_box,
            };
            crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
          }
          crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
            use crate::style::types::BoxSizing;
            use crate::style::types::CalcSizeBasis;
            let basis_border = match calc.basis {
              CalcSizeBasis::Auto => intrinsic_max,
              CalcSizeBasis::MinContent => intrinsic_min,
              CalcSizeBasis::MaxContent => intrinsic_max,
              CalcSizeBasis::FillAvailable => {
                if available_block_border_box.is_finite() {
                  available_block_border_box
                } else {
                  intrinsic_max
                }
              }
              CalcSizeBasis::FitContent { limit } => {
                let basis_border = match limit {
                  Some(limit) => resolve_length_with_percentage_metrics(
                    limit,
                    containing_height_for_percentages,
                    self.viewport_size,
                    font_size,
                    style.root_font_size,
                    Some(style),
                    Some(&self.font_context),
                  )
                  .map(|resolved| {
                    border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                  })
                  .unwrap_or(f32::INFINITY),
                  None => available_block_border_box,
                };
                crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
              }
              CalcSizeBasis::Length(len) => resolve_length_with_percentage_metrics(
                len,
                containing_height_for_percentages,
                self.viewport_size,
                font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(intrinsic_max),
            }
            .max(0.0);
            let basis_content = (basis_border - vertical_edges).max(0.0);
            let basis_specified = match style.box_sizing {
              BoxSizing::ContentBox => basis_content,
              BoxSizing::BorderBox => basis_border,
            };
            crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
              .and_then(|expr_sum| {
                crate::css::properties::parse_length(&format!("calc({expr_sum})"))
              })
              .and_then(|expr_len| {
                resolve_length_with_percentage_metrics(
                  expr_len,
                  containing_height_for_percentages,
                  self.viewport_size,
                  font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                )
              })
              .map(|resolved_specified| {
                border_size_from_box_sizing(resolved_specified, vertical_edges, style.box_sizing)
                  .max(0.0)
              })
              .unwrap_or(basis_border)
          }
        };
        (max_border - vertical_edges).max(0.0)
      } else {
        (if inline_is_horizontal {
          style.max_height
        } else {
          style.max_width
        })
        .as_ref()
        .and_then(|l| {
          resolve_length_with_percentage_metrics_and_root_font_metrics(
            *l,
            containing_height_for_percentages,
            self.viewport_size,
            font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
            self.factory.root_font_metrics(),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
      };
      let max_height = if reserved_horizontal_gutter > 0.0
        && style.box_sizing == crate::style::types::BoxSizing::ContentBox
        && max_height_keyword.is_none()
        && (if inline_is_horizontal {
          style.max_height.is_some()
        } else {
          style.max_width.is_some()
        })
        && max_height.is_finite()
      {
        (max_height - reserved_horizontal_gutter).max(0.0)
      } else {
        max_height
      };
      let max_height = if max_height.is_finite() && max_height < min_height {
        min_height
      } else {
        max_height
      };
      let height = crate::layout::utils::clamp_with_order(height, min_height, max_height);

      if should_relayout_abspos_descendants_for_cb_height && !skip_contents {
        // Absolutely positioned descendants can use percentage block-sizes/insets that resolve
        // against the *used* padding box height of their containing block (CSS 2.1 §10.5/§10.6.4),
        // even when the containing block itself is `height:auto`. The used height isn't known until
        // after in-flow layout, but inline formatting contexts lay out their positioned descendants
        // during the main flow pass. Re-run child layout with an updated containing block height so
        // nested abspos descendants see the correct percentage basis.

        // Restore the external float context snapshot so floats placed during the first pass do not
        // get duplicated when we run the second pass.
        if let (Some(snapshot), Some(ctx)) =
          (float_ctx_snapshot.take(), external_float_ctx.as_deref_mut())
        {
          *ctx = snapshot;
        }

        let used_padding_size =
          Size::new(padding_size.width, height + padding_top + padding_bottom);
        let used_cb_block_base = Some(used_padding_size.height);
        if establishes_positioned_cb {
          descendant_nearest_positioned_cb = ContainingBlock::with_viewport_and_bases(
            Rect::new(padding_origin, used_padding_size),
            self.viewport_size,
            Some(used_padding_size.width),
            used_cb_block_base,
          )
          .with_writing_mode_and_direction(style.writing_mode, style.direction)
          .with_box_id(Some(child.id));
        }
        if establishes_fixed_cb {
          descendant_nearest_fixed_cb = ContainingBlock::with_viewport_and_bases(
            Rect::new(padding_origin, used_padding_size),
            self.viewport_size,
            Some(used_padding_size.width),
            used_cb_block_base,
          )
          .with_writing_mode_and_direction(style.writing_mode, style.direction)
          .with_box_id(Some(child.id));
        }

        let (frags, _relayout_height, positioned, info) = if use_columns {
          let (frags, height, positioned, info) = self.layout_multicolumn(
            child,
            &child_constraints,
            &descendant_nearest_positioned_cb,
            &descendant_nearest_fixed_cb,
            computed_width.content_width,
            child_viewport,
          )?;
          (frags, height, positioned, info)
        } else {
          let (frags, height, positioned) = self.layout_children_with_external_floats(
            child,
            &child_constraints,
            &descendant_nearest_positioned_cb,
            &descendant_nearest_fixed_cb,
            child_viewport,
            external_float_ctx.as_deref_mut(),
            external_float_base_x + child_content_origin.x,
            external_float_base_y + child_content_origin.y,
          )?;
          (frags, height, positioned, None)
        };

        child_fragments = frags;
        positioned_children = positioned;
        column_info = info;

        if content_origin.x != 0.0 || content_origin.y != 0.0 {
          for fragment in child_fragments.iter_mut() {
            fragment.translate_root_in_place(content_origin);
          }
        }
      }

      // Create the fragment
      let box_height = border_top + padding_top + height + padding_bottom + border_bottom;
      let box_width = computed_width.border_box_width();

      // Layout out-of-flow positioned children against this block's padding box.
      if !positioned_children.is_empty() {
        let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
          self.font_context.clone(),
        );
        let mut anchor_index =
          crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
            child_fragments.as_slice(),
            child.id,
            &style.anchor_scope,
            self.viewport_size,
          );
        // Allow descendants to anchor against the containing block element itself.
        anchor_index.insert_names_for_box(
          child.id,
          &style.anchor_names,
          crate::layout::anchor_positioning::AnchorBox {
            rect: Rect::from_xywh(0.0, 0.0, box_width, box_height),
            writing_mode: style.writing_mode,
            direction: style.direction,
          },
        );
        // In-flow children were translated into this block's border-box coordinate space above, so
        // position out-of-flow children in that same coordinate space. The containing block for
        // absolute/fixed descendants is the padding box (CSS 2.1 §10.1), whose origin is the padding
        // edge inside the border box.
        let padding_origin = Point::new(computed_width.border_left, border_top);
        let padding_size = Size::new(
          computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
          height + padding_top + padding_bottom,
        );
        let padding_rect = Rect::new(padding_origin, padding_size);
        let mut anchor_index_physical = None;
        let mut parent_padding_cb_physical = None;
        let needs_physical_coordinate_space = block_axis_is_horizontal(style.writing_mode)
          || !inline_axis_positive(style.writing_mode, style.direction);
        if needs_physical_coordinate_space {
          let physical_children: Vec<_> = child_fragments
            .iter()
            .cloned()
            .map(|fragment| {
              convert_fragment_axes(
                fragment,
                box_width,
                box_height,
                style.writing_mode,
                style.direction,
              )
            })
            .collect();
          let mut physical_index =
            crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
              physical_children.as_slice(),
              child.id,
              &style.anchor_scope,
              self.viewport_size,
            );
          physical_index.insert_names_for_box(
            child.id,
            &style.anchor_names,
            crate::layout::anchor_positioning::AnchorBox {
              rect: Rect::new(
                Point::ZERO,
                if block_axis_is_horizontal(style.writing_mode) {
                  Size::new(box_height, box_width)
                } else {
                  Size::new(box_width, box_height)
                },
              ),
              writing_mode: style.writing_mode,
              direction: style.direction,
            },
          );
          anchor_index_physical = Some(physical_index);

          let padding_rect_physical = logical_rect_to_physical_full(
            padding_rect,
            box_width,
            box_height,
            style.writing_mode,
            style.direction,
          );
          parent_padding_cb_physical = Some(
            ContainingBlock::with_viewport_and_bases(
              padding_rect_physical,
              self.viewport_size,
              Some(padding_rect_physical.size.width),
              Some(padding_rect_physical.size.height),
            )
            .with_writing_mode_and_direction(style.writing_mode, style.direction)
            .with_box_id(Some(child.id)),
          );
        }
        let parent_padding_cb = ContainingBlock::with_viewport_and_bases(
          padding_rect,
          self.viewport_size,
          Some(padding_size.width),
          Some(padding_size.height),
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction)
        .with_box_id(Some(child.id));
        let base_factory = self.factory.clone();
        let viewport_cb = ContainingBlock::viewport(self.viewport_size);
        let abs_factory = if parent_padding_cb == base_factory.nearest_positioned_cb() {
          base_factory.clone()
        } else {
          base_factory.with_positioned_cb(parent_padding_cb)
        };
        let fixed_factory = if viewport_cb == parent_padding_cb {
          abs_factory.clone()
        } else if viewport_cb == base_factory.nearest_positioned_cb() {
          base_factory.clone()
        } else {
          base_factory.with_positioned_cb(viewport_cb)
        };
        let factory_for_cb = |cb: ContainingBlock| -> &FormattingContextFactory {
          if cb == parent_padding_cb {
            &abs_factory
          } else if cb == viewport_cb {
            &fixed_factory
          } else {
            &base_factory
          }
        };

        let trace_positioned = trace_positioned_ids();
        for PositionedCandidate {
          node: pos_child,
          source,
          static_position,
          query_parent_id,
          implicit_anchor_box_id,
        } in positioned_children
        {
          let original_style = pos_child.style.clone();
          if trace_positioned.contains(&pos_child.id) {
            eprintln!(
                        "[block-positioned-layout] parent_id={} child_id={} padding_rect=({:.1},{:.1},{:.1},{:.1})",
                        parent.id,
                        pos_child.id,
                        padding_rect.x(),
                        padding_rect.y(),
                        padding_rect.width(),
                        padding_rect.height()
                    );
          }
          let cb = match source {
            ContainingBlockSource::ParentPadding => parent_padding_cb,
            ContainingBlockSource::Explicit(cb) => cb,
          };
          let (anchors_for_cb, positioning_cb, needs_physical_conversion) =
            if cb == parent_padding_cb {
              match (anchor_index_physical.as_ref(), parent_padding_cb_physical) {
                (Some(index), Some(physical_cb)) => (Some(index), physical_cb, true),
                _ => (Some(&anchor_index), cb, false),
              }
            } else {
              (Some(&anchor_index), cb, false)
            };
          let factory = factory_for_cb(cb);
          // Layout the child as if it were in normal flow to obtain its intrinsic size.
          let mut static_style = pos_child.style.clone();
          {
            let s = Arc::make_mut(&mut static_style);
            s.position = Position::Relative;
            s.top = crate::style::types::InsetValue::Auto;
            s.right = crate::style::types::InsetValue::Auto;
            s.bottom = crate::style::types::InsetValue::Auto;
            s.left = crate::style::types::InsetValue::Auto;
          }

          let fc_type = pos_child
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let fc = factory.get(fc_type);
          let height_available = cb.block_percentage_base();
          let child_height_space = height_available
            .map(AvailableSpace::Definite)
            .unwrap_or(AvailableSpace::Indefinite);
          let child_constraints = LayoutConstraints::new(
            AvailableSpace::Definite(padding_size.width),
            child_height_space,
          );

          // Resolve positioned style against the containing block.
          let anchor_query = crate::layout::anchor_positioning::AnchorQueryContext {
            query_parent_box_id: Some(query_parent_id),
            implicit_anchor_box_id,
          };
          let positioned_style =
            crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
              &pos_child.style,
              &positioning_cb,
              self.viewport_size,
              &self.font_context,
              anchors_for_cb,
              anchor_query,
            );

          // When both insets are specified and the corresponding size is `auto`, the absolute
          // positioning algorithm resolves a definite used size from the constraint equation.
          //
          // Even if that used size matches the initial "static" layout size (e.g. because floats
          // stretch the box to the same height), descendants still need the definite size as the
          // percentage base for `height:100%`/`width:100%`. Force a relayout pass with the computed
          // used border-box size so percentage resolution is spec-correct.
          let relayout_for_definite_insets = (positioned_style.width.is_auto()
            && !positioned_style.left.is_auto()
            && !positioned_style.right.is_auto())
            || (positioned_style.height.is_auto()
              && !positioned_style.top.is_auto()
              && !positioned_style.bottom.is_auto());

          let mut static_pos = static_position.unwrap_or(Point::ZERO);
          if cb == parent_padding_cb {
            static_pos = Point::new(
              static_pos.x + computed_width.padding_left,
              static_pos.y + padding_top,
            );
          } else {
            // Static positions are recorded in this block's content coordinate space. When the
            // positioned containing block is an inherited one (e.g. viewport), rebase the static
            // position so it's relative to the containing block origin.
            let origin = positioning_cb.origin();
            static_pos = Point::new(static_pos.x - origin.x, static_pos.y - origin.y);
          }
          if needs_physical_conversion {
            static_pos = logical_rect_to_physical_full(
              Rect::new(static_pos, Size::new(0.0, 0.0)),
              padding_size.width,
              padding_size.height,
              style.writing_mode,
              style.direction,
            )
            .origin;
          }
          let is_replaced = pos_child.is_replaced();
          let needs_inline_intrinsics = (positioned_style.width.is_auto()
            && (positioned_style.left.is_auto()
              || positioned_style.right.is_auto()
              || is_replaced))
            || original_style.width_keyword.is_some()
            || original_style.min_width_keyword.is_some()
            || original_style.max_width_keyword.is_some();
          let needs_block_intrinsics = (positioned_style.height.is_auto()
            && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()))
            || original_style.height_keyword.is_some()
            || original_style.min_height_keyword.is_some()
            || original_style.max_height_keyword.is_some();
          // When `top/bottom/height` are all `auto`, CSS2.1 sizes the abspos box based on its normal
          // flow height (i.e. content height). In this case, block intrinsic probes (min/max-content)
          // are unnecessary and can be actively harmful: intrinsic probes run under intrinsic inline
          // constraints and may not match the final used inline size (e.g. `width: 100%`), leading to
          // an oversized block-size. That in turn affects `justify-content` in flex containers and
          // shifts content vertically (e.g. github.com's hero CTA).
          let skip_block_intrinsics = positioned_style.height.is_auto()
            && positioned_style.top.is_auto()
            && positioned_style.bottom.is_auto()
            && original_style.height_keyword.is_none()
            && original_style.min_height_keyword.is_none()
            && original_style.max_height_keyword.is_none();
          let (
            mut child_fragment,
            preferred_min_inline,
            preferred_inline,
            preferred_min_block,
            preferred_block,
          ) = if pos_child.id != 0 {
            crate::layout::style_override::with_style_override(
              pos_child.id,
              static_style.clone(),
              || {
                let child_fragment = fc.layout(&pos_child, &child_constraints)?;
                let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
                  match fc.compute_intrinsic_inline_sizes(&pos_child) {
                    Ok((min, max)) => (Some(min), Some(max)),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {
                      let min = match fc
                        .compute_intrinsic_inline_size(&pos_child, IntrinsicSizingMode::MinContent)
                      {
                        Ok(value) => Some(value),
                        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                        Err(_) => None,
                      };
                      let max = match fc
                        .compute_intrinsic_inline_size(&pos_child, IntrinsicSizingMode::MaxContent)
                      {
                        Ok(value) => Some(value),
                        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                        Err(_) => None,
                      };
                      (min, max)
                    }
                  }
                } else {
                  (None, None)
                };
                let preferred_min_block = if needs_block_intrinsics && !skip_block_intrinsics {
                  match fc.compute_intrinsic_block_size(&pos_child, IntrinsicSizingMode::MinContent)
                  {
                    Ok(value) => Some(value),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  }
                } else {
                  None
                };
                let preferred_block = if needs_block_intrinsics && !skip_block_intrinsics {
                  match fc.compute_intrinsic_block_size(&pos_child, IntrinsicSizingMode::MaxContent)
                  {
                    Ok(value) => Some(value),
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
            let mut layout_child = pos_child.clone();
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
                    Ok(value) => Some(value),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                  let max = match fc
                    .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
                  {
                    Ok(value) => Some(value),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                  (min, max)
                }
              }
            } else {
              (None, None)
            };
            let preferred_min_block = if needs_block_intrinsics && !skip_block_intrinsics {
              match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent)
              {
                Ok(value) => Some(value),
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => None,
              }
            } else {
              None
            };
            let preferred_block = if needs_block_intrinsics && !skip_block_intrinsics {
              match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent)
              {
                Ok(value) => Some(value),
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
              &original_style,
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

          if trace_positioned.contains(&pos_child.id) {
            eprintln!(
            "[block-positioned-intrinsics] child_id={} fc={:?} static_pos=({:.1},{:.1}) child_constraints=({:?},{:?}) static_layout_border_box=({:.1},{:.1}) intrinsic_size=({:.1},{:.1}) pref_min_inline={:?} pref_inline={:?} pref_min_block={:?} pref_block={:?}",
            pos_child.id,
            fc_type,
            static_pos.x,
            static_pos.y,
            child_constraints.available_width,
            child_constraints.available_height,
            child_fragment.bounds.width(),
            child_fragment.bounds.height(),
            intrinsic_size.width,
            intrinsic_size.height,
            preferred_min_inline,
            preferred_inline,
            preferred_min_block,
            preferred_block,
          );
          }

          let mut input = crate::layout::absolute_positioning::AbsoluteLayoutInput::new(
            positioned_style,
            intrinsic_size,
            static_pos,
          );
          input.is_replaced = is_replaced;
          input.preferred_min_inline_size = preferred_min_inline;
          input.preferred_inline_size = preferred_inline;
          input.preferred_min_block_size = preferred_min_block;
          input.preferred_block_size = preferred_block;
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );

          let (layout_positioned_style, mut result) =
            crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
              &abs,
              &input,
              &original_style,
              &positioning_cb,
              self.viewport_size,
              &self.font_context,
              anchors_for_cb,
              anchor_query,
            )?;
          let mut border_size_physical = Size::new(
            result.size.width + actual_horizontal,
            result.size.height + actual_vertical,
          );
          let mut border_origin_physical = Point::new(
            result.position.x - content_offset.x,
            result.position.y - content_offset.y,
          );
          let (mut border_origin, mut border_size) = if needs_physical_conversion {
            let border_rect = Rect::new(border_origin_physical, border_size_physical);
            let logical_rect = physical_rect_to_logical_full(
              border_rect,
              box_width,
              box_height,
              style.writing_mode,
              style.direction,
            );
            (logical_rect.origin, logical_rect.size)
          } else {
            (border_origin_physical, border_size_physical)
          };

          // When `height:auto` is content-based, the intrinsic height depends on the *used* width.
          // For abspos boxes this used width may be clamped by `max-width` (Discord's `.home_clyde`),
          // so remeasure intrinsic height at the resolved width before computing the final Y offset.
          if crate::layout::absolute_positioning::auto_height_uses_intrinsic_size(
            &layout_positioned_style,
            input.is_replaced,
          ) && (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          {
            let measure_constraints = child_constraints
              .with_width(AvailableSpace::Definite(border_size.width))
              .with_height(AvailableSpace::Indefinite)
              .with_used_border_box_size(Some(border_size.width), None);

            child_fragment = if pos_child.id != 0 {
              if supports_used_border_box {
                crate::layout::style_override::with_style_override(
                  pos_child.id,
                  static_style.clone(),
                  || fc.layout(&pos_child, &measure_constraints),
                )?
              } else {
                let mut measure_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut measure_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.width_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                }
                crate::layout::style_override::with_style_override(
                  pos_child.id,
                  measure_style,
                  || fc.layout(&pos_child, &measure_constraints),
                )?
              }
            } else {
              let mut relayout_child = pos_child.clone();
              if supports_used_border_box {
                relayout_child.style = static_style.clone();
              } else {
                let mut measure_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut measure_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.width_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                }
                relayout_child.style = measure_style;
              }
              fc.layout(&relayout_child, &measure_constraints)?
            };

            input.intrinsic_size.height =
              (child_fragment.bounds.size.height - actual_vertical).max(0.0);

            let (_, rerun_result) =
              crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
                &abs,
                &input,
                &original_style,
                &positioning_cb,
                self.viewport_size,
                &self.font_context,
                anchors_for_cb,
                anchor_query,
              )?;
            result = rerun_result;
            border_size_physical = Size::new(
              result.size.width + actual_horizontal,
              result.size.height + actual_vertical,
            );
            border_origin_physical = Point::new(
              result.position.x - content_offset.x,
              result.position.y - content_offset.y,
            );
            (border_origin, border_size) = if needs_physical_conversion {
              let border_rect = Rect::new(border_origin_physical, border_size_physical);
              let logical_rect = physical_rect_to_logical_full(
                border_rect,
                box_width,
                box_height,
                style.writing_mode,
                style.direction,
              );
              (logical_rect.origin, logical_rect.size)
            } else {
              (border_origin_physical, border_size_physical)
            };
          }
          let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
            || (border_size.height - child_fragment.bounds.height()).abs() > 0.01
            || relayout_for_definite_insets;
          if needs_relayout {
            let relayout_constraints = child_constraints
              .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
            if pos_child.id != 0 {
              if supports_used_border_box {
                child_fragment = crate::layout::style_override::with_style_override(
                  pos_child.id,
                  static_style.clone(),
                  || fc.layout(&pos_child, &relayout_constraints),
                )?;
              } else {
                let mut relayout_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut relayout_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.height = Some(crate::style::values::Length::px(border_size.height));
                  s.width_keyword = None;
                  s.height_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                  s.min_height_keyword = None;
                  s.max_height_keyword = None;
                }
                child_fragment = crate::layout::style_override::with_style_override(
                  pos_child.id,
                  relayout_style,
                  || fc.layout(&pos_child, &relayout_constraints),
                )?;
              }
            } else {
              let mut relayout_child = pos_child.clone();
              if supports_used_border_box {
                relayout_child.style = static_style.clone();
              } else {
                let mut relayout_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut relayout_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.height = Some(crate::style::values::Length::px(border_size.height));
                  s.width_keyword = None;
                  s.height_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                  s.min_height_keyword = None;
                  s.max_height_keyword = None;
                }
                relayout_child.style = relayout_style;
              }
              child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
            }
          }
          child_fragment.bounds = Rect::new(border_origin, border_size);
          // Child fragments are translated from the block's content coordinate space into the
          // fragment-local (border-box) coordinate space via `content_origin` above.
          //
          // Keep out-of-flow positioned descendants consistent. When the positioned containing block
          // is *not* this element's own padding box (i.e. it's inherited from an ancestor), the
          // computed fragment position is still expressed in the content coordinate space and must
          // be translated alongside in-flow fragments.
          //
          // Viewport-fixed fragments are stored in absolute viewport coordinates, and positioned
          // elements whose containing block is this element's padding box are already computed in the
          // border-box coordinate space, so neither should be translated here.
          if cb != viewport_cb
            && cb != parent_padding_cb
            && (content_origin.x != 0.0 || content_origin.y != 0.0)
          {
            child_fragment.translate_root_in_place(content_origin);
          }
          child_fragment.style = Some(original_style);
          if matches!(child_fragment.style.as_deref().map(|s| s.position), Some(Position::Absolute))
          {
            child_fragment.abs_containing_block_box_id = cb.box_id();
          }
          if trace_positioned.contains(&pos_child.id) {
            let (text_count, total) = count_text_fragments(&child_fragment);
            let mut snippets = Vec::new();
            collect_first_texts(&child_fragment, &mut snippets, 3);
            eprintln!(
                        "[block-positioned-placed] child_id={} pos=({:.1},{:.1}) size=({:.1},{:.1}) texts={}/{} first_texts={:?}",
                        pos_child.id,
                        child_fragment.bounds.x(),
                        child_fragment.bounds.y(),
                        child_fragment.bounds.width(),
                        child_fragment.bounds.height(),
                        text_count,
                        total,
                        snippets
                    );
          }
          child_fragments.push(child_fragment);
        }
      }

      // Fragment bounds are stored in the coordinate system of the parent fragment. When a child
      // establishes an orthogonal writing mode, its logical inline/block sizes swap relative to the
      // parent's axes; map through physical space so in-flow layout and fragmentation observe the
      // correct block progression extents.
      let child_block_is_horizontal = block_axis_is_horizontal(style.writing_mode);
      let (phys_w, phys_h) = if child_block_is_horizontal {
        (box_height, box_width)
      } else {
        (box_width, box_height)
      };
      let parent_block_is_horizontal = block_axis_is_horizontal(parent.style.writing_mode);
      let (inline_size_in_parent, block_size_in_parent) = if parent_block_is_horizontal {
        (phys_h, phys_w)
      } else {
        (phys_w, phys_h)
      };
      let bounds = Rect::from_xywh(
        child_border_origin.x,
        box_y,
        inline_size_in_parent,
        block_size_in_parent,
      );

      let mut fragment = FragmentNode::new_with_style(
        bounds,
        crate::tree::fragment_tree::FragmentContent::Block {
          box_id: Some(child.id),
        },
        child_fragments,
        style_arc.clone(),
      );
      fragment.block_metadata = Some(BlockFragmentMetadata {
        margin_top,
        margin_bottom,
        ..BlockFragmentMetadata::default()
      });
      if let Some(info) = column_info {
        fragment.fragmentation = Some(info.clone());
        // Keep logical bounds aligned with the physical multi-column fragment geometry so
        // pagination uses the clipped height rather than the unfragmented flow height.
        fragment.logical_override = Some(fragment.bounds);
      }

      if !skip_contents {
        // Remember the laid out content-box size so skipped-content placeholder sizing can reuse it
        // in subsequent layout passes (`contain-intrinsic-size: auto`).
        let remembered = if block_axis_is_horizontal(style.writing_mode) {
          Size::new(height, computed_width.content_width)
        } else {
          Size::new(computed_width.content_width, height)
        };
        remembered_size_cache_store(child, remembered);
      }

      self.maybe_attach_footnote_anchor(
        child,
        containing_width,
        nearest_positioned_cb,
        nearest_fixed_cb,
        &mut fragment,
      )?;

      fragment.scrollbar_reservation = crate::layout::utils::scrollbar_reservation_for_style(style);
      return Ok(fragment);
    }

    // FastRender models scrollbars as overlay by default: scrollbars do not affect layout unless the
    // author opts into reserving space via `scrollbar-gutter: stable`.
    //
    // Historically, `layout_block_child` implemented a multi-pass "convergence" loop for
    // `overflow:auto` children to detect overflow and then relayout with forced scrollbars. Under
    // the overlay scrollbar model this is unnecessary (no gutter is reserved when
    // `scrollbar-gutter: auto`), and is extremely expensive for float-heavy layouts because each
    // pass clones and replays the float context.
    //
    // Keep the legacy behavior behind a runtime toggle for experimenting with classic
    // (layout-affecting) scrollbars, but default to a single layout pass.
    if !crate::debug::runtime::runtime_toggles().truthy("FASTR_CLASSIC_SCROLLBARS") {
      return crate::layout::auto_scrollbars::with_bypass(child, || {
        self.layout_block_child(
          parent,
          child,
          containing_width,
          constraints,
          box_y,
          nearest_positioned_cb,
          nearest_fixed_cb,
          external_float_ctx,
          external_float_base_x,
          external_float_base_y,
          paint_viewport,
        )
      });
    }

    let style_override = crate::layout::style_override::style_override_for(child.id);
    let base_style = style_override.unwrap_or_else(|| child.style.clone());
    let gutter = crate::layout::utils::resolve_scrollbar_width(&base_style);
    let mut external_float_ctx = external_float_ctx;
    if gutter <= 0.0
      || (!matches!(base_style.overflow_x, Overflow::Auto)
        && !matches!(base_style.overflow_y, Overflow::Auto))
    {
      return crate::layout::auto_scrollbars::with_bypass(child, || {
        self.layout_block_child(
          parent,
          child,
          containing_width,
          constraints,
          box_y,
          nearest_positioned_cb,
          nearest_fixed_cb,
          external_float_ctx,
          external_float_base_x,
          external_float_base_y,
          paint_viewport,
        )
      });
    }

    // `layout_block_child` can participate in float placement via `external_float_ctx`. When
    // resolving `overflow:auto` scrollbars we may need to run multiple layout passes (for x/y
    // scrollbar convergence).
    //
    // In the common case the child establishes a BFC via `overflow:auto` and does *not* insert
    // floats into the parent's float context (it only queries it for float avoidance). In that
    // case we can safely reuse the real float context across passes and avoid cloning a potentially
    // very large `FloatContext`.
    //
    // Be conservative: if the child itself is an in-flow float, layout may insert into the float
    // context, so we must run intermediate passes against a scratch clone and only commit once.
    let child_in_flow_float = base_style.float.is_floating()
      && matches!(base_style.position, Position::Static | Position::Relative);
    let mut scratch_float: Option<FloatContext> = if child_in_flow_float {
      external_float_ctx.as_deref().cloned()
    } else {
      None
    };
    let mut scratch_float_dirty = false;

    let mut force_x = false;
    let mut force_y = false;
    for _ in 0..3 {
      let mut override_style = base_style.clone();
      let mut overridden = false;
      {
        let s = Arc::make_mut(&mut override_style);
        if force_x
          && matches!(base_style.overflow_x, Overflow::Auto)
          && !base_style.scrollbar_gutter.stable
        {
          s.overflow_x = Overflow::Scroll;
          overridden = true;
        }
        if force_y
          && matches!(base_style.overflow_y, Overflow::Auto)
          && !base_style.scrollbar_gutter.stable
        {
          s.overflow_y = Overflow::Scroll;
          overridden = true;
        }
      }

      record_overflow_auto_child_layout_pass();
      let fragment = {
        if child_in_flow_float && scratch_float_dirty {
          // Reset the scratch context back to the caller's snapshot state before each convergence
          // pass.
          if let (Some(scratch), Some(snapshot)) = (scratch_float.as_mut(), external_float_ctx.as_deref()) {
            scratch.clone_from(snapshot);
          }
        }

        let float_ctx_for_pass = if child_in_flow_float {
          scratch_float.as_mut()
        } else {
          external_float_ctx.as_deref_mut()
        };

        if overridden && child.id != 0 {
          crate::layout::style_override::with_style_override(child.id, override_style.clone(), || {
            crate::layout::auto_scrollbars::with_bypass(child, || {
              self.layout_block_child(
                parent,
                child,
                containing_width,
                constraints,
                box_y,
                nearest_positioned_cb,
                nearest_fixed_cb,
                float_ctx_for_pass,
                external_float_base_x,
                external_float_base_y,
                paint_viewport,
              )
            })
          })
        } else if overridden {
          let mut cloned = child.clone();
          cloned.style = override_style.clone();
          crate::layout::auto_scrollbars::with_bypass(&cloned, || {
            self.layout_block_child(
              parent,
              &cloned,
              containing_width,
              constraints,
              box_y,
              nearest_positioned_cb,
              nearest_fixed_cb,
              float_ctx_for_pass,
              external_float_base_x,
              external_float_base_y,
              paint_viewport,
            )
          })
        } else {
          crate::layout::auto_scrollbars::with_bypass(child, || {
            self.layout_block_child(
              parent,
              child,
              containing_width,
              constraints,
              box_y,
              nearest_positioned_cb,
              nearest_fixed_cb,
              float_ctx_for_pass,
              external_float_base_x,
              external_float_base_y,
              paint_viewport,
            )
          })
        }
      }?;
      scratch_float_dirty = scratch_float_dirty || child_in_flow_float;

      let (overflow_x, overflow_y) = crate::layout::utils::fragment_overflows_content_box(
        &fragment,
        override_style.as_ref(),
        containing_width,
        self.viewport_size,
        Some(&self.font_context),
      );
      let need_x = gutter > 0.0
        && matches!(base_style.overflow_x, Overflow::Auto)
        && !base_style.scrollbar_gutter.stable
        && overflow_x;
      let need_y = gutter > 0.0
        && matches!(base_style.overflow_y, Overflow::Auto)
        && !base_style.scrollbar_gutter.stable
        && overflow_y;

      if need_x == force_x && need_y == force_y {
        if child_in_flow_float {
          if let (Some(dest), Some(updated)) = (external_float_ctx.as_deref_mut(), scratch_float) {
            *dest = updated;
          }
        }
        return Ok(fragment);
      }
      force_x = need_x;
      force_y = need_y;
    }

    // Run one final pass that mutates the caller's float context and returns the fragment.
    let mut final_style = base_style.clone();
    let mut final_overridden = false;
    {
      let s = Arc::make_mut(&mut final_style);
      if force_x
        && matches!(base_style.overflow_x, Overflow::Auto)
        && !base_style.scrollbar_gutter.stable
      {
        s.overflow_x = Overflow::Scroll;
        final_overridden = true;
      }
      if force_y
        && matches!(base_style.overflow_y, Overflow::Auto)
        && !base_style.scrollbar_gutter.stable
      {
        s.overflow_y = Overflow::Scroll;
        final_overridden = true;
      }
    }
    record_overflow_auto_child_layout_pass();
    if final_overridden && child.id != 0 {
      crate::layout::style_override::with_style_override(child.id, final_style.clone(), || {
        crate::layout::auto_scrollbars::with_bypass(child, || {
          self.layout_block_child(
            parent,
            child,
            containing_width,
            constraints,
            box_y,
            nearest_positioned_cb,
            nearest_fixed_cb,
            external_float_ctx,
            external_float_base_x,
            external_float_base_y,
            paint_viewport,
          )
        })
      })
    } else if final_overridden {
      let mut cloned = child.clone();
      cloned.style = final_style;
      crate::layout::auto_scrollbars::with_bypass(&cloned, || {
        self.layout_block_child(
          parent,
          &cloned,
          containing_width,
          constraints,
          box_y,
          nearest_positioned_cb,
          nearest_fixed_cb,
          external_float_ctx,
          external_float_base_x,
          external_float_base_y,
          paint_viewport,
        )
      })
    } else {
      crate::layout::auto_scrollbars::with_bypass(child, || {
        self.layout_block_child(
          parent,
          child,
          containing_width,
          constraints,
          box_y,
          nearest_positioned_cb,
          nearest_fixed_cb,
          external_float_ctx,
          external_float_base_x,
          external_float_base_y,
          paint_viewport,
        )
      })
    }
  }

  fn parallel_block_child_indices(parent: &BoxNode) -> Option<Vec<usize>> {
    if parent.children.is_empty() {
      return None;
    }
    let mut indices = Vec::with_capacity(parent.children.len());
    for (idx, child) in parent.children.iter().enumerate() {
      if is_ignorable_whitespace(child) {
        continue;
      }
      if !(child.is_block_level()
        && !child.style.float.is_floating()
        && child.style.clear == Clear::None
        && child.style.running_position.is_none()
        && matches!(
          child.style.content_visibility,
          crate::style::types::ContentVisibility::Visible
        )
        && matches!(child.style.position, Position::Static | Position::Relative))
      {
        return None;
      }
      indices.push(idx);
    }
    (!indices.is_empty()).then_some(indices)
  }

  fn subtree_contains_floats(node: &BoxNode) -> bool {
    // Parallel block layout currently assumes float layout is a no-op and provides each sibling with
    // a fresh float context. This is only correct if there are no floats anywhere in the subtree.
    //
    // Keep this conservative (treat any in-flow `float` as disabling parallelism) rather than
    // trying to reason about BFC boundaries; correctness > parallelism coverage.
    let mut stack: Vec<&BoxNode> = node.children.iter().collect();
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    while let Some(node) = stack.pop() {
      if node.style.float.is_floating()
        && matches!(node.style.position, Position::Static | Position::Relative)
      {
        return true;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter() {
        stack.push(child);
      }
    }
    false
  }

  fn subtree_contains_content_visibility_auto(node: &BoxNode) -> bool {
    // `content-visibility:auto` decisions depend on the element's final placement relative to the
    // viewport. Parallel block layout currently lays out siblings at `box_y=0` and translates the
    // fragment root afterward, which can change whether an auto subtree is considered "in view".
    //
    // Until the parallel path is able to translate viewport-relative state into each child before
    // layout (or otherwise make auto activation translation-invariant), conservatively disable
    // parallelization when any descendant opts into auto skipping.
    if matches!(
      node.style.content_visibility,
      crate::style::types::ContentVisibility::Auto
    ) {
      return true;
    }
    let mut stack: Vec<&BoxNode> = node.children.iter().collect();
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    while let Some(node) = stack.pop() {
      if matches!(
        node.style.content_visibility,
        crate::style::types::ContentVisibility::Auto
      ) {
        return true;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter() {
        stack.push(child);
      }
    }
    false
  }

  fn translate_fragment_tree(fragment: &mut FragmentNode, delta: Point) {
    if delta.x == 0.0 && delta.y == 0.0 {
      return;
    }
    // `FragmentNode` positions are stored in the coordinate space of their parent
    // fragment. Child bounds (and `scroll_overflow`) are expressed in the fragment's
    // local coordinate space, so adjusting a block's placement within its parent
    // should only translate the fragment root, not its descendants.
    fragment.bounds = fragment.bounds.translate(delta);
    fragment.logical_override = fragment
      .logical_override
      .map(|logical| logical.translate(delta));
  }

  /// Cancels a parent-applied translation for out-of-flow positioned descendants whose containing
  /// blocks live outside the translated subtree.
  ///
  /// In the parallel block-children path we lay out each child at `box_y=0` (because its final
  /// vertical placement depends on previous siblings). After all children complete we translate the
  /// child's fragment root into its final position.
  ///
  /// For in-flow content, translating only the fragment root is correct because descendants are
  /// expressed in the parent's local coordinate space.
  ///
  /// Out-of-flow positioned descendants (e.g. `position:absolute` whose containing block is an
  /// ancestor of this child, or `position:fixed` relative to a non-viewport fixed containing block)
  /// *must not* inherit this translation: their bounds are resolved against an external containing
  /// block, so moving the subtree root would incorrectly offset them.
  ///
  /// Viewport-fixed fragments are stored in absolute viewport coordinates, so they already ignore
  /// the fragment-root translation; cancellation is only needed when fixed positioning resolves
  /// against an external (non-viewport) fixed containing block.
  ///
  /// Serial block layout avoids this by translating viewport-relative state into the child's
  /// coordinate space before running layout (see `FormattingContextFactory::translated_for_child`).
  /// The parallel path currently can't do that because it doesn't know `box_y` yet, so we correct
  /// the resulting fragment tree here.
  fn cancel_translation_for_out_of_flow_positioned_descendants(
    fragment: &mut FragmentNode,
    delta: Point,
    external_fixed_cb: bool,
  ) {
    if delta.x == 0.0 && delta.y == 0.0 {
      return;
    }
    let cancel = Point::new(-delta.x, -delta.y);
    fn walk(
      node: &mut FragmentNode,
      cancel: Point,
      has_abs_cb: bool,
      has_fixed_cb: bool,
      external_fixed_cb: bool,
    ) {
      let (has_abs_cb_here, has_fixed_cb_here) = node
        .style
        .as_deref()
        .map(|style| {
          (
            has_abs_cb || style.establishes_abs_containing_block(),
            has_fixed_cb || style.establishes_fixed_containing_block(),
          )
        })
        .unwrap_or((has_abs_cb, has_fixed_cb));
      for child in node.children_mut() {
        let is_abs = child
          .style
          .as_deref()
          .is_some_and(|style| matches!(style.position, Position::Absolute));
        let is_fixed = child
          .style
          .as_deref()
          .is_some_and(|style| matches!(style.position, Position::Fixed));
        let needs_cancel =
          (is_abs && !has_abs_cb_here) || (is_fixed && external_fixed_cb && !has_fixed_cb_here);
        if needs_cancel {
          child.bounds = child.bounds.translate(cancel);
          child.logical_override = child
            .logical_override
            .map(|logical| logical.translate(cancel));
          // The entire subtree under this out-of-flow fragment inherits the cancelled translation,
          // so avoid double-applying it to descendants.
          continue;
        }
        walk(
          child,
          cancel,
          has_abs_cb_here,
          has_fixed_cb_here,
          external_fixed_cb,
        );
      }
    }
    let has_abs_cb_root = fragment
      .style
      .as_deref()
      .is_some_and(|style| style.establishes_abs_containing_block());
    let has_fixed_cb_root = fragment
      .style
      .as_deref()
      .is_some_and(|style| style.establishes_fixed_containing_block());
    walk(
      fragment,
      cancel,
      has_abs_cb_root,
      has_fixed_cb_root,
      external_fixed_cb,
    );
  }

  #[allow(clippy::too_many_arguments)]
  fn try_parallel_block_children(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    margin_ctx: MarginCollapseContext,
    relative_cb: &ContainingBlock,
    containing_width: f32,
    float_ctx_empty: bool,
    paint_viewport: Rect,
  ) -> Option<Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>), LayoutError>> {
    if !float_ctx_empty || Self::is_multicol_container(&parent.style) {
      return None;
    }
    if crate::layout::formatting_context::fragmentainer_block_size_hint().is_some() {
      return None;
    }
    // Most HTML documents include ignorable whitespace-only text nodes between block elements.
    // Serial block layout explicitly skips these nodes (CSS 2.1 §16.6). Treat them as non-existent
    // for parallel fan-out so we can still parallelize wide sibling lists from real markup.
    let child_indices = Self::parallel_block_child_indices(parent)?;
    if !self.parallelism.should_parallelize(child_indices.len())
      || Self::subtree_contains_floats(parent)
      || Self::subtree_contains_content_visibility_auto(parent)
    {
      return None;
    }
    let deadline = active_deadline();
    let stage = active_stage();
    let heartbeat = active_heartbeat();
    let child_layout_ctx = self.clone();
    let parallel_results = child_indices
      .par_iter()
      .map(|idx| {
        let child = &parent.children[*idx];
        with_deadline(deadline.as_ref(), || {
          let _hb_guard = StageHeartbeatGuard::install(heartbeat);
          let _stage_guard = StageGuard::install(stage);
          child_layout_ctx.factory.debug_record_parallel_work();
          let fragment = child_layout_ctx.layout_block_child(
            parent,
            child,
            containing_width,
            constraints,
            0.0,
            nearest_positioned_cb,
            nearest_fixed_cb,
            None,
            0.0,
            0.0,
            paint_viewport,
          )?;
          let meta = fragment.block_metadata.clone().ok_or_else(|| {
            LayoutError::MissingContext(
              "Block fragment missing metadata for parallel layout".into(),
            )
          })?;
          Ok((*idx, fragment, meta))
        })
      })
      .collect::<Result<Vec<_>, LayoutError>>();

    let mut parallel_results = match parallel_results {
      Ok(results) => results,
      Err(err) => return Some(Err(err)),
    };
    // `child_indices` is collected in DOM order, and Rayon will usually preserve that order when
    // collecting into a Vec. Avoid paying an unconditional stable `sort_by_key` (which allocates
    // and caches keys) unless the collected results are actually out of order.
    let mut ordered = true;
    let mut prev_idx: Option<usize> = None;
    for (idx, _, _) in &parallel_results {
      if let Some(prev) = prev_idx {
        if *idx <= prev {
          ordered = false;
          break;
        }
      }
      prev_idx = Some(*idx);
    }
    if !ordered {
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Some(Err(LayoutError::Timeout { elapsed }));
      }
      parallel_results.sort_unstable_by_key(|(idx, _, _)| *idx);
    }

    let mut fragments = Vec::with_capacity(parallel_results.len());
    let mut content_height: f32 = 0.0;
    let mut current_y = 0.0;
    let mut margin_ctx = margin_ctx;
    let child_margin_mode = if parent.id == 1 && self.factory.quirks_mode() == QuirksMode::Quirks {
      CollapsedMarginMode::QuirksRootChild
    } else {
      CollapsedMarginMode::Normal
    };

    for (idx, mut fragment, _meta) in parallel_results {
      let child = &parent.children[idx];
      let child_margins = self.collapsed_block_margins(child, containing_width, child_margin_mode);

      let at_start_before = margin_ctx.is_at_start();
      let pending_margin = margin_ctx.pending_collapsible_margin();

      let flow_box_y = if at_start_before && !child_margins.collapsible_through {
        // Parent/first-child margin collapsing is represented by the parent’s own collapsed
        // margins. Discard any leading collapsible-through margins and place the first
        // non-empty child at the block start.
        margin_ctx.consume_pending();
        margin_ctx.push_collapsible_margin(child_margins.bottom);
        current_y
      } else {
        let (offset, _) = margin_ctx.process_child_margins(
          child_margins.top,
          child_margins.bottom,
          child_margins.collapsible_through,
        );
        current_y + offset
      };

      let layout_box_y = if child_margins.collapsible_through && !at_start_before {
        let collapsed_top = pending_margin.collapse_with(child_margins.top).resolve();
        current_y + collapsed_top
      } else {
        flow_box_y
      };

      let delta = layout_box_y - fragment.bounds.y();
      let delta = Point::new(0.0, delta);
      Self::translate_fragment_tree(&mut fragment, delta);
      let viewport_fixed_cb = ContainingBlock::viewport(self.viewport_size);
      let external_fixed_cb = *nearest_fixed_cb != viewport_fixed_cb;
      Self::cancel_translation_for_out_of_flow_positioned_descendants(
        &mut fragment,
        delta,
        external_fixed_cb,
      );

      let block_extent = if child_margins.collapsible_through {
        0.0
      } else {
        fragment.bounds.height()
      };
      let next_y = flow_box_y + block_extent;
      content_height = content_height.max(next_y);
      current_y = next_y;

      if matches!(child.style.position, Position::Relative) {
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
          &child.style,
          relative_cb,
          self.viewport_size,
          &self.font_context,
        );
        fragment = match PositionedLayout::with_font_context(self.font_context.clone())
          .apply_relative_positioning(&fragment, &positioned_style, relative_cb)
        {
          Ok(f) => f,
          Err(err) => return Some(Err(err)),
        };
      }

      fragments.push(fragment);
    }

    let trailing_margin = margin_ctx.pending_margin();
    let parent_is_independent_context_root =
      parent.id == 1 || self.independent_context_root_id == Some(parent.id);
    let allow_collapse_last =
      !parent_is_independent_context_root && should_collapse_with_last_child(&parent.style);
    let (_, parent_block_end) = block_axis_sides(&parent.style);
    let root_font_metrics = self.factory.root_font_metrics();
    let parent_has_bottom_separation = resolve_border_side(
      &parent.style,
      parent_block_end,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    ) > 0.0
      || resolve_padding_side(
        &parent.style,
        parent_block_end,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      ) > 0.0;

    let mut flow_height = current_y;
    if !allow_collapse_last || parent_has_bottom_separation {
      // Trailing margins apply after the last in-flow cursor (CSS 2.1 §10.6.3). The used height is
      // based on the cursor position, not the maximum descendant extent, so negative trailing
      // margins can shrink the auto-height and allow earlier content to overflow
      // (`overflow: visible` default).
      flow_height += trailing_margin;
    }
    if !flow_height.is_finite() {
      flow_height = 0.0;
    }
    flow_height = flow_height.max(0.0);

    Some(Ok((fragments, flow_height, Vec::new())))
  }

  fn layout_replaced_child(
    &self,
    parent: &BoxNode,
    child: &BoxNode,
    replaced_box: &ReplacedBox,
    containing_width: f32,
    constraints: &LayoutConstraints,
    box_y: f32,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
  ) -> Result<FragmentNode, LayoutError> {
    let style = &child.style;
    let root_font_metrics = self.factory.root_font_metrics();
    let toggles = crate::debug::runtime::runtime_toggles();
    let log_wide_flex = toggles.truthy("FASTR_LOG_WIDE_FLEX");
    let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);

    // Percentages on replaced elements resolve against the containing block size (width/height
    // when available). Even if the block height is indefinite, we still have a valid width
    // percentage base, which allows max-width: 100% (UA default) to clamp oversized images.
    let percentage_base = Some(crate::geometry::Size::new(
      containing_width,
      constraints.height().unwrap_or(f32::NAN),
    ));
    let used_size = compute_replaced_size(style, replaced_box, percentage_base, self.viewport_size);
    let used_inline = if inline_is_horizontal {
      used_size.width
    } else {
      used_size.height
    };
    let used_block = if inline_is_horizontal {
      used_size.height
    } else {
      used_size.width
    };
    if log_wide_flex && used_inline > containing_width + 0.5 {
      let resolved_max_w = style.max_width.as_ref().map(|l| {
        resolve_length_for_width(
          *l,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        )
      });
      let resolved_min_w = style.min_width.as_ref().map(|l| {
        resolve_length_for_width(
          *l,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        )
      });
      let selector = child
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anonymous>".to_string());
      eprintln!(
                "[replaced-wide] child_id={:?} selector={} used_w={:.1} used_h={:.1} containing_w={:.1} max_w={:?} min_w={:?}",
                child.id,
                selector,
                used_size.width,
                used_size.height,
                containing_width,
                resolved_max_w,
                resolved_min_w
            );
    }

    // Block-axis margins (used for fragmentation adjustments).
    let block_sides = block_axis_sides(style);
    let margin_top = resolve_margin_side(
      style,
      block_sides.0,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    );
    let margin_bottom = resolve_margin_side(
      style,
      block_sides.1,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    );

    // Use the resolved replaced width when computing horizontal metrics
    let mut width_style = style.clone();
    {
      let s = Arc::make_mut(&mut width_style);
      if inline_is_horizontal {
        s.width = Some(Length::px(used_inline));
        s.width_keyword = None;
      } else {
        s.height = Some(Length::px(used_inline));
        s.height_keyword = None;
      }
      s.box_sizing = crate::style::types::BoxSizing::ContentBox;
    }
    let inline_sides = inline_axis_sides(style);
    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);
    let computed_width = compute_block_width(
      &width_style,
      containing_width,
      self.viewport_size,
      root_font_metrics,
      inline_sides,
      inline_positive,
      &self.font_context,
    );

    let box_width = computed_width.border_box_width();
    let block_edges = if inline_is_horizontal {
      vertical_padding_and_borders(
        style,
        containing_width,
        self.viewport_size,
        &self.font_context,
        root_font_metrics,
      )
    } else {
      horizontal_padding_and_borders(
        style,
        containing_width,
        self.viewport_size,
        &self.font_context,
        root_font_metrics,
      )
    };
    let box_height = (used_block + block_edges).max(0.0);
    // Fragment bounds are stored in the coordinate system of the parent fragment. When the
    // replaced element establishes an orthogonal writing mode, its logical inline/block sizes are
    // swapped relative to the parent's axes; map through physical space so in-flow layout and
    // fragmentation observe the correct block progression extents.
    let child_block_is_horizontal = block_axis_is_horizontal(style.writing_mode);
    let (phys_w, phys_h) = if child_block_is_horizontal {
      (box_height, box_width)
    } else {
      (box_width, box_height)
    };
    let parent_block_is_horizontal = block_axis_is_horizontal(parent.style.writing_mode);
    let (inline_size_in_parent, block_size_in_parent) = if parent_block_is_horizontal {
      (phys_h, phys_w)
    } else {
      (phys_w, phys_h)
    };
    let bounds = Rect::from_xywh(
      computed_width.margin_left,
      box_y,
      inline_size_in_parent,
      block_size_in_parent,
    );

    let mut fragment = FragmentNode::new_with_style(
      bounds,
      FragmentContent::Replaced {
        replaced_type: replaced_box.replaced_type.clone(),
        box_id: Some(child.id),
      },
      vec![],
      child.style.clone(),
    );
    fragment.block_metadata = Some(BlockFragmentMetadata {
      margin_top,
      margin_bottom,
      ..BlockFragmentMetadata::default()
    });

    // Replaced elements are usually treated as layout leaves, but form controls can have generated
    // ::before/::after pseudo-element children. Only out-of-flow pseudo boxes are generated, so
    // lay them out here.
    if !child.children.is_empty() {
      let border_top = resolve_border_side(
        style,
        block_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let border_bottom = resolve_border_side(
        style,
        block_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let padding_rect = Rect::from_xywh(
        computed_width.border_left,
        border_top,
        (box_width - computed_width.border_left - computed_width.border_right).max(0.0),
        (box_height - border_top - border_bottom).max(0.0),
      );
      let padding_cb = ContainingBlock::with_viewport_and_bases(
        padding_rect,
        self.viewport_size,
        Some(padding_rect.size.width),
        Some(padding_rect.size.height),
      )
      .with_writing_mode_and_direction(style.writing_mode, style.direction)
      .with_box_id(Some(child.id));

      let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
        self.font_context.clone(),
      );
      // Translate the inherited containing blocks into the replaced element's local coordinate
      // space so abs/fixed positioned pseudo-elements resolve against the same reference boxes as
      // normal descendants would.
      let base_factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
      let mut factory = base_factory.translated_for_child(bounds.origin);
      if style.establishes_abs_containing_block() && padding_cb != factory.nearest_positioned_cb() {
        factory = factory.with_positioned_cb(padding_cb);
      }
      if style.establishes_fixed_containing_block() && padding_cb != factory.nearest_fixed_cb() {
        factory = factory.with_fixed_cb(padding_cb);
      }
      for positioned_child in child
        .children
        .iter()
        .filter(|desc| desc.style.position.is_absolutely_positioned())
      {
        let original_style = positioned_child.style.clone();
        let mut layout_child = positioned_child.clone();
        let mut child_style = layout_child.style.clone();
        {
          let s = Arc::make_mut(&mut child_style);
          s.position = Position::Relative;
          s.top = crate::style::types::InsetValue::Auto;
          s.right = crate::style::types::InsetValue::Auto;
          s.bottom = crate::style::types::InsetValue::Auto;
          s.left = crate::style::types::InsetValue::Auto;
        }
        layout_child.style = child_style;

        let fc_type = layout_child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = factory.get(fc_type);
        let cb = if matches!(original_style.position, Position::Fixed) {
          factory.nearest_fixed_cb()
        } else {
          factory.nearest_positioned_cb()
        };
        let child_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(cb.rect.size.width),
          cb.block_percentage_base()
            .map(AvailableSpace::Definite)
            .unwrap_or(AvailableSpace::Indefinite),
        );
        let mut child_fragment = fc.layout(&layout_child, &child_constraints)?;
        let implicit_anchor_box_id = positioned_child.implicit_anchor_box_id;
        let mut positioned_style =
          crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
            &original_style,
            &cb,
            self.viewport_size,
            &self.font_context,
            None,
            crate::layout::anchor_positioning::AnchorQueryContext {
              query_parent_box_id: Some(child.id),
              implicit_anchor_box_id,
            },
          );
        positioned_style.width_keyword = original_style.width_keyword;
        positioned_style.min_width_keyword = original_style.min_width_keyword;
        positioned_style.max_width_keyword = original_style.max_width_keyword;
        positioned_style.height_keyword = original_style.height_keyword;
        positioned_style.min_height_keyword = original_style.min_height_keyword;
        positioned_style.max_height_keyword = original_style.max_height_keyword;

        let relayout_for_definite_insets = (positioned_style.width.is_auto()
          && !positioned_style.left.is_auto()
          && !positioned_style.right.is_auto())
          || (positioned_style.height.is_auto()
            && !positioned_style.top.is_auto()
            && !positioned_style.bottom.is_auto());

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
        let intrinsic_size = Size::new(
          (child_fragment.bounds.size.width - actual_horizontal).max(0.0),
          (child_fragment.bounds.size.height - actual_vertical).max(0.0),
        );

        let mut input = crate::layout::absolute_positioning::AbsoluteLayoutInput::new(
          positioned_style,
          intrinsic_size,
          Point::ZERO,
        );
        input.is_replaced = positioned_child.is_replaced();

        let mut result = abs.layout_absolute(&input, &cb)?;
        let mut border_size = Size::new(
          result.size.width + actual_horizontal,
          result.size.height + actual_vertical,
        );
        let mut border_origin = Point::new(
          result.position.x - content_offset.x,
          result.position.y - content_offset.y,
        );

        if crate::layout::absolute_positioning::auto_height_uses_intrinsic_size(
          &input.style,
          input.is_replaced,
        ) && (border_size.width - child_fragment.bounds.width()).abs() > 0.01
        {
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );
          let is_table = matches!(fc_type, FormattingContextType::Table);
          let measure_constraints = child_constraints
            .with_width(AvailableSpace::Definite(border_size.width))
            .with_height(AvailableSpace::Indefinite)
            .with_used_border_box_size(Some(border_size.width), None);
          if supports_used_border_box
            && (is_table
              || (layout_child.style.width.is_none() && layout_child.style.width_keyword.is_none()))
          {
            child_fragment = fc.layout(&layout_child, &measure_constraints)?;
          } else {
            let mut measure_child = layout_child.clone();
            let mut measure_style = (*measure_child.style).clone();
            measure_style.width = Some(Length::px(border_size.width));
            measure_style.width_keyword = None;
            measure_style.min_width_keyword = None;
            measure_style.max_width_keyword = None;
            measure_child.style = Arc::new(measure_style);
            child_fragment = fc.layout(&measure_child, &measure_constraints)?;
          }

          input.intrinsic_size.height =
            (child_fragment.bounds.size.height - actual_vertical).max(0.0);
          result = abs.layout_absolute(&input, &cb)?;
          border_size = Size::new(
            result.size.width + actual_horizontal,
            result.size.height + actual_vertical,
          );
          border_origin = Point::new(
            result.position.x - content_offset.x,
            result.position.y - content_offset.y,
          );
        }
        let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          || (border_size.height - child_fragment.bounds.height()).abs() > 0.01
          || relayout_for_definite_insets;
        if needs_relayout {
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );
          let is_table = matches!(fc_type, FormattingContextType::Table);
          let relayout_constraints = child_constraints
            .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
          let width_auto =
            layout_child.style.width.is_none() && layout_child.style.width_keyword.is_none();
          let height_auto =
            layout_child.style.height.is_none() && layout_child.style.height_keyword.is_none();
          if supports_used_border_box && (is_table || (width_auto && height_auto)) {
            child_fragment = fc.layout(&layout_child, &relayout_constraints)?;
          } else {
            let mut relayout_style = layout_child.style.clone();
            {
              let s = Arc::make_mut(&mut relayout_style);
              s.width = Some(Length::px(border_size.width));
              s.height = Some(Length::px(border_size.height));
              s.width_keyword = None;
              s.height_keyword = None;
              s.min_width_keyword = None;
              s.max_width_keyword = None;
              s.min_height_keyword = None;
              s.max_height_keyword = None;
            }
            layout_child.style = relayout_style;
            child_fragment = fc.layout(&layout_child, &relayout_constraints)?;
          }
        }
        child_fragment.bounds = Rect::new(border_origin, border_size);
        child_fragment.style = Some(original_style);
        if matches!(child_fragment.style.as_deref().map(|s| s.position), Some(Position::Absolute))
        {
          child_fragment.abs_containing_block_box_id = cb.box_id();
        }
        match &mut child_fragment.content {
          FragmentContent::Block { box_id: id } => *id = Some(positioned_child.id),
          FragmentContent::Inline { box_id: id, .. } => *id = Some(positioned_child.id),
          FragmentContent::Text { box_id: id, .. } => *id = Some(positioned_child.id),
          FragmentContent::Replaced { box_id: id, .. } => *id = Some(positioned_child.id),
          FragmentContent::Line { .. }
          | FragmentContent::RunningAnchor { .. }
          | FragmentContent::FootnoteAnchor { .. } => {}
        }
        fragment.children_mut().push(child_fragment);
      }
    }

    Ok(fragment)
  }

  fn collapsed_block_margins(
    &self,
    node: &BoxNode,
    containing_width: f32,
    mode: CollapsedMarginMode,
  ) -> CollapsedBlockMargins {
    let mut cache: FxHashMap<(usize, CollapsedMarginMode), CollapsedBlockMargins> =
      FxHashMap::default();
    self.collapsed_block_margins_cached(node, containing_width, mode, &mut cache)
  }

  fn collapsed_block_margins_cached(
    &self,
    node: &BoxNode,
    containing_width: f32,
    mode: CollapsedMarginMode,
    cache: &mut FxHashMap<(usize, CollapsedMarginMode), CollapsedBlockMargins>,
  ) -> CollapsedBlockMargins {
    record_collapsed_block_margins_call();
    if let Some(cached) = cache.get(&(node.id, mode)) {
      return *cached;
    }

    let style = &node.style;
    let block_sides = block_axis_sides(style);
    let root_font_metrics = self.factory.root_font_metrics();
    let margin_order_for_side = |style: &ComputedStyle, side: PhysicalSide| -> i32 {
      match side {
        PhysicalSide::Top => style.logical.margin_orders.top,
        PhysicalSide::Right => style.logical.margin_orders.right,
        PhysicalSide::Bottom => style.logical.margin_orders.bottom,
        PhysicalSide::Left => style.logical.margin_orders.left,
      }
    };
    let margin_is_user_agent = |style: &ComputedStyle, side: PhysicalSide| -> bool {
      matches!(
        crate::style::cascade_order_origin(margin_order_for_side(style, side)),
        Some(crate::style::CascadeOrderOrigin::UserAgent)
      )
    };

    let suppress_ua_margins = matches!(mode, CollapsedMarginMode::QuirksSuppressUserAgent);
    let mut margin_top = resolve_margin_side(
      style,
      block_sides.0,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    );
    if suppress_ua_margins && margin_is_user_agent(style, block_sides.0) {
      margin_top = 0.0;
    }
    let mut margin_bottom = resolve_margin_side(
      style,
      block_sides.1,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    );
    if suppress_ua_margins && margin_is_user_agent(style, block_sides.1) {
      margin_bottom = 0.0;
    }
    let mut top = CollapsibleMargin::from_margin(margin_top);
    let mut bottom = CollapsibleMargin::from_margin(margin_bottom);

    // Root element margins never collapse with children (CSS 2.1 §8.3.1).
    let is_root = node.id == 1;
    let mut collapse_first = !is_root && should_collapse_with_first_child(style);
    let mut collapse_last = !is_root && should_collapse_with_last_child(style);

    let is_out_of_flow_or_float = |child: &BoxNode| -> bool {
      child.style.running_position.is_some()
        || matches!(child.style.position, Position::Absolute | Position::Fixed)
        || child.style.float.is_floating()
    };

    let subtree_contains_in_flow_float = |root: &BoxNode| -> bool {
      if establishes_bfc(&root.style) {
        return false;
      }
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if node.style.running_position.is_some()
          || matches!(node.style.position, Position::Absolute | Position::Fixed)
        {
          continue;
        }
        if node.style.float.is_floating() {
          return true;
        }
        if establishes_bfc(&node.style) {
          continue;
        }
        for child in &node.children {
          stack.push(child);
        }
      }
      false
    };
    let subtree_float_sides = |root: &BoxNode| -> (bool, bool) {
      if establishes_bfc(&root.style) {
        return (false, false);
      }
      let mut seen_left = false;
      let mut seen_right = false;
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if node.style.running_position.is_some()
          || matches!(node.style.position, Position::Absolute | Position::Fixed)
        {
          continue;
        }
        if node.style.float.is_floating() {
          if let Some(side) =
            resolve_float_side(node.style.float, style.writing_mode, style.direction)
          {
            match side {
              FloatSide::Left => seen_left = true,
              FloatSide::Right => seen_right = true,
            }
            if seen_left && seen_right {
              return (true, true);
            }
          }
        }
        if establishes_bfc(&node.style) {
          continue;
        }
        for child in &node.children {
          stack.push(child);
        }
      }
      (seen_left, seen_right)
    };

    let inline_subtree_generates_line_boxes = |root: &BoxNode| -> bool {
      let mut stack = vec![root];
      while let Some(node) = stack.pop() {
        if is_out_of_flow_or_float(node) {
          continue;
        }
        match &node.box_type {
          BoxType::Text(text_box) => {
            if !trim_ascii_whitespace(&text_box.text).is_empty()
              || matches!(
                node.style.white_space,
                crate::style::types::WhiteSpace::Pre | crate::style::types::WhiteSpace::PreWrap
              )
            {
              return true;
            }
          }
          BoxType::LineBreak(_) => return true,
          BoxType::Replaced(_) => return true,
          BoxType::Marker(_) => return true,
          BoxType::Inline(_)
          | BoxType::Anonymous(crate::tree::box_tree::AnonymousBox {
            anonymous_type: crate::tree::box_tree::AnonymousType::Inline,
            ..
          }) => {
            // Inline boxes with non-zero edges can generate fragments even if they contain no
            // text. If we treated them as ignorable, we'd incorrectly allow margin collapsing
            // through a visually non-empty block.
            let style = &node.style;
            let has_edges = !style.padding_top.is_zero()
              || !style.padding_right.is_zero()
              || !style.padding_bottom.is_zero()
              || !style.padding_left.is_zero()
              || !style.used_border_top_width().is_zero()
              || !style.used_border_right_width().is_zero()
              || !style.used_border_bottom_width().is_zero()
              || !style.used_border_left_width().is_zero()
              || style.margin_top.as_ref().is_some_and(|m| !m.is_zero())
              || style.margin_right.as_ref().is_some_and(|m| !m.is_zero())
              || style.margin_bottom.as_ref().is_some_and(|m| !m.is_zero())
              || style.margin_left.as_ref().is_some_and(|m| !m.is_zero())
              || node.formatting_context().is_some();

            if has_edges {
              return true;
            }
            for child in &node.children {
              stack.push(child);
            }
          }
          _ => return true,
        }
      }
      false
    };

    let is_ignorable_whitespace = |child: &BoxNode| -> bool {
      if matches!(&child.box_type, BoxType::Text(text_box)
      if trim_ascii_whitespace(&text_box.text).is_empty()
        && !matches!(
          child.style.white_space,
          crate::style::types::WhiteSpace::Pre | crate::style::types::WhiteSpace::PreWrap
        ))
      {
        return true;
      }

      // Inline boxes that collapse away entirely (e.g. an empty span with only collapsible
      // whitespace) do not generate line boxes. Treat them like ignorable whitespace so they do
      // not prevent margin collapsing through an otherwise-empty block (CSS 2.1 §8.3.1).
      match &child.box_type {
        BoxType::Inline(_)
        | BoxType::Anonymous(crate::tree::box_tree::AnonymousBox {
          anonymous_type: crate::tree::box_tree::AnonymousType::Inline,
          ..
        }) => !inline_subtree_generates_line_boxes(child),
        _ => false,
      }
    };

    let is_in_flow_block = |child: &BoxNode| -> bool {
      if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
        return false;
      }
      child.is_block_level()
        || matches!(child.box_type, BoxType::Replaced(_) if !child.style.display.is_inline_level())
    };

    // If the first/last in-flow content is not a block-level box (i.e., line boxes would be
    // generated first/last), parent/child margin collapsing cannot occur.
    if collapse_first {
      if let Some(first) = node
        .children
        .iter()
        .find(|c| !is_out_of_flow_or_float(c) && !is_ignorable_whitespace(c))
      {
        if !is_in_flow_block(first) {
          collapse_first = false;
        }
      }
    }
    if collapse_last {
      if let Some(last) = node
        .children
        .iter()
        .rev()
        .find(|c| !is_out_of_flow_or_float(c) && !is_ignorable_whitespace(c))
      {
        if !is_in_flow_block(last) {
          collapse_last = false;
        }
      }
    }

    let child_mode = if matches!(mode, CollapsedMarginMode::QuirksRootChild) {
      CollapsedMarginMode::QuirksSuppressUserAgent
    } else {
      mode
    };

    if collapse_first {
      let mut chain = CollapsibleMargin::ZERO;
      let mut seen_left_float = false;
      let mut seen_right_float = false;
      for child in &node.children {
        if child.style.float.is_floating()
          && !matches!(child.style.position, Position::Absolute | Position::Fixed)
        {
          if let Some(side) =
            resolve_float_side(child.style.float, style.writing_mode, style.direction)
          {
            match side {
              FloatSide::Left => seen_left_float = true,
              FloatSide::Right => seen_right_float = true,
            }
          }
        }
        if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
          continue;
        }
        if !is_in_flow_block(child) {
          break;
        }
        // Clearance (from `clear`) breaks margin adjoining, so parent/first-child margin collapsing
        // must not tunnel past floats into a cleared block (CSS 2.1 §9.5.2 / §8.3.1).
        if seen_left_float || seen_right_float {
          let clear_side =
            resolve_clear_side(child.style.clear, style.writing_mode, style.direction);
          let clears_seen_float = (clear_side.clears_left() && seen_left_float)
            || (clear_side.clears_right() && seen_right_float);
          if clears_seen_float {
            break;
          }
        }
        let child_margins =
          self.collapsed_block_margins_cached(child, containing_width, child_mode, cache);
        if child_margins.collapsible_through {
          chain = chain.collapse_with(child_margins.top.collapse_with(child_margins.bottom));
          // Track floats in collapsible-through subtrees so we can stop tunneling into later
          // `clear` blocks. On float-based two-column layouts, anonymous wrappers can contain floats
          // while remaining "empty" for margin-collapsing; those floats still introduce clearance
          // that breaks adjoining margins.
          let (left, right) = subtree_float_sides(child);
          seen_left_float |= left;
          seen_right_float |= right;
          continue;
        }
        chain = chain.collapse_with(child_margins.top);
        break;
      }
      top = top.collapse_with(chain);
    }

    if collapse_last {
      let mut chain = CollapsibleMargin::ZERO;
      for child in node.children.iter().rev() {
        if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
          continue;
        }
        if !is_in_flow_block(child) {
          break;
        }
        let child_margins =
          self.collapsed_block_margins_cached(child, containing_width, child_mode, cache);
        if child_margins.collapsible_through {
          chain = chain.collapse_with(child_margins.top.collapse_with(child_margins.bottom));
          continue;
        }
        chain = chain.collapse_with(child_margins.bottom);
        break;
      }
      bottom = bottom.collapse_with(chain);
    }

    // Collapsing through an empty block (CSS 2.1 §8.3.1).
    let mut has_in_flow_content = matches!(node.box_type, BoxType::Replaced(_));
    if !has_in_flow_content {
      let mut seen_float = false;
      // Floats do not generate line boxes, but when this block establishes a new BFC (e.g.
      // `overflow:hidden` / `display: flow-root`) they contribute to the block's used auto size.
      // Such blocks are therefore not "empty" for margin-collapsing-through purposes.
      let float_children_extend_auto_block_size = establishes_bfc(style)
        && (if block_axis_is_horizontal(style.writing_mode) {
          style.width.is_none() && style.width_keyword.is_none()
        } else {
          style.height.is_none() && style.height_keyword.is_none()
        });
      for child in &node.children {
        if child.style.float.is_floating()
          && !matches!(child.style.position, Position::Absolute | Position::Fixed)
        {
          seen_float = true;
          if float_children_extend_auto_block_size {
            has_in_flow_content = true;
            break;
          }
        }
        if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
          continue;
        }
        if is_in_flow_block(child) {
          if seen_float && child.style.clear != Clear::None {
            has_in_flow_content = true;
            break;
          }
          let child_margins =
            self.collapsed_block_margins_cached(child, containing_width, child_mode, cache);
          if !child_margins.collapsible_through {
            has_in_flow_content = true;
            break;
          }
          if subtree_contains_in_flow_float(child) {
            seen_float = true;
          }
          continue;
        }
        // Inline-level in-flow content generates line boxes and prevents collapsing-through.
        has_in_flow_content = true;
        break;
      }

      // Block formatting context roots (e.g. `overflow:hidden` clearfix containers) include floats
      // in their used block size. Even though floats are out of normal flow, their presence means
      // the box is not "empty" for the purposes of collapsing margins through it (CSS 2.1 §8.3.1).
      //
      // Without this, float-only BFC roots can be misclassified as collapsible-through, allowing
      // later sibling margins to collapse all the way out of ancestors. This is visible on pages
      // like sqlite.org where a float-based header incorrectly collapses away and lets the first
      // paragraph's `margin-top` shift the entire body down by a line-height.
      if !has_in_flow_content && establishes_bfc(style) {
        // Consider only in-flow floats: skip out-of-flow positioned subtrees.
        let mut stack: Vec<&BoxNode> = node.children.iter().collect();
        while let Some(desc) = stack.pop() {
          if desc.style.running_position.is_some()
            || matches!(desc.style.position, Position::Absolute | Position::Fixed)
          {
            continue;
          }
          if desc.style.float.is_floating()
            && matches!(desc.style.position, Position::Static | Position::Relative)
          {
            has_in_flow_content = true;
            break;
          }
          for child in &desc.children {
            stack.push(child);
          }
        }
      }
    }

    let collapsible_through = !has_in_flow_content && is_margin_collapsible_through(style);

    let result = CollapsedBlockMargins {
      top,
      bottom,
      collapsible_through,
    };
    cache.insert((node.id, mode), result);
    result
  }

  /// Lays out all children of a box
  #[allow(clippy::cognitive_complexity)]
  fn layout_children(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    paint_viewport: Rect,
  ) -> Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>), LayoutError> {
    self.layout_children_with_external_floats(
      parent,
      constraints,
      nearest_positioned_cb,
      nearest_fixed_cb,
      paint_viewport,
      None,
      0.0,
      0.0,
    )
  }

  #[allow(clippy::cognitive_complexity)]
  fn layout_children_with_external_floats(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    paint_viewport: Rect,
    mut external_float_ctx: Option<&mut FloatContext>,
    external_float_base_x: f32,
    external_float_base_y: f32,
  ) -> Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>), LayoutError> {
    let mut deadline_counter = 0usize;
    let toggles = crate::debug::runtime::runtime_toggles();
    let inline_is_horizontal = inline_axis_is_horizontal(parent.style.writing_mode);
    let inline_space = if inline_is_horizontal {
      constraints.available_width
    } else {
      constraints.available_height
    };
    let block_space = if inline_is_horizontal {
      constraints.available_height
    } else {
      constraints.available_width
    };
    let inline_percentage_base = match inline_space {
      AvailableSpace::Definite(_) => {
        // Child percentage sizes resolve against the parent’s used inline size (its content box),
        // not the parent’s containing block (which can differ for flex/grid items where we stash
        // the containing block inline size in `inline_percentage_base`).
        let base = if inline_is_horizontal {
          constraints.width()
        } else {
          constraints.height()
        };
        let viewport_inline = if inline_is_horizontal {
          self.viewport_size.width
        } else {
          self.viewport_size.height
        };
        base.unwrap_or(viewport_inline)
      }
      AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite => {
        constraints.inline_percentage_base.unwrap_or(0.0)
      }
    };
    let dump_cell_child_y = toggles.truthy("FASTR_DUMP_CELL_CHILD_Y");
    let mut fragments = Vec::new();
    let mut current_y: f32 = 0.0;
    // Floats are positioned using the margin-edge cursor, but must not consume the pending
    // collapsed margin chain between in-flow siblings (CSS 2.1 §8.3.1). Keep a separate cursor
    // for float placement so floats can be laid out at the correct Y without advancing the
    // in-flow stacking position.
    let mut float_cursor_y: f32 = 0.0;
    let mut content_height: f32 = 0.0;
    let mut content_height_before_last_cursor: f32 = 0.0;
    let mut margin_ctx = MarginCollapseContext::new();
    let mut inline_buffer: Vec<BoxNode> = Vec::new();
    let mut positioned_children: Vec<PositionedCandidate> = Vec::new();
    // Root element margins never collapse with their children (CSS 2.1 §8.3.1).
    //
    // Additionally, boxes that establish an independent formatting context (e.g. flex items) must
    // not collapse margins with their children (CSS Display 3 + CSS2.1 §8.3.1).
    let parent_is_independent_context_root =
      parent.id == 1 || self.independent_context_root_id == Some(parent.id);
    let mut collapse_with_parent_top =
      !parent_is_independent_context_root && should_collapse_with_first_child(&parent.style);
    let establishes_absolute_cb = parent.style.establishes_abs_containing_block();
    let establishes_fixed_cb = parent.style.establishes_fixed_containing_block();
    if !collapse_with_parent_top {
      margin_ctx.mark_content_encountered();
    }
    static TRACE_ENV_RAW_LOGGED: OnceLock<bool> = OnceLock::new();
    if let Some(val) = toggles.get("FASTR_TRACE_BOXES") {
      TRACE_ENV_RAW_LOGGED.get_or_init(|| {
        eprintln!("[trace-box-env-raw] {}", val);
        true
      });
    }
    let trace_boxes = toggles.usize_list("FASTR_TRACE_BOXES").unwrap_or_default();
    static TRACE_BOXES_LOGGED: OnceLock<bool> = OnceLock::new();
    if !trace_boxes.is_empty() {
      TRACE_BOXES_LOGGED.get_or_init(|| {
        eprintln!("[trace-box-env] ids={:?}", trace_boxes);
        true
      });
    }
    let progress_ms = toggles
      .usize("FASTR_LOG_BLOCK_PROGRESS_MS")
      .map(|v| v as u128)
      .unwrap_or(0);
    let progress_ids = toggles.usize_list("FASTR_LOG_BLOCK_PROGRESS_IDS");
    let progress_match = toggles.string_list("FASTR_LOG_BLOCK_PROGRESS_MATCH");
    let filters_set = progress_ids.is_some() || progress_match.is_some();
    let passes_filters = |node: &BoxNode| -> bool {
      let id_ok = progress_ids
        .as_ref()
        .map(|ids| ids.contains(&node.id))
        .unwrap_or(false);
      let match_ok = progress_match
        .as_ref()
        .map(|subs| {
          subs.iter().any(|sub| {
            node
              .debug_info
              .as_ref()
              .map(|d| d.to_selector().contains(sub))
              .unwrap_or(false)
          })
        })
        .unwrap_or(false);
      if !filters_set {
        true
      } else {
        id_ok || match_ok
      }
    };
    let should_log_progress = progress_ms > 0 && passes_filters(parent);
    let progress_ms = if should_log_progress { progress_ms } else { 0 };
    let progress_max = if progress_ms > 0 {
      toggles.usize("FASTR_LOG_BLOCK_PROGRESS_MAX").unwrap_or(10) as u32
    } else {
      0
    };
    static TOTAL_COUNT: OnceLock<std::sync::atomic::AtomicU32> = OnceLock::new();
    let total_cap = if should_log_progress {
      toggles
        .usize("FASTR_LOG_BLOCK_PROGRESS_TOTAL_MAX")
        .map(|v| v as u32)
        .or(Some(50))
    } else {
      None
    };
    let total_counter = TOTAL_COUNT.get_or_init(|| std::sync::atomic::AtomicU32::new(0));

    let within_total_cap = total_cap
      .map(|cap| total_counter.load(std::sync::atomic::Ordering::Relaxed) < cap)
      .unwrap_or(true);

    if progress_ms > 0 && within_total_cap {
      eprintln!(
        "[block-progress-start] parent_id={} children={} threshold_ms={}",
        parent.id,
        parent.children.len(),
        progress_ms
      );
      if total_cap.is_some() {
        total_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      }
    }
    let parent_selector = if progress_ms > 0 {
      parent
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anon>".to_string())
    } else {
      String::new()
    };
    let progress_start = Instant::now();
    let mut progress_last = if progress_ms > 0 {
      let clamped_ms = progress_ms.min(u128::from(u64::MAX)) as u64;
      progress_start
        .checked_sub(std::time::Duration::from_millis(clamped_ms))
        .unwrap_or(progress_start)
    } else {
      progress_start
    };
    let mut progress_count: u32 = 0;
    let mut progress_capped = false;

    // Get containing width from constraints, but guard against collapsed/indefinite widths that
    // would zero out percentage sizing for descendants. Mirror the root-width fallback used in
    // `layout` so children still see a usable containing block when the parent was laid out with
    // a near-zero available width (common when flex measurement feeds 0px constraints). When the
    // available inline size is intrinsic/indefinite (min-/max-content probes), avoid inflating
    // the base to the viewport — leave it at 0 unless the caller provided a definite percentage
    // base.
    let intrinsic_width = matches!(
      inline_space,
      AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite
    );
    let inline_viewport = if inline_is_horizontal {
      self.viewport_size.width
    } else {
      self.viewport_size.height
    };
    let mut containing_width = inline_percentage_base;
    // When flex/grid passes a used border-box size override, a near-zero containing width is
    // intentional (e.g. a 0px flex item) and should not fall back to the viewport.
    let used_border_box_inline = if inline_is_horizontal {
      constraints.used_border_box_width
    } else {
      constraints.used_border_box_height
    };
    if !intrinsic_width
      && containing_width <= 1.0
      && used_border_box_inline.is_none()
      && !self.suppress_near_zero_width_viewport_fallback
    {
      let width_is_absolute = parent
        .style
        .width
        .as_ref()
        .map(|l| l.unit.is_absolute())
        .unwrap_or(false);
      if !width_is_absolute {
        containing_width = inline_viewport;
      }
    }
    let has_external_float_ctx = external_float_ctx.is_some();
    // Flex items establish a new block formatting context for their contents, which prevents
    // parent/child margin collapsing and isolates floats from the outside.
    let owns_float_ctx = !has_external_float_ctx
      || establishes_bfc(&parent.style)
      || parent_is_independent_context_root;
    let mut local_float_ctx = FloatContext::new(containing_width);
    let float_base_x = if owns_float_ctx {
      0.0
    } else {
      external_float_base_x
    };
    let float_base_y = if owns_float_ctx {
      0.0
    } else {
      external_float_base_y
    };
    let float_ctx: &mut FloatContext = if owns_float_ctx {
      &mut local_float_ctx
    } else {
      external_float_ctx
        .as_deref_mut()
        .unwrap_or(&mut local_float_ctx)
    };

    // When we reuse an ancestor float context (i.e., this element does not establish a new BFC),
    // float placement must still be constrained by *this* element's containing block. Real pages
    // frequently have nested blocks with narrower widths; without scoping the float context width,
    // right floats can be incorrectly positioned outside their containing block and line boxes can
    // incorrectly "see" extra width that is not actually available.
    struct FloatContextWidthRestore {
      ctx: *mut FloatContext,
      prev_width: f32,
    }
    impl Drop for FloatContextWidthRestore {
      fn drop(&mut self) {
        // SAFETY: `ctx` points at the float context borrowed for the duration of
        // `layout_children_with_external_floats`; it remains valid until this guard is dropped.
        unsafe {
          (*self.ctx).set_containing_block_width(self.prev_width);
        }
      }
    }
    let _float_ctx_width_restore = if owns_float_ctx {
      None
    } else {
      let prev_width = float_ctx.containing_block_width();
      float_ctx.set_containing_block_width(containing_width);
      Some(FloatContextWidthRestore {
        ctx: float_ctx as *mut FloatContext,
        prev_width,
      })
    };

    let available_height = block_space;
    // Positioned offsets resolve percentages against the containing block's physical width/height.
    // Block layout tracks sizes in logical inline/block coordinates, so map them back to physical
    // axes before constructing the positioned containing block. Keep the percentage bases `None`
    // when the corresponding physical size is indefinite so percentage offsets resolve to `auto`
    // instead of being treated as 0px (CSS 2.1 §10.5/10.6).
    let block_space_px = block_space.to_option().unwrap_or(0.0);
    let (relative_width, relative_height) = if inline_is_horizontal {
      (containing_width, block_space_px)
    } else {
      (block_space_px, containing_width)
    };
    let physical_width_base = if inline_is_horizontal {
      Some(containing_width)
    } else {
      block_space.to_option()
    };
    let physical_height_base = if inline_is_horizontal {
      block_space.to_option()
    } else {
      Some(containing_width)
    };
    let relative_cb = ContainingBlock::with_viewport_and_bases(
      Rect::new(Point::ZERO, Size::new(relative_width, relative_height)),
      self.viewport_size,
      physical_width_base,
      physical_height_base,
    )
    .with_writing_mode_and_direction(parent.style.writing_mode, parent.style.direction);
    // Check for border/padding that prevents margin collapse with first child
    let (parent_block_start, _) = block_axis_sides(&parent.style);
    let root_font_metrics = self.factory.root_font_metrics();
    let parent_has_top_separation = resolve_border_side(
      &parent.style,
      parent_block_start,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    ) > 0.0
      || resolve_padding_side(
        &parent.style,
        parent_block_start,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      ) > 0.0;

    if parent_has_top_separation {
      collapse_with_parent_top = false;
      margin_ctx.mark_content_encountered();
    }

    let trace_positioned = trace_positioned_ids();
    let trace_block_text = trace_block_text_ids();
    if let Some(result) = self.try_parallel_block_children(
      parent,
      constraints,
      nearest_positioned_cb,
      nearest_fixed_cb,
      margin_ctx.clone(),
      &relative_cb,
      containing_width,
      float_ctx.is_empty(),
      paint_viewport,
    ) {
      return result;
    }

    // Inline formatting contexts lay out in the parent's *content* coordinate space (origin at the
    // content edge). When the parent establishes an abs/fixed containing block, the containing
    // block rectangle is the parent's padding box (origin at the padding edge). Convert the
    // nearest containing blocks into the inline context's content coordinate space so absolute
    // descendants inside inline flow can correctly position against the padding edge without
    // double-counting padding/border offsets.
    let cb_percentage_base = constraints.width().unwrap_or(self.viewport_size.width);
    let padding_left_for_cb = resolve_padding_side(
      &parent.style,
      PhysicalSide::Left,
      cb_percentage_base,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    );
    let padding_top_for_cb = resolve_padding_side(
      &parent.style,
      PhysicalSide::Top,
      cb_percentage_base,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    );
    let inline_nearest_positioned_cb = if establishes_absolute_cb {
      ContainingBlock::with_viewport_and_bases(
        Rect::new(
          Point::new(-padding_left_for_cb, -padding_top_for_cb),
          nearest_positioned_cb.rect.size,
        ),
        self.viewport_size,
        nearest_positioned_cb.inline_percentage_base(),
        nearest_positioned_cb.block_percentage_base(),
      )
      .with_writing_mode_and_direction(
        nearest_positioned_cb.writing_mode,
        nearest_positioned_cb.direction,
      )
    } else {
      *nearest_positioned_cb
    };
    let inline_nearest_fixed_cb = if establishes_fixed_cb {
      ContainingBlock::with_viewport_and_bases(
        Rect::new(
          Point::new(-padding_left_for_cb, -padding_top_for_cb),
          nearest_fixed_cb.rect.size,
        ),
        self.viewport_size,
        nearest_fixed_cb.inline_percentage_base(),
        nearest_fixed_cb.block_percentage_base(),
      )
      .with_writing_mode_and_direction(nearest_fixed_cb.writing_mode, nearest_fixed_cb.direction)
    } else {
      *nearest_fixed_cb
    };

    let inline_fc_owned: Option<Box<InlineFormattingContext>> = if inline_nearest_positioned_cb
      == self.nearest_positioned_cb
      && inline_nearest_fixed_cb == self.nearest_fixed_cb
    {
      None
    } else {
      Some(Box::new(InlineFormattingContext::with_factory(
        self.child_factory_for_cbs(inline_nearest_positioned_cb, inline_nearest_fixed_cb),
      )))
    };
    let inline_fc = inline_fc_owned
      .as_deref()
      .unwrap_or_else(|| self.intrinsic_inline_fc.as_ref());

    let child_layout_ctx: &BlockFormattingContext = self;
    let child_margin_mode = if parent.id == 1 && self.factory.quirks_mode() == QuirksMode::Quirks {
      CollapsedMarginMode::QuirksRootChild
    } else {
      CollapsedMarginMode::Normal
    };

    let layout_in_flow_block_child = |child: &BoxNode,
                                      margin_ctx: &mut MarginCollapseContext,
                                      current_y: f32,
                                      float_ctx_ref: &mut FloatContext|
     -> Result<(FragmentNode, f32), LayoutError> {
      let trace_child = !trace_boxes.is_empty() && trace_boxes.contains(&child.id);
      let trace_start = trace_child.then(Instant::now);
      if trace_child {
        eprintln!(
          "[trace-margins-start] parent_id={} child_id={} children={}",
          parent.id,
          child.id,
          child.children.len()
        );
      }
      let child_margins =
        child_layout_ctx.collapsed_block_margins(child, containing_width, child_margin_mode);
      if let Some(start) = trace_start {
        eprintln!(
          "[trace-margins-end] parent_id={} child_id={} elapsed_ms={} margins={:?}",
          parent.id,
          child.id,
          start.elapsed().as_millis(),
          child_margins
        );
      }
      let at_start_before = margin_ctx.is_at_start();
      let pending_margin = margin_ctx.pending_collapsible_margin();
      if !trace_boxes.is_empty() && trace_boxes.contains(&child.id) {
        eprintln!(
            "[trace-box-margins] id={} pending={} child_top={} child_bottom={} collapsible_through={} at_start={} collapse_with_parent_top={}",
            child.id,
            pending_margin,
            child_margins.top,
            child_margins.bottom,
            child_margins.collapsible_through,
            at_start_before,
            collapse_with_parent_top
          );
      }
      let margin_edge_y = current_y + pending_margin.resolve();
      let clear_side = resolve_clear_side(
        child.style.clear,
        parent.style.writing_mode,
        parent.style.direction,
      );
      // `clearance` is a *delta* in the block axis, so it is invariant under the translation
      // between this formatting context's coordinate space and an ancestor float context.
      //
      // When `clear: none`, `FloatContext::compute_clearance` returns the input `y` directly.
      // Converting that result back into local coordinates via `y - float_base_y` can introduce
      // tiny positive rounding errors (from catastrophic cancellation) that would incorrectly be
      // treated as "has clearance", breaking sibling margin collapsing.
      let clearance = if clear_side.is_clearing() {
        float_ctx_ref.clearance_amount(float_base_y + margin_edge_y, clear_side)
      } else {
        0.0
      };

      let flow_box_y = if clearance > 0.0 {
        // Clearance is added above the top margin edge and breaks margin adjoining.
        margin_ctx.mark_content_encountered();
        let (offset, _) = margin_ctx.process_child_with_clearance(
          clearance,
          child_margins.top,
          child_margins.bottom,
          child_margins.collapsible_through,
        );
        current_y + offset
      } else if collapse_with_parent_top && at_start_before && !child_margins.collapsible_through {
        // Parent/first-child margin collapsing is represented by the parent's own collapsed
        // margins. Discard any leading collapsible-through margins and place the first
        // non-empty child at the block start.
        margin_ctx.consume_pending();
        margin_ctx.push_collapsible_margin(child_margins.bottom);
        current_y
      } else {
        let (offset, _) = margin_ctx.process_child_margins(
          child_margins.top,
          child_margins.bottom,
          child_margins.collapsible_through,
        );
        current_y + offset
      };

      // For boxes that are "collapsed through" (their block-start and block-end margins are
      // adjoining), CSS defines the position of their block-start border edge for the purposes
      // of laying out descendants.
      //
      // In particular: when the box is not participating in parent/first-child margin
      // collapsing, its block-start border edge is defined as if it had a non-zero block-end
      // border (CSS Box 3 §4.3 “collapsed through” border edge position). This ensures that
      // out-of-flow descendants (floats/abspos) are positioned after the collapsed sibling margin
      // chain, even though the box itself is invisible in the block layout cursor.
      let layout_box_y =
        if child_margins.collapsible_through && clearance == 0.0 && !at_start_before {
          let collapsed_top = pending_margin.collapse_with(child_margins.top).resolve();
          current_y + collapsed_top
        } else {
          flow_box_y
        };

      let fragment = child_layout_ctx.layout_block_child(
        parent,
        child,
        containing_width,
        constraints,
        layout_box_y,
        nearest_positioned_cb,
        nearest_fixed_cb,
        Some(&mut *float_ctx_ref),
        float_base_x,
        float_base_y,
        paint_viewport,
      )?;
      let block_extent = if child_margins.collapsible_through {
        0.0
      } else {
        fragment.bounds.height()
      };
      let next_y = flow_box_y + block_extent;
      Ok((fragment, next_y))
    };

    // Flushes buffered inline-level siblings by creating an inline formatting context wrapper.
    //
    // Returns the Y position (in this BFC's local coordinate space) of the *last* line box top, if
    // the flushed content generated line boxes.
    //
    // Floats are positioned relative to the current line box (CSS 2.1 §9.5.1). Flushing inline
    // content advances `current_y` to after the line boxes, which would incorrectly push a
    // following float down by one line. Callers that need the current line top must use this
    // return value instead of `current_y`.
    let flush_inline_buffer = |buffer: &mut Vec<BoxNode>,
                               fragments: &mut Vec<FragmentNode>,
                               current_y: &mut f32,
                               content_height: &mut f32,
                               content_height_before_last_cursor: &mut f32,
                               margin_ctx: &mut MarginCollapseContext,
                               float_ctx_ref: &mut FloatContext,
                               deadline_counter: &mut usize,
                               positioned_children: &mut Vec<PositionedCandidate>|
     -> Result<Option<f32>, LayoutError> {
      if buffer.is_empty() {
        return Ok(None);
      }

      // Collapsible whitespace that is the *only* in-flow content should not generate an empty
      // line box (CSS 2.1 §9.4.2: line boxes are created only when there are inline-level boxes).
      // Leaving these nodes in the inline buffer would create 0px-wide inline fragments with a
      // non-zero strut height, which advances the cursor and pushes subsequent floats down.
      if buffer.iter().all(is_collapsible_whitespace_run) {
        buffer.clear();
        return Ok(None);
      }

      // A run of inline-level siblings can contain only out-of-flow positioned boxes (e.g.
      // `<div><span style="position:absolute"></span></div>`). These must not generate line boxes
      // or consume pending collapsible margins, but still need a float-aware static position.
      if buffer.iter().all(is_out_of_flow) {
        let pending_margin = margin_ctx.pending_collapsible_margin().resolve();
        let static_y = *current_y + pending_margin;
        let (left_edge, _) = float_ctx_ref.available_width_at_y_in_containing_block(
          float_base_y + static_y,
          float_base_x,
          containing_width,
        );
        let left_offset = (left_edge - float_base_x).max(0.0);
        for child in buffer.drain(..) {
          let static_position = Some(Point::new(left_offset, static_y));
          let source = match child.style.position {
            Position::Fixed => {
              if establishes_fixed_cb {
                ContainingBlockSource::ParentPadding
              } else {
                ContainingBlockSource::Explicit(*nearest_fixed_cb)
              }
            }
            Position::Absolute => {
              if establishes_absolute_cb {
                ContainingBlockSource::ParentPadding
              } else {
                ContainingBlockSource::Explicit(*nearest_positioned_cb)
              }
            }
            _ => ContainingBlockSource::Explicit(*nearest_positioned_cb),
          };
          let implicit_anchor_box_id = child.implicit_anchor_box_id;
          positioned_children.push(PositionedCandidate {
            node: child,
            source,
            static_position,
            query_parent_id: parent.id,
            implicit_anchor_box_id,
          });
        }
        return Ok(None);
      }

      // If the buffer contains any *in-flow* block-level boxes (or only whitespace), lay each out
      // separately to avoid creating an inline formatting context that spans mixed block content.
      //
      // Out-of-flow positioned boxes can be blockified (CSS2.1 §9.7) even when their original
      // display type is inline-level. We still defer those to the inline formatting context for
      // float-aware static-position anchoring, so they must not trigger this "block content in the
      // inline buffer" fallback.
      let has_block = buffer
        .iter()
        .any(|b| !is_out_of_flow(b) && b.is_block_level() && !b.style.float.is_floating());
      let all_whitespace = buffer.iter().all(|b| match &b.box_type {
        BoxType::Text(text) => trim_ascii_whitespace(&text.text).is_empty(),
        _ => false,
      });
      if has_block || all_whitespace {
        for child in buffer.drain(..) {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(deadline_counter, 16, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }
          let treated_as_block = child.is_block_level()
            || matches!(
              child.box_type,
              BoxType::Replaced(_) if !child.style.display.is_inline_level()
            );

          let (fragment, next_y) = if treated_as_block {
            layout_in_flow_block_child(&child, margin_ctx, *current_y, float_ctx_ref)?
          } else {
            let pending_margin = margin_ctx.consume_pending();
            *current_y += pending_margin;
            let box_y = *current_y;
            let fragment = child_layout_ctx.layout_block_child(
              parent,
              &child,
              containing_width,
              constraints,
              box_y,
              nearest_positioned_cb,
              nearest_fixed_cb,
              Some(&mut *float_ctx_ref),
              float_base_x,
              float_base_y,
              paint_viewport,
            )?;
            let block_extent = fragment.bounds.height();
            let next_y = box_y + block_extent;
            (fragment, next_y)
          };
          *content_height_before_last_cursor = *content_height;
          *content_height = content_height.max(next_y);
          *current_y = next_y;
          let mut fragment = fragment;
          if child.style.position.is_relative() {
            let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
              &child.style,
              &relative_cb,
              self.viewport_size,
              &self.font_context,
            );
            fragment = PositionedLayout::with_font_context(self.font_context.clone())
              .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
          }
          fragments.push(fragment);
        }
        return Ok(None);
      }

      // Inline content only breaks margin collapsing when it generates line boxes (CSS 2.1 §8.3.1).
      //
      // In-flow inline content that collapses away (e.g. an empty `<span>` or whitespace that is
      // trimmed at the start of the line) must not mark "content encountered" nor consume the
      // pending margin chain, otherwise parent/first-child margin collapsing is incorrectly
      // prevented.
      let pending_margin = margin_ctx.pending_margin();

      // Inline formatting contexts should lay out the inline content area; padding/borders are
      // applied by the surrounding block container. Clear edges on the synthetic inline container
      // to avoid double-counting them in block layout.
      let mut inline_style = parent.style.clone();
      {
        let s = Arc::make_mut(&mut inline_style);
        s.padding_top = Length::px(0.0);
        s.padding_right = Length::px(0.0);
        s.padding_bottom = Length::px(0.0);
        s.padding_left = Length::px(0.0);
        s.border_top_width = Length::px(0.0);
        s.border_right_width = Length::px(0.0);
        s.border_bottom_width = Length::px(0.0);
        s.border_left_width = Length::px(0.0);
        // The inline container is synthetic: it's just the inline formatting context wrapper for a
        // block container's inline content. It should not establish a containing block (for abspos
        // or fixed descendants), otherwise out-of-flow positioned children nested in inline content
        // can resolve percentages against the wrapper's (often 0px-tall) line box bounds instead of
        // the real block container padding box.
        s.position = Position::Static;
        s.top = crate::style::types::InsetValue::Auto;
        s.right = crate::style::types::InsetValue::Auto;
        s.bottom = crate::style::types::InsetValue::Auto;
        s.left = crate::style::types::InsetValue::Auto;
        s.translate = crate::css::types::TranslateValue::None;
        s.rotate = crate::css::types::RotateValue::None;
        s.scale = crate::css::types::ScaleValue::None;
        s.transform.clear();
        s.offset_path = crate::style::types::OffsetPath::None;
        s.perspective = None;
        s.filter.clear();
        s.backdrop_filter.clear();
        s.will_change = crate::style::types::WillChange::Auto;
        s.containment = crate::style::types::Containment::none();
      }
      let mut inline_container = BoxNode::new_inline(inline_style, std::mem::take(buffer));
      // If the inline container would start below the current cursor because of pending
      // margins, advance to that baseline first.
      let inline_y = *current_y + pending_margin;
      let inline_constraints = if inline_is_horizontal {
        LayoutConstraints::new(AvailableSpace::Definite(containing_width), available_height)
      } else {
        LayoutConstraints::new(available_height, AvailableSpace::Definite(containing_width))
      }
      .with_inline_percentage_base(Some(containing_width))
      .with_block_percentage_base(constraints.block_percentage_base);
      let mut inline_fragment = match inline_fc.layout_with_floats(
        &inline_container,
        &inline_constraints,
        Some(&mut *float_ctx_ref),
        float_base_x,
        float_base_y + inline_y,
      ) {
        Ok(fragment) => fragment,
        Err(err) => {
          *buffer = std::mem::take(&mut inline_container.children);
          return Err(err);
        }
      };

      // Inline formatting contexts produce fragment trees in the parent formatting context's
      // logical coordinate system (x = inline axis, y = block axis). Block layout will convert the
      // full subtree once at the end, so no additional axis conversion is needed here.

      inline_fragment.bounds = Rect::from_xywh(
        0.0,
        inline_y,
        inline_fragment.bounds.width(),
        inline_fragment.bounds.height(),
      );

      let mut line_boxes_bottom: f32 = 0.0;
      let mut has_line_boxes = false;
      let mut last_line_top_y: Option<f32> = None;
      for child in inline_fragment.children.iter() {
        if matches!(child.content, FragmentContent::Line { .. }) {
          has_line_boxes = true;
          line_boxes_bottom = line_boxes_bottom.max(child.bounds.max_y());
          let y = inline_y + child.bounds.y();
          if y.is_finite() {
            last_line_top_y = Some(last_line_top_y.map_or(y, |cur| cur.max(y)));
          }
        }
      }

      if has_line_boxes {
        // Line boxes terminate any margin collapsing chain above them.
        let applied_margin = margin_ctx.consume_pending();
        *current_y += applied_margin;

        // Floats are out-of-flow: only in-flow line boxes advance the block cursor/auto height.
        // BFC roots incorporate floats separately via `float_bottom` once child layout completes.
        let line_bottom = *current_y + line_boxes_bottom;
        *content_height = content_height.max(line_bottom);
        *current_y = line_bottom;
        fragments.push(inline_fragment);
      } else if !inline_fragment.children.is_empty() {
        // Floats, positioned descendants, and other out-of-flow contributions may still need to
        // paint, but should not prevent margin collapsing between in-flow blocks.
        fragments.push(inline_fragment);
      }
      // Restore the buffered inline-level children back to the caller. Most call sites immediately
      // clear the buffer, but floats need to keep the just-laid-out inline run around temporarily
      // so it can be re-laid out after placing the float (to account for float intrusion when
      // applying `text-align` and line breaking).
      *buffer = std::mem::take(&mut inline_container.children);
      Ok(last_line_top_y)
    };

    for (child_idx, child) in parent.children.iter().enumerate() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 16, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      if progress_ms > 0 {
        if let Some(cap) = total_cap {
          let current = total_counter.load(std::sync::atomic::Ordering::Relaxed);
          if current >= cap {
            continue;
          }
        }
        if progress_count < progress_max || progress_max == 0 {
          let now = Instant::now();
          if now.duration_since(progress_last).as_millis() >= progress_ms {
            let child_selector = child
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anon>".to_string());
            eprintln!(
                            "[block-progress] parent_id={} child={}/{} elapsed_ms={} selector={} child_selector={}",
                            parent.id,
                            child_idx,
                            parent.children.len(),
                            now.duration_since(progress_start).as_millis(),
                            parent_selector,
                            child_selector
                        );
            progress_last = now;
            if progress_max > 0 {
              progress_count += 1;
            }
            if total_cap.is_some() {
              total_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
          }
        } else if !progress_capped {
          eprintln!(
            "[block-progress-cap] parent_id={} selector={} max_logs={}",
            parent.id, parent_selector, progress_max
          );
          progress_capped = true;
        }
      }
      // Skip collapsible whitespace text in block formatting contexts (CSS 2.1 §16.6).
      if let BoxType::Text(text_box) = &child.box_type {
        if trim_ascii_whitespace(&text_box.text).is_empty()
          && !matches!(
            child.style.white_space,
            crate::style::types::WhiteSpace::Pre | crate::style::types::WhiteSpace::PreWrap
          )
        {
          if trace_positioned.contains(&child.id) || trace_block_text.contains(&child.id) {
            eprintln!(
              "[block-text-skip] id={} selector={:?} raw={:?}",
              child.id,
              child.debug_info.as_ref().map(|d| d.to_selector()),
              text_box.text
            );
          }
          continue;
        }
        if trace_block_text.contains(&child.id) {
          eprintln!(
            "[block-text] id={} selector={:?} text={:?} white_space={:?}",
            child.id,
            child.debug_info.as_ref().map(|d| d.to_selector()),
            text_box.text,
            child.style.white_space
          );
        }
      }

      let running_name = if matches!(
        child.box_type,
        BoxType::Block(_) | BoxType::Inline(_) | BoxType::Replaced(_)
      ) {
        child.style.running_position.as_ref()
      } else {
        None
      };

      if let Some(running_name) = running_name {
        let pending_margin = margin_ctx.pending_margin();
        // Running elements are positioned based on the hypothetical in-flow position. Make sure we
        // resolve intrinsic sizing keywords (`min-content`, `max-content`, `fit-content(...)`) the
        // same way as normal-flow blocks so the anchor point lines up with the rendered box.
        let inline_sides = inline_axis_sides(&child.style);
        let inline_positive = inline_axis_positive(child.style.writing_mode, child.style.direction);
        let root_font_metrics = self.factory.root_font_metrics();
        let mut hypo_width = compute_block_width(
          &child.style,
          containing_width,
          self.viewport_size,
          root_font_metrics,
          inline_sides,
          inline_positive,
          &self.font_context,
        );
        let width_auto = child.style.width.is_none() && child.style.width_keyword.is_none();
        let inline_edges_for_fit = hypo_width.border_left
          + hypo_width.padding_left
          + hypo_width.padding_right
          + hypo_width.border_right;
        let available_inline_border_box = (containing_width
          - resolve_margin_side(
            &child.style,
            inline_sides.0,
            containing_width,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          )
          - resolve_margin_side(
            &child.style,
            inline_sides.1,
            containing_width,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          ))
        .max(0.0);
        let available_content_for_fit =
          (available_inline_border_box - inline_edges_for_fit).max(0.0);
        let mut intrinsic_content_sizes = None;
        if child.style.width.is_none() && child.style.width_keyword.is_some() {
          let keyword = child.style.width_keyword.unwrap();
          let factory = self.child_factory_for_cb(*nearest_positioned_cb);
          let fc_type = child.formatting_context().unwrap_or_else(|| {
            if child.is_block_level() {
              FormattingContextType::Block
            } else {
              FormattingContextType::Inline
            }
          });
          let (min_content, max_content) =
            self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?;
          intrinsic_content_sizes = Some((min_content, max_content));
          let keyword_content = self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            &child.style,
            inline_edges_for_fit,
          );
          let specified_width = match child.style.box_sizing {
            crate::style::types::BoxSizing::ContentBox => keyword_content,
            crate::style::types::BoxSizing::BorderBox => keyword_content + inline_edges_for_fit,
          };
          let mut width_style = child.style.clone();
          {
            let s = Arc::make_mut(&mut width_style);
            s.width = Some(Length::px(specified_width));
            s.width_keyword = None;
          }
          hypo_width = compute_block_width(
            &width_style,
            containing_width,
            self.viewport_size,
            root_font_metrics,
            inline_sides,
            inline_positive,
            &self.font_context,
          );
        }

        // Apply min/max sizing constraints after resolving intrinsic keywords (CSS 2.1 §10.4),
        // mirroring `layout_block_child` so auto margins stay consistent.
        let horizontal_edges = hypo_width.border_left
          + hypo_width.padding_left
          + hypo_width.padding_right
          + hypo_width.border_right;
        let reserved_vertical_gutter =
          if child.style.box_sizing == crate::style::types::BoxSizing::ContentBox {
            let reservation = crate::layout::utils::scrollbar_reservation_for_style(&child.style);
            (reservation.left + reservation.right).max(0.0)
          } else {
            0.0
          };
        let min_width = if let Some(keyword) = child.style.min_width_keyword {
          if intrinsic_content_sizes.is_none() {
            let factory = self.child_factory_for_cb(*nearest_positioned_cb);
            let fc_type = child.formatting_context().unwrap_or_else(|| {
              if child.is_block_level() {
                FormattingContextType::Block
              } else {
                FormattingContextType::Inline
              }
            });
            intrinsic_content_sizes = Some(
              self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?,
            );
          }
          let (min_content, max_content) = intrinsic_content_sizes.unwrap();
          self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            &child.style,
            horizontal_edges,
          )
        } else {
          child
            .style
            .min_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                containing_width,
                &child.style,
                &self.font_context,
                self.viewport_size,
                root_font_metrics,
              )
            })
            .map(|w| content_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(0.0)
        };
        let min_width = if reserved_vertical_gutter > 0.0
          && child.style.min_width_keyword.is_none()
          && child.style.min_width.is_some()
        {
          (min_width - reserved_vertical_gutter).max(0.0)
        } else {
          min_width
        };
        let max_width = if let Some(keyword) = child.style.max_width_keyword {
          if intrinsic_content_sizes.is_none() {
            let factory = self.child_factory_for_cb(*nearest_positioned_cb);
            let fc_type = child.formatting_context().unwrap_or_else(|| {
              if child.is_block_level() {
                FormattingContextType::Block
              } else {
                FormattingContextType::Inline
              }
            });
            intrinsic_content_sizes = Some(
              self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?,
            );
          }
          let (min_content, max_content) = intrinsic_content_sizes.unwrap();
          self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            &child.style,
            horizontal_edges,
          )
        } else {
          child
            .style
            .max_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                containing_width,
                &child.style,
                &self.font_context,
                self.viewport_size,
                root_font_metrics,
              )
            })
            .map(|w| content_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(f32::INFINITY)
        };
        let max_width = if reserved_vertical_gutter > 0.0
          && child.style.max_width_keyword.is_none()
          && child.style.max_width.is_some()
          && max_width.is_finite()
        {
          (max_width - reserved_vertical_gutter).max(0.0)
        } else {
          max_width
        };
        let max_width = if max_width.is_finite() && max_width < min_width {
          min_width
        } else {
          max_width
        };
        let clamped_content_width =
          crate::layout::utils::clamp_with_order(hypo_width.content_width, min_width, max_width);
        if clamped_content_width != hypo_width.content_width {
          let (margin_left, margin_right) = recompute_margins_for_width(
            &child.style,
            containing_width,
            clamped_content_width,
            hypo_width.border_left,
            hypo_width.padding_left,
            hypo_width.padding_right,
            hypo_width.border_right,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          );
          hypo_width.content_width = clamped_content_width;
          hypo_width.margin_left = margin_left;
          hypo_width.margin_right = margin_right;
        }

        // If we're being asked to use a border-box width override, treat the inline size as auto
        // so the constraint equation can resolve the new width + margins.
        if width_auto {
          if let Some(used_border_box) = constraints.used_border_box_width {
            let used_content = (used_border_box - horizontal_edges).max(0.0);
            let (margin_left, margin_right) = recompute_margins_for_width(
              &child.style,
              containing_width,
              used_content,
              hypo_width.border_left,
              hypo_width.padding_left,
              hypo_width.padding_right,
              hypo_width.border_right,
              self.viewport_size,
              &self.font_context,
              root_font_metrics,
            );
            hypo_width.content_width = used_content;
            hypo_width.margin_left = margin_left;
            hypo_width.margin_right = margin_right;
          }
        }
        let static_x = hypo_width.margin_left;
        let static_y = current_y + pending_margin;

        let mut snapshot_node = child.clone();
        let mut snapshot_style = snapshot_node.style.clone();
        {
          let s = Arc::make_mut(&mut snapshot_style);
          s.running_position = None;
          s.position = Position::Static;
        }
        snapshot_node.style = snapshot_style;
        crate::layout::running_elements::clear_running_position_in_box_tree(&mut snapshot_node);

        let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
        let fc_type = snapshot_node.formatting_context().unwrap_or_else(|| {
          if snapshot_node.is_block_level() {
            FormattingContextType::Block
          } else {
            FormattingContextType::Inline
          }
        });
        let fc = factory.get(fc_type);
        let snapshot_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(containing_width),
          AvailableSpace::Indefinite,
        );
        let snapshot_fragment = fc.layout(&snapshot_node, &snapshot_constraints)?;
        let anchor_bounds = Rect::from_xywh(static_x, static_y, 0.0, 0.01);
        let mut anchor =
          FragmentNode::new_running_anchor(anchor_bounds, running_name.clone(), snapshot_fragment);
        anchor.style = Some(child.style.clone());
        fragments.push(anchor);
        continue;
      }

      // Skip out-of-flow positioned boxes (absolute/fixed)
      if is_out_of_flow(child) {
        // Inline-level positioned children still participate in the inline stream for the purposes
        // of static-position computation (e.g. they should start after floats and preceding text).
        //
        // Defer these to the inline formatting context, which will insert a zero-sized
        // `StaticPositionAnchor` at the correct inline cursor.
        if child.original_display.is_inline_level() {
          inline_buffer.push(child.clone());
          continue;
        }
        if trace_positioned.contains(&child.id) {
          eprintln!(
            "[block-positioned] parent_id={} child_id={} selector={} pos={:?}",
            parent.id,
            child.id,
            child
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anon>".into()),
            child.style.position
          );
        }
        // Static position is defined in terms of the hypothetical in-flow margin edge, which for
        // block-level siblings must respect vertical margin collapsing. Because absolutely/fixed
        // positioned boxes do not participate in margin collapse with surrounding flow content, we
        // must compute the collapsed block-start margin without mutating the `MarginCollapseContext`.
        let pending_margin = margin_ctx.pending_collapsible_margin();
        let block_sides = block_axis_sides(&child.style);
        let margin_top = resolve_margin_side(
          &child.style,
          block_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let collapsed_margin = pending_margin
          .collapse_with(CollapsibleMargin::from_margin(margin_top))
          .resolve();
        // Static position is based on the hypothetical in-flow margin edge. For normal blocks, the
        // margin edge is aligned to the containing block start, so the inline coordinate is 0 and
        // the absolute positioning constraint equation will apply the actual margin.
        let static_x = 0.0;
        // `AbsoluteLayout` applies the element's margin-top as part of the constraint equation, so
        // the static position must be recorded at the (collapsed) margin edge rather than the
        // border edge.
        let static_y = current_y + collapsed_margin - margin_top;
        let static_position = Some(Point::new(static_x, static_y));
        let source = match child.style.position {
          Position::Fixed => {
            if establishes_fixed_cb {
              ContainingBlockSource::ParentPadding
            } else {
              ContainingBlockSource::Explicit(*nearest_fixed_cb)
            }
          }
          Position::Absolute => {
            if establishes_absolute_cb {
              ContainingBlockSource::ParentPadding
            } else {
              ContainingBlockSource::Explicit(*nearest_positioned_cb)
            }
          }
          _ => ContainingBlockSource::Explicit(*nearest_positioned_cb),
        };
        let implicit_anchor_box_id = child.implicit_anchor_box_id;
        positioned_children.push(PositionedCandidate {
          node: child.clone(),
          source,
          static_position,
          query_parent_id: parent.id,
          implicit_anchor_box_id,
        });
        continue;
      }

      // Floats are taken out of flow but still participate in this BFC's float context.
      //
      // Inline-level floats (e.g. `display:inline-block; float:right`) must be handled by the
      // inline formatting context so they can be positioned relative to the current line box. If
      // we flush the inline buffer and place the float as a standalone block-level float here, it
      // is forced below any already-laid-out line boxes and cannot share the first line (CSS 2.1
      // §9.5.1).
      if child.style.float.is_floating()
        && !matches!(child.style.position, Position::Absolute | Position::Fixed)
      {
        // If we've already buffered inline content, the float's "as high as possible" vertical
        // position depends on the line box currently being built. Defer float placement to the
        // inline formatting context so it can be positioned relative to the correct line top (CSS
        // 2.1 §9.5.1), even when the floated box itself is block-level.
        if !inline_buffer.is_empty() || child.is_inline_level() {
          inline_buffer.push(child.clone());
          continue;
        }

        let _ = flush_inline_buffer(
          &mut inline_buffer,
          &mut fragments,
          &mut current_y,
          &mut content_height,
          &mut content_height_before_last_cursor,
          &mut margin_ctx,
          float_ctx,
          &mut deadline_counter,
          &mut positioned_children,
        )?;
        inline_buffer.clear();

        // Floats are out-of-flow: their own margins never collapse, but they also must not break
        // the sibling margin collapsing chain between in-flow blocks.
        //
        // When parent/first-child margin collapsing applies, any leading collapsible-through
        // margins at the start of the BFC are represented by the *parent's* own collapsed margins.
        // They must not shift floats down inside the parent (notably: an empty block with
        // `margin-top`/`margin-bottom` followed by floats).
        let float_base_y_local = if collapse_with_parent_top && margin_ctx.is_at_start() {
          margin_ctx.consume_pending_without_marking_content();
          current_y
        } else {
          current_y + margin_ctx.pending_margin()
        };
        float_cursor_y = float_cursor_y.max(float_base_y_local);

        // Honor clearance against existing floats for this float's placement only.
        float_cursor_y += float_ctx.clearance_amount(
          float_base_y + float_cursor_y,
          resolve_clear_side(
            child.style.clear,
            parent.style.writing_mode,
            parent.style.direction,
          ),
        );

        let percentage_base = containing_width;
        let margin_left = child
          .style
          .margin_left
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              percentage_base,
              &child.style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .unwrap_or(0.0);
        let margin_right = child
          .style
          .margin_right
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              percentage_base,
              &child.style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .unwrap_or(0.0);
        let horizontal_edges = horizontal_padding_and_borders(
          &child.style,
          percentage_base,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );

        // CSS 2.1 shrink-to-fit formula for floats
        let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let child_bfc = BlockFormattingContext::with_factory(factory.clone());
        let (preferred_min_content, preferred_content) = if fc_type == FormattingContextType::Block
        {
          match child_bfc.compute_intrinsic_inline_sizes(child) {
            Ok(values) => values,
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {
              let preferred_content = match child_bfc
                .compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent)
              {
                Ok(value) => value,
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => 0.0,
              };
              (0.0, preferred_content)
            }
          }
        } else {
          let fc = factory.get(fc_type);
          match fc.compute_intrinsic_inline_sizes(child) {
            Ok(values) => values,
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {
              // Preserve legacy semantics for non-timeout intrinsic sizing failures: treat
              // the min-content width as 0 but still attempt the max-content measurement.
              let preferred_content =
                match fc.compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent) {
                  Ok(value) => value,
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => 0.0,
                };
              (0.0, preferred_content)
            }
          }
        };

        let edges_base0 = horizontal_padding_and_borders(
          &child.style,
          0.0,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        let intrinsic_min =
          rebase_intrinsic_border_box_size(preferred_min_content, edges_base0, horizontal_edges);
        let intrinsic_max =
          rebase_intrinsic_border_box_size(preferred_content, edges_base0, horizontal_edges);

        // CSS 2.1 §10.3.5: the shrink-to-fit "available width" for a float is based on the width
        // of the containing block, *not* on the remaining space at the tentative y-position after
        // accounting for other floats. If the float doesn't fit next to prior floats it should
        // move down, rather than shrinking and wrapping its own contents.
        let available = (containing_width - margin_left - margin_right).max(0.0);

        let specified_width = child
          .style
          .width
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              percentage_base,
              &child.style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .map(|w| border_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
          .or_else(|| {
            child.style.width_keyword.map(|keyword| match keyword {
              crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
              crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
              crate::style::types::IntrinsicSizeKeyword::FillAvailable => available,
              crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
                if let Some(limit) = limit {
                  let resolved = resolve_length_for_width(
                    limit,
                    percentage_base,
                    &child.style,
                    &self.font_context,
                    self.viewport_size,
                    root_font_metrics,
                  );
                  let resolved_border =
                    border_size_from_box_sizing(resolved, horizontal_edges, child.style.box_sizing);
                  intrinsic_max.min(intrinsic_min.max(resolved_border))
                } else {
                  intrinsic_max.min(available.max(intrinsic_min))
                }
              }
              crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
                use crate::style::types::BoxSizing;
                use crate::style::types::CalcSizeBasis;
                let shrink = intrinsic_max.min(available.max(intrinsic_min));
                let basis_border = match calc.basis {
                  CalcSizeBasis::Auto => shrink,
                  CalcSizeBasis::MinContent => intrinsic_min,
                  CalcSizeBasis::MaxContent => intrinsic_max,
                  CalcSizeBasis::FillAvailable => available,
                  CalcSizeBasis::FitContent { limit } => {
                    if let Some(limit) = limit {
                      let resolved = resolve_length_for_width(
                        limit,
                        percentage_base,
                        &child.style,
                        &self.font_context,
                        self.viewport_size,
                        root_font_metrics,
                      );
                      let resolved_border = border_size_from_box_sizing(
                        resolved,
                        horizontal_edges,
                        child.style.box_sizing,
                      );
                      intrinsic_max.min(intrinsic_min.max(resolved_border))
                    } else {
                      intrinsic_max.min(available.max(intrinsic_min))
                    }
                  }
                  CalcSizeBasis::Length(len) => {
                    let specified = resolve_length_for_width(
                      len,
                      percentage_base,
                      &child.style,
                      &self.font_context,
                      self.viewport_size,
                      root_font_metrics,
                    );
                    border_size_from_box_sizing(specified, horizontal_edges, child.style.box_sizing)
                  }
                }
                .max(0.0);
                let basis_content = (basis_border - horizontal_edges).max(0.0);
                let basis_specified = match child.style.box_sizing {
                  BoxSizing::ContentBox => basis_content,
                  BoxSizing::BorderBox => basis_border,
                };
                crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
                  .and_then(|expr_sum| {
                    crate::css::properties::parse_length(&format!("calc({expr_sum})"))
                  })
                  .map(|expr_len| {
                    let resolved_specified = resolve_length_for_width(
                      expr_len,
                      percentage_base,
                      &child.style,
                      &self.font_context,
                      self.viewport_size,
                      root_font_metrics,
                    );
                    border_size_from_box_sizing(
                      resolved_specified,
                      horizontal_edges,
                      child.style.box_sizing,
                    )
                    .max(0.0)
                  })
                  .unwrap_or(basis_border)
              }
            })
          });

        let min_width = if let Some(keyword) = child.style.min_width_keyword {
          match keyword {
            crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
            crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable => available,
            crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
              if let Some(limit) = limit {
                let resolved = resolve_length_for_width(
                  limit,
                  percentage_base,
                  &child.style,
                  &self.font_context,
                  self.viewport_size,
                  root_font_metrics,
                );
                let resolved_border =
                  border_size_from_box_sizing(resolved, horizontal_edges, child.style.box_sizing);
                intrinsic_max.min(intrinsic_min.max(resolved_border))
              } else {
                intrinsic_max.min(available.max(intrinsic_min))
              }
            }
            crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
              use crate::style::types::BoxSizing;
              use crate::style::types::CalcSizeBasis;
              let shrink = intrinsic_max.min(available.max(intrinsic_min));
              let basis_border = match calc.basis {
                CalcSizeBasis::Auto => shrink,
                CalcSizeBasis::MinContent => intrinsic_min,
                CalcSizeBasis::MaxContent => intrinsic_max,
                CalcSizeBasis::FillAvailable => available,
                CalcSizeBasis::FitContent { limit } => {
                  if let Some(limit) = limit {
                    let resolved = resolve_length_for_width(
                      limit,
                      percentage_base,
                      &child.style,
                      &self.font_context,
                      self.viewport_size,
                      root_font_metrics,
                    );
                    let resolved_border = border_size_from_box_sizing(
                      resolved,
                      horizontal_edges,
                      child.style.box_sizing,
                    );
                    intrinsic_max.min(intrinsic_min.max(resolved_border))
                  } else {
                    intrinsic_max.min(available.max(intrinsic_min))
                  }
                }
                CalcSizeBasis::Length(len) => {
                  let specified = resolve_length_for_width(
                    len,
                    percentage_base,
                    &child.style,
                    &self.font_context,
                    self.viewport_size,
                    root_font_metrics,
                  );
                  border_size_from_box_sizing(specified, horizontal_edges, child.style.box_sizing)
                }
              }
              .max(0.0);
              let basis_content = (basis_border - horizontal_edges).max(0.0);
              let basis_specified = match child.style.box_sizing {
                BoxSizing::ContentBox => basis_content,
                BoxSizing::BorderBox => basis_border,
              };
              crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
                .and_then(|expr_sum| {
                  crate::css::properties::parse_length(&format!("calc({expr_sum})"))
                })
                .map(|expr_len| {
                  let resolved_specified = resolve_length_for_width(
                    expr_len,
                    percentage_base,
                    &child.style,
                    &self.font_context,
                    self.viewport_size,
                    root_font_metrics,
                  );
                  border_size_from_box_sizing(
                    resolved_specified,
                    horizontal_edges,
                    child.style.box_sizing,
                  )
                  .max(0.0)
                })
                .unwrap_or(basis_border)
            }
          }
        } else {
          child
            .style
            .min_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                percentage_base,
                &child.style,
                &self.font_context,
                self.viewport_size,
                root_font_metrics,
              )
            })
            .map(|w| border_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(0.0)
        };
        let max_width = if let Some(keyword) = child.style.max_width_keyword {
          match keyword {
            crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
            crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable => available,
            crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
              if let Some(limit) = limit {
                let resolved = resolve_length_for_width(
                  limit,
                  percentage_base,
                  &child.style,
                  &self.font_context,
                  self.viewport_size,
                  root_font_metrics,
                );
                let resolved_border =
                  border_size_from_box_sizing(resolved, horizontal_edges, child.style.box_sizing);
                intrinsic_max.min(intrinsic_min.max(resolved_border))
              } else {
                intrinsic_max.min(available.max(intrinsic_min))
              }
            }
            crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
              use crate::style::types::BoxSizing;
              use crate::style::types::CalcSizeBasis;
              let shrink = intrinsic_max.min(available.max(intrinsic_min));
              let basis_border = match calc.basis {
                CalcSizeBasis::Auto => shrink,
                CalcSizeBasis::MinContent => intrinsic_min,
                CalcSizeBasis::MaxContent => intrinsic_max,
                CalcSizeBasis::FillAvailable => available,
                CalcSizeBasis::FitContent { limit } => {
                  if let Some(limit) = limit {
                    let resolved = resolve_length_for_width(
                      limit,
                      percentage_base,
                      &child.style,
                      &self.font_context,
                      self.viewport_size,
                      root_font_metrics,
                    );
                    let resolved_border = border_size_from_box_sizing(
                      resolved,
                      horizontal_edges,
                      child.style.box_sizing,
                    );
                    intrinsic_max.min(intrinsic_min.max(resolved_border))
                  } else {
                    intrinsic_max.min(available.max(intrinsic_min))
                  }
                }
                CalcSizeBasis::Length(len) => {
                  let specified = resolve_length_for_width(
                    len,
                    percentage_base,
                    &child.style,
                    &self.font_context,
                    self.viewport_size,
                    root_font_metrics,
                  );
                  border_size_from_box_sizing(specified, horizontal_edges, child.style.box_sizing)
                }
              }
              .max(0.0);
              let basis_content = (basis_border - horizontal_edges).max(0.0);
              let basis_specified = match child.style.box_sizing {
                BoxSizing::ContentBox => basis_content,
                BoxSizing::BorderBox => basis_border,
              };
              crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
                .and_then(|expr_sum| {
                  crate::css::properties::parse_length(&format!("calc({expr_sum})"))
                })
                .map(|expr_len| {
                  let resolved_specified = resolve_length_for_width(
                    expr_len,
                    percentage_base,
                    &child.style,
                    &self.font_context,
                    self.viewport_size,
                    root_font_metrics,
                  );
                  border_size_from_box_sizing(
                    resolved_specified,
                    horizontal_edges,
                    child.style.box_sizing,
                  )
                  .max(0.0)
                })
                .unwrap_or(basis_border)
            }
          }
        } else {
          child
            .style
            .max_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                percentage_base,
                &child.style,
                &self.font_context,
                self.viewport_size,
                root_font_metrics,
              )
            })
            .map(|w| border_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(f32::INFINITY)
        };
        let max_width = if max_width.is_finite() && max_width < min_width {
          min_width
        } else {
          max_width
        };

        let used_border_box = if let Some(specified) = specified_width {
          crate::layout::utils::clamp_with_order(specified, min_width, max_width)
        } else {
          let shrink = intrinsic_max.min(available.max(intrinsic_min));
          crate::layout::utils::clamp_with_order(shrink, min_width, max_width)
        };

        // Layout the float's contents using the *containing block* width as the percentage base
        // (CSS 2.1 §8.3), while forcing the used border-box width we computed above for `width:auto`
        // shrink-to-fit (CSS 2.1 §10.3.5). Passing the used content width as the constraint would
        // incorrectly resolve percentage padding/borders against the float's own content box.
        let width_auto = specified_width.is_none();
        // Floats establish their own formatting context roots, so their percentage block sizes (for
        // example `height: 100%`) resolve against the containing block provided by the caller.
        //
        // Use the parent's resolved block size as the "available height" only when the parent has
        // a definite block-size percentage base. This avoids incorrectly resolving percentages
        // against an unrelated ancestor's available height (e.g. the viewport) when the containing
        // block's height is `auto` (CSS2.1 §10.5).
        let parent_block_size_is_auto = if inline_is_horizontal {
          parent.style.height.is_none() && parent.style.height_keyword.is_none()
        } else {
          parent.style.width.is_none() && parent.style.width_keyword.is_none()
        };
        let parent_used_border_box_block_size = if inline_is_horizontal {
          constraints.used_border_box_height
        } else {
          constraints.used_border_box_width
        };
        let float_height_space =
          if parent_block_size_is_auto && parent_used_border_box_block_size.is_none() {
            AvailableSpace::Indefinite
          } else {
            match block_space {
              AvailableSpace::Definite(value) => AvailableSpace::Definite(value),
              _ => AvailableSpace::Indefinite,
            }
          };
        let child_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(containing_width),
          float_height_space,
        )
        .with_used_border_box_size(width_auto.then_some(used_border_box), None);
        let mut fragment = if fc_type == FormattingContextType::Block {
          child_bfc.layout(child, &child_constraints)?
        } else {
          let fc = factory.get(fc_type);
          // Non-block formatting contexts (grid/flex/table) return fragments in physical
          // coordinates. The block formatting context keeps fragments in logical coordinates
          // until `convert_fragment_axes` runs at the end of `layout`, so convert the subtree
          // back into logical space here to avoid double-applying the writing-mode transform.
          unconvert_fragment_axes_root(fc.layout(child, &child_constraints)?)
        };

        let block_sides = block_axis_sides(&child.style);
        let margin_top = resolve_margin_side(
          &child.style,
          block_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let margin_bottom = resolve_margin_side(
          &child.style,
          block_sides.1,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let box_width = used_border_box;
        let float_height = margin_top + fragment.bounds.height() + margin_bottom;

        let side = match resolve_float_side(
          child.style.float,
          parent.style.writing_mode,
          parent.style.direction,
        ) {
          Some(side) => side,
          None => {
            debug_assert!(false, "expected floating side for box_id={}", child.id);
            FloatSide::Left
          }
        };

        let (fx, fy) = float_ctx.compute_float_position_in_containing_block(
          side,
          margin_left + box_width + margin_right,
          float_height,
          float_base_y + float_cursor_y,
          float_base_x,
          containing_width,
        );
        // CSS 2.1 §9.5.1: a float's outer top may not be higher than the outer top of any float
        // generated by an element earlier in the source. If this float is pushed down to fit
        // alongside earlier floats, subsequent floats must not "rise back up" into the earlier
        // available space.
        float_cursor_y = (fy - float_base_y).max(float_cursor_y);

        fragment.bounds = Rect::from_xywh(
          fx + margin_left - float_base_x,
          fy - float_base_y + margin_top,
          box_width,
          fragment.bounds.height(),
        );
        let border_box_height = fragment.bounds.height();
        let margin_box = Rect::from_xywh(
          fx,
          fy,
          margin_left + box_width + margin_right,
          margin_top + border_box_height + margin_bottom,
        );
        let border_box = Rect::from_xywh(
          fx + margin_left,
          fy + margin_top,
          box_width,
          border_box_height,
        );
        let containing_block_size =
          Size::new(containing_width, block_space.to_option().unwrap_or(0.0));
        let shape = build_float_shape(
          &child.style,
          margin_box,
          border_box,
          containing_block_size,
          self.viewport_size,
          &self.font_context,
          factory.image_cache(),
        )?;
        float_ctx.add_float_with_shape(
          side,
          fx,
          fy,
          margin_left + box_width + margin_right,
          float_height,
          shape,
        );
        if owns_float_ctx {
          content_height = content_height.max(fy + float_height - float_base_y);
        }

        if child.style.position.is_relative() {
          let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
            &child.style,
            &relative_cb,
            self.viewport_size,
            &self.font_context,
          );
          fragment = PositionedLayout::with_font_context(self.font_context.clone())
            .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
        }
        fragments.push(fragment);
        continue;
      }

      // Layout in-flow children
      let treated_as_block = child.is_block_level()
        || matches!(child.box_type, BoxType::Replaced(_) if !child.style.display.is_inline_level());

      if treated_as_block {
        let _ = flush_inline_buffer(
          &mut inline_buffer,
          &mut fragments,
          &mut current_y,
          &mut content_height,
          &mut content_height_before_last_cursor,
          &mut margin_ctx,
          float_ctx,
          &mut deadline_counter,
          &mut positioned_children,
        )?;
        inline_buffer.clear();

        if dump_cell_child_y && matches!(parent.style.display, Display::TableCell) {
          eprintln!(
            "cell child layout: parent_id={} child_idx={} child_display={:?} current_y={:.2}",
            parent.id, child_idx, child.style.display, current_y
          );
        }
        if !trace_boxes.is_empty() && trace_boxes.contains(&child.id) {
          eprintln!(
                    "[trace-box-pre] id={} display={:?} width={:?} min=({:?},{:?}) max=({:?},{:?}) margin=({:?},{:?})",
                    child.id,
                    child.style.display,
                    child.style.width,
                    child.style.min_width,
                    child.style.min_height,
                    child.style.max_width,
                    child.style.max_height,
                    child.style.margin_left,
                     child.style.margin_right,
          );
        }

        let (fragment, next_y) =
          layout_in_flow_block_child(child, &mut margin_ctx, current_y, float_ctx)?;

        if dump_cell_child_y && matches!(parent.style.display, Display::TableCell) {
          let b = fragment.bounds;
          eprintln!(
                        "cell child placed: parent_id={} child_id={} display={:?} current_y={:.2} frag=({:.2},{:.2},{:.2},{:.2}) next_y={:.2}",
                        parent.id,
                        child.id,
                        child.style.display,
                        current_y,
                        b.x(),
                        b.y(),
                        b.width(),
                        b.height(),
                        next_y
                    );
        }
        if !trace_boxes.is_empty() && trace_boxes.contains(&child.id) {
          eprintln!(
                        "[trace-box] id={} display={:?} width={:?} height={:?} min=({:?},{:?}) max=({:?},{:?}) at y={:.2} -> next_y={:.2}",
                        child.id,
                        child.style.display,
                        child.style.width,
                        child.style.height,
                        child.style.min_width,
                        child.style.min_height,
                        child.style.max_width,
                        child.style.max_height,
                        current_y,
                        next_y
                    );
        }

        content_height_before_last_cursor = content_height;
        content_height = content_height.max(next_y);
        current_y = next_y;
        let mut fragment = fragment;
        if child.style.position.is_relative() {
          let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
            &child.style,
            &relative_cb,
            self.viewport_size,
            &self.font_context,
          );
          fragment = PositionedLayout::with_font_context(self.font_context.clone())
            .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
        }
        fragments.push(fragment);
      } else {
        // Inline-level non-replaced elements should still respect block/inline splits:
        // if this inline itself establishes a block formatting context (e.g., display:block
        // on an inline ancestor), flush the buffer and lay it out as a block.
        if child.is_block_level() {
          let _ = flush_inline_buffer(
            &mut inline_buffer,
            &mut fragments,
            &mut current_y,
            &mut content_height,
            &mut content_height_before_last_cursor,
            &mut margin_ctx,
            float_ctx,
            &mut deadline_counter,
            &mut positioned_children,
          )?;
          inline_buffer.clear();
          let (fragment, next_y) =
            layout_in_flow_block_child(child, &mut margin_ctx, current_y, float_ctx)?;
          content_height_before_last_cursor = content_height;
          content_height = content_height.max(next_y);
          current_y = next_y;
          let mut fragment = fragment;
          if child.style.position.is_relative() {
            let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
              &child.style,
              &relative_cb,
              self.viewport_size,
              &self.font_context,
            );
            fragment = PositionedLayout::with_font_context(self.font_context.clone())
              .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
          }
          fragments.push(fragment);
        } else {
          inline_buffer.push(child.clone());
        }
      }
    }

    let _ = flush_inline_buffer(
      &mut inline_buffer,
      &mut fragments,
      &mut current_y,
      &mut content_height,
      &mut content_height_before_last_cursor,
      &mut margin_ctx,
      float_ctx,
      &mut deadline_counter,
      &mut positioned_children,
    )?;
    inline_buffer.clear();

    // Resolve any trailing margins
    let trailing_margin = margin_ctx.pending_margin();
    let allow_collapse_last =
      !parent_is_independent_context_root && should_collapse_with_last_child(&parent.style);

    // Check for bottom separation
    let (_, parent_block_end) = block_axis_sides(&parent.style);
    let root_font_metrics = self.factory.root_font_metrics();
    let parent_has_bottom_separation = resolve_border_side(
      &parent.style,
      parent_block_end,
      containing_width,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    ) > 0.0
      || resolve_padding_side(
        &parent.style,
        parent_block_end,
        containing_width,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      ) > 0.0;

    let mut flow_height = current_y;
    if !allow_collapse_last || parent_has_bottom_separation {
      flow_height += trailing_margin;
    }
    if !flow_height.is_finite() {
      flow_height = 0.0;
    }
    flow_height = flow_height.max(0.0);

    // Float boxes extend the formatting context height for BFC roots (the "clearfix" behavior).
    let float_bottom = if owns_float_ctx {
      float_ctx
        .left_floats()
        .iter()
        .chain(float_ctx.right_floats())
        .map(|f| f.bottom())
        .fold(flow_height, f32::max)
    } else {
      flow_height
    };

    if let Some(err) = float_ctx.take_timeout_error() {
      return Err(err);
    }

    Ok((fragments, float_bottom, positioned_children))
  }

  fn is_multicol_container(style: &ComputedStyle) -> bool {
    style.column_count.unwrap_or(1) > 1 || style.column_width.is_some()
  }

  fn compute_column_geometry(
    &self,
    style: &ComputedStyle,
    available_inline: f32,
  ) -> (usize, f32, f32) {
    let available_inline = available_inline.max(0.0);
    let root_font_metrics = self.factory.root_font_metrics();
    let gap = resolve_length_for_width(
      style.column_gap,
      available_inline,
      style,
      &self.font_context,
      self.viewport_size,
      root_font_metrics,
    )
    .max(0.0);

    let specified_width = style.column_width.as_ref().and_then(|l| {
      let resolved = resolve_length_for_width(
        *l,
        available_inline,
        style,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      (resolved.is_finite() && resolved > 0.0).then_some(resolved)
    });
    let specified_count = style.column_count.unwrap_or(0) as usize;

    let compute_width = |count: usize| {
      let count = count.max(1) as f32;
      ((available_inline - gap * (count - 1.0)) / count).max(0.0)
    };

    if specified_count > 0 {
      if let Some(spec_width) = specified_width {
        let denom = spec_width + gap;
        let max_fit = if denom > 0.0 {
          ((available_inline + gap) / denom).floor() as usize
        } else {
          1
        };
        let count = specified_count.min(max_fit.max(1)).max(1);
        return (count, compute_width(count), gap);
      }

      let count = specified_count.max(1);
      return (count, compute_width(count), gap);
    }

    if let Some(spec_width) = specified_width {
      let denom = spec_width + gap;
      let count = if denom > 0.0 {
        ((available_inline + gap) / denom).floor().max(1.0) as usize
      } else {
        1
      };
      return (count, compute_width(count), gap);
    }

    (1, available_inline, gap)
  }

  fn set_logical_from_bounds(fragment: &mut FragmentNode) {
    fragment.logical_override = Some(fragment.bounds);
    for child in fragment.children_mut() {
      Self::set_logical_from_bounds(child);
    }
  }

  fn clear_logical_overrides(fragment: &mut FragmentNode) {
    fragment.logical_override = None;
    for child in fragment.children_mut() {
      Self::clear_logical_overrides(child);
    }
  }

  fn clone_with_children(parent: &BoxNode, children: Vec<BoxNode>) -> BoxNode {
    BoxNode {
      style: parent.style.clone(),
      original_display: parent.original_display,
      starting_style: parent.starting_style.clone(),
      box_type: parent.box_type.clone(),
      children,
      footnote_body: parent.footnote_body.clone(),
      id: parent.id,
      generated_pseudo: parent.generated_pseudo,
      debug_info: parent.debug_info.clone(),
      styled_node_id: parent.styled_node_id,
      implicit_anchor_box_id: parent.implicit_anchor_box_id,
      form_control: parent.form_control.clone(),
      table_cell_span: parent.table_cell_span,
      table_column_span: parent.table_column_span,
      first_line_style: parent.first_line_style.clone(),
      first_letter_style: parent.first_letter_style.clone(),
    }
  }

  fn translate_with_logical(
    fragment: &mut FragmentNode,
    dx: f32,
    physical_dy: f32,
    logical_dy: f32,
  ) {
    // Fragment bounds are stored in the local coordinate system of their parent fragment. When
    // positioning a laid-out subtree within a parent, only the *root* bounds should be translated;
    // translating descendants would effectively apply the offset multiple times as absolute
    // positions are computed by accumulating parent + child offsets.
    fragment.bounds = Rect::from_xywh(
      fragment.bounds.x() + dx,
      fragment.bounds.y() + physical_dy,
      fragment.bounds.width(),
      fragment.bounds.height(),
    );
    if let Some(logical) = fragment.logical_override {
      fragment.logical_override = Some(Rect::from_xywh(
        logical.x() + dx,
        logical.y() + logical_dy,
        logical.width(),
        logical.height(),
      ));
    }
    // Mirror `FragmentNode::translate_root_in_place` semantics: moving a running/footnote anchor
    // must move its stored snapshot subtree as well.
    fragment.starting_style = None;
    match &mut fragment.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot, .. } => {
        Arc::make_mut(snapshot).translate_root_in_place(Point::new(dx, physical_dy));
      }
      _ => {}
    }
  }

  fn layout_column_segment(
    &self,
    parent: &BoxNode,
    children: &[BoxNode],
    column_count: usize,
    column_width: f32,
    column_gap: f32,
    available_block: AvailableSpace,
    column_fill: ColumnFill,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    paint_viewport: Rect,
  ) -> Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>, f32), LayoutError> {
    let mut deadline_counter = 0usize;
    if children.is_empty() {
      return Ok((Vec::new(), 0.0, Vec::new(), 0.0));
    }

    let writing_mode = parent.style.writing_mode;
    let direction = parent.style.direction;
    let inline_is_horizontal = inline_axis_is_horizontal(writing_mode);
    let inline_positive = inline_axis_positive(writing_mode, direction);
    let inline_sign = if inline_positive { 1.0 } else { -1.0 };
    let axes = FragmentAxes::from_writing_mode_and_direction(writing_mode, direction);

    let column_constraints = if inline_is_horizontal {
      LayoutConstraints::new(AvailableSpace::Definite(column_width), available_block)
    } else {
      LayoutConstraints::new(available_block, AvailableSpace::Definite(column_width))
    }
    .with_inline_percentage_base(Some(column_width))
    .with_block_percentage_base(available_block.to_option());

    if column_count <= 1 {
      let parent_clone = Self::clone_with_children(parent, children.to_vec());
      let (frags, height, positioned) = self.layout_children(
        &parent_clone,
        &column_constraints,
        nearest_positioned_cb,
        nearest_fixed_cb,
        paint_viewport,
      )?;
      return Ok((frags, height, positioned, height));
    }

    let parent_clone = Self::clone_with_children(parent, children.to_vec());
    let (flow_fragments, flow_height, flow_positioned) = self.layout_children(
      &parent_clone,
      &column_constraints,
      nearest_positioned_cb,
      nearest_fixed_cb,
      paint_viewport,
    )?;
    let flow_fragments: Vec<FragmentNode> = flow_fragments
      .into_iter()
      .map(|mut frag| {
        Self::set_logical_from_bounds(&mut frag);
        frag
      })
      .collect();

    let mut flow_root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, column_width, flow_height),
      flow_fragments,
    );
    flow_root.style = Some(parent.style.clone());
    let flow_content_height = flow_height.max(flow_root.logical_bounding_box().height());
    let mut flow_block_size = flow_content_height;
    if let AvailableSpace::Definite(h) = available_block {
      if h.is_finite() && h > 0.0 {
        flow_block_size = flow_block_size.max(h);
      }
    }
    if flow_block_size.is_finite() && flow_block_size > 0.0 {
      flow_root.bounds = Rect::from_xywh(0.0, 0.0, column_width, flow_block_size);
    }

    let mut physical_flow_root = flow_root.clone();
    Self::clear_logical_overrides(&mut physical_flow_root);
    physical_flow_root = convert_fragment_axes(
      physical_flow_root,
      column_width,
      flow_block_size,
      writing_mode,
      direction,
    );

    let mut analyzer = FragmentationAnalyzer::new(
      &physical_flow_root,
      FragmentationContext::Column,
      axes,
      false,
      None,
    );
    let mut flow_extent = analyzer.content_extent();

    let balanced_height = if column_count > 0 {
      flow_extent / column_count as f32
    } else {
      flow_extent
    };
    let fragmentainer_hint = crate::layout::formatting_context::fragmentainer_block_size_hint()
      .filter(|h| h.is_finite() && *h > 0.0);
    let fragmented_context = fragmentainer_hint.is_some();
    let fragmentainer_axes =
      crate::layout::formatting_context::fragmentainer_axes_hint().unwrap_or(axes);
    let mut column_height = match column_fill {
      ColumnFill::Auto => match available_block {
        AvailableSpace::Definite(h) => h,
        _ => balanced_height,
      },
      ColumnFill::Balance | ColumnFill::BalanceAll => balanced_height,
    };
    if matches!(available_block, AvailableSpace::Indefinite) {
      if let Some(hint) = fragmentainer_hint {
        column_height = hint;
      }
    }
    if matches!(column_fill, ColumnFill::Balance | ColumnFill::BalanceAll)
      && fragmentainer_hint.is_none()
      && column_height.is_finite()
      && column_height > 0.0
      && flow_extent.is_finite()
      && flow_extent > 0.0
      && column_count > 1
    {
      let max_height = match available_block {
        AvailableSpace::Definite(h) if h.is_finite() && h > 0.0 => h,
        _ => flow_extent,
      };
      let max_height = max_height.max(0.0);
      let min_height = column_height.min(max_height);
      if max_height > 0.0 && min_height > 0.0 {
        let mut fragment_count_for = |height: f32| -> Result<usize, LayoutError> {
          Ok(
            analyzer
              .boundaries(height, flow_extent.max(height))?
              .len()
              .saturating_sub(1),
          )
        };

        let count_at_max = fragment_count_for(max_height)?;
        if count_at_max > column_count {
          column_height = max_height;
        } else {
          let count_at_min = fragment_count_for(min_height)?;
          if count_at_min > column_count {
            let mut low = min_height;
            let mut high = max_height;
            for _ in 0..16 {
              let mid = (low + high) / 2.0;
              let count_at_mid = fragment_count_for(mid)?;
              if count_at_mid <= column_count {
                high = mid;
              } else {
                low = mid;
              }
            }
            column_height = high;
            if fragment_count_for(column_height)? > column_count {
              column_height = max_height;
            }
          } else {
            column_height = min_height;
          }
        }
      }
    }
    if let AvailableSpace::Definite(h) = available_block {
      if h.is_finite() && h > 0.0 {
        column_height = column_height.min(h);
      }
    }
    if !column_height.is_finite() || column_height <= 0.0 {
      column_height = flow_extent.max(0.0);
    }
    if column_height <= 0.0 {
      return Ok((
        flow_root.children.to_vec(),
        flow_content_height,
        flow_positioned,
        flow_content_height,
      ));
    }

    let root_block_size = axes.block_size(&physical_flow_root.bounds);
    let page_size = fragmentainer_hint.unwrap_or(column_height);
    const PAGE_OFFSET_EPSILON: f32 = 0.01;
    let (_offset_in_page, first_set_height) = if fragmented_context {
      let abs_offset = crate::layout::formatting_context::fragmentainer_block_offset_hint();
      let abs_offset = if abs_offset.is_finite() {
        abs_offset
      } else {
        0.0
      };
      let mut offset_in_page = abs_offset.rem_euclid(page_size);
      if !offset_in_page.is_finite() {
        offset_in_page = 0.0;
      }
      // Avoid treating tiny float rounding noise as a real intra-page offset.
      if offset_in_page <= PAGE_OFFSET_EPSILON
        || (page_size - offset_in_page).abs() <= PAGE_OFFSET_EPSILON
      {
        offset_in_page = 0.0;
      }
      let first_set_height = if offset_in_page > PAGE_OFFSET_EPSILON {
        (page_size - offset_in_page).max(0.0)
      } else {
        page_size
      };
      (offset_in_page, first_set_height)
    } else {
      (0.0, page_size)
    };

    let fragmentainer_size_for_set = |set: usize| -> f32 {
      if fragmented_context && set == 0 {
        first_set_height
      } else {
        page_size
      }
    };
    let fragmentainer_size_for_column = |index: usize| -> f32 {
      if fragmented_context {
        fragmentainer_size_for_set(index / column_count)
      } else {
        column_height
      }
    };
    let set_offset = |set: usize| -> f32 {
      if !fragmented_context || set == 0 {
        0.0
      } else {
        first_set_height + (set.saturating_sub(1) as f32) * page_size
      }
    };

    // Some forced breaks (e.g. `break-after: column`) can be specified on elements that have zero
    // block-size in the fragmentation axis. In that case the break does not advance the flow
    // cursor, so we must explicitly insert blank space so subsequent siblings land in the next
    // column.
    let mut required_total_extent = 0.0f32;
    let mut shifted_forced_breaks = false;
    if column_height.is_finite() && column_height > 0.0 && !physical_flow_root.children.is_empty() {
      const EPS: f32 = 0.01;
      let mut idx = 0usize;
      while idx + 1 < physical_flow_root.children.len() {
        let (child_end, child_block_size, next_start, forced_between) = {
          let child = &physical_flow_root.children[idx];
          let next = &physical_flow_root.children[idx + 1];
          let child_start = axes.abs_block_start(&child.bounds, 0.0, root_block_size);
          let child_block_size = axes.block_size(&child.bounds);
          let child_end = child_start + child_block_size;
          let next_start = axes.abs_block_start(&next.bounds, 0.0, root_block_size);
          let child_after = child
            .style
            .as_ref()
            .map(|s| s.break_after)
            .unwrap_or(crate::style::types::BreakBetween::Auto);
          let next_before = next
            .style
            .as_ref()
            .map(|s| s.break_before)
            .unwrap_or(crate::style::types::BreakBetween::Auto);
          let forced_between = forces_break_between(child_after, FragmentationContext::Column)
            || forces_break_between(next_before, FragmentationContext::Column);
          (child_end, child_block_size, next_start, forced_between)
        };

        let non_advancing =
          child_block_size <= EPS && next_start <= child_end + EPS && forced_between;
        if non_advancing {
          let first_set_span = if fragmented_context {
            first_set_height.max(0.0) * column_count as f32
          } else {
            0.0
          };
          let (fragmentainer, remainder) =
            if fragmented_context && child_end >= first_set_span - EPS {
              let remainder = (child_end - first_set_span).rem_euclid(page_size);
              (page_size, remainder)
            } else {
              let remainder = child_end.rem_euclid(first_set_height);
              (first_set_height, remainder)
            };

          let advance = if remainder <= EPS {
            fragmentainer
          } else {
            fragmentainer - remainder
          };
          if advance.is_finite() && advance > EPS {
            let delta = axes.block_offset(advance);
            for sibling in physical_flow_root.children_mut().iter_mut().skip(idx + 1) {
              sibling.translate_root_in_place(delta);
            }
            shifted_forced_breaks = true;
          }
          let shifted = child_end + advance;
          let next_fragmentainer = if fragmented_context {
            if shifted >= first_set_span - EPS {
              page_size
            } else {
              first_set_height
            }
          } else {
            column_height
          };
          required_total_extent = required_total_extent.max(shifted + next_fragmentainer);
        }

        idx += 1;
      }
      if shifted_forced_breaks {
        analyzer = FragmentationAnalyzer::new(
          &physical_flow_root,
          FragmentationContext::Column,
          axes,
          false,
          None,
        );
        flow_extent = analyzer.content_extent();
      }
    }

    // In a paged context, page/side breaks inside a multi-column container must be promoted to
    // *column set* boundaries (i.e. between page-sized sets of columns) so the outer paginator never
    // fragments mid column-set.
    //
    // Collect forced pagination boundaries from the uncolumnized flow root and inject them into the
    // column fragmentation analyzer so boundary selection (which runs in
    // `FragmentationContext::Column`) still accounts for `break-before/after: page|left|right|recto|verso`.
    let mut forced_pagination_boundaries: Vec<ForcedBoundary> = Vec::new();
    if fragmented_context {
      let page_progression_is_ltr = fragmentainer_axes.page_progression_is_ltr();
      // `break-before/after: always` forces a break in the *immediately containing* fragmentation
      // context (CSS Break 4). In paged multi-column layout that is the column context, so `always`
      // must not be promoted to a column-set boundary (which would create an extra page).
      forced_pagination_boundaries =
        collect_forced_boundaries_for_explicit_page_breaks_with_axes_and_page_progression(
          &physical_flow_root,
          0.0,
          axes,
          page_progression_is_ltr,
        );

      // Deduplicate forced boundaries by position and merge side constraints (matching the @page
      // paginator's `dedup_forced_boundaries` logic).
      const EPSILON: f32 = 0.01;
      forced_pagination_boundaries.sort_by(|a, b| {
        a.position
          .partial_cmp(&b.position)
          .unwrap_or(std::cmp::Ordering::Equal)
      });
      let mut deduped: Vec<ForcedBoundary> = Vec::new();
      for boundary in forced_pagination_boundaries.drain(..) {
        if let Some(last) = deduped.last_mut() {
          if (last.position - boundary.position).abs() < EPSILON {
            match (last.page_side, boundary.page_side) {
              (None, side) => last.page_side = side,
              (side, None) => last.page_side = side,
              (Some(a), Some(b)) if a == b => last.page_side = Some(a),
              // Conflicting side constraints at the same boundary are unsatisfiable; treat it as a
              // generic forced break.
              (Some(_), Some(_)) => last.page_side = None,
            }
            continue;
          }
        }
        deduped.push(boundary);
      }
      forced_pagination_boundaries = deduped;

      analyzer.add_forced_break_positions(
        forced_pagination_boundaries
          .iter()
          .map(|boundary| boundary.position),
      );
    }

    let min_total_extent = if fragmented_context {
      first_set_height
    } else {
      column_height
    };
    // When paginating a multi-column container we still need the segment's final height to reach
    // at least one full fragmentainer (page/column-set) so subsequent siblings start on the next
    // page boundary. However, the *column slicing* should stop at the end of actual content (plus
    // any extra space required by non-advancing forced breaks). Otherwise the boundary generator
    // will treat trailing empty space as an additional column, which inflates the computed column
    // count and can create spurious extra column-sets before `column-span: all` spanners.
    let boundaries_extent = flow_extent.max(required_total_extent);
    let total_extent = boundaries_extent.max(min_total_extent);
    let mut boundaries = if fragmented_context {
      let mut boundaries = vec![0.0];
      let mut start = 0.0f32;
      let mut column_index = 0usize;
      while start < boundaries_extent - PAGE_OFFSET_EPSILON {
        let fragmentainer_size = fragmentainer_size_for_column(column_index);
        let next = analyzer.next_boundary(start, fragmentainer_size, boundaries_extent)?;
        debug_assert!(
          next + PAGE_OFFSET_EPSILON >= start,
          "fragmentation boundary must not move backwards"
        );
        if (next - start).abs() < PAGE_OFFSET_EPSILON {
          boundaries.push(boundaries_extent);
          break;
        }
        boundaries.push(next);
        start = next;
        column_index = column_index.saturating_add(1);
      }
      if boundaries_extent - *boundaries.last().unwrap_or(&0.0) > PAGE_OFFSET_EPSILON {
        boundaries.push(boundaries_extent);
      }
      boundaries
    } else {
      analyzer.boundaries(column_height, total_extent)?
    };
    let mut fragment_count = boundaries.len().saturating_sub(1);
    if fragmentainer_hint.is_none()
      && matches!(column_fill, ColumnFill::Balance | ColumnFill::BalanceAll)
      && fragment_count == column_count
    {
      let balanced = analyzer.balanced_boundaries(column_count, column_height, total_extent)?;
      let balanced_count = balanced.len().saturating_sub(1);
      if balanced_count == column_count {
        boundaries = balanced;
      }
    }

    // Promote any forced pagination break positions to column-set boundaries. When the break occurs
    // mid column-set and there is still in-segment content after it, we pad the boundary list with
    // zero-length fragments (empty columns) so the remaining content starts at the first column of a
    // new set. If the forced break is at the end of this segment's content (e.g. immediately before a
    // `column-span: all` spanner), we skip padding so we don't create an extra empty set; instead we
    // still record a promoted boundary so side constraints are communicated to the outer paginator.
    let mut promoted_set_start_slices: Vec<(usize, Option<PageSide>)> = Vec::new();
    if fragmented_context && !forced_pagination_boundaries.is_empty() && column_count > 0 {
      const EPSILON: f32 = 0.01;
      // `flow_extent` reflects the analyzer's content extent, which can include synthetic trailing
      // space (e.g. when the uncolumnized flow root is enlarged by a fragmentainer size hint).
      // When deciding whether a forced page/side break occurs at the *end of real content* (so we
      // should avoid padding extra empty columns/sets), use the geometric extent of the actual
      // descendants instead.
      let content_end = physical_flow_root
        .children
        .iter()
        .map(|child| {
          let bbox = child.logical_bounding_box();
          axes.abs_block_end(&bbox, 0.0, root_block_size)
        })
        .fold(0.0f32, f32::max);
      for forced in &forced_pagination_boundaries {
        let p = forced.position;
        if !p.is_finite() {
          continue;
        }
        let Some(boundary_index) = boundaries.iter().position(|b| (*b - p).abs() < EPSILON) else {
          continue;
        };

        // The next slice index in the current boundary list that would start a new set.
        let pad = (column_count - (boundary_index % column_count)) % column_count;
        let start_slice = boundary_index + pad;
        if start_slice % column_count != 0 {
          continue;
        }

        let has_segment_content_after = p + EPSILON < content_end;
        if has_segment_content_after {
          // Pad after the forced break so the slice index containing content after `p` becomes a
          // multiple of `column_count` (start of the next column set).
          for _ in 0..pad {
            boundaries.insert(boundary_index + 1, p);
          }
        }

        // Always record the promoted boundary so side constraints (left/right/recto/verso) are
        // applied at the set boundary even when the break is at the end of the segment and the next
        // content is outside the segment (e.g. a spanner).
        promoted_set_start_slices.push((start_slice, forced.page_side));
      }

      promoted_set_start_slices.sort_by_key(|(idx, _)| *idx);
      promoted_set_start_slices.dedup_by(|a, b| {
        if a.0 != b.0 {
          return false;
        }
        a.1 = match (a.1, b.1) {
          (None, side) => side,
          (side, None) => side,
          (Some(a), Some(b)) if a == b => Some(a),
          (Some(_), Some(_)) => None,
        };
        true
      });
    }

    // In paged/fragmented contexts the fragmentainer block-size hint pins the physical column
    // height to a fixed value (e.g. the page height). `column-fill: balance` and `balance-all`
    // still need to distribute content evenly across the columns inside each fragmentainer,
    // leaving whitespace at the bottom of shorter columns rather than filling sequentially.
    if fragmentainer_hint.is_some()
      && matches!(column_fill, ColumnFill::Balance | ColumnFill::BalanceAll)
      && column_count > 1
    {
      let base_boundaries = boundaries;
      let base_fragment_count = base_boundaries.len().saturating_sub(1);
      let set_count = (base_fragment_count + column_count - 1) / column_count;
      if base_fragment_count > 0 && set_count > 0 {
        let last_set = set_count.saturating_sub(1);
        let mut balanced_boundaries =
          Vec::with_capacity(set_count.saturating_mul(column_count) + 1);
        balanced_boundaries.push(*base_boundaries.first().unwrap_or(&0.0));

        for set in 0..set_count {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(&mut deadline_counter, 16, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }

          let start_idx = set * column_count;
          let end_idx = ((set + 1) * column_count).min(base_fragment_count);
          let set_start = base_boundaries.get(start_idx).copied().unwrap_or(0.0);
          let set_end = base_boundaries.get(end_idx).copied().unwrap_or(set_start);
          let should_balance = match column_fill {
            ColumnFill::BalanceAll => true,
            ColumnFill::Balance => set == last_set,
            _ => false,
          };

          if !should_balance {
            if start_idx + 1 <= end_idx && end_idx < base_boundaries.len() {
              balanced_boundaries.extend_from_slice(&base_boundaries[start_idx + 1..=end_idx]);
            }
            // Ensure subsequent sets map to the correct column indices even if the analyzer
            // produces fewer fragments (empty columns are represented as zero-length fragments).
            while balanced_boundaries.len() < (set + 1) * column_count + 1 {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let set_total_extent = (set_end - set_start).max(0.0);
          if set_total_extent <= 0.0 {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          // Only analyze the actual content in the set. Some callers extend the boundary list to
          // include trailing empty space (e.g. when the content is shorter than the fragmentainer).
          // Fragmenting that trailing empty region can create spurious extra "columns".
          let set_content_end = flow_extent.min(set_end).max(set_start);
          let set_content_total = (set_content_end - set_start).max(0.0);
          if set_content_total <= 0.0 {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let set_fragmentainer_size = fragmentainer_size_for_set(set);
          let clipped_content = clip_node_with_axes(
            &physical_flow_root,
            set_start,
            set_content_end,
            0.0,
            set_start,
            root_block_size,
            axes,
            0,
            1,
            FragmentationContext::Column,
            set_fragmentainer_size,
          )?;
          let Some(mut clipped_content) = clipped_content else {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          };

          // `clip_node_with_axes` retains the original fragment slice metadata. Reset it so the
          // analyzer's `content_extent` is bounded to the clipped range.
          clipped_content.slice_info =
            crate::tree::fragment_tree::FragmentSliceInfo::single(set_content_total);

          let mut set_analyzer = FragmentationAnalyzer::new(
            &clipped_content,
            FragmentationContext::Column,
            axes,
            false,
            None,
          );
          let content_extent = set_analyzer
            .content_extent()
            .max(0.0)
            .min(set_content_total);
          if content_extent <= 0.0 {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let min_height = (content_extent / column_count as f32).max(0.0);
          let max_height = set_fragmentainer_size.min(content_extent).max(0.0);
          if min_height > set_fragmentainer_size || max_height <= 0.0 {
            if start_idx + 1 <= end_idx && end_idx < base_boundaries.len() {
              balanced_boundaries.extend_from_slice(&base_boundaries[start_idx + 1..=end_idx]);
            }
            while balanced_boundaries.len() < (set + 1) * column_count + 1 {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let mut fragment_count_for = |height: f32| -> Result<usize, LayoutError> {
            Ok(
              set_analyzer
                .boundaries_clamped_total(height, content_extent)?
                .len()
                .saturating_sub(1),
            )
          };
          let count_at_max = fragment_count_for(max_height)?;
          if count_at_max > column_count {
            if start_idx + 1 <= end_idx && end_idx < base_boundaries.len() {
              balanced_boundaries.extend_from_slice(&base_boundaries[start_idx + 1..=end_idx]);
            }
            while balanced_boundaries.len() < (set + 1) * column_count + 1 {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let used_height = {
            let count_at_min = fragment_count_for(min_height)?;
            if count_at_min > column_count {
              let mut low = min_height;
              let mut high = max_height;
              for _ in 0..16 {
                let mid = (low + high) / 2.0;
                let count_at_mid = fragment_count_for(mid)?;
                if count_at_mid <= column_count {
                  high = mid;
                } else {
                  low = mid;
                }
              }
              let mut height = high;
              if fragment_count_for(height)? > column_count {
                height = max_height;
              }
              height
            } else {
              min_height
            }
          };

          let mut set_boundaries =
            set_analyzer.boundaries_clamped_total(used_height, content_extent)?;
          if let Some(last) = set_boundaries.last_mut() {
            *last = set_total_extent;
          }
          while set_boundaries.len() < column_count + 1 {
            set_boundaries.push(set_total_extent);
          }
          for boundary in set_boundaries.iter().skip(1) {
            balanced_boundaries.push(set_start + *boundary);
          }
        }

        boundaries = balanced_boundaries;
      } else {
        boundaries = base_boundaries;
      }
    }
    fragment_count = boundaries.len().saturating_sub(1);
    if fragment_count == 0 {
      return Ok((
        flow_root.children.to_vec(),
        flow_content_height,
        flow_positioned,
        flow_content_height,
      ));
    }

    let stride = column_width + column_gap;
    let mut fragments = Vec::new();
    let mut fragment_heights = vec![0.0f32; fragment_count];
    let mut fragment_has_content = vec![false; fragment_count];

    // Clear forced page/side break hints inside the columnized subtree so the outer paginator does
    // not select an in-set boundary (which would split a column set). The promoted breaks are
    // reintroduced via marker fragments at column-set boundaries.
    fn rewrite_pagination_breaks_in_place(node: &mut FragmentNode) {
      let rewrite = |value: crate::style::types::BreakBetween| match value {
        crate::style::types::BreakBetween::Page
        | crate::style::types::BreakBetween::Left
        | crate::style::types::BreakBetween::Right
        | crate::style::types::BreakBetween::Recto
        | crate::style::types::BreakBetween::Verso => crate::style::types::BreakBetween::Auto,
        other => other,
      };

      if let Some(style) = node.style.as_ref() {
        let new_before = rewrite(style.break_before);
        let new_after = rewrite(style.break_after);
        if new_before != style.break_before || new_after != style.break_after {
          let mut updated = style.as_ref().clone();
          updated.break_before = new_before;
          updated.break_after = new_after;
          node.style = Some(Arc::new(updated));
        }
      }
      for child in node.children_mut().iter_mut() {
        rewrite_pagination_breaks_in_place(child);
      }
    }

    let mut promoted_break_iter = promoted_set_start_slices.iter().peekable();
    let push_promoted_marker =
      |fragments: &mut Vec<FragmentNode>, slice_idx: usize, side: Option<PageSide>| {
        let set_index = slice_idx / column_count;
        // Markers sit at the start of a column set boundary. Give them a tiny extent so they are
        // associated with the *preceding* page when the set boundary lands exactly on a page boundary
        // (otherwise zero-sized blocks at the boundary would be assigned to the next page, creating an
        // extra blank page when the marker forces a break).
        const MARKER_BLOCK_SIZE: f32 = 0.02;
        let set_start = set_offset(set_index).max(0.0);
        let block_size = if set_start > MARKER_BLOCK_SIZE {
          MARKER_BLOCK_SIZE
        } else {
          0.0
        };
        let marker_start = (set_start - block_size).max(0.0);
        let mut offset = fragmentainer_axes.block_offset(marker_start);
        if !fragmentainer_axes.block_positive() {
          let adjust = fragmentainer_axes.block_offset((page_size - block_size).max(0.0));
          offset = offset.translate(Point::new(-adjust.x, -adjust.y));
        }
        let bounds = match fragmentainer_axes.block_axis() {
          crate::layout::axis::PhysicalAxis::Y => {
            Rect::from_xywh(offset.x, offset.y, 0.0, block_size)
          }
          crate::layout::axis::PhysicalAxis::X => {
            Rect::from_xywh(offset.x, offset.y, block_size, 0.0)
          }
        };
        let mut marker_style = ComputedStyle::default();
        marker_style.display = Display::Block;
        marker_style.writing_mode = writing_mode;
        marker_style.direction = direction;
        // The marker represents a forced column-set boundary. Use `break-after` so the forced break
        // is anchored to the marker's own position rather than the end edge of the preceding column,
        // which may be shorter than the fragmentainer height in paged multi-column layout.
        marker_style.break_after = match side {
          Some(PageSide::Left) => crate::style::types::BreakBetween::Left,
          Some(PageSide::Right) => crate::style::types::BreakBetween::Right,
          None => crate::style::types::BreakBetween::Page,
        };
        fragments.push(FragmentNode::new_block_styled(
          bounds,
          Vec::new(),
          Arc::new(marker_style),
        ));
      };

    for (index, window) in boundaries.windows(2).enumerate() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 32, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      let start = window[0];
      let end = window[1];
      if end <= start {
        continue;
      }

      if fragmented_context {
        while promoted_break_iter
          .peek()
          .is_some_and(|&&(slice_idx, _)| slice_idx == index)
        {
          let Some((slice_idx, side)) = promoted_break_iter.next().copied() else {
            break;
          };
          push_promoted_marker(&mut fragments, slice_idx, side);
        }
      }
      let fragmentainer_size = fragmentainer_size_for_column(index);
      if let Some(mut clipped) = clip_node_with_axes(
        &physical_flow_root,
        start,
        end,
        0.0,
        start,
        root_block_size,
        axes,
        index,
        fragment_count,
        FragmentationContext::Column,
        fragmentainer_size,
      )? {
        let has_content = !clipped.children.is_empty();
        fragment_has_content[index] = has_content;
        normalize_fragment_margins_with_axes(
          &mut clipped,
          index == 0,
          index + 1 >= fragment_count,
          index != 0 && analyzer.is_forced_break_at(start),
          index + 1 != fragment_count && analyzer.is_forced_break_at(end),
          fragmentainer_size,
          axes,
        );
        propagate_fragment_metadata(&mut clipped, index, fragment_count);
        let col = if fragmented_context {
          index % column_count
        } else {
          index
        };
        let set = if fragmented_context {
          index / column_count
        } else {
          0
        };
        propagate_fragmentainer_columns(&mut clipped, set, col);
        let offset = axes
          .inline_offset(col as f32 * stride)
          .translate(fragmentainer_axes.block_offset(set_offset(set)));
        fragment_heights[index] = axes.block_size(&clipped.logical_bounding_box());
        let mut children: Vec<_> = std::mem::take(&mut clipped.children).into_iter().collect();
        for child in &mut children {
          if fragmented_context {
            rewrite_pagination_breaks_in_place(child);
          }
          child.translate_root_in_place(offset);
        }
        fragments.extend(children);
      }
    }
    if fragmented_context {
      while let Some((slice_idx, side)) = promoted_break_iter.next().copied() {
        push_promoted_marker(&mut fragments, slice_idx, side);
      }
    }

    let mut positioned_children = Vec::new();
    for mut positioned in flow_positioned {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 32, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      if let Some(pos) = positioned.static_position {
        if fragment_count > 0 && !boundaries.is_empty() {
          let flow_coord = pos.y;
          let frag_index = boundaries
            .windows(2)
            .position(|w| flow_coord >= w[0] - 0.01 && flow_coord < w[1] + 0.01)
            .unwrap_or(fragment_count - 1);
          let start = boundaries[frag_index];
          let col = if fragmented_context {
            frag_index % column_count
          } else {
            frag_index
          };
          let set = if fragmented_context {
            frag_index / column_count
          } else {
            0
          };
          let mut translated = pos;
          let column_delta = if inline_positive {
            col as f32 * stride
          } else {
            -(col as f32 * stride)
          };
          let block_delta = set_offset(set) - start;
          translated.x += column_delta;
          translated.y += block_delta;
          positioned.static_position = Some(translated);
        }
      }
      positioned_children.push(positioned);
    }

    let set_count = if fragment_count == 0 {
      0
    } else if fragmented_context {
      (fragment_count + column_count - 1) / column_count
    } else {
      1
    };
    let mut set_heights = vec![0.0f32; set_count];
    for (idx, height) in fragment_heights.iter().copied().enumerate() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      let set = if fragmented_context {
        idx / column_count
      } else {
        0
      };
      if set < set_heights.len() {
        set_heights[set] = set_heights[set].max(height);
      }
    }

    let mut segment_height = if fragmented_context {
      let last_set_bottom = if set_count > 0 {
        let last_set = set_count - 1;
        let mut bottom = 0.0f32;
        for (idx, height) in fragment_heights.iter().copied().enumerate() {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }
          if idx / column_count == last_set {
            bottom = bottom.max(set_offset(last_set) + height);
          }
        }
        bottom
      } else {
        0.0
      };
      let base = if set_count > 0 {
        first_set_height + (set_count.saturating_sub(1) as f32) * page_size
      } else {
        0.0
      };
      base.max(last_set_bottom)
    } else {
      let mut height = fragment_heights.iter().copied().fold(0.0, f32::max);
      if matches!(column_fill, ColumnFill::Auto) {
        if let AvailableSpace::Definite(h) = available_block {
          if h.is_finite() && h > 0.0 {
            height = height.max(h);
          }
        }
      }
      height
    };
    if segment_height == 0.0 {
      segment_height = flow_content_height;
    }

    let parent_inline_size = if column_count > 0 {
      column_width * column_count as f32 + column_gap * column_count.saturating_sub(1) as f32
    } else {
      column_width
    };
    let mut fragments: Vec<FragmentNode> = fragments
      .into_iter()
      .map(|fragment| {
        let mut converted = unconvert_fragment_axes(
          fragment,
          parent_inline_size,
          segment_height,
          writing_mode,
          direction,
        );
        Self::set_logical_from_bounds(&mut converted);
        converted
      })
      .collect();

    if column_count > 1
      && column_gap > 0.0
      && !matches!(
        parent.style.column_rule_style,
        BorderStyle::None | BorderStyle::Hidden
      )
    {
      let root_font_metrics = self.factory.root_font_metrics();
      let mut rule_width = resolve_length_for_width(
        parent.style.column_rule_width,
        column_width,
        &parent.style,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      )
      .min(column_gap)
      .max(0.0);
      if rule_width > 0.0 {
        let color = parent.style.column_rule_color.unwrap_or(parent.style.color);
        let mut rule_style = ComputedStyle::default();
        rule_style.writing_mode = writing_mode;
        rule_style.direction = direction;
        if rule_width > column_gap {
          rule_width = column_gap;
        }
        rule_style.display = Display::Block;
        rule_style.writing_mode = writing_mode;
        rule_style.direction = direction;
        if inline_is_horizontal {
          rule_style.border_left_width = Length::px(rule_width);
          rule_style.border_left_style = parent.style.column_rule_style;
          rule_style.border_left_color = color;
        } else {
          rule_style.border_top_width = Length::px(rule_width);
          rule_style.border_top_style = parent.style.column_rule_style;
          rule_style.border_top_color = color;
        }
        let rule_style = Arc::new(rule_style);
        for set in 0..set_count {
          let cols_in_set = if fragmented_context {
            let remaining = fragment_count.saturating_sub(set * column_count);
            remaining.min(column_count)
          } else if set == 0 {
            fragment_count
          } else {
            0
          };
          if cols_in_set < 2 {
            continue;
          }

          let rule_extent = if fragmented_context {
            fragmentainer_size_for_set(set).max(set_heights.get(set).copied().unwrap_or(0.0))
          } else {
            segment_height
          };
          for i in 1..cols_in_set {
            let left_idx = if fragmented_context {
              set * column_count + (i - 1)
            } else {
              i - 1
            };
            let right_idx = left_idx + 1;
            if !fragment_has_content[left_idx] || !fragment_has_content[right_idx] {
              continue;
            }

            let prev_origin = (i - 1) as f32 * stride * inline_sign;
            let curr_origin = i as f32 * stride * inline_sign;
            let left_origin = prev_origin.min(curr_origin);
            let right_origin = prev_origin.max(curr_origin);
            let gap_start = left_origin + column_width;
            let gap = (right_origin - gap_start).max(0.0);
            let x = gap_start + (gap - rule_width).max(0.0) * 0.5;
            let y = if fragmented_context {
              set_offset(set)
            } else {
              0.0
            };
            let bounds = Rect::from_xywh(x, y, rule_width, rule_extent);
            let mut rule_fragment =
              FragmentNode::new_block_styled(bounds, Vec::new(), rule_style.clone());
            Self::set_logical_from_bounds(&mut rule_fragment);
            fragments.push(rule_fragment);
          }
        }
      }
    }

    // In paged contexts, `break-before/after: always` forces a break in the *immediately
    // containing* fragmentation context (CSS Break 4). Inside multicol pagination that context is
    // the column fragmentation, and the break has already been applied when we computed the
    // column boundaries. Leave `always` on descendant fragments and the outer paginator can
    // misinterpret it as a forced *page* break, splitting the page mid column-set.
    if fragmentainer_hint.is_some() {
      fn strip_always_breaks(fragment: &mut FragmentNode) {
        if fragment
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.break_before, BreakBetween::Always))
        {
          if let Some(style) = fragment.style.as_mut() {
            Arc::make_mut(style).break_before = BreakBetween::Auto;
          }
        }
        if fragment
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.break_after, BreakBetween::Always))
        {
          if let Some(style) = fragment.style.as_mut() {
            Arc::make_mut(style).break_after = BreakBetween::Auto;
          }
        }
        for child in fragment.children_mut() {
          strip_always_breaks(child);
        }
      }

      for fragment in &mut fragments {
        strip_always_breaks(fragment);
      }
    }

    Ok((
      fragments,
      segment_height,
      positioned_children,
      flow_content_height,
    ))
  }

  fn layout_multicolumn(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    available_inline: f32,
    paint_viewport: Rect,
  ) -> Result<
    (
      Vec<FragmentNode>,
      f32,
      Vec<PositionedCandidate>,
      Option<FragmentationInfo>,
    ),
    LayoutError,
  > {
    let inline_is_horizontal = inline_axis_is_horizontal(parent.style.writing_mode);
    let available_block = if inline_is_horizontal {
      constraints.available_height
    } else {
      constraints.available_width
    };
    let (column_count, column_width, column_gap) =
      self.compute_column_geometry(&parent.style, available_inline);
    if column_count <= 1 {
      let (frags, height, positioned) = self.layout_children(
        parent,
        constraints,
        nearest_positioned_cb,
        nearest_fixed_cb,
        paint_viewport,
      )?;
      let info = FragmentationInfo {
        column_count,
        column_gap,
        column_width,
        flow_height: height,
      };
      return Ok((frags, height, positioned, Some(info)));
    }

    let fragmentainer_hint = crate::layout::formatting_context::fragmentainer_block_size_hint()
      .filter(|size| size.is_finite() && *size > 0.0);
    let paged_multicol = fragmentainer_hint.is_some();

    let base_offset = crate::layout::formatting_context::fragmentainer_block_offset_hint();
    let base_offset = if base_offset.is_finite() {
      base_offset
    } else {
      0.0
    };
    let mut fragments = Vec::new();
    let mut positioned_children = Vec::new();
    let mut physical_offset = 0.0;
    let mut logical_offset = 0.0;
    let mut idx = 0;
    let mut deadline_counter = 0usize;

    while idx < parent.children.len() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 16, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      let next_span = parent.children[idx..]
        .iter()
        .position(|c| c.style.column_span == ColumnSpan::All)
        .map(|p| p + idx);
      let end = next_span.unwrap_or(parent.children.len());

      if end > idx {
        let segment_viewport = paint_viewport.translate(Point::new(0.0, -physical_offset));
        let segment_column_fill = if next_span.is_some() {
          match parent.style.column_fill {
            ColumnFill::Auto => ColumnFill::Balance,
            fill => fill,
          }
        } else {
          parent.style.column_fill
        };
        let _segment_offset_guard = paged_multicol.then(|| {
          crate::layout::formatting_context::set_fragmentainer_block_offset_hint(
            base_offset + logical_offset,
          )
        });
        let (mut seg_fragments, seg_height, mut seg_positioned, seg_flow_height) = self
          .layout_column_segment(
            parent,
            &parent.children[idx..end],
            column_count,
            column_width,
            column_gap,
            available_block,
            segment_column_fill,
            nearest_positioned_cb,
            nearest_fixed_cb,
            segment_viewport,
          )?;
        for frag in &mut seg_fragments {
          Self::translate_with_logical(frag, 0.0, physical_offset, logical_offset);
        }
        for positioned in &mut seg_positioned {
          if let Some(pos) = positioned.static_position {
            positioned.static_position = Some(Point::new(pos.x, pos.y + physical_offset));
          }
        }
        fragments.extend(seg_fragments);
        positioned_children.extend(seg_positioned);
        physical_offset += seg_height;
        // In a paged context (`fragmentainer_hint` is set), column sets are stacked in physical
        // fragmentainer coordinates (page-sized blocks). The fragments produced by
        // `layout_column_segment` already have logical coordinates aligned with those physical page
        // offsets, so the flow cursor must advance by the *full* segment height. Advancing by the
        // uncolumnized flow height would desynchronize logical vs physical coordinates, confusing the
        // outer paginator (extra blank pages / wrong placement for following spanners).
        if paged_multicol {
          logical_offset += seg_height;
        } else {
          logical_offset += seg_flow_height;
        }
      }

      if let Some(span_idx) = next_span {
        let span_parent =
          Self::clone_with_children(parent, vec![parent.children[span_idx].clone()]);
        let span_constraints = if inline_is_horizontal {
          LayoutConstraints::new(AvailableSpace::Definite(available_inline), available_block)
        } else {
          LayoutConstraints::new(available_block, AvailableSpace::Definite(available_inline))
        }
        .with_inline_percentage_base(Some(available_inline))
        .with_block_percentage_base(available_block.to_option());
        let _span_offset_guard = crate::layout::formatting_context::fragmentainer_block_size_hint()
          .filter(|size| size.is_finite() && *size > 0.0)
          .map(|_| {
            crate::layout::formatting_context::set_fragmentainer_block_offset_hint(
              base_offset + logical_offset,
            )
          });
        let (mut span_fragments, span_height, mut span_positioned) = self.layout_children(
          &span_parent,
          &span_constraints,
          nearest_positioned_cb,
          nearest_fixed_cb,
          paint_viewport.translate(Point::new(0.0, -physical_offset)),
        )?;
        for frag in &mut span_fragments {
          Self::set_logical_from_bounds(frag);
          Self::translate_with_logical(frag, 0.0, physical_offset, logical_offset);
        }
        for positioned in &mut span_positioned {
          if let Some(pos) = positioned.static_position {
            positioned.static_position = Some(Point::new(pos.x, pos.y + physical_offset));
          }
        }
        fragments.extend(span_fragments);
        positioned_children.extend(span_positioned);
        physical_offset += span_height;
        logical_offset += span_height;
        idx = span_idx + 1;
      } else {
        break;
      }
    }

    let info = FragmentationInfo {
      column_count,
      column_gap,
      column_width,
      flow_height: logical_offset,
    };

    Ok((fragments, physical_offset, positioned_children, Some(info)))
  }
}

impl Default for BlockFormattingContext {
  fn default() -> Self {
    Self::new()
  }
}

impl std::fmt::Debug for BlockFormattingContext {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("BlockFormattingContext")
  }
}

fn has_percentage_sizing_hint(style: &ComputedStyle) -> bool {
  style.width.as_ref().is_some_and(|len| len.has_percentage())
    || style
      .height
      .as_ref()
      .is_some_and(|len| len.has_percentage())
    || style
      .min_width
      .as_ref()
      .is_some_and(|len| len.has_percentage())
    || style
      .max_width
      .as_ref()
      .is_some_and(|len| len.has_percentage())
    || style
      .min_height
      .as_ref()
      .is_some_and(|len| len.has_percentage())
    || style
      .max_height
      .as_ref()
      .is_some_and(|len| len.has_percentage())
    || style
      .width_keyword
      .is_some_and(|keyword| keyword.has_percentage())
    || style
      .height_keyword
      .is_some_and(|keyword| keyword.has_percentage())
    || style
      .min_width_keyword
      .is_some_and(|keyword| keyword.has_percentage())
    || style
      .max_width_keyword
      .is_some_and(|keyword| keyword.has_percentage())
    || style
      .min_height_keyword
      .is_some_and(|keyword| keyword.has_percentage())
    || style
      .max_height_keyword
      .is_some_and(|keyword| keyword.has_percentage())
}

impl FormattingContext for BlockFormattingContext {
  #[allow(clippy::cognitive_complexity)]
  fn layout(
    &self,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
  ) -> Result<FragmentNode, LayoutError> {
    if crate::layout::auto_scrollbars::should_bypass(box_node) {
      let _profile = layout_timer(LayoutKind::Block);
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      let style_override = crate::layout::style_override::style_override_for(box_node.id);
      if let Some(cached) = layout_cache_lookup(
        box_node,
        FormattingContextType::Block,
        constraints,
        self.factory.viewport_scroll(),
        self.viewport_size,
        self.nearest_positioned_cb,
        self.nearest_fixed_cb,
      ) {
        return Ok(cached);
      }
      let style = style_override.as_ref().unwrap_or(&box_node.style);
      let base_paint_viewport =
        paint_viewport_for(style.writing_mode, style.direction, self.viewport_size);
      let toggles = crate::debug::runtime::runtime_toggles();
      let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);
      let root_font_metrics = self.factory.root_font_metrics();
      let _inline_positive = inline_axis_positive(style.writing_mode, style.direction);
      let _block_positive = block_axis_positive(style.writing_mode);
      let inline_space = if inline_is_horizontal {
        constraints.available_width
      } else {
        constraints.available_height
      };
      let inline_viewport = if inline_is_horizontal {
        self.viewport_size.width
      } else {
        self.viewport_size.height
      };
      let log_skinny = toggles.truthy("FASTR_LOG_SKINNY_FLEX");
      let inline_percentage_base = match inline_space {
        AvailableSpace::Definite(_) => {
          let base = if inline_is_horizontal {
            constraints
              .inline_percentage_base
              .or_else(|| constraints.width())
          } else {
            constraints.height()
          };
          let viewport_inline = if inline_is_horizontal {
            self.viewport_size.width
          } else {
            self.viewport_size.height
          };
          base.unwrap_or(viewport_inline)
        }
        AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite => {
          constraints.inline_percentage_base.unwrap_or(0.0)
        }
      };
      // When the containing block inline size is intrinsic/indefinite (min-/max-content probes),
      // percentage widths behave as `auto` per CSS sizing. Strip percentage width/min/max hints
      // so intrinsic sizing does not resolve them against an unrelated base (e.g., viewport).
      let use_percent_as_auto = matches!(
        inline_space,
        AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite
      );
      let style_for_width_owned: Option<Arc<ComputedStyle>> = use_percent_as_auto.then(|| {
        let mut owned = style.clone();
        {
          let s = Arc::make_mut(&mut owned);
          if matches!(s.width, Some(len) if len.unit.is_percentage())
            || s
              .width_keyword
              .is_some_and(|keyword| keyword.has_percentage())
          {
            s.width = None;
            s.width_keyword = None;
          }
          if matches!(s.min_width, Some(len) if len.unit.is_percentage())
            || s
              .min_width_keyword
              .is_some_and(|keyword| keyword.has_percentage())
          {
            s.min_width = None;
            s.min_width_keyword = None;
          }
          if matches!(s.max_width, Some(len) if len.unit.is_percentage())
            || s
              .max_width_keyword
              .is_some_and(|keyword| keyword.has_percentage())
          {
            s.max_width = None;
            s.max_width_keyword = None;
          }
        }
        owned
      });
      let style_for_width: &ComputedStyle = style_for_width_owned
        .as_deref()
        .unwrap_or_else(|| style.as_ref());

      // When available width is indefinite/max-content, try to derive a reasonable containing
      // width from the element's own sizing hints (max-width/width/min-width) before falling
      // back to the viewport. The base for percentages must be the parent’s containing width
      // (the constraint) rather than the viewport; otherwise centered/narrow wrappers (e.g.,
      // 400px max-width zones) inflate to 1200px during intrinsic probes.
      let preferred_containing_width = |percentage_base: f32| {
        let resolve = |len: &Length| {
          resolve_length_for_width(
            *len,
            percentage_base,
            style,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          )
        };
        style
          .max_width
          .as_ref()
          .map(resolve)
          .or_else(|| style.width.as_ref().map(resolve))
          .or_else(|| style.min_width.as_ref().map(resolve))
      };

      // Replaced elements laid out as standalone formatting contexts: compute their used size
      // directly instead of running the block width algorithm (which would treat the specified
      // width as the used content width without honoring max-width).
      if let BoxType::Replaced(replaced_box) = &box_node.box_type {
        let mut containing_width = inline_percentage_base;
        if containing_width <= 1.0
          && constraints.used_border_box_width.is_none()
          && !self.suppress_near_zero_width_viewport_fallback
        {
          let width_is_absolute = style
            .width
            .as_ref()
            .map(|l| l.unit.is_absolute())
            .unwrap_or(false);
          if !width_is_absolute {
            containing_width = self.viewport_size.width;
          }
        }
        let containing_height = if inline_is_horizontal {
          // Prefer the explicit block percentage base so replaced elements with percentage heights
          // can resolve against a definite containing-block size even when the block axis available
          // space is indefinite (e.g. `aspect-ratio` auto heights).
          constraints
            .block_percentage_base
            .or_else(|| constraints.height())
        } else {
          constraints
            .block_percentage_base
            .or_else(|| constraints.width())
        };
        let percentage_base = Some(crate::geometry::Size::new(
          containing_width,
          containing_height.unwrap_or(f32::NAN),
        ));
        // `compute_replaced_size` returns the used content-box size and already accounts for min/max
        // constraints while interpreting `box-sizing`. Avoid reapplying min/max clamps here.
        //
        // When we're performing intrinsic/indefinite probes, percentage-based width constraints behave
        // as `auto` (CSS Sizing). Use the `style_for_width` variant that strips percentage
        // width/min/max hints so `max-width: 100%` doesn't incorrectly clamp replaced elements to 0
        // when the containing block inline size is unknown (e.g. intrinsic block-size probes for
        // absolutely positioned replaced elements).
        let used_size = compute_replaced_size(
          style_for_width,
          replaced_box,
          percentage_base,
          self.viewport_size,
        );
        {
          let toggles = crate::debug::runtime::runtime_toggles();
          if toggles.truthy("FASTR_LOG_REPLACED_SIZES") {
            let matches_filter = toggles
              .usize_list("FASTR_LOG_REPLACED_SIZE_IDS")
              .map(|ids| ids.contains(&box_node.id))
              .unwrap_or(true);
            if matches_filter {
              let selector = box_node
                .debug_info
                .as_ref()
                .map(|d| d.to_selector())
                .unwrap_or_else(|| "<anon>".to_string());
              eprintln!(
              "[replaced-size] id={} selector={} replaced={:?} intrinsic_size={:?} intrinsic_ratio={:?} no_intrinsic_ratio={} style_width={:?} style_height={:?} style_min_w={:?} style_max_w={:?} sizing_width={:?} sizing_height={:?} sizing_min_w={:?} sizing_max_w={:?} min_h={:?} max_h={:?} width_kw={:?} height_kw={:?} aspect_ratio={:?} percentage_base={:?} used_size=({:.2},{:.2})",
              box_node.id,
              selector,
              replaced_box.replaced_type,
              replaced_box.intrinsic_size,
              replaced_box.aspect_ratio,
              replaced_box.no_intrinsic_ratio,
              style.width,
              style.height,
              style.min_width,
              style.max_width,
              style_for_width.width,
              style_for_width.height,
              style_for_width.min_width,
              style_for_width.max_width,
              style.min_height,
              style.max_height,
              style.width_keyword,
              style.height_keyword,
              style.aspect_ratio,
              percentage_base,
              used_size.width,
              used_size.height
            );
            }
          }
        }
        if log_skinny && containing_width <= 1.0 {
          let selector = box_node
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          eprintln!(
                    "[skinny-block-constraint] id={} selector={} replaced containing_w={:.2} used_w={:.2} min_w={:?} max_w={:?}",
                    box_node.id, selector, containing_width, used_size.width, style.min_width, style.max_width
                );
        }
        // `compute_replaced_size` returns a content-box size. Fragment bounds are border-box sized,
        // so include padding and border edges here; absolute positioning and container query sizing
        // both expect border-box fragment geometry.
        let root_font_metrics = self.factory.root_font_metrics();
        let inline_edges = if inline_is_horizontal {
          horizontal_padding_and_borders(
            style,
            containing_width,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          )
        } else {
          vertical_padding_and_borders(
            style,
            containing_width,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          )
        };
        let block_edges = if inline_is_horizontal {
          vertical_padding_and_borders(
            style,
            containing_width,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          )
        } else {
          horizontal_padding_and_borders(
            style,
            containing_width,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          )
        };

        let used_inline = if inline_is_horizontal {
          used_size.width
        } else {
          used_size.height
        };
        let used_block = if inline_is_horizontal {
          used_size.height
        } else {
          used_size.width
        };
        let bounds = Rect::new(
          Point::new(0.0, 0.0),
          Size::new(
            (used_inline + inline_edges).max(0.0),
            (used_block + block_edges).max(0.0),
          ),
        );
        let fragment = FragmentNode::new_with_style(
          bounds,
          crate::tree::fragment_tree::FragmentContent::Replaced {
            replaced_type: replaced_box.replaced_type.clone(),
            box_id: Some(box_node.id),
          },
          vec![],
          box_node.style.clone(),
        );
        let converted = convert_fragment_axes(
          fragment,
          bounds.width(),
          bounds.height(),
          style.writing_mode,
          style.direction,
        );
        return Ok(converted);
      }

      let intrinsic_width_mode = matches!(
        inline_space,
        AvailableSpace::MaxContent | AvailableSpace::MinContent | AvailableSpace::Indefinite
      );
      let mut containing_width = match inline_space {
        AvailableSpace::Definite(w) => w,
        // In-flow blocks use the containing block’s inline size; shrink-to-fit contexts should
        // feed a definite width in constraints. When the available width is indefinite/max/min
        // content, prefer the element’s own sizing hints (resolved against the parent
        // containing width when known) before falling back to the viewport.
        AvailableSpace::MaxContent | AvailableSpace::MinContent | AvailableSpace::Indefinite => {
          preferred_containing_width(inline_percentage_base).unwrap_or(inline_percentage_base)
        }
      };
      let used_border_box_inline = if inline_is_horizontal {
        constraints.used_border_box_width
      } else {
        constraints.used_border_box_height
      };
      if containing_width <= 1.0
        && !intrinsic_width_mode
        && used_border_box_inline.is_none()
        && !self.suppress_near_zero_width_viewport_fallback
      {
        let width_is_absolute = style
          .width
          .as_ref()
          .map(|l| l.unit.is_absolute())
          .unwrap_or(false);
        if !width_is_absolute {
          containing_width = inline_viewport;
        }
      }
      if toggles.truthy("FASTR_LOG_SMALL_BLOCK") && containing_width < 150.0 {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anonymous>".to_string());
        eprintln!(
                "[block-small] id={} selector={} containing_w={:.1} avail_w={:?} width_decl={:?} min_w={:?} max_w={:?}",
                box_node.id,
                selector,
                containing_width,
                constraints.available_width,
                style.width,
                style.min_width,
                style.max_width,
            );
      }
      let containing_height = if inline_is_horizontal {
        constraints.height()
      } else {
        constraints.width()
      };
      // For flex items, prefer the max-content contribution instead of filling the available
      // width when width is auto (CSS Flexbox §4.5: auto main size uses the max-content size).
      // This avoids the block constraint equation forcing auto margins/auto widths to span the
      // containing block during flex item hypothetical sizing.
      let flex_pref_border = if self.flex_item_mode
        && style_for_width.width.is_none()
        && style_for_width.width_keyword.is_none()
      {
        let intrinsic_mode = match constraints.available_width {
          AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
          _ => IntrinsicSizingMode::MaxContent,
        };
        Some(self.compute_intrinsic_inline_size(box_node, intrinsic_mode)?)
      } else {
        None
      };

      let inline_sides = inline_axis_sides(style);
      let inline_positive = inline_axis_positive(style.writing_mode, style.direction);

      let mut computed_width = compute_block_width(
        style_for_width,
        containing_width,
        self.viewport_size,
        root_font_metrics,
        inline_sides,
        inline_positive,
        &self.font_context,
      );
      let width_auto = style_for_width.width.is_none() && style_for_width.width_keyword.is_none();
      let inline_edges = computed_width.border_left
        + computed_width.padding_left
        + computed_width.padding_right
        + computed_width.border_right;
      let available_inline_border_box = (containing_width
        - resolve_margin_side(
          style,
          inline_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        )
        - resolve_margin_side(
          style,
          inline_sides.1,
          containing_width,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        ))
      .max(0.0);

      let available_content_for_fit = (available_inline_border_box - inline_edges).max(0.0);
      let mut intrinsic_content_sizes = None;
      if style_for_width.width.is_none() && style_for_width.width_keyword.is_some() {
        let keyword = style_for_width.width_keyword.unwrap();
        let fc_type = box_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let (min_content, max_content) = self.intrinsic_inline_content_sizes_for_sizing_keywords(
          box_node,
          fc_type,
          &self.factory,
        )?;
        intrinsic_content_sizes = Some((min_content, max_content));
        let keyword_content = self.resolve_intrinsic_size_keyword_to_content_width(
          keyword,
          min_content,
          max_content,
          available_content_for_fit,
          containing_width,
          style_for_width,
          inline_edges,
        );
        if self.flex_item_mode {
          computed_width.content_width = keyword_content;
        } else {
          let specified_width = match style_for_width.box_sizing {
            crate::style::types::BoxSizing::ContentBox => keyword_content,
            crate::style::types::BoxSizing::BorderBox => keyword_content + inline_edges,
          };
          let mut width_style = style_for_width_owned
            .as_ref()
            .cloned()
            .unwrap_or_else(|| style.clone());
          {
            let s = Arc::make_mut(&mut width_style);
            s.width = Some(Length::px(specified_width));
            s.width_keyword = None;
          }
          computed_width = compute_block_width(
            &width_style,
            containing_width,
            self.viewport_size,
            root_font_metrics,
            inline_sides,
            inline_positive,
            &self.font_context,
          );
        }
      }

      if style.shrink_to_fit_inline_size && width_auto {
        let log_shrink = toggles.truthy("FASTR_LOG_SHRINK_TO_FIT");

        let fc_type = box_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let (preferred_min_content, preferred_content) = if fc_type == FormattingContextType::Block
        {
          self.compute_intrinsic_inline_sizes(box_node)?
        } else {
          let fc = self.factory.get(fc_type);
          fc.compute_intrinsic_inline_sizes(box_node)?
        };

        let edges_base0 = inline_axis_padding_and_borders(
          style,
          0.0,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        let preferred_min =
          rebase_intrinsic_border_box_size(preferred_min_content, edges_base0, inline_edges);
        let preferred =
          rebase_intrinsic_border_box_size(preferred_content, edges_base0, inline_edges);
        let shrink_border_box = preferred.min(available_inline_border_box.max(preferred_min));
        let shrink_content = (shrink_border_box - inline_edges).max(0.0);
        if log_shrink {
          let selector = box_node
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          eprintln!(
                    "[shrink-to-fit] id={} selector={} preferred_min={:.1} preferred={:.1} available={:.1} content={:.1} edges={:.1}",
                    box_node.id, selector, preferred_min, preferred, available_inline_border_box, shrink_content, inline_edges
                );
        }
        let (margin_left, margin_right) = recompute_margins_for_width(
          style,
          containing_width,
          shrink_content,
          computed_width.border_left,
          computed_width.padding_left,
          computed_width.padding_right,
          computed_width.border_right,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        computed_width.content_width = shrink_content;
        computed_width.margin_left = margin_left;
        computed_width.margin_right = margin_right;
      }
      // When asked for intrinsic max-/min-content sizes, override the constraint equation with
      // the corresponding intrinsic inline size so flex/inline shrink-to-fit measurements don't
      // default to the full containing block width.
      if matches!(
        constraints.available_width,
        AvailableSpace::MinContent | AvailableSpace::MaxContent
      ) {
        let intrinsic_mode = match constraints.available_width {
          AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
          _ => IntrinsicSizingMode::MaxContent,
        };
        match self.compute_intrinsic_inline_size(box_node, intrinsic_mode) {
          Ok(intrinsic_border) => {
            let edges_base0 = inline_axis_padding_and_borders(
              style,
              0.0,
              self.viewport_size,
              &self.font_context,
              root_font_metrics,
            );
            let intrinsic_border =
              rebase_intrinsic_border_box_size(intrinsic_border, edges_base0, inline_edges);
            let intrinsic_content = (intrinsic_border - inline_edges).max(0.0);
            computed_width.content_width = intrinsic_content;
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }
      let horizontal_edges = computed_width.border_left
        + computed_width.padding_left
        + computed_width.padding_right
        + computed_width.border_right;
      if let Some(pref_border) = flex_pref_border {
        let edges_base0 = inline_axis_padding_and_borders(
          style,
          0.0,
          self.viewport_size,
          &self.font_context,
          root_font_metrics,
        );
        let pref_border =
          rebase_intrinsic_border_box_size(pref_border, edges_base0, horizontal_edges);
        let pref_content = (pref_border - horizontal_edges).max(0.0);
        computed_width.content_width = pref_content;
      }
      let min_width = if let Some(keyword) = style_for_width.min_width_keyword {
        if intrinsic_content_sizes.is_none() {
          let fc_type = box_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          intrinsic_content_sizes = Some(self.intrinsic_inline_content_sizes_for_sizing_keywords(
            box_node,
            fc_type,
            &self.factory,
          )?);
        }
        let (min_content, max_content) = intrinsic_content_sizes.unwrap();
        self.resolve_intrinsic_size_keyword_to_content_width(
          keyword,
          min_content,
          max_content,
          available_content_for_fit,
          containing_width,
          style_for_width,
          horizontal_edges,
        )
      } else {
        style_for_width
          .min_width
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              containing_width,
              style_for_width,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
          .unwrap_or(0.0)
      };
      let reserved_vertical_gutter =
        if style.box_sizing == crate::style::types::BoxSizing::ContentBox {
          let reservation = crate::layout::utils::scrollbar_reservation_for_style(style);
          (reservation.left + reservation.right).max(0.0)
        } else {
          0.0
        };
      let min_width = if reserved_vertical_gutter > 0.0
        && style_for_width.min_width_keyword.is_none()
        && style_for_width.min_width.is_some()
      {
        (min_width - reserved_vertical_gutter).max(0.0)
      } else {
        min_width
      };

      let max_width = if let Some(keyword) = style_for_width.max_width_keyword {
        if intrinsic_content_sizes.is_none() {
          let fc_type = box_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          intrinsic_content_sizes = Some(self.intrinsic_inline_content_sizes_for_sizing_keywords(
            box_node,
            fc_type,
            &self.factory,
          )?);
        }
        let (min_content, max_content) = intrinsic_content_sizes.unwrap();
        self.resolve_intrinsic_size_keyword_to_content_width(
          keyword,
          min_content,
          max_content,
          available_content_for_fit,
          containing_width,
          style_for_width,
          horizontal_edges,
        )
      } else {
        style_for_width
          .max_width
          .as_ref()
          .and_then(|l| {
            let percentage_base = containing_width.is_finite().then_some(containing_width);
            resolve_length_with_percentage_metrics(
              *l,
              percentage_base,
              self.viewport_size,
              style_for_width.font_size,
              style_for_width.root_font_size,
              Some(style_for_width),
              Some(&self.font_context),
            )
          })
          .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
          .unwrap_or(f32::INFINITY)
      };
      let max_width = if reserved_vertical_gutter > 0.0
        && style_for_width.max_width_keyword.is_none()
        && style_for_width.max_width.is_some()
        && max_width.is_finite()
      {
        (max_width - reserved_vertical_gutter).max(0.0)
      } else {
        max_width
      };

      // CSS 2.1 §10.4: if the computed min-width exceeds max-width, max-width is set to min-width.
      let max_width = if max_width.is_finite() && max_width < min_width {
        min_width
      } else {
        max_width
      };

      let clamped_content_width =
        crate::layout::utils::clamp_with_order(computed_width.content_width, min_width, max_width);
      let log_wide_block = toggles.truthy("FASTR_LOG_WIDE_FLEX");
      if log_wide_block && computed_width.content_width > self.viewport_size.width + 0.5 {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anonymous>".to_string());
        eprintln!(
                "[block-wide] box_id={:?} selector={} display={:?} containing={:.1} content_w={:.1} total_w={:.1} width={:?} min_w={:?} max_w={:?} viewport_w={:.1} avail_w={:?} margins=({:.1},{:.1})",
                box_node.id,
                selector,
                style.display,
                containing_width,
                computed_width.content_width,
                computed_width.total_width(),
                style.width,
                style.min_width,
                style.max_width,
                self.viewport_size.width,
                constraints.available_width,
                computed_width.margin_left,
                computed_width.margin_right,
            );
      }
      if self.flex_item_mode {
        // Flex items use their specified margins when computing hypothetical sizes; auto
        // margins resolve to 0 instead of being rebalanced to satisfy the block constraint
        // equation. Keep the clamped content width but avoid recomputing margins.
        computed_width.content_width = clamped_content_width;
        let resolved_ml = style
          .margin_left
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              containing_width,
              style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .unwrap_or(0.0);
        let resolved_mr = style
          .margin_right
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              containing_width,
              style,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            )
          })
          .unwrap_or(0.0);
        computed_width.margin_left = resolved_ml;
        computed_width.margin_right = resolved_mr;
      } else {
        if clamped_content_width != computed_width.content_width {
          let (margin_left, margin_right) = recompute_margins_for_width(
            style,
            containing_width,
            clamped_content_width,
            computed_width.border_left,
            computed_width.padding_left,
            computed_width.padding_right,
            computed_width.border_right,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          );
          computed_width.content_width = clamped_content_width;
          computed_width.margin_left = margin_left;
          computed_width.margin_right = margin_right;
        }
      }

      // The parent formatting context may have already resolved a definite used border-box inline
      // size for this box (e.g. flex/grid items after Taffy sizing). In that case the final used
      // size must be treated as authoritative, even when the box has an authored inline-size
      // (notably percentages): the used size can differ after flexing, and re-resolving percentage
      // widths against the item’s own used size can cause descendants to be laid out at the wrong
      // width.
      let used_border_box_inline = if inline_is_horizontal {
        constraints.used_border_box_width
      } else {
        constraints.used_border_box_height
      };
      if let Some(used_border_box) = used_border_box_inline {
        let used_content = (used_border_box - horizontal_edges).max(0.0);
        if self.flex_item_mode {
          computed_width.content_width = used_content;
        } else {
          let (margin_left, margin_right) = recompute_margins_for_width(
            style,
            containing_width,
            used_content,
            computed_width.border_left,
            computed_width.padding_left,
            computed_width.padding_right,
            computed_width.border_right,
            self.viewport_size,
            &self.font_context,
            root_font_metrics,
          );
          computed_width.content_width = used_content;
          computed_width.margin_left = margin_left;
          computed_width.margin_right = margin_right;
        }
      }

      let border_top = resolve_length_for_width(
        style.used_border_top_width(),
        containing_width,
        style,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let border_bottom = resolve_length_for_width(
        style.used_border_bottom_width(),
        containing_width,
        style,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let mut padding_top = resolve_length_for_width(
        style.padding_top,
        containing_width,
        style,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      let mut padding_bottom = resolve_length_for_width(
        style.padding_bottom,
        containing_width,
        style,
        &self.font_context,
        self.viewport_size,
        root_font_metrics,
      );
      // Reserve space for a horizontal scrollbar when requested by `scrollbar-gutter: stable`.
      let reserve_horizontal_gutter = style.scrollbar_gutter.stable
        && matches!(
          style.overflow_x,
          Overflow::Hidden | Overflow::Auto | Overflow::Scroll
        );
      let mut reserved_horizontal_gutter = 0.0;
      if reserve_horizontal_gutter {
        let gutter = resolve_scrollbar_width(style);
        if gutter > 0.0 {
          if style.scrollbar_gutter.both_edges {
            padding_top += gutter;
            reserved_horizontal_gutter += gutter;
          }
          padding_bottom += gutter;
          reserved_horizontal_gutter += gutter;
        }
      }
      let vertical_edges = border_top + padding_top + padding_bottom + border_bottom;

      // Block-axis sizing uses physical `width`/`height` depending on writing mode.
      let block_length = if inline_is_horizontal {
        style.height
      } else {
        style.width
      };
      let block_keyword = if inline_is_horizontal {
        style.height_keyword
      } else {
        style.width_keyword
      };
      // CSS Sizing L3: min-/max-content block-size is equivalent to max-content block-size for block
      // containers / inline boxes, so `block-size: min-content|max-content` behaves like `auto`.
      let block_keyword = match block_keyword {
        Some(crate::style::types::IntrinsicSizeKeyword::MinContent)
        | Some(crate::style::types::IntrinsicSizeKeyword::MaxContent) => None,
        other => other,
      };
      let min_block_keyword = if inline_is_horizontal {
        style.min_height_keyword
      } else {
        style.min_width_keyword
      };
      let max_block_keyword = if inline_is_horizontal {
        style.max_height_keyword
      } else {
        style.max_width_keyword
      };
      let block_keyword_is_content_based = inline_is_horizontal
        && matches!(
          block_keyword,
          Some(IntrinsicSizeKeyword::MinContent | IntrinsicSizeKeyword::MaxContent)
        );
      let margin_top = style
        .margin_top
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          )
        })
        .unwrap_or(0.0);
      let margin_bottom = style
        .margin_bottom
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          )
        })
        .unwrap_or(0.0);
      let available_block_border_box = containing_height
        .map(|h| (h - margin_top - margin_bottom).max(0.0))
        .unwrap_or(f32::INFINITY);

      let intrinsic_block_sizes = if (block_keyword.is_some() && !block_keyword_is_content_based)
        || min_block_keyword.is_some()
        || max_block_keyword.is_some()
      {
        let fc_type = box_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let (min_base0, max_base0) = if fc_type == FormattingContextType::Block {
          compute_intrinsic_block_sizes_without_block_size_constraints(self, box_node)?
        } else {
          let fc = self.factory.get(fc_type);
          compute_intrinsic_block_sizes_without_block_size_constraints(fc.as_ref(), box_node)?
        };

        let border_top_base0 = resolve_length_for_width(
          style.used_border_top_width(),
          0.0,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let border_bottom_base0 = resolve_length_for_width(
          style.used_border_bottom_width(),
          0.0,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let mut padding_top_base0 = resolve_length_for_width(
          style.padding_top,
          0.0,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        let mut padding_bottom_base0 = resolve_length_for_width(
          style.padding_bottom,
          0.0,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        if reserve_horizontal_gutter {
          let gutter = resolve_scrollbar_width(style);
          if style.scrollbar_gutter.both_edges {
            padding_top_base0 += gutter;
          }
          padding_bottom_base0 += gutter;
        }
        let vertical_edges_base0 =
          border_top_base0 + padding_top_base0 + padding_bottom_base0 + border_bottom_base0;
        Some((
          rebase_intrinsic_border_box_size(min_base0, vertical_edges_base0, vertical_edges),
          rebase_intrinsic_border_box_size(max_base0, vertical_edges_base0, vertical_edges),
        ))
      } else {
        None
      };

      let mut resolved_height = block_length
        .and_then(|h| {
          resolve_length_with_percentage_metrics_and_root_font_metrics(
            h,
            containing_height,
            self.viewport_size,
            style.font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
            self.factory.root_font_metrics(),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing));
      // Like `layout_block_child`, track whether the block-size should establish a definite
      // percentage base for in-flow descendants (CSS2.1 §10.5).
      let mut block_size_definite_for_percentages = resolved_height.is_some();

      // When flowing in a vertical/sideways writing mode, the block axis maps to the physical
      // x-axis. `width: auto` should therefore stretch to fill the available block size of the
      // containing block (mirroring the auto-width behavior for block boxes in horizontal writing
      // modes) instead of collapsing to the content size.
      //
      // Note: This is handled for in-flow children in `layout_block_child`, but formatting context
      // roots (including the document root `<html>`) enter through `BlockFormattingContext::layout`
      // directly and therefore need the same safeguard here.
      let height_auto =
        block_length.is_none() && (block_keyword.is_none() || block_keyword_is_content_based);
      if resolved_height.is_none()
        && height_auto
        && block_axis_is_horizontal(style.writing_mode)
        && available_block_border_box.is_finite()
      {
        resolved_height = Some((available_block_border_box - vertical_edges).max(0.0));
        block_size_definite_for_percentages = true;
      }
      if reserved_horizontal_gutter > 0.0
        && block_length.is_some()
        && style.box_sizing == crate::style::types::BoxSizing::ContentBox
      {
        resolved_height = resolved_height.map(|h| (h - reserved_horizontal_gutter).max(0.0));
      }
      if let Some(height_keyword) = block_keyword {
        if !block_keyword_is_content_based {
          let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
          let used_border_box = match height_keyword {
            crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
            crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
              if available_block_border_box.is_finite() {
                available_block_border_box
              } else {
                intrinsic_max
              }
            }
            crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
              let basis_border = match limit {
                Some(limit) => resolve_length_with_percentage_metrics_and_root_font_metrics(
                  limit,
                  containing_height,
                  self.viewport_size,
                  style.font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                  self.factory.root_font_metrics(),
                )
                .map(|resolved| {
                  border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                })
                .unwrap_or(f32::INFINITY),
                None => available_block_border_box,
              };
              crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
            }
            crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
              use crate::style::types::BoxSizing;
              use crate::style::types::CalcSizeBasis;
              let basis_border = match calc.basis {
                CalcSizeBasis::Auto => intrinsic_max,
                CalcSizeBasis::MinContent => intrinsic_min,
                CalcSizeBasis::MaxContent => intrinsic_max,
                CalcSizeBasis::FillAvailable => {
                  if available_block_border_box.is_finite() {
                    available_block_border_box
                  } else {
                    intrinsic_max
                  }
                }
                CalcSizeBasis::FitContent { limit } => {
                  let basis_border = match limit {
                    Some(limit) => resolve_length_with_percentage_metrics(
                      limit,
                      containing_height,
                      self.viewport_size,
                      style.font_size,
                      style.root_font_size,
                      Some(style),
                      Some(&self.font_context),
                    )
                    .map(|resolved| {
                      border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                    })
                    .unwrap_or(f32::INFINITY),
                    None => available_block_border_box,
                  };
                  crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
                }
                CalcSizeBasis::Length(len) => resolve_length_with_percentage_metrics(
                  len,
                  containing_height,
                  self.viewport_size,
                  style.font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                )
                .map(|resolved| {
                  border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                })
                .unwrap_or(intrinsic_max),
              }
              .max(0.0);
              let basis_content = (basis_border - vertical_edges).max(0.0);
              let basis_specified = match style.box_sizing {
                BoxSizing::ContentBox => basis_content,
                BoxSizing::BorderBox => basis_border,
              };
              crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
                .and_then(|expr_sum| {
                  crate::css::properties::parse_length(&format!("calc({expr_sum})"))
                })
                .and_then(|expr_len| {
                  resolve_length_with_percentage_metrics(
                    expr_len,
                    containing_height,
                    self.viewport_size,
                    style.font_size,
                    style.root_font_size,
                    Some(style),
                    Some(&self.font_context),
                  )
                })
                .map(|resolved_specified| {
                  border_size_from_box_sizing(resolved_specified, vertical_edges, style.box_sizing)
                    .max(0.0)
                })
                .unwrap_or(basis_border)
            }
          };
          resolved_height = Some((used_border_box - vertical_edges).max(0.0));
          block_size_definite_for_percentages = matches!(
            height_keyword,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable
          ) && available_block_border_box.is_finite();
        }
      }
      // Like the inline axis override above, honor a parent-resolved used border-box block size even
      // when the authored height is non-auto.
      //
      // Keep track of the pre-override height so we can decide whether the forced used size should
      // establish a definite block percentage base for descendants (see `block_percentage_base`
      // below).
      let resolved_height_before_used_border_box = resolved_height;
      let used_border_box = if inline_is_horizontal {
        constraints.used_border_box_height
      } else {
        constraints.used_border_box_width
      };
      if let Some(used_border_box) = used_border_box {
        resolved_height = Some((used_border_box - vertical_edges).max(0.0));
        if constraints.used_border_box_size_forces_block_percentage_base {
          block_size_definite_for_percentages = true;
        }
      }
      let resolved_height_base = block_size_definite_for_percentages
        .then(|| {
          resolved_height
            .filter(|h| h.is_finite())
            .map(|h| h.max(0.0))
        })
        .flatten();
      let child_height_space = resolved_height_base
        .map(|h| AvailableSpace::Definite(h.max(0.0)))
        .unwrap_or(AvailableSpace::Indefinite);

      let block_percentage_base = if constraints.used_border_box_size_forces_block_percentage_base {
        resolved_height_base
      } else {
        block_size_definite_for_percentages
          .then(|| {
            resolved_height_before_used_border_box
              .filter(|h| h.is_finite())
              .map(|h| h.max(0.0))
          })
          .flatten()
      };
      let child_constraints = if inline_is_horizontal {
        LayoutConstraints::new(
          AvailableSpace::Definite(computed_width.content_width),
          child_height_space,
        )
      } else {
        LayoutConstraints::new(
          child_height_space,
          AvailableSpace::Definite(computed_width.content_width),
        )
      }
      .with_inline_percentage_base(Some(computed_width.content_width))
      .with_block_percentage_base(block_percentage_base);

      let content_origin = Point::new(
        computed_width.border_left + computed_width.padding_left,
        border_top + padding_top,
      );
      // Like `layout_block_child`, `BlockFormattingContext::layout` runs layout in the block's
      // content coordinate space (0,0 at the content edge) and translates fragments into border-box
      // coordinates before returning. Containing blocks for positioned descendants must therefore be
      // expressed in content coordinates as well: the padding edge is at (-padding_start, -padding_block_start).
      let padding_origin = Point::new(-computed_width.padding_left, -padding_top);
      let content_height_base = resolved_height.unwrap_or(0.0).max(0.0);
      let padding_size = Size::new(
        computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
        content_height_base + padding_top + padding_bottom,
      );
      let cb_block_base = resolved_height.map(|h| h.max(0.0) + padding_top + padding_bottom);
      let establishes_positioned_cb = style.establishes_abs_containing_block();
      let establishes_fixed_cb = style.establishes_fixed_containing_block();
      let should_relayout_abspos_descendants_for_cb_height = establishes_positioned_cb
        && cb_block_base.is_none()
        && has_abspos_descendant_needing_used_cb_height(box_node);
      let nearest_cb = if establishes_positioned_cb {
        ContainingBlock::with_viewport_and_bases(
          Rect::new(padding_origin, padding_size),
          self.viewport_size,
          Some(padding_size.width),
          cb_block_base,
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction)
        .with_box_id(Some(box_node.id))
      } else {
        self.nearest_positioned_cb
      };
      let nearest_fixed_cb = if establishes_fixed_cb {
        ContainingBlock::with_viewport_and_bases(
          Rect::new(padding_origin, padding_size),
          self.viewport_size,
          Some(padding_size.width),
          cb_block_base,
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction)
        .with_box_id(Some(box_node.id))
      } else {
        self.nearest_fixed_cb
      };

      let mut child_ctx = self.clone();
      child_ctx.flex_item_mode = false;
      child_ctx.independent_context_root_mode = false;
      child_ctx.independent_context_root_id =
        (self.flex_item_mode || self.independent_context_root_mode).then_some(box_node.id);
      child_ctx.nearest_positioned_cb = nearest_cb;
      child_ctx.nearest_fixed_cb = nearest_fixed_cb;
      if nearest_cb != self.nearest_positioned_cb || nearest_fixed_cb != self.nearest_fixed_cb {
        if nearest_cb != self.nearest_positioned_cb {
          child_ctx.factory = child_ctx.factory.with_positioned_cb(nearest_cb);
        }
        if nearest_fixed_cb != self.nearest_fixed_cb {
          child_ctx.factory = child_ctx.factory.with_fixed_cb(nearest_fixed_cb);
        }
        child_ctx.intrinsic_inline_fc = Arc::new(InlineFormattingContext::with_factory(
          child_ctx.factory.clone(),
        ));
      }
      let mut paint_viewport = base_paint_viewport;
      // The viewport rectangle is expressed in the formatting context's coordinate space. When this
      // block formatting context is nested inside another formatting context, the caller translates
      // the factory's `viewport_scroll` so it already accounts for the nested origin.
      let scroll = self.factory.viewport_scroll();
      if scroll.x.is_finite() && scroll.y.is_finite() {
        let (scroll_inline, scroll_block) = if inline_is_horizontal {
          (scroll.x, scroll.y)
        } else {
          (scroll.y, scroll.x)
        };
        paint_viewport = paint_viewport.translate(Point::new(scroll_inline, scroll_block));
      }
      // Layout uses the block's content box coordinate space; translate the viewport into that
      // coordinate system so culling decisions stay relative to `box_y`/`margin_left` placement.
      let viewport_content_origin = Point::new(
        computed_width.margin_left + content_origin.x,
        content_origin.y,
      );
      paint_viewport = paint_viewport.translate(Point::new(
        -viewport_content_origin.x,
        -viewport_content_origin.y,
      ));
      let use_columns = Self::is_multicol_container(style);
      let skip_contents = match style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          let activation_margin = toggles
            .f64("FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX")
            .unwrap_or(0.0)
            .max(0.0) as f32;
          let viewport = if activation_margin > 0.0 {
            paint_viewport.inflate(activation_margin)
          } else {
            paint_viewport
          };

          let box_width = computed_width.border_box_width();
          let box_width = if box_width.is_finite() {
            box_width.max(0.0)
          } else {
            0.0
          };

          let estimated_border_box_block_size = resolved_height
            .filter(|h| h.is_finite())
            .map(|h| h.max(0.0))
            .or_else(|| {
              let axis_is_width = block_axis_is_horizontal(style.writing_mode);
              let axis = if axis_is_width {
                style.contain_intrinsic_width
              } else {
                style.contain_intrinsic_height
              };
              axis
                .auto
                .then(|| {
                  remembered_size_cache_lookup(box_node).map(|size| {
                    if axis_is_width {
                      size.width
                    } else {
                      size.height
                    }
                  })
                })
                .flatten()
                .filter(|v| v.is_finite())
                .map(|v| v.max(0.0))
                .or_else(|| {
                  axis
                    .length
                    .and_then(|l| {
                      resolve_length_with_percentage_metrics_and_root_font_metrics(
                        l,
                        containing_height,
                        self.viewport_size,
                        style.font_size,
                        style.root_font_size,
                        Some(style),
                        Some(&self.font_context),
                        self.factory.root_font_metrics(),
                      )
                    })
                    .map(|v| v.max(0.0))
                })
            })
            .and_then(|content_estimate| {
              let border_box = content_estimate + vertical_edges;
              border_box.is_finite().then_some(border_box.max(0.0))
            });

          if let Some(block_size) = estimated_border_box_block_size {
            let border_box =
              Rect::from_xywh(-content_origin.x, -content_origin.y, box_width, block_size);
            !viewport.intersects(border_box)
          } else {
            false
          }
        }
        crate::style::types::ContentVisibility::Visible => false,
      };
      let layout_contents = |ctx: &BlockFormattingContext,
                             nearest_cb: &ContainingBlock,
                             nearest_fixed_cb: &ContainingBlock|
       -> Result<
        (
          Vec<FragmentNode>,
          f32,
          Vec<PositionedCandidate>,
          Option<FragmentationInfo>,
        ),
        LayoutError,
      > {
        let (mut child_fragments, mut content_height, positioned_children, column_info) =
          if skip_contents {
            (Vec::new(), 0.0, Vec::new(), None)
          } else if use_columns {
            let (frags, height, positioned, info) = ctx.layout_multicolumn(
              box_node,
              &child_constraints,
              nearest_cb,
              nearest_fixed_cb,
              computed_width.content_width,
              paint_viewport,
            )?;
            (frags, height, positioned, info)
          } else {
            let (frags, height, positioned) = ctx.layout_children(
              box_node,
              &child_constraints,
              nearest_cb,
              nearest_fixed_cb,
              paint_viewport,
            )?;
            (frags, height, positioned, None)
          };

        if skip_contents || style.containment.size {
          let axis_is_width = block_axis_is_horizontal(style.writing_mode);
          let axis = if axis_is_width {
            style.contain_intrinsic_width
          } else {
            style.contain_intrinsic_height
          };
          let remembered = axis
            .auto
            .then(|| {
              remembered_size_cache_lookup(box_node).map(|size| {
                if axis_is_width {
                  size.width
                } else {
                  size.height
                }
              })
            })
            .flatten();
          let resolved = if axis.auto {
            remembered.or_else(|| {
              axis.length.and_then(|l| {
                resolve_length_with_percentage_metrics_and_root_font_metrics(
                  l,
                  containing_height,
                  self.viewport_size,
                  style.font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                  self.factory.root_font_metrics(),
                )
              })
            })
          } else {
            axis.length.and_then(|l| {
              resolve_length_with_percentage_metrics_and_root_font_metrics(
                l,
                containing_height,
                self.viewport_size,
                style.font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
                self.factory.root_font_metrics(),
              )
            })
          };
          let mut value = resolved.unwrap_or(0.0);
          if !value.is_finite() {
            value = 0.0;
          }
          content_height = value.max(0.0);
        }

        // HTML fieldset/legend layout (approximation):
        //
        // The HTML rendering model positions the first `<legend>` element child on the fieldset
        // border and wraps the remaining children in a "fieldset content" box. To approximate this in
        // a CSS-centric block formatting context we:
        // - Keep the anonymous fieldset content box in normal flow.
        // - Pull the legend upward so it overlaps the fieldset border.
        // - Pull the content box upward so it does not reserve the legend's full block-size, while
        //   still avoiding overlap with the legend's lower half.
        if let Some((fieldset_content_id, legend_id)) = {
          let mut content_id: Option<usize> = None;
          let mut legend_id: Option<usize> = None;
          for child in &box_node.children {
            if matches!(
              &child.box_type,
              BoxType::Anonymous(anon) if anon.anonymous_type == AnonymousType::FieldsetContent
            ) {
              content_id = Some(child.id);
              continue;
            }
            if child.style.shrink_to_fit_inline_size && legend_id.is_none() {
              legend_id = Some(child.id);
            }
          }
          content_id.map(|content_id| (content_id, legend_id))
        } {
          if let Some(legend_id) = legend_id {
            let mut legend_idx: Option<usize> = None;
            let mut content_idx: Option<usize> = None;
            for (idx, frag) in child_fragments.iter().enumerate() {
              let FragmentContent::Block { box_id: Some(id) } = frag.content else {
                continue;
              };
              if id == legend_id {
                legend_idx = Some(idx);
              } else if id == fieldset_content_id {
                content_idx = Some(idx);
              }
              if legend_idx.is_some() && content_idx.is_some() {
                break;
              }
            }

            if let (Some(legend_idx), Some(content_idx)) = (legend_idx, content_idx) {
              let legend_block_size = child_fragments[legend_idx].bounds.height().max(0.0);
              let legend_half_block = legend_block_size * 0.5;

              // Desired legend placement in the fieldset's border box coordinate space: center it on
              // the block-start border edge so it straddles the border line.
              let legend_border_y = border_top - legend_half_block;
              let legend_content_y = legend_border_y - content_origin.y;

              // Place the content box at the normal content origin unless the legend's lower half would
              // overlap it.
              let content_border_y = border_top + padding_top.max(legend_half_block);
              let content_content_y = content_border_y - content_origin.y;

              let legend_delta = legend_content_y - child_fragments[legend_idx].bounds.y();
              let content_delta = content_content_y - child_fragments[content_idx].bounds.y();
              if legend_delta.is_finite() {
                child_fragments[legend_idx].translate_root_in_place(Point::new(0.0, legend_delta));
              }
              if content_delta.is_finite() {
                child_fragments[content_idx]
                  .translate_root_in_place(Point::new(0.0, content_delta));
              }

              // Recompute the flow block-size after repositioning so the legend does not artificially
              // increase the fieldset's auto height.
              let mut max_end = 0.0f32;
              for frag in &child_fragments {
                let end = frag.bounds.y() + frag.bounds.height();
                if end.is_finite() {
                  max_end = max_end.max(end.max(0.0));
                }
              }
              content_height = max_end;
            }
          }
        }

        // Child fragments are produced in the block's content coordinate space (0,0 at the content
        // box). Translate them into the fragment's local coordinate space (border box) so padding and
        // borders correctly offset in-flow content.
        if content_origin.x != 0.0 || content_origin.y != 0.0 {
          for fragment in child_fragments.iter_mut() {
            fragment.translate_root_in_place(content_origin);
          }
        }

        Ok((
          child_fragments,
          content_height,
          positioned_children,
          column_info,
        ))
      };

      let (mut child_fragments, content_height, mut positioned_children, mut column_info) =
        layout_contents(&child_ctx, &nearest_cb, &nearest_fixed_cb)?;

      let min_height = if let Some(keyword) = style.min_height_keyword {
        let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
        let min_border = match keyword {
          crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
          crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
          crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
            if available_block_border_box.is_finite() {
              available_block_border_box
            } else {
              intrinsic_max
            }
          }
          crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
            let basis_border = match limit {
              Some(limit) => resolve_length_with_percentage_metrics_and_root_font_metrics(
                limit,
                containing_height,
                self.viewport_size,
                style.font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
                self.factory.root_font_metrics(),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(f32::INFINITY),
              None => available_block_border_box,
            };
            crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
          }
          crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
            use crate::style::types::BoxSizing;
            use crate::style::types::CalcSizeBasis;
            let basis_border = match calc.basis {
              CalcSizeBasis::Auto => intrinsic_max,
              CalcSizeBasis::MinContent => intrinsic_min,
              CalcSizeBasis::MaxContent => intrinsic_max,
              CalcSizeBasis::FillAvailable => {
                if available_block_border_box.is_finite() {
                  available_block_border_box
                } else {
                  intrinsic_max
                }
              }
              CalcSizeBasis::FitContent { limit } => {
                let basis_border = match limit {
                  Some(limit) => resolve_length_with_percentage_metrics(
                    limit,
                    containing_height,
                    self.viewport_size,
                    style.font_size,
                    style.root_font_size,
                    Some(style),
                    Some(&self.font_context),
                  )
                  .map(|resolved| {
                    border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                  })
                  .unwrap_or(f32::INFINITY),
                  None => available_block_border_box,
                };
                crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
              }
              CalcSizeBasis::Length(len) => resolve_length_with_percentage_metrics(
                len,
                containing_height,
                self.viewport_size,
                style.font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(intrinsic_max),
            }
            .max(0.0);
            let basis_content = (basis_border - vertical_edges).max(0.0);
            let basis_specified = match style.box_sizing {
              BoxSizing::ContentBox => basis_content,
              BoxSizing::BorderBox => basis_border,
            };
            crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
              .and_then(|expr_sum| {
                crate::css::properties::parse_length(&format!("calc({expr_sum})"))
              })
              .and_then(|expr_len| {
                resolve_length_with_percentage_metrics(
                  expr_len,
                  containing_height,
                  self.viewport_size,
                  style.font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                )
              })
              .map(|resolved_specified| {
                border_size_from_box_sizing(resolved_specified, vertical_edges, style.box_sizing)
                  .max(0.0)
              })
              .unwrap_or(basis_border)
          }
        };
        (min_border - vertical_edges).max(0.0)
      } else {
        style
          .min_height
          .as_ref()
          .and_then(|l| {
            resolve_length_with_percentage_metrics_and_root_font_metrics(
              *l,
              containing_height,
              self.viewport_size,
              style.font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
              self.factory.root_font_metrics(),
            )
          })
          .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
          .unwrap_or(0.0)
      };
      let min_height = if reserved_horizontal_gutter > 0.0
        && style.box_sizing == crate::style::types::BoxSizing::ContentBox
        && style.min_height_keyword.is_none()
        && style.min_height.is_some()
      {
        (min_height - reserved_horizontal_gutter).max(0.0)
      } else {
        min_height
      };
      let max_height = if let Some(keyword) = style.max_height_keyword {
        let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
        let max_border = match keyword {
          crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
          crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
          crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
            if available_block_border_box.is_finite() {
              available_block_border_box
            } else {
              intrinsic_max
            }
          }
          crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
            let basis_border = match limit {
              Some(limit) => resolve_length_with_percentage_metrics_and_root_font_metrics(
                limit,
                containing_height,
                self.viewport_size,
                style.font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
                self.factory.root_font_metrics(),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(f32::INFINITY),
              None => available_block_border_box,
            };
            crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
          }
          crate::style::types::IntrinsicSizeKeyword::CalcSize(calc) => {
            use crate::style::types::BoxSizing;
            use crate::style::types::CalcSizeBasis;
            let basis_border = match calc.basis {
              CalcSizeBasis::Auto => intrinsic_max,
              CalcSizeBasis::MinContent => intrinsic_min,
              CalcSizeBasis::MaxContent => intrinsic_max,
              CalcSizeBasis::FillAvailable => {
                if available_block_border_box.is_finite() {
                  available_block_border_box
                } else {
                  intrinsic_max
                }
              }
              CalcSizeBasis::FitContent { limit } => {
                let basis_border = match limit {
                  Some(limit) => resolve_length_with_percentage_metrics(
                    limit,
                    containing_height,
                    self.viewport_size,
                    style.font_size,
                    style.root_font_size,
                    Some(style),
                    Some(&self.font_context),
                  )
                  .map(|resolved| {
                    border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
                  })
                  .unwrap_or(f32::INFINITY),
                  None => available_block_border_box,
                };
                crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
              }
              CalcSizeBasis::Length(len) => resolve_length_with_percentage_metrics(
                len,
                containing_height,
                self.viewport_size,
                style.font_size,
                style.root_font_size,
                Some(style),
                Some(&self.font_context),
              )
              .map(|resolved| {
                border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing)
              })
              .unwrap_or(intrinsic_max),
            }
            .max(0.0);
            let basis_content = (basis_border - vertical_edges).max(0.0);
            let basis_specified = match style.box_sizing {
              BoxSizing::ContentBox => basis_content,
              BoxSizing::BorderBox => basis_border,
            };
            crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
              .and_then(|expr_sum| {
                crate::css::properties::parse_length(&format!("calc({expr_sum})"))
              })
              .and_then(|expr_len| {
                resolve_length_with_percentage_metrics(
                  expr_len,
                  containing_height,
                  self.viewport_size,
                  style.font_size,
                  style.root_font_size,
                  Some(style),
                  Some(&self.font_context),
                )
              })
              .map(|resolved_specified| {
                border_size_from_box_sizing(resolved_specified, vertical_edges, style.box_sizing)
                  .max(0.0)
              })
              .unwrap_or(basis_border)
          }
        };
        (max_border - vertical_edges).max(0.0)
      } else {
        style
          .max_height
          .as_ref()
          .and_then(|l| {
            resolve_length_with_percentage_metrics_and_root_font_metrics(
              *l,
              containing_height,
              self.viewport_size,
              style.font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
              self.factory.root_font_metrics(),
            )
          })
          .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
          .unwrap_or(f32::INFINITY)
      };
      let max_height = if reserved_horizontal_gutter > 0.0
        && style.box_sizing == crate::style::types::BoxSizing::ContentBox
        && style.max_height_keyword.is_none()
        && style.max_height.is_some()
        && max_height.is_finite()
      {
        (max_height - reserved_horizontal_gutter).max(0.0)
      } else {
        max_height
      };

      let max_height = if max_height.is_finite() && max_height < min_height {
        min_height
      } else {
        max_height
      };
      let height = crate::layout::utils::clamp_with_order(
        resolved_height.unwrap_or(content_height),
        min_height,
        max_height,
      );

      if should_relayout_abspos_descendants_for_cb_height && !skip_contents {
        // Absolutely positioned descendants can use percentage block-sizes/insets that resolve
        // against the *used* padding box height of their containing block (CSS 2.1 §10.5/§10.6.4),
        // even when the containing block itself is `height:auto`. The used height isn't known until
        // after in-flow layout, but inline formatting contexts lay out their positioned descendants
        // during the main flow pass. Re-run child layout with an updated containing block height so
        // nested abspos descendants see the correct percentage basis and constraint-equation inputs.

        let used_padding_size =
          Size::new(padding_size.width, height + padding_top + padding_bottom);
        let used_cb_block_base = Some(used_padding_size.height);
        let relayout_nearest_cb = if establishes_positioned_cb {
          ContainingBlock::with_viewport_and_bases(
            Rect::new(padding_origin, used_padding_size),
            self.viewport_size,
            Some(used_padding_size.width),
            used_cb_block_base,
          )
        } else {
          nearest_cb
        };
        let relayout_nearest_fixed_cb = if establishes_fixed_cb {
          ContainingBlock::with_viewport_and_bases(
            Rect::new(padding_origin, used_padding_size),
            self.viewport_size,
            Some(used_padding_size.width),
            used_cb_block_base,
          )
        } else {
          nearest_fixed_cb
        };

        let mut relayout_ctx = child_ctx.clone();
        relayout_ctx.nearest_positioned_cb = relayout_nearest_cb;
        relayout_ctx.nearest_fixed_cb = relayout_nearest_fixed_cb;
        if relayout_nearest_cb != child_ctx.nearest_positioned_cb
          || relayout_nearest_fixed_cb != child_ctx.nearest_fixed_cb
        {
          if relayout_nearest_cb != child_ctx.nearest_positioned_cb {
            relayout_ctx.factory = relayout_ctx.factory.with_positioned_cb(relayout_nearest_cb);
          }
          if relayout_nearest_fixed_cb != child_ctx.nearest_fixed_cb {
            relayout_ctx.factory = relayout_ctx
              .factory
              .with_fixed_cb(relayout_nearest_fixed_cb);
          }
          relayout_ctx.intrinsic_inline_fc = Arc::new(InlineFormattingContext::with_factory(
            relayout_ctx.factory.clone(),
          ));
        }

        let (frags, _relayout_height, positioned, info) = layout_contents(
          &relayout_ctx,
          &relayout_nearest_cb,
          &relayout_nearest_fixed_cb,
        )?;
        child_fragments = frags;
        positioned_children = positioned;
        column_info = info;
      }

      if !skip_contents {
        let remembered = if block_axis_is_horizontal(style.writing_mode) {
          Size::new(height, computed_width.content_width)
        } else {
          Size::new(computed_width.content_width, height)
        };
        remembered_size_cache_store(box_node, remembered);
      }

      let box_height = border_top + padding_top + height + padding_bottom + border_bottom;
      // For root/layout entry points, keep fragment bounds scoped to the border box so margins
      // don’t inflate measured sizes (e.g., when flex items are measured via a block FC). The
      // margin space stays outside the fragment’s local coordinates, matching the child layout
      // path in `layout_block_child`.
      let box_width = computed_width.border_box_width();

      // Layout out-of-flow positioned children against this block's padding box (CSS 2.1 §10.1).
      //
      // `child_fragments` have already been translated into the border-box coordinate space above, so
      // keep the containing block in that same coordinate space (origin at the padding edge).
      let padding_origin = Point::new(computed_width.border_left, border_top);
      let padding_size = Size::new(
        computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
        height + padding_top + padding_bottom,
      );
      let padding_rect = Rect::new(padding_origin, padding_size);

      if !positioned_children.is_empty() {
        let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
          self.font_context.clone(),
        );
        // `child_fragments` are already in this block's border-box coordinate space, so build the
        // anchor index from them directly. Out-of-flow positioned boxes are also placed in this same
        // coordinate space (with the padding edge at `padding_origin`).
        let mut anchor_index =
          crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
            child_fragments.as_slice(),
            box_node.id,
            &style.anchor_scope,
            self.viewport_size,
          );
        // Allow descendants to anchor against the containing block element itself.
        anchor_index.insert_names_for_box(
          box_node.id,
          &style.anchor_names,
          crate::layout::anchor_positioning::AnchorBox {
            rect: Rect::from_xywh(0.0, 0.0, box_width, box_height),
            writing_mode: style.writing_mode,
            direction: style.direction,
          },
        );
        let mut anchor_index_physical = None;
        let mut parent_padding_cb_physical = None;
        let needs_physical_coordinate_space = block_axis_is_horizontal(style.writing_mode)
          || !inline_axis_positive(style.writing_mode, style.direction);
        if needs_physical_coordinate_space {
          let physical_children: Vec<_> = child_fragments
            .iter()
            .cloned()
            .map(|fragment| {
              convert_fragment_axes(
                fragment,
                box_width,
                box_height,
                style.writing_mode,
                style.direction,
              )
            })
            .collect();
          let mut physical_index =
            crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
              physical_children.as_slice(),
              box_node.id,
              &style.anchor_scope,
              self.viewport_size,
            );
          physical_index.insert_names_for_box(
            box_node.id,
            &style.anchor_names,
            crate::layout::anchor_positioning::AnchorBox {
              rect: Rect::new(
                Point::ZERO,
                if block_axis_is_horizontal(style.writing_mode) {
                  Size::new(box_height, box_width)
                } else {
                  Size::new(box_width, box_height)
                },
              ),
              writing_mode: style.writing_mode,
              direction: style.direction,
            },
          );
          anchor_index_physical = Some(physical_index);

          let padding_rect_physical = logical_rect_to_physical_full(
            padding_rect,
            box_width,
            box_height,
            style.writing_mode,
            style.direction,
          );
          parent_padding_cb_physical = Some(
            ContainingBlock::with_viewport_and_bases(
              padding_rect_physical,
              self.viewport_size,
              Some(padding_rect_physical.size.width),
              Some(padding_rect_physical.size.height),
            )
            .with_writing_mode_and_direction(style.writing_mode, style.direction)
            .with_box_id(Some(box_node.id)),
          );
        }
        let parent_padding_cb = ContainingBlock::with_viewport_and_bases(
          padding_rect,
          self.viewport_size,
          Some(padding_size.width),
          Some(padding_size.height),
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction)
        .with_box_id(Some(box_node.id));
        let base_factory = self.factory.clone();
        let viewport_cb = ContainingBlock::viewport(self.viewport_size);
        let abs_factory = if parent_padding_cb == base_factory.nearest_positioned_cb() {
          base_factory.clone()
        } else {
          base_factory.with_positioned_cb(parent_padding_cb)
        };
        let fixed_factory = if viewport_cb == parent_padding_cb {
          abs_factory.clone()
        } else if viewport_cb == base_factory.nearest_positioned_cb() {
          base_factory.clone()
        } else {
          base_factory.with_positioned_cb(viewport_cb)
        };
        let factory_for_cb = |cb: ContainingBlock| -> &FormattingContextFactory {
          if cb == parent_padding_cb {
            &abs_factory
          } else if cb == viewport_cb {
            &fixed_factory
          } else {
            &base_factory
          }
        };

        let trace_positioned = trace_positioned_ids();
        for PositionedCandidate {
          node: child,
          source,
          static_position,
          query_parent_id,
          implicit_anchor_box_id,
        } in positioned_children
        {
          let original_style = child.style.clone();
          let cb = match source {
            ContainingBlockSource::ParentPadding => parent_padding_cb,
            ContainingBlockSource::Explicit(cb) => cb,
          };
          let (anchors_for_cb, positioning_cb, needs_physical_conversion) =
            if cb == parent_padding_cb {
              match (anchor_index_physical.as_ref(), parent_padding_cb_physical) {
                (Some(index), Some(physical_cb)) => (Some(index), physical_cb, true),
                _ => (Some(&anchor_index), cb, false),
              }
            } else {
              (Some(&anchor_index), cb, false)
            };
          let factory = factory_for_cb(cb);
          // Layout the child as if it were in normal flow to obtain its intrinsic size.
          let mut static_style = child.style.clone();
          {
            let s = Arc::make_mut(&mut static_style);
            s.position = Position::Relative;
            s.top = crate::style::types::InsetValue::Auto;
            s.right = crate::style::types::InsetValue::Auto;
            s.bottom = crate::style::types::InsetValue::Auto;
            s.left = crate::style::types::InsetValue::Auto;
          }

          let fc_type = child
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let fc = factory.get(fc_type);
          let child_height_space = cb
            .block_percentage_base()
            .map(AvailableSpace::Definite)
            .unwrap_or(AvailableSpace::Indefinite);
          let child_constraints = LayoutConstraints::new(
            AvailableSpace::Definite(padding_size.width),
            child_height_space,
          );

          // Resolve positioned style against the containing block.
          let anchor_query = crate::layout::anchor_positioning::AnchorQueryContext {
            query_parent_box_id: Some(query_parent_id),
            implicit_anchor_box_id,
          };
          let positioned_style =
            crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
              &original_style,
              &positioning_cb,
              self.viewport_size,
              &self.font_context,
              anchors_for_cb,
              anchor_query,
            );

          let relayout_for_definite_insets = (positioned_style.width.is_auto()
            && !positioned_style.left.is_auto()
            && !positioned_style.right.is_auto())
            || (positioned_style.height.is_auto()
              && !positioned_style.top.is_auto()
              && !positioned_style.bottom.is_auto());

          let mut static_pos = static_position.unwrap_or(Point::ZERO);
          if cb == parent_padding_cb {
            static_pos = Point::new(
              static_pos.x + computed_width.padding_left,
              static_pos.y + padding_top,
            );
          } else {
            // Static positions are recorded in this block's content coordinate space. When the
            // positioned containing block is an inherited one (e.g. viewport), rebase the static
            // position so it's relative to the containing block origin.
            let origin = positioning_cb.origin();
            static_pos = Point::new(static_pos.x - origin.x, static_pos.y - origin.y);
          }
          if needs_physical_conversion {
            static_pos = logical_rect_to_physical_full(
              Rect::new(static_pos, Size::new(0.0, 0.0)),
              padding_size.width,
              padding_size.height,
              style.writing_mode,
              style.direction,
            )
            .origin;
          }
          let is_replaced = child.is_replaced();
          let needs_inline_intrinsics = (positioned_style.width.is_auto()
            && (positioned_style.left.is_auto()
              || positioned_style.right.is_auto()
              || is_replaced))
            || original_style.width_keyword.is_some()
            || original_style.min_width_keyword.is_some()
            || original_style.max_width_keyword.is_some();
          let needs_block_intrinsics = (positioned_style.height.is_auto()
            && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()))
            || original_style.height_keyword.is_some()
            || original_style.min_height_keyword.is_some()
            || original_style.max_height_keyword.is_some();
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
                let child_fragment = fc.layout(&child, &child_constraints)?;
                let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
                  match fc.compute_intrinsic_inline_sizes(&child) {
                    Ok((min, max)) => (Some(min), Some(max)),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {
                      let min = match fc
                        .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MinContent)
                      {
                        Ok(value) => Some(value),
                        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                        Err(_) => None,
                      };
                      let max = match fc
                        .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MaxContent)
                      {
                        Ok(value) => Some(value),
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
                  match fc.compute_intrinsic_block_size(&child, IntrinsicSizingMode::MinContent) {
                    Ok(value) => Some(value),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  }
                } else {
                  None
                };
                let preferred_block = if needs_block_intrinsics {
                  match fc.compute_intrinsic_block_size(&child, IntrinsicSizingMode::MaxContent) {
                    Ok(value) => Some(value),
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
                    Ok(value) => Some(value),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                  let max = match fc
                    .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
                  {
                    Ok(value) => Some(value),
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
              match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent)
              {
                Ok(value) => Some(value),
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => None,
              }
            } else {
              None
            };
            let preferred_block = if needs_block_intrinsics {
              match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent)
              {
                Ok(value) => Some(value),
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
              &original_style,
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
          input.is_replaced = is_replaced;
          input.preferred_min_inline_size = preferred_min_inline;
          input.preferred_inline_size = preferred_inline;
          input.preferred_min_block_size = preferred_min_block;
          input.preferred_block_size = preferred_block;
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );

          let (layout_positioned_style, mut result) =
            crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
              &abs,
              &input,
              &original_style,
              &positioning_cb,
              self.viewport_size,
              &self.font_context,
              anchors_for_cb,
              anchor_query,
            )?;
          let mut border_size_physical = Size::new(
            result.size.width + actual_horizontal,
            result.size.height + actual_vertical,
          );
          let mut border_origin_physical = Point::new(
            result.position.x - content_offset.x,
            result.position.y - content_offset.y,
          );
          let (mut border_origin, mut border_size) = if needs_physical_conversion {
            let border_rect = Rect::new(border_origin_physical, border_size_physical);
            let logical_rect = physical_rect_to_logical_full(
              border_rect,
              box_width,
              box_height,
              style.writing_mode,
              style.direction,
            );
            (logical_rect.origin, logical_rect.size)
          } else {
            (border_origin_physical, border_size_physical)
          };

          if crate::layout::absolute_positioning::auto_height_uses_intrinsic_size(
            &layout_positioned_style,
            input.is_replaced,
          ) && (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          {
            let measure_constraints = child_constraints
              .with_width(AvailableSpace::Definite(border_size.width))
              .with_height(AvailableSpace::Indefinite)
              .with_used_border_box_size(Some(border_size.width), None);

            child_fragment = if child.id != 0 {
              if supports_used_border_box {
                crate::layout::style_override::with_style_override(
                  child.id,
                  static_style.clone(),
                  || fc.layout(&child, &measure_constraints),
                )?
              } else {
                let mut measure_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut measure_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.width_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                }
                crate::layout::style_override::with_style_override(child.id, measure_style, || {
                  fc.layout(&child, &measure_constraints)
                })?
              }
            } else {
              let mut relayout_child = child.clone();
              if supports_used_border_box {
                relayout_child.style = static_style.clone();
              } else {
                let mut measure_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut measure_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.width_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                }
                relayout_child.style = measure_style;
              }
              fc.layout(&relayout_child, &measure_constraints)?
            };

            input.intrinsic_size.height =
              (child_fragment.bounds.size.height - actual_vertical).max(0.0);

            let (_, rerun_result) =
              crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
                &abs,
                &input,
                &original_style,
                &positioning_cb,
                self.viewport_size,
                &self.font_context,
                anchors_for_cb,
                anchor_query,
              )?;
            result = rerun_result;
            border_size_physical = Size::new(
              result.size.width + actual_horizontal,
              result.size.height + actual_vertical,
            );
            border_origin_physical = Point::new(
              result.position.x - content_offset.x,
              result.position.y - content_offset.y,
            );
            (border_origin, border_size) = if needs_physical_conversion {
              let border_rect = Rect::new(border_origin_physical, border_size_physical);
              let logical_rect = physical_rect_to_logical_full(
                border_rect,
                box_width,
                box_height,
                style.writing_mode,
                style.direction,
              );
              (logical_rect.origin, logical_rect.size)
            } else {
              (border_origin_physical, border_size_physical)
            };
          }
          let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
            || (border_size.height - child_fragment.bounds.height()).abs() > 0.01
            || relayout_for_definite_insets;
          if needs_relayout {
            let relayout_constraints = child_constraints
              .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
            if child.id != 0 {
              if supports_used_border_box {
                child_fragment = crate::layout::style_override::with_style_override(
                  child.id,
                  static_style.clone(),
                  || fc.layout(&child, &relayout_constraints),
                )?;
              } else {
                let mut relayout_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut relayout_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.height = Some(crate::style::values::Length::px(border_size.height));
                  s.width_keyword = None;
                  s.height_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                  s.min_height_keyword = None;
                  s.max_height_keyword = None;
                }
                child_fragment = crate::layout::style_override::with_style_override(
                  child.id,
                  relayout_style,
                  || fc.layout(&child, &relayout_constraints),
                )?;
              }
            } else {
              let mut relayout_child = child.clone();
              if supports_used_border_box {
                relayout_child.style = static_style.clone();
              } else {
                let mut relayout_style = static_style.clone();
                {
                  let s = Arc::make_mut(&mut relayout_style);
                  s.width = Some(crate::style::values::Length::px(border_size.width));
                  s.height = Some(crate::style::values::Length::px(border_size.height));
                  s.width_keyword = None;
                  s.height_keyword = None;
                  s.min_width_keyword = None;
                  s.max_width_keyword = None;
                  s.min_height_keyword = None;
                  s.max_height_keyword = None;
                }
                relayout_child.style = relayout_style;
              }
              child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
            }
          }
          child_fragment.bounds = Rect::new(border_origin, border_size);
          // Match `layout_block_child`: translate positioned fragments that are still expressed in
          // the content coordinate space alongside in-flow fragments. See the comment at the
          // equivalent site above for the reasoning behind excluding viewport-fixed fragments and
          // elements positioned against this element's own padding box.
          if cb != viewport_cb
            && cb != parent_padding_cb
            && (content_origin.x != 0.0 || content_origin.y != 0.0)
          {
            child_fragment.translate_root_in_place(content_origin);
          }
          child_fragment.style = Some(original_style);
          if matches!(child_fragment.style.as_deref().map(|s| s.position), Some(Position::Absolute))
          {
            child_fragment.abs_containing_block_box_id = cb.box_id();
          }
          if trace_positioned.contains(&child.id) {
            let (text_count, total) = count_text_fragments(&child_fragment);
            eprintln!(
                        "[block-positioned-placed] child_id={} pos=({:.1},{:.1}) size=({:.1},{:.1}) texts={}/{}",
                        child.id,
                        child_fragment.bounds.x(),
                        child_fragment.bounds.y(),
                        border_size.width,
                        border_size.height,
                        text_count,
                        total
                    );
          }
          child_fragments.push(child_fragment);
        }
      }

      let bounds = Rect::from_xywh(computed_width.margin_left, 0.0, box_width, box_height);

      let mut fragment = FragmentNode::new_with_style(
        bounds,
        crate::tree::fragment_tree::FragmentContent::Block {
          box_id: Some(box_node.id),
        },
        child_fragments,
        box_node.style.clone(),
      );
      if let Some(info) = column_info {
        fragment.fragmentation = Some(info.clone());
        // Keep logical bounds aligned with the physical multi-column fragment geometry so
        // pagination uses the clipped height rather than the unfragmented flow height.
        fragment.logical_override = Some(fragment.bounds);
      }

      // Apply relative positioning after normal flow layout (CSS 2.1 §9.4.3).
      if style.position.is_relative() {
        let (cb_width, cb_height, inline_base, block_base) = if inline_is_horizontal {
          (
            constraints.width().unwrap_or(0.0),
            constraints.height().unwrap_or(0.0),
            constraints.width(),
            constraints.height(),
          )
        } else {
          (
            constraints.height().unwrap_or(0.0),
            constraints.width().unwrap_or(0.0),
            constraints.height(),
            constraints.width(),
          )
        };
        let containing_block = ContainingBlock::with_viewport_and_bases(
          Rect::new(Point::ZERO, Size::new(cb_width, cb_height)),
          self.viewport_size,
          inline_base,
          block_base,
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction);
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
          style,
          &containing_block,
          self.viewport_size,
          &self.font_context,
        );
        fragment = PositionedLayout::with_font_context(self.font_context.clone())
          .apply_relative_positioning(&fragment, &positioned_style, &containing_block)?;
      }

      fragment.scrollbar_reservation = crate::layout::utils::scrollbar_reservation_for_style(style);
      let converted = convert_fragment_axes(
        fragment,
        box_width,
        box_height,
        style.writing_mode,
        style.direction,
      );

      layout_cache_store(
        box_node,
        FormattingContextType::Block,
        constraints,
        &converted,
        self.factory.viewport_scroll(),
        self.viewport_size,
        self.nearest_positioned_cb,
        self.nearest_fixed_cb,
      );

      return Ok(converted);
    }

    // FastRender models scrollbars as overlay by default: scrollbars do not affect layout unless the
    // author opts into reserving space via `scrollbar-gutter: stable`.
    //
    // The `overflow:auto` convergence loop below exists for classic scrollbar experiments (where
    // scrollbars can reduce the scrollport size). Skip it under the default overlay model to avoid
    // redundant layout passes.
    if !crate::debug::runtime::runtime_toggles().truthy("FASTR_CLASSIC_SCROLLBARS") {
      return crate::layout::auto_scrollbars::with_bypass(box_node, || {
        self.layout(box_node, constraints)
      });
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let base_style = style_override.unwrap_or_else(|| box_node.style.clone());
    let gutter = crate::layout::utils::resolve_scrollbar_width(&base_style);
    if gutter <= 0.0
      || !base_style.scrollbar_gutter.stable
      || (!matches!(base_style.overflow_x, Overflow::Auto)
        && !matches!(base_style.overflow_y, Overflow::Auto))
    {
      return crate::layout::auto_scrollbars::with_bypass(box_node, || {
        self.layout(box_node, constraints)
      });
    }

    let containing_width = constraints
      .inline_percentage_base
      .or_else(|| constraints.width())
      .unwrap_or(self.viewport_size.width);
    let mut force_x = false;
    let mut force_y = false;
    let mut last: Option<FragmentNode> = None;
    for _ in 0..3 {
      let mut override_style = base_style.clone();
      let mut overridden = false;
      {
        let s = Arc::make_mut(&mut override_style);
        if force_x
          && matches!(base_style.overflow_x, Overflow::Auto)
          && !base_style.scrollbar_gutter.stable
        {
          s.overflow_x = Overflow::Scroll;
          overridden = true;
        }
        if force_y
          && matches!(base_style.overflow_y, Overflow::Auto)
          && !base_style.scrollbar_gutter.stable
        {
          s.overflow_y = Overflow::Scroll;
          overridden = true;
        }
      }

      let fragment = if overridden && box_node.id != 0 {
        crate::layout::style_override::with_style_override(
          box_node.id,
          override_style.clone(),
          || {
            crate::layout::auto_scrollbars::with_bypass(box_node, || {
              self.layout(box_node, constraints)
            })
          },
        )
      } else if overridden {
        let mut cloned = box_node.clone();
        cloned.style = override_style.clone();
        crate::layout::auto_scrollbars::with_bypass(&cloned, || self.layout(&cloned, constraints))
      } else {
        crate::layout::auto_scrollbars::with_bypass(box_node, || self.layout(box_node, constraints))
      }?;

      let (overflow_x, overflow_y) = crate::layout::utils::fragment_overflows_content_box(
        &fragment,
        override_style.as_ref(),
        containing_width,
        self.viewport_size,
        Some(&self.font_context),
      );
      let need_x = gutter > 0.0
        && matches!(base_style.overflow_x, Overflow::Auto)
        && !base_style.scrollbar_gutter.stable
        && overflow_x;
      let need_y = gutter > 0.0
        && matches!(base_style.overflow_y, Overflow::Auto)
        && !base_style.scrollbar_gutter.stable
        && overflow_y;

      last = Some(fragment);
      if need_x == force_x && need_y == force_y {
        break;
      }
      force_x = need_x;
      force_y = need_y;
    }

    let Some(last) = last else {
      debug_assert!(false, "at least one layout pass");
      return Err(LayoutError::MissingContext(
        "block layout produced no fragments".to_string(),
      ));
    };
    Ok(last)
  }

  fn compute_intrinsic_inline_sizes(&self, box_node: &BoxNode) -> Result<(f32, f32), LayoutError> {
    count_block_intrinsic_call();
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style = style_override.as_ref().unwrap_or(&box_node.style);
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let root_font_metrics = self.factory.root_font_metrics();

    // Intrinsic inline sizes are normally memoized since they can require expensive inline layout.
    // However, when inline-size containment is enabled and `contain-intrinsic-*: auto` is in effect,
    // the returned size depends on the element's remembered size, which can change within a cache
    // epoch as elements are laid out (e.g., as `content-visibility:auto` boxes transition from
    // skipped → laid out). In that case, bypass the intrinsic cache so callers always observe the
    // latest remembered size.
    if !style.containment.isolates_inline_size() {
      let min_cached = intrinsic_cache_lookup(box_node, IntrinsicSizingMode::MinContent);
      let max_cached = intrinsic_cache_lookup(box_node, IntrinsicSizingMode::MaxContent);
      if let (Some(min), Some(max)) = (min_cached, max_cached) {
        return Ok((min, max));
      }
    }

    let edges = if inline_is_horizontal {
      horizontal_padding_and_borders(
        style,
        0.0,
        self.viewport_size,
        &self.font_context,
        root_font_metrics,
      )
    } else {
      vertical_padding_and_borders(
        style,
        0.0,
        self.viewport_size,
        &self.font_context,
        root_font_metrics,
      )
    };
    // Honor specified sizes in the inline axis that resolve without a containing block.
    let specified_inline = if inline_is_horizontal {
      style.width.as_ref()
    } else {
      style.height.as_ref()
    };
    if let Some(specified) = specified_inline {
      if !specified.has_percentage() {
        let resolved = resolve_length_for_width(
          *specified,
          0.0,
          style,
          &self.font_context,
          self.viewport_size,
          root_font_metrics,
        );
        if resolved.is_finite() {
          let result = border_size_from_box_sizing(resolved.max(0.0), edges, style.box_sizing);
          intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, result);
          intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, result);
          return Ok((result, result));
        }
      }
    }

    if style.containment.isolates_inline_size() {
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      let remembered = axis
        .auto
        .then(|| {
          remembered_size_cache_lookup(box_node).map(|size| {
            if inline_is_horizontal {
              size.width
            } else {
              size.height
            }
          })
        })
        .flatten();
      let fallback = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        remembered,
        Some(0.0),
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
      let result = (edges + fallback).max(0.0);
      if !axis.auto {
        intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, result);
        intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, result);
      }
      return Ok((result, result));
    }

    let has_percentage_sizing = has_percentage_sizing_hint(style);

    if let Some(control) = box_node.form_control.as_deref() {
      // `appearance:none` form controls are generated as normal boxes so their authored/pseudo
      // content can participate in layout, but intrinsic sizing must still follow form-control
      // rules. In particular, text inputs/areas must not use the synthesized placeholder/value
      // text for their min-content size because that feeds flexbox's `min-width:auto` algorithm and
      // makes inputs appear unshrinkable.
      let shaper = self.factory.shaping_pipeline();
      let intrinsic_size =
        crate::tree::form_control_intrinsic::intrinsic_content_size_for_form_control(
          control,
          style,
          self.viewport_size,
          None,
          &self.font_context,
          Some(&shaper),
        );
      // Use a synthetic replaced box to reuse the replaced sizing algorithm. The replaced type is
      // irrelevant here (the only branch in `compute_replaced_size` that depends on it is intrinsic
      // aspect-ratio derivation, which we disable to match form-control behavior).
      let synthetic_replaced = ReplacedBox {
        replaced_type: crate::tree::box_tree::ReplacedType::Canvas,
        intrinsic_size: Some(intrinsic_size),
        aspect_ratio: None,
        no_intrinsic_ratio: true,
      };

      let max_size = compute_replaced_size(style, &synthetic_replaced, None, self.viewport_size);
      let max_inline_size = if inline_is_horizontal {
        max_size.width
      } else {
        max_size.height
      };
      let max_result = (max_inline_size + edges).max(0.0);

      let mut min_result = if has_percentage_sizing {
        let min_size = compute_replaced_size(
          style,
          &synthetic_replaced,
          Some(Size::new(0.0, 0.0)),
          self.viewport_size,
        );
        let min_inline_size = if inline_is_horizontal {
          min_size.width
        } else {
          min_size.height
        };
        (min_inline_size + edges).max(0.0).min(max_result)
      } else {
        max_result
      };

      use crate::tree::box_tree::FormControlKind;
      if matches!(
        &control.control,
        FormControlKind::Text { .. } | FormControlKind::TextArea { .. }
      ) {
        min_result = edges.max(0.0).min(max_result);
      }

      intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, min_result);
      intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, max_result);
      return Ok((min_result, max_result));
    }

    if let BoxType::Replaced(replaced_box) = &box_node.box_type {
      // Replaced elements' min- and max-content sizes are typically equal because they do not
      // wrap. However, when a replaced element's used size is defined in terms of percentages
      // (`width:100%`, `height:100%`, `max-width:100%`, etc) and the percentage base is unknown
      // (intrinsic sizing probes), treating those unresolved percentages as `auto` makes the
      // element appear unshrinkable. This cascades into flexbox's `min-width:auto` algorithm
      // (content-based automatic minimum size), causing flex items containing percentage-sized
      // replaced elements to refuse shrinking.
      //
      // To avoid that, use a 0px percentage base for the *min-content* probe when any relevant
      // sizing hint includes percentages. The max-content size continues to treat unresolved
      // percentages as `auto` so the intrinsic preferred size remains available to algorithms
      // that need it.

      let max_size = compute_replaced_size(style, replaced_box, None, self.viewport_size);
      let max_inline_size = if inline_is_horizontal {
        max_size.width
      } else {
        max_size.height
      };
      let max_result = (max_inline_size + edges).max(0.0);

      let mut min_result = if has_percentage_sizing {
        let min_size = compute_replaced_size(
          style,
          replaced_box,
          Some(Size::new(0.0, 0.0)),
          self.viewport_size,
        );
        let min_inline_size = if inline_is_horizontal {
          min_size.width
        } else {
          min_size.height
        };
        (min_inline_size + edges).max(0.0).min(max_result)
      } else {
        max_result
      };
      if let crate::tree::box_tree::ReplacedType::FormControl(control) = &replaced_box.replaced_type
      {
        use crate::tree::box_tree::FormControlKind;

        if matches!(
          &control.control,
          FormControlKind::Text { .. } | FormControlKind::TextArea { .. }
        ) {
          min_result = edges.max(0.0).min(max_result);
        }
      }

      intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, min_result);
      intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, max_result);
      return Ok((min_result, max_result));
    }

    let factory = &self.factory;
    let inline_fc = self.intrinsic_inline_fc.as_ref();

    // Inline formatting context contribution (text and inline-level children).
    // Block-level children split inline runs into separate formatting contexts.
    let log_ids = crate::debug::runtime::runtime_toggles()
      .usize_list("FASTR_LOG_INTRINSIC_IDS")
      .unwrap_or_default();
    let log_children = !log_ids.is_empty() && log_ids.contains(&box_node.id);

    let mut inline_min_width = 0.0f32;
    let mut inline_max_width = 0.0f32;
    let mut block_min_width = 0.0f32;
    let mut block_max_width = 0.0f32;
    let mut float_min_width = 0.0f32;
    let mut float_max_width = 0.0f32;
    let mut float_line_width = 0.0f32;
    let mut float_line_has_left = false;
    let mut float_line_has_right = false;

    let mut inline_child_debug: Vec<(usize, Display)> = Vec::new();

    // Avoid going through `factory.get(FormattingContextType::Block)` for block children:
    // `FormattingContextFactory::block_context` constructs a BlockFormattingContext backed by a
    // `detached()` factory clone (to avoid factory↔cached-FC Arc cycles). If we used `get(Block)`
    // recursively we'd create a new detached factory per block depth during intrinsic sizing, which
    // is exactly the kind of allocation churn tables can amplify.
    let compute_child_intrinsic_sizes = |child: &BoxNode| -> Result<(f32, f32), LayoutError> {
      let fc_type = child
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      if fc_type == FormattingContextType::Block {
        self.compute_intrinsic_inline_sizes(child)
      } else {
        factory.get(fc_type).compute_intrinsic_inline_sizes(child)
      }
    };

    // Parallel intrinsic sizing path: pre-scan into DOM-order segments, compute expensive
    // contributions in parallel, then combine deterministically.
    if self.parallelism.should_parallelize(box_node.children.len()) && !box_node.children.is_empty()
    {
      #[derive(Debug)]
      enum Segment<'a> {
        InlineRun { start: usize, end: usize },
        BlockChild(&'a BoxNode),
        FloatChild(&'a BoxNode),
      }

      #[derive(Clone, Copy, Debug)]
      struct FloatMeta {
        outer_min: f32,
        outer_max: f32,
        clear_side: ClearSide,
        float_side: Option<FloatSide>,
      }

      #[derive(Clone, Copy, Debug)]
      enum Contribution {
        InlineRun { min: f32, max: f32 },
        BlockChild { outer_min: f32, outer_max: f32 },
        Float(FloatMeta),
      }

      let mut segments: Vec<Segment<'_>> = Vec::with_capacity(box_node.children.len());
      // Store inline-run children in a single arena so we can reference them by index range without
      // allocating a new Vec for every flushed run.
      let mut inline_nodes: Vec<&BoxNode> = Vec::with_capacity(box_node.children.len());
      let mut run_start: Option<usize> = None;
      let mut deadline_counter = 0usize;
      for child in &box_node.children {
        if let Err(RenderError::Timeout { elapsed, .. }) =
          check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
        {
          return Err(LayoutError::Timeout { elapsed });
        }
        if is_out_of_flow(child) {
          continue;
        }

        if child.style.float.is_floating() {
          if let Some(start) = run_start.take() {
            let end = inline_nodes.len();
            if start != end {
              segments.push(Segment::InlineRun { start, end });
            }
          }
          segments.push(Segment::FloatChild(child));
          continue;
        }

        let treated_as_block = match child.box_type {
          BoxType::Replaced(_) if child.style.display.is_inline_level() => false,
          _ => child.is_block_level(),
        };

        if treated_as_block {
          if let Some(start) = run_start.take() {
            let end = inline_nodes.len();
            if start != end {
              segments.push(Segment::InlineRun { start, end });
            }
          }
          segments.push(Segment::BlockChild(child));
        } else {
          if log_children {
            inline_child_debug.push((child.id, child.style.display));
          }
          if run_start.is_none() {
            run_start = Some(inline_nodes.len());
          }
          inline_nodes.push(child);
        }
      }
      if let Some(start) = run_start {
        let end = inline_nodes.len();
        if start != end {
          segments.push(Segment::InlineRun { start, end });
        }
      }

      let parent_writing_mode = style.writing_mode;
      let parent_direction = style.direction;

      let compute_segment = |segment: &Segment<'_>| -> Result<Contribution, LayoutError> {
        match segment {
          Segment::InlineRun { start, end } => {
            let run = &inline_nodes[*start..*end];
            let (min_width, max_width) = inline_fc.intrinsic_widths_for_children(style, run)?;
            if log_children {
              let ids: Vec<usize> = run.iter().map(|c| c.id()).collect();
              eprintln!(
                "[intrinsic-inline-run] parent_id={} ids={:?} min={:.2} max={:.2}",
                box_node.id, ids, min_width, max_width
              );
            }
            Ok(Contribution::InlineRun {
              min: min_width,
              max: max_width,
            })
          }
          Segment::BlockChild(child) => {
            let (child_min, child_max) = compute_child_intrinsic_sizes(child)?;
            let (margin_start_side, margin_end_side) = inline_axis_sides(&child.style);
            let margin_start = resolve_margin_side(
              &child.style,
              margin_start_side,
              0.0,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            );
            let margin_end = resolve_margin_side(
              &child.style,
              margin_end_side,
              0.0,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            );
            let outer_min = (child_min + margin_start + margin_end).max(0.0);
            let outer_max = (child_max + margin_start + margin_end).max(0.0);
            if log_children {
              let sel = child
                .debug_info
                .as_ref()
                .map(|d| d.to_selector())
                .unwrap_or_else(|| "<anon>".to_string());
              let disp = child.style.display;
              eprintln!(
                "[intrinsic-child] parent_id={} child_id={} selector={} display={:?} min={:.2} max={:.2}",
                box_node.id, child.id, sel, disp, outer_min, outer_max
              );
            }
            Ok(Contribution::BlockChild {
              outer_min,
              outer_max,
            })
          }
          Segment::FloatChild(child) => {
            let (child_min, child_max) = compute_child_intrinsic_sizes(child)?;
            let (margin_start_side, margin_end_side) = inline_axis_sides(&child.style);
            let margin_start = resolve_margin_side(
              &child.style,
              margin_start_side,
              0.0,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            );
            let margin_end = resolve_margin_side(
              &child.style,
              margin_end_side,
              0.0,
              &self.font_context,
              self.viewport_size,
              root_font_metrics,
            );
            let outer_min = (child_min + margin_start + margin_end).max(0.0);
            let outer_max = (child_max + margin_start + margin_end).max(0.0);
            let clear_side =
              resolve_clear_side(child.style.clear, parent_writing_mode, parent_direction);
            let float_side =
              resolve_float_side(child.style.float, parent_writing_mode, parent_direction);
            Ok(Contribution::Float(FloatMeta {
              outer_min,
              outer_max,
              clear_side,
              float_side,
            }))
          }
        }
      };

      let mut contributions: Vec<Contribution> = if segments.len() > 1 {
        let deadline = active_deadline();
        let stage = active_stage();
        let heartbeat = active_heartbeat();
        let mut segment_results = segments
          .par_iter()
          .enumerate()
          .map_init(
            || 0usize,
            |deadline_counter, (idx, segment)| {
              with_deadline(deadline.as_ref(), || {
                let _hb_guard = StageHeartbeatGuard::install(heartbeat);
                let _stage_guard = StageGuard::install(stage);
                self.factory.debug_record_parallel_work();
                if let Err(RenderError::Timeout { elapsed, .. }) =
                  check_active_periodic(deadline_counter, 64, RenderStage::Layout)
                {
                  return Err(LayoutError::Timeout { elapsed });
                }
                compute_segment(segment).map(|value| (idx, value))
              })
            },
          )
          .collect::<Result<Vec<_>, LayoutError>>()?;

        // `par_iter().enumerate()` is indexed, but collecting through `Result` does not guarantee
        // stable ordering. Ensure deterministic DOM-order combination.
        let mut ordered = true;
        let mut prev_idx: Option<usize> = None;
        for (idx, _) in &segment_results {
          if let Some(prev) = prev_idx {
            if *idx <= prev {
              ordered = false;
              break;
            }
          }
          prev_idx = Some(*idx);
        }
        if !ordered {
          if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
            return Err(LayoutError::Timeout { elapsed });
          }
          segment_results.sort_unstable_by_key(|(idx, _)| *idx);
        }
        segment_results
          .into_iter()
          .enumerate()
          .map(|(expected_idx, (idx, value))| {
            debug_assert_eq!(
              idx, expected_idx,
              "parallel block intrinsic segment index mismatch"
            );
            value
          })
          .collect()
      } else {
        let mut out = Vec::with_capacity(segments.len());
        let mut deadline_counter = 0usize;
        for segment in &segments {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }
          out.push(compute_segment(segment)?);
        }
        out
      };

      // Combine segment results deterministically in DOM order.
      let mut combine_deadline_counter = 0usize;
      for contribution in contributions.drain(..) {
        if let Err(RenderError::Timeout { elapsed, .. }) =
          check_active_periodic(&mut combine_deadline_counter, 64, RenderStage::Layout)
        {
          return Err(LayoutError::Timeout { elapsed });
        }
        match contribution {
          Contribution::InlineRun { min, max } => {
            inline_min_width = inline_min_width.max(min);
            inline_max_width = inline_max_width.max(max);
          }
          Contribution::BlockChild {
            outer_min,
            outer_max,
          } => {
            block_min_width = block_min_width.max(outer_min);
            block_max_width = block_max_width.max(outer_max);
          }
          Contribution::Float(meta) => {
            float_min_width = float_min_width.max(meta.outer_min);
            if (meta.clear_side.clears_left() && float_line_has_left)
              || (meta.clear_side.clears_right() && float_line_has_right)
            {
              float_max_width = float_max_width.max(float_line_width);
              float_line_width = 0.0;
              float_line_has_left = false;
              float_line_has_right = false;
            }
            float_line_width += meta.outer_max;
            match meta.float_side {
              Some(FloatSide::Left) => float_line_has_left = true,
              Some(FloatSide::Right) => float_line_has_right = true,
              None => {}
            }
          }
        }
      }
    } else {
      // Serial path (unchanged from the historical implementation).
      let mut inline_run: Vec<&BoxNode> = Vec::new();
      let flush_inline_run = |run: &mut Vec<&BoxNode>,
                              widest_min: &mut f32,
                              widest_max: &mut f32|
       -> Result<(), LayoutError> {
        if run.is_empty() {
          return Ok(());
        }

        let (min_width, max_width) =
          inline_fc.intrinsic_widths_for_children(style, run.as_slice())?;
        if log_children {
          let ids: Vec<usize> = run.iter().map(|c| c.id()).collect();
          eprintln!(
            "[intrinsic-inline-run] parent_id={} ids={:?} min={:.2} max={:.2}",
            box_node.id, ids, min_width, max_width
          );
        }

        *widest_min = widest_min.max(min_width);
        *widest_max = widest_max.max(max_width);
        run.clear();
        Ok(())
      };

      let mut deadline_counter = 0usize;
      for child in &box_node.children {
        if let Err(RenderError::Timeout { elapsed, .. }) =
          check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
        {
          return Err(LayoutError::Timeout { elapsed });
        }
        if is_out_of_flow(child) {
          continue;
        }

        if child.style.float.is_floating() {
          flush_inline_run(
            &mut inline_run,
            &mut inline_min_width,
            &mut inline_max_width,
          )?;

          let (child_min, child_max) = compute_child_intrinsic_sizes(child)?;

          let (margin_start_side, margin_end_side) = inline_axis_sides(&child.style);
          let margin_start = resolve_margin_side(
            &child.style,
            margin_start_side,
            0.0,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          );
          let margin_end = resolve_margin_side(
            &child.style,
            margin_end_side,
            0.0,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          );
          let outer_min = (child_min + margin_start + margin_end).max(0.0);
          let outer_max = (child_max + margin_start + margin_end).max(0.0);

          float_min_width = float_min_width.max(outer_min);
          let clear_side =
            resolve_clear_side(child.style.clear, style.writing_mode, style.direction);
          if (clear_side.clears_left() && float_line_has_left)
            || (clear_side.clears_right() && float_line_has_right)
          {
            float_max_width = float_max_width.max(float_line_width);
            float_line_width = 0.0;
            float_line_has_left = false;
            float_line_has_right = false;
          }
          float_line_width += outer_max;
          match resolve_float_side(child.style.float, style.writing_mode, style.direction) {
            Some(FloatSide::Left) => float_line_has_left = true,
            Some(FloatSide::Right) => float_line_has_right = true,
            None => {}
          }
          continue;
        }
        let treated_as_block = match child.box_type {
          BoxType::Replaced(_) if child.style.display.is_inline_level() => false,
          _ => child.is_block_level(),
        };

        if treated_as_block {
          flush_inline_run(
            &mut inline_run,
            &mut inline_min_width,
            &mut inline_max_width,
          )?;

          let (child_min, child_max) = compute_child_intrinsic_sizes(child)?;
          // Intrinsic sizes are defined in terms of the *outer* size of in-flow children, i.e. the
          // margin box. Include the child's inline-axis margins when accumulating the min/max-content
          // widths of this block container.
          let (margin_start_side, margin_end_side) = inline_axis_sides(&child.style);
          let margin_start = resolve_margin_side(
            &child.style,
            margin_start_side,
            0.0,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          );
          let margin_end = resolve_margin_side(
            &child.style,
            margin_end_side,
            0.0,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          );
          let outer_min = (child_min + margin_start + margin_end).max(0.0);
          let outer_max = (child_max + margin_start + margin_end).max(0.0);
          block_min_width = block_min_width.max(outer_min);
          block_max_width = block_max_width.max(outer_max);
          if log_children {
            let sel = child
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anon>".to_string());
            let disp = child.style.display;
            eprintln!(
              "[intrinsic-child] parent_id={} child_id={} selector={} display={:?} min={:.2} max={:.2}",
              box_node.id, child.id, sel, disp, outer_min, outer_max
            );
          }
        } else {
          if log_children {
            inline_child_debug.push((child.id, child.style.display));
          }
          inline_run.push(child);
        }
      }
      flush_inline_run(
        &mut inline_run,
        &mut inline_min_width,
        &mut inline_max_width,
      )?;
    }

    let min_content_width = inline_min_width.max(block_min_width).max(float_min_width);
    float_max_width = float_max_width.max(float_line_width);
    let max_content_width = block_max_width
      .max(float_max_width)
      .max(inline_max_width + float_max_width);

    // Add this box's own padding and borders.
    let mut min_width = min_content_width + edges;
    let mut max_width = max_content_width + edges;

    // Apply min/max constraints to the border box.
    let min_inline_constraint = if inline_is_horizontal {
      style.min_width
    } else {
      style.min_height
    };
    let min_constraint = min_inline_constraint
      .and_then(|l| {
        resolve_length_with_percentage_metrics(
          l,
          None,
          self.viewport_size,
          style.font_size,
          style.root_font_size,
          Some(style.as_ref()),
          Some(&self.font_context),
        )
      })
      .map(|w| border_size_from_box_sizing(w, edges, style.box_sizing))
      .unwrap_or(0.0);
    let max_inline_constraint = if inline_is_horizontal {
      style.max_width
    } else {
      style.max_height
    };
    let max_constraint = max_inline_constraint
      .and_then(|l| {
        resolve_length_with_percentage_metrics(
          l,
          None,
          self.viewport_size,
          style.font_size,
          style.root_font_size,
          Some(style.as_ref()),
          Some(&self.font_context),
        )
      })
      .map(|w| border_size_from_box_sizing(w, edges, style.box_sizing))
      .unwrap_or(f32::INFINITY);
    let (min_constraint, max_constraint) = if max_constraint < min_constraint {
      (min_constraint, min_constraint)
    } else {
      (min_constraint, max_constraint)
    };
    min_width = crate::layout::utils::clamp_with_order(min_width, min_constraint, max_constraint);
    max_width = crate::layout::utils::clamp_with_order(max_width, min_constraint, max_constraint);

    let clamped_min = min_width.max(0.0);
    let clamped_max = max_width.max(0.0);

    // Optional tracing for over-large intrinsic widths.
    if !log_ids.is_empty() && log_ids.contains(&box_node.id) {
      let selector = box_node
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anon>".to_string());
      if !inline_child_debug.is_empty() {
        eprintln!(
          "[intrinsic-inline-children] parent_id={} ids={:?}",
          box_node.id, inline_child_debug
        );
      }
      eprintln!(
        "[intrinsic-widths] id={} selector={} inline_min={:.2} inline_max={:.2} block_min={:.2} block_max={:.2} float_min={:.2} float_max={:.2} edges={:.2} min={:.2} max={:.2} result_min={:.2} result_max={:.2}",
        box_node.id,
        selector,
        inline_min_width,
        inline_max_width,
        block_min_width,
        block_max_width,
        float_min_width,
        float_max_width,
        edges,
        min_constraint,
        max_constraint,
        clamped_min,
        clamped_max
      );
    }

    intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, clamped_min);
    intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, clamped_max);
    Ok((clamped_min, clamped_max))
  }

  fn compute_intrinsic_inline_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style = style_override.as_ref().unwrap_or(&box_node.style);
    if style.containment.isolates_inline_size() {
      let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      if axis.auto {
        let (min, max) = self.compute_intrinsic_inline_sizes(box_node)?;
        return Ok(match mode {
          IntrinsicSizingMode::MinContent => min,
          IntrinsicSizingMode::MaxContent => max,
        });
      }
    }

    if let Some(cached) = intrinsic_cache_lookup(box_node, mode) {
      count_block_intrinsic_call();
      return Ok(cached);
    }
    // For blocks, computing min/max-content widths shares most of the work (inline item
    // collection, shaping, descendant traversal). When we're missing a single intrinsic mode,
    // compute and cache both to avoid an immediate second pass from grid/flex track sizing.
    let (min, max) = self.compute_intrinsic_inline_sizes(box_node)?;
    Ok(match mode {
      IntrinsicSizingMode::MinContent => min,
      IntrinsicSizingMode::MaxContent => max,
    })
  }

  fn compute_intrinsic_block_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    // The default `FormattingContext::compute_intrinsic_block_size` implementation lays out the box
    // under `AvailableSpace::{MinContent,MaxContent}` in the inline axis. Block layout's intrinsic
    // probes intentionally treat percentage bases as 0px, but the containing block inline size still
    // needs to be *non-zero* so the element lays out at its intrinsic inline size rather than being
    // forced into a 0px column (which can explode the computed block size and poison flex/grid
    // sizing probes).
    //
    // Compute the element's intrinsic *inline* size first, then lay it out with that definite
    // inline size to obtain the corresponding intrinsic block size.
    count_block_intrinsic_call();
    if let Some(cached) = intrinsic_block_cache_lookup(box_node, mode) {
      return Ok(cached);
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
    let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);

    let intrinsic_inline = self.compute_intrinsic_inline_size(box_node, mode)?.max(0.0);

    // Force a non-zero inline size so intrinsic block-size probes don't lay out the element in a
    // 0px column, but still treat percentage-based values as if their percentage base were 0px (the
    // same behavior the default intrinsic probe path gets from `AvailableSpace::{Min,Max}Content`).
    //
    // We achieve this by:
    // 1) running layout under a *definite* inline constraint equal to the intrinsic inline size, and
    // 2) installing a temporary style override that resolves padding/margin percentages against a
    //    0px base so they don't pick up that definite inline constraint.
    let mut constraints = if inline_is_horizontal {
      LayoutConstraints::new(
        AvailableSpace::Definite(intrinsic_inline),
        AvailableSpace::Indefinite,
      )
    } else {
      LayoutConstraints::new(
        AvailableSpace::Indefinite,
        AvailableSpace::Definite(intrinsic_inline),
      )
    };
    // Block layout contains a "near-zero width" safeguard that falls back to the viewport size in
    // order to keep percentage sizing usable when flex/grid measurement passes a collapsed
    // containing block. Intrinsic block-size probes intentionally pass definite constraints that
    // can be 0px (e.g. when the min-content inline size is 0), so mark the computed inline size as
    // an explicit used border-box override to prevent that fallback from inflating intrinsic
    // measurements.
    if inline_is_horizontal {
      constraints.used_border_box_width = Some(intrinsic_inline);
    } else {
      constraints.used_border_box_height = Some(intrinsic_inline);
    }
    let mut probe_style = style_override.unwrap_or_else(|| box_node.style.clone());
    {
      let s = Arc::make_mut(&mut probe_style);

      // Layout uses the containing block inline size for resolving all padding percentages (even
      // for `padding-top`/`padding-bottom`). For intrinsic probes, that base is unknown, so resolve
      // against 0px and store the resulting absolute lengths in the override.
      let (
        padding_left,
        padding_right,
        padding_top,
        padding_bottom,
        margin_left,
        margin_right,
        margin_top,
        margin_bottom,
      ) = {
        let style_ref: &ComputedStyle = &*s;
        let root_font_metrics = self.factory.root_font_metrics();
        let resolve0 = |len: Length| {
          resolve_length_for_width(
            len,
            0.0,
            style_ref,
            &self.font_context,
            self.viewport_size,
            root_font_metrics,
          )
        };
        (
          Length::px(resolve0(style_ref.padding_left)),
          Length::px(resolve0(style_ref.padding_right)),
          Length::px(resolve0(style_ref.padding_top)),
          Length::px(resolve0(style_ref.padding_bottom)),
          style_ref.margin_left.map(|m| Length::px(resolve0(m))),
          style_ref.margin_right.map(|m| Length::px(resolve0(m))),
          style_ref.margin_top.map(|m| Length::px(resolve0(m))),
          style_ref.margin_bottom.map(|m| Length::px(resolve0(m))),
        )
      };
      s.padding_left = padding_left;
      s.padding_right = padding_right;
      s.padding_top = padding_top;
      s.padding_bottom = padding_bottom;
      s.margin_left = margin_left;
      s.margin_right = margin_right;
      s.margin_top = margin_top;
      s.margin_bottom = margin_bottom;

      // Percentage widths/heights behave as `auto` when the percentage base is unknown. The
      // definite inline constraint passed via `constraints` is only there to avoid 0px columns, not
      // to serve as a percentage base.
      if matches!(s.width, Some(len) if len.unit.is_percentage())
        || s
          .width_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.width = None;
        s.width_keyword = None;
      }
      if matches!(s.min_width, Some(len) if len.unit.is_percentage())
        || s
          .min_width_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.min_width = None;
        s.min_width_keyword = None;
      }
      if matches!(s.max_width, Some(len) if len.unit.is_percentage())
        || s
          .max_width_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.max_width = None;
        s.max_width_keyword = None;
      }
      if matches!(s.height, Some(len) if len.unit.is_percentage())
        || s
          .height_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.height = None;
        s.height_keyword = None;
      }
      if matches!(s.min_height, Some(len) if len.unit.is_percentage())
        || s
          .min_height_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.min_height = None;
        s.min_height_keyword = None;
      }
      if matches!(s.max_height, Some(len) if len.unit.is_percentage())
        || s
          .max_height_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.max_height = None;
        s.max_height_keyword = None;
      }
    }

    let mut probe_ctx = self.clone();
    probe_ctx.suppress_near_zero_width_viewport_fallback = true;
    let fragment = if box_node.id != 0 {
      crate::layout::style_override::with_style_override(box_node.id, probe_style, || {
        probe_ctx.layout(box_node, &constraints)
      })?
    } else {
      let mut cloned = box_node.clone();
      cloned.style = probe_style;
      probe_ctx.layout(&cloned, &constraints)?
    };
    let block_size = if inline_is_horizontal {
      fragment.bounds.height()
    } else {
      fragment.bounds.width()
    };
    intrinsic_block_cache_store(box_node, mode, block_size);
    Ok(block_size)
  }
}

fn convert_fragment_axes_in_place(
  fragment: &mut FragmentNode,
  parent_inline_size: f32,
  parent_block_size: f32,
  parent_writing_mode: WritingMode,
  parent_direction: crate::style::types::Direction,
) {
  // Fragment bounds are always expressed in the coordinate system of their parent fragment.
  // That coordinate system is determined by the parent's writing mode, not the child's.
  //
  // Writing-mode affects how a fragment lays out its *children*, but does not change how the
  // fragment's own border box is positioned within its parent.
  let parent_block_is_horizontal = block_axis_is_horizontal(parent_writing_mode);
  let parent_inline_positive = inline_axis_positive(parent_writing_mode, parent_direction);
  let parent_block_positive = block_axis_positive(parent_writing_mode);
  let parent_physical_width = if parent_block_is_horizontal {
    parent_block_size
  } else {
    parent_inline_size
  };
  let parent_physical_height = if parent_block_is_horizontal {
    parent_inline_size
  } else {
    parent_block_size
  };

  let logical_inline_start = fragment.bounds.x();
  let logical_block_start = fragment.bounds.y();
  let inline_size = fragment.bounds.width();
  let block_size = fragment.bounds.height();

  let (phys_x, phys_y, phys_w, phys_h) = if parent_block_is_horizontal {
    let phys_x = if parent_block_positive {
      logical_block_start
    } else {
      parent_physical_width - logical_block_start - block_size
    };
    let phys_y = if parent_inline_positive {
      logical_inline_start
    } else {
      parent_physical_height - logical_inline_start - inline_size
    };
    (phys_x, phys_y, block_size, inline_size)
  } else {
    let phys_x = if parent_inline_positive {
      logical_inline_start
    } else {
      parent_physical_width - logical_inline_start - inline_size
    };
    let phys_y = if parent_block_positive {
      logical_block_start
    } else {
      parent_physical_height - logical_block_start - block_size
    };
    (phys_x, phys_y, inline_size, block_size)
  };

  fragment.bounds = Rect::from_xywh(phys_x, phys_y, phys_w, phys_h);
  if let Some(logical) = fragment.logical_override {
    let logical_inline_start = logical.x();
    let logical_block_start = logical.y();
    let inline_size = logical.width();
    let block_size = logical.height();
    let (phys_x, phys_y, phys_w, phys_h) = if parent_block_is_horizontal {
      let phys_x = if parent_block_positive {
        logical_block_start
      } else {
        parent_physical_width - logical_block_start - block_size
      };
      let phys_y = if parent_inline_positive {
        logical_inline_start
      } else {
        parent_physical_height - logical_inline_start - inline_size
      };
      (phys_x, phys_y, block_size, inline_size)
    } else {
      let phys_x = if parent_inline_positive {
        logical_inline_start
      } else {
        parent_physical_width - logical_inline_start - inline_size
      };
      let phys_y = if parent_block_positive {
        logical_block_start
      } else {
        parent_physical_height - logical_block_start - block_size
      };
      (phys_x, phys_y, inline_size, block_size)
    };
    fragment.logical_override = Some(Rect::from_xywh(phys_x, phys_y, phys_w, phys_h));
  }

  let style_wm = fragment
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(parent_writing_mode);
  let dir = fragment
    .style
    .as_ref()
    .map(|s| s.direction)
    .unwrap_or(parent_direction);

  // Convert `scroll_overflow` from this fragment's local logical coordinate system to physical.
  // Unlike `bounds`, `scroll_overflow` is local to the fragment, so it must be converted using the
  // fragment's own writing mode.
  let phys_w = fragment.bounds.width();
  let phys_h = fragment.bounds.height();
  let child_inline = if block_axis_is_horizontal(style_wm) {
    phys_h
  } else {
    phys_w
  };
  let child_block = if block_axis_is_horizontal(style_wm) {
    phys_w
  } else {
    phys_h
  };
  fragment.scroll_overflow = logical_rect_to_physical(
    fragment.scroll_overflow,
    child_inline,
    child_block,
    style_wm,
    dir,
  );
  if block_axis_is_horizontal(style_wm) {
    // `ScrollbarReservation` is expressed in the fragment's local coordinate space. When converting
    // logical axes to physical, swap the edge contributions the same way `logical_rect_to_physical`
    // swaps width/height and x/y.
    let inline_positive = inline_axis_positive(style_wm, dir);
    let block_positive = block_axis_positive(style_wm);
    let logical = fragment.scrollbar_reservation;
    fragment.scrollbar_reservation = crate::tree::fragment_tree::ScrollbarReservation {
      // Physical x axis corresponds to the logical block axis in vertical writing modes.
      left: if block_positive {
        logical.top
      } else {
        logical.bottom
      },
      right: if block_positive {
        logical.bottom
      } else {
        logical.top
      },
      // Physical y axis corresponds to the logical inline axis.
      top: if inline_positive {
        logical.left
      } else {
        logical.right
      },
      bottom: if inline_positive {
        logical.right
      } else {
        logical.left
      },
    };
  }

  // Recurse into children using this fragment's writing mode (children are laid out in the
  // fragment's logical coordinate system).
  for child in fragment.children_mut() {
    convert_fragment_axes_in_place(child, child_inline, child_block, style_wm, dir);
  }
}

fn convert_fragment_axes(
  mut fragment: FragmentNode,
  parent_inline_size: f32,
  parent_block_size: f32,
  parent_writing_mode: WritingMode,
  parent_direction: crate::style::types::Direction,
) -> FragmentNode {
  convert_fragment_axes_in_place(
    &mut fragment,
    parent_inline_size,
    parent_block_size,
    parent_writing_mode,
    parent_direction,
  );
  fragment
}

fn unconvert_fragment_axes_in_place(
  fragment: &mut FragmentNode,
  parent_inline_size: f32,
  parent_block_size: f32,
  parent_writing_mode: WritingMode,
  parent_direction: crate::style::types::Direction,
) {
  // Inverse of `convert_fragment_axes`: map physical bounds back into the logical coordinate system
  // of the parent fragment.
  let parent_block_is_horizontal = block_axis_is_horizontal(parent_writing_mode);
  let parent_inline_positive = inline_axis_positive(parent_writing_mode, parent_direction);
  let parent_block_positive = block_axis_positive(parent_writing_mode);
  let parent_physical_width = if parent_block_is_horizontal {
    parent_block_size
  } else {
    parent_inline_size
  };
  let parent_physical_height = if parent_block_is_horizontal {
    parent_inline_size
  } else {
    parent_block_size
  };

  let phys_x = fragment.bounds.x();
  let phys_y = fragment.bounds.y();
  let phys_w = fragment.bounds.width();
  let phys_h = fragment.bounds.height();

  let inline_size = if parent_block_is_horizontal {
    phys_h
  } else {
    phys_w
  };
  let block_size = if parent_block_is_horizontal {
    phys_w
  } else {
    phys_h
  };

  let logical_block_start = if parent_block_is_horizontal {
    if parent_block_positive {
      phys_x
    } else {
      parent_physical_width - phys_x - block_size
    }
  } else if parent_block_positive {
    phys_y
  } else {
    parent_physical_height - phys_y - block_size
  };

  let logical_inline_start = if parent_block_is_horizontal {
    if parent_inline_positive {
      phys_y
    } else {
      parent_physical_height - phys_y - inline_size
    }
  } else if parent_inline_positive {
    phys_x
  } else {
    parent_physical_width - phys_x - inline_size
  };

  fragment.bounds = Rect::from_xywh(
    logical_inline_start,
    logical_block_start,
    inline_size,
    block_size,
  );
  if let Some(logical) = fragment.logical_override {
    let phys_x = logical.x();
    let phys_y = logical.y();
    let phys_w = logical.width();
    let phys_h = logical.height();
    let inline_size = if parent_block_is_horizontal {
      phys_h
    } else {
      phys_w
    };
    let block_size = if parent_block_is_horizontal {
      phys_w
    } else {
      phys_h
    };
    let logical_block_start = if parent_block_is_horizontal {
      if parent_block_positive {
        phys_x
      } else {
        parent_physical_width - phys_x - block_size
      }
    } else if parent_block_positive {
      phys_y
    } else {
      parent_physical_height - phys_y - block_size
    };
    let logical_inline_start = if parent_block_is_horizontal {
      if parent_inline_positive {
        phys_y
      } else {
        parent_physical_height - phys_y - inline_size
      }
    } else if parent_inline_positive {
      phys_x
    } else {
      parent_physical_width - phys_x - inline_size
    };
    fragment.logical_override = Some(Rect::from_xywh(
      logical_inline_start,
      logical_block_start,
      inline_size,
      block_size,
    ));
  }

  let style_wm = fragment
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(parent_writing_mode);
  let dir = fragment
    .style
    .as_ref()
    .map(|s| s.direction)
    .unwrap_or(parent_direction);
  let child_inline = if block_axis_is_horizontal(style_wm) {
    phys_h
  } else {
    phys_w
  };
  let child_block = if block_axis_is_horizontal(style_wm) {
    phys_w
  } else {
    phys_h
  };
  fragment.scroll_overflow = physical_rect_to_logical(
    fragment.scroll_overflow,
    child_inline,
    child_block,
    style_wm,
    dir,
  );
  if block_axis_is_horizontal(style_wm) {
    // Invert the edge swapping performed in `convert_fragment_axes_in_place`.
    let inline_positive = inline_axis_positive(style_wm, dir);
    let block_positive = block_axis_positive(style_wm);
    let physical = fragment.scrollbar_reservation;
    fragment.scrollbar_reservation = crate::tree::fragment_tree::ScrollbarReservation {
      // Logical inline axis maps to physical y.
      left: if inline_positive {
        physical.top
      } else {
        physical.bottom
      },
      right: if inline_positive {
        physical.bottom
      } else {
        physical.top
      },
      // Logical block axis maps to physical x.
      top: if block_positive {
        physical.left
      } else {
        physical.right
      },
      bottom: if block_positive {
        physical.right
      } else {
        physical.left
      },
    };
  }
  for child in fragment.children_mut() {
    unconvert_fragment_axes_in_place(child, child_inline, child_block, style_wm, dir);
  }
}

fn unconvert_fragment_axes(
  mut fragment: FragmentNode,
  parent_inline_size: f32,
  parent_block_size: f32,
  parent_writing_mode: WritingMode,
  parent_direction: crate::style::types::Direction,
) -> FragmentNode {
  unconvert_fragment_axes_in_place(
    &mut fragment,
    parent_inline_size,
    parent_block_size,
    parent_writing_mode,
    parent_direction,
  );
  fragment
}

/// Converts a fragment subtree produced in physical coordinates back into logical coordinates.
///
/// Block layout produces fragments in a logical coordinate system (inline axis = X, block axis = Y)
/// and then converts them to physical coordinates based on `writing-mode`. Some layout algorithms
/// (notably tables) need to embed the result back into an unconverted parent flow. This helper
/// inverts the axis conversion for such call sites.
pub(crate) fn unconvert_fragment_axes_root(fragment: FragmentNode) -> FragmentNode {
  let style_wm = fragment
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  let dir = fragment
    .style
    .as_ref()
    .map(|s| s.direction)
    .unwrap_or(crate::style::types::Direction::Ltr);
  let (inline_size, block_size) = if block_axis_is_horizontal(style_wm) {
    // Physical width/height correspond to block/inline sizes respectively.
    (fragment.bounds.height(), fragment.bounds.width())
  } else {
    (fragment.bounds.width(), fragment.bounds.height())
  };
  unconvert_fragment_axes(fragment, inline_size, block_size, style_wm, dir)
}

/// Converts a fragment subtree produced in logical coordinates into physical coordinates.
///
/// Formatting contexts generally produce fragments in a logical coordinate system (inline axis = X,
/// block axis = Y) and then convert them to physical coordinates based on `writing-mode`. Some
/// layout algorithms (notably inline layout when embedding an inline-block fragment) need to round
/// trip between coordinate spaces; this helper applies the forward conversion starting at the
/// fragment root.
pub(crate) fn convert_fragment_axes_root(fragment: FragmentNode) -> FragmentNode {
  let style_wm = fragment
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  let dir = fragment
    .style
    .as_ref()
    .map(|s| s.direction)
    .unwrap_or(crate::style::types::Direction::Ltr);
  let inline_size = fragment.bounds.width();
  let block_size = fragment.bounds.height();
  convert_fragment_axes(fragment, inline_size, block_size, style_wm, dir)
}

fn logical_rect_to_physical_full(
  rect: Rect,
  parent_inline_size: f32,
  parent_block_size: f32,
  writing_mode: WritingMode,
  direction: crate::style::types::Direction,
) -> Rect {
  // Equivalent to the bounds conversion performed by `convert_fragment_axes_in_place`, but for
  // standalone rectangles.
  let parent_block_is_horizontal = block_axis_is_horizontal(writing_mode);
  let parent_inline_positive = inline_axis_positive(writing_mode, direction);
  let parent_block_positive = block_axis_positive(writing_mode);
  let parent_physical_width = if parent_block_is_horizontal {
    parent_block_size
  } else {
    parent_inline_size
  };
  let parent_physical_height = if parent_block_is_horizontal {
    parent_inline_size
  } else {
    parent_block_size
  };

  let logical_inline_start = rect.x();
  let logical_block_start = rect.y();
  let inline_size = rect.width();
  let block_size = rect.height();

  if parent_block_is_horizontal {
    let phys_x = if parent_block_positive {
      logical_block_start
    } else {
      parent_physical_width - logical_block_start - block_size
    };
    let phys_y = if parent_inline_positive {
      logical_inline_start
    } else {
      parent_physical_height - logical_inline_start - inline_size
    };
    Rect::from_xywh(phys_x, phys_y, block_size, inline_size)
  } else {
    let phys_x = if parent_inline_positive {
      logical_inline_start
    } else {
      parent_physical_width - logical_inline_start - inline_size
    };
    let phys_y = if parent_block_positive {
      logical_block_start
    } else {
      parent_physical_height - logical_block_start - block_size
    };
    Rect::from_xywh(phys_x, phys_y, inline_size, block_size)
  }
}

fn physical_rect_to_logical_full(
  rect: Rect,
  parent_inline_size: f32,
  parent_block_size: f32,
  writing_mode: WritingMode,
  direction: crate::style::types::Direction,
) -> Rect {
  // Inverse of `logical_rect_to_physical_full`.
  let parent_block_is_horizontal = block_axis_is_horizontal(writing_mode);
  let parent_inline_positive = inline_axis_positive(writing_mode, direction);
  let parent_block_positive = block_axis_positive(writing_mode);
  let parent_physical_width = if parent_block_is_horizontal {
    parent_block_size
  } else {
    parent_inline_size
  };
  let parent_physical_height = if parent_block_is_horizontal {
    parent_inline_size
  } else {
    parent_block_size
  };

  if parent_block_is_horizontal {
    // Physical x/y correspond to the logical block/inline axes respectively.
    let block_size = rect.width();
    let inline_size = rect.height();

    let logical_block_start = if parent_block_positive {
      rect.x()
    } else {
      parent_physical_width - rect.x() - block_size
    };
    let logical_inline_start = if parent_inline_positive {
      rect.y()
    } else {
      parent_physical_height - rect.y() - inline_size
    };

    Rect::from_xywh(
      logical_inline_start,
      logical_block_start,
      inline_size,
      block_size,
    )
  } else {
    let inline_size = rect.width();
    let block_size = rect.height();

    let logical_inline_start = if parent_inline_positive {
      rect.x()
    } else {
      parent_physical_width - rect.x() - inline_size
    };
    let logical_block_start = if parent_block_positive {
      rect.y()
    } else {
      parent_physical_height - rect.y() - block_size
    };

    Rect::from_xywh(
      logical_inline_start,
      logical_block_start,
      inline_size,
      block_size,
    )
  }
}

pub(crate) fn logical_rect_to_physical(
  rect: Rect,
  parent_inline_size: f32,
  parent_block_size: f32,
  writing_mode: WritingMode,
  direction: crate::style::types::Direction,
) -> Rect {
  if !block_axis_is_horizontal(writing_mode) {
    return rect;
  }
  let inline_positive = inline_axis_positive(writing_mode, direction);
  let block_positive = block_axis_positive(writing_mode);

  let logical_inline_start = rect.x();
  let logical_block_start = rect.y();
  let inline_size = rect.width();
  let block_size = rect.height();

  let phys_x = if block_positive {
    logical_block_start
  } else {
    parent_block_size - logical_block_start - block_size
  };
  let phys_y = if inline_positive {
    logical_inline_start
  } else {
    parent_inline_size - logical_inline_start - inline_size
  };

  Rect::from_xywh(phys_x, phys_y, block_size, inline_size)
}

pub(crate) fn physical_rect_to_logical(
  rect: Rect,
  parent_inline_size: f32,
  parent_block_size: f32,
  writing_mode: WritingMode,
  direction: crate::style::types::Direction,
) -> Rect {
  if !block_axis_is_horizontal(writing_mode) {
    return rect;
  }
  let inline_positive = inline_axis_positive(writing_mode, direction);
  let block_positive = block_axis_positive(writing_mode);

  let block_size = rect.width();
  let inline_size = rect.height();

  let logical_block_start = if block_positive {
    rect.x()
  } else {
    parent_block_size - rect.x() - block_size
  };
  let logical_inline_start = if inline_positive {
    rect.y()
  } else {
    parent_inline_size - rect.y() - inline_size
  };

  Rect::from_xywh(
    logical_inline_start,
    logical_block_start,
    inline_size,
    block_size,
  )
}
/// Checks if a box is out of normal flow (absolute/fixed positioned or float)
fn is_out_of_flow(box_node: &BoxNode) -> bool {
  let position = box_node.style.position;
  box_node.style.running_position.is_some()
    || matches!(position, Position::Absolute | Position::Fixed)
}

fn count_text_fragments(fragment: &FragmentNode) -> (usize, usize) {
  fn walk(node: &FragmentNode, text: &mut usize, total: &mut usize) {
    *total += 1;
    if matches!(node.content, FragmentContent::Text { .. }) {
      *text += 1;
    }
    for child in node.children.iter() {
      walk(child, text, total);
    }
  }

  let mut text = 0;
  let mut total = 0;
  walk(fragment, &mut text, &mut total);
  (text, total)
}

fn collect_first_texts(fragment: &FragmentNode, out: &mut Vec<String>, limit: usize) {
  fn walk(node: &FragmentNode, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
      return;
    }
    if let FragmentContent::Text { text, .. } = &node.content {
      out.push(text.to_string());
      if out.len() >= limit {
        return;
      }
    }
    for child in node.children.iter() {
      walk(child, out, limit);
      if out.len() >= limit {
        return;
      }
    }
  }

  walk(fragment, out, limit);
}

fn trace_positioned_ids() -> Vec<usize> {
  crate::debug::runtime::runtime_toggles()
    .usize_list("FASTR_TRACE_POSITIONED")
    .unwrap_or_default()
}

fn trace_block_text_ids() -> Vec<usize> {
  crate::debug::runtime::runtime_toggles()
    .usize_list("FASTR_TRACE_BLOCK_TEXT")
    .unwrap_or_default()
}

fn resolve_length_for_width(
  length: Length,
  percentage_base: f32,
  style: &ComputedStyle,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  let base = if percentage_base.is_finite() {
    Some(percentage_base)
  } else {
    None
  };
  resolve_length_with_percentage_metrics_and_root_font_metrics(
    length,
    base,
    viewport,
    style.font_size,
    style.root_font_size,
    Some(style),
    Some(font_context),
    root_font_metrics,
  )
  .unwrap_or(0.0)
}

fn horizontal_padding_and_borders(
  style: &ComputedStyle,
  percentage_base: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  let mut total = resolve_length_for_width(
    style.padding_left,
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  ) + resolve_length_for_width(
    style.padding_right,
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  ) + resolve_length_for_width(
    style.used_border_left_width(),
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  ) + resolve_length_for_width(
    style.used_border_right_width(),
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  );

  let reserve_vertical_gutter = style.scrollbar_gutter.stable
    && matches!(
      style.overflow_y,
      Overflow::Hidden | Overflow::Auto | Overflow::Scroll
    );
  if reserve_vertical_gutter {
    let gutter = resolve_scrollbar_width(style);
    if gutter > 0.0 {
      total += gutter;
      if style.scrollbar_gutter.both_edges {
        total += gutter;
      }
    }
  }

  total
}

fn vertical_padding_and_borders(
  style: &ComputedStyle,
  percentage_base: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  let mut total = resolve_length_for_width(
    style.padding_top,
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  ) + resolve_length_for_width(
    style.padding_bottom,
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  ) + resolve_length_for_width(
    style.used_border_top_width(),
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  ) + resolve_length_for_width(
    style.used_border_bottom_width(),
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  );

  let reserve_horizontal_gutter = style.scrollbar_gutter.stable
    && matches!(
      style.overflow_x,
      Overflow::Hidden | Overflow::Auto | Overflow::Scroll
    );
  if reserve_horizontal_gutter {
    let gutter = resolve_scrollbar_width(style);
    if gutter > 0.0 {
      total += gutter;
      if style.scrollbar_gutter.both_edges {
        total += gutter;
      }
    }
  }

  total
}

fn inline_axis_padding_and_borders(
  style: &ComputedStyle,
  percentage_base: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  if inline_axis_is_horizontal(style.writing_mode) {
    horizontal_padding_and_borders(
      style,
      percentage_base,
      viewport,
      font_context,
      root_font_metrics,
    )
  } else {
    vertical_padding_and_borders(
      style,
      percentage_base,
      viewport,
      font_context,
      root_font_metrics,
    )
  }
}

fn rebase_intrinsic_border_box_size(base: f32, edges_base: f32, edges_actual: f32) -> f32 {
  (base - edges_base + edges_actual).max(0.0)
}

fn compute_intrinsic_block_sizes_without_block_size_constraints(
  fc: &dyn FormattingContext,
  box_node: &BoxNode,
) -> Result<(f32, f32), LayoutError> {
  let style_override = crate::layout::style_override::style_override_for(box_node.id);
  let style = style_override.as_ref().unwrap_or(&box_node.style);
  let mut probe_style = style.clone();
  {
    let s = Arc::make_mut(&mut probe_style);
    if block_axis_is_horizontal(style.writing_mode) {
      s.width = None;
      s.width_keyword = None;
      s.min_width = None;
      s.min_width_keyword = None;
      s.max_width = None;
      s.max_width_keyword = None;
    } else {
      s.height = None;
      s.height_keyword = None;
      s.min_height = None;
      s.min_height_keyword = None;
      s.max_height = None;
      s.max_height_keyword = None;
    }
  }

  let compute = |node: &BoxNode| -> Result<(f32, f32), LayoutError> {
    let min = match fc.compute_intrinsic_block_size(node, IntrinsicSizingMode::MinContent) {
      Ok(value) => value,
      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
      Err(_) => 0.0,
    };
    let max = match fc.compute_intrinsic_block_size(node, IntrinsicSizingMode::MaxContent) {
      Ok(value) => value,
      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
      Err(_) => min,
    };
    Ok((min, max))
  };

  if box_node.id != 0 {
    crate::layout::style_override::with_style_override(box_node.id, probe_style, || {
      compute(box_node)
    })
  } else {
    let mut cloned = box_node.clone();
    cloned.style = probe_style;
    compute(&cloned)
  }
}

fn recompute_margins_for_width(
  style: &ComputedStyle,
  containing_width: f32,
  content_width: f32,
  border_left: f32,
  padding_left: f32,
  padding_right: f32,
  border_right: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
  root_font_metrics: Option<RootFontMetrics>,
) -> (f32, f32) {
  let margin_left = match &style.margin_left {
    Some(len) => MarginValue::Length(resolve_length_for_width(
      *len,
      containing_width,
      style,
      font_context,
      viewport,
      root_font_metrics,
    )),
    None => MarginValue::Auto,
  };
  let margin_right = match &style.margin_right {
    Some(len) => MarginValue::Length(resolve_length_for_width(
      *len,
      containing_width,
      style,
      font_context,
      viewport,
      root_font_metrics,
    )),
    None => MarginValue::Auto,
  };

  let borders_and_padding = border_left + padding_left + padding_right + border_right;

  match (margin_left, margin_right) {
    (MarginValue::Auto, MarginValue::Auto) => {
      let remaining = containing_width - borders_and_padding - content_width;
      let margin = (remaining / 2.0).max(0.0);
      (margin, margin)
    }
    (MarginValue::Auto, MarginValue::Length(mr)) => {
      let ml = containing_width - borders_and_padding - content_width - mr;
      (ml, mr)
    }
    (MarginValue::Length(ml), MarginValue::Auto) => {
      let mr = containing_width - borders_and_padding - content_width - ml;
      (ml, mr)
    }
    (MarginValue::Length(ml), MarginValue::Length(_mr)) => {
      let mr = containing_width - borders_and_padding - content_width - ml;
      (ml, mr)
    }
  }
}
fn resolve_margin_side(
  style: &ComputedStyle,
  side: PhysicalSide,
  percentage_base: f32,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  let length = match side {
    PhysicalSide::Top => style.margin_top,
    PhysicalSide::Right => style.margin_right,
    PhysicalSide::Bottom => style.margin_bottom,
    PhysicalSide::Left => style.margin_left,
  };
  length
    .map(|l| {
      resolve_length_for_width(
        l,
        percentage_base,
        style,
        font_context,
        viewport,
        root_font_metrics,
      )
    })
    .unwrap_or(0.0)
}

fn resolve_padding_side(
  style: &ComputedStyle,
  side: PhysicalSide,
  percentage_base: f32,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  let length = match side {
    PhysicalSide::Top => style.padding_top,
    PhysicalSide::Right => style.padding_right,
    PhysicalSide::Bottom => style.padding_bottom,
    PhysicalSide::Left => style.padding_left,
  };
  resolve_length_for_width(
    length,
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  )
}

fn resolve_border_side(
  style: &ComputedStyle,
  side: PhysicalSide,
  percentage_base: f32,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
  root_font_metrics: Option<RootFontMetrics>,
) -> f32 {
  let length = match side {
    PhysicalSide::Top => style.used_border_top_width(),
    PhysicalSide::Right => style.used_border_right_width(),
    PhysicalSide::Bottom => style.used_border_bottom_width(),
    PhysicalSide::Left => style.used_border_left_width(),
  };
  resolve_length_for_width(
    length,
    percentage_base,
    style,
    font_context,
    viewport,
    root_font_metrics,
  )
}
#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::Transform;
  use crate::debug::runtime;
  use crate::layout::contexts::inline::InlineFormattingContext;
  use crate::layout::formatting_context::IntrinsicSizingMode;
  use crate::style::display::Display;
  use crate::style::display::FormattingContextType;
  use crate::style::float::Float;
  use crate::style::position::Position;
  use crate::style::types::BorderStyle;
  use crate::style::types::ContainIntrinsicSizeAxis;
  use crate::style::types::ContentVisibility;
  use crate::style::types::FlexDirection;
  use crate::style::types::IntrinsicSizeKeyword;
  use crate::style::types::ListStylePosition;
  use crate::style::types::ListStyleType;
  use crate::style::types::Overflow;
  use crate::style::types::ScrollbarWidth;
  use crate::style::types::WritingMode;
  use crate::style::values::Length;
  use crate::style::ComputedStyle;
  use crate::text::font_loader::FontContext;
  use crate::tree::box_generation_demo::BoxGenerator;
  use crate::tree::box_generation_demo::DOMNode;
  use crate::tree::box_tree::BoxTree;
  use crate::tree::fragment_tree::FragmentContent;
  use crate::tree::fragment_tree::FragmentNode;
  use std::collections::HashMap;
  use std::sync::Arc;

  fn default_style() -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    Arc::new(style)
  }

  #[test]
  fn non_ascii_whitespace_block_trim_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert!(
      !trim_ascii_whitespace(nbsp).is_empty(),
      "NBSP must not be treated as collapsible ASCII whitespace"
    );
  }

  fn block_style_with_height(height: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.height = Some(Length::px(height));
    style.height_keyword = None;
    Arc::new(style)
  }

  #[test]
  fn margin_collapse_through_clear_without_floats() {
    let bfc = BlockFormattingContext::new();

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut clear_style = ComputedStyle::default();
    clear_style.display = Display::Block;
    clear_style.clear = Clear::Both;

    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![BoxNode::new_block(
        Arc::new(clear_style),
        FormattingContextType::Block,
        vec![],
      )],
    );

    let margins = bfc.collapsed_block_margins(&parent, 800.0, CollapsedMarginMode::Normal);
    assert!(
      margins.collapsible_through,
      "A clear-only empty block should still be collapsible-through when it does not follow a float"
    );
  }

  #[test]
  fn margin_collapse_through_clear_after_float_breaks() {
    let bfc = BlockFormattingContext::new();

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;

    let mut clear_style = ComputedStyle::default();
    clear_style.display = Display::Block;
    clear_style.clear = Clear::Both;

    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![
        BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]),
        BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]),
      ],
    );

    let margins = bfc.collapsed_block_margins(&parent, 800.0, CollapsedMarginMode::Normal);
    assert!(
      !margins.collapsible_through,
      "A block that contains a float and a later clearing block should not be treated as collapsible-through"
    );
  }

  #[test]
  fn margin_collapse_first_child_does_not_tunnel_past_float_in_collapsible_through_subtree() {
    let bfc = BlockFormattingContext::new();

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut spacer_style = ComputedStyle::default();
    spacer_style.display = Display::Block;
    spacer_style.margin_top = Some(Length::px(15.0));
    spacer_style.margin_bottom = Some(Length::px(15.0));

    let mut float_wrapper_style = ComputedStyle::default();
    float_wrapper_style.display = Display::Block;

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;

    let mut clear_style = ComputedStyle::default();
    clear_style.display = Display::Block;
    clear_style.clear = Clear::Both;
    clear_style.margin_top = Some(Length::px(20.0));

    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = 2;

    let mut float_child =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
    float_child.id = 4;

    let mut float_wrapper = BoxNode::new_block(
      Arc::new(float_wrapper_style),
      FormattingContextType::Block,
      vec![float_child],
    );
    float_wrapper.id = 3;

    let mut clear_block =
      BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]);
    clear_block.id = 5;

    let mut parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![spacer, float_wrapper, clear_block],
    );
    parent.id = 1_000;

    let margins = bfc.collapsed_block_margins(&parent, 800.0, CollapsedMarginMode::Normal);
    assert!(
      (margins.top.resolve() - 15.0).abs() < 0.01,
      "expected margin collapsing to stop before the cleared block when a float exists inside a collapsible-through subtree; got {}",
      margins.top.resolve()
    );
  }

  #[test]
  fn block_auto_height_respects_negative_trailing_margins() {
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    // Prevent parent/last-child margin collapsing so the child's negative bottom margin affects the
    // parent's used height.
    parent_style.border_bottom_style = BorderStyle::Solid;
    parent_style.border_bottom_width = Length::px(1.0);
    let parent_style = Arc::new(parent_style);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(50.0));
    child_style.height_keyword = None;
    child_style.margin_bottom = Some(Length::px(-8.0));
    let child_style = Arc::new(child_style);

    let mut child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);
    child.id = 3;

    let mut parent = BoxNode::new_block(parent_style, FormattingContextType::Block, vec![child]);
    parent.id = 2;

    let mut root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![parent]);
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let parent_fragment = find_block_fragment(&fragment, 2).expect("parent fragment");
    assert!(
      (parent_fragment.bounds.height() - 43.0).abs() < 0.5,
      "expected parent border-box height ≈43px (50px child, -8px margin, 1px border), got {:.2}",
      parent_fragment.bounds.height()
    );
  }

  #[test]
  fn collapsed_block_margins_avoids_exponential_recursion_for_empty_block_chains() {
    reset_collapsed_block_margins_call_tracking();
    set_collapsed_block_margins_call_limit(Some(10_000));

    let bfc = BlockFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.margin_top = Some(Length::px(10.0));
    style.margin_bottom = Some(Length::px(20.0));
    let style = Arc::new(style);

    let mut node = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    for _ in 0..256usize {
      node = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![node]);
    }

    let margins = bfc.collapsed_block_margins(&node, 800.0, CollapsedMarginMode::Normal);
    assert!(margins.collapsible_through);
    assert!(
      collapsed_block_margins_calls() < 5_000,
      "expected collapsed_block_margins to be cached, calls={}",
      collapsed_block_margins_calls()
    );

    reset_collapsed_block_margins_call_tracking();
  }

  #[test]
  fn floats_use_child_formatting_context_for_layout() {
    let bfc = BlockFormattingContext::new();

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.width = Some(Length::px(200.0));
    parent_style.height = Some(Length::px(200.0));
    parent_style.width_keyword = None;
    parent_style.height_keyword = None;

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Flex;
    float_style.float = Float::Left;
    float_style.flex_direction = FlexDirection::Column;
    float_style.width = Some(Length::px(100.0));
    float_style.height = Some(Length::px(100.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;

    let mut child_a_style = ComputedStyle::default();
    child_a_style.width = Some(Length::px(100.0));
    child_a_style.height = Some(Length::px(20.0));
    child_a_style.width_keyword = None;
    child_a_style.height_keyword = None;
    child_a_style.order = 2;

    let mut child_b_style = ComputedStyle::default();
    child_b_style.width = Some(Length::px(100.0));
    child_b_style.height = Some(Length::px(30.0));
    child_b_style.width_keyword = None;
    child_b_style.height_keyword = None;
    child_b_style.order = 0;

    let mut child_a = BoxNode::new_block(
      Arc::new(child_a_style),
      FormattingContextType::Block,
      vec![],
    );
    child_a.id = 3;
    let mut child_b = BoxNode::new_block(
      Arc::new(child_b_style),
      FormattingContextType::Block,
      vec![],
    );
    child_b.id = 4;

    let mut float_box = BoxNode::new_block(
      Arc::new(float_style),
      FormattingContextType::Flex,
      vec![child_a, child_b],
    );
    float_box.id = 2;

    let mut parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![float_box],
    );
    parent.id = 1;

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = bfc
      .layout(&parent, &constraints)
      .expect("layout should succeed");

    let float_fragment = find_block_fragment(&fragment, 2).expect("float fragment");
    let ordered_children = float_fragment
      .children
      .iter()
      .filter_map(|child| match &child.content {
        FragmentContent::Block { box_id: Some(id) } => Some(*id),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(
      ordered_children,
      vec![4, 3],
      "expected floated flex container to emit children in `order`-sorted fragment order",
    );

    let frag_b = find_block_fragment(float_fragment, 4).expect("child B fragment");
    let frag_a = find_block_fragment(float_fragment, 3).expect("child A fragment");
    assert!(
      frag_b.bounds.y() <= frag_a.bounds.y(),
      "expected lower-order item to appear first along the flex container main axis",
    );
  }
  fn content_visibility_test_guard() -> runtime::ThreadRuntimeTogglesGuard {
    // Keep content-visibility:auto tests deterministic even when developers have FASTR_* env vars
    // set locally (e.g. activation margin experiments).
    runtime::set_thread_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(HashMap::from(
      [(
        "FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX".to_string(),
        "0".to_string(),
      )],
    ))))
  }

  fn inline_canvas(id: usize, width: f32, height: f32) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    let mut node = BoxNode::new_replaced(
      Arc::new(style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(width, height)),
      None,
    );
    node.id = id;
    node
  }

  #[test]
  fn width_max_content_rebases_percent_padding_and_borders() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width_keyword = Some(crate::style::types::IntrinsicSizeKeyword::MaxContent);
    child_style.padding_left = Length::percent(10.0);
    child_style.padding_right = Length::px(5.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.margin_left = Some(Length::px(0.0));
    child_style.margin_right = Some(Length::px(0.0));

    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let intrinsic_max_base0 = fc
      .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MaxContent)
      .unwrap();
    let edges_base0 =
      inline_axis_padding_and_borders(&child_style, 0.0, viewport, &font_context, None);
    let edges_actual =
      inline_axis_padding_and_borders(&child_style, 300.0, viewport, &font_context, None);
    let expected_border_box =
      rebase_intrinsic_border_box_size(intrinsic_max_base0, edges_base0, edges_actual);

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - expected_border_box).abs() < 0.5,
      "expected border-box width {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.width()
    );
  }

  #[test]
  fn width_fit_content_function_clamps_between_min_and_max_content() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width_keyword = Some(crate::style::types::IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::px(50.0)),
    });
    child_style.padding_left = Length::percent(10.0);
    child_style.padding_right = Length::px(5.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.margin_left = Some(Length::px(0.0));
    child_style.margin_right = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "word ".repeat(20));
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let (min_base0, max_base0) = fc.compute_intrinsic_inline_sizes(&child).unwrap();
    let edges_base0 =
      inline_axis_padding_and_borders(&child_style, 0.0, viewport, &font_context, None);
    let edges_actual =
      inline_axis_padding_and_borders(&child_style, 300.0, viewport, &font_context, None);
    let intrinsic_min = rebase_intrinsic_border_box_size(min_base0, edges_base0, edges_actual);
    let intrinsic_max = rebase_intrinsic_border_box_size(max_base0, edges_base0, edges_actual);
    let limit_border = border_size_from_box_sizing(50.0, edges_actual, child_style.box_sizing);
    let expected_border_box = intrinsic_max.min(intrinsic_min.max(limit_border));

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - expected_border_box).abs() < 0.5,
      "expected fit-content border-box width {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.width()
    );
  }

  #[test]
  fn max_width_fit_content_caps_auto_width_blocks() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.max_width_keyword =
      Some(crate::style::types::IntrinsicSizeKeyword::FitContent { limit: None });
    child_style.padding_left = Length::percent(10.0);
    child_style.padding_right = Length::px(5.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.margin_left = Some(Length::px(0.0));
    child_style.margin_right = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "short".into());
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let (min_base0, max_base0) = fc.compute_intrinsic_inline_sizes(&child).unwrap();
    let edges_base0 =
      inline_axis_padding_and_borders(&child_style, 0.0, viewport, &font_context, None);
    let edges_actual =
      inline_axis_padding_and_borders(&child_style, 300.0, viewport, &font_context, None);
    let intrinsic_min = rebase_intrinsic_border_box_size(min_base0, edges_base0, edges_actual);
    let intrinsic_max = rebase_intrinsic_border_box_size(max_base0, edges_base0, edges_actual);
    let expected_border_box = intrinsic_max.min(300.0_f32.max(intrinsic_min));

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - expected_border_box).abs() < 0.5,
      "expected capped border-box width {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.width()
    );
    assert!(
      child_frag.bounds.width() < 299.0,
      "expected max-width:fit-content to shrink below the containing width; got {:.2}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn max_width_fit_content_clamps_explicit_width_against_available_space() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut base_style = ComputedStyle::default();
    base_style.display = Display::Block;
    base_style.margin_left = Some(Length::px(0.0));
    base_style.margin_right = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "hello world goodbye".into());
    let inline = BoxNode::new_inline(default_style(), vec![text]);

    let intrinsic_child = BoxNode::new_block(
      Arc::new(base_style.clone()),
      FormattingContextType::Block,
      vec![inline.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let (min_border, max_border) = fc.compute_intrinsic_inline_sizes(&intrinsic_child).unwrap();
    assert!(
      min_border + 0.5 < 80.0 && max_border > 80.0 + 0.5,
      "expected intrinsic widths to straddle 80px (min={min_border:.2}, max={max_border:.2})",
    );

    let mut child_style = base_style;
    child_style.width = Some(Length::px(500.0));
    child_style.width_keyword = None;
    child_style.max_width = None;
    child_style.max_width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

    let child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let fragment = fc
      .layout(&parent, &LayoutConstraints::definite_width(80.0))
      .unwrap();
    assert!(
      (fragment.bounds.width() - 80.0).abs() < 0.5,
      "expected parent width to be 80px, got {:.2}",
      fragment.bounds.width()
    );
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected max-width:fit-content to clamp explicit width to 80px, got {:.2}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn height_max_content_rebases_percent_padding_and_borders() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height_keyword = Some(crate::style::types::IntrinsicSizeKeyword::MaxContent);
    // Percentage vertical padding uses the containing block inline size as its base (CSS2.1 10.5),
    // which is also the key edge case for intrinsic size rebasing.
    child_style.padding_top = Length::percent(10.0);
    child_style.padding_bottom = Length::px(5.0);
    child_style.border_top_width = Length::px(2.0);
    child_style.border_bottom_width = Length::px(2.0);
    child_style.margin_top = Some(Length::px(0.0));
    child_style.margin_bottom = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "word ".repeat(40));
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let (_min_base0, max_base0) =
      compute_intrinsic_block_sizes_without_block_size_constraints(&fc, &child).unwrap();
    let edges_base0 =
      vertical_padding_and_borders(&child_style, 0.0, viewport, &font_context, None);
    let edges_actual =
      vertical_padding_and_borders(&child_style, 300.0, viewport, &font_context, None);
    let expected_border_box =
      rebase_intrinsic_border_box_size(max_base0, edges_base0, edges_actual);

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.height() - expected_border_box).abs() < 0.5,
      "expected border-box height {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.height()
    );
  }

  #[test]
  fn max_width_clamps_and_centers_in_flow_blocks() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(20.0));
    child_style.box_sizing = crate::style::types::BoxSizing::BorderBox;
    child_style.padding_left = Length::px(10.0);
    child_style.padding_right = Length::px(10.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.max_width = Some(Length::px(200.0));
    child_style.margin_left = None;
    child_style.margin_right = None;

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let viewport = Size::new(500.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(500.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);

    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 200.0).abs() < 0.5,
      "expected border-box width 200, got {}",
      child_frag.bounds.width()
    );
    assert!(
      (child_frag.bounds.x() - 150.0).abs() < 0.5,
      "expected centered x=150, got {}",
      child_frag.bounds.x()
    );
  }

  #[test]
  fn block_width_max_content_keyword_uses_intrinsic_inline_size() {
    let child_canvas = inline_canvas(9101, 80.0, 20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = None;
    child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let mut child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    child.id = 9100;

    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected max-content width 80, got {}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn root_width_max_content_keyword_uses_intrinsic_inline_size() {
    let child_canvas = inline_canvas(9401, 80.0, 20.0);

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = None;
    root_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();
    assert!(
      (fragment.bounds.width() - 80.0).abs() < 0.5,
      "expected root max-content width 80, got {}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn block_max_width_max_content_keyword_clamps_width() {
    let child_canvas = inline_canvas(9201, 80.0, 20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = Some(Length::px(150.0));
    child_style.width_keyword = None;
    child_style.max_width = None;
    child_style.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let mut child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    child.id = 9200;

    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected max-width:max-content to clamp to 80, got {}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn block_min_width_max_content_keyword_clamps_width() {
    let child_canvas = inline_canvas(9301, 80.0, 20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = Some(Length::px(50.0));
    child_style.width_keyword = None;
    child_style.min_width = None;
    child_style.min_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let mut child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    child.id = 9300;

    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected min-width:max-content to expand to 80, got {}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn percent_heights_resolve_with_used_height_override() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_top = Length::px(10.0);
    parent_style.padding_bottom = Length::px(10.0);
    parent_style.border_top_style = BorderStyle::Solid;
    parent_style.border_bottom_style = BorderStyle::Solid;
    parent_style.border_top_width = Length::px(5.0);
    parent_style.border_bottom_width = Length::px(5.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::percent(50.0));
    child_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let viewport = Size::new(300.0, 300.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(300.0), AvailableSpace::Indefinite)
        .with_used_border_box_size(None, Some(200.0));

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert!(
      (fragment.bounds.height() - 200.0).abs() < 0.5,
      "expected parent border-box height 200, got {}",
      fragment.bounds.height()
    );
    assert_eq!(fragment.children.len(), 1);

    // Parent vertical edges = 5 + 10 + 10 + 5 = 30px, so the used content height is 170px.
    // The child has height:50%, so it should resolve to 85px.
    assert!(
      (fragment.children[0].bounds.height() - 85.0).abs() < 0.5,
      "expected child height ~85, got {}",
      fragment.children[0].bounds.height()
    );
  }

  #[test]
  fn block_children_are_offset_by_parent_padding_and_border() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(20.0);
    parent_style.border_left_style = BorderStyle::Solid;
    parent_style.border_top_style = BorderStyle::Solid;
    parent_style.border_left_width = Length::px(5.0);
    parent_style.border_top_width = Length::px(2.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(10.0));

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child = &fragment.children[0];
    assert!(
      (child.bounds.x() - 15.0).abs() < 0.01,
      "expected child x≈15px, got {}",
      child.bounds.x()
    );
    assert!(
      (child.bounds.y() - 22.0).abs() < 0.01,
      "expected child y≈22px, got {}",
      child.bounds.y()
    );
  }

  #[test]
  fn horizontal_scrollbar_reserves_gutter_height() {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.overflow_x = Overflow::Scroll;
    style.scrollbar_width = ScrollbarWidth::Thin;

    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      Size::new(200.0, 200.0),
      ContainingBlock::viewport(Size::new(200.0, 200.0)),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let node = BoxNode::new_block(
      Arc::new(style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    let fragment = fc.layout(&node, &constraints).unwrap();
    assert!((fragment.bounds.height() - 0.0).abs() < 0.01);

    style.scrollbar_gutter.stable = true;
    let node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    let fragment = fc.layout(&node, &constraints).unwrap();
    assert!((fragment.bounds.height() - 8.0).abs() < 0.01);
  }

  #[test]
  fn overflow_auto_child_does_not_enter_convergence_loop_under_overlay_scrollbars() {
    // Keep this test independent of process-wide env state by explicitly using a fresh
    // (toggle-free) thread-local runtime config.
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::new())),
      || {
        reset_overflow_auto_child_layout_passes();

        let mut parent_style = ComputedStyle::default();
        parent_style.display = Display::Block;

        let mut child_style = ComputedStyle::default();
        child_style.display = Display::Block;
        child_style.overflow_y = Overflow::Auto;
        child_style.height = Some(Length::px(50.0));

        let mut inner_style = ComputedStyle::default();
        inner_style.display = Display::Block;
        inner_style.height = Some(Length::px(10.0));

        let inner =
          BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);
        let child = BoxNode::new_block(
          Arc::new(child_style),
          FormattingContextType::Block,
          vec![inner],
        );
        let parent = BoxNode::new_block(
          Arc::new(parent_style),
          FormattingContextType::Block,
          vec![child],
        );

        let fc = BlockFormattingContext::new();
        let constraints = LayoutConstraints::definite_width(200.0);
        fc.layout(&parent, &constraints).unwrap();

        assert_eq!(
          overflow_auto_child_layout_passes(),
          0,
          "expected overflow:auto blocks to skip the convergence reflow loop under the default overlay scrollbar model"
        );
      },
    );
  }

  #[test]
  fn overflow_auto_child_skips_convergence_loop_even_when_overflowing() {
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::new())),
      || {
        reset_overflow_auto_child_layout_passes();

        let mut parent_style = ComputedStyle::default();
        parent_style.display = Display::Block;

        let mut child_style = ComputedStyle::default();
        child_style.display = Display::Block;
        child_style.overflow_y = Overflow::Auto;
        child_style.height = Some(Length::px(50.0));

        // Force vertical overflow so the legacy convergence loop would attempt multiple passes.
        let mut inner_style = ComputedStyle::default();
        inner_style.display = Display::Block;
        inner_style.height = Some(Length::px(200.0));

        let inner =
          BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);
        let child = BoxNode::new_block(
          Arc::new(child_style),
          FormattingContextType::Block,
          vec![inner],
        );
        let parent = BoxNode::new_block(
          Arc::new(parent_style),
          FormattingContextType::Block,
          vec![child],
        );

        let fc = BlockFormattingContext::new();
        let constraints = LayoutConstraints::definite_width(200.0);
        fc.layout(&parent, &constraints).unwrap();

        assert_eq!(
          overflow_auto_child_layout_passes(),
          0,
          "expected overflow:auto blocks to skip the convergence reflow loop under the default overlay scrollbar model, even when content overflows"
        );
      },
    );
  }

  #[test]
  fn overflow_auto_child_enters_convergence_loop_when_classic_scrollbars_enabled() {
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_CLASSIC_SCROLLBARS".to_string(),
        "1".to_string(),
      )]))),
      || {
        reset_overflow_auto_child_layout_passes();

        let mut parent_style = ComputedStyle::default();
        parent_style.display = Display::Block;

        let mut child_style = ComputedStyle::default();
        child_style.display = Display::Block;
        child_style.overflow_y = Overflow::Auto;
        child_style.height = Some(Length::px(50.0));

        // No overflow needed; classic scrollbar mode should still execute the convergence pass for
        // overflow:auto blocks (legacy behavior).
        let mut inner_style = ComputedStyle::default();
        inner_style.display = Display::Block;
        inner_style.height = Some(Length::px(10.0));

        let inner =
          BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);
        let child = BoxNode::new_block(
          Arc::new(child_style),
          FormattingContextType::Block,
          vec![inner],
        );
        let parent = BoxNode::new_block(
          Arc::new(parent_style),
          FormattingContextType::Block,
          vec![child],
        );

        let fc = BlockFormattingContext::new();
        let constraints = LayoutConstraints::definite_width(200.0);
        fc.layout(&parent, &constraints).unwrap();

        let passes = overflow_auto_child_layout_passes();
        assert!(
          passes >= 1,
          "expected classic scrollbar mode to re-enable the overflow:auto convergence loop (got {passes})"
        );
      },
    );
  }

  #[test]
  fn overflow_auto_child_does_not_clone_external_float_ctx_when_not_floating() {
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_CLASSIC_SCROLLBARS".to_string(),
        "1".to_string(),
      )]))),
      || {
        crate::layout::float_context::reset_float_context_clone_counter();

        let mut parent_style = ComputedStyle::default();
        parent_style.display = Display::Block;

        let mut float_style = ComputedStyle::default();
        float_style.display = Display::Block;
        float_style.float = Float::Left;
        float_style.width = Some(Length::px(10.0));
        float_style.height = Some(Length::px(10.0));
        let float_style = Arc::new(float_style);

        let mut children = Vec::new();
        // Create a large float context by inserting many preceding floats. The subsequent
        // overflow:auto child is not itself a float, so it should not require cloning this context
        // during scrollbar convergence.
        for _ in 0..200 {
          children.push(BoxNode::new_block(
            float_style.clone(),
            FormattingContextType::Block,
            vec![],
          ));
        }

        let mut child_style = ComputedStyle::default();
        child_style.display = Display::Block;
        child_style.overflow_y = Overflow::Auto;
        child_style.height = Some(Length::px(50.0));

        let mut inner_style = ComputedStyle::default();
        inner_style.display = Display::Block;
        inner_style.height = Some(Length::px(10.0));
        let inner =
          BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

        let child = BoxNode::new_block(
          Arc::new(child_style),
          FormattingContextType::Block,
          vec![inner],
        );
        children.push(child);

        let parent = BoxNode::new_block(
          Arc::new(parent_style),
          FormattingContextType::Block,
          children,
        );

        let fc = BlockFormattingContext::new();
        let constraints = LayoutConstraints::definite_width(200.0);
        fc.layout(&parent, &constraints).unwrap();

        assert_eq!(
          crate::layout::float_context::float_context_clone_count(),
          0,
          "expected overflow:auto scrollbar convergence to reuse the existing float context when the child cannot mutate it"
        );
      },
    );
  }

  #[test]
  fn padding_offsets_in_flow_children() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(30.0));

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_fragment = &fragment.children[0];
    assert!(
      (child_fragment.bounds.x() - 10.0).abs() < 0.01,
      "expected child x≈10, got {}",
      child_fragment.bounds.x()
    );
    assert!(
      (child_fragment.bounds.y() - 20.0).abs() < 0.01,
      "expected child y≈20, got {}",
      child_fragment.bounds.y()
    );
  }

  #[test]
  fn padding_offsets_children_of_in_flow_blocks() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(30.0));

    let grandchild =
      BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![grandchild],
    );
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&root, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    assert_eq!(fragment.children[0].children.len(), 1);
    let grandchild_fragment = &fragment.children[0].children[0];
    assert!(
      (grandchild_fragment.bounds.x() - 10.0).abs() < 0.01,
      "expected grandchild x≈10, got {}",
      grandchild_fragment.bounds.x()
    );
    assert!(
      (grandchild_fragment.bounds.y() - 20.0).abs() < 0.01,
      "expected grandchild y≈20, got {}",
      grandchild_fragment.bounds.y()
    );
  }

  #[test]
  fn content_visibility_auto_skips_after_remembered_size() {
    let _toggles_guard = content_visibility_test_guard();
    let _cache_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::remembered_size_cache_clear();

    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let spacer_style = block_style_with_height(300.0);
    let spacer = BoxNode::new_block(spacer_style, FormattingContextType::Block, vec![]);

    let mut content_child_style = ComputedStyle::default();
    content_child_style.display = Display::Block;
    content_child_style.height = Some(Length::px(50.0));
    let content_child = BoxNode::new_block(
      Arc::new(content_child_style),
      FormattingContextType::Block,
      vec![],
    );

    let mut cv_style = ComputedStyle::default();
    cv_style.display = Display::Block;
    cv_style.content_visibility = ContentVisibility::Auto;
    let cv_node = BoxNode::new_block(
      Arc::new(cv_style),
      FormattingContextType::Block,
      vec![content_child],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, cv_node],
    );
    let tree = BoxTree::new(root);

    // Pass #1: offscreen heuristic is satisfied, but there is no definite placeholder size yet, so
    // we must NOT skip descendant layout.
    let frag1 = fc.layout(&tree.root, &constraints).expect("layout pass #1");
    assert_eq!(frag1.children.len(), 2);
    let cv_frag1 = &frag1.children[1];
    assert_eq!(
      cv_frag1.children.len(),
      1,
      "first pass should lay out descendants to establish remembered size"
    );
    assert!(
      (cv_frag1.bounds.height() - 50.0).abs() < 0.1,
      "expected cv:auto placeholder height to match laid-out content on pass #1"
    );

    // Pass #2: the element now has a remembered size, so it can skip layout and use that size as a
    // definite placeholder.
    let frag2 = fc.layout(&tree.root, &constraints).expect("layout pass #2");
    assert_eq!(frag2.children.len(), 2);
    let cv_frag2 = &frag2.children[1];
    assert_eq!(
      cv_frag2.children.len(),
      0,
      "second pass should skip descendant layout using remembered size"
    );
    assert!(
      (cv_frag2.bounds.height() - 50.0).abs() < 0.1,
      "expected cv:auto placeholder height to come from remembered size on pass #2"
    );

    crate::layout::formatting_context::remembered_size_cache_clear();
  }

  fn block_style() -> ComputedStyle {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style
  }

  #[test]
  fn contain_intrinsic_size_auto_uses_remembered_size_when_skipped() {
    let _toggles_guard = content_visibility_test_guard();
    let _cache_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::remembered_size_cache_clear();

    // Layout structure:
    // root
    //  ├─ spacer (1000px tall)     -> pushes the auto element below a small viewport
    //  ├─ auto (content-visibility:auto)
    //  │    └─ tall child (500px)  -> establishes a non-zero remembered size
    //  └─ after (10px tall)        -> should not shift between layout passes

    let root_style = Arc::new(block_style());

    let mut spacer_style = block_style();
    spacer_style.height = Some(Length::px(1000.0));

    let mut auto_style = block_style();
    auto_style.content_visibility = ContentVisibility::Auto;

    let mut tall_child_style = block_style();
    tall_child_style.height = Some(Length::px(500.0));

    let mut after_style = block_style();
    after_style.height = Some(Length::px(10.0));

    let auto = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![BoxNode::new_block(
        Arc::new(tall_child_style),
        FormattingContextType::Block,
        vec![],
      )],
    );

    let tree = BoxTree::new(BoxNode::new_block(
      root_style,
      FormattingContextType::Block,
      vec![
        BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]),
        auto,
        BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]),
      ],
    ));

    let constraints = LayoutConstraints::definite_width(800.0);

    // Pass #1: large viewport so `content-visibility:auto` does NOT skip.
    let viewport_large = Size::new(800.0, 3000.0);
    let fc =
      BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_large);
    let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #1");

    assert_eq!(root_frag.children.len(), 3);
    let auto_frag = &root_frag.children[1];
    assert!(
      !auto_frag.children.is_empty(),
      "expected auto element contents to be laid out in pass #1"
    );
    let remembered_height = auto_frag.bounds.height();
    assert!(
      remembered_height > 0.0,
      "expected a non-zero laid out block-size for the auto element"
    );
    let after_y_pass1 = root_frag.children[2].bounds.y();

    // Pass #2: small viewport so the auto element is considered out-of-viewport and skipped.
    let viewport_small = Size::new(800.0, 600.0);
    let fc =
      BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_small);
    let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #2");

    assert_eq!(root_frag.children.len(), 3);
    let auto_frag = &root_frag.children[1];
    assert!(
      auto_frag.children.is_empty(),
      "expected auto element contents to be skipped in pass #2"
    );

    let placeholder_height = auto_frag.bounds.height();
    assert!(
      (placeholder_height - remembered_height).abs() < 0.01,
      "expected skipped placeholder block-size {placeholder_height} to match remembered size {remembered_height}"
    );

    let after_y_pass2 = root_frag.children[2].bounds.y();
    assert!(
      (after_y_pass2 - after_y_pass1).abs() < 0.01,
      "expected following content to keep the same offset (pass1={after_y_pass1}, pass2={after_y_pass2})"
    );

    crate::layout::formatting_context::remembered_size_cache_clear();
  }

  #[test]
  fn contain_intrinsic_size_auto_falls_back_to_length_then_remembered_size() {
    let _toggles_guard = content_visibility_test_guard();
    let _cache_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::remembered_size_cache_clear();

    // Layout structure:
    // root
    //  ├─ spacer (1000px tall)     -> pushes the auto element below a small viewport
    //  ├─ auto (content-visibility:auto; contain-intrinsic-size:auto 30px)
    //  │    └─ tall child (500px)  -> establishes a non-zero remembered size once laid out
    //  └─ after (10px tall)        -> should shift from fallback (30px) to remembered (500px)
    //
    // Pass #1: element skipped => use fallback length (30px)
    // Pass #2: element laid out => remember 500px
    // Pass #3: element skipped again => use remembered 500px (not fallback)

    let root_style = Arc::new(block_style());

    let mut spacer_style = block_style();
    spacer_style.height = Some(Length::px(1000.0));

    let mut auto_style = block_style();
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.contain_intrinsic_height = ContainIntrinsicSizeAxis {
      auto: true,
      length: Some(Length::px(30.0)),
    };

    let mut tall_child_style = block_style();
    tall_child_style.height = Some(Length::px(500.0));

    let mut after_style = block_style();
    after_style.height = Some(Length::px(10.0));

    let auto = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![BoxNode::new_block(
        Arc::new(tall_child_style),
        FormattingContextType::Block,
        vec![],
      )],
    );

    let tree = BoxTree::new(BoxNode::new_block(
      root_style,
      FormattingContextType::Block,
      vec![
        BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]),
        auto,
        BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]),
      ],
    ));

    let constraints = LayoutConstraints::definite_width(800.0);

    // Pass #1: small viewport => auto element is skipped, using fallback length (30px).
    let viewport_small = Size::new(800.0, 600.0);
    let fc =
      BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_small);
    let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #1");

    assert_eq!(root_frag.children.len(), 3);
    let auto_frag = &root_frag.children[1];
    assert!(
      auto_frag.children.is_empty(),
      "expected auto element contents to be skipped in pass #1"
    );
    assert!(
      (auto_frag.bounds.height() - 30.0).abs() < 0.01,
      "expected pass #1 placeholder height to use fallback length"
    );
    let after_y_pass1 = root_frag.children[2].bounds.y();

    // Pass #2: large viewport => auto element is laid out and its 500px block-size is remembered.
    let viewport_large = Size::new(800.0, 3000.0);
    let fc =
      BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_large);
    let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #2");
    assert_eq!(root_frag.children.len(), 3);
    let auto_frag = &root_frag.children[1];
    assert!(
      !auto_frag.children.is_empty(),
      "expected auto element contents to be laid out in pass #2"
    );
    let remembered_height = auto_frag.bounds.height();
    assert!(
      (remembered_height - 500.0).abs() < 0.01,
      "expected pass #2 laid out height to match the tall child"
    );
    let after_y_pass2 = root_frag.children[2].bounds.y();
    assert!(
      after_y_pass2 > after_y_pass1 + 100.0,
      "expected following content to shift down once the element is laid out (pass1={after_y_pass1}, pass2={after_y_pass2})"
    );

    // Pass #3: small viewport again => auto element is skipped, using remembered size (500px).
    let fc =
      BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_small);
    let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #3");
    assert_eq!(root_frag.children.len(), 3);
    let auto_frag = &root_frag.children[1];
    assert!(
      auto_frag.children.is_empty(),
      "expected auto element contents to be skipped in pass #3"
    );
    let placeholder_height = auto_frag.bounds.height();
    assert!(
      (placeholder_height - remembered_height).abs() < 0.01,
      "expected pass #3 placeholder height to use remembered size (placeholder={placeholder_height}, remembered={remembered_height})"
    );
    let after_y_pass3 = root_frag.children[2].bounds.y();
    assert!(
      (after_y_pass3 - after_y_pass2).abs() < 0.01,
      "expected following content to keep the remembered offset (pass2={after_y_pass2}, pass3={after_y_pass3})"
    );

    crate::layout::formatting_context::remembered_size_cache_clear();
  }

  fn block_style_with_margin(margin: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.margin_top = Some(Length::px(margin));
    style.margin_bottom = Some(Length::px(margin));
    style.margin_left = Some(Length::px(margin));
    style.margin_right = Some(Length::px(margin));
    Arc::new(style)
  }

  #[test]
  fn vertical_writing_blocks_stack_horizontally() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.writing_mode = WritingMode::VerticalRl;
    // In vertical writing modes the inline axis is vertical, so the inline size maps to the
    // physical height.
    root_style.height = Some(Length::px(200.0));
    root_style.height_keyword = None;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.writing_mode = WritingMode::VerticalRl;
    // The block axis is horizontal, so the block size maps to the physical width.
    child_style.width = Some(Length::px(40.0));
    child_style.width_keyword = None;

    let child1 = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child1, child2],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&root, &constraints).unwrap();

    let root_w = fragment.bounds.width();
    let root_h = fragment.bounds.height();
    // Block axis is horizontal; total block extent should be sum of block sizes (approx 80).
    assert!(
      (root_w - 80.0).abs() < 0.5,
      "expected ~80 width, got {}x{}",
      root_w,
      root_h
    );
    assert!(
      (root_h - 200.0).abs() < 0.5,
      "expected height 200, got {}x{}",
      root_w,
      root_h
    );
    assert_eq!(fragment.children.len(), 2);

    let first = &fragment.children[0];
    let second = &fragment.children[1];

    // Children are transposed: physical width = block size (40), height = inline size (200).
    assert!((first.bounds.width() - 40.0).abs() < 0.5);
    assert!((first.bounds.height() - 200.0).abs() < 0.5);
    assert!((second.bounds.width() - 40.0).abs() < 0.5);
    assert!((second.bounds.height() - 200.0).abs() < 0.5);

    // vertical-rl stacks from right to left (block-axis negative).
    assert!(first.bounds.x() > second.bounds.x());
    assert!((first.bounds.x() - 40.0).abs() < 0.5);
    assert!((second.bounds.x()).abs() < 0.5);
    assert!((first.bounds.y()).abs() < 0.5);
    assert!((second.bounds.y()).abs() < 0.5);
  }

  #[test]
  fn test_bfc_new() {
    let _bfc = BlockFormattingContext::new();
  }

  #[test]
  fn test_layout_empty_block() {
    let bfc = BlockFormattingContext::new();
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 800.0);
    assert_eq!(fragment.bounds.height(), 0.0);
  }

  #[test]
  fn test_layout_block_with_explicit_height() {
    let bfc = BlockFormattingContext::new();
    let root = BoxNode::new_block(
      block_style_with_height(200.0),
      FormattingContextType::Block,
      vec![],
    );
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 800.0);
    assert_eq!(fragment.bounds.height(), 200.0);
  }

  #[test]
  fn multicol_column_span_all_fragment_positions_are_stable() {
    fn block_with_id(id: usize, style: Arc<ComputedStyle>) -> BoxNode {
      let mut node = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
      node.id = id;
      node
    }

    fn fragments_with_id<'a>(root: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
      fn walk<'a>(node: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
        if let FragmentContent::Block {
          box_id: Some(box_id),
        } = &node.content
        {
          if *box_id == id {
            out.push(node);
          }
        }
        for child in node.children.iter() {
          walk(child, id, out);
        }
      }

      let mut out = Vec::new();
      walk(root, id, &mut out);
      out
    }

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(2);
    multicol_style.column_gap = Length::px(20.0);
    let multicol_style = Arc::new(multicol_style);

    let child_style = block_style_with_height(20.0);
    let span_style = {
      let mut style = (*block_style_with_height(10.0)).clone();
      style.column_span = ColumnSpan::All;
      Arc::new(style)
    };

    let mut multicol = BoxNode::new_block(
      multicol_style,
      FormattingContextType::Block,
      vec![
        block_with_id(5001, child_style.clone()),
        block_with_id(5002, child_style.clone()),
        block_with_id(5003, child_style.clone()),
        block_with_id(5004, child_style.clone()),
        block_with_id(5005, span_style),
        block_with_id(5006, child_style.clone()),
        block_with_id(5007, child_style.clone()),
        block_with_id(5008, child_style.clone()),
        block_with_id(5009, child_style),
      ],
    );
    multicol.id = 5000;

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&multicol, &constraints).expect("multicol layout");

    let col_width = (200.0 - 20.0) / 2.0;
    let col2_x = col_width + 20.0;
    let segment_height = 40.0;
    let span_height = 10.0;
    let after_y = segment_height + span_height;

    let expected = [
      (5001, 0.0, 0.0, col_width, 20.0),
      (5002, 0.0, 20.0, col_width, 20.0),
      (5003, col2_x, 0.0, col_width, 20.0),
      (5004, col2_x, 20.0, col_width, 20.0),
      (5005, 0.0, segment_height, 200.0, span_height),
      (5006, 0.0, after_y, col_width, 20.0),
      (5007, 0.0, after_y + 20.0, col_width, 20.0),
      (5008, col2_x, after_y, col_width, 20.0),
      (5009, col2_x, after_y + 20.0, col_width, 20.0),
    ];

    for (id, x, y, w, h) in expected {
      let hits = fragments_with_id(&fragment, id);
      assert_eq!(
        hits.len(),
        1,
        "expected exactly one fragment for box_id={}",
        id
      );
      let frag = hits[0];
      assert!(
        (frag.bounds.x() - x).abs() < 0.1,
        "box_id={} expected x≈{}, got {}",
        id,
        x,
        frag.bounds.x()
      );
      assert!(
        (frag.bounds.y() - y).abs() < 0.1,
        "box_id={} expected y≈{}, got {}",
        id,
        y,
        frag.bounds.y()
      );
      assert!(
        (frag.bounds.width() - w).abs() < 0.1,
        "box_id={} expected width≈{}, got {}",
        id,
        w,
        frag.bounds.width()
      );
      assert!(
        (frag.bounds.height() - h).abs() < 0.1,
        "box_id={} expected height≈{}, got {}",
        id,
        h,
        frag.bounds.height()
      );
    }
  }

  #[test]
  fn multicol_balance_distributes_blocks_with_margins() {
    fn block_with_id(id: usize, style: Arc<ComputedStyle>) -> BoxNode {
      let mut node = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
      node.id = id;
      node
    }

    fn fragments_with_id<'a>(root: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
      fn walk<'a>(node: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
        if let FragmentContent::Block {
          box_id: Some(box_id),
        } = &node.content
        {
          if *box_id == id {
            out.push(node);
          }
        }
        for child in node.children.iter() {
          walk(child, id, out);
        }
      }

      let mut out = Vec::new();
      walk(root, id, &mut out);
      out
    }

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(2);
    multicol_style.column_gap = Length::px(16.0);
    let multicol_style = Arc::new(multicol_style);

    let mut child_style = (*block_style_with_height(84.0)).clone();
    child_style.margin_top = Some(Length::px(16.0));
    child_style.margin_bottom = Some(Length::px(16.0));
    let child_style = Arc::new(child_style);

    let mut multicol = BoxNode::new_block(
      multicol_style,
      FormattingContextType::Block,
      vec![
        block_with_id(7001, child_style.clone()),
        block_with_id(7002, child_style.clone()),
        block_with_id(7003, child_style.clone()),
        block_with_id(7004, child_style),
      ],
    );
    multicol.id = 7000;

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&multicol, &constraints).expect("multicol layout");

    let col_width = (200.0 - 16.0) / 2.0;
    let col2_x = col_width + 16.0;

    let expected = [
      (7001, 0.0, 0.0, col_width, 84.0),
      (7002, 0.0, 100.0, col_width, 84.0),
      // The second column begins with the third block, which must have its own margin-top applied.
      (7003, col2_x, 16.0, col_width, 84.0),
      // The inter-block margin between the third and fourth blocks must still apply (16px).
      (7004, col2_x, 116.0, col_width, 84.0),
    ];

    for (id, x, y, w, h) in expected {
      let hits = fragments_with_id(&fragment, id);
      assert_eq!(
        hits.len(),
        1,
        "expected exactly one fragment for box_id={}",
        id
      );
      let frag = hits[0];
      assert!(
        (frag.bounds.x() - x).abs() < 0.1,
        "box_id={} expected x≈{}, got {}",
        id,
        x,
        frag.bounds.x()
      );
      assert!(
        (frag.bounds.y() - y).abs() < 0.1,
        "box_id={} expected y≈{}, got {}",
        id,
        y,
        frag.bounds.y()
      );
      assert!(
        (frag.bounds.width() - w).abs() < 0.1,
        "box_id={} expected width≈{}, got {}",
        id,
        w,
        frag.bounds.width()
      );
      assert!(
        (frag.bounds.height() - h).abs() < 0.1,
        "box_id={} expected height≈{}, got {}",
        id,
        h,
        frag.bounds.height()
      );
    }
  }

  #[test]
  fn multicol_segment_offset_skips_offscreen_auto() {
    let _guard = content_visibility_test_guard();

    fn block_with_id(id: usize, style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> BoxNode {
      let mut node = BoxNode::new_block(style, FormattingContextType::Block, children);
      node.id = id;
      node
    }

    fn fragments_with_id<'a>(root: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
      fn walk<'a>(node: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
        if let FragmentContent::Block {
          box_id: Some(box_id),
        } = &node.content
        {
          if *box_id == id {
            out.push(node);
          }
        }
        for child in node.children.iter() {
          walk(child, id, out);
        }
      }

      let mut out = Vec::new();
      walk(root, id, &mut out);
      out
    }

    let viewport = Size::new(200.0, 100.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(2);
    let multicol_style = Arc::new(multicol_style);

    let seg1 = block_with_id(6001, block_style_with_height(200.0), vec![]);
    let span_all = block_with_id(
      6002,
      {
        let mut style = (*block_style_with_height(10.0)).clone();
        style.column_span = ColumnSpan::All;
        Arc::new(style)
      },
      vec![],
    );

    let inner_child = block_with_id(6004, block_style_with_height(60.0), vec![]);
    let auto = block_with_id(
      6003,
      {
        let mut style = (*block_style_with_height(60.0)).clone();
        style.content_visibility = ContentVisibility::Auto;
        Arc::new(style)
      },
      vec![inner_child],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![BoxNode::new_block(
        multicol_style,
        FormattingContextType::Block,
        vec![seg1, span_all, auto],
      )],
    );

    let fragment = fc.layout(&root, &constraints).unwrap();

    let auto_fragments = fragments_with_id(&fragment, 6003);
    assert!(
      !auto_fragments.is_empty(),
      "expected to find fragment(s) for box_id=6003"
    );
    assert!(
      auto_fragments.iter().all(|frag| frag.children.is_empty()),
      "expected content-visibility:auto descendants to be skipped when the segment is offscreen"
    );
  }

  #[test]
  fn multicol_column_count_width_resolves_used_count() {
    let fc = BlockFormattingContext::new();

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(3);
    multicol_style.column_width = Some(Length::px(250.0));
    multicol_style.column_gap = Length::px(40.0);

    let (count, width, gap) = fc.compute_column_geometry(&multicol_style, 600.0);
    assert_eq!(count, 2);
    assert!((gap - 40.0).abs() < 0.1);
    assert!((width - 280.0).abs() < 0.1);
  }

  #[test]
  fn legend_auto_width_shrinks_to_content() {
    let mut legend_style = ComputedStyle::default();
    legend_style.display = Display::Block;
    legend_style.shrink_to_fit_inline_size = true;

    let mut legend_child_style = ComputedStyle::default();
    legend_child_style.display = Display::Block;
    legend_child_style.width = Some(Length::px(80.0));
    legend_child_style.height = Some(Length::px(10.0));
    legend_child_style.width_keyword = None;
    legend_child_style.height_keyword = None;
    let legend_child = BoxNode::new_block(
      Arc::new(legend_child_style),
      FormattingContextType::Block,
      vec![],
    );

    let legend = BoxNode::new_block(
      Arc::new(legend_style),
      FormattingContextType::Block,
      vec![legend_child],
    );

    let mut sibling_style = ComputedStyle::default();
    sibling_style.display = Display::Block;
    let sibling = BoxNode::new_block(
      Arc::new(sibling_style),
      FormattingContextType::Block,
      vec![],
    );

    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      Size::new(200.0, 200.0),
      ContainingBlock::viewport(Size::new(200.0, 200.0)),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let solo = fc.layout(&legend, &constraints).expect("legend layout");
    assert!(
      (solo.bounds.width() - 80.0).abs() < 0.1,
      "legend root should shrink to its contents; got width {}",
      solo.bounds.width()
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![legend, sibling],
    );
    let fragment = fc.layout(&root, &constraints).expect("block layout");

    assert_eq!(
      fragment.children.len(),
      2,
      "root should produce two children"
    );
    let legend_fragment = &fragment.children[0];
    assert!(
      legend_fragment
        .style
        .as_ref()
        .map(|s| s.shrink_to_fit_inline_size)
        .unwrap_or(false),
      "legend fragment should carry shrink-to-fit flag"
    );
    assert!(
      (legend_fragment.bounds.width() - 80.0).abs() < 0.1,
      "legend should shrink to its contents; got width {}",
      legend_fragment.bounds.width()
    );
    assert!(
      legend_fragment.bounds.x().abs() < 0.01,
      "legend should start at the origin"
    );

    let sibling_fragment = &fragment.children[1];
    assert!(
      (sibling_fragment.bounds.width() - 200.0).abs() < 0.1,
      "normal block should span the containing width; got width {}",
      sibling_fragment.bounds.width()
    );
    assert!(
      sibling_fragment.bounds.x().abs() < 0.01,
      "sibling should start at the container origin; got {}",
      sibling_fragment.bounds.x()
    );
  }

  #[test]
  fn test_layout_nested_blocks() {
    let bfc = BlockFormattingContext::new();

    let child1 = BoxNode::new_block(
      block_style_with_height(100.0),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      block_style_with_height(150.0),
      FormattingContextType::Block,
      vec![],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![child1, child2],
    );
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
    assert!(fragment.bounds.height() >= 250.0);
  }

  #[test]
  fn relative_block_offsets_fragment_without_affecting_flow_size() {
    let mut relative_style = ComputedStyle::default();
    relative_style.display = Display::Block;
    relative_style.position = Position::Relative;
    relative_style.left = crate::style::types::InsetValue::Length(Length::px(30.0));
    relative_style.top = crate::style::types::InsetValue::Length(Length::px(20.0));
    relative_style.width = Some(Length::px(100.0));
    relative_style.height = Some(Length::px(40.0));
    relative_style.width_keyword = None;
    relative_style.height_keyword = None;

    let child = BoxNode::new_block(
      Arc::new(relative_style),
      FormattingContextType::Block,
      vec![],
    );
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let constraints = LayoutConstraints::definite(300.0, 200.0);

    let fragment = BlockFormattingContext::new()
      .layout(&root, &constraints)
      .unwrap();

    assert_eq!(fragment.bounds.height(), 40.0);
    let child_fragment = fragment.children.first().expect("child");
    assert_eq!(child_fragment.bounds.width(), 100.0);
    assert_eq!(child_fragment.bounds.height(), 40.0);
    assert_eq!(child_fragment.bounds.x(), 30.0);
    assert_eq!(child_fragment.bounds.y(), 20.0);
  }

  #[test]
  fn relative_block_percentage_offsets_use_containing_block() {
    let mut relative_style = ComputedStyle::default();
    relative_style.display = Display::Block;
    relative_style.position = Position::Relative;
    relative_style.left = crate::style::types::InsetValue::Length(Length::percent(50.0)); // 50% of 200 = 100
    relative_style.top = crate::style::types::InsetValue::Length(Length::percent(25.0)); // 25% of 120 = 30
    relative_style.width = Some(Length::px(40.0));
    relative_style.height = Some(Length::px(10.0));
    relative_style.width_keyword = None;
    relative_style.height_keyword = None;

    let child = BoxNode::new_block(
      Arc::new(relative_style),
      FormattingContextType::Block,
      vec![],
    );
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.height = Some(Length::px(120.0));
    root_style.height_keyword = None;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = BlockFormattingContext::new()
      .layout(&root, &constraints)
      .unwrap();

    let child_fragment = fragment.children.first().expect("child");
    assert_eq!(child_fragment.bounds.x(), 100.0);
    assert_eq!(child_fragment.bounds.y(), 30.0);
  }

  #[test]
  fn percentage_height_uses_definite_containing_block() {
    let bfc = BlockFormattingContext::new();

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::percent(50.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.height = Some(Length::px(300.0));
    root_style.height_keyword = None;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let child_fragment = fragment.children.first().expect("child fragment");
    assert!((child_fragment.bounds.height() - 150.0).abs() < 0.1);
  }

  #[test]
  fn aspect_ratio_sets_auto_height() {
    let bfc = BlockFormattingContext::new();

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.aspect_ratio = crate::style::types::AspectRatio::Ratio(2.0);
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let child_fragment = fragment.children.first().expect("child fragment");
    assert_eq!(child_fragment.bounds.height(), 100.0);
  }

  #[test]
  fn percentage_height_resolves_against_aspect_ratio_auto_height() {
    let bfc = BlockFormattingContext::new();

    let mut inline_block_style = ComputedStyle::default();
    inline_block_style.display = Display::InlineBlock;
    inline_block_style.width = Some(Length::px(10.0));
    inline_block_style.width_keyword = None;
    inline_block_style.height = Some(Length::percent(100.0));
    inline_block_style.height_keyword = None;
    let mut inline_block = BoxNode::new_inline_block(
      Arc::new(inline_block_style),
      FormattingContextType::Block,
      vec![],
    );
    inline_block.id = 4;

    let mut percent_style = ComputedStyle::default();
    percent_style.display = Display::Block;
    percent_style.height = Some(Length::percent(100.0));
    percent_style.height_keyword = None;
    let mut percent_box = BoxNode::new_block(
      Arc::new(percent_style),
      FormattingContextType::Block,
      vec![inline_block],
    );
    percent_box.id = 3;

    let mut ratio_style = ComputedStyle::default();
    ratio_style.display = Display::Block;
    ratio_style.aspect_ratio = crate::style::types::AspectRatio::Ratio(2.0);
    let mut ratio_box = BoxNode::new_block(
      Arc::new(ratio_style),
      FormattingContextType::Block,
      vec![percent_box],
    );
    ratio_box.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![ratio_box],
    );
    // Root element margins never collapse with children; mimic the real HTML root.
    root.id = 1;

    let fragment = bfc
      .layout(&root, &LayoutConstraints::definite_width(200.0))
      .unwrap();

    fn find_fragment_by_box_id<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment_by_box_id(child, id) {
          return Some(found);
        }
      }
      None
    }

    let ratio_fragment = find_fragment_by_box_id(&fragment, 2).expect("ratio fragment");
    assert!(
      (ratio_fragment.bounds.height() - 100.0).abs() < 0.1,
      "expected aspect-ratio box to be 100px tall; got {}",
      ratio_fragment.bounds.height()
    );

    let percent_fragment = find_fragment_by_box_id(&fragment, 3).expect("percent fragment");
    assert!(
      (percent_fragment.bounds.height() - 100.0).abs() < 0.1,
      "expected height:100% child to fill ratio height; got {}",
      percent_fragment.bounds.height()
    );

    let inline_block_fragment =
      find_fragment_by_box_id(&fragment, 4).expect("inline-block fragment");
    assert!(
      (inline_block_fragment.bounds.height() - 100.0).abs() < 0.1,
      "expected inline-block height:100% to resolve against definite containing height; got {}",
      inline_block_fragment.bounds.height()
    );
  }

  #[test]
  fn percentage_height_resolves_for_inline_replaced_against_aspect_ratio_auto_height() {
    let bfc = BlockFormattingContext::new();

    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Inline;
    replaced_style.width = Some(Length::percent(100.0));
    replaced_style.width_keyword = None;
    replaced_style.height = Some(Length::percent(100.0));
    replaced_style.height_keyword = None;
    let mut replaced = BoxNode::new_replaced(
      Arc::new(replaced_style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(150.0, 150.0)),
      None,
    );
    replaced.id = 3;

    let mut ratio_style = ComputedStyle::default();
    ratio_style.display = Display::Block;
    ratio_style.aspect_ratio = crate::style::types::AspectRatio::Ratio(1.5);
    // Avoid baseline strut effects; this test is about percentage resolution, not typography.
    ratio_style.line_height = crate::style::types::LineHeight::Length(Length::px(0.0));
    let mut ratio_box = BoxNode::new_block(
      Arc::new(ratio_style),
      FormattingContextType::Block,
      vec![replaced],
    );
    ratio_box.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![ratio_box],
    );
    // Root element margins never collapse with children; mimic the real HTML root.
    root.id = 1;

    let fragment = bfc
      .layout(&root, &LayoutConstraints::definite_width(150.0))
      .unwrap();

    fn find_fragment_by_box_id<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment_by_box_id(child, id) {
          return Some(found);
        }
      }
      None
    }

    let ratio_fragment = find_fragment_by_box_id(&fragment, 2).expect("ratio fragment");
    assert!(
      (ratio_fragment.bounds.height() - 100.0).abs() < 0.1,
      "expected aspect-ratio box to be 100px tall; got {}",
      ratio_fragment.bounds.height()
    );

    let replaced_fragment = find_fragment_by_box_id(&fragment, 3).expect("replaced fragment");
    assert!(
      (replaced_fragment.bounds.height() - 100.0).abs() < 0.1,
      "expected height:100% replaced content to resolve against ratio height; got {}",
      replaced_fragment.bounds.height()
    );
  }

  #[test]
  fn percentage_height_without_base_falls_back_to_auto() {
    let bfc = BlockFormattingContext::new();
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::percent(60.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let constraints = LayoutConstraints::definite_width(200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let child_fragment = fragment.children.first().expect("child fragment");
    assert_eq!(child_fragment.bounds.height(), 0.0);
  }

  #[test]
  fn float_percentage_height_uses_definite_containing_block() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(50.0));
    float_style.width_keyword = None;
    float_style.height = Some(Length::percent(100.0));
    float_style.height_keyword = None;

    let mut inner_style = ComputedStyle::default();
    inner_style.display = Display::Block;
    inner_style.height = Some(Length::px(40.0));
    inner_style.height_keyword = None;
    let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

    let float_node = BoxNode::new_block(
      Arc::new(float_style),
      FormattingContextType::Block,
      vec![inner],
    );

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.height = Some(Length::px(28.0));
    root_style.height_keyword = None;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![float_node],
    );

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = bfc.layout(&root, &constraints).unwrap();

    let float_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .is_some_and(|style| style.float.is_floating())
      })
      .expect("float fragment");

    assert!(
      (float_fragment.bounds.height() - 28.0).abs() < 0.1,
      "expected float height to resolve height:100% against containing block height; got {}",
      float_fragment.bounds.height()
    );
  }

  #[test]
  fn float_percentage_height_without_base_falls_back_to_auto() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(50.0));
    float_style.width_keyword = None;
    float_style.height = Some(Length::percent(100.0));
    float_style.height_keyword = None;

    let mut inner_style = ComputedStyle::default();
    inner_style.display = Display::Block;
    inner_style.height = Some(Length::px(40.0));
    inner_style.height_keyword = None;
    let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

    let float_node = BoxNode::new_block(
      Arc::new(float_style),
      FormattingContextType::Block,
      vec![inner],
    );

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    // Root has `height:auto`, but is laid out with a definite available height. Percentage heights
    // on descendants must not resolve against that unrelated available height (CSS2.1 §10.5).
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![float_node],
    );

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = bfc.layout(&root, &constraints).unwrap();

    let float_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .is_some_and(|style| style.float.is_floating())
      })
      .expect("float fragment");

    assert!(
      (float_fragment.bounds.height() - 40.0).abs() < 0.1,
      "expected float height percentage without a base to fall back to auto (content height); got {}",
      float_fragment.bounds.height()
    );
  }

  #[test]
  fn test_sibling_margin_collapse() {
    let bfc = BlockFormattingContext::new();

    let child1 = BoxNode::new_block(
      block_style_with_margin(20.0),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      block_style_with_margin(30.0),
      FormattingContextType::Block,
      vec![],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![child1, child2],
    );
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 2);
  }

  #[test]
  fn test_fc_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BlockFormattingContext>();
  }

  #[test]
  fn floats_extend_height_and_clear_moves_following_block() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(60.0));
    float_style.height = Some(Length::px(50.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let mut cleared_style = ComputedStyle::default();
    cleared_style.display = Display::Block;
    cleared_style.clear = crate::style::float::Clear::Left;
    cleared_style.height = Some(Length::px(10.0));
    cleared_style.height_keyword = None;
    let cleared_node = BoxNode::new_block(
      Arc::new(cleared_style),
      FormattingContextType::Block,
      vec![],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, cleared_node],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    assert!(
      fragment.bounds.height() >= 60.0,
      "BFC height should include float and cleared block; got {}",
      fragment.bounds.height()
    );

    let mut float_y = None;
    let mut clear_y = None;
    for child in fragment.children.iter() {
      if let Some(style) = &child.style {
        if style.float.is_floating() {
          float_y = Some(child.bounds.y());
        }
        if matches!(
          style.clear,
          crate::style::float::Clear::Left | crate::style::float::Clear::Both
        ) {
          clear_y = Some(child.bounds.y());
        }
      }
    }

    let float_y = float_y.expect("float fragment");
    let clear_y = clear_y.expect("cleared fragment");
    assert!(float_y.abs() < 0.01);
    assert!(
      clear_y >= 50.0,
      "cleared block should be pushed below float; got clear_y={clear_y}"
    );
  }

  #[test]
  fn inline_lines_shorten_next_to_float() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(80.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let text = BoxNode::new_text(default_style(), "text".to_string());
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, BoxNode::new_inline(default_style(), vec![text])],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let line = find_line(&fragment).expect("line fragment");
    assert!(
      line.bounds.width() <= 120.0,
      "line width should be shortened by float; got {}",
      line.bounds.width()
    );
    assert!(
      line.bounds.x() >= 79.9,
      "line should start after the float; got x={}",
      line.bounds.x()
    );
  }

  #[test]
  fn inline_lines_inside_following_block_boxes_consult_parent_float_context() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(80.0));
    float_style.height = Some(Length::px(20.0));
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let text = BoxNode::new_text(default_style(), "text".to_string());
    let paragraph = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![text]);

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, paragraph],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let line = find_line(&fragment).expect("line fragment");
    assert!(
      line.bounds.width() <= 120.0,
      "line width should be shortened by float; got {}",
      line.bounds.width()
    );
    assert!(
      line.bounds.x() >= 79.9,
      "line should start after the float; got x={}",
      line.bounds.x()
    );
  }

  #[test]
  fn block_intrinsic_inline_sizes_include_float_children() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(50.0));
    float_style.height = Some(Length::px(10.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let mut float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
    float_node.id = 2;

    let mut parent = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node],
    );
    parent.id = 1;

    let (min, max) = bfc
      .compute_intrinsic_inline_sizes(&parent)
      .expect("intrinsic widths");
    assert!(
      min >= 50.0 - 0.01,
      "min-content should include float width (expected >= 50, got {min})"
    );
    assert!(
      max >= 50.0 - 0.01,
      "max-content should include float width (expected >= 50, got {max})"
    );
  }

  #[test]
  fn block_intrinsic_inline_sizes_parallel_matches_serial_for_many_block_children() {
    // Ensure the Rayon global pool is initialized with FastRender's conservative defaults so the
    // parallel intrinsic-sizing path doesn't trip Rayon's lazy init in constrained test runners.
    crate::rayon_global::ensure_global_pool().expect("rayon global pool");

    // Guard against other concurrently running tests mutating/clearing the global intrinsic caches
    // mid-run (which can make regression assertions flaky).
    let _cache_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let epoch = crate::layout::formatting_context::intrinsic_cache_epoch() + 1;

    // Build a synthetic tree that yields many block-child segments. Each child contains inline
    // text so intrinsic sizing does non-trivial work.
    const CHILD_COUNT: usize = 64;
    const LONG_WORD: &str = "supercalifragilisticexpialidocious";
    const FILL: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let root_style = Arc::new(root_style);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    let child_style = Arc::new(child_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    let text_style = Arc::new(text_style);

    // Insert a few floats with `clear` so the intrinsic float-line accumulation is order-dependent.
    // This helps guard against regressions where parallel segment results are combined out of DOM
    // order (e.g. missing/incorrect sort), which would otherwise be masked when only commutative
    // reductions (like `max`) are involved.
    let make_float = |float: Float, clear: Clear, width: f32, label: &str| {
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style.float = float;
      style.clear = clear;
      style.width = Some(Length::px(width));
      style.width_keyword = None;
      style.margin_left = Some(Length::px(1.0));
      style.margin_right = Some(Length::px(2.0));
      let _ = label;
      BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
    };

    let mut children = Vec::with_capacity(CHILD_COUNT + 16);
    for idx in 0..CHILD_COUNT {
      let payload = if idx % 7 == 0 {
        format!("child-{idx} {LONG_WORD} {FILL} {FILL}")
      } else {
        format!("child-{idx} {FILL} {FILL}")
      };
      let text = BoxNode::new_text(text_style.clone(), payload);
      children.push(BoxNode::new_block(
        child_style.clone(),
        FormattingContextType::Block,
        vec![text],
      ));

      // Every 16 blocks, inject a float "line" where the 3rd float clears, forcing a flush.
      if idx % 16 == 15 {
        children.push(make_float(Float::Left, Clear::None, 40.0, "float-left"));
        children.push(make_float(Float::Right, Clear::None, 60.0, "float-right"));
        children.push(make_float(Float::Left, Clear::Both, 55.0, "float-clear"));
        children.push(make_float(Float::Right, Clear::None, 35.0, "float-right-2"));
      }
    }

    let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);

    let viewport = Size::new(800.0, 600.0);
    let font_ctx = FontContext::new();

    crate::layout::formatting_context::intrinsic_cache_use_epoch(epoch, true);
    let serial_factory = FormattingContextFactory::with_font_context_and_viewport(
      font_ctx.clone(),
      viewport,
    )
    .with_parallelism(LayoutParallelism::disabled());
    let serial_bfc = BlockFormattingContext::with_factory(serial_factory);
    let (serial_min, serial_max) = serial_bfc
      .compute_intrinsic_inline_sizes(&root)
      .expect("serial intrinsic sizing");

    crate::layout::formatting_context::intrinsic_cache_use_epoch(epoch + 1, true);
    let parallelism = LayoutParallelism::enabled(1).with_max_threads(Some(2));
    assert!(
      parallelism.should_parallelize(root.children.len()),
      "expected intrinsic sizing to take the parallel path (children={})",
      root.children.len()
    );
    let collector = Arc::new(crate::layout::engine::LayoutParallelDebugCollector::default());
    let _debug_guard = crate::layout::engine::LayoutParallelDebugCollectorThreadGuard::install(Some(
      collector.clone(),
    ));
    let parallel_factory =
      FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
        .with_parallelism(parallelism)
        .with_layout_parallel_debug(Some(collector));
    let parallel_bfc = BlockFormattingContext::with_factory(parallel_factory);

    // Run inside a dedicated 2-thread pool when possible so we exercise true parallel execution
    // even if the global pool was initialized with a single thread (e.g. CPU budget = 1).
    let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build().ok();
    let (parallel_min, parallel_max) = if let Some(pool) = pool {
      pool.install(|| {
        parallel_bfc
          .compute_intrinsic_inline_sizes(&root)
          .expect("parallel intrinsic sizing")
      })
    } else {
      parallel_bfc
        .compute_intrinsic_inline_sizes(&root)
        .expect("parallel intrinsic sizing")
    };

    let counters = crate::layout::engine::layout_parallel_debug_counters();
    assert!(
      counters.work_items > 0,
      "expected parallel intrinsic sizing to record debug work items"
    );

    const EPS: f32 = 1e-3;
    assert!(
      (serial_min - parallel_min).abs() < EPS,
      "min-content mismatch: serial={serial_min} parallel={parallel_min}"
    );
    assert!(
      (serial_max - parallel_max).abs() < EPS,
      "max-content mismatch: serial={serial_max} parallel={parallel_max}"
    );
  }

  #[test]
  fn float_shrink_to_fit_reuses_intrinsic_cache_within_epoch() {
    let _guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let next_epoch = crate::layout::formatting_context::intrinsic_cache_epoch() + 1;
    crate::layout::formatting_context::intrinsic_cache_use_epoch(next_epoch, true);

    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width_keyword = None;
    float_style.height_keyword = None;

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    let text_style = Arc::new(text_style);
    let text = BoxNode::new_text(
      text_style,
      "lorem ipsum dolor sit amet consectetur adipiscing elit".to_string(),
    );

    let mut float_node = BoxNode::new_block(
      Arc::new(float_style),
      FormattingContextType::Block,
      vec![text],
    );
    float_node.id = 42424;

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node],
    );

    // Warm the intrinsic cache for the float subtree (as would happen during an earlier intrinsic
    // sizing probe on the parent).
    let _ = bfc
      .compute_intrinsic_inline_sizes(&root)
      .expect("intrinsic sizing should succeed");

    // Ensure the subsequent layout pass observes cache hits rather than recomputing intrinsic
    // widths.
    crate::layout::formatting_context::intrinsic_cache_reset_counters();

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let _ = bfc
      .layout(&root, &constraints)
      .expect("layout should succeed");

    let (lookups, hits, stores, block_calls, flex_calls, inline_calls) =
      crate::layout::formatting_context::intrinsic_cache_stats();
    assert!(
      hits >= 2,
      "expected intrinsic cache hits on shrink-to-fit float sizing; lookups={lookups} hits={hits} stores={stores} block_calls={block_calls} flex_calls={flex_calls} inline_calls={inline_calls}",
    );
  }

  #[test]
  fn float_shrink_to_fit_rebases_percentage_padding_and_borders() {
    let bfc = BlockFormattingContext::new();

    let containing_width = 200.0;
    let constraints = LayoutConstraints::definite(containing_width, 200.0);

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    float_style.padding_left = Length::percent(10.0);
    float_style.padding_right = Length::percent(10.0);
    float_style.border_left_style = BorderStyle::Solid;
    float_style.border_right_style = BorderStyle::Solid;
    float_style.border_left_width = Length::percent(5.0);
    float_style.border_right_width = Length::percent(5.0);

    let mut float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
    float_node.id = 42425;

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node],
    );
    let fragment = bfc.layout(&root, &constraints).expect("layout");

    let float_fragment = find_block_fragment(&fragment, 42425).expect("float fragment");
    let expected_padding = containing_width * (10.0 / 100.0) * 2.0;
    let expected_borders = containing_width * (5.0 / 100.0) * 2.0;
    let expected_width = expected_padding + expected_borders;
    assert!(
      (float_fragment.bounds.width() - expected_width).abs() < 0.1,
      "expected rebased float width {expected_width}, got {}",
      float_fragment.bounds.width(),
    );
  }

  #[test]
  fn block_level_bfc_boxes_shift_right_to_avoid_float_margin_box() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(80.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    float_style.margin_right = Some(Length::px(10.0));
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    // Use a child that establishes a BFC (`display: table` does so) and is narrow enough to fit in
    // the space to the right of the float.
    let mut table_style = ComputedStyle::default();
    table_style.display = Display::Table;
    table_style.width = Some(Length::px(40.0));
    table_style.height = Some(Length::px(10.0));
    table_style.width_keyword = None;
    table_style.height_keyword = None;
    let table_node =
      BoxNode::new_block(Arc::new(table_style), FormattingContextType::Table, vec![]);

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, table_node],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = bfc.layout(&root, &constraints).unwrap();

    let float_margin_box_right = 80.0 + 10.0;

    let table_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.display == Display::Table)
          .unwrap_or(false)
      })
      .expect("table fragment");

    assert!(
      table_fragment.bounds.x() >= float_margin_box_right - 0.5,
      "expected table border box to be shifted right of the float margin box (x>={}); got x={}",
      float_margin_box_right,
      table_fragment.bounds.x()
    );
  }

  #[test]
  fn parent_first_child_margin_collapsing_ignores_empty_inline_content() {
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    let mut leading_inline = BoxNode::new_inline(Arc::new(inline_style), vec![]);
    leading_inline.id = 3;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.margin_top = Some(Length::px(10.0));
    child_style.height = Some(Length::px(20.0));
    child_style.height_keyword = None;
    let mut first_block_child =
      BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    first_block_child.id = 4;

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    let mut parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![leading_inline, first_block_child],
    );
    parent.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(viewport.width));
    root_style.width_keyword = None;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let parent_fragment = find_block_fragment(&fragment, 2).expect("parent fragment");
    let child_fragment = find_block_fragment(parent_fragment, 4).expect("child fragment");

    assert!(
      child_fragment.bounds.y().abs() < 0.1,
      "expected first block child to start at block-start due to margin collapsing (y={})",
      child_fragment.bounds.y()
    );
  }

  #[test]
  fn margin_collapsing_treats_blocks_with_only_collapsed_inline_content_as_empty() {
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    let text = BoxNode::new_text(Arc::new(text_style), "\n    \n".to_string());

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    let inline = BoxNode::new_inline(Arc::new(inline_style), vec![text]);

    let mut empty_block_style = ComputedStyle::default();
    empty_block_style.display = Display::Block;
    let mut empty_block = BoxNode::new_block(
      Arc::new(empty_block_style),
      FormattingContextType::Block,
      vec![inline],
    );
    empty_block.id = 3;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.margin_top = Some(Length::px(10.0));
    child_style.height = Some(Length::px(20.0));
    child_style.height_keyword = None;
    let mut first_block_child =
      BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    first_block_child.id = 4;

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    let mut parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![empty_block, first_block_child],
    );
    parent.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(viewport.width));
    root_style.width_keyword = None;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let parent_fragment = find_block_fragment(&fragment, 2).expect("parent fragment");
    let empty_block_fragment =
      find_block_fragment(parent_fragment, 3).expect("empty block fragment");
    assert!(
      empty_block_fragment.bounds.height() <= 0.01,
      "expected the whitespace-only block to have no in-flow block-size (h={})",
      empty_block_fragment.bounds.height()
    );

    let child_fragment = find_block_fragment(parent_fragment, 4).expect("child fragment");
    assert!(
      child_fragment.bounds.y().abs() < 0.1,
      "expected margin to collapse through the whitespace-only block (y={})",
      child_fragment.bounds.y()
    );
  }

  #[test]
  fn float_negative_margin_reduces_blocked_width() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(60.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    float_style.margin_left = Some(Length::px(-20.0));
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let text = BoxNode::new_text(default_style(), "text".to_string());
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, BoxNode::new_inline(default_style(), vec![text])],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let float_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.float.is_floating())
          .unwrap_or(false)
      })
      .expect("float fragment");
    assert!(
      float_fragment.bounds.x() < 0.0,
      "negative margin should shift float left; got {}",
      float_fragment.bounds.x()
    );

    let line = find_line(&fragment).expect("line fragment");
    assert!(
      (line.bounds.x() - 40.0).abs() < 1.0,
      "line should start after the reduced margin box; got {}",
      line.bounds.x()
    );
  }

  #[test]
  fn float_auto_width_shrinks_to_available_space_next_to_float() {
    let bfc = BlockFormattingContext::new();

    let mut wide_style = ComputedStyle::default();
    wide_style.display = Display::Block;
    wide_style.float = Float::Left;
    wide_style.width = Some(Length::px(120.0));
    wide_style.height = Some(Length::px(20.0));
    wide_style.width_keyword = None;
    wide_style.height_keyword = None;
    let wide_float = BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.float = Float::Left;
    let text = BoxNode::new_text(default_style(), "word ".repeat(20));
    let auto_float = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![BoxNode::new_inline(default_style(), vec![text])],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![wide_float, auto_float],
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    let floats: Vec<_> = fragment
      .children
      .iter()
      .filter(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.float.is_floating())
          .unwrap_or(false)
      })
      .collect();

    assert_eq!(floats.len(), 2);

    let mut wide = None;
    let mut auto = None;
    for float in floats {
      if (float.bounds.width() - 120.0).abs() < 0.5 {
        wide = Some(float);
      } else {
        auto = Some(float);
      }
    }

    let wide = wide.expect("wide float fragment");
    let auto = auto.expect("auto float fragment");

    assert!(
      auto.bounds.y() < 0.01,
      "auto float should stay alongside the existing float; got y={}",
      auto.bounds.y()
    );
    assert!(
      (auto.bounds.x() - wide.bounds.width()).abs() < 0.5,
      "auto float should start after the first float; got x={}",
      auto.bounds.x()
    );
    assert!(
      auto.bounds.width() <= 90.0,
      "auto float should shrink to the available 80px space; got {}",
      auto.bounds.width()
    );
  }

  #[test]
  fn float_percent_padding_resolves_against_containing_block_width() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(100.0));
    float_style.width_keyword = None;
    float_style.padding_left = Length::percent(10.0);
    float_style.padding_right = Length::px(0.0);
    float_style.border_left_width = Length::px(0.0);
    float_style.border_right_width = Length::px(0.0);

    let text = BoxNode::new_text(default_style(), "hello".into());
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let float_node = BoxNode::new_block(
      Arc::new(float_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node.clone()],
    );
    let constraints = LayoutConstraints::definite_width(200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let float_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.float.is_floating())
          .unwrap_or(false)
      })
      .expect("float fragment");

    fn find_line_offset_x(fragment: &FragmentNode, offset: f32) -> Option<f32> {
      let offset = offset + fragment.bounds.x();
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(offset);
      }
      for child in fragment.children.iter() {
        if let Some(found) = find_line_offset_x(child, offset) {
          return Some(found);
        }
      }
      None
    }

    let line_x = float_fragment
      .children
      .iter()
      .find_map(|child| find_line_offset_x(child, 0.0))
      .expect("line fragment");

    let inline_sides = inline_axis_sides(&float_style);
    let inline_positive = inline_axis_positive(float_style.writing_mode, float_style.direction);
    let computed_width = compute_block_width(
      &float_style,
      200.0,
      bfc.viewport_size,
      None,
      inline_sides,
      inline_positive,
      &bfc.font_context,
    );
    let expected_offset = computed_width.border_left + computed_width.padding_left;

    assert!(
      (line_x - expected_offset).abs() < 0.5,
      "expected line to start at x={:.2} inside the float (border+padding); got x={:.2}",
      expected_offset,
      line_x
    );
  }

  #[test]
  fn list_marker_outside_positions_marker_left_of_text() {
    let generator = BoxGenerator::new();

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_position = ListStylePosition::Outside;
    let li_style = Arc::new(li_style);

    let mut ul_style = ComputedStyle::default();
    ul_style.display = Display::Block;
    let ul_style = Arc::new(ul_style);

    let li = DOMNode::new_element(
      "li",
      li_style.clone(),
      vec![DOMNode::new_text("Item", li_style.clone())],
    );
    let ul = DOMNode::new_element("ul", ul_style, vec![li]);
    let box_tree = generator.generate(&ul).unwrap();

    let bfc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = bfc.layout(&box_tree.root, &constraints).unwrap();

    let li_fragment = fragment.children.first().expect("li fragment");
    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let line = find_line(li_fragment).expect("line fragment");

    let marker = line
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.list_style_type == ListStyleType::None)
          .unwrap_or(false)
      })
      .expect("marker fragment");
    let text = line
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.list_style_type != ListStyleType::None)
          .unwrap_or(false)
      })
      .expect("text fragment");

    assert!(marker.bounds.x() < 0.0);
    assert!(text.bounds.x() >= 0.0);
  }

  #[test]
  fn intrinsic_inline_size_splits_runs_around_block_children() {
    let bfc = BlockFormattingContext::new();
    let ifc = InlineFormattingContext::new();

    let text_left = BoxNode::new_text(default_style(), "unbreakable".to_string());
    let text_right = BoxNode::new_text(default_style(), "unbreakable".to_string());
    let block_child = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let run_container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text_left.clone()],
    );
    let run_min = ifc
      .compute_intrinsic_inline_size(&run_container, IntrinsicSizingMode::MinContent)
      .unwrap();
    let run_max = ifc
      .compute_intrinsic_inline_size(&run_container, IntrinsicSizingMode::MaxContent)
      .unwrap();

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text_left, block_child, text_right],
    );
    let min_width = bfc
      .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MinContent)
      .unwrap();
    assert!(
      min_width <= run_min * 1.1,
      "min-content width should follow the widest inline run, got {min_width} vs run {run_min}"
    );

    let max_width = bfc
      .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
      .unwrap();
    assert!(
            max_width <= run_max * 1.1,
            "max-content width should not concatenate inline runs across blocks, got {max_width} vs run {run_max}"
        );
  }

  #[test]
  fn intrinsic_inline_size_includes_inline_replaced_children() {
    let bfc = BlockFormattingContext::new();
    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Inline;
    let replaced = BoxNode::new_replaced(
      Arc::new(replaced_style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(120.0, 50.0)),
      None,
    );

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![replaced],
    );
    let min = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MinContent)
      .unwrap();
    let max = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
      .unwrap();

    assert!(
      (min - 120.0).abs() < 0.5,
      "expected min-content width ~120, got {min}"
    );
    assert!(
      (max - 120.0).abs() < 0.5,
      "expected max-content width ~120, got {max}"
    );
  }

  #[test]
  fn size_containment_zeroes_intrinsic_inline_contribution() {
    let mut style = (*default_style()).clone();
    style.containment =
      crate::style::types::Containment::with_flags(true, false, false, false, false);
    style.padding_left = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.border_left_style = BorderStyle::Solid;
    style.border_right_style = BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    let container = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Block,
      vec![BoxNode::new_text(
        default_style(),
        "superlongword".to_string(),
      )],
    );

    let bfc = BlockFormattingContext::new();
    let max = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
      .unwrap();
    let min = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MinContent)
      .unwrap();

    assert!((max - 12.0).abs() < 0.001);
    assert!((min - 12.0).abs() < 0.001);
  }

  #[test]
  fn intrinsic_percent_sized_replaced_elements_zero_out_min_content_contribution() {
    use crate::tree::box_tree::ReplacedType;

    let bfc = BlockFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = Some(Length::percent(100.0));
    style.width_keyword = None;
    style.max_width = Some(Length::percent(100.0));
    style.max_width_keyword = None;

    let mut node = BoxNode::new_replaced(
      Arc::new(style),
      ReplacedType::Canvas,
      Some(Size::new(200.0, 100.0)),
      None,
    );
    node.id = 1234;

    let (min, max) = bfc.compute_intrinsic_inline_sizes(&node).unwrap();
    assert!(
      (min - 0.0).abs() < 0.001,
      "expected min-content 0, got {min}"
    );
    assert!(
      (max - 200.0).abs() < 0.001,
      "expected max-content 200, got {max}"
    );
  }

  #[test]
  fn contain_intrinsic_size_auto_uses_remembered_size_for_intrinsic_sizing_under_containment() {
    let _guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::intrinsic_cache_use_epoch(1, true);
    crate::layout::formatting_context::intrinsic_cache_clear();

    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.containment =
      crate::style::types::Containment::with_flags(false, true, false, false, false);

    let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    node.id = 4242;

    // Pre-seed the intrinsic cache to ensure `contain-intrinsic-size: auto` bypasses stale cached
    // values when a remembered size is available.
    crate::layout::formatting_context::intrinsic_cache_store(
      &node,
      IntrinsicSizingMode::MinContent,
      0.0,
    );
    crate::layout::formatting_context::intrinsic_cache_store(
      &node,
      IntrinsicSizingMode::MaxContent,
      0.0,
    );
    crate::layout::formatting_context::remembered_size_cache_store(&node, Size::new(123.0, 456.0));

    let bfc = BlockFormattingContext::new();
    let (min, max) = bfc.compute_intrinsic_inline_sizes(&node).unwrap();
    assert!((min - 123.0).abs() < 0.001, "expected remembered min {min}");
    assert!((max - 123.0).abs() < 0.001, "expected remembered max {max}");
    assert!(
      (bfc
        .compute_intrinsic_inline_size(&node, IntrinsicSizingMode::MinContent)
        .unwrap()
        - 123.0)
        .abs()
        < 0.001
    );
    assert!(
      (bfc
        .compute_intrinsic_inline_size(&node, IntrinsicSizingMode::MaxContent)
        .unwrap()
        - 123.0)
        .abs()
        < 0.001
    );

    crate::layout::formatting_context::intrinsic_cache_use_epoch(1, true);
    crate::layout::formatting_context::intrinsic_cache_clear();
  }

  #[test]
  fn absolutely_positioned_child_uses_padding_containing_block_when_parent_positioned() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(10.0);
    parent_style.border_left_style = BorderStyle::Solid;
    parent_style.border_top_style = BorderStyle::Solid;
    parent_style.border_left_width = Length::px(2.0);
    parent_style.border_top_width = Length::px(2.0);
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.position = Position::Absolute;
    child_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    child_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    child_style.width = Some(Length::px(50.0));
    child_style.height = Some(Length::px(20.0));
    child_style.width_keyword = None;
    child_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(300.0, 300.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert_eq!(child_frag.bounds.x(), 7.0);
    assert_eq!(child_frag.bounds.y(), 9.0);
    assert_eq!(child_frag.bounds.width(), 50.0);
    assert_eq!(child_frag.bounds.height(), 20.0);
  }

  #[test]
  fn absolutely_positioned_child_uses_viewport_cb_through_collapsed_margins() {
    // Regression test for `layout/fixed-vs-viewport-001-ref.html`:
    // an absolutely positioned element with no positioned ancestors should resolve against the
    // initial containing block (viewport) even when an in-flow sibling's top margin collapses into
    // the parent and shifts the parent's border box down.
    let viewport = Size::new(200.0, 160.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(30.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(20.0));
    abs_style.width = Some(Length::px(40.0));
    abs_style.height = Some(Length::px(30.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;
    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = 3;

    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    outer_style.margin_top = Some(Length::px(90.0));
    outer_style.margin_left = Some(Length::px(100.0));
    outer_style.width = Some(Length::px(80.0));
    outer_style.height = Some(Length::px(60.0));
    outer_style.width_keyword = None;
    outer_style.height_keyword = None;
    let mut outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![]);
    outer.id = 4;

    let mut body_style = ComputedStyle::default();
    body_style.display = Display::Block;
    let mut body = BoxNode::new_block(
      Arc::new(body_style),
      FormattingContextType::Block,
      vec![abs_child, outer],
    );
    body.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![body],
    );
    // Root element margins never collapse with children; mimic the real HTML root.
    root.id = 1;

    fn find_bounds_global(node: &FragmentNode, id: usize, offset: Point) -> Option<Rect> {
      let next_offset = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
      if node.box_id() == Some(id) {
        return Some(Rect::new(next_offset, node.bounds.size));
      }
      for child in node.children.iter() {
        if let Some(found) = find_bounds_global(child, id, next_offset) {
          return Some(found);
        }
      }
      None
    }

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let body_bounds = find_bounds_global(&fragment, 2, Point::ZERO).expect("body bounds");
    assert!(
      (body_bounds.y() - 90.0).abs() < 0.01,
      "expected body to be shifted down by collapsed margin; got y={}",
      body_bounds.y()
    );

    let abs_bounds = find_bounds_global(&fragment, 3, Point::ZERO).expect("abs bounds");
    assert!(
      (abs_bounds.x() - 30.0).abs() < 0.01 && (abs_bounds.y() - 20.0).abs() < 0.01,
      "expected absolute child to stay at viewport position (30,20); got ({},{})",
      abs_bounds.x(),
      abs_bounds.y()
    );
  }

  #[test]
  fn fixed_positioned_child_uses_viewport_cb_through_offsets() {
    let viewport = Size::new(200.0, 160.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut fixed_style = ComputedStyle::default();
    fixed_style.display = Display::Block;
    fixed_style.position = Position::Fixed;
    fixed_style.left = crate::style::types::InsetValue::Length(Length::px(30.0));
    fixed_style.top = crate::style::types::InsetValue::Length(Length::px(20.0));
    fixed_style.width = Some(Length::px(40.0));
    fixed_style.height = Some(Length::px(30.0));
    fixed_style.width_keyword = None;
    fixed_style.height_keyword = None;
    let mut fixed_child =
      BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Block, vec![]);
    fixed_child.id = 4;

    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    outer_style.margin_top = Some(Length::px(90.0));
    outer_style.margin_left = Some(Length::px(100.0));
    outer_style.width = Some(Length::px(80.0));
    outer_style.height = Some(Length::px(60.0));
    outer_style.width_keyword = None;
    outer_style.height_keyword = None;
    let mut outer = BoxNode::new_block(
      Arc::new(outer_style),
      FormattingContextType::Block,
      vec![fixed_child],
    );
    outer.id = 3;

    let mut body_style = ComputedStyle::default();
    body_style.display = Display::Block;
    let mut body = BoxNode::new_block(
      Arc::new(body_style),
      FormattingContextType::Block,
      vec![outer],
    );
    body.id = 2;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![body],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let fixed_fragment = find_block_fragment(&fragment, 4).expect("fixed fragment");
    assert!(
      (fixed_fragment.bounds.x() - 30.0).abs() < 0.01
        && (fixed_fragment.bounds.y() - 20.0).abs() < 0.01,
      "expected fixed fragment to use viewport coordinates (30,20); got ({},{})",
      fixed_fragment.bounds.x(),
      fixed_fragment.bounds.y()
    );
  }

  #[test]
  fn absolutely_positioned_child_percent_top_resolves_against_auto_height_padding_box() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;
    parent_style.padding_top = Length::px(10.0);
    parent_style.padding_bottom = Length::px(10.0);

    let flow_child = BoxNode::new_block(
      block_style_with_height(100.0),
      FormattingContextType::Block,
      vec![],
    );

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::percent(50.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_id = 4242;
    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = abs_id;

    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![flow_child, abs_child],
    );

    fn find_fragment<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment(child, id) {
          return Some(found);
        }
      }
      None
    }

    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&parent, &LayoutConstraints::definite(300.0, 300.0))
      .unwrap();
    let abs_frag = find_fragment(&fragment, abs_id).expect("absolute fragment");

    assert!(
      (abs_frag.bounds.y() - 60.0).abs() < 0.01,
      "expected top:50% to resolve against 100px content height + 20px padding (y=60); got {}",
      abs_frag.bounds.y()
    );
  }

  #[test]
  fn absolutely_positioned_descendant_auto_height_fills_used_containing_block_height() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;

    let flow_child = BoxNode::new_block(
      block_style_with_height(100.0),
      FormattingContextType::Block,
      vec![],
    );

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.bottom = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height = None;
    abs_style.height_keyword = None;

    let abs_id = 4243;
    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = abs_id;

    let mut wrapper_style = ComputedStyle::default();
    wrapper_style.display = Display::Block;
    let wrapper = BoxNode::new_block(
      Arc::new(wrapper_style),
      FormattingContextType::Block,
      vec![abs_child],
    );

    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![flow_child, wrapper],
    );

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(300.0));
    root_style.width_keyword = None;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );

    fn find_fragment<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment(child, id) {
          return Some(found);
        }
      }
      None
    }

    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(300.0, 300.0))
      .unwrap();
    let abs_frag = find_fragment(&fragment, abs_id).expect("absolute fragment");

    assert!(
      (abs_frag.bounds.height() - 100.0).abs() < 0.01,
      "expected abspos descendant with top/bottom and height:auto to fill the containing block's used height (100); got {}",
      abs_frag.bounds.height()
    );
  }

  #[test]
  fn absolutely_positioned_child_width_max_content_centers_with_insets() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.position = Position::Absolute;
    child_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    child_style.right = crate::style::types::InsetValue::Length(Length::px(0.0));
    child_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    child_style.height = Some(Length::px(20.0));
    child_style.height_keyword = None;
    child_style.margin_left = None;
    child_style.margin_right = None;
    child_style.width = None;
    child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

    let text = BoxNode::new_text(default_style(), "x".to_string());
    let child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![text],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(300.0, 300.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];

    assert!(
      child_frag.bounds.width() < 199.5,
      "expected max-content width smaller than containing block; got {}",
      child_frag.bounds.width()
    );
    let expected_x = (200.0 - child_frag.bounds.width()) / 2.0;
    assert!(
      (child_frag.bounds.x() - expected_x).abs() < 0.5,
      "expected centered x≈{}, got {} (width={})",
      expected_x,
      child_frag.bounds.x(),
      child_frag.bounds.width()
    );
  }

  #[test]
  fn absolute_children_inside_block_descendants_are_laid_out() {
    // Regression: positioned children collected during block child layout were dropped.
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(400.0));
    root_style.width_keyword = None;

    let mut middle_style = ComputedStyle::default();
    middle_style.display = Display::Block;
    middle_style.width = Some(Length::px(200.0));
    middle_style.width_keyword = None;
    middle_style.padding_left = Length::px(10.0);
    middle_style.padding_top = Length::px(10.0);
    middle_style.height = Some(Length::px(80.0));
    middle_style.height_keyword = None;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    abs_style.width = Some(Length::px(30.0));
    abs_style.height = Some(Length::px(12.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let middle = BoxNode::new_block(
      Arc::new(middle_style),
      FormattingContextType::Block,
      vec![abs_child],
    );
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![middle],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(500.0, 500.0);
    let fragment = fc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let middle_frag = &fragment.children[0];
    assert_eq!(
      middle_frag.children.len(),
      1,
      "positioned child should be laid out"
    );
    let abs_frag = &middle_frag.children[0];
    // Positioned child should still be included; coordinates are resolved relative to the
    // containing block origin (padding box in our implementation).
    assert_eq!(abs_frag.bounds.x(), 5.0);
    assert_eq!(abs_frag.bounds.y(), 7.0);
    assert_eq!(abs_frag.bounds.width(), 30.0);
    assert_eq!(abs_frag.bounds.height(), 12.0);
  }

  #[test]
  fn absolute_children_inside_inline_descendants_use_updated_positioned_containing_block() {
    // Regression: BlockFormattingContext reuses a cached InlineFormattingContext when the nearest
    // positioned containing block matches. When cloning a block context for a positioned element,
    // we must rebuild that cached inline context so percentage offsets for absolutely positioned
    // descendants resolve against the new containing block (not the previous one).

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(800.0));
    root_style.width_keyword = None;

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::percent(50.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(Arc::new(inline_style), vec![abs_child]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![inline],
    );
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );

    fn find_abs_fragment(node: &FragmentNode) -> Option<&FragmentNode> {
      if node
        .style
        .as_ref()
        .map(|s| s.position == Position::Absolute)
        .unwrap_or(false)
      {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_abs_fragment(child) {
          return Some(found);
        }
      }
      None
    }

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&root, &constraints).unwrap();

    let abs_fragment =
      find_abs_fragment(&fragment).expect("expected absolute-positioned descendant");
    assert_eq!(
      abs_fragment.bounds.x(),
      100.0,
      "left:50% should resolve against the positioned 200px-wide containing block"
    );
  }

  #[test]
  fn inline_absolute_children_use_updated_nearest_positioned_cb() {
    fn find_fragment_by_box_id<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment_by_box_id(child, id) {
          return Some(found);
        }
      }
      None
    }

    let viewport = Size::new(300.0, 300.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Block;
    container_style.width = Some(Length::px(200.0));
    container_style.width_keyword = None;
    // Non-empty transforms establish a new positioned containing block. The block formatting
    // context clones itself when entering such a subtree; ensure the shared inline formatting
    // context is rebuilt with the updated `nearest_positioned_cb`.
    container_style.transform = vec![Transform::TranslateX(Length::px(0.0))];

    let mut wrapper_style = ComputedStyle::default();
    wrapper_style.display = Display::Inline;

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::percent(50.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(20.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_id = 9001;
    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = abs_id;

    let inline_wrapper = BoxNode::new_inline(
      Arc::new(wrapper_style),
      vec![
        BoxNode::new_text(Arc::new(text_style), "hi".to_string()),
        abs_child,
      ],
    );
    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Block,
      vec![inline_wrapper],
    );
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![container],
    );

    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(300.0, 300.0))
      .unwrap();

    let abs_fragment = find_fragment_by_box_id(&fragment, abs_id).expect("positioned fragment");
    assert!(
      (abs_fragment.bounds.x() - 100.0).abs() < 0.01,
      "left:50% should resolve against the transformed 200px-wide containing block, got {}",
      abs_fragment.bounds.x()
    );
  }

  #[test]
  fn table_cell_intrinsic_width_uses_inline_children_path() {
    let mut cell_style = ComputedStyle::default();
    cell_style.display = Display::TableCell;
    cell_style.font_size = 16.0;
    let cell_style = Arc::new(cell_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let text1 = BoxNode::new_text(text_style.clone(), "hello ".to_string());
    let text2 = BoxNode::new_text(text_style, "world".to_string());
    let cell = BoxNode::new_block(
      cell_style.clone(),
      FormattingContextType::Block,
      vec![text1, text2],
    );

    let fc = BlockFormattingContext::new();
    let inline_fc = InlineFormattingContext::with_factory(fc.child_factory());
    let inline_container = BoxNode::new_inline(cell_style, cell.children.clone());

    let expected_min = inline_fc
      .compute_intrinsic_inline_size(&inline_container, IntrinsicSizingMode::MinContent)
      .expect("inline min");
    let expected_max = inline_fc
      .compute_intrinsic_inline_size(&inline_container, IntrinsicSizingMode::MaxContent)
      .expect("inline max");

    let actual_min = fc
      .compute_intrinsic_inline_size(&cell, IntrinsicSizingMode::MinContent)
      .expect("block min");
    let actual_max = fc
      .compute_intrinsic_inline_size(&cell, IntrinsicSizingMode::MaxContent)
      .expect("block max");

    assert!(
      (actual_min - expected_min).abs() < 0.01,
      "min-content: expected {}, got {}",
      expected_min,
      actual_min
    );
    assert!(
      (actual_max - expected_max).abs() < 0.01,
      "max-content: expected {}, got {}",
      expected_max,
      actual_max
    );
  }

  fn find_block_fragment<'a>(
    fragment: &'a FragmentNode,
    box_id: usize,
  ) -> Option<&'a FragmentNode> {
    if let FragmentContent::Block {
      box_id: Some(found),
    } = &fragment.content
    {
      if *found == box_id {
        return Some(fragment);
      }
    }
    for child in fragment.children.iter() {
      if let Some(found) = find_block_fragment(child, box_id) {
        return Some(found);
      }
    }
    None
  }

  #[test]
  fn content_visibility_auto_translates_paint_viewport_through_nested_offsets() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(400.0),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(50.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 5;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a deterministic placeholder block-size so `content-visibility:auto` can skip layout
    // when offscreen. Without this, the default `contain-intrinsic-size:auto` has no fallback
    // length and we keep laying out descendants to avoid collapsing the element to 0px.
    auto_style.contain_intrinsic_height.length = Some(Length::px(50.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 4;

    let mut wrapper = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    wrapper.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, wrapper],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 4).expect("auto fragment");
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when translated viewport is offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_without_placeholder_does_not_skip_layout() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(400.0),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(50.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 4;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, auto_box],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 3).expect("auto fragment");
    assert!(
      auto_fragment.bounds.y() > viewport.height,
      "expected auto subtree to start below the viewport (y={} height={})",
      auto_fragment.bounds.y(),
      viewport.height
    );
    assert!(
      !auto_fragment.children.is_empty(),
      "expected descendants to be laid out when no placeholder is available"
    );
  }

  #[test]
  fn content_visibility_auto_skips_when_fully_outside_inline_axis() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 12;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.margin_left = Some(Length::px(500.0));
    auto_style.width = Some(Length::px(100.0));
    auto_style.width_keyword = None;
    // Ensure auto skipping uses a non-content placeholder block-size.
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 11;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    root.id = 10;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 11).expect("auto fragment");
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when the border box is outside the inline axis"
    );
  }

  #[test]
  fn content_visibility_auto_respects_vertical_writing_mode() {
    let _toggles = content_visibility_test_guard();
    // Choose a viewport where physical height > width so vertical writing mode mapping matters:
    // the logical block size should come from the physical width (50), not height (100).
    let viewport = Size::new(50.0, 100.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.writing_mode = WritingMode::VerticalRl;
    let parent = BoxNode::new_block(Arc::new(parent_style), FormattingContextType::Block, vec![]);

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 22;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.writing_mode = WritingMode::VerticalRl;
    auto_style.content_visibility = ContentVisibility::Auto;
    // For vertical writing modes, the logical block axis maps to the physical inline axis, so the
    // placeholder must come from the corresponding contain-intrinsic axis.
    auto_style.contain_intrinsic_width.length = Some(Length::px(10.0));
    let auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );

    let containing_width = viewport.height;
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(containing_width),
      AvailableSpace::Indefinite,
    );
    let paint_viewport = paint_viewport_for(WritingMode::VerticalRl, Direction::Ltr, viewport);
    let current_y = viewport.width + 10.0;
    let nearest_cb = ContainingBlock::viewport(viewport);

    let fragment = fc
      .layout_block_child(
        &parent,
        &auto_box,
        containing_width,
        &constraints,
        current_y,
        &nearest_cb,
        &nearest_cb,
        None,
        0.0,
        0.0,
        paint_viewport,
      )
      .expect("layout_block_child");

    assert!(
      fragment.children.is_empty(),
      "expected descendants to be skipped in vertical writing modes when offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_skips_descendants_with_fixed_height_through_nested_offsets() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(300.0),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 5;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.height = Some(Length::px(50.0));
    auto_style.height_keyword = None;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 4;

    let mut wrapper = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    wrapper.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, wrapper],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");

    let wrapper_fragment = find_block_fragment(&fragment, 3).expect("wrapper fragment");
    assert!(
      wrapper_fragment.bounds.y() > viewport.height,
      "expected wrapper subtree to be positioned below the paint viewport"
    );

    let auto_fragment = find_block_fragment(&fragment, 4).expect("auto fragment");
    assert!(
      auto_fragment.bounds.y().abs() < 0.1,
      "expected auto fragment to have a small local block offset (y={})",
      auto_fragment.bounds.y()
    );
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when translated viewport is offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_skips_descendants_with_fixed_height_outside_inline_axis() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 12;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.margin_left = Some(Length::px(viewport.width + 10.0));
    auto_style.width = Some(Length::px(50.0));
    auto_style.width_keyword = None;
    auto_style.height = Some(Length::px(10.0));
    auto_style.height_keyword = None;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 11;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    root.id = 10;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 11).expect("auto fragment");
    assert!(
      auto_fragment.bounds.x() > viewport.width,
      "expected auto fragment to be positioned outside the inline axis (x={})",
      auto_fragment.bounds.x()
    );
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when the border box is outside the inline axis"
    );
  }

  #[test]
  fn content_visibility_auto_skips_descendants_in_vertical_writing_mode_with_spacer_offset() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(50.0, 100.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite(viewport.width, viewport.height);

    let spacer_style = {
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style.writing_mode = WritingMode::VerticalLr;
      // In vertical writing modes, the block axis is horizontal (physical width), so set `width`
      // to push subsequent siblings beyond the viewport block axis.
      style.width = Some(Length::px(viewport.width + 10.0));
      style.width_keyword = None;
      Arc::new(style)
    };
    let mut spacer = BoxNode::new_block(spacer_style, FormattingContextType::Block, vec![]);
    spacer.id = 2;

    let leaf_style = {
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style.writing_mode = WritingMode::VerticalLr;
      style.width = Some(Length::px(10.0));
      style.width_keyword = None;
      Arc::new(style)
    };
    let mut leaf = BoxNode::new_block(leaf_style, FormattingContextType::Block, vec![]);
    leaf.id = 5;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.writing_mode = WritingMode::VerticalLr;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.width = Some(Length::px(10.0));
    auto_style.width_keyword = None;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 4;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.writing_mode = WritingMode::VerticalLr;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![spacer, auto_box],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 4).expect("auto fragment");
    assert!(
      auto_fragment.bounds.x() > viewport.width,
      "expected auto fragment to be positioned beyond the viewport block axis (x={})",
      auto_fragment.bounds.x()
    );
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped in vertical writing mode when offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_accounts_for_viewport_scroll() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let scroll = Point::new(0.0, 300.0);
    let constraints = LayoutConstraints::definite(viewport.width, viewport.height);

    let fc_no_scroll = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(scroll.y),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 4;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a deterministic placeholder so offscreen auto content can be skipped pre-scroll.
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, auto_box],
    );
    root.id = 1;

    let fragment_no_scroll = fc_no_scroll.layout(&root, &constraints).expect("layout");
    let auto_fragment_no_scroll =
      find_block_fragment(&fragment_no_scroll, 3).expect("auto fragment");
    assert!(
      auto_fragment_no_scroll.children.is_empty(),
      "expected descendants to be skipped before scrolling"
    );

    let factory = crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    )
    .with_viewport_scroll(scroll);
    let fc = BlockFormattingContext::with_factory(factory);
    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 3).expect("auto fragment");
    assert!(
      !auto_fragment.children.is_empty(),
      "expected scrolled viewport to keep descendants active"
    );
  }

  #[test]
  fn content_visibility_auto_uses_viewport_scroll_when_constraints_are_not_viewport_sized() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    // The scroll is expressed in the nested formatting context's coordinate space. A negative
    // scroll offset corresponds to the nested context being shifted positively relative to the
    // viewport.
    let scroll = Point::new(-300.0, 0.0);
    // Use constraints that do not match the viewport size to mirror nested layout calls (e.g., flex
    // items, table cells) that still need viewport-relative `content-visibility:auto` decisions.
    let constraints = LayoutConstraints::definite(100.0, viewport.height);

    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        FontContext::new(),
        viewport,
        ContainingBlock::viewport(viewport),
      )
      .with_viewport_scroll(scroll);
    let fc = BlockFormattingContext::with_factory(factory);

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 3;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a deterministic placeholder so offscreen auto content can be skipped.
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 2;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 2).expect("auto fragment");
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when the viewport does not intersect the translated subtree",
    );
  }

  #[test]
  fn floats_protrude_out_of_non_bfc_blocks_and_affect_following_siblings() {
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let nearest_cb = ContainingBlock::viewport(viewport);
    let constraints = LayoutConstraints::definite_width(viewport.width);

    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    let outer_style = Arc::new(outer_style);

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(40.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_box = BoxNode::new_replaced(
      Arc::new(float_style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(40.0, 20.0)),
      None,
    );
    let float_container = BoxNode::new_block(
      block_style.clone(),
      FormattingContextType::Block,
      vec![float_box],
    );

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    let text = BoxNode::new_text(Arc::new(text_style), "hi".to_string());
    let text_container = BoxNode::new_block(block_style, FormattingContextType::Block, vec![text]);

    let outer = BoxNode::new_block(
      outer_style,
      FormattingContextType::Block,
      vec![float_container, text_container],
    );

    let paint_viewport =
      paint_viewport_for(outer.style.writing_mode, outer.style.direction, viewport);
    let mut float_ctx = FloatContext::new(viewport.width);
    let (fragments, _height, _positioned) = fc
      .layout_children_with_external_floats(
        &outer,
        &constraints,
        &nearest_cb,
        &nearest_cb,
        paint_viewport,
        Some(&mut float_ctx),
        0.0,
        0.0,
      )
      .expect("layout children");

    assert_eq!(fragments.len(), 2);
    assert!(
      fragments[0].bounds.height().abs() < 0.1,
      "expected float container height to ignore floats, got {:.2}",
      fragments[0].bounds.height()
    );
    assert!(
      fragments[1].bounds.y().abs() < 0.1,
      "expected following sibling to start at y=0, got {:.2}",
      fragments[1].bounds.y()
    );

    let (left_edge, available_width) = float_ctx.available_width_at_y(0.0);
    assert!(
      (left_edge - 40.0).abs() < 0.5,
      "expected left edge to be pushed past float (≈40px), got {:.2}",
      left_edge
    );
    assert!(
      (available_width - (viewport.width - 40.0)).abs() < 0.5,
      "expected available width to shrink (≈{:.2}px), got {:.2}",
      viewport.width - 40.0,
      available_width
    );
  }

  #[test]
  fn bfc_roots_avoid_overlapping_external_floats() {
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let nearest_cb = ContainingBlock::viewport(viewport);
    let constraints = LayoutConstraints::definite_width(viewport.width);

    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    let outer_style = Arc::new(outer_style);

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(40.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_box = BoxNode::new_replaced(
      Arc::new(float_style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(40.0, 20.0)),
      None,
    );
    let float_container = BoxNode::new_block(
      block_style.clone(),
      FormattingContextType::Block,
      vec![float_box],
    );

    let mut bfc_style = ComputedStyle::default();
    bfc_style.display = Display::Block;
    bfc_style.overflow_x = Overflow::Hidden;
    bfc_style.width = Some(Length::px(20.0));
    bfc_style.width_keyword = None;
    bfc_style.height = Some(Length::px(10.0));
    bfc_style.height_keyword = None;
    let bfc_style = Arc::new(bfc_style);

    let mut inner_text_style = ComputedStyle::default();
    inner_text_style.display = Display::Inline;
    let text = BoxNode::new_text(Arc::new(inner_text_style), "hi".to_string());
    let bfc_container = BoxNode::new_block(bfc_style, FormattingContextType::Block, vec![text]);

    let outer = BoxNode::new_block(
      outer_style,
      FormattingContextType::Block,
      vec![float_container, bfc_container],
    );

    let paint_viewport =
      paint_viewport_for(outer.style.writing_mode, outer.style.direction, viewport);
    let mut float_ctx = FloatContext::new(viewport.width);
    let (fragments, _height, _positioned) = fc
      .layout_children_with_external_floats(
        &outer,
        &constraints,
        &nearest_cb,
        &nearest_cb,
        paint_viewport,
        Some(&mut float_ctx),
        0.0,
        0.0,
      )
      .expect("layout children");

    assert_eq!(fragments.len(), 2);
    assert!(
      fragments[0].bounds.height().abs() < 0.1,
      "expected float container height to ignore floats, got {:.2}",
      fragments[0].bounds.height()
    );
    assert!(
      fragments[1].bounds.y().abs() < 0.1,
      "expected following sibling to start at y=0, got {:.2}",
      fragments[1].bounds.y()
    );
    assert!(
      (fragments[1].bounds.x() - 40.0).abs() < 0.5,
      "expected BFC root to be shifted past the float, got {:.2}",
      fragments[1].bounds.x()
    );
  }

  #[test]
  fn sibling_margin_collapsing_is_not_broken_by_spurious_clearance() {
    // Regression: clearance calculations in an external float context used to compute the
    // post-clearance margin-edge position via `(base + y) - base`. With certain `base` values this
    // produced tiny positive rounding errors even when `clear:none`, triggering the clearance path
    // and breaking sibling margin collapsing (margins summed instead of collapsing).
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let nearest_cb = ContainingBlock::viewport(viewport);
    let constraints = LayoutConstraints::definite_width(viewport.width);

    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    let outer_style = Arc::new(outer_style);

    let mut first_style = ComputedStyle::default();
    first_style.display = Display::Block;
    first_style.height = Some(Length::px(1.0));
    first_style.height_keyword = None;
    first_style.margin_bottom = Some(Length::px(23.1));
    let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

    let mut second_style = ComputedStyle::default();
    second_style.display = Display::Block;
    second_style.height = Some(Length::px(1.0));
    second_style.height_keyword = None;
    second_style.margin_top = Some(Length::px(21.0));
    let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

    let outer = BoxNode::new_block(
      outer_style,
      FormattingContextType::Block,
      vec![first, second],
    );

    let paint_viewport =
      paint_viewport_for(outer.style.writing_mode, outer.style.direction, viewport);
    let mut float_ctx = FloatContext::new(viewport.width);

    // Ensure this test covers the rounding behavior that used to trigger spurious clearance.
    let float_base_y: f32 = 7958.390625;
    let margin_edge_y: f32 = 24.1;
    let cleared_margin_edge_y = (float_base_y + margin_edge_y) - float_base_y;
    let clearance = (cleared_margin_edge_y - margin_edge_y).max(0.0_f32);
    assert!(
      clearance > 0.0,
      "test precondition failed: expected cancellation to produce spurious clearance, got {clearance}"
    );

    let (fragments, _height, _positioned) = fc
      .layout_children_with_external_floats(
        &outer,
        &constraints,
        &nearest_cb,
        &nearest_cb,
        paint_viewport,
        Some(&mut float_ctx),
        0.0,
        float_base_y,
      )
      .expect("layout children");

    assert_eq!(fragments.len(), 2);
    assert!(
      (fragments[1].bounds.y() - 24.1).abs() < 0.01,
      "expected sibling margins to collapse (gap=max(23.1,21)), got y={}",
      fragments[1].bounds.y()
    );
  }

  #[test]
  fn block_layout_reuses_shaping_pipeline_across_children() {
    // Regression: block layout used to instantiate a new shaping pipeline for each block box that
    // buffered inline children, preventing font fallback caches from being reused across blocks.
    crate::text::pipeline::ShapingPipeline::debug_reset_new_call_count();

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.font_size = 16.0;
    let root_style = Arc::new(root_style);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.font_size = 16.0;
    let child_style = Arc::new(child_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let children = (0..64usize)
      .map(|idx| {
        let text = BoxNode::new_text(text_style.clone(), format!("hello {idx}"));
        BoxNode::new_block(
          child_style.clone(),
          FormattingContextType::Block,
          vec![text],
        )
      })
      .collect();
    let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);

    let factory = FormattingContextFactory::new().with_parallelism(LayoutParallelism::disabled());
    let fc = factory.create(FormattingContextType::Block);
    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let _fragment = fc.layout(&root, &constraints).expect("layout");

    assert_eq!(
      crate::text::pipeline::ShapingPipeline::debug_new_call_count(),
      1
    );
  }

  #[test]
  fn intrinsic_block_size_probe_does_not_fallback_to_viewport_width() {
    // Regression: intrinsic block-size probes used to inflate nested layout when the min-content
    // inline size was 0px. A "near-zero containing width" safeguard would fall back to the
    // viewport size during descendant layout, producing huge intrinsic block sizes (e.g. square
    // images sized with `width: 100%` would report an intrinsic height equal to the viewport
    // inline size).

    // Outer wrapper whose min-content inline size becomes 0 due to percentage-based descendants.
    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    let outer_style = Arc::new(outer_style);

    // Intermediate block so the descendant layout path is exercised (the safeguard triggers on
    // this element before laying out the replaced child).
    let mut middle_style = ComputedStyle::default();
    middle_style.display = Display::Block;
    let middle_style = Arc::new(middle_style);

    // Replaced element sized with percentages; with a 0px percentage base its used size should be
    // 0x0, not expanded to the viewport.
    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Block;
    replaced_style.width = Some(Length::percent(100.0));
    let replaced_style = Arc::new(replaced_style);
    let replaced = BoxNode::new_replaced(
      replaced_style,
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(1000.0, 1000.0)),
      Some(1.0),
    );

    let middle = BoxNode::new_block(middle_style, FormattingContextType::Block, vec![replaced]);
    let outer = BoxNode::new_block(outer_style, FormattingContextType::Block, vec![middle]);

    let factory = FormattingContextFactory::new().with_parallelism(LayoutParallelism::disabled());
    let fc = factory.create(FormattingContextType::Block);

    let min_block = fc
      .compute_intrinsic_block_size(&outer, IntrinsicSizingMode::MinContent)
      .expect("intrinsic block size");

    assert!(
      min_block < 1.0,
      "expected intrinsic min-content block size to remain near 0px, got {:.2}",
      min_block
    );
  }
}
