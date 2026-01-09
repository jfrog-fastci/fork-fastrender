//! Fragmentation utilities
//!
//! Pagination and multi-column output require splitting a laid-out fragment tree
//! into fragmentainers (pages/columns). Fragmentation happens in the block axis
//! and respects authored break hints (`break-before/after/inside`), widows/orphans
//! constraints, and line-level break opportunities. The fragment tree that comes
//! out of layout is treated as flow order; this module decides where to break and
//! clones the appropriate fragment subtrees for each fragmentainer.

use std::sync::OnceLock;

use crate::error::RenderStage;
use crate::geometry::{Point, Rect};
use crate::layout::axis::{FragmentAxes, PhysicalAxis};
use crate::layout::formatting_context::LayoutError;
use crate::render_control::check_active;
use crate::style::display::Display;
use crate::style::page::PageSide;
use crate::style::position::Position;
use crate::style::types::{BreakBetween, BreakInside, Direction, FlexDirection, WritingMode};
use crate::style::{
  block_axis_is_horizontal, block_axis_positive, inline_axis_positive, ComputedStyle,
};
use crate::tree::fragment_tree::{
  FragmentChildren, FragmentContent, FragmentNode, FragmentSliceInfo, GridItemFragmentationData,
  GridTrackRanges,
};

/// The fragmentation context determines how break hints are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentationContext {
  /// Fragmentation across pages.
  Page,
  /// Fragmentation across columns.
  Column,
}

/// Options controlling how fragments are split across fragmentainers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FragmentationOptions {
  /// Block-size of each fragmentainer (page/column).
  pub fragmentainer_size: f32,
  /// Gap between successive fragmentainers when translated into absolute space.
  pub fragmentainer_gap: f32,
  /// Number of columns to target during fragmentation.
  pub column_count: usize,
  /// Gap between columns.
  pub column_gap: f32,
}

impl FragmentationOptions {
  /// Creates a new set of fragmentation options for a given fragmentainer size.
  pub fn new(fragmentainer_size: f32) -> Self {
    Self {
      fragmentainer_size,
      fragmentainer_gap: 0.0,
      column_count: 1,
      column_gap: 0.0,
    }
  }

  /// Sets a gap between fragmentainers (useful for pagination).
  pub fn with_gap(mut self, gap: f32) -> Self {
    self.fragmentainer_gap = gap.max(0.0);
    self
  }

  /// Configures column count and gap.
  pub fn with_columns(mut self, count: usize, gap: f32) -> Self {
    self.column_count = count.max(1);
    self.column_gap = gap.max(0.0);
    self
  }
}

impl Default for FragmentationOptions {
  fn default() -> Self {
    Self::new(0.0)
  }
}

/// Computes the inline size available to each column when fragmenting into multiple columns.
///
/// The result subtracts total inter-column gaps from the initial containing block width and
/// divides the remainder evenly across columns. A minimum of one column is enforced to avoid
/// division by zero when callers construct `FragmentationOptions` manually.
pub fn column_inline_size(icb_width: f32, options: &FragmentationOptions) -> f32 {
  let count = options.column_count.max(1) as f32;
  if count <= 1.0 {
    return icb_width;
  }

  let gaps = options.column_gap.max(0.0) * (count - 1.0);
  let available = (icb_width - gaps).max(0.0);
  available / count
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakStrength {
  Forced,
  Auto,
  Avoid,
}

#[derive(Debug, Clone, PartialEq)]
enum BreakKind {
  BetweenSiblings,
  LineBoundary {
    container_id: usize,
    line_index_end: usize,
  },
  EndOfContent,
}

#[derive(Debug, Clone)]
struct BreakOpportunity {
  pos: f32,
  strength: BreakStrength,
  kind: BreakKind,
}

fn max_break_strength(a: BreakStrength, b: BreakStrength) -> BreakStrength {
  match (a, b) {
    (BreakStrength::Forced, _) | (_, BreakStrength::Forced) => BreakStrength::Forced,
    (BreakStrength::Avoid, _) | (_, BreakStrength::Avoid) => BreakStrength::Avoid,
    _ => BreakStrength::Auto,
  }
}

#[derive(Debug, Clone)]
struct LineContainer {
  id: usize,
  line_ends: Vec<f32>,
  widows: usize,
  orphans: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicRange {
  pub(crate) start: f32,
  pub(crate) end: f32,
}

#[derive(Debug, Clone, Copy)]
struct AtomicCandidate {
  range: AtomicRange,
  /// The minimum fragmentainer block-size needed for this range to be treated as atomic.
  ///
  /// This can differ from `range.end - range.start` when the atomic range is widened to cover
  /// adjacent gutters (e.g. grid track ranges that absorb a preceding row/column gap).
  required_fragmentainer_size: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ForcedBoundary {
  pub position: f32,
  pub page_side: Option<PageSide>,
}

#[derive(Default, Debug)]
struct BreakCollection {
  opportunities: Vec<BreakOpportunity>,
  line_containers: Vec<LineContainer>,
  atomic: Vec<AtomicRange>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FragmentAxis {
  pub(crate) block_is_horizontal: bool,
  pub(crate) block_positive: bool,
}

impl FragmentAxis {
  fn from_writing_mode(mode: WritingMode) -> Self {
    Self {
      block_is_horizontal: block_axis_is_horizontal(mode),
      block_positive: block_axis_positive(mode),
    }
  }

  pub(crate) fn block_size(&self, rect: &Rect) -> f32 {
    if self.block_is_horizontal {
      rect.width()
    } else {
      rect.height()
    }
  }

  pub(crate) fn inline_size(&self, rect: &Rect) -> f32 {
    if self.block_is_horizontal {
      rect.height()
    } else {
      rect.width()
    }
  }

  fn block_start(&self, rect: &Rect) -> f32 {
    if self.block_is_horizontal {
      rect.x()
    } else {
      rect.y()
    }
  }

  fn inline_start(&self, rect: &Rect) -> f32 {
    if self.block_is_horizontal {
      rect.y()
    } else {
      rect.x()
    }
  }

  fn flow_offset(&self, physical_block_start: f32, block_size: f32, parent_block_size: f32) -> f32 {
    if self.block_positive {
      physical_block_start
    } else {
      parent_block_size - physical_block_start - block_size
    }
  }

  pub(crate) fn flow_range(
    &self,
    parent_abs_flow_start: f32,
    parent_block_size: f32,
    rect: &Rect,
  ) -> (f32, f32) {
    let offset = self.flow_offset(
      self.block_start(rect),
      self.block_size(rect),
      parent_block_size,
    );
    let start = parent_abs_flow_start + offset;
    (start, start + self.block_size(rect))
  }

  fn flow_point_to_physical(&self, flow_offset: f32, parent_block_size: f32) -> f32 {
    if self.block_positive {
      flow_offset
    } else {
      parent_block_size - flow_offset
    }
  }

  fn flow_box_start_to_physical(
    &self,
    flow_offset: f32,
    block_size: f32,
    parent_block_size: f32,
  ) -> f32 {
    if self.block_positive {
      flow_offset
    } else {
      parent_block_size - flow_offset - block_size
    }
  }

  fn update_block_components(&self, rect: Rect, block_start: f32, block_size: f32) -> Rect {
    if self.block_is_horizontal {
      Rect::from_xywh(block_start, rect.y(), block_size, rect.height())
    } else {
      Rect::from_xywh(rect.x(), block_start, rect.width(), block_size)
    }
  }

  fn block_translation(&self, delta_flow: f32) -> Point {
    let delta = if self.block_positive {
      delta_flow
    } else {
      -delta_flow
    };
    if self.block_is_horizontal {
      Point::new(delta, 0.0)
    } else {
      Point::new(0.0, delta)
    }
  }
}

pub(crate) fn fragmentation_axis(root: &FragmentNode) -> FragmentAxis {
  let writing_mode = root
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  FragmentAxis::from_writing_mode(writing_mode)
}

fn axis_for_child_in_context(
  parent_axis: &FragmentAxis,
  _context: FragmentationContext,
  _parent_writing_mode: WritingMode,
  _child_writing_mode: WritingMode,
) -> FragmentAxis {
  *parent_axis
}

fn axis_from_fragment_axes(axes: FragmentAxes) -> FragmentAxis {
  FragmentAxis {
    block_is_horizontal: axes.block_axis() == PhysicalAxis::X,
    block_positive: axes.block_positive(),
  }
}

fn axes_from_root(root: &FragmentNode) -> FragmentAxes {
  let (writing_mode, direction) = root
    .style
    .as_ref()
    .map(|s| (s.writing_mode, s.direction))
    .unwrap_or((WritingMode::HorizontalTb, Direction::Ltr));
  FragmentAxes::from_writing_mode_and_direction(writing_mode, direction)
}

fn check_layout_deadline() -> Result<(), LayoutError> {
  if let Err(crate::error::RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout)
  {
    return Err(LayoutError::Timeout { elapsed });
  }
  Ok(())
}

fn grid_item_parallel_flow_required_block_size(
  item: &FragmentNode,
  axes: FragmentAxes,
  fragmentainer_size: f32,
) -> f32 {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return axis_from_fragment_axes(axes).block_size(&item.bounds);
  }

  let axis = axis_from_fragment_axes(axes);
  let item_block_size = axis.block_size(&item.bounds);
  if item_block_size <= BREAK_EPSILON {
    return item_block_size.max(0.0);
  }

  let mut boundaries = collect_forced_boundaries_with_axes(item, 0.0, axes);
  if boundaries.is_empty() {
    return item_block_size;
  }

  let mut positions: Vec<f32> = boundaries.drain(..).map(|b| b.position).collect();
  positions.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  positions.dedup_by(|a, b| (*a - *b).abs() < BREAK_EPSILON);

  // Model forced breaks as inserting blank space up to the next fragmentainer boundary. This matches
  // the spec language that a forced break "effectively increases the size of its contents" (CSS
  // Grid 2 §Fragmenting Grid Layout).
  let mut shift = 0.0f32;
  for pos in positions {
    if pos <= BREAK_EPSILON {
      continue;
    }
    if pos >= item_block_size - BREAK_EPSILON {
      continue;
    }
    let effective = pos + shift;
    let remainder = effective.rem_euclid(fragmentainer_size);
    // If the forced break already aligns to a fragmentainer boundary, no extra space is needed.
    let advance = (fragmentainer_size - remainder).rem_euclid(fragmentainer_size);
    shift += advance;
  }

  (item_block_size + shift).max(item_block_size)
}

#[derive(Debug, Clone)]
struct ParallelFlowShiftMap {
  /// Sorted list of (break_position, cumulative_shift_after_break).
  ///
  /// Positions are expressed in the *original* flow coordinate system.
  breaks: Vec<(f32, f32)>,
}

impl ParallelFlowShiftMap {
  fn for_forced_breaks(mut positions: Vec<f32>, fragmentainer_size: f32) -> Option<Self> {
    if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
      return None;
    }
    if positions.is_empty() {
      return None;
    }

    positions.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    positions.dedup_by(|a, b| (*a - *b).abs() < BREAK_EPSILON);

    let mut shift = 0.0f32;
    let mut breaks = Vec::new();
    for pos in positions {
      let effective = pos + shift;
      let remainder = effective.rem_euclid(fragmentainer_size);
      // If the forced break already aligns to a fragmentainer boundary, no extra space is needed.
      let advance = (fragmentainer_size - remainder).rem_euclid(fragmentainer_size);
      if advance <= BREAK_EPSILON {
        continue;
      }
      shift += advance;
      breaks.push((pos, shift));
    }
    if breaks.is_empty() {
      return None;
    }
    Some(Self { breaks })
  }

  fn shift_for(&self, pos: f32) -> f32 {
    if self.breaks.is_empty() {
      return 0.0;
    }
    let idx = self
      .breaks
      .partition_point(|(break_pos, _)| *break_pos <= pos + BREAK_EPSILON);
    if idx == 0 {
      0.0
    } else {
      self.breaks[idx - 1].1
    }
  }
}

fn apply_parallel_flow_shifts_to_descendants(
  node: &mut FragmentNode,
  abs_start: f32,
  parent_shift: f32,
  axis: &FragmentAxis,
  shifts: &ParallelFlowShiftMap,
) {
  let node_block_size = axis.block_size(&node.bounds);
  for child in node.children_mut().iter_mut() {
    let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
    let child_shift = shifts.shift_for(child_abs_start);
    let delta = child_shift - parent_shift;
    if delta.abs() > BREAK_EPSILON {
      translate_fragment_in_parent_space(child, axis.block_translation(delta));
    }
    apply_parallel_flow_shifts_to_descendants(child, child_abs_start, child_shift, axis, shifts);
  }
}

pub(crate) fn apply_grid_parallel_flow_forced_break_shifts(
  root: &mut FragmentNode,
  axes: FragmentAxes,
  fragmentainer_size: f32,
) {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return;
  }

  let axis = axis_from_fragment_axes(axes);
  let default_style = default_style();

  fn walk(
    node: &mut FragmentNode,
    abs_start: f32,
    axis: &FragmentAxis,
    axes: FragmentAxes,
    fragmentainer_size: f32,
    default_style: &ComputedStyle,
  ) {
    let style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let node_block_size = axis.block_size(&node.bounds);

    if matches!(style.display, Display::Grid | Display::InlineGrid) {
      if let Some(grid_info) = node.grid_fragmentation.clone() {
        let in_flow_count = grid_info.items.len().min(node.children.len());
        for idx in 0..in_flow_count {
          let placement = &grid_info.items[idx];
          if !grid_item_spans_single_track(placement, axis) {
            continue;
          }
          let Some(child) = node.children_mut().get_mut(idx) else {
            continue;
          };

          let child_block_size = axis.block_size(&child.bounds);
          if child_block_size <= BREAK_EPSILON {
            continue;
          }

          // Discover forced breaks inside this grid item and model them as inserting blank space up
          // to the next fragmentainer boundary (CSS Grid 2 §Fragmenting Grid Layout).
          let mut boundaries = collect_forced_boundaries_with_axes(child, 0.0, axes);
          if boundaries.is_empty() {
            continue;
          }
          let mut positions: Vec<f32> = boundaries.drain(..).map(|b| b.position).collect();
          positions.retain(|p| *p > BREAK_EPSILON && *p < child_block_size - BREAK_EPSILON);
          let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size)
          else {
            continue;
          };

          apply_parallel_flow_shifts_to_descendants(child, 0.0, 0.0, axis, &shifts);
        }
      }
    }

    for child in node.children_mut().iter_mut() {
      let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
      walk(
        child,
        child_abs_start,
        axis,
        axes,
        fragmentainer_size,
        default_style,
      );
    }
  }

  walk(root, 0.0, &axis, axes, fragmentainer_size, default_style);
}

pub(crate) fn apply_float_parallel_flow_forced_break_shifts(
  root: &mut FragmentNode,
  axes: FragmentAxes,
  fragmentainer_size: f32,
  context: FragmentationContext,
) {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return;
  }

  let axis = axis_from_fragment_axes(axes);
  let default_style = default_style();
  let root_writing_mode = root
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);

  fn walk(
    node: &mut FragmentNode,
    abs_start: f32,
    axis: &FragmentAxis,
    fragmentainer_size: f32,
    context: FragmentationContext,
    inherited_writing_mode: WritingMode,
    default_style: &ComputedStyle,
  ) {
    let style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let node_writing_mode = node
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(inherited_writing_mode);
    let node_block_size = axis.block_size(&node.bounds);

    if style.float.is_floating() {
      // Forced breaks inside floats form a parallel fragmentation flow. They should not force page
      // breaks for the main flow, but instead insert blank space up to the next fragmentainer
      // boundary, increasing the float's effective height (CSS Break 3 §Parallel Fragmentation
      // Flows).
      let mut collection = BreakCollection::default();
      collect_break_opportunities(
        node,
        abs_start,
        &mut collection,
        0,
        0,
        context,
        axis,
        node_writing_mode,
        false,
      );

      let float_end = abs_start + node_block_size;
      let mut positions: Vec<f32> = collection
        .opportunities
        .into_iter()
        .filter(|o| matches!(o.strength, BreakStrength::Forced))
        .map(|o| o.pos)
        .collect();
      positions.retain(|p| *p > abs_start + BREAK_EPSILON && *p < float_end - BREAK_EPSILON);
      if let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size) {
        apply_parallel_flow_shifts_to_descendants(node, abs_start, 0.0, axis, &shifts);
      }

      // Nested floats are already accounted for when collecting break opportunities inside this
      // float. Avoid applying shifts multiple times by not recursing into descendants.
      return;
    }

    for child in node.children_mut().iter_mut() {
      let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
      let child_writing_mode = child
        .style
        .as_ref()
        .map(|s| s.writing_mode)
        .unwrap_or(node_writing_mode);
      walk(
        child,
        child_abs_start,
        axis,
        fragmentainer_size,
        context,
        child_writing_mode,
        default_style,
      );
    }
  }

  walk(
    root,
    0.0,
    &axis,
    fragmentainer_size,
    context,
    root_writing_mode,
    default_style,
  );
}

fn grid_container_parallel_flow_required_block_size(
  node: &FragmentNode,
  axis: &FragmentAxis,
  axes: FragmentAxes,
  fragmentainer_size: f32,
  context: FragmentationContext,
) -> Option<f32> {
  if !matches!(context, FragmentationContext::Page) {
    return None;
  }
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return None;
  }

  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  if !matches!(style.display, Display::Grid | Display::InlineGrid) {
    return None;
  }
  let Some(grid_info) = node.grid_fragmentation.as_ref() else {
    return None;
  };

  let node_block_size = axis.block_size(&node.bounds);
  let mut required = node_block_size;
  for (idx, placement) in grid_info.items.iter().enumerate() {
    if idx >= node.children.len() {
      break;
    }
    if !grid_item_spans_single_track(placement, axis) {
      continue;
    }

    let child = &node.children[idx];
    let child_block_size = axis.block_size(&child.bounds);
    let child_start = axis.flow_offset(
      axis.block_start(&child.bounds),
      child_block_size,
      node_block_size,
    );
    let child_required =
      grid_item_parallel_flow_required_block_size(child, axes, fragmentainer_size);
    required = required.max(child_start + child_required);
  }

  if required > node_block_size + BREAK_EPSILON {
    Some(required)
  } else {
    None
  }
}

/// Computes the total block-axis extent of a fragment tree, accounting for parallel fragmentation
/// flows (currently grid items in a row).
pub(crate) fn parallel_flow_content_extent(
  root: &FragmentNode,
  axes: FragmentAxes,
  fragmentainer_size_hint: Option<f32>,
  context: FragmentationContext,
) -> f32 {
  let axis = axis_from_fragment_axes(axes);
  let mut extent = axis.block_size(&root.logical_bounding_box());

  if !matches!(context, FragmentationContext::Page) {
    return extent;
  }
  let Some(fragmentainer_size) = fragmentainer_size_hint.filter(|s| s.is_finite() && *s > 0.0)
  else {
    return extent;
  };

  fn walk(
    node: &FragmentNode,
    abs_start: f32,
    axis: &FragmentAxis,
    axes: FragmentAxes,
    fragmentainer_size: f32,
    extent: &mut f32,
  ) {
    let node_block_size = axis.block_size(&node.bounds);
    if let Some(required) = grid_container_parallel_flow_required_block_size(
      node,
      axis,
      axes,
      fragmentainer_size,
      FragmentationContext::Page,
    ) {
      *extent = extent.max(abs_start + required);
    }

    for child in node.children.iter() {
      let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
      walk(
        child,
        child_abs_start,
        axis,
        axes,
        fragmentainer_size,
        extent,
      );
    }
  }

  walk(root, 0.0, &axis, axes, fragmentainer_size, &mut extent);
  extent
}

#[derive(Debug)]
pub struct FragmentationAnalyzer {
  _axis: FragmentAxis,
  context: FragmentationContext,
  enforce_fragmentainer_size: bool,
  opportunities: Vec<BreakOpportunity>,
  line_containers: Vec<LineContainer>,
  line_starts: Vec<usize>,
  atomic_candidates: Vec<AtomicCandidate>,
  table_repetitions: Vec<TableRepetitionInfo>,
  content_extent: f32,
  deadline_counter: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TableRepetitionInfo {
  pub(crate) start: f32,
  pub(crate) end: f32,
  pub(crate) header_block_size: f32,
  pub(crate) footer_block_size: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ConstraintKey {
  /// Whether the candidate would place too few lines at the start of the fragment.
  violates_orphans: bool,
  /// Whether the candidate would place too few lines at the end of a continuation fragment.
  violates_continuation_widows: bool,
  /// Whether taking the candidate leaves too few lines for the eventual last fragment.
  violates_future_widows: bool,
}
// ConstraintKey derives Ord so candidates can be compared lexicographically, relaxing
// constraints in the order: orphans → continuation widows → future widows.

const BREAK_EPSILON: f32 = 0.01;
const LINE_FALLBACK_EPSILON: f32 = 1.0;
const SIBLING_LIMIT_FALLBACK_MAX: f32 = 50.0;
const SIBLING_LIMIT_FALLBACK_RATIO: f32 = 0.15;

fn grid_tracks_in_fragmentation_axis<'a>(
  tracks: &'a GridTrackRanges,
  axis: &FragmentAxis,
) -> &'a [(f32, f32)] {
  if axis.block_is_horizontal {
    &tracks.columns
  } else {
    &tracks.rows
  }
}

fn grid_item_lines_in_fragmentation_axis(
  placement: &GridItemFragmentationData,
  axis: &FragmentAxis,
) -> (u16, u16) {
  if axis.block_is_horizontal {
    (placement.column_start, placement.column_end)
  } else {
    (placement.row_start, placement.row_end)
  }
}

fn grid_item_spans_single_track(
  placement: &GridItemFragmentationData,
  axis: &FragmentAxis,
) -> bool {
  let (start, end) = grid_item_lines_in_fragmentation_axis(placement, axis);
  end.saturating_sub(start) == 1
}

#[derive(Debug, Clone)]
struct FlexLineRange {
  start: f32,
  end: f32,
}

#[derive(Debug, Clone)]
struct FlexRowLineData {
  lines: Vec<FlexLineRange>,
  line_for_child: Vec<Option<usize>>,
}

fn is_row_flex_container(style: &ComputedStyle) -> bool {
  matches!(style.display, Display::Flex | Display::InlineFlex)
    && matches!(
      style.flex_direction,
      FlexDirection::Row | FlexDirection::RowReverse
    )
}

fn is_in_flow_flex_child(content: &FragmentContent, style: &ComputedStyle) -> bool {
  if matches!(
    content,
    FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
  ) {
    return false;
  }
  !matches!(style.position, Position::Absolute | Position::Fixed)
}

fn collect_row_flex_lines(
  node: &FragmentNode,
  abs_start: f32,
  axis: &FragmentAxis,
  node_block_size: f32,
  style: &ComputedStyle,
  node_writing_mode: WritingMode,
  default_style: &ComputedStyle,
) -> Option<FlexRowLineData> {
  if !is_row_flex_container(style) {
    return None;
  }

  // Flex layout emits in-flow children in order-modified document order (`order`, then DOM index),
  // so the fragment tree child order matches the spec's flex item ordering requirements.
  let inline_positive = inline_axis_positive(node_writing_mode, style.direction);
  let main_positive = match style.flex_direction {
    FlexDirection::Row => inline_positive,
    FlexDirection::RowReverse => !inline_positive,
    _ => inline_positive,
  };
  let container_inline_size = axis.inline_size(&node.bounds);

  let mut in_flow_indices: Vec<usize> = Vec::new();
  for (idx, child) in node.children.iter().enumerate() {
    let child_style = child
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    if is_in_flow_flex_child(&child.content, child_style) {
      in_flow_indices.push(idx);
    }
  }
  if in_flow_indices.is_empty() {
    return None;
  }

  const FLEX_LINE_WRAP_EPSILON: f32 = 0.5;
  let mut lines: Vec<FlexLineRange> = Vec::new();
  let mut line_for_child: Vec<Option<usize>> = vec![None; node.children.len()];

  let mut prev_main_start: Option<f32> = None;
  let mut current_line_index = 0usize;
  let mut current_start: f32 = f32::INFINITY;
  let mut current_end: f32 = f32::NEG_INFINITY;

  for child_idx in in_flow_indices {
    let child = &node.children[child_idx];
    let child_inline_size = axis.inline_size(&child.bounds);
    let child_inline_start = axis.inline_start(&child.bounds);
    let mut main_start = if main_positive {
      child_inline_start
    } else {
      container_inline_size - child_inline_start - child_inline_size
    };
    if !main_start.is_finite() {
      main_start = 0.0;
    }

    if let Some(prev) = prev_main_start {
      if main_start + FLEX_LINE_WRAP_EPSILON < prev {
        if current_end > current_start + BREAK_EPSILON {
          lines.push(FlexLineRange {
            start: current_start,
            end: current_end,
          });
        }
        current_line_index = current_line_index.saturating_add(1);
        current_start = f32::INFINITY;
        current_end = f32::NEG_INFINITY;
      }
    }
    prev_main_start = Some(main_start);

    let (child_abs_start, child_abs_end) =
      axis.flow_range(abs_start, node_block_size, &child.bounds);
    current_start = current_start.min(child_abs_start);
    current_end = current_end.max(child_abs_end);
    line_for_child[child_idx] = Some(current_line_index);
  }

  if current_end > current_start + BREAK_EPSILON {
    lines.push(FlexLineRange {
      start: current_start,
      end: current_end,
    });
  }
  if lines.is_empty() {
    return None;
  }

  Some(FlexRowLineData {
    lines,
    line_for_child,
  })
}

impl FragmentationAnalyzer {
  pub fn new(
    root: &FragmentNode,
    context: FragmentationContext,
    axes: FragmentAxes,
    enforce_fragmentainer_size: bool,
    fragmentainer_size_hint: Option<f32>,
  ) -> Self {
    let axis = axis_from_fragment_axes(axes);
    let root_writing_mode = root
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(WritingMode::HorizontalTb);
    let mut collection = BreakCollection::default();
    collect_break_opportunities(
      root,
      0.0,
      &mut collection,
      0,
      0,
      context,
      &axis,
      root_writing_mode,
      true,
    );

    // Collect atomic range *candidates* independent of the current fragmentainer size.
    //
    // Callers may request "soft" enforcement of the fragmentainer size (e.g. multi-column layout
    // looking slightly past the column height to the next break opportunity). That behaviour must
    // not disable size-aware atomic modelling: instead we collect all candidates upfront (including
    // those that only become atomic when they fit) and filter them per-boundary-selection based on
    // the fragmentainer size used for that boundary.
    let mut atomic_candidates = Vec::new();
    collect_atomic_candidates_with_axis(
      root,
      0.0,
      &mut atomic_candidates,
      &axis,
      axis.block_size(&root.bounds),
      context,
      root_writing_mode,
    );

    collection.opportunities.sort_by(|a, b| {
      a.pos
        .partial_cmp(&b.pos)
        .unwrap_or(std::cmp::Ordering::Equal)
    });
    collection.opportunities.dedup_by(|a, b| {
      (a.pos - b.pos).abs() < BREAK_EPSILON && a.kind == b.kind && a.strength == b.strength
    });

    let content_extent = parallel_flow_content_extent(root, axes, fragmentainer_size_hint, context);
    let table_repetitions = collect_table_repetition_info_with_axis(
      root,
      0.0,
      &axis,
      context,
      root_writing_mode,
    );
    let line_containers = collection.line_containers;
    let line_starts = vec![0; line_containers.len()];
    Self {
      _axis: axis,
      context,
      opportunities: collection.opportunities,
      line_containers,
      line_starts,
      atomic_candidates,
      table_repetitions,
      content_extent,
      deadline_counter: 0,
      enforce_fragmentainer_size,
    }
  }

  pub fn content_extent(&self) -> f32 {
    self.content_extent
  }

  pub fn boundaries(
    &mut self,
    fragmentainer_size: f32,
    total_extent: f32,
  ) -> Result<Vec<f32>, LayoutError> {
    let effective_total = total_extent.max(self.content_extent);
    if fragmentainer_size <= 0.0 {
      return Ok(vec![0.0, effective_total]);
    }

    self.reset_state();
    let atomic = self.atomic_ranges_for(fragmentainer_size);
    let mut boundaries = vec![0.0];
    let mut start = 0.0;
    let mut opportunity_cursor = 0usize;

    while start < effective_total - BREAK_EPSILON {
      if self.deadline_counter % 8 == 0 {
        check_layout_deadline()?;
      }
      self.deadline_counter = self.deadline_counter.wrapping_add(1);

      let next = self.select_next_boundary(
        start,
        fragmentainer_size,
        effective_total,
        &mut opportunity_cursor,
        &atomic,
      );
      debug_assert!(
        next + BREAK_EPSILON >= start,
        "boundaries must not move backwards"
      );
      if (next - start).abs() < BREAK_EPSILON {
        boundaries.push(effective_total);
        break;
      }
      boundaries.push(next);
      start = next;
    }

    if effective_total - *boundaries.last().unwrap_or(&0.0) > BREAK_EPSILON {
      boundaries.push(effective_total);
    }

    boundaries.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    boundaries.dedup_by(|a, b| (*a - *b).abs() < BREAK_EPSILON);
    Ok(boundaries)
  }

  /// Computes balanced fragmentation boundaries for a fixed number of fragmentainers.
  ///
  /// This is primarily used for `column-fill: balance`, where we want content to be distributed
  /// more evenly across a fixed number of columns instead of greedily filling each column up to the
  /// fragmentainer limit.
  ///
  /// The algorithm selects each boundary by targeting the *average remaining extent per remaining
  /// fragmentainer*, clamped to `max_fragmentainer_size`, while ensuring that the remaining content
  /// can still fit in the remaining fragmentainers.
  pub fn balanced_boundaries(
    &mut self,
    fragmentainer_count: usize,
    max_fragmentainer_size: f32,
    total_extent: f32,
  ) -> Result<Vec<f32>, LayoutError> {
    let effective_total = total_extent.max(self.content_extent);
    if fragmentainer_count <= 1 || max_fragmentainer_size <= 0.0 {
      return Ok(vec![0.0, effective_total]);
    }

    self.reset_state();
    // `max_fragmentainer_size` is the physical fragmentainer size. The balancing loop may target a
    // smaller "ideal" size for early boundaries, but atomic ranges should still be considered
    // unbreakable so long as they fit within the physical fragmentainer.
    let atomic = self.atomic_ranges_for(max_fragmentainer_size);
    let mut boundaries = vec![0.0];
    let mut start = 0.0f32;
    let mut opportunity_cursor = 0usize;
    let mut remaining = fragmentainer_count;

    while remaining > 1 && start < effective_total - BREAK_EPSILON {
      if self.deadline_counter % 8 == 0 {
        check_layout_deadline()?;
      }
      self.deadline_counter = self.deadline_counter.wrapping_add(1);

      let remaining_extent = effective_total - start;
      let ideal = remaining_extent / remaining as f32;

      // Do not pick a boundary so early that the remaining content cannot fit within the remaining
      // fragmentainers when each is capped at `max_fragmentainer_size`.
      let remaining_after = remaining - 1;
      let min_boundary = effective_total - max_fragmentainer_size * remaining_after as f32;
      let min_size = (min_boundary - start).max(0.0);

      let mut fragmentainer_size = ideal
        .min(max_fragmentainer_size)
        .max(min_size)
        .max(BREAK_EPSILON);

      // When balancing across a fixed number of fragmentainers we still must honour explicit breaks
      // (`break-before/after`, etc). Ensure the next forced boundary is inside the selection
      // window so `select_next_boundary` can pick it even when the ideal balanced size would stop
      // short.
      if opportunity_cursor > self.opportunities.len() {
        opportunity_cursor = self.opportunities.len();
      }
      let advance = self.opportunities[opportunity_cursor..]
        .partition_point(|o| o.pos <= start + BREAK_EPSILON);
      opportunity_cursor = (opportunity_cursor + advance).min(self.opportunities.len());
      if let Some(forced_pos) = self.opportunities[opportunity_cursor..]
        .iter()
        .find(|o| matches!(o.strength, BreakStrength::Forced) && o.pos > start + BREAK_EPSILON)
        .map(|o| o.pos)
      {
        fragmentainer_size = fragmentainer_size.max((forced_pos - start).max(BREAK_EPSILON));
      }

      let next = self.select_next_boundary(
        start,
        fragmentainer_size,
        effective_total,
        &mut opportunity_cursor,
        &atomic,
      );
      debug_assert!(
        next + BREAK_EPSILON >= start,
        "boundaries must not move backwards"
      );
      if (next - start).abs() < BREAK_EPSILON {
        boundaries.push(effective_total);
        break;
      }
      boundaries.push(next);
      start = next;
      remaining = remaining.saturating_sub(1);
    }

    if effective_total - *boundaries.last().unwrap_or(&0.0) > BREAK_EPSILON {
      boundaries.push(effective_total);
    }
    boundaries.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    boundaries.dedup_by(|a, b| (*a - *b).abs() < BREAK_EPSILON);
    Ok(boundaries)
  }

  fn reset_state(&mut self) {
    for start in &mut self.line_starts {
      *start = 0;
    }
    self.deadline_counter = 0;
  }

  fn atomic_ranges_for(&self, fragmentainer_size: f32) -> Vec<AtomicRange> {
    if self.atomic_candidates.is_empty() {
      return Vec::new();
    }

    let mut ranges: Vec<AtomicRange> = if fragmentainer_size.is_finite() && fragmentainer_size > 0.0
    {
      self
        .atomic_candidates
        .iter()
        .copied()
        .filter(|candidate| {
          let required = candidate.required_fragmentainer_size.max(0.0);
          required.is_finite() && required <= fragmentainer_size + BREAK_EPSILON
        })
        .map(|candidate| candidate.range)
        .collect()
    } else {
      self.atomic_candidates.iter().map(|c| c.range).collect()
    };

    normalize_atomic_ranges(&mut ranges);
    // Forced breaks override `break-inside: avoid-*` semantics. Atomic ranges represent avoid-inside
    // (and similar indivisible) content, so ensure they never span across forced break opportunity
    // positions; otherwise forced breaks can be incorrectly suppressed by `pos_is_inside_atomic`.
    split_atomic_ranges_at_forced_break_opportunities(&mut ranges, &self.opportunities);
    ranges
  }

  fn advance_line_starts(&mut self, boundary: f32) {
    for container in &self.line_containers {
      if let Some(slot) = self.line_starts.get_mut(container.id) {
        let remaining = &container.line_ends[*slot..];
        let advanced = remaining.partition_point(|end| *end <= boundary + BREAK_EPSILON);
        *slot = (*slot + advanced).min(container.line_ends.len());
      }
    }
  }

  fn select_next_boundary(
    &mut self,
    start: f32,
    fragmentainer: f32,
    total_extent: f32,
    opportunity_cursor: &mut usize,
    atomic: &[AtomicRange],
  ) -> f32 {
    if let Some(range) = atomic_containing(start, atomic) {
      let boundary = range.end.min(total_extent);
      self.advance_line_starts(boundary);
      return boundary;
    }

    let header_overhead = table_header_overhead_at(&self.table_repetitions, start)
      .min((fragmentainer - BREAK_EPSILON).max(0.0));
    let max_without_footer = (fragmentainer - header_overhead).max(BREAK_EPSILON);
    let limit = (start + max_without_footer).min(total_extent);

    let mut chosen = self.select_next_boundary_with_limit(
      start,
      limit,
      fragmentainer,
      total_extent,
      opportunity_cursor,
      atomic,
    );

    if let Some(table) = innermost_footer_table_at(&self.table_repetitions, chosen) {
      let footer_overhead = table
        .footer_block_size
        .min((fragmentainer - header_overhead - BREAK_EPSILON).max(0.0));
      if footer_overhead > BREAK_EPSILON {
        let max_with_footer = (fragmentainer - header_overhead - footer_overhead).max(BREAK_EPSILON);
        let limit_with_footer = (start + max_with_footer).min(total_extent);

        // If reserving footer space leaves no room to show any of the table, push the entire table
        // to the next fragmentainer by breaking at the table's start edge.
        if limit_with_footer <= table.start + BREAK_EPSILON {
          let boundary = table.start.min(total_extent);
          self.advance_line_starts(boundary);
          return boundary;
        }

        if chosen > limit_with_footer + BREAK_EPSILON {
          chosen = self.select_next_boundary_with_limit(
            start,
            limit_with_footer,
            fragmentainer,
            total_extent,
            opportunity_cursor,
            atomic,
          );
        }
      }
    }

    self.advance_line_starts(chosen);
    chosen
  }

  fn select_next_boundary_with_limit(
    &mut self,
    start: f32,
    mut limit: f32,
    fragmentainer: f32,
    total_extent: f32,
    opportunity_cursor: &mut usize,
    atomic: &[AtomicRange],
  ) -> f32 {
    // Avoid selecting a boundary that lands inside an atomic range. If the natural fragmentainer
    // limit would split an atomic range, clamp to the atomic start so the next fragment starts
    // before the atomic content.
    if let Some(range) = atomic_containing(limit, atomic) {
      if range.start > start + BREAK_EPSILON {
        limit = limit.min(range.start);
      } else {
        // The fragment starts inside (or at the beginning of) an atomic range, and the current
        // fragmentainer limit falls within it. Prefer overflowing rather than producing an empty
        // fragmentainer that would never advance the cursor.
        return range.end.min(total_extent);
      }
    }

    let ops = &self.opportunities;
    if *opportunity_cursor > ops.len() {
      *opportunity_cursor = ops.len();
    }
    let advance = ops[*opportunity_cursor..].partition_point(|o| o.pos <= start + BREAK_EPSILON);
    *opportunity_cursor = (*opportunity_cursor + advance).min(ops.len());
    let window_end = *opportunity_cursor
      + ops[*opportunity_cursor..].partition_point(|o| o.pos <= limit + BREAK_EPSILON);
    let window_end = window_end.min(ops.len());
    let window = *opportunity_cursor..window_end;

    if let Some(pos) = self.forced_in_window(start, limit, total_extent, window.clone(), atomic) {
      self.advance_line_starts(pos);
      return pos;
    }

    let mut best: Option<(ConstraintKey, u8, u8, f32)> = None;
    for opportunity in self.opportunities[window.clone()].iter() {
      if opportunity.pos <= start + BREAK_EPSILON {
        continue;
      }
      if opportunity.pos > limit + BREAK_EPSILON {
        break;
      }
      if pos_is_inside_atomic(opportunity.pos, atomic) {
        continue;
      }
      if matches!(opportunity.strength, BreakStrength::Forced) {
        continue;
      }

      let constraint_key =
        constraint_key_for(opportunity, &self.line_containers, &self.line_starts);
      let strength_penalty = match opportunity.strength {
        BreakStrength::Avoid => 1,
        _ => 0,
      };
      let kind_rank = match opportunity.kind {
        BreakKind::BetweenSiblings => 0,
        BreakKind::LineBoundary { .. } => 1,
        BreakKind::EndOfContent => 2,
      };

      match best {
        None => best = Some((constraint_key, strength_penalty, kind_rank, opportunity.pos)),
        Some((best_key, best_penalty, best_kind, best_pos)) => {
          if constraint_key < best_key
            || (constraint_key == best_key && strength_penalty < best_penalty)
            || (constraint_key == best_key
              && strength_penalty == best_penalty
              && kind_rank < best_kind)
            || (constraint_key == best_key
              && strength_penalty == best_penalty
              && kind_rank == best_kind
              && opportunity.pos > best_pos + BREAK_EPSILON)
          {
            best = Some((constraint_key, strength_penalty, kind_rank, opportunity.pos));
          }
        }
      }
    }

    if let Some((_, _, kind_rank, pos)) = best {
      let clamped = pos.min(total_extent);
      // Break opportunities between siblings can appear far before the fragmentainer limit when
      // large empty gaps exist between siblings (common in absolutely-positioned/layered content).
      //
      // Choosing such an early boundary shifts the coordinate mapping for subsequent fragments
      // because fragment stacking assumes fixed-size fragmentainers (`fragmentainer_size +
      // fragmentainer_gap`). Prefer the natural fragmentainer limit unless the sibling boundary is
      // effectively at the limit.
      if kind_rank == 0 && matches!(self.context, FragmentationContext::Page) {
        // Allow a small amount of slack for sibling boundaries near the limit: the closer the
        // boundary is, the less it perturbs the flow→fragment mapping. Cap the slack so huge pages
        // do not accept large shifts.
        let sibling_limit_fallback = (fragmentainer * SIBLING_LIMIT_FALLBACK_RATIO)
          .min(SIBLING_LIMIT_FALLBACK_MAX)
          .max(LINE_FALLBACK_EPSILON);
        if clamped + sibling_limit_fallback < limit {
          self.advance_line_starts(limit);
          return limit;
        }
      }
      self.advance_line_starts(clamped);
      return clamped;
    }

    if matches!(self.context, FragmentationContext::Column) && !self.enforce_fragmentainer_size {
      // Multi-column layout prefers moving content to the next available break opportunity rather
      // than slicing it at an arbitrary fragmentainer limit (e.g. splitting a block box when the
      // next legal break is just after the limit). Only do this when the caller did not request a
      // hard fragmentainer size.
      if let Some(next) = self.opportunities[window_end..]
        .iter()
        .find(|o| o.pos > limit + BREAK_EPSILON && !pos_is_inside_atomic(o.pos, atomic))
      {
        let clamped = next.pos.min(total_extent);
        self.advance_line_starts(clamped);
        return clamped;
      }
    }

    let mut fallback = limit;
    if let Some(near_line) = self.near_line_boundary(start, limit, window.clone(), atomic) {
      fallback = near_line;
    }

    let clamped = fallback.min(total_extent);
    let next = if clamped <= start + BREAK_EPSILON {
      (start + fragmentainer).min(total_extent)
    } else {
      clamped
    };
    next
  }

  fn forced_in_window(
    &self,
    start: f32,
    limit: f32,
    total_extent: f32,
    window: std::ops::Range<usize>,
    atomic: &[AtomicRange],
  ) -> Option<f32> {
    let forced = self.opportunities[window]
      .iter()
      .find(|o| {
        matches!(o.strength, BreakStrength::Forced)
          && o.pos > start + BREAK_EPSILON
          && o.pos <= limit + BREAK_EPSILON
          && !pos_is_inside_atomic(o.pos, atomic)
      })
      .map(|o| o.pos.min(total_extent));
    if forced.is_some() {
      return forced;
    }

    // Even if the fragmentainer limit lands at the end of the total extent, we still need to
    // honour any forced breaks inside the window above. Otherwise `break-before/after` rules would
    // be ignored whenever the content fits in a single fragmentainer.
    if limit >= total_extent - BREAK_EPSILON {
      return Some(total_extent);
    }

    None
  }

  fn near_line_boundary(
    &self,
    start: f32,
    limit: f32,
    window: std::ops::Range<usize>,
    atomic: &[AtomicRange],
  ) -> Option<f32> {
    self.opportunities[window].iter().find_map(|o| {
      if o.pos <= start + BREAK_EPSILON {
        return None;
      }
      if o.pos - limit > LINE_FALLBACK_EPSILON {
        return None;
      }
      if pos_is_inside_atomic(o.pos, atomic) {
        return None;
      }
      match o.kind {
        BreakKind::LineBoundary { .. } if o.pos > limit => Some(o.pos),
        _ => None,
      }
    })
  }
}

/// Returns the block-axis boundaries where a fragment tree should be split for a given
/// fragmentainer size.
///
/// The returned vector always starts at 0.0 and ends at the end of the content range (expanded to
/// at least one fragmentainer). When `fragmentainer_size` is non-positive, a single fragment
/// containing all content is implied.
pub fn resolve_fragmentation_boundaries(
  root: &FragmentNode,
  fragmentainer_size: f32,
) -> Result<Vec<f32>, LayoutError> {
  resolve_fragmentation_boundaries_with_context(
    root,
    fragmentainer_size,
    FragmentationContext::Page,
  )
}

pub fn resolve_fragmentation_boundaries_with_context(
  root: &FragmentNode,
  fragmentainer_size: f32,
  context: FragmentationContext,
) -> Result<Vec<f32>, LayoutError> {
  let axes = axes_from_root(root);
  resolve_fragmentation_boundaries_with_axes(root, fragmentainer_size, context, axes)
}

pub fn resolve_fragmentation_boundaries_with_axes(
  root: &FragmentNode,
  fragmentainer_size: f32,
  context: FragmentationContext,
  axes: FragmentAxes,
) -> Result<Vec<f32>, LayoutError> {
  let enforce_fragmentainer_size = matches!(context, FragmentationContext::Page);
  let mut analyzer = FragmentationAnalyzer::new(
    root,
    context,
    axes,
    enforce_fragmentainer_size,
    match context {
      FragmentationContext::Page => Some(fragmentainer_size),
      FragmentationContext::Column => None,
    },
  );
  let total_extent = analyzer.content_extent().max(fragmentainer_size);
  analyzer.boundaries(fragmentainer_size, total_extent)
}

/// Fragment a tree using the provided writing mode and direction. Non-default axes currently
/// defer to the primary fragmentation path while keeping pagination API-compatible.
pub fn fragment_tree_for_writing_mode(
  root: &FragmentNode,
  options: &FragmentationOptions,
  writing_mode: WritingMode,
  direction: Direction,
) -> Result<Vec<FragmentNode>, LayoutError> {
  let axes = FragmentAxes::from_writing_mode_and_direction(writing_mode, direction);
  fragment_tree_with_axes(root, options, axes)
}

/// Axis-aware fragmentation entry point.
pub fn fragment_tree_with_axes(
  root: &FragmentNode,
  options: &FragmentationOptions,
  axes: FragmentAxes,
) -> Result<Vec<FragmentNode>, LayoutError> {
  if axes.block_axis() == PhysicalAxis::Y && axes.block_positive() {
    return fragment_tree(root, options);
  }

  fragment_tree(root, options)
}

/// Splits a fragment tree into multiple fragmentainer roots based on the given options.
///
/// The returned fragments retain the original tree structure but are clipped to the
/// fragmentainer block-size. Fragment metadata (`fragment_index`, `fragment_count`, and
/// `fragmentainer_index`) are populated so downstream stages can reason about page/column
/// membership.
pub fn fragment_tree(
  root: &FragmentNode,
  options: &FragmentationOptions,
) -> Result<Vec<FragmentNode>, LayoutError> {
  if options.fragmentainer_size <= 0.0 {
    return Ok(vec![root.clone()]);
  }

  let axes = axes_from_root(root);
  let axis = axis_from_fragment_axes(axes);
  let inline_is_horizontal = axes.inline_axis() == PhysicalAxis::X;
  let block_sign = if axis.block_positive { 1.0 } else { -1.0 };
  let inline_sign = if axes.inline_positive() { 1.0 } else { -1.0 };
  let context = if options.column_count > 1 {
    FragmentationContext::Column
  } else {
    FragmentationContext::Page
  };

  // Model forced breaks inside single-track grid items (parallel fragmentation flow) as inserting
  // blank space up to the next fragmentainer boundary (CSS Grid 2 §Fragmenting Grid Layout). This
  // ensures the continuation content appears on later pages without forcing sibling grid items onto
  // the next page.
  let mut root = root.clone();
  if matches!(context, FragmentationContext::Page) {
    apply_grid_parallel_flow_forced_break_shifts(&mut root, axes, options.fragmentainer_size);
  }
  apply_float_parallel_flow_forced_break_shifts(
    &mut root,
    axes,
    options.fragmentainer_size,
    context,
  );

  let mut analyzer =
    FragmentationAnalyzer::new(&root, context, axes, true, Some(options.fragmentainer_size));

  let total_extent = analyzer.content_extent().max(options.fragmentainer_size);
  let boundaries = analyzer.boundaries(options.fragmentainer_size, total_extent)?;
  if boundaries.len() < 2 {
    return Ok(vec![root.clone()]);
  }

  let fragment_count = boundaries.len() - 1;
  let column_count = options.column_count.max(1);
  let column_step = axis.inline_size(&root.bounds) + options.column_gap;
  let fragment_step = options.fragmentainer_size + options.fragmentainer_gap;
  let mut fragments = Vec::with_capacity(fragment_count);

  for (index, window) in boundaries.windows(2).enumerate() {
    let start = window[0];
    let end = window[1];
    if end <= start {
      continue;
    }

    if let Some(mut clipped) = clip_node(
      &root,
      &axis,
      start,
      end,
      0.0,
      start,
      end,
      axis.block_size(&root.bounds),
      index,
      fragment_count,
      context,
      options.fragmentainer_size,
      axes,
    )? {
      normalize_fragment_margins(&mut clipped, index == 0, index + 1 == fragment_count, &axis);
      propagate_fragment_metadata(&mut clipped, index, fragment_count);

      // Translate fragments to account for fragmentainer gaps so downstream consumers
      // can reason about the absolute position of each fragmentainer stack. When
      // columns are requested, fragments are distributed left-to-right before
      // stacking additional rows vertically.
      let column = index % column_count;
      let row = index / column_count;
      let column_offset = column as f32 * column_step * inline_sign;
      let row_offset = row as f32 * fragment_step * block_sign;
      let mut offset = Point::new(0.0, 0.0);
      if inline_is_horizontal {
        offset.x += column_offset;
      } else {
        offset.y += column_offset;
      }
      if axis.block_is_horizontal {
        offset.x += row_offset;
      } else {
        offset.y += row_offset;
      }
      clipped.translate_root_in_place(offset);
      fragments.push(clipped);
    }
  }

  if fragments.is_empty() {
    fragments.push(root.clone());
  }

  Ok(fragments)
}

pub(crate) fn propagate_fragment_metadata(node: &mut FragmentNode, index: usize, count: usize) {
  node.fragment_index = index;
  node.fragment_count = count.max(1);
  // Update the page index while preserving any nested column metadata already attached by earlier
  // fragmentation passes (e.g. multi-column layout inside a paginated page).
  node.fragmentainer = node.fragmentainer.with_page_index(index);
  node.fragmentainer_index = node.fragmentainer.flattened_index();
  for child in node.children_mut() {
    propagate_fragment_metadata(child, index, count);
  }
}

fn clip_grid_item_parallel_for_page(
  item: &FragmentNode,
  axis: &FragmentAxis,
  axes: FragmentAxes,
  fragmentainer_size: f32,
  page_index: usize,
) -> Result<Option<FragmentNode>, LayoutError> {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return Ok(None);
  }

  // Treat the grid item subtree as its own flow starting at the origin.
  let mut local = item.clone();
  let origin = local.bounds.origin;
  if origin.x != 0.0 || origin.y != 0.0 {
    local.bounds = Rect::from_xywh(0.0, 0.0, local.bounds.width(), local.bounds.height());
    if let Some(logical) = local.logical_override {
      local.logical_override = Some(logical.translate(Point::new(-origin.x, -origin.y)));
    }
  }

  let mut analyzer = FragmentationAnalyzer::new(
    &local,
    FragmentationContext::Page,
    axes,
    true,
    Some(fragmentainer_size),
  );
  let total_extent = analyzer.content_extent();
  let boundaries = analyzer.boundaries(fragmentainer_size, total_extent)?;
  let fragment_count = boundaries.len().saturating_sub(1);
  if fragment_count == 0 || page_index >= fragment_count {
    return Ok(None);
  }

  let start = boundaries[page_index];
  let end = boundaries[page_index + 1];
  if end <= start + BREAK_EPSILON {
    return Ok(None);
  }

  clip_node(
    &local,
    axis,
    start,
    end,
    0.0,
    start,
    end,
    axis.block_size(&local.bounds),
    page_index,
    fragment_count,
    FragmentationContext::Page,
    fragmentainer_size,
    axes,
  )
}

pub(crate) fn clip_node(
  node: &FragmentNode,
  axis: &FragmentAxis,
  fragment_start: f32,
  fragment_end: f32,
  parent_abs_flow_start: f32,
  parent_clipped_flow_start: f32,
  parent_clipped_flow_end: f32,
  parent_block_size: f32,
  fragment_index: usize,
  fragment_count: usize,
  context: FragmentationContext,
  fragmentainer_size: f32,
  axes: FragmentAxes,
) -> Result<Option<FragmentNode>, LayoutError> {
  check_layout_deadline()?;
  // When a parent fragment's own bounds no longer overlap the fragmentainer but its logical
  // bounding box does (e.g. descendants overflow), callers can pass a "clipped" range whose end
  // precedes its start. Clamp to a non-decreasing range so descendant translation always anchors
  // to the parent's clipped start edge.
  let parent_clipped_flow_end = parent_clipped_flow_end.max(parent_clipped_flow_start);
  let parent_clip_start_offset = parent_clipped_flow_start - parent_abs_flow_start;
  let parent_clip_end_offset = parent_clipped_flow_end - parent_abs_flow_start;
  let parent_clip_origin_phys = axis
    .flow_point_to_physical(parent_clip_start_offset, parent_block_size)
    .min(axis.flow_point_to_physical(parent_clip_end_offset, parent_block_size));
  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);

  let original_node_block_size = axis.block_size(&node.bounds);
  let mut node_block_size = original_node_block_size;
  let (node_flow_start, mut node_flow_end) =
    axis.flow_range(parent_abs_flow_start, parent_block_size, &node.bounds);
  if let Some(required) =
    grid_container_parallel_flow_required_block_size(node, axis, axes, fragmentainer_size, context)
  {
    node_block_size = node_block_size.max(required);
    node_flow_end = node_flow_start + node_block_size;
  }
  let node_bbox = node.logical_bounding_box();
  let node_bbox_block_size = axis.block_size(&node_bbox);
  let node_bbox_flow_start = parent_abs_flow_start
    + axis.flow_offset(
      axis.block_start(&node_bbox),
      node_bbox_block_size,
      parent_block_size,
    );
  let mut node_bbox_flow_end = (node_bbox_flow_start + node_bbox_block_size).max(node_flow_end);

  // Parallel flows (grid items in a row) can extend the effective block size of descendants even
  // when their laid-out bounds fit within a single page. Pagination inflates the total extent using
  // `parallel_flow_content_extent`, so clipping must also treat ancestor nodes as overlapping later
  // pages or the continuation content would be dropped.
  if matches!(context, FragmentationContext::Page)
    && fragmentainer_size.is_finite()
    && fragmentainer_size > 0.0
    && node_bbox_flow_end <= fragment_start
  {
    let required = parallel_flow_content_extent(node, axes, Some(fragmentainer_size), context);
    if required > node_block_size + BREAK_EPSILON {
      node_block_size = node_block_size.max(required);
      node_flow_end = node_flow_start + node_block_size;
      node_bbox_flow_end = node_bbox_flow_end.max(node_flow_end);
    }
  }

  // Treat zero-length fragments (common for empty blocks that only contribute forced breaks) as a
  // point at their start edge so they participate in clipping/fragmentation. Using the normal
  // half-open overlap test (`end <= start`) would drop them from *both* adjacent fragmentainers.
  let bbox_is_zero = node_bbox_block_size <= BREAK_EPSILON;
  if node_bbox_flow_end < fragment_start
    || (node_bbox_flow_end <= fragment_start && !bbox_is_zero)
    || node_bbox_flow_start >= fragment_end
  {
    return Ok(None);
  }
  let is_table_row_like = matches!(
    style.display,
    Display::TableRow
      | Display::TableRowGroup
      | Display::TableHeaderGroup
      | Display::TableFooterGroup
  );
  let mut avoid_inside = avoids_break_inside(style.break_inside, context) || is_table_row_like;
  if avoid_inside && node_block_size > (fragment_end - fragment_start) + 0.01 {
    avoid_inside = false;
  }

  // Honor break-inside/line constraints by keeping the fragment intact within a single fragmentainer.
  let node_overlaps = node_flow_end > fragment_start && node_flow_start < fragment_end;
  if avoid_inside
    && node_overlaps
    && (node_flow_start < fragment_start || node_flow_end > fragment_end)
  {
    // If a fragmentation boundary falls inside an element that avoids breaks (e.g.
    // `break-inside: avoid-*`), move the entire element to the fragment that starts
    // inside it instead of letting it overflow the fragment that ends mid-element.
    //
    // This mirrors the special-case handling for indivisible line boxes below.
    let fragment_starts_inside_node =
      fragment_start > node_flow_start && fragment_start < node_flow_end;
    if fragment_starts_inside_node {
      let mut cloned = clone_without_children(node);
      let node_phys_start = axis.flow_box_start_to_physical(
        fragment_start - parent_abs_flow_start,
        node_block_size,
        parent_block_size,
      );
      let new_block_start = node_phys_start - parent_clip_origin_phys;
      cloned.bounds = axis.update_block_components(node.bounds, new_block_start, node_block_size);
      cloned.fragment_index = fragment_index;
      cloned.fragment_count = fragment_count.max(1);
      cloned.fragmentainer_index = fragment_index;
      cloned.children = node.children.clone();
      return Ok(Some(cloned));
    }
    return Ok(None);
  }

  // `node_flow_end` is based on the fragment's own bounds, which may be limited to the viewport
  // even when descendants overflow (common for the root when paginating). Use the logical bounding
  // box extent instead so the clip window stays consistent for descendants—especially when block
  // progression is reversed (e.g. `writing-mode: vertical-rl`), where the physical origin of the
  // clip window depends on both its start *and* end.
  let clipped_flow_start = node_flow_start.max(fragment_start);
  let clipped_flow_end = node_bbox_flow_end.min(fragment_end);
  // Descendants can extend outside a node's own border box (e.g. scroll overflow or reversed
  // block progression that places content in negative physical coordinates). Use the logical
  // bounding box overlap to derive the clipping window that is propagated to children so
  // overflow content participates in fragmentation.
  let clipped_bbox_flow_start = node_bbox_flow_start.max(fragment_start);
  let clipped_bbox_flow_end = node_bbox_flow_end.min(fragment_end);
  let new_block_size = (clipped_flow_end - clipped_flow_start).max(0.0);
  let clipped_phys_start = axis.flow_box_start_to_physical(
    clipped_flow_start - parent_abs_flow_start,
    new_block_size,
    parent_block_size,
  );
  let new_block_start = clipped_phys_start - parent_clip_origin_phys;

  let mut cloned = clone_without_children(node);
  const CLIP_EPSILON: f32 = 0.01;
  if let Some(meta) = cloned.block_metadata.as_mut() {
    // Only mark as clipped when the fragmentainer boundary actually slices through the fragment.
    // Treat near-equal comparisons as un-clipped so margins can be normalized at exact boundaries
    // (e.g. when a column boundary lands precisely on a sibling boundary).
    meta.clipped_top = node_flow_start + CLIP_EPSILON < fragment_start;
    meta.clipped_bottom = node_flow_end > fragment_end + CLIP_EPSILON;
  }
  cloned.bounds = axis.update_block_components(node.bounds, new_block_start, new_block_size);
  cloned.fragment_index = fragment_index;
  cloned.fragment_count = fragment_count.max(1);
  cloned.fragmentainer_index = fragment_index;
  let logical_block_size = axis.block_size(&node.logical_bounds());
  let base_offset = node.slice_info.slice_offset.max(0.0);
  let mut original_block_size = logical_block_size.max(node_block_size);
  let previously_fragmented =
    base_offset > CLIP_EPSILON || !node.slice_info.is_first || !node.slice_info.is_last;
  if previously_fragmented {
    original_block_size = original_block_size.max(node.slice_info.original_block_size);
  }
  let node_block_span = (node_bbox_flow_end - node_flow_start).max(node_block_size);
  original_block_size = original_block_size.max(base_offset + node_block_span);
  let slice_offset = base_offset + (clipped_flow_start - node_flow_start).max(0.0);
  let slice_end_offset = base_offset + (clipped_flow_end - node_flow_start).max(0.0);
  let epsilon = 0.01;
  cloned.slice_info = FragmentSliceInfo {
    is_first: slice_offset <= epsilon,
    is_last: slice_end_offset >= original_block_size - epsilon,
    slice_offset: slice_offset.min(original_block_size),
    original_block_size,
  };

  if matches!(node.content, FragmentContent::Line { .. }) {
    // Line boxes are indivisible. If a break lands inside a line, move the whole
    // line to the fragment that starts within the line box (instead of letting
    // it overflow the fragment that ends mid-line).
    let overlaps = node_flow_end > fragment_start && node_flow_start < fragment_end;
    let fragment_starts_inside = fragment_start > node_flow_start && fragment_start < node_flow_end;
    let fragment_is_last = fragment_index + 1 == fragment_count;
    let fragment_contains_line_start =
      node_flow_start >= fragment_start && node_flow_start < fragment_end;
    let fully_contained = node_flow_start >= fragment_start && node_flow_end <= fragment_end;
    if !overlaps
      || (!fully_contained
        && !fragment_starts_inside
        && !(fragment_is_last && fragment_contains_line_start))
    {
      return Ok(None);
    }

    let line_phys_start = axis.flow_box_start_to_physical(
      clipped_flow_start - parent_abs_flow_start,
      node_block_size,
      parent_block_size,
    );
    let line_block_start = line_phys_start - parent_clip_origin_phys;
    cloned.bounds = axis.update_block_components(node.bounds, line_block_start, node_block_size);
    cloned.children = node.children.clone();
    return Ok(Some(cloned));
  }

  let grid_items = if matches!(context, FragmentationContext::Page)
    && matches!(style.display, Display::Grid | Display::InlineGrid)
  {
    node.grid_fragmentation.as_ref()
  } else {
    None
  };

  for (idx, child) in node.children.iter().enumerate() {
    let parallel_item = grid_items
      .and_then(|info| info.items.get(idx))
      .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    if parallel_item {
      let child_abs_start = axis
        .flow_range(node_flow_start, original_node_block_size, &child.bounds)
        .0;
      let child_required =
        grid_item_parallel_flow_required_block_size(child, axes, fragmentainer_size);
      let child_abs_end = child_abs_start + child_required;
      if child_abs_end <= fragment_start + BREAK_EPSILON
        || child_abs_start >= fragment_end - BREAK_EPSILON
      {
        continue;
      }

      let local_index = if fragment_start > child_abs_start + BREAK_EPSILON
        && fragmentainer_size.is_finite()
        && fragmentainer_size > 0.0
      {
        (((fragment_start - child_abs_start) / fragmentainer_size).floor() as i32).max(0) as usize
      } else {
        0
      };

      if let Some(mut item_fragment) =
        clip_grid_item_parallel_for_page(child, axis, axes, fragmentainer_size, local_index)?
      {
        // Position the clipped fragment in the current fragmentainer slice. This mirrors the
        // coordinate mapping performed by the normal `clip_node` recursion, but without slicing the
        // grid item's internal flow.
        let child_clipped_flow_start = child_abs_start.max(fragment_start);
        let node_clip_origin_phys = axis
          .flow_point_to_physical(
            clipped_flow_start - node_flow_start,
            original_node_block_size,
          )
          .min(
            axis
              .flow_point_to_physical(clipped_flow_end - node_flow_start, original_node_block_size),
          );
        let child_phys_start = axis.flow_box_start_to_physical(
          child_clipped_flow_start - node_flow_start,
          axis.block_size(&item_fragment.bounds),
          original_node_block_size,
        );
        let child_block_start = child_phys_start - node_clip_origin_phys;
        let child_inline_start = axis.inline_start(&child.bounds);
        let origin = if axis.block_is_horizontal {
          Point::new(child_block_start, child_inline_start)
        } else {
          Point::new(child_inline_start, child_block_start)
        };
        item_fragment.translate_root_in_place(origin);
        cloned.children_mut().push(item_fragment);
      }
      continue;
    }

    let child_writing_mode = child
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(style.writing_mode);
    let child_axis =
      axis_for_child_in_context(axis, context, style.writing_mode, child_writing_mode);
    if let Some(child_clipped) = clip_node(
      child,
      &child_axis,
      fragment_start,
      fragment_end,
      node_flow_start,
      clipped_bbox_flow_start,
      clipped_bbox_flow_end,
      original_node_block_size,
      fragment_index,
      fragment_count,
      context,
      fragmentainer_size,
      axes,
    )? {
      cloned.children_mut().push(child_clipped);
    }
  }

  if matches!(style.display, Display::Table | Display::InlineTable) {
    inject_table_headers_and_footers(
      node,
      &mut cloned,
      fragment_index,
      fragment_count,
      axis,
      context,
    );
  }

  // Forced breaks inside floats are modeled by shifting descendants so continuation content lands on
  // later pages (see `apply_float_parallel_flow_forced_break_shifts`). That makes the float's
  // logical bounding box extend beyond its own border box so descendants can be clipped onto later
  // pages/columns.
  //
  // However, the blank space introduced by those shifts is part of the float's *parallel flow* and
  // should not block the main flow within the current fragmentainer (e.g. `clear: both` content).
  //
  // Only apply trimming when the float was effectively "inflated" beyond its border box by
  // descendant overflow. This avoids shrinking fixed-size floats (or floats with padding/borders)
  // that legitimately occupy space even when their children do not.
  let float_inflated_by_overflow =
    node_bbox_flow_end > node_flow_end + BREAK_EPSILON || node_bbox_flow_start + BREAK_EPSILON < node_flow_start;
  if style.float.is_floating()
    && axis.block_positive
    && !node.children.is_empty()
    && float_inflated_by_overflow
  {
    // Use the children that survived clipping to find the actual block-axis extent of this float
    // fragment. `cloned.bounds` is based on the intersection of the float's (potentially expanded)
    // logical bounding box with the fragmentainer window, which can include trailing blank space.
    let original_block_size = axis.block_size(&cloned.bounds);
    let mut max_flow_end = 0.0f32;
    for child in cloned.children.iter() {
      let (_, end) = axis.flow_range(0.0, original_block_size, &child.bounds);
      if end.is_finite() {
        max_flow_end = max_flow_end.max(end);
      }
    }

    if max_flow_end <= BREAK_EPSILON {
      // No visible content in this float slice; dropping the fragment avoids generating empty
      // float fragments that would otherwise block following cleared content.
      return Ok(None);
    }

    if max_flow_end + BREAK_EPSILON < original_block_size {
      let block_start = axis.block_start(&cloned.bounds);
      cloned.bounds = axis.update_block_components(cloned.bounds, block_start, max_flow_end);
      // Update slice metadata so background painting and other consumers see the trimmed extent.
      let slice_offset = cloned.slice_info.slice_offset;
      let slice_end_offset = slice_offset + max_flow_end;
      let epsilon = 0.01;
      cloned.slice_info.is_last = slice_end_offset >= cloned.slice_info.original_block_size - epsilon;
    }
  }

  Ok(Some(cloned))
}

/// Axis-aware wrapper around `clip_node` for callers that use `FragmentAxes`.
pub(crate) fn clip_node_with_axes(
  node: &FragmentNode,
  fragment_start: f32,
  fragment_end: f32,
  parent_abs_start: f32,
  parent_clipped_abs_start: f32,
  parent_block_size: f32,
  axes: FragmentAxes,
  fragment_index: usize,
  fragment_count: usize,
  context: FragmentationContext,
  fragmentainer_size: f32,
) -> Result<Option<FragmentNode>, LayoutError> {
  let axis = axis_from_fragment_axes(axes);
  clip_node(
    node,
    &axis,
    fragment_start,
    fragment_end,
    parent_abs_start,
    parent_clipped_abs_start,
    parent_clipped_abs_start + (fragment_end - fragment_start),
    parent_block_size,
    fragment_index,
    fragment_count,
    context,
    fragmentainer_size,
    axes,
  )
}

fn clone_without_children(node: &FragmentNode) -> FragmentNode {
  FragmentNode {
    bounds: node.bounds,
    block_metadata: node.block_metadata.clone(),
    logical_override: node.logical_override,
    content: node.content.clone(),
    table_borders: node.table_borders.clone(),
    grid_tracks: node.grid_tracks.clone(),
    baseline: node.baseline,
    children: FragmentChildren::default(),
    style: node.style.clone(),
    starting_style: node.starting_style.clone(),
    fragment_index: node.fragment_index,
    fragment_count: node.fragment_count,
    fragmentainer_index: node.fragmentainer_index,
    fragmentainer: node.fragmentainer,
    slice_info: node.slice_info,
    scroll_overflow: node.scroll_overflow,
    fragmentation: node.fragmentation.clone(),
    grid_fragmentation: node.grid_fragmentation.clone(),
  }
}

fn is_table_header_fragment(node: &FragmentNode) -> bool {
  node
    .style
    .as_ref()
    .is_some_and(|s| matches!(s.display, Display::TableHeaderGroup))
}

fn is_table_footer_fragment(node: &FragmentNode) -> bool {
  node
    .style
    .as_ref()
    .is_some_and(|s| matches!(s.display, Display::TableFooterGroup))
}

fn collect_table_repetition_info_with_axis(
  root: &FragmentNode,
  abs_start: f32,
  axis: &FragmentAxis,
  context: FragmentationContext,
  inherited_writing_mode: WritingMode,
) -> Vec<TableRepetitionInfo> {
  let mut out = Vec::new();
  let default_style = default_style();

  fn walk(
    node: &FragmentNode,
    abs_start: f32,
    axis: &FragmentAxis,
    context: FragmentationContext,
    inherited_writing_mode: WritingMode,
    default_style: &ComputedStyle,
    out: &mut Vec<TableRepetitionInfo>,
  ) {
    let style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let node_writing_mode = node
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(inherited_writing_mode);
    let node_block_size = axis.block_size(&node.bounds);

    if matches!(style.display, Display::Table | Display::InlineTable) {
      let mut header_block_size = 0.0f32;
      let mut footer_block_size = 0.0f32;
      for child in node.children.iter().filter(|c| is_table_header_fragment(c)) {
        let (start, end) = axis.flow_range(abs_start, node_block_size, &child.bounds);
        header_block_size += (end - start).max(0.0);
      }
      for child in node.children.iter().filter(|c| is_table_footer_fragment(c)) {
        let (start, end) = axis.flow_range(abs_start, node_block_size, &child.bounds);
        footer_block_size += (end - start).max(0.0);
      }
      if header_block_size > BREAK_EPSILON || footer_block_size > BREAK_EPSILON {
        out.push(TableRepetitionInfo {
          start: abs_start,
          end: abs_start + node_block_size,
          header_block_size,
          footer_block_size,
        });
      }
    }

    for child in node.children.iter() {
      let child_writing_mode = child
        .style
        .as_ref()
        .map(|s| s.writing_mode)
        .unwrap_or(node_writing_mode);
      let child_axis =
        axis_for_child_in_context(axis, context, node_writing_mode, child_writing_mode);
      let child_abs_start = child_axis.flow_range(abs_start, node_block_size, &child.bounds).0;
      walk(
        child,
        child_abs_start,
        &child_axis,
        context,
        child_writing_mode,
        default_style,
        out,
      );
    }
  }

  walk(
    root,
    abs_start,
    axis,
    context,
    inherited_writing_mode,
    default_style,
    &mut out,
  );
  out
}

pub(crate) fn collect_table_repetition_info_with_axes(
  root: &FragmentNode,
  axes: FragmentAxes,
  context: FragmentationContext,
) -> Vec<TableRepetitionInfo> {
  let axis = axis_from_fragment_axes(axes);
  let default_style = default_style();
  let root_writing_mode = root
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(default_style.writing_mode);
  collect_table_repetition_info_with_axis(root, 0.0, &axis, context, root_writing_mode)
}

fn table_header_overhead_at(tables: &[TableRepetitionInfo], pos: f32) -> f32 {
  tables
    .iter()
    .filter(|info| {
      info.header_block_size > BREAK_EPSILON
        && pos > info.start + BREAK_EPSILON
        && pos < info.end - BREAK_EPSILON
    })
    .map(|info| info.header_block_size)
    .sum()
}

fn innermost_footer_table_at<'a>(
  tables: &'a [TableRepetitionInfo],
  pos: f32,
) -> Option<&'a TableRepetitionInfo> {
  tables
    .iter()
    .filter(|info| {
      info.footer_block_size > BREAK_EPSILON
        && pos > info.start + BREAK_EPSILON
        && pos < info.end - BREAK_EPSILON
    })
    .max_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal))
}

fn inject_table_headers_and_footers(
  original: &FragmentNode,
  clipped: &mut FragmentNode,
  fragment_index: usize,
  fragment_count: usize,
  axis: &FragmentAxis,
  context: FragmentationContext,
) {
  let default_style = default_style();
  let table_writing_mode = original
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(default_style.writing_mode);
  let axis_for_candidate = |candidate: &FragmentNode| -> FragmentAxis {
    let candidate_writing_mode = candidate
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(table_writing_mode);
    axis_for_child_in_context(axis, context, table_writing_mode, candidate_writing_mode)
  };

  let headers: Vec<_> = original
    .children
    .iter()
    .filter(|c| is_table_header_fragment(c))
    .collect();
  let footers: Vec<_> = original
    .children
    .iter()
    .filter(|c| is_table_footer_fragment(c))
    .collect();
  if headers.is_empty() && footers.is_empty() {
    return;
  }

  let has_header = clipped.children.iter().any(is_table_header_fragment);
  let has_footer = clipped.children.iter().any(is_table_footer_fragment);

  let mut max_block_extent = axis.block_size(&clipped.bounds);
  let original_block_size = axis.block_size(&original.bounds);
  let clipped_block_size = axis.block_size(&clipped.bounds);
  // `clip_node` rebases descendants so the fragment's clipping window becomes the new origin for
  // child coordinates. When duplicating table headers/footers, the candidate fragments come from
  // the original coordinate system, so we must apply the same rebasing transform to keep the
  // injected fragments aligned with the clipped slice.
  let base_offset = original.slice_info.slice_offset.max(0.0);
  let slice_start = (clipped.slice_info.slice_offset - base_offset).max(0.0);
  let clip_origin_phys =
    axis.flow_box_start_to_physical(slice_start, clipped_block_size, original_block_size);
  let rebase_translation = if axis.block_is_horizontal {
    Point::new(-clip_origin_phys, 0.0)
  } else {
    Point::new(0.0, -clip_origin_phys)
  };

  if !headers.is_empty() && !has_header && !clipped.slice_info.is_first {
    let mut regions = Vec::new();
    for header in &headers {
      let header_axis = axis_for_candidate(header);
      let (start, end) = header_axis.flow_range(0.0, original_block_size, &header.bounds);
      regions.push((start, end));
    }
    let region_height: f32 = regions.iter().map(|(s, e)| e - s).sum();
    for child in clipped.children_mut() {
      let child_axis = axis_for_candidate(child);
      translate_fragment_in_parent_space(child, child_axis.block_translation(region_height));
    }
    let mut offset = 0.0;
    let mut clones = Vec::new();
    for (start, end) in regions {
      for candidate in original.children.iter() {
        let Some(style) = candidate.style.as_ref() else {
          continue;
        };
        if !matches!(
          style.display,
          Display::TableHeaderGroup
            | Display::TableFooterGroup
            | Display::TableRowGroup
            | Display::TableRow
            | Display::TableCell
        ) {
          continue;
        }
        let candidate_axis = axis_for_candidate(candidate);
        let (c_start, c_end) =
          candidate_axis.flow_range(0.0, original_block_size, &candidate.bounds);
        if c_start + 0.01 >= start && c_end <= end + 0.01 {
          let mut clone = candidate.clone();
          translate_fragment_in_parent_space(
            &mut clone,
            axis.block_translation(slice_start + offset - start),
          );
          translate_fragment_in_parent_space(&mut clone, rebase_translation);
          propagate_fragment_metadata(&mut clone, fragment_index, fragment_count);
          clones.push(clone);
        }
      }
      offset += end - start;
    }
    max_block_extent = max_block_extent.max(offset);
    clipped.children_mut().splice(0..0, clones);
  }

  if !footers.is_empty() && !has_footer && !clipped.slice_info.is_last {
    let mut regions = Vec::new();
    for footer in &footers {
      let footer_axis = axis_for_candidate(footer);
      let (start, end) = footer_axis.flow_range(0.0, original_block_size, &footer.bounds);
      regions.push((start, end));
    }
    let footer_start = clipped
      .children
      .iter()
      .map(|c| {
        let child_axis = axis_for_candidate(c);
        child_axis.flow_range(0.0, clipped_block_size, &c.bounds).1
      })
      .fold(0.0, f32::max);
    let mut footer_offset = footer_start;
    let mut clones = Vec::new();
    for (start, end) in regions {
      let region_translation = axis.block_translation(slice_start + footer_offset - start);
      for candidate in original.children.iter() {
        let Some(style) = candidate.style.as_ref() else {
          continue;
        };
        if !matches!(
          style.display,
          Display::TableHeaderGroup
            | Display::TableFooterGroup
            | Display::TableRowGroup
            | Display::TableRow
            | Display::TableCell
        ) {
          continue;
        }
        let candidate_axis = axis_for_candidate(candidate);
        let (c_start, c_end) =
          candidate_axis.flow_range(0.0, original_block_size, &candidate.bounds);
        if c_start + 0.01 >= start && c_end <= end + 0.01 {
          let mut clone = candidate.clone();
          translate_fragment_in_parent_space(
            &mut clone,
            region_translation,
          );
          translate_fragment_in_parent_space(&mut clone, rebase_translation);
          propagate_fragment_metadata(&mut clone, fragment_index, fragment_count);
          clones.push(clone);
        }
      }
      footer_offset += (end - start).max(0.0);
    }
    max_block_extent = max_block_extent.max(footer_offset);
    clipped.children_mut().extend(clones);
  }

  let children_block_end = clipped
    .children
    .iter()
    .map(|c| {
      let child_axis = axis_for_candidate(c);
      child_axis.flow_range(0.0, clipped_block_size, &c.bounds).1
    })
    .fold(0.0, f32::max);
  let new_block_size = clipped_block_size
    .max(max_block_extent)
    .max(children_block_end);
  clipped.bounds = axis.update_block_components(
    clipped.bounds,
    axis.block_start(&clipped.bounds),
    new_block_size,
  );
  let mut scroll_overflow = clipped.scroll_overflow;
  let block_overflow = axis.block_size(&scroll_overflow).max(new_block_size);
  let inline_overflow = axis
    .inline_size(&scroll_overflow)
    .max(axis.inline_size(&clipped.bounds));
  scroll_overflow = if axis.block_is_horizontal {
    Rect::from_xywh(
      scroll_overflow.x(),
      scroll_overflow.y(),
      block_overflow,
      inline_overflow,
    )
  } else {
    Rect::from_xywh(
      scroll_overflow.x(),
      scroll_overflow.y(),
      inline_overflow,
      block_overflow,
    )
  };
  clipped.scroll_overflow = scroll_overflow;
}

fn translate_fragment_in_parent_space(node: &mut FragmentNode, offset: Point) {
  node.bounds = node.bounds.translate(offset);
  if let Some(logical) = node.logical_override {
    node.logical_override = Some(logical.translate(offset));
  }
}

pub(crate) fn normalize_fragment_margins(
  fragment: &mut FragmentNode,
  is_first_fragment: bool,
  is_last_fragment: bool,
  axis: &FragmentAxis,
) {
  const EPSILON: f32 = 0.01;
  let fragment_block_size = axis.block_size(&fragment.bounds);

  // Reset carried collapsed margin from previous fragmentainer by reapplying the fragment's own
  // top margin to the first block that starts this slice.
  if !is_first_fragment {
    if let Some(min_start) = fragment
      .children
      .iter()
      .map(|c| {
        axis.flow_offset(
          axis.block_start(&c.bounds),
          axis.block_size(&c.bounds),
          fragment_block_size,
        )
      })
      .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    {
      for child in fragment.children_mut().iter_mut().filter(|c| {
        let start = axis.flow_offset(
          axis.block_start(&c.bounds),
          axis.block_size(&c.bounds),
          fragment_block_size,
        );
        (start - min_start).abs() < EPSILON
      }) {
        if let Some(meta) = child.block_metadata.as_ref() {
          if meta.clipped_top {
            continue;
          }
          let desired_top = meta.margin_top;
          let child_start = axis.flow_offset(
            axis.block_start(&child.bounds),
            axis.block_size(&child.bounds),
            fragment_block_size,
          );
          let delta = desired_top - child_start;
          if delta.abs() > EPSILON {
            translate_fragment_in_parent_space(child, axis.block_translation(delta));
          }
        }
      }
    }
  }

  // Include the trailing margin of the last complete block when this slice is not the final one.
  if !is_last_fragment {
    if let Some((max_end, meta)) = fragment
      .children
      .iter()
      .filter_map(|c| {
        let block_size = axis.block_size(&c.bounds);
        let start = axis.flow_offset(axis.block_start(&c.bounds), block_size, fragment_block_size);
        c.block_metadata.as_ref().map(|m| (start + block_size, m))
      })
      .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    {
      let target_block = if meta.clipped_bottom {
        max_end
      } else {
        max_end + meta.margin_bottom
      };
      let new_block_size = fragment_block_size.max(target_block);
      fragment.bounds = axis.update_block_components(
        fragment.bounds,
        axis.block_start(&fragment.bounds),
        new_block_size,
      );
      let mut scroll_overflow = fragment.scroll_overflow;
      let block_overflow = axis.block_size(&scroll_overflow).max(new_block_size);
      let inline_overflow = axis
        .inline_size(&scroll_overflow)
        .max(axis.inline_size(&fragment.bounds));
      scroll_overflow = if axis.block_is_horizontal {
        Rect::from_xywh(
          scroll_overflow.x(),
          scroll_overflow.y(),
          block_overflow,
          inline_overflow,
        )
      } else {
        Rect::from_xywh(
          scroll_overflow.x(),
          scroll_overflow.y(),
          inline_overflow,
          block_overflow,
        )
      };
      fragment.scroll_overflow = scroll_overflow;
    }
  }
}

/// Axis-aware wrapper around [`normalize_fragment_margins`].
pub(crate) fn normalize_fragment_margins_with_axes(
  fragment: &mut FragmentNode,
  is_first_fragment: bool,
  is_last_fragment: bool,
  _fragment_block_size: f32,
  axes: FragmentAxes,
) {
  let axis = axis_from_fragment_axes(axes);
  normalize_fragment_margins(fragment, is_first_fragment, is_last_fragment, &axis);
}

fn collect_break_opportunities(
  node: &FragmentNode,
  abs_start: f32,
  collection: &mut BreakCollection,
  avoid_depth: usize,
  inline_depth: usize,
  context: FragmentationContext,
  axis: &FragmentAxis,
  inherited_writing_mode: WritingMode,
  suppress_float_descendants: bool,
) {
  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  let node_writing_mode = node
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(inherited_writing_mode);
  let is_table_row_like = matches!(
    style.display,
    Display::TableRow
      | Display::TableRowGroup
      | Display::TableHeaderGroup
      | Display::TableFooterGroup
  );
  let inside_avoid = avoid_depth
    + usize::from(avoids_break_inside(style.break_inside, context))
    + usize::from(is_table_row_like);
  let inside_inline = inline_depth
    + usize::from(matches!(
      node.content,
      FragmentContent::Line { .. } | FragmentContent::Inline { .. }
    ));

  if suppress_float_descendants && style.float.is_floating() {
    return;
  }

  let node_block_size = axis.block_size(&node.bounds);
  let node_flow_start = abs_start;
  let abs_end = abs_start + node_block_size;

  // When the fragment includes both grid track ranges and per-item placement metadata, break hints
  // on grid items apply to the corresponding grid line boundaries (CSS Grid 2 §Fragmenting Grid
  // Layout).
  let mut grid_item_count = 0usize;
  if matches!(style.display, Display::Grid | Display::InlineGrid) {
    if let (Some(grid_tracks), Some(grid_items)) = (
      node.grid_tracks.as_deref(),
      node.grid_fragmentation.as_deref(),
    ) {
      let tracks = grid_tracks_in_fragmentation_axis(grid_tracks, axis);
      if !tracks.is_empty() && !grid_items.items.is_empty() {
        let in_flow_count = grid_items.items.len().min(node.children.len());
        grid_item_count = in_flow_count;

        // One slot per grid line (track_count + 1). Index `i` corresponds to the boundary at line
        // `i + 1` in the fragmentation axis.
        let mut boundary_strengths = vec![BreakStrength::Auto; tracks.len() + 1];

        for idx in 0..in_flow_count {
          let child = &node.children[idx];
          let child_style = child
            .style
            .as_ref()
            .map(|s| s.as_ref())
            .unwrap_or(default_style);
          let placement = &grid_items.items[idx];
          let (start_line, end_line) = grid_item_lines_in_fragmentation_axis(placement, axis);

          let before_strength =
            combine_breaks(BreakBetween::Auto, child_style.break_before, context);
          if !matches!(before_strength, BreakStrength::Auto) {
            let boundary_idx = start_line.saturating_sub(1) as usize;
            if let Some(slot) = boundary_strengths.get_mut(boundary_idx) {
              *slot = max_break_strength(*slot, before_strength);
            }
          }

          let after_strength = combine_breaks(child_style.break_after, BreakBetween::Auto, context);
          if !matches!(after_strength, BreakStrength::Auto) {
            let boundary_idx = end_line.saturating_sub(1) as usize;
            if let Some(slot) = boundary_strengths.get_mut(boundary_idx) {
              *slot = max_break_strength(*slot, after_strength);
            }
          }
        }

        for (boundary_idx, strength) in boundary_strengths.into_iter().enumerate() {
          if matches!(strength, BreakStrength::Auto) {
            continue;
          }
          let strength = apply_avoid_penalty(strength, inside_avoid > 0);
          let pos = if boundary_idx == 0 {
            abs_start
          } else if boundary_idx == tracks.len() {
            abs_end
          } else {
            let Some((track_start, track_end)) =
              tracks.get(boundary_idx.saturating_sub(1)).copied()
            else {
              continue;
            };
            let track_size = track_end - track_start;
            if track_size <= BREAK_EPSILON {
              continue;
            }
            // The gutter belongs to the following band; align the boundary to the end edge of the
            // preceding track (in the flow direction) so breaks never land after a
            // `row-gap`/`column-gap`.
            abs_start + axis.flow_offset(track_start, track_size, node_block_size) + track_size
          };

          collection.opportunities.push(BreakOpportunity {
            pos,
            strength,
            kind: BreakKind::BetweenSiblings,
          });
        }
      }
    }
  }

  // CSS Flexbox 1 §Fragmenting Flex Layout: in row flex containers, break opportunities occur
  // between flex lines (not between flex items). Break hints on items apply to the flex line
  // boundary, and forced breaks on the first/last line propagate to the container edges to avoid
  // gap-only pages.
  if is_row_flex_container(style) {
    if let Some(flex_lines) = collect_row_flex_lines(
      node,
      abs_start,
      axis,
      node_block_size,
      style,
      node_writing_mode,
      default_style,
    ) {
      let line_count = flex_lines.lines.len();
      let mut boundary_strengths = vec![BreakStrength::Auto; line_count + 1];

      for (child_idx, line_idx) in flex_lines.line_for_child.iter().enumerate() {
        let Some(line_idx) = *line_idx else {
          continue;
        };
        let child = &node.children[child_idx];
        let child_style = child
          .style
          .as_ref()
          .map(|s| s.as_ref())
          .unwrap_or(default_style);

        let before_strength = combine_breaks(BreakBetween::Auto, child_style.break_before, context);
        if !matches!(before_strength, BreakStrength::Auto) {
          if let Some(slot) = boundary_strengths.get_mut(line_idx) {
            *slot = max_break_strength(*slot, before_strength);
          }
        }

        let after_strength = combine_breaks(child_style.break_after, BreakBetween::Auto, context);
        if !matches!(after_strength, BreakStrength::Auto) {
          if let Some(slot) = boundary_strengths.get_mut(line_idx + 1) {
            *slot = max_break_strength(*slot, after_strength);
          }
        }
      }

      let base_strength = apply_avoid_penalty(BreakStrength::Auto, inside_avoid > 0);
      let first_line_start = flex_lines.lines[0].start;
      collection.opportunities.push(BreakOpportunity {
        pos: first_line_start,
        strength: base_strength,
        kind: BreakKind::BetweenSiblings,
      });

      for (idx, line) in flex_lines.lines.iter().enumerate() {
        let boundary_idx = idx + 1;
        // Break-after on the last line propagates to the flex container; keep the line boundary as
        // a normal (auto) break opportunity so forced breaks don't create gap-only fragmentainers
        // containing just trailing padding/align-content spacing.
        let mut strength = if boundary_idx == line_count {
          BreakStrength::Auto
        } else {
          boundary_strengths[boundary_idx]
        };
        strength = apply_avoid_penalty(strength, inside_avoid > 0);
        collection.opportunities.push(BreakOpportunity {
          pos: line.end,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }

      let start_strength = boundary_strengths[0];
      if !matches!(start_strength, BreakStrength::Auto) {
        let strength = apply_avoid_penalty(start_strength, inside_avoid > 0);
        collection.opportunities.push(BreakOpportunity {
          pos: abs_start,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }
      let end_strength = boundary_strengths[line_count];
      if !matches!(end_strength, BreakStrength::Auto) {
        let strength = apply_avoid_penalty(end_strength, inside_avoid > 0);
        collection.opportunities.push(BreakOpportunity {
          pos: abs_end,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }

      for child in node.children.iter() {
        let child_writing_mode = child
          .style
          .as_ref()
          .map(|s| s.writing_mode)
          .unwrap_or(node_writing_mode);
        let child_axis =
          axis_for_child_in_context(axis, context, node_writing_mode, child_writing_mode);
        let child_abs_start = child_axis
          .flow_range(node_flow_start, node_block_size, &child.bounds)
          .0;
        collect_break_opportunities(
          child,
          child_abs_start,
          collection,
          inside_avoid,
          inside_inline,
          context,
          &child_axis,
          child_writing_mode,
          suppress_float_descendants,
        );
      }
      return;
    }
  }

  let mut line_positions: Vec<Option<(usize, f32)>> = vec![None; node.children.len()];
  let mut line_ends = Vec::new();
  for (idx, child) in node.children.iter().enumerate() {
    if matches!(child.content, FragmentContent::Line { .. }) {
      let (_, line_end) = axis.flow_range(node_flow_start, node_block_size, &child.bounds);
      line_ends.push(line_end);
      line_positions[idx] = Some((line_ends.len(), line_end));
    }
  }

  let line_container_id = if !line_ends.is_empty() {
    let container_id = collection.line_containers.len();
    collection.line_containers.push(LineContainer {
      id: container_id,
      line_ends: line_ends.clone(),
      widows: style.widows.max(1),
      orphans: style.orphans.max(1),
    });
    Some(container_id)
  } else {
    None
  };

  let grid_items = if matches!(context, FragmentationContext::Page)
    && matches!(style.display, Display::Grid | Display::InlineGrid)
  {
    node.grid_fragmentation.as_ref()
  } else {
    None
  };

  for (idx, child) in node.children.iter().enumerate() {
    let child_writing_mode = child
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(node_writing_mode);
    let child_axis =
      axis_for_child_in_context(axis, context, node_writing_mode, child_writing_mode);
    let (child_abs_start, child_abs_end) =
      child_axis.flow_range(node_flow_start, node_block_size, &child.bounds);
    let child_style = child
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let next_child = node.children.get(idx + 1);
    let next_style = next_child
      .and_then(|c| c.style.as_ref())
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let next_abs_start = next_child.map(|next| {
      let next_writing_mode = next
        .style
        .as_ref()
        .map(|s| s.writing_mode)
        .unwrap_or(node_writing_mode);
      let next_axis =
        axis_for_child_in_context(axis, context, node_writing_mode, next_writing_mode);
      next_axis
        .flow_range(node_flow_start, node_block_size, &next.bounds)
        .0
    });

    if let (Some(container_id), Some((line_index_end, line_end))) =
      (line_container_id, line_positions[idx])
    {
      let mut strength = BreakStrength::Auto;
      if inside_avoid > 0 {
        strength = BreakStrength::Avoid;
      }
      collection.opportunities.push(BreakOpportunity {
        pos: line_end,
        strength,
        kind: BreakKind::LineBoundary {
          container_id,
          line_index_end,
        },
      });
    }

    let child_break_before = if idx < grid_item_count {
      BreakBetween::Auto
    } else {
      child_style.break_before
    };
    if idx == 0 && !matches!(child_break_before, BreakBetween::Auto) {
      let mut strength = combine_breaks(BreakBetween::Auto, child_break_before, context);
      strength = apply_avoid_penalty(strength, inside_avoid > 0);
      if strength == BreakStrength::Auto
        && matches!(child.content, FragmentContent::Block { box_id: None })
      {
        strength = BreakStrength::Avoid;
      }
      // Forced breaks before the first in-flow child should propagate to the start of the parent
      // block rather than occurring at the child's border box start (which may be offset by
      // padding/margins). This matches the CSS Break "break propagation" behavior and prevents
      // fragmentainers that contain only leading padding space.
      let pos = if matches!(strength, BreakStrength::Forced) {
        abs_start
      } else {
        child_abs_start
      };
      collection.opportunities.push(BreakOpportunity {
        pos,
        strength,
        kind: BreakKind::BetweenSiblings,
      });
    }

    let skip_descendants = grid_items
      .and_then(|info| info.items.get(idx))
      .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    if !skip_descendants {
      collect_break_opportunities(
        child,
        child_abs_start,
        collection,
        inside_avoid,
        inside_inline,
        context,
        &child_axis,
        child_writing_mode,
        suppress_float_descendants,
      );
    }

    let child_break_after = if idx < grid_item_count {
      BreakBetween::Auto
    } else {
      child_style.break_after
    };
    let next_break_before = if idx + 1 < grid_item_count {
      BreakBetween::Auto
    } else {
      next_style.break_before
    };
    let mut strength = combine_breaks(child_break_after, next_break_before, context);
    strength = apply_avoid_penalty(strength, inside_avoid > 0);
    if strength == BreakStrength::Auto
      && matches!(child.content, FragmentContent::Block { box_id: None })
    {
      strength = BreakStrength::Avoid;
    }
    // Break opportunities between siblings span the entire gap between the end of one fragment and
    // the start of the next. `child_abs_end` is only the end of the current child itself; when the
    // next sibling begins later (e.g. due to margins), consumers are still free to break anywhere
    // inside the gap. Record the boundary at the next sibling's start when available so the
    // boundary-selection logic can still choose the fragmentainer limit without being biased toward
    // an early break.
    let mut boundary_pos = child_abs_end;
    if !matches!(strength, BreakStrength::Forced) {
      if let Some(next_start) = next_abs_start {
        if next_start > boundary_pos {
          boundary_pos = next_start;
        }
      }
    }

    if matches!(strength, BreakStrength::Forced) {
      if let Some(meta) = child.block_metadata.as_ref() {
        let mut candidate = child_abs_end + meta.margin_bottom;
        if candidate < child_abs_end {
          candidate = child_abs_end;
        }
        if let Some(next_start) = next_abs_start {
          candidate = candidate.min(next_start);
        }
        boundary_pos = candidate;
      }
    }
    let include_boundary = if inside_inline > 0 {
      strength != BreakStrength::Auto
    } else {
      match child.content {
        FragmentContent::Line { .. }
        | FragmentContent::Inline { .. }
        | FragmentContent::Text { .. } => strength != BreakStrength::Auto,
        _ => true,
      }
    };
    if include_boundary {
      collection.opportunities.push(BreakOpportunity {
        pos: boundary_pos,
        strength,
        kind: BreakKind::BetweenSiblings,
      });
    }
  }
}

pub(crate) fn collect_forced_boundaries(
  node: &FragmentNode,
  abs_start: f32,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_with_axes(node, abs_start, axes_from_root(node))
}

pub(crate) fn collect_forced_boundaries_for_pagination(
  node: &FragmentNode,
  abs_start: f32,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_for_pagination_with_axes(node, abs_start, axes_from_root(node))
}

pub(crate) fn collect_forced_boundaries_for_pagination_with_axes(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_with_axes_internal(node, abs_start, axes, true)
}

pub(crate) fn collect_forced_boundaries_with_axes(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_with_axes_internal(node, abs_start, axes, false)
}

fn collect_forced_boundaries_with_axes_internal(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
  suppress_parallel_grid_item_descendants: bool,
) -> Vec<ForcedBoundary> {
  let page_progression_is_ltr = axes.page_progression_is_ltr();

  fn is_forced_page_break(between: BreakBetween) -> bool {
    matches!(
      between,
      BreakBetween::Always
        | BreakBetween::Page
        | BreakBetween::Left
        | BreakBetween::Right
        | BreakBetween::Recto
        | BreakBetween::Verso
    )
  }

  fn break_side_hint(between: BreakBetween, page_progression_is_ltr: bool) -> Option<PageSide> {
    match between {
      BreakBetween::Left => Some(PageSide::Left),
      BreakBetween::Right => Some(PageSide::Right),
      BreakBetween::Verso => Some(if page_progression_is_ltr {
        PageSide::Left
      } else {
        PageSide::Right
      }),
      BreakBetween::Recto => Some(if page_progression_is_ltr {
        PageSide::Right
      } else {
        PageSide::Left
      }),
      _ => None,
    }
  }

  #[derive(Clone, Copy, Default)]
  struct BoundaryRequirement {
    forced: bool,
    side: Option<PageSide>,
  }

  fn merge_boundary_side(current: &mut Option<PageSide>, incoming: Option<PageSide>) {
    match (*current, incoming) {
      (None, side) => *current = side,
      (side, None) => *current = side,
      (Some(a), Some(b)) if a == b => *current = Some(a),
      (Some(_), Some(_)) => *current = None,
    }
  }

  fn record_boundary(requirement: &mut BoundaryRequirement, side: Option<PageSide>) {
    requirement.forced = true;
    merge_boundary_side(&mut requirement.side, side);
  }

  fn collect(
    node: &FragmentNode,
    abs_start: f32,
    forced: &mut Vec<ForcedBoundary>,
    default_style: &ComputedStyle,
    axis: &FragmentAxis,
    parent_block_size: f32,
    suppress_parallel_grid_item_descendants: bool,
    page_progression_is_ltr: bool,
  ) {
    let node_style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    if suppress_parallel_grid_item_descendants && node_style.float.is_floating() {
      return;
    }
    let grid_items = if matches!(node_style.display, Display::Grid | Display::InlineGrid) {
      node.grid_fragmentation.as_deref()
    } else {
      None
    };
    let in_flow_grid_item_count = grid_items
      .as_ref()
      .map(|grid_items| grid_items.items.len().min(node.children.len()))
      .unwrap_or(0);

    let mut grid_item_count = 0usize;
    if matches!(node_style.display, Display::Grid | Display::InlineGrid) {
      if let (Some(grid_tracks), Some(grid_items)) = (node.grid_tracks.as_deref(), grid_items) {
        let tracks = grid_tracks_in_fragmentation_axis(grid_tracks, axis);
        if !tracks.is_empty() && !grid_items.items.is_empty() {
          let in_flow_count = grid_items.items.len().min(node.children.len());
          // One slot per grid line (track_count + 1). Index `i` corresponds to the boundary at line
          // `i + 1`.
          let mut boundary_reqs = vec![BoundaryRequirement::default(); tracks.len() + 1];

          for idx in 0..in_flow_count {
            let child = &node.children[idx];
            let child_style = child
              .style
              .as_ref()
              .map(|s| s.as_ref())
              .unwrap_or(default_style);
            let placement = &grid_items.items[idx];
            let (start_line, end_line) = grid_item_lines_in_fragmentation_axis(placement, axis);

            if is_forced_page_break(child_style.break_before) {
              let boundary_idx = start_line.saturating_sub(1) as usize;
              if let Some(req) = boundary_reqs.get_mut(boundary_idx) {
                record_boundary(
                  req,
                  break_side_hint(child_style.break_before, page_progression_is_ltr),
                );
              }
            }

            if is_forced_page_break(child_style.break_after) {
              let boundary_idx = end_line.saturating_sub(1) as usize;
              if let Some(req) = boundary_reqs.get_mut(boundary_idx) {
                record_boundary(
                  req,
                  break_side_hint(child_style.break_after, page_progression_is_ltr),
                );
              }
            }
          }

          if boundary_reqs.iter().any(|req| req.forced) {
            // Grid line boundaries should align to row/column band atomic ranges. Our atomic range
            // collection treats the gutter *following* a track as belonging to the next band, so the
            // boundary before line `i + 1` is the end edge of track `i` (rather than the start edge
            // of track `i + 1`, which would land after the gutter and can create gap-only pages when
            // page sizes line up exactly with track ends).
            let mut track_flow_ends = Vec::with_capacity(tracks.len());
            for (track_start, track_end) in tracks.iter().copied() {
              let track_size = (track_end - track_start).max(0.0);
              if !track_start.is_finite() {
                track_flow_ends.push(abs_start);
                continue;
              }
              track_flow_ends.push(
                abs_start
                  + axis.flow_offset(track_start, track_size, parent_block_size)
                  + track_size,
              );
            }

            if boundary_reqs[0].forced {
              forced.push(ForcedBoundary {
                position: abs_start,
                page_side: boundary_reqs[0].side,
              });
            }
            for idx in 1..tracks.len() {
              let req = boundary_reqs[idx];
              if !req.forced {
                continue;
              }
              if let Some(&position) = track_flow_ends.get(idx.saturating_sub(1)) {
                forced.push(ForcedBoundary {
                  position,
                  page_side: req.side,
                });
              }
            }
            let end_req = boundary_reqs[tracks.len()];
            if end_req.forced {
              forced.push(ForcedBoundary {
                position: abs_start + parent_block_size,
                page_side: end_req.side,
              });
            }

            // Grid items are the first in-flow children; remember how many so we can avoid emitting
            // forced boundaries at their own fragment ends below.
            grid_item_count = in_flow_count;
          }
        }
      }
    }

    let mut flex_line_map: Option<Vec<Option<usize>>> = None;
    if is_row_flex_container(node_style) {
      let node_block_size = parent_block_size;
      if let Some(flex_lines) = collect_row_flex_lines(
        node,
        abs_start,
        axis,
        node_block_size,
        node_style,
        node_style.writing_mode,
        default_style,
      ) {
        let line_count = flex_lines.lines.len();
        let mut boundary_reqs = vec![BoundaryRequirement::default(); line_count + 1];

        for (child_idx, line_idx) in flex_lines.line_for_child.iter().enumerate() {
          let Some(line_idx) = *line_idx else {
            continue;
          };
          let child = &node.children[child_idx];
          let child_style = child
            .style
            .as_ref()
            .map(|s| s.as_ref())
            .unwrap_or(default_style);

          if is_forced_page_break(child_style.break_before) {
            if let Some(req) = boundary_reqs.get_mut(line_idx) {
              record_boundary(
                req,
                break_side_hint(child_style.break_before, page_progression_is_ltr),
              );
            }
          }

          if is_forced_page_break(child_style.break_after) {
            if let Some(req) = boundary_reqs.get_mut(line_idx + 1) {
              record_boundary(
                req,
                break_side_hint(child_style.break_after, page_progression_is_ltr),
              );
            }
          }
        }

        if boundary_reqs.iter().any(|req| req.forced) {
          let line_ends: Vec<f32> = flex_lines.lines.iter().map(|line| line.end).collect();

          if boundary_reqs[0].forced {
            forced.push(ForcedBoundary {
              position: abs_start,
              page_side: boundary_reqs[0].side,
            });
          }

          for idx in 1..line_count {
            let req = boundary_reqs[idx];
            if !req.forced {
              continue;
            }
            if let Some(&position) = line_ends.get(idx.saturating_sub(1)) {
              forced.push(ForcedBoundary {
                position,
                page_side: req.side,
              });
            }
          }

          let end_req = boundary_reqs[line_count];
          if end_req.forced {
            forced.push(ForcedBoundary {
              position: abs_start + parent_block_size,
              page_side: end_req.side,
            });
          }

          flex_line_map = Some(flex_lines.line_for_child);
        }
      }
    }

    for (idx, child) in node.children.iter().enumerate() {
      let child_block_size = axis.block_size(&child.bounds);
      let (child_abs_start, child_abs_end) =
        axis.flow_range(abs_start, parent_block_size, &child.bounds);
      let child_style = child
        .style
        .as_ref()
        .map(|s| s.as_ref())
        .unwrap_or(default_style);
      let next_style = node
        .children
        .get(idx + 1)
        .and_then(|c| c.style.as_ref())
        .map(|s| s.as_ref())
        .unwrap_or(default_style);

      if idx == 0 && is_forced_page_break(child_style.break_before) {
        let break_from_flex = flex_line_map
          .as_ref()
          .and_then(|map| map.get(idx))
          .is_some_and(|slot| slot.is_some());
        if idx >= grid_item_count && !break_from_flex {
          forced.push(ForcedBoundary {
            // Forced breaks before the first in-flow child are treated as applying at the start of
            // the containing block. This matches the CSS Break requirement to suppress leading
            // blank pages: a `break-before` on the first element should influence the initial page
            // side without carving out an empty fragmentainer slice when the element is offset by
            // padding or similar.
            position: abs_start,
            page_side: break_side_hint(child_style.break_before, page_progression_is_ltr),
          });
        }
      }

      let break_after = is_forced_page_break(child_style.break_after);
      let break_before = is_forced_page_break(next_style.break_before);
      if break_after || break_before {
        let break_from_grid =
          (break_after && idx < grid_item_count) || (break_before && idx + 1 < grid_item_count);
        let break_from_flex = flex_line_map.as_ref().is_some_and(|map| {
          let current_is_flex = break_after && map.get(idx).is_some_and(|slot| slot.is_some());
          let next_is_flex = break_before && map.get(idx + 1).is_some_and(|slot| slot.is_some());
          current_is_flex || next_is_flex
        });
        if !break_from_grid && !break_from_flex {
          let mut boundary = child_abs_end;
          if let Some(meta) = child.block_metadata.as_ref() {
            let mut candidate = child_abs_end + meta.margin_bottom;
            if candidate < child_abs_end {
              candidate = child_abs_end;
            }
            if let Some(next_child) = node.children.get(idx + 1) {
              let next_start = axis
                .flow_range(abs_start, parent_block_size, &next_child.bounds)
                .0;
              candidate = candidate.min(next_start);
            }
            boundary = candidate;
          }
          forced.push(ForcedBoundary {
            position: boundary,
            page_side: break_side_hint(next_style.break_before, page_progression_is_ltr).or(
              break_side_hint(child_style.break_after, page_progression_is_ltr),
            ),
          });
        }
      }
      let skip_parallel_flow_descendants = suppress_parallel_grid_item_descendants
        && idx < in_flow_grid_item_count
        && grid_items
          .and_then(|grid_items| grid_items.items.get(idx))
          .map(|placement| grid_item_spans_single_track(placement, axis))
          .unwrap_or(false);
      if !skip_parallel_flow_descendants {
        collect(
          child,
          child_abs_start,
          forced,
          default_style,
          axis,
          child_block_size,
          suppress_parallel_grid_item_descendants,
          page_progression_is_ltr,
        );
      }
    }
  }

  let default_style = default_style();
  let axis = axis_from_fragment_axes(axes);
  let mut boundaries = Vec::new();
  collect(
    node,
    abs_start,
    &mut boundaries,
    default_style,
    &axis,
    axis.block_size(&node.bounds),
    suppress_parallel_grid_item_descendants,
    page_progression_is_ltr,
  );
  boundaries
}

fn constraint_key_for(
  opportunity: &BreakOpportunity,
  line_containers: &[LineContainer],
  line_starts: &[usize],
) -> ConstraintKey {
  if let BreakKind::LineBoundary {
    container_id,
    line_index_end,
  } = opportunity.kind
  {
    if let Some(container) = line_containers.get(container_id) {
      let start_line = line_starts
        .get(container_id)
        .copied()
        .unwrap_or(0)
        .min(line_index_end);
      let lines_in_fragment = line_index_end.saturating_sub(start_line);
      let remaining = container.line_ends.len().saturating_sub(line_index_end);
      let violates_orphans = remaining > 0 && lines_in_fragment < container.orphans;
      let violates_continuation_widows = start_line > 0 && lines_in_fragment < container.widows;
      let violates_future_widows = remaining > 0 && remaining < container.widows;
      return ConstraintKey {
        violates_orphans,
        violates_continuation_widows,
        violates_future_widows,
      };
    }
  }

  ConstraintKey {
    violates_orphans: false,
    violates_continuation_widows: false,
    violates_future_widows: false,
  }
}

fn pos_is_inside_atomic(pos: f32, atomic: &[AtomicRange]) -> bool {
  atomic_containing(pos, atomic).is_some()
}

fn atomic_containing(pos: f32, atomic: &[AtomicRange]) -> Option<AtomicRange> {
  if atomic.is_empty() {
    return None;
  }
  let idx = atomic.partition_point(|range| range.start <= pos + BREAK_EPSILON);
  if idx == 0 {
    return None;
  }
  let candidate = atomic[idx - 1];
  // Atomic ranges represent indivisible content in the flow. Treat the endpoints as break-safe:
  // callers can choose boundaries at the range start/end, but never *inside*.
  if pos > candidate.start + BREAK_EPSILON
    && pos < candidate.end - BREAK_EPSILON
    && (candidate.end - candidate.start) > BREAK_EPSILON
  {
    Some(candidate)
  } else {
    None
  }
}

fn collect_atomic_candidate_for_node(
  node: &FragmentNode,
  abs_start: f32,
  axis: &FragmentAxis,
  parent_block_size: f32,
  candidates: &mut Vec<AtomicCandidate>,
  context: FragmentationContext,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let start = abs_start;
  let mut end = abs_start + node_block_size;
  if end <= start + BREAK_EPSILON {
    return;
  }
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style());

  if style.float.is_floating() {
    // Floats are only atomic when they fit within a single fragmentainer. Compute the candidate
    // range here and defer the "fits" decision to boundary selection.
    //
    // Use the logical bounding box so forced breaks modeled as blank insertion (or other overflow)
    // can expand the float's effective height.
    let parent_abs_flow_start = abs_start
      - axis.flow_offset(
        axis.block_start(&node.bounds),
        node_block_size,
        parent_block_size,
      );
    let bbox = node.logical_bounding_box();
    let bbox_block_size = axis.block_size(&bbox);
    let bbox_flow_start = parent_abs_flow_start
      + axis.flow_offset(axis.block_start(&bbox), bbox_block_size, parent_block_size);
    let bbox_flow_end = bbox_flow_start + bbox_block_size;
    if bbox_flow_end > end + BREAK_EPSILON {
      end = bbox_flow_end;
    }

    let required = (end - start).max(0.0);
    candidates.push(AtomicCandidate {
      range: AtomicRange { start, end },
      required_fragmentainer_size: required,
    });
  }

  let is_table_row_like = matches!(
    style.display,
    Display::TableRow
      | Display::TableRowGroup
      | Display::TableHeaderGroup
      | Display::TableFooterGroup
  );
  let avoid_inside = avoids_break_inside(style.break_inside, context) || is_table_row_like;
  if avoid_inside {
    let required = (end - start).max(0.0);
    candidates.push(AtomicCandidate {
      range: AtomicRange { start, end },
      required_fragmentainer_size: required,
    });
  }

  if matches!(style.display, Display::Grid | Display::InlineGrid) {
    if let Some(grid_tracks) = node.grid_tracks.as_deref() {
      let tracks = grid_tracks_in_fragmentation_axis(grid_tracks, axis);

      // Treat each grid track as indivisible. Additionally, treat the inter-track gutter preceding
      // each track as part of the following track so pagination never splits a `row-gap`/`column-gap`
      // across fragmentainers (and avoids producing a fragmentainer that contains only the gap).
      //
      // Note: the "fits" decision for a track is based on the *track size* (excluding the absorbed
      // gutter). The gutter is empty space; it may force a fragmentainer to under-fill, but should
      // not cause a track band that otherwise fits to become breakable.
      let mut prev_flow_end: Option<f32> = None;
      for (track_start, track_end) in tracks.iter().copied() {
        let track_size = (track_end - track_start).max(0.0);
        let flow_offset = axis.flow_offset(track_start, track_size, node_block_size);
        let mut start = abs_start + flow_offset;
        let end = start + track_size;

        if let Some(prev_end) = prev_flow_end {
          if start > prev_end + BREAK_EPSILON {
            start = prev_end;
          }
        }
        prev_flow_end = Some(end);

        if track_size <= BREAK_EPSILON {
          continue;
        }

        if end > start + BREAK_EPSILON {
          candidates.push(AtomicCandidate {
            range: AtomicRange { start, end },
            required_fragmentainer_size: track_size,
          });
        }
      }
    }
  }

  if is_row_flex_container(style) {
    let node_writing_mode = style.writing_mode;
    if let Some(flex_lines) = collect_row_flex_lines(
      node,
      abs_start,
      axis,
      node_block_size,
      style,
      node_writing_mode,
      default_style(),
    ) {
      // Treat each flex line as indivisible in the fragmentation axis. Like grid track atomic
      // candidates, assign any inter-line gutter to the following line so we never produce a
      // fragmentainer that contains only the gap.
      //
      // Note: the "fits" decision is based on the *line size* (excluding the absorbed gutter).
      let mut prev_flow_end: Option<f32> = None;
      for line in flex_lines.lines.into_iter() {
        let line_size = (line.end - line.start).max(0.0);
        let mut start = line.start;
        let end = line.end;

        if let Some(prev_end) = prev_flow_end {
          if start > prev_end + BREAK_EPSILON {
            start = prev_end;
          }
        }
        prev_flow_end = Some(end);

        if line_size <= BREAK_EPSILON {
          continue;
        }

        if end > start + BREAK_EPSILON {
          candidates.push(AtomicCandidate {
            range: AtomicRange { start, end },
            required_fragmentainer_size: line_size,
          });
        }
      }
    }
  }
}

fn collect_atomic_candidates_with_axis(
  node: &FragmentNode,
  abs_start: f32,
  candidates: &mut Vec<AtomicCandidate>,
  axis: &FragmentAxis,
  parent_block_size: f32,
  context: FragmentationContext,
  inherited_writing_mode: WritingMode,
) {
  collect_atomic_candidate_for_node(
    node,
    abs_start,
    axis,
    parent_block_size,
    candidates,
    context,
  );

  let node_block_size = axis.block_size(&node.bounds);
  let node_writing_mode = node
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(inherited_writing_mode);

  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  if style.float.is_floating() {
    return;
  }
  let grid_items = if matches!(context, FragmentationContext::Page)
    && matches!(style.display, Display::Grid | Display::InlineGrid)
  {
    node.grid_fragmentation.as_ref()
  } else {
    None
  };

  for (idx, child) in node.children.iter().enumerate() {
    let skip_descendants = grid_items
      .and_then(|info| info.items.get(idx))
      .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    if skip_descendants {
      continue;
    }

    let child_writing_mode = child
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(node_writing_mode);
    let child_axis =
      axis_for_child_in_context(axis, context, node_writing_mode, child_writing_mode);
    let child_abs_start = child_axis.flow_range(abs_start, node_block_size, &child.bounds).0;
    collect_atomic_candidates_with_axis(
      child,
      child_abs_start,
      candidates,
      &child_axis,
      node_block_size,
      context,
      child_writing_mode,
    );
  }
}

fn collect_atomic_range_for_node(
  node: &FragmentNode,
  abs_start: f32,
  axis: &FragmentAxis,
  parent_block_size: f32,
  ranges: &mut Vec<AtomicRange>,
  context: FragmentationContext,
  fragmentainer_size: Option<f32>,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let start = abs_start;
  let mut end = abs_start + node_block_size;
  if end <= start + BREAK_EPSILON {
    return;
  }
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style());
  if style.float.is_floating() {
    // Floats are only atomic when they fit within a single fragmentainer. Otherwise they may split
    // across fragmentainers (CSS Break 3 §Parallel Fragmentation Flows).
    //
    // Use the logical bounding box so forced breaks modeled as blank insertion (or other overflow)
    // can expand the float's effective height.
    let parent_abs_flow_start = abs_start
      - axis.flow_offset(
        axis.block_start(&node.bounds),
        node_block_size,
        parent_block_size,
      );
    let bbox = node.logical_bounding_box();
    let bbox_block_size = axis.block_size(&bbox);
    let bbox_flow_start = parent_abs_flow_start
      + axis.flow_offset(axis.block_start(&bbox), bbox_block_size, parent_block_size);
    let bbox_flow_end = bbox_flow_start + bbox_block_size;
    if bbox_flow_end > end + BREAK_EPSILON {
      end = bbox_flow_end;
    }

    let height = end - start;
    if fragmentainer_size.is_some_and(|size| height <= size + BREAK_EPSILON) {
      ranges.push(AtomicRange { start, end });
    }
  }

  let height = end - start;
  let fits_fragmentainer = fragmentainer_size
    .map(|size| height <= size + BREAK_EPSILON)
    .unwrap_or(true);
  let is_table_row_like = matches!(
    style.display,
    Display::TableRow
      | Display::TableRowGroup
      | Display::TableHeaderGroup
      | Display::TableFooterGroup
  );
  let avoid_inside = avoids_break_inside(style.break_inside, context) || is_table_row_like;
  if avoid_inside && (fits_fragmentainer || !matches!(context, FragmentationContext::Page)) {
    ranges.push(AtomicRange { start, end });
  }

  if matches!(style.display, Display::Grid | Display::InlineGrid) {
    if let Some(grid_tracks) = node.grid_tracks.as_deref() {
      let tracks = grid_tracks_in_fragmentation_axis(grid_tracks, axis);

      // Treat each grid track as indivisible. Additionally, treat the inter-track gutter preceding
      // each track as part of the following track so pagination never splits a `row-gap`/`column-gap`
      // across fragmentainers (and avoids producing a fragmentainer that contains only the gap).
      let mut prev_flow_end: Option<f32> = None;
      for (track_start, track_end) in tracks.iter().copied() {
        let track_size = (track_end - track_start).max(0.0);
        let flow_offset = axis.flow_offset(track_start, track_size, node_block_size);
        let mut start = abs_start + flow_offset;
        let end = start + track_size;

        if let Some(prev_end) = prev_flow_end {
          if start > prev_end + BREAK_EPSILON {
            start = prev_end;
          }
        }
        prev_flow_end = Some(end);

        if track_size <= BREAK_EPSILON {
          continue;
        }

        if matches!(context, FragmentationContext::Page) {
          if let Some(fragmentainer_size) = fragmentainer_size {
            if track_size > fragmentainer_size + BREAK_EPSILON {
              continue;
            }
          }
        }

        if end > start + BREAK_EPSILON {
          ranges.push(AtomicRange { start, end });
        }
      }
    }
  }

  if is_row_flex_container(style) {
    let node_writing_mode = style.writing_mode;
    if let Some(flex_lines) = collect_row_flex_lines(
      node,
      abs_start,
      axis,
      node_block_size,
      style,
      node_writing_mode,
      default_style(),
    ) {
      // Treat each flex line as indivisible in the fragmentation axis. Like grid track atomic
      // ranges, assign any inter-line gutter to the following line so we never produce a
      // fragmentainer that contains only the gap.
      let mut prev_flow_end: Option<f32> = None;
      for line in flex_lines.lines.into_iter() {
        let line_size = (line.end - line.start).max(0.0);
        let mut start = line.start;
        let end = line.end;

        if let Some(prev_end) = prev_flow_end {
          if start > prev_end + BREAK_EPSILON {
            start = prev_end;
          }
        }
        prev_flow_end = Some(end);

        if line_size <= BREAK_EPSILON {
          continue;
        }

        if matches!(context, FragmentationContext::Page) {
          if let Some(fragmentainer_size) = fragmentainer_size {
            if line_size > fragmentainer_size + BREAK_EPSILON {
              continue;
            }
          }
        }

        if end > start + BREAK_EPSILON {
          ranges.push(AtomicRange { start, end });
        }
      }
    }
  }
}

pub(crate) fn collect_atomic_ranges(
  node: &FragmentNode,
  abs_start: f32,
  ranges: &mut Vec<AtomicRange>,
  context: FragmentationContext,
  fragmentainer_size: Option<f32>,
) {
  let axis = fragmentation_axis(node);
  let writing_mode = node
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  collect_atomic_ranges_with_axis(
    node,
    abs_start,
    ranges,
    &axis,
    axis.block_size(&node.bounds),
    context,
    fragmentainer_size,
    writing_mode,
  );
}

fn collect_atomic_ranges_with_axis(
  node: &FragmentNode,
  abs_start: f32,
  ranges: &mut Vec<AtomicRange>,
  axis: &FragmentAxis,
  parent_block_size: f32,
  context: FragmentationContext,
  fragmentainer_size: Option<f32>,
  inherited_writing_mode: WritingMode,
) {
  collect_atomic_range_for_node(
    node,
    abs_start,
    axis,
    parent_block_size,
    ranges,
    context,
    fragmentainer_size,
  );

  let node_block_size = axis.block_size(&node.bounds);
  let node_writing_mode = node
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(inherited_writing_mode);

  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  if style.float.is_floating() {
    return;
  }
  let grid_items = if matches!(context, FragmentationContext::Page)
    && matches!(style.display, Display::Grid | Display::InlineGrid)
  {
    node.grid_fragmentation.as_ref()
  } else {
    None
  };

  for (idx, child) in node.children.iter().enumerate() {
    let skip_descendants = grid_items
      .and_then(|info| info.items.get(idx))
      .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    if skip_descendants {
      continue;
    }

    let child_writing_mode = child
      .style
      .as_ref()
      .map(|s| s.writing_mode)
      .unwrap_or(node_writing_mode);
    let child_axis =
      axis_for_child_in_context(axis, context, node_writing_mode, child_writing_mode);
    let child_abs_start = child_axis
      .flow_range(abs_start, node_block_size, &child.bounds)
      .0;
    collect_atomic_ranges_with_axis(
      child,
      child_abs_start,
      ranges,
      &child_axis,
      node_block_size,
      context,
      fragmentainer_size,
      child_writing_mode,
    );
  }
}

pub(crate) fn collect_atomic_ranges_with_axes(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
  ranges: &mut Vec<AtomicRange>,
  context: FragmentationContext,
  fragmentainer_size: Option<f32>,
) {
  let axis = axis_from_fragment_axes(axes);
  let writing_mode = node
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  collect_atomic_ranges_with_axis(
    node,
    abs_start,
    ranges,
    &axis,
    axis.block_size(&node.bounds),
    context,
    fragmentainer_size,
    writing_mode,
  );
}

pub(crate) fn normalize_atomic_ranges(ranges: &mut Vec<AtomicRange>) {
  ranges.retain(|range| range.end > range.start + BREAK_EPSILON);
  ranges.sort_by(|a, b| {
    a.start
      .partial_cmp(&b.start)
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  let mut merged: Vec<AtomicRange> = Vec::with_capacity(ranges.len());
  for mut range in ranges.iter().copied() {
    if let Some(last) = merged.last_mut() {
      // Atomic ranges treat their endpoints as break-safe (see `atomic_containing`). Do not merge
      // ranges that only touch within epsilon, otherwise the shared endpoint becomes interior to
      // the merged interval and would incorrectly forbid breaks between adjacent atomic siblings
      // (including forced breaks).
      if range.start < last.end - BREAK_EPSILON {
        last.end = last.end.max(range.end);
        continue;
      }

      // Snap tiny overlaps/gaps so the remaining intervals stay non-overlapping while preserving
      // the break-safe boundary at the join.
      if (range.start - last.end).abs() <= BREAK_EPSILON {
        range.start = last.end;
        if range.end <= range.start + BREAK_EPSILON {
          continue;
        }
      }
    }
    merged.push(range);
  }

  ranges.clear();
  ranges.extend(merged);
}

fn split_atomic_ranges_at_forced_break_opportunities(
  atomic_ranges: &mut Vec<AtomicRange>,
  opportunities: &[BreakOpportunity],
) {
  if atomic_ranges.is_empty() {
    return;
  }

  let mut points: Vec<f32> = opportunities
    .iter()
    .filter(|o| matches!(o.strength, BreakStrength::Forced))
    .map(|o| o.pos)
    .collect();
  if points.is_empty() {
    return;
  }
  points.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  points.dedup_by(|a, b| (*a - *b).abs() < BREAK_EPSILON);

  // Atomic ranges treat their endpoints as break-safe (see `atomic_containing`). Split ranges at
  // forced break positions so the forced boundary becomes an endpoint and is therefore never
  // considered "inside atomic".
  let mut split: Vec<AtomicRange> = Vec::with_capacity(atomic_ranges.len());
  let mut point_idx = 0usize;
  for range in atomic_ranges.iter().copied() {
    let mut start = range.start;

    while point_idx < points.len() && points[point_idx] <= start + BREAK_EPSILON {
      point_idx += 1;
    }

    let mut local_idx = point_idx;
    while local_idx < points.len() {
      let pos = points[local_idx];
      if pos >= range.end - BREAK_EPSILON {
        break;
      }
      if pos <= start + BREAK_EPSILON {
        local_idx += 1;
        continue;
      }
      split.push(AtomicRange { start, end: pos });
      start = pos;
      local_idx += 1;
    }
    split.push(AtomicRange {
      start,
      end: range.end,
    });

    point_idx = local_idx;
  }

  atomic_ranges.clear();
  atomic_ranges.extend(split);
  normalize_atomic_ranges(atomic_ranges);
}

fn combine_breaks(
  after: BreakBetween,
  before: BreakBetween,
  context: FragmentationContext,
) -> BreakStrength {
  if forces_break_between(after, context) || forces_break_between(before, context) {
    return BreakStrength::Forced;
  }

  if avoids_break_between(after, context) || avoids_break_between(before, context) {
    return BreakStrength::Avoid;
  }

  BreakStrength::Auto
}

pub(crate) fn forces_break_between(value: BreakBetween, context: FragmentationContext) -> bool {
  match value {
    BreakBetween::Always => true,
    BreakBetween::Column => matches!(context, FragmentationContext::Column),
    BreakBetween::Page
    | BreakBetween::Left
    | BreakBetween::Right
    | BreakBetween::Recto
    | BreakBetween::Verso => matches!(context, FragmentationContext::Page),
    _ => false,
  }
}

pub(crate) fn avoids_break_between(value: BreakBetween, context: FragmentationContext) -> bool {
  match value {
    BreakBetween::Avoid => true,
    BreakBetween::AvoidPage => matches!(context, FragmentationContext::Page),
    BreakBetween::AvoidColumn => matches!(context, FragmentationContext::Column),
    _ => false,
  }
}

pub(crate) fn avoids_break_inside(value: BreakInside, context: FragmentationContext) -> bool {
  match value {
    BreakInside::Avoid => true,
    BreakInside::AvoidPage => matches!(context, FragmentationContext::Page),
    BreakInside::AvoidColumn => matches!(context, FragmentationContext::Column),
    _ => false,
  }
}

fn apply_avoid_penalty(strength: BreakStrength, inside_avoid: bool) -> BreakStrength {
  if inside_avoid && !matches!(strength, BreakStrength::Forced) {
    BreakStrength::Avoid
  } else {
    strength
  }
}

fn default_style() -> &'static ComputedStyle {
  static DEFAULT: OnceLock<ComputedStyle> = OnceLock::new();
  DEFAULT.get_or_init(ComputedStyle::default)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::layout::axis::FragmentAxes;
  use crate::tree::fragment_tree::{GridFragmentationInfo, GridTrackRanges};
  use std::sync::Arc;
  use std::time::{Duration, Instant};

  fn default_axes() -> FragmentAxes {
    FragmentAxes::from_writing_mode_and_direction(WritingMode::HorizontalTb, Direction::Ltr)
  }

  #[test]
  fn massive_opportunities_remains_fast() {
    let line_height = 1.0;
    let line_count = 10_000;
    let mut lines = Vec::with_capacity(line_count);
    for i in 0..line_count {
      let y = i as f32 * line_height;
      lines.push(FragmentNode::new_line(
        Rect::from_xywh(0.0, y, 100.0, line_height),
        line_height * 0.8,
        Vec::new(),
      ));
    }
    let total_height = line_count as f32 * line_height;
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, total_height), lines);
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(50.0),
    );
    let total_extent = analyzer.content_extent().max(50.0);
    let start = Instant::now();
    let boundaries = analyzer.boundaries(50.0, total_extent).unwrap();
    let elapsed = start.elapsed();
    let expected = (total_extent / 50.0).ceil() as usize + 1;
    assert_eq!(boundaries.len(), expected);
    assert!(
      elapsed < Duration::from_millis(500),
      "expected linear boundary resolution, took {elapsed:?}"
    );
  }

  #[test]
  fn atomic_ranges_are_not_split() {
    let mut avoid = ComputedStyle::default();
    avoid.break_inside = BreakInside::Avoid;
    let atomic = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 30.0),
      vec![],
      Arc::new(avoid),
    );
    let trailing = FragmentNode::new_block(Rect::from_xywh(0.0, 30.0, 100.0, 70.0), vec![]);
    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![atomic, trailing],
    );
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(40.0),
    );
    let total_extent = analyzer.content_extent().max(40.0);
    let boundaries = analyzer.boundaries(40.0, total_extent).unwrap();
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      first_break >= 30.0 - BREAK_EPSILON,
      "first break should land after the atomic range, got {first_break}"
    );
    assert!(
      boundaries
        .iter()
        .all(|b| *b <= 0.0 + BREAK_EPSILON || *b >= 30.0 - BREAK_EPSILON),
      "no boundary should fall inside the atomic range: {boundaries:?}"
    );
  }

  #[test]
  fn grid_track_rows_are_not_split_when_they_fit_fragmentainer() {
    let leading = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 15.0), vec![]);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 15.0, 100.0, 30.0),
      vec![
        FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 25.0), vec![]),
        FragmentNode::new_block(Rect::from_xywh(0.0, 25.0, 100.0, 5.0), vec![]),
      ],
      Arc::new(grid_style),
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 30.0)],
      columns: Vec::new(),
    }));

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 45.0), vec![leading, grid]);
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(40.0),
    );
    let total_extent = analyzer.content_extent().max(40.0);
    let boundaries = analyzer.boundaries(40.0, total_extent).unwrap();
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);

    assert!(
      (first_break - 15.0).abs() < BREAK_EPSILON,
      "expected break before the grid row band, got {first_break} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn grid_forced_breaks_align_to_track_boundaries_in_block_negative_writing_mode() {
    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    grid_style.writing_mode = WritingMode::VerticalRl;
    let grid_style = Arc::new(grid_style);

    let mut first_style = ComputedStyle::default();
    first_style.break_after = BreakBetween::Page;
    first_style.writing_mode = WritingMode::VerticalRl;
    let first_style = Arc::new(first_style);

    let mut first =
      FragmentNode::new_block_styled(Rect::from_xywh(40.0, 0.0, 30.0, 20.0), vec![], first_style);
    // Ensure the metadata is wired consistently for the propagation logic.
    first.content = FragmentContent::Block { box_id: Some(1) };

    let mut second = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 30.0, 20.0),
      vec![],
      Arc::new({
        let mut style = ComputedStyle::default();
        style.writing_mode = WritingMode::VerticalRl;
        style
      }),
    );
    second.content = FragmentContent::Block { box_id: Some(2) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 70.0, 20.0),
      vec![first, second],
      grid_style,
    );
    // Two 30px columns with a 10px gap between them in physical space. In `writing-mode: vertical-rl`
    // the block axis is horizontal and negative, so the first column appears on the right.
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: Vec::new(),
      columns: vec![(40.0, 70.0), (0.0, 30.0)],
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![
        GridItemFragmentationData {
          box_id: 1,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 2,
        },
        GridItemFragmentationData {
          box_id: 2,
          row_start: 1,
          row_end: 2,
          column_start: 2,
          column_end: 3,
        },
      ],
    }));

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 70.0, 20.0), vec![grid]);

    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr),
      true,
      Some(100.0),
    );
    let total_extent = analyzer.content_extent().max(100.0);
    let boundaries = analyzer.boundaries(100.0, total_extent).unwrap();

    assert_eq!(
      boundaries
        .iter()
        .copied()
        .filter(|b| *b > BREAK_EPSILON && (*b - 30.0).abs() < BREAK_EPSILON)
        .count(),
      1,
      "expected the forced break to align to the end edge of the first column track (flow pos 30), got {boundaries:?}"
    );
  }

  #[test]
  fn grid_gutters_are_not_split_in_block_negative_writing_mode() {
    // Two 30px tracks separated by a 10px gap. Use a fragmentainer size that would land 5px into
    // the gap if we didn't treat the gutter as belonging to the following track.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    grid_style.writing_mode = WritingMode::VerticalRl;
    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 70.0, 20.0),
      vec![
        FragmentNode::new_block(Rect::from_xywh(40.0, 0.0, 30.0, 20.0), vec![]),
        FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 30.0, 20.0), vec![]),
      ],
      Arc::new(grid_style),
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: Vec::new(),
      columns: vec![(40.0, 70.0), (0.0, 30.0)],
    }));

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 70.0, 20.0), vec![grid]);
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr),
      true,
      Some(35.0),
    );
    let total_extent = analyzer.content_extent().max(35.0);
    let boundaries = analyzer.boundaries(35.0, total_extent).unwrap();

    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_break - 30.0).abs() < BREAK_EPSILON,
      "expected break at the start of the 10px column gap (flow pos 30), got {first_break} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn avoid_inside_blocks_move_to_next_fragment_when_boundary_splits() {
    let mut avoid = ComputedStyle::default();
    avoid.break_inside = BreakInside::AvoidColumn;
    let avoid = Arc::new(avoid);

    let leading = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 80.0), vec![]);
    let atomic =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 80.0, 100.0, 30.0), vec![], avoid);

    // Root is intentionally taller than the content so we can clip two full fragmentainers.
    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 200.0),
      vec![leading, atomic],
    );
    let axis = fragmentation_axis(&root);
    let root_block_size = axis.block_size(&root.bounds);

    let fragment1 = clip_node(
      &root,
      &axis,
      0.0,
      100.0,
      0.0,
      0.0,
      100.0,
      root_block_size,
      0,
      2,
      FragmentationContext::Column,
      100.0,
      default_axes(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
      fragment1.children.len(),
      1,
      "avoid-inside child should not be included in the fragment that ends mid-node"
    );

    let fragment2 = clip_node(
      &root,
      &axis,
      100.0,
      200.0,
      0.0,
      100.0,
      200.0,
      root_block_size,
      1,
      2,
      FragmentationContext::Column,
      100.0,
      default_axes(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
      fragment2.children.len(),
      1,
      "avoid-inside child should be moved to the fragment that starts inside it"
    );
    let moved = &fragment2.children[0];
    assert!(
      (moved.bounds.y() - 0.0).abs() < 0.01,
      "moved node should start at the top of the fragment, got y={}",
      moved.bounds.y()
    );
    assert!(
      (moved.bounds.height() - 30.0).abs() < 0.01,
      "moved node should retain its full block size, got h={}",
      moved.bounds.height()
    );
  }
}
