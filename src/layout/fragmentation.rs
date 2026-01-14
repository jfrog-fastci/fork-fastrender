//! Fragmentation utilities
//!
//! Pagination and multi-column output require splitting a laid-out fragment tree
//! into fragmentainers (pages/columns). Fragmentation happens in the block axis
//! and respects authored break hints (`break-before/after/inside`), widows/orphans
//! constraints, and line-level break opportunities. The fragment tree that comes
//! out of layout is treated as flow order; this module decides where to break and
//! clones the appropriate fragment subtrees for each fragmentainer.

use std::sync::{Arc, OnceLock};

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
  CollapsedBorderSegment, FragmentChildren, FragmentContent, FragmentNode, FragmentSliceInfo,
  GridItemFragmentationData, GridTrackRanges, TableCollapsedBorders,
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
}

#[derive(Debug, Clone)]
struct BreakOpportunity {
  /// Inclusive block-axis start of the break range.
  ///
  /// Most break opportunities are points (`pos == end`). Between-sibling break
  /// opportunities can span an entire gap (e.g. margin collapses), in which case
  /// `pos` is the start edge of the gap and `end` is the end edge.
  pos: f32,
  /// Inclusive block-axis end of the break range.
  end: f32,
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
  /// Whether this atomic range corresponds to a float's parallel fragmentation flow.
  ///
  /// Floats are treated as atomic *only when they fit* within the fragmentainer. When they do not
  /// fit, we still want to fragment them to make progress, even if there are no explicit break
  /// opportunities inside the float. Tracking float candidates lets the boundary selection logic
  /// distinguish "no break opportunities" from "must slice through a float".
  is_float: bool,
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
  item_abs_start: f32,
  context: FragmentationContext,
) -> f32 {
  let axis = axis_from_fragment_axes(axes);
  let item_block_size = axis.block_size(&item.bounds).max(0.0);
  if item_block_size <= BREAK_EPSILON {
    return item_block_size;
  }

  // Parallel-flow forced breaks are modeled by shifting descendants so that continuation content
  // lands on later fragmentainers (see `apply_grid_parallel_flow_forced_break_shifts`). After those
  // shifts, the grid item's logical bounding box captures the effective block-size increase.
  //
  // Prefer that geometry-derived extent here so any descendant overflow is reflected when
  // determining which fragmentainers this item overlaps.
  let bbox_block_size = axis.block_size(&item.logical_bounding_box());
  let mut required = if bbox_block_size.is_finite() {
    bbox_block_size.max(item_block_size)
  } else {
    item_block_size
  };

  // Backwards-compatibility fallback: if no descendant overflow is visible but a fragmentainer size
  // is available, still model forced breaks as inserting blank space. This keeps fragmentation
  // robust even when callers invoke clipping without applying shift modelling first.
  if required <= item_block_size + BREAK_EPSILON
    && fragmentainer_size.is_finite()
    && fragmentainer_size > 0.0
  {
    let item_abs_end = item_abs_start + item_block_size;
    let mut positions: Vec<f32> = match context {
      FragmentationContext::Page => collect_forced_boundaries_with_axes(item, item_abs_start, axes)
        .into_iter()
        .map(|b| b.position)
        .collect(),
      FragmentationContext::Column => {
        let default_style = default_style();
        let item_writing_mode = item
          .style
          .as_ref()
          .map(|s| s.writing_mode)
          .unwrap_or(default_style.writing_mode);
        let mut collection = BreakCollection::default();
        collect_break_opportunities(
          item,
          item_abs_start,
          &mut collection,
          0,
          0,
          context,
          &axis,
          item_writing_mode,
          true,
          false,
          false,
        );
        collection
          .opportunities
          .into_iter()
          .filter(|o| matches!(o.strength, BreakStrength::Forced))
          .map(|o| o.pos)
          .collect()
      }
    };

    positions.retain(|p| *p > item_abs_start + BREAK_EPSILON && *p < item_abs_end - BREAK_EPSILON);
    if let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size) {
      let shift = shifts.shift_for(item_abs_end);
      required = required.max(item_block_size + shift);
    }
  }

  required
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
  context: FragmentationContext,
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
    context: FragmentationContext,
    default_style: &ComputedStyle,
  ) {
    let style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let node_writing_mode = style.writing_mode;
    let node_block_size = axis.block_size(&node.bounds);

    if matches!(style.display, Display::Grid | Display::InlineGrid) {
      if let Some(grid_info) = node.grid_fragmentation.clone() {
        let in_flow_count = grid_info.items.len().min(node.children.len());
        for idx in 0..in_flow_count {
          let Some(child) = node.children_mut().get_mut(idx) else {
            continue;
          };

          let child_block_size = axis.block_size(&child.bounds);
          if child_block_size <= BREAK_EPSILON {
            continue;
          }
          let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
          let child_abs_end = child_abs_start + child_block_size;

          // Discover forced breaks inside this grid item and model them as inserting blank space up
          // to the next fragmentainer boundary (CSS Grid 2 §Fragmenting Grid Layout).
          //
          // Use `collect_break_opportunities` so we respect the current fragmentation context (page
          // vs. column) and suppress nested parallel flows like floats.
          let child_writing_mode = child
            .style
            .as_ref()
            .map(|s| s.writing_mode)
            .unwrap_or(node_writing_mode);
          let mut collection = BreakCollection::default();
          collect_break_opportunities(
            child,
            child_abs_start,
            &mut collection,
            0,
            0,
            context,
            axis,
            child_writing_mode,
            true,
            false,
            false,
          );
          let mut positions: Vec<f32> = collection
            .opportunities
            .into_iter()
            .filter(|o| matches!(o.strength, BreakStrength::Forced))
            .map(|o| o.pos)
            .collect();
          positions
            .retain(|p| *p > child_abs_start + BREAK_EPSILON && *p < child_abs_end - BREAK_EPSILON);
          let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size)
          else {
            continue;
          };

          apply_parallel_flow_shifts_to_descendants(child, child_abs_start, 0.0, axis, &shifts);
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
        context,
        default_style,
      );
    }
  }

  walk(
    root,
    0.0,
    &axis,
    axes,
    fragmentainer_size,
    context,
    default_style,
  );
}

pub(crate) fn apply_flex_parallel_flow_forced_break_shifts(
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

  fn walk(
    node: &mut FragmentNode,
    abs_start: f32,
    axis: &FragmentAxis,
    axes: FragmentAxes,
    fragmentainer_size: f32,
    context: FragmentationContext,
    default_style: &ComputedStyle,
  ) {
    let style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let node_writing_mode = style.writing_mode;
    let node_block_size = axis.block_size(&node.bounds);

    if is_row_flex_container(style) {
      for child in node.children_mut().iter_mut() {
        let child_style = child
          .style
          .as_ref()
          .map(|s| s.as_ref())
          .unwrap_or(default_style);
        if !is_in_flow_flex_child(&child.content, child_style) {
          continue;
        }

        let child_block_size = axis.block_size(&child.bounds);
        if child_block_size <= BREAK_EPSILON {
          continue;
        }
        let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
        let child_abs_end = child_abs_start + child_block_size;

        let mut positions: Vec<f32> = match context {
          FragmentationContext::Page => {
            collect_forced_boundaries_for_pagination_with_axes(child, child_abs_start, axes)
              .into_iter()
              .map(|b| b.position)
              .collect()
          }
          FragmentationContext::Column => {
            let child_writing_mode = child
              .style
              .as_ref()
              .map(|s| s.writing_mode)
              .unwrap_or(node_writing_mode);
            let mut collection = BreakCollection::default();
            collect_break_opportunities(
              child,
              child_abs_start,
              &mut collection,
              0,
              0,
              context,
              axis,
              child_writing_mode,
              true,
              false,
              false,
            );
            collection
              .opportunities
              .into_iter()
              .filter(|o| matches!(o.strength, BreakStrength::Forced))
              .map(|o| o.pos)
              .collect()
          }
        };
        positions
          .retain(|p| *p > child_abs_start + BREAK_EPSILON && *p < child_abs_end - BREAK_EPSILON);
        if let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size)
        {
          apply_parallel_flow_shifts_to_descendants(child, child_abs_start, 0.0, axis, &shifts);
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
        context,
        default_style,
      );
    }
  }

  walk(
    root,
    0.0,
    &axis,
    axes,
    fragmentainer_size,
    context,
    default_style,
  );
}

pub(crate) fn apply_table_cell_parallel_flow_forced_break_shifts(
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

    if matches!(style.display, Display::TableCell) {
      // CSS Break 3 §3: forced breaks inside table cells establish a parallel fragmentation flow.
      // They must not force breaks in the surrounding table row (or sibling cells); instead
      // continuation content inside the cell is shifted to the next fragmentainer boundary.
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
        true,
        false,
        true,
      );

      let cell_end = abs_start + node_block_size;
      let mut positions: Vec<f32> = collection
        .opportunities
        .into_iter()
        .filter(|o| matches!(o.strength, BreakStrength::Forced))
        .map(|o| o.pos)
        .collect();
      positions.retain(|p| *p > abs_start + BREAK_EPSILON && *p < cell_end - BREAK_EPSILON);
      if let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size) {
        apply_parallel_flow_shifts_to_descendants(node, abs_start, 0.0, axis, &shifts);
      }
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
        false,
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

pub(crate) fn apply_abspos_parallel_flow_forced_break_shifts(
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

    if style.position.is_absolutely_positioned() {
      // CSS Break 3 §3: absolutely-positioned elements establish a parallel fragmentation flow.
      // Forced breaks inside them must not force breaks in the main flow; instead, continuation
      // content is shifted to the next fragmentainer boundary.
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
        false,
        false,
      );

      let abs_end = abs_start + node_block_size;
      let mut positions: Vec<f32> = collection
        .opportunities
        .into_iter()
        .filter(|o| matches!(o.strength, BreakStrength::Forced))
        .map(|o| o.pos)
        .collect();
      positions.retain(|p| *p > abs_start + BREAK_EPSILON && *p < abs_end - BREAK_EPSILON);
      if let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size) {
        apply_parallel_flow_shifts_to_descendants(node, abs_start, 0.0, axis, &shifts);
      }

      // Like floats, avoid applying shifts multiple times within this subtree.
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
  abs_start: f32,
  axis: &FragmentAxis,
  axes: FragmentAxes,
  fragmentainer_size: f32,
  context: FragmentationContext,
) -> Option<f32> {
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
    let child_abs_start = abs_start + child_start;
    let child_required = grid_item_parallel_flow_required_block_size(
      child,
      axes,
      fragmentainer_size,
      child_abs_start,
      context,
    );
    required = required.max(child_start + child_required);
  }

  if required > node_block_size + BREAK_EPSILON {
    Some(required)
  } else {
    None
  }
}

fn abspos_parallel_flow_required_block_size(
  node: &FragmentNode,
  abs_start: f32,
  axis: &FragmentAxis,
  fragmentainer_size: f32,
  context: FragmentationContext,
) -> Option<f32> {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return None;
  }

  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  if !style.position.is_absolutely_positioned() {
    return None;
  }

  let node_block_size = axis.block_size(&node.bounds).max(0.0);
  if node_block_size <= BREAK_EPSILON {
    return None;
  }

  // CSS Break 3 §3: Absolutely-positioned elements establish a parallel fragmentation flow. Model
  // forced breaks inside by inserting blank space up to the next fragmentainer boundary so
  // continuation content overlaps later fragmentainers without forcing breaks in the main flow.
  let node_writing_mode = node
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(default_style.writing_mode);
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
    false,
    false,
  );

  let abs_end = abs_start + node_block_size;
  let mut positions: Vec<f32> = collection
    .opportunities
    .into_iter()
    .filter(|o| matches!(o.strength, BreakStrength::Forced))
    .map(|o| o.pos)
    .collect();
  positions.retain(|p| *p > abs_start + BREAK_EPSILON && *p < abs_end - BREAK_EPSILON);

  let Some(shifts) = ParallelFlowShiftMap::for_forced_breaks(positions, fragmentainer_size) else {
    return None;
  };

  let required = node_block_size + shifts.shift_for(abs_end);
  if required > node_block_size + BREAK_EPSILON {
    Some(required)
  } else {
    None
  }
}

/// Computes the total block-axis extent of a fragment tree, accounting for parallel fragmentation
/// flows (e.g. grid items in a row and absolutely-positioned subtrees).
pub(crate) fn parallel_flow_content_extent(
  root: &FragmentNode,
  axes: FragmentAxes,
  fragmentainer_size_hint: Option<f32>,
  context: FragmentationContext,
) -> f32 {
  let axis = axis_from_fragment_axes(axes);
  let mut extent = match context {
    FragmentationContext::Column => {
      // In multi-column layout the synthetic flow root can be enlarged to satisfy a definite
      // block-size on the multicol container (e.g. `height`). That trailing empty space must not
      // participate in column balancing: spanners (`column-span: all`) and `column-fill: balance`
      // depend on the *content* extent, not the container's fixed height.
      //
      // Use the union of descendants (plus the origin) rather than including the root bounds to
      // ignore that inflated trailing space. However, descendant bounds do not include trailing
      // margins, so also consider the fragment's original unfragmented block-size (`slice_info`)
      // which is set before any synthetic enlargement.
      let mut bbox = Rect::from_xywh(0.0, 0.0, 0.0, 0.0);
      for child in root.children.iter() {
        bbox = bbox.union(child.logical_bounding_box());
      }
      let mut bbox_extent = axis.block_size(&bbox);
      let bounds_extent = axis.block_size(&root.bounds);
      let mut original_extent = root.slice_info.original_block_size;

      // When balancing columns inside a clipped fragment (e.g. when paginating a multi-column
      // container), the slice metadata still reflects the original unfragmented block-size.
      // Clamping prevents a "full height" original size from forcing additional columns within the
      // clipped fragmentainer.
      //
      // Descendant logical bounding boxes can also extend beyond the clipped fragment bounds due to
      // visual overflow (e.g. text that overflows a fixed-height box). Column fragmentainers clip
      // their contents; allowing that overflow to inflate the computed extent causes spurious extra
      // columns and (in paged multicol) unstable pagination.
      if original_extent.is_finite()
        && original_extent > 0.0
        && bounds_extent.is_finite()
        && bounds_extent > 0.0
      {
        original_extent = original_extent.min(bounds_extent);
      } else if !original_extent.is_finite() || original_extent < 0.0 {
        original_extent = 0.0;
      }

      if bbox_extent.is_finite() && bbox_extent > 0.0 && bounds_extent.is_finite() && bounds_extent > 0.0 {
        bbox_extent = bbox_extent.min(bounds_extent);
      } else if !bbox_extent.is_finite() || bbox_extent < 0.0 {
        bbox_extent = 0.0;
      }

      bbox_extent.max(original_extent)
    }
    FragmentationContext::Page => {
      // Fragment roots are sometimes given a synthetic block-size (e.g. to satisfy a definite
      // fragmentainer size hint). When that size is non-finite, it can poison pagination by making
      // the computed content extent infinite, causing boundary generation to allocate until OOM.
      //
      // Prefer the logical bounding box (which includes the root) when finite, but fall back to the
      // union of descendants when the root bounds are non-finite.
      //
      // Additionally, account for *physical* overflow that may extend beyond the logical flow
      // extents. This is important when layout rewrites logical positions (e.g. paged multicol)
      // while leaving descendant bounds in physical space: visible overflow can otherwise be clipped
      // away at the end of the document.
      let bbox = root.logical_bounding_box();
      let mut extent = axis.block_size(&bbox);
      if !extent.is_finite() || extent < 0.0 {
        let mut child_bbox = Rect::from_xywh(0.0, 0.0, 0.0, 0.0);
        for child in root.children.iter() {
          child_bbox = child_bbox.union(child.logical_bounding_box());
        }
        extent = axis.block_size(&child_bbox);
      }
      if extent.is_finite() && extent >= 0.0 {
        // `Rect::bounding_box` includes descendant overflow in physical coordinates. Only include
        // overflow that extends in the *flow direction* (so content that paints before the start
        // edge doesn't create extra trailing pages).
        let root_block_size = axis.block_size(&root.bounds);
        let flow_overflow = if root_block_size.is_finite() && root_block_size > 0.0 {
          let physical_bbox = root.bounding_box();
          let bbox_block_start = axis.block_start(&physical_bbox);
          let bbox_block_end = bbox_block_start + axis.block_size(&physical_bbox);
          let root_block_start = axis.block_start(&root.bounds);
          let root_block_end = root_block_start + root_block_size;
          let overflow_extent = if axis.block_positive {
            bbox_block_end - root_block_start
          } else {
            root_block_end - bbox_block_start
          };
          if overflow_extent.is_finite() && overflow_extent > 0.0 {
            overflow_extent
          } else {
            0.0
          }
        } else {
          0.0
        };
        extent.max(flow_overflow)
      } else {
        0.0
      }
    }
  };

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
    context: FragmentationContext,
    extent: &mut f32,
  ) {
    let node_block_size = axis.block_size(&node.bounds);
    if let Some(required) = grid_container_parallel_flow_required_block_size(
      node,
      abs_start,
      axis,
      axes,
      fragmentainer_size,
      context,
    ) {
      *extent = extent.max(abs_start + required);
    }
    if let Some(required) =
      abspos_parallel_flow_required_block_size(node, abs_start, axis, fragmentainer_size, context)
    {
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
        context,
        extent,
      );
    }
  }

  walk(
    root,
    0.0,
    &axis,
    axes,
    fragmentainer_size,
    context,
    &mut extent,
  );
  extent
}

#[derive(Debug)]
pub struct FragmentationAnalyzer {
  _axis: FragmentAxis,
  context: FragmentationContext,
  enforce_fragmentainer_size: bool,
  allow_early_sibling_breaks: bool,
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

fn is_table_row_like(display: Display) -> bool {
  matches!(
    display,
    Display::TableRow | Display::TableHeaderGroup | Display::TableFooterGroup
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

  // Infer line breaks from the main-axis positions produced by flex layout.
  //
  // Flex items in a line are laid out in (visual) order with non-decreasing main-axis starts. When
  // wrapping, the first item of the next line resets toward the start edge of the container. Track
  // the *end* of the previous item rather than just its start so we detect wraps even when both
  // lines begin at the same main start (e.g. when each line contains a single full-width item).
  let mut prev_main_end: Option<f32> = None;
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
    let mut main_end = main_start + child_inline_size;
    if !main_end.is_finite() {
      main_end = main_start;
    }

    if let Some(prev_end) = prev_main_end {
      if main_start + FLEX_LINE_WRAP_EPSILON < prev_end {
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
    prev_main_end = Some(main_end);

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
      false,
      false,
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
      false,
    );

    collection.opportunities.sort_by(|a, b| {
      a.pos
        .partial_cmp(&b.pos)
        .unwrap_or(std::cmp::Ordering::Equal)
    });
    collection.opportunities.dedup_by(|a, b| {
      (a.pos - b.pos).abs() < BREAK_EPSILON
        && (a.end - b.end).abs() < BREAK_EPSILON
        && a.kind == b.kind
        && a.strength == b.strength
    });

    let content_extent = parallel_flow_content_extent(root, axes, fragmentainer_size_hint, context);
    let table_repetitions = collect_table_repetition_info_with_axis(root, 0.0, &axis, context);
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
      allow_early_sibling_breaks: false,
    }
  }

  /// When enabled, pagination will honour between-sibling break opportunities even when they fall
  /// noticeably before the fragmentainer limit.
  ///
  /// The default behaviour (disabled) prefers the fragmentainer limit for such early sibling
  /// boundaries to avoid perturbing the flow→fragment coordinate mapping used by the generic
  /// `fragment_tree` pagination path. Consumers that perform their own coordinate mapping (e.g.
  /// `layout::pagination` for @page-aware pagination) should enable this so the break selection
  /// matches the CSS Break "find a break before the overflowing sibling" behaviour.
  pub fn set_allow_early_sibling_breaks(&mut self, allow: bool) {
    self.allow_early_sibling_breaks = allow;
  }

  /// Returns true when `pos` matches a forced break opportunity.
  pub fn is_forced_break_at(&self, pos: f32) -> bool {
    if !pos.is_finite() {
      return false;
    }
    self
      .opportunities
      .iter()
      .any(|o| matches!(o.strength, BreakStrength::Forced) && (o.pos - pos).abs() < BREAK_EPSILON)
  }

  /// Adds additional forced break positions.
  ///
  /// This is useful for pagination consumers that need to introduce mandatory boundaries that are
  /// not expressed via `break-before/after` (e.g. CSS Paged Media named-page transitions).
  pub fn add_forced_break_positions<I>(&mut self, positions: I)
  where
    I: IntoIterator<Item = f32>,
  {
    let added: Vec<f32> = positions.into_iter().filter(|p| p.is_finite()).collect();
    if added.is_empty() {
      return;
    }

    for pos in &added {
      self.opportunities.push(BreakOpportunity {
        pos: *pos,
        end: *pos,
        strength: BreakStrength::Forced,
        kind: BreakKind::BetweenSiblings,
      });
    }

    self.opportunities.sort_by(|a, b| {
      a.pos
        .partial_cmp(&b.pos)
        .unwrap_or(std::cmp::Ordering::Equal)
    });
    self.opportunities.dedup_by(|a, b| {
      (a.pos - b.pos).abs() < BREAK_EPSILON
        && (a.end - b.end).abs() < BREAK_EPSILON
        && a.kind == b.kind
        && a.strength == b.strength
    });

    // Atomic ranges are derived from the candidate set per fragmentainer size and will be split at
    // these forced opportunities by `atomic_ranges_for(..)`.
  }

  /// Updates internal line-container state as if all breaks up to `start` have already been
  /// consumed.
  pub fn seek(&mut self, start: f32) {
    for container in &self.line_containers {
      if let Some(slot) = self.line_starts.get_mut(container.id) {
        let advanced = container
          .line_ends
          .partition_point(|end| *end <= start + BREAK_EPSILON);
        *slot = advanced.min(container.line_ends.len());
      }
    }
  }

  /// Selects the next fragmentation boundary starting at `start`.
  ///
  /// The returned boundary is expressed in the same block-axis coordinate system as the collected
  /// break opportunities.
  pub fn next_boundary(
    &mut self,
    start: f32,
    fragmentainer_size: f32,
    total_extent: f32,
  ) -> Result<f32, LayoutError> {
    if self.deadline_counter % 8 == 0 {
      check_layout_deadline()?;
    }
    self.deadline_counter = self.deadline_counter.wrapping_add(1);

    let effective_total = total_extent.max(self.content_extent);
    if fragmentainer_size <= 0.0 {
      self.seek(start);
      return Ok(effective_total);
    }

    self.seek(start);
    let mut cursor = self
      .opportunities
      .partition_point(|o| o.pos <= start + BREAK_EPSILON);
    Ok(self.select_next_boundary(
      start,
      fragmentainer_size,
      effective_total,
      &mut cursor,
      &self.atomic_ranges_for(fragmentainer_size),
    ))
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
    self.boundaries_with_effective_total(fragmentainer_size, effective_total)
  }

  /// Like [`Self::boundaries`], but treats `total_extent` as authoritative instead of always
  /// expanding it to `max(total_extent, self.content_extent)`.
  ///
  /// This is useful when callers intentionally want to clamp the analyzed extent (e.g. when
  /// fragmenting a clipped subtree where tiny visual overflow beyond the clipped range should not
  /// create additional fragments).
  pub fn boundaries_clamped_total(
    &mut self,
    fragmentainer_size: f32,
    total_extent: f32,
  ) -> Result<Vec<f32>, LayoutError> {
    self.boundaries_with_effective_total(fragmentainer_size, total_extent)
  }

  fn boundaries_with_effective_total(
    &mut self,
    fragmentainer_size: f32,
    effective_total: f32,
  ) -> Result<Vec<f32>, LayoutError> {
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

  /// Selects the next fragmentation boundary after `start`, using `fragmentainer_size` as the
  /// current fragmentainer limit.
  ///
  /// This is a lower-level API than [`Self::boundaries`] intended for callers that need to
  /// incrementally fragment content while varying fragmentainer sizes (e.g. footnote areas whose
  /// available block-size can change page-to-page).
  ///
  /// Callers must maintain `opportunity_cursor` and feed back the returned boundary as the next
  /// `start` to ensure widows/orphans bookkeeping remains consistent.
  pub fn next_boundary_with_cursor(
    &mut self,
    start: f32,
    fragmentainer_size: f32,
    total_extent: f32,
    opportunity_cursor: &mut usize,
  ) -> Result<f32, LayoutError> {
    if self.deadline_counter % 8 == 0 {
      check_layout_deadline()?;
    }
    self.deadline_counter = self.deadline_counter.wrapping_add(1);

    let effective_total = total_extent.max(self.content_extent);
    let atomic = self.atomic_ranges_for(fragmentainer_size);
    Ok(self.select_next_boundary(
      start,
      fragmentainer_size,
      effective_total,
      opportunity_cursor,
      &atomic,
    ))
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

      let saved_line_starts = self.line_starts.clone();
      let saved_deadline_counter = self.deadline_counter;
      let saved_opportunity_cursor = opportunity_cursor;

      let next = self.select_next_boundary(
        start,
        fragmentainer_size,
        effective_total,
        &mut opportunity_cursor,
        &atomic,
      );
      let default_line_starts = self.line_starts.clone();
      let default_deadline_counter = self.deadline_counter;
      let default_opportunity_cursor = opportunity_cursor;

      // The computed `fragmentainer_size` targets the ideal balanced height, but the chosen break
      // opportunity can still fall *before* `min_boundary` if there is no legal break between
      // `min_boundary` and the ideal limit. That would leave more remaining content than can fit in
      // the remaining fragmentainers, causing the final fragment to overflow even when a valid
      // boundary exists within the physical max fragmentainer size.
      //
      // If this happens, retry with the physical max fragmentainer size to force a later boundary
      // when possible.
      let next = if next + BREAK_EPSILON < min_boundary
        && max_fragmentainer_size > fragmentainer_size + BREAK_EPSILON
      {
        self.line_starts = saved_line_starts;
        self.deadline_counter = saved_deadline_counter;
        opportunity_cursor = saved_opportunity_cursor;

        let retry = self.select_next_boundary(
          start,
          max_fragmentainer_size,
          effective_total,
          &mut opportunity_cursor,
          &atomic,
        );
        if retry + BREAK_EPSILON >= min_boundary {
          retry
        } else {
          self.line_starts = default_line_starts;
          self.deadline_counter = default_deadline_counter;
          opportunity_cursor = default_opportunity_cursor;
          next
        }
      } else {
        next
      };
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

  fn limit_inside_breakable_float(&self, limit: f32, fragmentainer_size: f32) -> bool {
    if !(limit.is_finite()
      && fragmentainer_size.is_finite()
      && fragmentainer_size > 0.0
      && !self.atomic_candidates.is_empty())
    {
      return false;
    }

    self.atomic_candidates.iter().any(|candidate| {
      if !candidate.is_float {
        return false;
      }
      let required = candidate.required_fragmentainer_size.max(0.0);
      if !(required.is_finite() && required > fragmentainer_size + BREAK_EPSILON) {
        return false;
      }
      limit > candidate.range.start + BREAK_EPSILON && limit < candidate.range.end - BREAK_EPSILON
    })
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
    if let Some(range) = atomic_containing_for_fragmentainer(start, fragmentainer, atomic) {
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
        let max_with_footer =
          (fragmentainer - header_overhead - footer_overhead).max(BREAK_EPSILON);
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
    let mut limit_clamped_to_atomic_start = false;
    if let Some(range) = atomic_containing_for_fragmentainer(limit, fragmentainer, atomic) {
      if range.start > start + BREAK_EPSILON {
        if range.start + BREAK_EPSILON < limit {
          limit_clamped_to_atomic_start = true;
        }
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

    if let Some(pos) = self.forced_in_window(
      start,
      limit,
      total_extent,
      window.clone(),
      fragmentainer,
      atomic,
    ) {
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
      // Break opportunities can span a range (e.g. between siblings). When the natural limit lands
      // inside the range, we can break at the limit. However, float rounding can also place the
      // range start slightly *after* the limit while still within `BREAK_EPSILON`. In that case we
      // must not break before the opportunity starts, or we'd slice the preceding box and produce a
      // near-zero continuation fragment.
      let candidate_pos = opportunity.end.min(limit).max(opportunity.pos);
      if candidate_pos <= start + BREAK_EPSILON {
        continue;
      }
      if pos_is_inside_atomic_for_fragmentainer(candidate_pos, fragmentainer, atomic) {
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
      };

      match best {
        None => best = Some((constraint_key, strength_penalty, kind_rank, candidate_pos)),
        Some((best_key, best_penalty, best_kind, best_pos)) => {
          if constraint_key < best_key
            || (constraint_key == best_key && strength_penalty < best_penalty)
            || (constraint_key == best_key
              && strength_penalty == best_penalty
              && kind_rank < best_kind)
            || (constraint_key == best_key
              && strength_penalty == best_penalty
              && kind_rank == best_kind
              && candidate_pos > best_pos + BREAK_EPSILON)
          {
            best = Some((constraint_key, strength_penalty, kind_rank, candidate_pos));
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
      if kind_rank == 0 && self.enforce_fragmentainer_size && !self.allow_early_sibling_breaks {
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

    if matches!(self.context, FragmentationContext::Column)
      && !self.enforce_fragmentainer_size
      && !self.limit_inside_breakable_float(limit, fragmentainer)
      && !limit_clamped_to_atomic_start
    {
      // Multi-column layout prefers moving content to the next available break opportunity rather
      // than slicing it at an arbitrary fragmentainer limit (e.g. splitting a block box when the
      // next legal break is just after the limit). Only do this when the caller did not request a
      // hard fragmentainer size.
      //
      // However, parallel fragmentation flows like floats suppress their internal break
      // opportunities. Without a distance cap, this "lookahead" can skip an entire oversized float
      // and yield a first column that exceeds the fragmentainer size, preventing the float from
      // fragmenting across columns/column-sets.
      //
      // Restrict the lookahead to break opportunities that are *very* close to the limit so we
      // still avoid near-zero continuation fragments caused by float rounding, while ensuring
      // genuinely oversized content fragments to make progress.
      if let Some(next) = self.opportunities[window_end..].iter().find(|o| {
        o.pos > limit + BREAK_EPSILON
          && !pos_is_inside_atomic_for_fragmentainer(o.pos, fragmentainer, atomic)
      }) {
        let delta = next.pos - limit;
        if delta.is_finite() && delta <= LINE_FALLBACK_EPSILON {
          let clamped = next.pos.min(total_extent);
          self.advance_line_starts(clamped);
          return clamped;
        }
      }
    }

    let mut fallback = limit;
    if let Some(near_line) =
      self.near_line_boundary(start, limit, window.clone(), fragmentainer, atomic)
    {
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
    fragmentainer: f32,
    atomic: &[AtomicRange],
  ) -> Option<f32> {
    let forced = self.opportunities[window]
      .iter()
      .find(|o| {
        matches!(o.strength, BreakStrength::Forced)
          && o.pos > start + BREAK_EPSILON
          && o.pos <= limit + BREAK_EPSILON
          && !pos_is_inside_atomic_for_fragmentainer(o.pos, fragmentainer, atomic)
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
    fragmentainer: f32,
    atomic: &[AtomicRange],
  ) -> Option<f32> {
    self.opportunities[window].iter().find_map(|o| {
      if o.pos <= start + BREAK_EPSILON {
        return None;
      }
      if o.pos - limit > LINE_FALLBACK_EPSILON {
        return None;
      }
      if pos_is_inside_atomic_for_fragmentainer(o.pos, fragmentainer, atomic) {
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

/// Fragment a tree using the provided writing mode and direction.
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
  fragment_tree_impl(root, options, axes)
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
  let axes = axes_from_root(root);
  fragment_tree_impl(root, options, axes)
}

fn fragment_tree_impl(
  root: &FragmentNode,
  options: &FragmentationOptions,
  axes: FragmentAxes,
) -> Result<Vec<FragmentNode>, LayoutError> {
  if options.fragmentainer_size <= 0.0 {
    return Ok(vec![root.clone()]);
  }

  let axis = axis_from_fragment_axes(axes);
  let inline_is_horizontal = axes.inline_axis() == PhysicalAxis::X;
  let block_sign = if axis.block_positive { 1.0 } else { -1.0 };
  let inline_sign = if axes.inline_positive() { 1.0 } else { -1.0 };
  let context = if options.column_count > 1 {
    FragmentationContext::Column
  } else {
    FragmentationContext::Page
  };

  // Model forced breaks inside grid items (parallel fragmentation flow) as inserting blank space up
  // to the next fragmentainer boundary (CSS Grid 2 §Fragmenting Grid Layout). This ensures the
  // continuation content appears on later pages/columns without forcing sibling grid items onto the
  // next fragmentainer, even when the item spans multiple tracks.
  let mut root = root.clone();
  apply_grid_parallel_flow_forced_break_shifts(
    &mut root,
    axes,
    options.fragmentainer_size,
    context,
  );
  apply_table_cell_parallel_flow_forced_break_shifts(
    &mut root,
    axes,
    options.fragmentainer_size,
    context,
  );
  apply_float_parallel_flow_forced_break_shifts(
    &mut root,
    axes,
    options.fragmentainer_size,
    context,
  );
  apply_flex_parallel_flow_forced_break_shifts(
    &mut root,
    axes,
    options.fragmentainer_size,
    context,
  );
  apply_abspos_parallel_flow_forced_break_shifts(
    &mut root,
    axes,
    options.fragmentainer_size,
    context,
  );

  let mut analyzer =
    FragmentationAnalyzer::new(&root, context, axes, true, Some(options.fragmentainer_size));
  if matches!(context, FragmentationContext::Column) {
    analyzer.set_allow_early_sibling_breaks(true);
  }

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
      let break_before_forced = index != 0 && analyzer.is_forced_break_at(start);
      let break_after_forced = index + 1 != fragment_count && analyzer.is_forced_break_at(end);
      normalize_fragment_margins(
        &mut clipped,
        index == 0,
        index + 1 == fragment_count,
        break_before_forced,
        break_after_forced,
        &axis,
      );
      propagate_fragment_metadata(&mut clipped, index, fragment_count);

      // Translate fragments to account for fragmentainer gaps so downstream consumers
      // can reason about the absolute position of each fragmentainer stack. When
      // columns are requested, fragments are distributed left-to-right before
      // stacking additional rows vertically.
      let column = index % column_count;
      let row = index / column_count;
      if options.column_count > 1 {
        propagate_fragmentainer_columns(&mut clipped, row, column);
      }
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

pub(crate) fn propagate_fragmentainer_columns(
  node: &mut FragmentNode,
  column_set: usize,
  column: usize,
) {
  if node.fragmentainer.column_set_index.is_none() {
    node.fragmentainer.column_set_index = Some(column_set);
  }
  if node.fragmentainer.column_index.is_none() {
    node.fragmentainer.column_index = Some(column);
  }
  node.fragmentainer_index = node.fragmentainer.flattened_index();
  for child in node.children_mut() {
    propagate_fragmentainer_columns(child, column_set, column);
  }
}

fn clip_grid_item_parallel_for_fragmentainer(
  item: &FragmentNode,
  axis: &FragmentAxis,
  axes: FragmentAxes,
  fragmentainer_size: f32,
  fragment_index: usize,
  offset_in_fragment: f32,
  context: FragmentationContext,
) -> Result<Option<FragmentNode>, LayoutError> {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return Ok(None);
  }

  let mut offset = offset_in_fragment;
  if !offset.is_finite() || offset < 0.0 {
    offset = 0.0;
  } else if offset > fragmentainer_size {
    offset = fragmentainer_size;
  }
  if offset <= BREAK_EPSILON || (fragmentainer_size - offset) <= BREAK_EPSILON {
    offset = 0.0;
  }

  // Treat the grid item subtree as its own flow starting at the origin. When the grid item begins
  // partway through the current fragmentainer, wrap it in a synthetic root that offsets the item by
  // `offset` along the fragmentation axis. This mirrors the page's coordinate system so the item's
  // first fragment only gets `fragmentainer_size - offset` space while subsequent fragments use the
  // full fragmentainer size.
  let mut local_item = item.clone();
  let origin = local_item.bounds.origin;
  if origin.x != 0.0 || origin.y != 0.0 {
    local_item.bounds = Rect::from_xywh(
      0.0,
      0.0,
      local_item.bounds.width(),
      local_item.bounds.height(),
    );
    if let Some(logical) = local_item.logical_override {
      local_item.logical_override = Some(logical.translate(Point::new(-origin.x, -origin.y)));
    }
  }

  let (flow_root, offset_item_child) = if offset > 0.0 {
    let item_block_size = axis.block_size(&local_item.bounds);
    let wrapper_block_size = item_block_size + offset;
    let wrapper_inline_size = axis.inline_size(&local_item.bounds);
    let wrapper_bounds = if axis.block_is_horizontal {
      Rect::from_xywh(0.0, 0.0, wrapper_block_size, wrapper_inline_size)
    } else {
      Rect::from_xywh(0.0, 0.0, wrapper_inline_size, wrapper_block_size)
    };

    // Position the item inside the wrapper at the desired flow offset without shifting its
    // descendants (bounds are relative to the item root, so moving the root is sufficient).
    let target_block_start =
      axis.flow_box_start_to_physical(offset, item_block_size, wrapper_block_size);
    let current_block_start = axis.block_start(&local_item.bounds);
    let delta = target_block_start - current_block_start;
    if delta.abs() > BREAK_EPSILON {
      let delta_point = if axis.block_is_horizontal {
        Point::new(delta, 0.0)
      } else {
        Point::new(0.0, delta)
      };
      translate_fragment_in_parent_space(&mut local_item, delta_point);
    }

    (
      FragmentNode::new_block(wrapper_bounds, vec![local_item]),
      true,
    )
  } else {
    (local_item, false)
  };

  let mut analyzer =
    FragmentationAnalyzer::new(&flow_root, context, axes, true, Some(fragmentainer_size));
  // Forced breaks inside parallel grid items are modelled as blank insertion via
  // `apply_grid_parallel_flow_forced_break_shifts`. When fragmenting the item subtree to obtain the
  // per-fragmentainer slice, suppress the original forced break opportunities so they do not
  // introduce additional boundaries (which would effectively apply the forced break twice).
  analyzer
    .opportunities
    .retain(|o| !matches!(o.strength, BreakStrength::Forced));
  let total_extent = analyzer.content_extent();
  let boundaries = analyzer.boundaries(fragmentainer_size, total_extent)?;
  let fragment_count = boundaries.len().saturating_sub(1);
  if fragment_count == 0 || fragment_index >= fragment_count {
    return Ok(None);
  }

  let start = boundaries[fragment_index];
  let end = boundaries[fragment_index + 1];
  if end <= start + BREAK_EPSILON {
    return Ok(None);
  }

  let Some(mut clipped_root) = clip_node(
    &flow_root,
    axis,
    start,
    end,
    0.0,
    start,
    end,
    axis.block_size(&flow_root.bounds),
    fragment_index,
    fragment_count,
    context,
    fragmentainer_size,
    axes,
  )?
  else {
    return Ok(None);
  };

  if !offset_item_child {
    return Ok(Some(clipped_root));
  }

  // `FragmentNode` implements a custom `Drop` to avoid recursive destruction, so we must not move
  // fields out directly (E0509). Take the children list out and leave an empty placeholder instead.
  let mut iter = std::mem::take(&mut clipped_root.children).into_iter();
  let Some(mut item_fragment) = iter.next() else {
    return Ok(None);
  };

  // The synthetic wrapper introduces a leading offset before the grid item. For the first fragment,
  // `clip_node` returns the item positioned at that offset. Shift it back so the returned fragment
  // behaves like a normal clipped subtree rooted at the item origin.
  if fragment_index == 0 {
    let item_block_start = axis.block_start(&item_fragment.bounds);
    if item_block_start.abs() > BREAK_EPSILON {
      let delta_point = if axis.block_is_horizontal {
        Point::new(-item_block_start, 0.0)
      } else {
        Point::new(0.0, -item_block_start)
      };
      translate_fragment_in_parent_space(&mut item_fragment, delta_point);
    }
  }

  Ok(Some(item_fragment))
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
  if let Some(required) = grid_container_parallel_flow_required_block_size(
    node,
    node_flow_start,
    axis,
    axes,
    fragmentainer_size,
    context,
  ) {
    node_block_size = node_block_size.max(required);
    node_flow_end = node_flow_start + node_block_size;
  }
  if let Some(required) =
    abspos_parallel_flow_required_block_size(node, node_flow_start, axis, fragmentainer_size, context)
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
  // when their laid-out bounds fit within a single fragmentainer. Boundary resolution inflates the
  // total extent using `parallel_flow_content_extent`, so clipping must also treat ancestor nodes as
  // overlapping later fragmentainers (pages/columns) or the continuation content would be dropped.
  let parallel_flow_might_extend_past_fragment_start = node_bbox_flow_end <= fragment_start
    || (matches!(context, FragmentationContext::Column)
      && node_flow_end <= fragment_start
      && node_bbox_flow_end > fragment_start + BREAK_EPSILON);
  if fragmentainer_size.is_finite()
    && fragmentainer_size > 0.0
    && parallel_flow_might_extend_past_fragment_start
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
  //
  // In multi-column fragmentation, content is clipped to the column fragmentainer and must not
  // "continue" solely because a descendant overflows the box's own bounds (e.g. a 10px-tall box
  // whose text has a 1px descent overflow). Including such overflow in the overlap test can produce
  // tiny continuation fragments that duplicate `box_id`s across columns/pages, which in turn breaks
  // the paginator's stable break-token mapping.
  let (overlap_start, overlap_end, overlap_is_zero) = if matches!(context, FragmentationContext::Column) {
    (node_flow_start, node_flow_end, node_block_size <= BREAK_EPSILON)
  } else {
    (node_bbox_flow_start, node_bbox_flow_end, node_bbox_block_size <= BREAK_EPSILON)
  };
  if overlap_end < fragment_start
    || (overlap_end <= fragment_start && !overlap_is_zero)
    || overlap_start >= fragment_end
  {
    return Ok(None);
  }
  let table_row_like = is_table_row_like(style.display);
  let mut avoid_inside = avoids_break_inside(style.break_inside, context) || table_row_like;
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
  let clipped_flow_end = if matches!(context, FragmentationContext::Column) {
    node_flow_end.min(fragment_end)
  } else {
    node_bbox_flow_end.min(fragment_end)
  };
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
    if !overlaps {
      return Ok(None);
    }

    // If the line itself is larger than the fragmentainer, the "move the whole line to the
    // fragment that starts within the line box" rule would skip it entirely on the fragment that
    // contains its start edge (leading to blank pages + infinite pagination loops). In this case,
    // keep the line on the fragment that contains its start and allow it to overflow.
    let oversized = fragmentainer_size.is_finite()
      && fragmentainer_size > 0.0
      && node_block_size.is_finite()
      && node_block_size > fragmentainer_size + BREAK_EPSILON;
    if oversized {
      if !fragment_contains_line_start {
        return Ok(None);
      }
    } else if !fully_contained
      && !fragment_starts_inside
      && !(fragment_is_last && fragment_contains_line_start)
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

  let grid_items = if matches!(style.display, Display::Grid | Display::InlineGrid) {
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
      let child_required = grid_item_parallel_flow_required_block_size(
        child,
        axes,
        fragmentainer_size,
        child_abs_start,
        context,
      );
      let child_abs_end = child_abs_start + child_required;
      if child_abs_end <= fragment_start + BREAK_EPSILON
        || child_abs_start >= fragment_end - BREAK_EPSILON
      {
        continue;
      }

      let item_starts_in_fragment = child_abs_start + BREAK_EPSILON >= fragment_start
        && child_abs_start < fragment_end - BREAK_EPSILON;
      let mut offset_in_fragment = 0.0f32;
      if fragmentainer_size.is_finite() && fragmentainer_size > 0.0 {
        offset_in_fragment = if item_starts_in_fragment {
          // The item begins partway through this fragmentainer slice. Measure the offset from the
          // fragmentainer start edge in flow coordinates so the item's first fragment only gets the
          // remaining `fragmentainer_size - offset` space.
          let delta = (child_abs_start - fragment_start).max(0.0);
          if axis.block_positive {
            delta
          } else {
            // For reversed block progression, compute the offset using the same physical→flow
            // mapping as `FragmentAxis::flow_offset` so the value matches the flow coordinate system.
            let child_block_size = axis.block_size(&child.bounds);
            let phys = axis.flow_box_start_to_physical(delta, child_block_size, fragmentainer_size);
            axis.flow_offset(phys, child_block_size, fragmentainer_size)
          }
        } else {
          // When the item start is on an earlier page, recover its start-page offset by treating
          // the current fragment start as another fragmentainer boundary.
          (child_abs_start - fragment_start).rem_euclid(fragmentainer_size)
        };

        if offset_in_fragment <= BREAK_EPSILON
          || (fragmentainer_size - offset_in_fragment) <= BREAK_EPSILON
        {
          offset_in_fragment = 0.0;
        }
      }

      let local_index = if fragment_start > child_abs_start + BREAK_EPSILON
        && fragmentainer_size.is_finite()
        && fragmentainer_size > 0.0
      {
        let delta = fragment_start - child_abs_start;
        if offset_in_fragment <= BREAK_EPSILON {
          ((delta / fragmentainer_size).floor() as i32).max(0) as usize
        } else {
          let first = (fragmentainer_size - offset_in_fragment).max(0.0);
          if delta + BREAK_EPSILON < first {
            0
          } else {
            let remaining = (delta - first).max(0.0);
            1 + (((remaining / fragmentainer_size).floor() as i32).max(0) as usize)
          }
        }
      } else {
        0
      };

      if let Some(mut item_fragment) = clip_grid_item_parallel_for_fragmentainer(
        child,
        axis,
        axes,
        fragmentainer_size,
        local_index,
        offset_in_fragment,
        context,
      )? {
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
    if let Some(child_clipped) = clip_node(
      child,
      axis,
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
  let float_inflated_by_overflow = node_bbox_flow_end > node_flow_end + BREAK_EPSILON
    || node_bbox_flow_start + BREAK_EPSILON < node_flow_start;
  if style.float.is_floating() && !node.children.is_empty() && float_inflated_by_overflow {
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
      // `bounds` stores the physical block-start coordinate (left/top). For reversed block
      // progression (`block_positive = false`), keep the physical block-end edge fixed when
      // shrinking so the fragment remains anchored to the same flow start.
      let new_block_start = if axis.block_positive {
        block_start
      } else {
        block_start + (original_block_size - max_flow_end)
      };
      cloned.bounds = axis.update_block_components(cloned.bounds, new_block_start, max_flow_end);
      // Update slice metadata so background painting and other consumers see the trimmed extent.
      let slice_offset = cloned.slice_info.slice_offset;
      let slice_end_offset = slice_offset + max_flow_end;
      let epsilon = 0.01;
      cloned.slice_info.is_last =
        slice_end_offset >= cloned.slice_info.original_block_size - epsilon;
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
    stacking_context: node.stacking_context,
    fragment_index: node.fragment_index,
    fragment_count: node.fragment_count,
    fragmentainer_index: node.fragmentainer_index,
    fragmentainer: node.fragmentainer,
    slice_info: node.slice_info,
    scroll_overflow: node.scroll_overflow,
    abs_containing_block_box_id: node.abs_containing_block_box_id,
    scrollbar_reservation: node.scrollbar_reservation,
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
) -> Vec<TableRepetitionInfo> {
  let mut out = Vec::new();
  let default_style = default_style();

  fn walk(
    node: &FragmentNode,
    abs_start: f32,
    axis: &FragmentAxis,
    _context: FragmentationContext,
    default_style: &ComputedStyle,
    out: &mut Vec<TableRepetitionInfo>,
  ) {
    let style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
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
      let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
      walk(child, child_abs_start, axis, _context, default_style, out);
    }
  }

  walk(root, abs_start, axis, context, default_style, &mut out);
  out
}

pub(crate) fn collect_table_repetition_info_with_axes(
  root: &FragmentNode,
  axes: FragmentAxes,
  context: FragmentationContext,
) -> Vec<TableRepetitionInfo> {
  let axis = axis_from_fragment_axes(axes);
  collect_table_repetition_info_with_axis(root, 0.0, &axis, context)
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
    .max_by(|a, b| {
      a.start
        .partial_cmp(&b.start)
        .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Computes grid line center positions for a collapsed-border table fragment.
///
/// This mirrors `layout::table::collapsed_line_positions`, but only returns the line centers
/// (fragmentation does not need the per-row/column start offsets).
///
/// Line widths are centered on the grid lines. `padding_start` / `padding_end` describe the
/// distance from the fragment origin to the first/last grid line centers (e.g. half of the
/// baseline outer border widths when the slice includes the table's outer edges; CSS 2.1 §17.6.2).
fn collapsed_line_positions(
  sizes: &[f32],
  line_widths: &[f32],
  padding_start: f32,
  padding_end: f32,
) -> Vec<f32> {
  debug_assert!(line_widths.len() >= sizes.len().saturating_add(1));

  let mut line_pos = Vec::with_capacity(sizes.len() + 1);
  let mut cursor = padding_start;
  line_pos.push(cursor);

  for (idx, size) in sizes.iter().enumerate() {
    let prev_half = line_widths.get(idx).copied().unwrap_or(0.0) * 0.5;
    let next_half = line_widths.get(idx + 1).copied().unwrap_or(0.0) * 0.5;
    cursor += prev_half + size + next_half;
    line_pos.push(cursor);
  }

  let _extent = cursor + padding_end;
  line_pos
}

fn inject_table_headers_and_footers(
  original: &FragmentNode,
  clipped: &mut FragmentNode,
  fragment_index: usize,
  fragment_count: usize,
  axis: &FragmentAxis,
  _context: FragmentationContext,
) {
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
  let header_injected = !headers.is_empty() && !has_header && !clipped.slice_info.is_first;
  let footer_injected = !footers.is_empty() && !has_footer && !clipped.slice_info.is_last;

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

  if header_injected {
    let mut regions = Vec::new();
    for header in &headers {
      let (start, end) = axis.flow_range(0.0, original_block_size, &header.bounds);
      regions.push((start, end));
    }
    let region_height: f32 = regions.iter().map(|(s, e)| e - s).sum();
    for child in clipped.children_mut() {
      translate_fragment_in_parent_space(child, axis.block_translation(region_height));
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
        let (c_start, c_end) = axis.flow_range(0.0, original_block_size, &candidate.bounds);
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

  if footer_injected {
    let mut regions = Vec::new();
    for footer in &footers {
      let (start, end) = axis.flow_range(0.0, original_block_size, &footer.bounds);
      regions.push((start, end));
    }
    let footer_start = clipped
      .children
      .iter()
      .map(|c| axis.flow_range(0.0, clipped_block_size, &c.bounds).1)
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
        let (c_start, c_end) = axis.flow_range(0.0, original_block_size, &candidate.bounds);
        if c_start + 0.01 >= start && c_end <= end + 0.01 {
          let mut clone = candidate.clone();
          translate_fragment_in_parent_space(&mut clone, region_translation);
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

  if header_injected || footer_injected {
    if let Some(orig_borders) = original.table_borders.as_deref() {
      if clipped.table_borders.is_some() {
        const EPSILON: f32 = 0.01;
        let slice_end = slice_start + clipped_block_size;

        let mut row_offsets = Vec::with_capacity(orig_borders.row_count);
        for row in 0..orig_borders.row_count {
          row_offsets.push(orig_borders.row_offset(row).unwrap_or(0.0));
        }

        let start_pp = row_offsets.partition_point(|&o| o <= slice_start + EPSILON);
        let body_start_row = start_pp.saturating_sub(1).min(orig_borders.row_count);
        let end_pp = row_offsets.partition_point(|&o| o <= slice_end - EPSILON);
        let body_end_row = end_pp.max(body_start_row).min(orig_borders.row_count);

        let header_range = if header_injected {
          orig_borders.header_rows.unwrap_or((0, 0))
        } else {
          (0, 0)
        };
        let footer_range = if footer_injected {
          orig_borders.footer_rows.unwrap_or((0, 0))
        } else {
          (0, 0)
        };

        let header_len = header_range.1.saturating_sub(header_range.0);
        let footer_len = footer_range.1.saturating_sub(footer_range.0);
        let body_len = body_end_row.saturating_sub(body_start_row);

        let mut row_map: Vec<usize> = Vec::with_capacity(header_len + body_len + footer_len);
        if header_len > 0 {
          row_map.extend(header_range.0..header_range.1);
        }
        row_map.extend(body_start_row..body_end_row);
        if footer_len > 0 {
          row_map.extend(footer_range.0..footer_range.1);
        }

        let new_row_count = row_map.len();
        if new_row_count > 0 {
          let mut vertical_borders =
            Vec::with_capacity((orig_borders.column_count + 1) * new_row_count);
          for col in 0..=orig_borders.column_count {
            for &orig_row in &row_map {
              vertical_borders.push(
                orig_borders
                  .vertical_segment(col, orig_row)
                  .unwrap_or(CollapsedBorderSegment::none()),
              );
            }
          }

          let starts_with_header_rows = match orig_borders.header_rows {
            Some((start, end)) if start < end => row_map.first().map_or(false, |r| *r < end),
            _ => false,
          };
          let ends_with_footer_rows = match orig_borders.footer_rows {
            Some((start, end)) if start < end => row_map.last().map_or(false, |r| *r >= start),
            _ => false,
          };

          let mut boundary_source: Vec<Option<usize>> = Vec::with_capacity(new_row_count + 1);
          for boundary in 0..=new_row_count {
            let source = if boundary == 0 {
              if starts_with_header_rows {
                Some(0)
              } else {
                Some(body_start_row)
              }
            } else if boundary == new_row_count {
              if ends_with_footer_rows {
                Some(orig_borders.row_count)
              } else {
                Some(body_end_row)
              }
            } else if header_injected && header_len > 0 && body_len > 0 && boundary == header_len {
              Some(header_range.1)
            } else if footer_injected
              && footer_len > 0
              && body_len > 0
              && boundary == header_len + body_len
            {
              Some(footer_range.0)
            } else {
              let prev_orig_row = row_map[boundary - 1];
              let next_orig_row = row_map[boundary];
              (next_orig_row == prev_orig_row + 1).then_some(next_orig_row)
            };
            boundary_source.push(source);
          }

          let mut horizontal_borders =
            Vec::with_capacity((new_row_count + 1) * orig_borders.column_count);
          let mut horizontal_line_base = Vec::with_capacity(new_row_count + 1);
          for source in &boundary_source {
            horizontal_line_base.push(
              source
                .map(|orig_boundary| orig_borders.horizontal_line_width(orig_boundary))
                .unwrap_or(0.0),
            );
            for col in 0..orig_borders.column_count {
              horizontal_borders.push(
                source
                  .and_then(|orig_boundary| orig_borders.horizontal_segment(orig_boundary, col))
                  .unwrap_or(CollapsedBorderSegment::none()),
              );
            }
          }

          let mut row_heights = Vec::with_capacity(new_row_count);
          for &orig_row in &row_map {
            row_heights.push(orig_borders.row_height(orig_row).unwrap_or(0.0));
          }

          let padding_start = if clipped.slice_info.is_first && !header_injected {
            orig_borders
              .row_line_positions
              .first()
              .copied()
              .unwrap_or(0.0)
          } else {
            0.0
          };
          let padding_end = if clipped.slice_info.is_last && !footer_injected {
            let last_line = orig_borders
              .row_line_positions
              .last()
              .copied()
              .unwrap_or(0.0);
            (original_block_size - last_line).max(0.0)
          } else {
            0.0
          };

          let row_line_positions = collapsed_line_positions(
            &row_heights,
            &horizontal_line_base,
            padding_start,
            padding_end,
          );

          let mut corner_borders =
            Vec::with_capacity((new_row_count + 1) * (orig_borders.column_count + 1));
          for source in &boundary_source {
            for col in 0..=orig_borders.column_count {
              corner_borders.push(
                source
                  .and_then(|orig_boundary| orig_borders.corner(orig_boundary, col))
                  .unwrap_or(CollapsedBorderSegment::none()),
              );
            }
          }

          let max_corner_half = corner_borders
            .iter()
            .map(|c| c.width * 0.5)
            .fold(0.0f32, f32::max);

          // Collapsed-border paint bounds may extend outside the table fragment rect.
          //
          // In particular, outer-edge segments can be thicker than the baseline widths used for
          // layout (§17.6.2). That extra thickness must spill outward into the margin instead of
          // widening the table (WPT `border-collapse-basic-001`), so we must recompute paint bounds
          // from the actual outer-edge segment widths for this fragment slice.
          //
          // This must match the paint-time "inside" clamping in
          // `paint::display_list_renderer::render_table_collapsed_borders`.
          let baseline_left = orig_borders.vertical_line_width(0);
          let baseline_right = orig_borders.vertical_line_width(orig_borders.column_count);
          let mut outer_left_outward = 0.0f32;
          let mut outer_right_outward = 0.0f32;
          if new_row_count > 0 {
            // `vertical_borders` is column-major: `[col0 rows..., col1 rows..., ...]`.
            for row in 0..new_row_count {
              let width = vertical_borders[row].width;
              let inside = (width.min(baseline_left)) * 0.5;
              outer_left_outward = outer_left_outward.max(width - inside);
            }
            let right_start = orig_borders.column_count * new_row_count;
            for idx in right_start..right_start + new_row_count {
              let width = vertical_borders[idx].width;
              let inside = (width.min(baseline_right)) * 0.5;
              outer_right_outward = outer_right_outward.max(width - inside);
            }
          }

          let baseline_top = horizontal_line_base.first().copied().unwrap_or(0.0);
          let baseline_bottom = horizontal_line_base.last().copied().unwrap_or(0.0);
          let mut outer_top_outward = 0.0f32;
          let mut outer_bottom_outward = 0.0f32;
          if orig_borders.column_count > 0 {
            // `horizontal_borders` is boundary-major: `[boundary0 cols..., boundary1 cols..., ...]`.
            for col in 0..orig_borders.column_count {
              let width = horizontal_borders[col].width;
              let inside = (width.min(baseline_top)) * 0.5;
              outer_top_outward = outer_top_outward.max(width - inside);
            }
            let bottom_start = new_row_count * orig_borders.column_count;
            for idx in bottom_start..bottom_start + orig_borders.column_count {
              let width = horizontal_borders[idx].width;
              let inside = (width.min(baseline_bottom)) * 0.5;
              outer_bottom_outward = outer_bottom_outward.max(width - inside);
            }
          }

          let min_x = orig_borders
            .column_line_positions
            .first()
            .copied()
            .unwrap_or(0.0)
            - outer_left_outward.max(max_corner_half);
          let max_x = orig_borders
            .column_line_positions
            .last()
            .copied()
            .unwrap_or(0.0)
            + outer_right_outward.max(max_corner_half);
          let min_y = row_line_positions.first().copied().unwrap_or(0.0)
            - outer_top_outward.max(max_corner_half);
          let max_y = row_line_positions.last().copied().unwrap_or(0.0)
            + outer_bottom_outward.max(max_corner_half);

          clipped.table_borders = Some(Arc::new(TableCollapsedBorders {
            column_count: orig_borders.column_count,
            row_count: new_row_count,
            column_line_positions: orig_borders.column_line_positions.clone(),
            row_line_positions,
            vertical_borders,
            horizontal_borders,
            corner_borders,
            vertical_line_base: orig_borders.vertical_line_base.clone(),
            horizontal_line_base,
            paint_bounds: Rect::from_xywh(
              min_x,
              min_y,
              (max_x - min_x).max(0.0),
              (max_y - min_y).max(0.0),
            ),
            header_rows: orig_borders.header_rows,
            footer_rows: orig_borders.footer_rows,
            fragment_local: true,
          }));
        }
      }
    }
  }

  let children_block_end = clipped
    .children
    .iter()
    .map(|c| axis.flow_range(0.0, clipped_block_size, &c.bounds).1)
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
  break_before_forced: bool,
  _break_after_forced: bool,
  axis: &FragmentAxis,
) {
  const EPSILON: f32 = 0.01;
  let fragment_block_size = axis.block_size(&fragment.bounds);

  // CSS Break 3 §Adjoining Margins at Breaks:
  // - Unforced breaks: margins adjoining the break are truncated to zero.
  // - Forced breaks: margins *before* the break are truncated, margins *after* are preserved.
  //
  // We lay out the fragment tree in continuous space first, then slice it per-fragmentainer.
  // This normalization step adjusts in-flow block fragment positions/sizes so the sliced output
  // matches the spec's margin truncation rules.

  // Normalize the start edge (margin-top after the break).
  //
  // For continuation fragments, truncate the first in-flow block's top margin to 0 unless the
  // preceding break was forced, in which case preserve it. Apply the translation to all in-flow
  // block children to avoid consuming inter-sibling collapsed margins.
  if !is_first_fragment {
    let mut min_start: Option<f32> = None;
    for child in fragment
      .children
      .iter()
      // Floats form a parallel fragmentation flow: they can continue at the start of the next
      // fragmentainer and force clearance on following in-flow blocks. In that case the block's
      // block-start offset is not an "adjoining margin after a break" and must not be normalized
      // away (otherwise cleared content can overlap the continued float).
      //
      // Include floats when finding the earliest flow start so margin normalization only triggers
      // when the fragment actually begins with an in-flow block.
      .filter(|c| {
        c.block_metadata.is_some()
          || c
            .style
            .as_deref()
            .is_some_and(|style| style.float.is_floating())
      })
    {
      let start = axis.flow_offset(
        axis.block_start(&child.bounds),
        axis.block_size(&child.bounds),
        fragment_block_size,
      );
      min_start = Some(match min_start {
        None => start,
        Some(prev) => prev.min(start),
      });
    }

    if let Some(min_start) = min_start {
      let mut delta: Option<f32> = None;
      for child in fragment
        .children
        .iter()
        .filter(|c| c.block_metadata.is_some())
      {
        let start = axis.flow_offset(
          axis.block_start(&child.bounds),
          axis.block_size(&child.bounds),
          fragment_block_size,
        );
        if (start - min_start).abs() >= EPSILON {
          continue;
        }
        let Some(meta) = child.block_metadata.as_ref() else {
          continue;
        };
        if meta.clipped_top {
          continue;
        }
        let desired = if break_before_forced {
          meta.margin_top
        } else {
          0.0
        };
        delta = Some(desired - start);
        break;
      }

      if let Some(delta) = delta {
        if delta.abs() > EPSILON {
          for child in fragment
            .children_mut()
            .iter_mut()
            .filter(|c| c.block_metadata.is_some())
          {
            translate_fragment_in_parent_space(child, axis.block_translation(delta));
          }
        }
      }
    }
  }

  // Normalize the end edge (margin-bottom before the break).
  //
  // CSS Break 3 truncates margins *before* a break, regardless of whether the break is forced or
  // unforced. Since this fragment is the one *preceding* the break, always shrink it to the last
  // in-flow block end so it does not retain trailing margin space that conceptually belongs to the
  // next fragmentainer.
  if !is_last_fragment {
    if let Some(max_end) = fragment
      .children
      .iter()
      .filter_map(|c| {
        let block_size = axis.block_size(&c.bounds);
        let start = axis.flow_offset(axis.block_start(&c.bounds), block_size, fragment_block_size);
        c.block_metadata.as_ref().map(|_| start + block_size)
      })
      .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    {
      let new_block_size = max_end.max(0.0);
      if new_block_size.is_finite() && new_block_size + EPSILON < fragment_block_size {
        fragment.bounds = axis.update_block_components(
          fragment.bounds,
          axis.block_start(&fragment.bounds),
          new_block_size,
        );
        // Keep overflow at least as large as the fragment bounds.
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
}

/// Axis-aware wrapper around [`normalize_fragment_margins`].
pub(crate) fn normalize_fragment_margins_with_axes(
  fragment: &mut FragmentNode,
  is_first_fragment: bool,
  is_last_fragment: bool,
  break_before_forced: bool,
  break_after_forced: bool,
  _fragment_block_size: f32,
  axes: FragmentAxes,
) {
  let axis = axis_from_fragment_axes(axes);
  normalize_fragment_margins(
    fragment,
    is_first_fragment,
    is_last_fragment,
    break_before_forced,
    break_after_forced,
    &axis,
  );
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
  suppress_parallel_flow_descendants: bool,
  suppress_forced_breaks: bool,
  parallel_flow_root: bool,
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
  let table_row_like = is_table_row_like(style.display);
  // CSS Break 3 excludes absolutely-positioned elements from `break-inside` applicability.
  let break_inside = if style.position.is_absolutely_positioned() {
    BreakInside::Auto
  } else {
    style.break_inside
  };
  let inside_avoid = avoid_depth
    + usize::from(avoids_break_inside(break_inside, context))
    + usize::from(table_row_like);
  let inside_inline = inline_depth
    + usize::from(matches!(
      node.content,
      FragmentContent::Line { .. } | FragmentContent::Inline { .. }
    ));

  // Parallel fragmentation flows (CSS Break 3 §3) must not contribute their internal break
  // opportunities to the parent flow. Floats and absolutely-positioned elements establish parallel
  // flows, so suppress them when requested.
  //
  // Note: Table cells are handled separately: their *forced* breaks must not propagate, but
  // non-forced break opportunities (e.g. line boundaries) still matter when fragmenting oversized
  // rows. See `child_suppress_forced_breaks` below.
  let is_parallel_flow = style.float.is_floating() || style.position.is_absolutely_positioned();
  if suppress_parallel_flow_descendants && !parallel_flow_root && is_parallel_flow {
    return;
  }

  let node_block_size = axis.block_size(&node.bounds);
  let node_flow_start = abs_start;
  let abs_end = abs_start + node_block_size;
  let is_row_flex_container_in_context = is_row_flex_container(style);

  let grid_items = if matches!(style.display, Display::Grid | Display::InlineGrid) {
    node.grid_fragmentation.as_deref()
  } else {
    None
  };

  // When the fragment includes both grid track ranges and per-item placement metadata, break hints
  // on grid items apply to the corresponding grid line boundaries (CSS Grid 2 §Fragmenting Grid
  // Layout).
  // Grid fragments may omit `grid_tracks` while retaining `grid_fragmentation` placement info. Use
  // the placement info alone to identify in-flow grid item children so we still suppress internal
  // forced breaks (which must not propagate to siblings; CSS Grid 2 §Fragmenting Grid Layout).
  let grid_item_count_parallel_flow = grid_items
    .as_ref()
    .map(|grid_items| grid_items.items.len().min(node.children.len()))
    .unwrap_or(0);
  // Break hints (`break-before/after`) can only be remapped to grid line boundaries when we have
  // physical track ranges for the fragmentation axis. When those ranges are missing (or empty),
  // fall back to treating authored break hints as applying at the grid item's own fragment
  // boundary so they still influence pagination.
  let mut grid_item_count_break_hint_suppression = 0usize;

  let grid_tracks = node
    .grid_tracks
    .as_deref()
    .map(|tracks| grid_tracks_in_fragmentation_axis(tracks, axis));
  let grid_item_break_hints_use_tracks = grid_item_count_parallel_flow > 0
    && grid_tracks.is_some_and(|tracks| !tracks.is_empty());
  let grid_item_break_hints_fallback_to_edges =
    grid_item_count_parallel_flow > 0 && !grid_item_break_hints_use_tracks;

  if grid_item_break_hints_use_tracks {
    grid_item_count_break_hint_suppression = grid_item_count_parallel_flow;
  }

  if let (Some(tracks), Some(grid_items)) = (grid_tracks, grid_items) {
    if grid_item_break_hints_use_tracks {
      let in_flow_count = grid_item_count_parallel_flow;

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

        let child_break_before = if child_style.position.is_absolutely_positioned() {
          BreakBetween::Auto
        } else {
          child_style.break_before
        };
        let mut before_strength =
          combine_breaks(BreakBetween::Auto, child_break_before, context);
        if suppress_forced_breaks && matches!(before_strength, BreakStrength::Forced) {
          before_strength = BreakStrength::Auto;
        }
        if !matches!(before_strength, BreakStrength::Auto) {
          let boundary_idx = start_line.saturating_sub(1) as usize;
          if let Some(slot) = boundary_strengths.get_mut(boundary_idx) {
            *slot = max_break_strength(*slot, before_strength);
          }
        }

        let child_break_after = if child_style.position.is_absolutely_positioned() {
          BreakBetween::Auto
        } else {
          child_style.break_after
        };
        let mut after_strength =
          combine_breaks(child_break_after, BreakBetween::Auto, context);
        if suppress_forced_breaks && matches!(after_strength, BreakStrength::Forced) {
          after_strength = BreakStrength::Auto;
        }
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
        let mut strength = apply_avoid_penalty(strength, inside_avoid > 0);
        if suppress_forced_breaks && matches!(strength, BreakStrength::Forced) {
          strength = BreakStrength::Auto;
        }
        if matches!(strength, BreakStrength::Auto) {
          continue;
        }
        let pos = if boundary_idx == 0 {
          abs_start
        } else if boundary_idx == tracks.len() {
          abs_end
        } else {
          let Some((track_start, track_end)) = tracks.get(boundary_idx.saturating_sub(1)).copied()
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
          end: pos,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
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

        let child_break_before = if child_style.position.is_absolutely_positioned() {
          BreakBetween::Auto
        } else {
          child_style.break_before
        };
        let before_strength = combine_breaks(BreakBetween::Auto, child_break_before, context);
        if !matches!(before_strength, BreakStrength::Auto) {
          if let Some(slot) = boundary_strengths.get_mut(line_idx) {
            *slot = max_break_strength(*slot, before_strength);
          }
        }

        let child_break_after = if child_style.position.is_absolutely_positioned() {
          BreakBetween::Auto
        } else {
          child_style.break_after
        };
        let after_strength = combine_breaks(child_break_after, BreakBetween::Auto, context);
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
        end: first_line_start,
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
          end: line.end,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }

      let start_strength = boundary_strengths[0];
      if !matches!(start_strength, BreakStrength::Auto) {
        let strength = apply_avoid_penalty(start_strength, inside_avoid > 0);
        collection.opportunities.push(BreakOpportunity {
          pos: abs_start,
          end: abs_start,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }
      let end_strength = boundary_strengths[line_count];
      if !matches!(end_strength, BreakStrength::Auto) {
        let strength = apply_avoid_penalty(end_strength, inside_avoid > 0);
        collection.opportunities.push(BreakOpportunity {
          pos: abs_end,
          end: abs_end,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }

      for (child_idx, child) in node.children.iter().enumerate() {
        if suppress_parallel_flow_descendants
          && flex_lines
            .line_for_child
            .get(child_idx)
            .is_some_and(|slot| slot.is_some())
        {
          continue;
        }
        let child_style = child
          .style
          .as_ref()
          .map(|s| s.as_ref())
          .unwrap_or(default_style);
        let child_abs_start = axis
          .flow_range(node_flow_start, node_block_size, &child.bounds)
          .0;
        collect_break_opportunities(
          child,
          child_abs_start,
          collection,
          inside_avoid,
          inside_inline,
          context,
          axis,
          node_writing_mode,
          suppress_parallel_flow_descendants,
          suppress_forced_breaks
            || (is_row_flex_container_in_context && child_style.position.is_in_flow()),
          false,
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

  // Break propagation must be based on the first/last *in-flow* child, excluding absolutely
  // positioned descendants (CSS Break 3 §4.1).
  let mut first_in_flow_child: Option<usize> = None;
  let mut last_in_flow_child: Option<usize> = None;
  let mut next_in_flow: Vec<Option<usize>> = vec![None; node.children.len()];
  {
    let mut last_seen: Option<usize> = None;
    for (idx, child) in node.children.iter().enumerate().rev() {
      let child_style = child
        .style
        .as_ref()
        .map(|s| s.as_ref())
        .unwrap_or(default_style);
      if child_style.position.is_in_flow() {
        if last_in_flow_child.is_none() {
          last_in_flow_child = Some(idx);
        }
        next_in_flow[idx] = last_seen;
        last_seen = Some(idx);
        first_in_flow_child = Some(idx);
      }
    }
  }

  for (idx, child) in node.children.iter().enumerate() {
    let (child_abs_start, child_abs_end) =
      axis.flow_range(node_flow_start, node_block_size, &child.bounds);
    let child_style = child
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let next_in_flow_idx = next_in_flow.get(idx).copied().flatten();
    let next_in_flow_child = next_in_flow_idx.and_then(|i| node.children.get(i));
    let next_style = next_in_flow_child
      .and_then(|c| c.style.as_ref())
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let next_abs_start = next_in_flow_child.map(|next| {
      axis
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
        end: line_end,
        strength,
        kind: BreakKind::LineBoundary {
          container_id,
          line_index_end,
        },
      });
    }

    // Absolutely-positioned boxes do not participate in sibling break propagation. Treat them as
    // `auto` for `break-before/after` in the parent flow (CSS Break 3 §4.1).
    let child_break_before_value = if child_style.position.is_absolutely_positioned() {
      BreakBetween::Auto
    } else {
      child_style.break_before
    };
    let child_break_after_value = if child_style.position.is_absolutely_positioned() {
      BreakBetween::Auto
    } else {
      child_style.break_after
    };

    let child_break_before = if idx < grid_item_count_break_hint_suppression {
      BreakBetween::Auto
    } else {
      child_break_before_value
    };
    // Fragment trees can include "background layer" fragments that overlap the same flow
    // start position (notably table row-group fragments that precede row fragments for paint
    // ordering). In those cases the first *flow* child with `break-before` may not be the first
    // child in DOM/paint order, but browsers still treat it as a break at the start edge.
    //
    // Detect this for table rows by checking whether the child starts at the parent's flow start.
    let treat_as_first_break_before = Some(idx) == first_in_flow_child
      || (matches!(child_style.display, Display::TableRow)
        && (child_abs_start - abs_start).abs() <= BREAK_EPSILON);
    if treat_as_first_break_before && !matches!(child_break_before, BreakBetween::Auto) {
      let mut strength = combine_breaks(BreakBetween::Auto, child_break_before, context);
      strength = apply_avoid_penalty(strength, inside_avoid > 0);
      if suppress_forced_breaks && matches!(strength, BreakStrength::Forced) {
        strength = BreakStrength::Auto;
      }
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
        end: pos,
        strength,
        kind: BreakKind::BetweenSiblings,
      });
    }

    if grid_item_break_hints_fallback_to_edges
      && idx > 0
      && idx < grid_item_count_parallel_flow
      && !matches!(child_break_before_value, BreakBetween::Auto)
    {
      let mut strength = combine_breaks(BreakBetween::Auto, child_break_before_value, context);
      strength = apply_avoid_penalty(strength, inside_avoid > 0);
      if suppress_forced_breaks && matches!(strength, BreakStrength::Forced) {
        strength = BreakStrength::Auto;
      }
      if strength == BreakStrength::Auto
        && matches!(child.content, FragmentContent::Block { box_id: None })
      {
        strength = BreakStrength::Avoid;
      }
      if !matches!(strength, BreakStrength::Auto) {
        collection.opportunities.push(BreakOpportunity {
          pos: child_abs_start,
          end: child_abs_start,
          strength,
          kind: BreakKind::BetweenSiblings,
        });
      }
    }

    // In pagination, treat in-flow grid items as parallel fragmentation flows so their internal
    // break opportunities (including forced breaks) do not affect sibling items (CSS Grid 2
    // §Fragmenting Grid Layout).
    let is_in_flow_grid_item = idx < grid_item_count_parallel_flow && child_style.position.is_in_flow();
    let parallel_grid_item = grid_items
      .and_then(|info| info.items.get(idx))
      .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    // Grid items that span a single track in the fragmentation axis establish a parallel
    // fragmentation flow (CSS Grid 2 §Fragmenting Grid Layout). When collecting break opportunities
    // for the main pagination flow we also suppress descendants for spanning items so their internal
    // break opportunities do not become global boundaries.
    let skip_descendants = parallel_grid_item
      || (suppress_parallel_flow_descendants
        && matches!(context, FragmentationContext::Page)
        && is_in_flow_grid_item);
    if !skip_descendants {
      // CSS Grid 2 §Fragmenting Grid Layout: A forced break inside a grid item effectively
      // increases the size of its contents; it must not become a global forced break that affects
      // sibling items. Suppress forced breaks while recursing into in-flow grid items (including
      // those spanning multiple tracks), but still collect non-forced opportunities.
      let child_suppress_forced_breaks = suppress_forced_breaks
        || (is_row_flex_container_in_context && child_style.position.is_in_flow())
        || (idx < grid_item_count_parallel_flow && child_style.position.is_in_flow())
        || ((matches!(style.display, Display::Table | Display::InlineTable)
          || matches!(style.display, Display::TableRow))
          && matches!(child_style.display, Display::TableCell));
      collect_break_opportunities(
        child,
        child_abs_start,
        collection,
        inside_avoid,
        inside_inline,
        context,
        axis,
        node_writing_mode,
        suppress_parallel_flow_descendants,
        child_suppress_forced_breaks,
        false,
      );
    }

    if !child_style.position.is_in_flow() {
      continue;
    }

    let child_break_after = if idx < grid_item_count_break_hint_suppression {
      BreakBetween::Auto
    } else {
      child_break_after_value
    };
    let next_break_before = if next_in_flow_idx.is_some_and(|next_idx| next_idx < grid_item_count_break_hint_suppression) {
      BreakBetween::Auto
    } else {
      if next_style.position.is_absolutely_positioned() {
        BreakBetween::Auto
      } else {
        next_style.break_before
      }
    };
    let mut strength = combine_breaks(child_break_after, next_break_before, context);
    strength = apply_avoid_penalty(strength, inside_avoid > 0);
    if suppress_forced_breaks && matches!(strength, BreakStrength::Forced) {
      strength = BreakStrength::Auto;
    }
    if strength == BreakStrength::Auto
      && matches!(child.content, FragmentContent::Block { box_id: None })
    {
      strength = BreakStrength::Avoid;
    }
    // Break opportunities between siblings span the entire gap between the end of one fragment and
    // the start of the next (e.g. due to vertical margins). This allows fragmentation algorithms
    // (notably column balancing) to choose a boundary inside the gap when the fragmentainer limit
    // falls there.
    let (pos, end) = if matches!(strength, BreakStrength::Forced) {
      // CSS Break 3 §Adjoining Margins at Breaks: forced breaks truncate margins *before* the break.
      // Anchor the forced boundary to the end edge of the preceding child's border box so the
      // child's bottom margin does not become part of the previous fragmentainer slice.
      let mut boundary = child_abs_end;
      // Forced breaks after the last in-flow child propagate to the end edge of the containing
      // block. This mirrors the `break-before` propagation logic above and prevents fragmentation
      // from creating a trailing slice that contains only the parent's padding/align-content gaps
      // when the last child ends early.
      if Some(idx) == last_in_flow_child {
        boundary = abs_end;
      }
      (boundary, boundary)
    } else {
      let range_start = child_abs_end;
      let mut range_end = next_abs_start.unwrap_or(range_start);
      if range_end < range_start {
        range_end = range_start;
      }
      (range_start, range_end)
    };
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
        pos,
        end,
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
  collect_forced_boundaries_with_axes_internal(
    node,
    abs_start,
    axes,
    true,
    axes.page_progression_is_ltr(),
    true,
  )
}

pub(crate) fn collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
  page_progression_is_ltr: bool,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_with_axes_internal(
    node,
    abs_start,
    axes,
    true,
    page_progression_is_ltr,
    true,
  )
}

pub(crate) fn collect_forced_boundaries_for_pagination_with_axes_and_page_progression_excluding_always(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
  page_progression_is_ltr: bool,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_with_axes_internal(
    node,
    abs_start,
    axes,
    true,
    page_progression_is_ltr,
    false,
  )
}

pub(crate) fn collect_forced_boundaries_for_explicit_page_breaks_with_axes_and_page_progression(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
  page_progression_is_ltr: bool,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_for_pagination_with_axes_and_page_progression_excluding_always(
    node,
    abs_start,
    axes,
    page_progression_is_ltr,
  )
}

pub(crate) fn collect_forced_boundaries_with_axes(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
) -> Vec<ForcedBoundary> {
  collect_forced_boundaries_with_axes_internal(
    node,
    abs_start,
    axes,
    false,
    axes.page_progression_is_ltr(),
    true,
  )
}

fn collect_forced_boundaries_with_axes_internal(
  node: &FragmentNode,
  abs_start: f32,
  axes: FragmentAxes,
  suppress_parallel_flow_descendants: bool,
  page_progression_is_ltr: bool,
  include_always: bool,
) -> Vec<ForcedBoundary> {
  fn is_forced_page_break(between: BreakBetween, include_always: bool) -> bool {
    match between {
      // `always` is a forced break in the current fragmentation context. Nested callers (paged
      // multicol promotion) can set `include_always=false` so it is not treated as a pagination
      // boundary.
      BreakBetween::Always => include_always,
      BreakBetween::Page
      | BreakBetween::Left
      | BreakBetween::Right
      | BreakBetween::Recto
      | BreakBetween::Verso => true,
      _ => false,
    }
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
    side_conflict: bool,
  }

  fn merge_boundary_side(requirement: &mut BoundaryRequirement, incoming: Option<PageSide>) {
    if requirement.side_conflict {
      return;
    }
    match (requirement.side, incoming) {
      (None, side) => requirement.side = side,
      (Some(_), None) => {}
      (Some(a), Some(b)) if a == b => {}
      (Some(_), Some(_)) => {
        requirement.side = None;
        requirement.side_conflict = true;
      }
    };
  }

  fn record_boundary(requirement: &mut BoundaryRequirement, side: Option<PageSide>) {
    requirement.forced = true;
    merge_boundary_side(requirement, side);
  }

  fn collect(
    node: &FragmentNode,
    abs_start: f32,
    forced: &mut Vec<ForcedBoundary>,
    default_style: &ComputedStyle,
    axis: &FragmentAxis,
    parent_block_size: f32,
    suppress_parallel_flow_descendants: bool,
    page_progression_is_ltr: bool,
    include_always: bool,
  ) {
    let node_style = node
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    if suppress_parallel_flow_descendants
      && (node_style.float.is_floating()
        || node_style.position.is_absolutely_positioned()
        || matches!(node_style.display, Display::TableCell))
    {
      return;
    }
    let node_is_row_flex_container = is_row_flex_container(node_style);
    let grid_items = if matches!(node_style.display, Display::Grid | Display::InlineGrid) {
      node.grid_fragmentation.as_deref()
    } else {
      None
    };
    let in_flow_grid_item_count = grid_items
      .as_ref()
      .map(|grid_items| grid_items.items.len().min(node.children.len()))
      .unwrap_or(0);
    let grid_track_ranges_in_axis = if matches!(node_style.display, Display::Grid | Display::InlineGrid) {
      node
        .grid_tracks
        .as_deref()
        .map(|tracks| grid_tracks_in_fragmentation_axis(tracks, axis))
    } else {
      None
    };
    let grid_item_break_hints_fallback_to_edges =
      in_flow_grid_item_count > 0 && !grid_track_ranges_in_axis.is_some_and(|tracks| !tracks.is_empty());

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

            let child_break_before = if child_style.position.is_absolutely_positioned() {
              BreakBetween::Auto
            } else {
              child_style.break_before
            };
            if is_forced_page_break(child_break_before, include_always) {
              let boundary_idx = start_line.saturating_sub(1) as usize;
              if let Some(req) = boundary_reqs.get_mut(boundary_idx) {
                record_boundary(
                  req,
                  break_side_hint(child_break_before, page_progression_is_ltr),
                );
              }
            }

            let child_break_after = if child_style.position.is_absolutely_positioned() {
              BreakBetween::Auto
            } else {
              child_style.break_after
            };
            if is_forced_page_break(child_break_after, include_always) {
              let boundary_idx = end_line.saturating_sub(1) as usize;
              if let Some(req) = boundary_reqs.get_mut(boundary_idx) {
                record_boundary(
                  req,
                  break_side_hint(child_break_after, page_progression_is_ltr),
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
    if node_is_row_flex_container {
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

          let child_break_before = if child_style.position.is_absolutely_positioned() {
            BreakBetween::Auto
          } else {
            child_style.break_before
          };
          if is_forced_page_break(child_break_before, include_always) {
            if let Some(req) = boundary_reqs.get_mut(line_idx) {
              record_boundary(
                req,
                break_side_hint(child_break_before, page_progression_is_ltr),
              );
            }
          }

          let child_break_after = if child_style.position.is_absolutely_positioned() {
            BreakBetween::Auto
          } else {
            child_style.break_after
          };
          if is_forced_page_break(child_break_after, include_always) {
            if let Some(req) = boundary_reqs.get_mut(line_idx + 1) {
              record_boundary(
                req,
                break_side_hint(child_break_after, page_progression_is_ltr),
              );
            }
          }
        }

        if boundary_reqs.iter().any(|req| req.forced) {
          // Align boundaries to the end edge of each preceding flex line so breaks never land after
          // a `row-gap`/`align-content` gap and accidentally create gap-only pages.
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

    // Break propagation must be based on the first/last in-flow child, excluding absolutely
    // positioned descendants (CSS Break 3 §4.1).
    let mut first_in_flow_child: Option<usize> = None;
    let mut last_in_flow_child: Option<usize> = None;
    let mut next_in_flow: Vec<Option<usize>> = vec![None; node.children.len()];
    {
      let mut last_seen: Option<usize> = None;
      for (idx, child) in node.children.iter().enumerate().rev() {
        let child_style = child
          .style
          .as_ref()
          .map(|s| s.as_ref())
          .unwrap_or(default_style);
        if child_style.position.is_in_flow() {
          if last_in_flow_child.is_none() {
            last_in_flow_child = Some(idx);
          }
          next_in_flow[idx] = last_seen;
          last_seen = Some(idx);
          first_in_flow_child = Some(idx);
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
      let next_in_flow_idx = next_in_flow.get(idx).copied().flatten();
      let next_style = next_in_flow_idx
        .and_then(|next_idx| node.children.get(next_idx))
        .and_then(|c| c.style.as_ref())
        .map(|s| s.as_ref())
        .unwrap_or(default_style);

      let child_break_before = if child_style.position.is_absolutely_positioned() {
        BreakBetween::Auto
      } else {
        child_style.break_before
      };
      let child_break_after = if child_style.position.is_absolutely_positioned() {
        BreakBetween::Auto
      } else {
        child_style.break_after
      };

      if Some(idx) == first_in_flow_child && is_forced_page_break(child_break_before, include_always) {
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
            page_side: break_side_hint(child_break_before, page_progression_is_ltr),
          });
        }
      }

      if grid_item_break_hints_fallback_to_edges
        && idx > 0
        && idx < in_flow_grid_item_count
        && child_style.position.is_in_flow()
        && is_forced_page_break(child_style.break_before, include_always)
      {
        forced.push(ForcedBoundary {
          position: child_abs_start,
          page_side: break_side_hint(child_break_before, page_progression_is_ltr),
        });
      }

      let break_after = is_forced_page_break(child_break_after, include_always);
      let mut break_before = next_in_flow_idx
        .is_some_and(|_| is_forced_page_break(next_style.break_before, include_always));
      if break_before
        && grid_item_break_hints_fallback_to_edges
        && next_in_flow_idx.is_some_and(|next_idx| next_idx < in_flow_grid_item_count)
      {
        break_before = false;
      }
      if break_after || break_before {
        let next_idx_for_break = next_in_flow_idx.unwrap_or(idx.saturating_add(1));
        let break_from_grid =
          (break_after && idx < grid_item_count) || (break_before && next_idx_for_break < grid_item_count);
        let break_from_flex = flex_line_map.as_ref().is_some_and(|map| {
          let current_is_flex = break_after && map.get(idx).is_some_and(|slot| slot.is_some());
          let next_is_flex =
            break_before && map.get(next_idx_for_break).is_some_and(|slot| slot.is_some());
          current_is_flex || next_is_flex
        });
        if !break_from_grid && !break_from_flex {
          // CSS Break 3 §Adjoining Margins at Breaks: forced breaks truncate margins *before* the
          // break. Align the forced boundary to the preceding child's border-box end so its
          // bottom margin does not become part of the previous page slice.
          let mut boundary = child_abs_end;
          // Forced breaks after the last in-flow child propagate to the end edge of the containing
          // block. This matches `collect_break_opportunities` so side constraints like
          // `break-after: left/right` are applied at the actual page boundary instead of the child's
          // border box end (which can be offset from the parent's end by padding/align-content gaps).
          if break_after && Some(idx) == last_in_flow_child {
            boundary = abs_start + parent_block_size;
          }
          let page_side = if break_before {
            break_side_hint(next_style.break_before, page_progression_is_ltr)
              .or(break_side_hint(child_break_after, page_progression_is_ltr))
          } else {
            break_side_hint(child_break_after, page_progression_is_ltr)
          };
          forced.push(ForcedBoundary {
            position: boundary,
            page_side,
          });
        }
      }
      let skip_parallel_flow_descendants = (suppress_parallel_flow_descendants
        && idx < in_flow_grid_item_count
        && child_style.position.is_in_flow())
        || (suppress_parallel_flow_descendants
          && node_is_row_flex_container
          && child_style.position.is_in_flow())
        || (suppress_parallel_flow_descendants && child_style.position.is_absolutely_positioned());
      if !skip_parallel_flow_descendants {
        collect(
          child,
          child_abs_start,
          forced,
          default_style,
          axis,
          child_block_size,
          suppress_parallel_flow_descendants,
          page_progression_is_ltr,
          include_always,
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
    suppress_parallel_flow_descendants,
    page_progression_is_ltr,
    include_always,
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

fn atomic_containing_for_fragmentainer(
  pos: f32,
  fragmentainer: f32,
  atomic: &[AtomicRange],
) -> Option<AtomicRange> {
  if fragmentainer <= 0.0 {
    return None;
  }
  // `atomic` ranges are filtered per-fragmentainer-size upstream (see `atomic_ranges_for`), using
  // each candidate's `required_fragmentainer_size` (which may exclude absorbed gutters). Some
  // atomic candidates widen their range to include adjacent empty gutters (grid gaps, flex line
  // gaps) so breaks never land inside the gutter; in those cases `range.end - range.start` can
  // exceed the fragmentainer size, but the widened range should still be treated as atomic here.
  atomic_containing(pos, atomic)
}

fn pos_is_inside_atomic_for_fragmentainer(
  pos: f32,
  fragmentainer: f32,
  atomic: &[AtomicRange],
) -> bool {
  atomic_containing_for_fragmentainer(pos, fragmentainer, atomic).is_some()
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
  suppress_break_inside_avoid: bool,
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

  if matches!(node.content, FragmentContent::Replaced { .. })
    && style.display.is_block_level()
    && !style.float.is_floating()
  {
    let required = (end - start).max(0.0);
    candidates.push(AtomicCandidate {
      range: AtomicRange { start, end },
      required_fragmentainer_size: required,
      is_float: false,
    });
  }

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
      is_float: true,
    });
  }

  if !style.float.is_floating()
    && matches!(node.content, FragmentContent::Replaced { .. })
    && style.display.is_block_level()
  {
    let required = (end - start).max(0.0);
    candidates.push(AtomicCandidate {
      range: AtomicRange { start, end },
      required_fragmentainer_size: required,
      is_float: false,
    });
  }

  let table_row_like = is_table_row_like(style.display);
  // CSS Break 3: `break-inside` does not apply to absolutely-positioned boxes.
  let break_inside = if style.position.is_absolutely_positioned() {
    BreakInside::Auto
  } else {
    style.break_inside
  };
  let avoid_inside = avoids_break_inside(break_inside, context);
  // Flex items in row-direction flex containers establish parallel fragmentation flows (similar to
  // grid items). `break-inside: avoid*` should apply *within* the item (i.e. move/suppress its own
  // fragmentation) but must not clamp the parent flow's global boundary selection, or unrelated
  // sibling flex items/lines would be forced onto the next page.
  //
  // We still treat table-row-like fragments as atomic: they are not flex items and must remain
  // unbroken in the parent flow when they fit.
  if table_row_like || (avoid_inside && !suppress_break_inside_avoid) {
    let required = (end - start).max(0.0);
    candidates.push(AtomicCandidate {
      range: AtomicRange { start, end },
      required_fragmentainer_size: required,
      is_float: false,
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
            is_float: false,
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
            is_float: false,
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
  suppress_break_inside_avoid: bool,
) {
  collect_atomic_candidate_for_node(
    node,
    abs_start,
    axis,
    parent_block_size,
    candidates,
    context,
    suppress_break_inside_avoid,
  );

  let node_block_size = axis.block_size(&node.bounds);

  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  let node_is_row_flex_container = is_row_flex_container(style);
  if style.float.is_floating() {
    return;
  }
  let grid_items = if matches!(style.display, Display::Grid | Display::InlineGrid) {
    node.grid_fragmentation.as_ref()
  } else {
    None
  };
  let in_flow_grid_item_count = grid_items
    .as_ref()
    .map(|info| info.items.len().min(node.children.len()))
    .unwrap_or(0);

  for (idx, child) in node.children.iter().enumerate() {
    let child_style = child
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let child_suppress_break_inside_avoid = matches!(context, FragmentationContext::Page)
      && node_is_row_flex_container
      && is_in_flow_flex_child(&child.content, child_style);
    let skip_descendants = (idx < in_flow_grid_item_count && child_style.position.is_in_flow())
      || grid_items
        .and_then(|info| info.items.get(idx))
        .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
    if skip_descendants {
      // In-flow grid items and single-track (parallel-flow) grid items are fragmented specially and
      // should not contribute descendant break opportunities to their container. However, the grid
      // item box itself can still be atomic (e.g. `break-inside: avoid-*`) and must participate in
      // boundary selection.
      collect_atomic_candidate_for_node(
        child,
        child_abs_start,
        axis,
        node_block_size,
        candidates,
        context,
        child_suppress_break_inside_avoid,
      );
      continue;
    }

    collect_atomic_candidates_with_axis(
      child,
      child_abs_start,
      candidates,
      axis,
      node_block_size,
      context,
      child_suppress_break_inside_avoid,
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
  let table_row_like = is_table_row_like(style.display);
  let avoid_inside = avoids_break_inside(style.break_inside, context) || table_row_like;
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
  collect_atomic_ranges_with_axis(
    node,
    abs_start,
    ranges,
    &axis,
    axis.block_size(&node.bounds),
    context,
    fragmentainer_size,
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

  let default_style = default_style();
  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  if style.float.is_floating() {
    return;
  }
  let grid_items = if matches!(style.display, Display::Grid | Display::InlineGrid) {
    node.grid_fragmentation.as_ref()
  } else {
    None
  };
  let in_flow_grid_item_count = grid_items
    .as_ref()
    .map(|info| info.items.len().min(node.children.len()))
    .unwrap_or(0);

  for (idx, child) in node.children.iter().enumerate() {
    let child_style = child
      .style
      .as_ref()
      .map(|s| s.as_ref())
      .unwrap_or(default_style);
    let skip_descendants = (idx < in_flow_grid_item_count && child_style.position.is_in_flow())
      || grid_items
        .and_then(|info| info.items.get(idx))
        .is_some_and(|placement| grid_item_spans_single_track(placement, axis));
    if skip_descendants {
      continue;
    }

    let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
    collect_atomic_ranges_with_axis(
      child,
      child_abs_start,
      ranges,
      axis,
      node_block_size,
      context,
      fragmentainer_size,
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
  collect_atomic_ranges_with_axis(
    node,
    abs_start,
    ranges,
    &axis,
    axis.block_size(&node.bounds),
    context,
    fragmentainer_size,
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
  use crate::style::float::Float;
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::fragment_tree::{BlockFragmentMetadata, GridFragmentationInfo, GridTrackRanges};
  use std::sync::Arc;
  use std::time::{Duration, Instant};

  fn default_axes() -> FragmentAxes {
    FragmentAxes::from_writing_mode_and_direction(WritingMode::HorizontalTb, Direction::Ltr)
  }

  fn box_id(node: &FragmentNode) -> Option<usize> {
    match node.content {
      FragmentContent::Block { box_id } => box_id,
      _ => None,
    }
  }

  #[test]
  fn block_level_replaced_is_not_split_even_when_early_sibling_breaks_are_disallowed() {
    // Regression: block-level replaced fragments (e.g. `display: block` images/canvas/SVG) must not
    // be sliced across fragmentainers when they fit wholly on the next fragmentainer.
    //
    // This fixture triggers the "early sibling break" heuristic: there is a long gap between the
    // first sibling and the replaced fragment, so the normal between-sibling break opportunity
    // occurs well before the fragmentainer limit. Without treating the replaced fragment as atomic,
    // pagination falls back to the limit and clips the replaced content.
    let fragmentainer_size = 100.0;

    let first = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 10.0), vec![]);

    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Block;
    let replaced_style = Arc::new(replaced_style);

    // Starts far before the 100px fragmentainer limit (at 50px) but extends past it (to 110px). The
    // element itself is only 60px tall, so it would fit on the next fragmentainer.
    let mut replaced =
      FragmentNode::new_replaced(Rect::from_xywh(0.0, 50.0, 100.0, 60.0), ReplacedType::Canvas);
    replaced.style = Some(replaced_style);

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 110.0),
      vec![first, replaced],
    );

    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .expect("boundaries");

    let first_boundary = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);

    assert!(
      (first_boundary - 50.0).abs() < BREAK_EPSILON,
      "expected the replaced fragment to be moved to the next fragmentainer (break at 50px), got {first_boundary} (boundaries={boundaries:?})"
    );
    assert!(
      boundaries
        .iter()
        .all(|b| (*b - 100.0).abs() > BREAK_EPSILON || *b <= BREAK_EPSILON),
      "no boundary should fall inside the replaced fragment (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn inline_replaced_inside_line_does_not_create_atomic_pagination() {
    // Negative regression test: inline replaced fragments should not become atomic candidates. If
    // they did, their vertical range could clamp the fragmentainer limit *inside* a line box,
    // forcing a boundary that produces an empty first fragmentainer (blank page).
    let fragmentainer_size = 100.0;

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    let inline_style = Arc::new(inline_style);

    // Inline replaced content positioned within the line box. It intentionally overflows the line's
    // own bounds so that (incorrect) atomic handling would clamp the first boundary to 60px.
    let mut inline_replaced =
      FragmentNode::new_replaced(Rect::from_xywh(0.0, 60.0, 100.0, 60.0), ReplacedType::Canvas);
    inline_replaced.style = Some(inline_style);

    let line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 0.0, 100.0, 80.0),
      0.0,
      vec![inline_replaced],
    );

    // Trailing block content so pagination produces multiple fragments.
    let trailing = FragmentNode::new_block(Rect::from_xywh(0.0, 80.0, 100.0, 80.0), vec![]);

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 160.0),
      vec![line, trailing],
    );

    let options = FragmentationOptions::new(fragmentainer_size);
    let fragments = fragment_tree(&root, &options).expect("fragment tree");
    assert!(
      fragments.len() >= 2,
      "expected multi-fragment output, got {fragments:?}"
    );
    assert!(
      !fragments[0].children.is_empty(),
      "expected the first fragmentainer to contain content (inline replaced must not force an early atomic boundary)"
    );
    assert!(
      matches!(fragments[0].children[0].content, FragmentContent::Line { .. }),
      "expected the first fragmentainer to contain the line box"
    );
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
  fn block_level_replaced_is_pushed_to_next_fragmentainer() {
    // Regression: block-level replaced content must not be sliced by a fragmentainer limit when it
    // can fit on the next fragmentainer.
    //
    // Layout:
    // - block A: 0..80
    // - replaced: 80..110 (height 30)
    // Fragmentainer size: 100
    //
    // Without treating the replaced fragment as atomic, the pagination boundary selection prefers
    // the fragmentainer limit (100) over the early between-sibling break at 80, slicing the
    // replaced box.
    let leading = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 80.0), vec![]);

    let mut replaced =
      FragmentNode::new_replaced(Rect::from_xywh(0.0, 80.0, 100.0, 30.0), ReplacedType::Canvas);
    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Block;
    replaced.style = Some(Arc::new(replaced_style));

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 110.0),
      vec![leading, replaced],
    );
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(100.0),
    );
    let total_extent = analyzer.content_extent().max(100.0);
    let boundaries = analyzer.boundaries(100.0, total_extent).unwrap();
    let first_boundary = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_boundary - 80.0).abs() < BREAK_EPSILON,
      "expected break before block-level replaced fragment, got {first_boundary} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn inline_replaced_inside_line_is_not_treated_as_atomic() {
    // Regression guard: inline replaced fragments inside a line box must not be modeled as atomic
    // ranges in the pagination flow. Line boxes are already indivisible; introducing atomic ranges
    // inside them could clamp boundaries to mid-line child starts (illegal breakpoints).
    let mut replaced =
      FragmentNode::new_replaced(Rect::from_xywh(0.0, 5.0, 100.0, 20.0), ReplacedType::Canvas);
    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Inline;
    replaced.style = Some(Arc::new(replaced_style));

    let line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 80.0, 100.0, 30.0),
      24.0,
      vec![replaced],
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 110.0), vec![line]);

    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(100.0),
    );
    let total_extent = analyzer.content_extent().max(100.0);
    let boundaries = analyzer.boundaries(100.0, total_extent).unwrap();
    let first_boundary = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_boundary - 100.0).abs() < BREAK_EPSILON,
      "expected boundary to remain at the fragmentainer limit (not clamped to inline replaced child start), got {first_boundary} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn balanced_boundaries_can_break_inside_sibling_gaps() {
    // Between-sibling break opportunities can span a gap (e.g. collapsed margins). When the
    // fragmentainer limit falls inside that gap, the boundary selection should be able to break at
    // the limit (truncating the adjoining margin space) instead of picking an earlier break point.
    //
    // This regression models the MDN multicol example where 4 equal-height blocks should balance
    // 2-per-column. The ideal height (384/2=192) lands inside the gap between blocks 2 and 3.
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    let style = Arc::new(style);

    fn block(style: &Arc<ComputedStyle>, id: usize, y: f32) -> FragmentNode {
      let mut node =
        FragmentNode::new_block_with_id(Rect::from_xywh(0.0, y, 100.0, 84.0), id, vec![]);
      node.style = Some(style.clone());
      node
    }

    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 384.0),
      vec![
        block(&style, 1, 0.0),
        block(&style, 2, 100.0),
        block(&style, 3, 200.0),
        block(&style, 4, 300.0),
      ],
      style.clone(),
    );
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Column,
      default_axes(),
      false,
      None,
    );
    let boundaries = analyzer
      .balanced_boundaries(2, 200.0, analyzer.content_extent())
      .expect("balanced boundaries");
    assert_eq!(boundaries, vec![0.0, 192.0, 384.0]);
  }

  #[test]
  fn sibling_gap_boundary_is_not_selected_before_gap_start_due_to_epsilon() {
    // Regression: break opportunities that start just after the fragmentainer limit (within
    // `BREAK_EPSILON`) should snap forward to the opportunity start, not backward to the limit.
    //
    // Without this, pagination/columns can end up slicing the preceding box and creating a
    // near-zero continuation fragment on the next fragmentainer, which then interferes with margin
    // normalization (as seen in the MDN multicol guide fixture).
    let first =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.005), 1, vec![]);
    let second =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 150.0, 100.0, 10.0), 2, vec![]);
    let root =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 160.0), vec![first, second]);
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(100.0),
    );
    let total_extent = analyzer.content_extent().max(100.0);
    let boundaries = analyzer.boundaries(100.0, total_extent).unwrap();

    let first_boundary = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_boundary - 100.005).abs() < 0.001,
      "expected boundary at the sibling gap start (100.005), got {first_boundary} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn normalize_fragment_margins_truncates_leading_margins_after_unforced_break() {
    let axis = axis_from_fragment_axes(default_axes());
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    let style = Arc::new(style);

    fn child(style: &Arc<ComputedStyle>, id: usize, y: f32) -> FragmentNode {
      let mut node =
        FragmentNode::new_block_with_id(Rect::from_xywh(0.0, y, 100.0, 84.0), id, vec![]);
      node.style = Some(style.clone());
      node.block_metadata = Some(BlockFragmentMetadata {
        margin_top: 16.0,
        margin_bottom: 16.0,
        clipped_top: false,
        clipped_bottom: false,
      });
      node
    }

    // Model a continuation fragment that begins inside the gap before the first in-flow block
    // (e.g. margin space preceding the first box in the fragment). Unforced breaks truncate this
    // leading margin, so the first box should start at the origin.
    let mut fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 284.0),
      vec![
        child(&style, 1, 16.0),
        child(&style, 2, 116.0),
        child(&style, 3, 216.0),
      ],
      style,
    );
    normalize_fragment_margins(&mut fragment, false, true, false, false, &axis);

    let positions: Vec<f32> = fragment.children.iter().map(|c| c.bounds.y()).collect();
    assert_eq!(positions, vec![0.0, 100.0, 200.0]);
  }

  #[test]
  fn normalize_fragment_margins_preserves_leading_margins_after_forced_break() {
    let axis = axis_from_fragment_axes(default_axes());
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    let style = Arc::new(style);

    fn child(style: &Arc<ComputedStyle>, id: usize, y: f32) -> FragmentNode {
      let mut node =
        FragmentNode::new_block_with_id(Rect::from_xywh(0.0, y, 100.0, 84.0), id, vec![]);
      node.style = Some(style.clone());
      node.block_metadata = Some(BlockFragmentMetadata {
        margin_top: 16.0,
        margin_bottom: 16.0,
        clipped_top: false,
        clipped_bottom: false,
      });
      node
    }

    // Forced breaks preserve margins after the break, so continuation fragments should keep the
    // leading top margin of the first in-flow block.
    let mut fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 284.0),
      vec![
        child(&style, 1, 0.0),
        child(&style, 2, 100.0),
        child(&style, 3, 200.0),
      ],
      style,
    );
    normalize_fragment_margins(&mut fragment, false, true, true, false, &axis);

    let positions: Vec<f32> = fragment.children.iter().map(|c| c.bounds.y()).collect();
    assert_eq!(positions, vec![16.0, 116.0, 216.0]);
  }

  #[test]
  fn normalize_fragment_margins_truncates_trailing_margins_before_break() {
    let axis = axis_from_fragment_axes(default_axes());
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    let style = Arc::new(style);

    let mut child =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 84.0), 1, vec![]);
    child.style = Some(style.clone());
    child.block_metadata = Some(BlockFragmentMetadata {
      margin_top: 16.0,
      margin_bottom: 16.0,
      clipped_top: false,
      clipped_bottom: false,
    });

    let mut fragment =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![child], style);
    normalize_fragment_margins(&mut fragment, true, false, false, false, &axis);

    assert!(
      (fragment.bounds.height() - 84.0).abs() < 0.01,
      "expected trailing margins to be truncated at an unforced break (bounds={:?})",
      fragment.bounds
    );
  }

  #[test]
  fn normalize_fragment_margins_truncates_trailing_margins_before_forced_break() {
    let axis = axis_from_fragment_axes(default_axes());
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    let style = Arc::new(style);

    let mut child =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 84.0), 1, vec![]);
    child.style = Some(style.clone());
    child.block_metadata = Some(BlockFragmentMetadata {
      margin_top: 16.0,
      margin_bottom: 16.0,
      clipped_top: false,
      clipped_bottom: false,
    });

    let mut fragment =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![child], style);
    normalize_fragment_margins(&mut fragment, true, false, false, true, &axis);

    assert!(
      (fragment.bounds.height() - 84.0).abs() < 0.01,
      "expected trailing margins to be truncated even when the break is forced (bounds={:?})",
      fragment.bounds
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
  fn grid_item_page_side_breaks_map_to_track_boundaries() {
    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut first_style = ComputedStyle::default();
    first_style.display = Display::Block;
    first_style.break_after = BreakBetween::Left;
    let first_style = Arc::new(first_style);

    let mut second_style = ComputedStyle::default();
    second_style.display = Display::Block;
    second_style.break_before = BreakBetween::Left;
    let second_style = Arc::new(second_style);

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![
        FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 60.0), vec![], first_style),
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 60.0, 100.0, 60.0),
          vec![],
          second_style,
        ),
      ],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 60.0), (60.0, 120.0)],
      columns: Vec::new(),
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
          row_start: 2,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let forced = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &grid,
      0.0,
      default_axes(),
      true,
    );
    let Some(boundary) = forced
      .iter()
      .find(|boundary| (boundary.position - 60.0).abs() < BREAK_EPSILON)
    else {
      panic!("expected forced boundary at the 60px track edge, got {forced:?}");
    };
    assert_eq!(boundary.page_side, Some(PageSide::Left));
  }

  #[test]
  fn grid_item_page_side_forced_break_is_mapped_to_track_boundary() {
    let axes = default_axes();
    let track_boundary = 50.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut breaking_style = ComputedStyle::default();
    breaking_style.display = Display::Block;
    breaking_style.break_after = BreakBetween::Left;
    let breaking_style = Arc::new(breaking_style);

    // Item 1 sits in the first track but does not fill the row band (e.g. aligned start), so the
    // forced break must map to the track boundary rather than the item's own border-box end.
    let mut item1 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
      vec![],
      breaking_style,
    );
    item1.content = FragmentContent::Block { box_id: Some(1) };

    let mut item2 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 60.0, 100.0, 50.0),
      vec![],
      item_style,
    );
    item2.content = FragmentContent::Block { box_id: Some(2) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 110.0),
      vec![item1, item2],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 50.0), (60.0, 110.0)],
      columns: Vec::new(),
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
          row_start: 2,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let forced = collect_forced_boundaries_for_pagination_with_axes(&grid, 0.0, axes);
    assert_eq!(forced.len(), 1, "forced={forced:?}");
    let boundary = &forced[0];
    assert!(
      (boundary.position - track_boundary).abs() < BREAK_EPSILON,
      "expected forced boundary at the end edge of the first row track ({track_boundary}), got {boundary:?}"
    );
    assert_eq!(boundary.page_side, Some(PageSide::Left));
  }

  #[test]
  fn grid_item_recto_verso_side_hints_resolve_using_page_progression_direction() {
    let axes = default_axes();
    let track_boundary = 50.0;

    fn make_grid(break_after: BreakBetween) -> FragmentNode {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = Display::Grid;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.display = Display::Block;
      let item_style = Arc::new(item_style);

      let mut breaking_style = ComputedStyle::default();
      breaking_style.display = Display::Block;
      breaking_style.break_after = break_after;
      let breaking_style = Arc::new(breaking_style);

      let mut item1 = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
        vec![],
        breaking_style,
      );
      item1.content = FragmentContent::Block { box_id: Some(1) };

      let mut item2 = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 60.0, 100.0, 50.0),
        vec![],
        item_style,
      );
      item2.content = FragmentContent::Block { box_id: Some(2) };

      let mut grid = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 110.0),
        vec![item1, item2],
        grid_style,
      );
      grid.grid_tracks = Some(Arc::new(GridTrackRanges {
        rows: vec![(0.0, 50.0), (60.0, 110.0)],
        columns: Vec::new(),
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
            row_start: 2,
            row_end: 3,
            column_start: 1,
            column_end: 2,
          },
        ],
      }));

      grid
    }

    let recto = make_grid(BreakBetween::Recto);
    let forced_ltr = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &recto,
      0.0,
      axes,
      true,
    );
    assert_eq!(forced_ltr.len(), 1, "forced_ltr={forced_ltr:?}");
    assert!(
      (forced_ltr[0].position - track_boundary).abs() < BREAK_EPSILON,
      "forced_ltr={forced_ltr:?}"
    );
    assert_eq!(forced_ltr[0].page_side, Some(PageSide::Right));

    let forced_rtl = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &recto,
      0.0,
      axes,
      false,
    );
    assert_eq!(forced_rtl.len(), 1, "forced_rtl={forced_rtl:?}");
    assert!(
      (forced_rtl[0].position - track_boundary).abs() < BREAK_EPSILON,
      "forced_rtl={forced_rtl:?}"
    );
    assert_eq!(forced_rtl[0].page_side, Some(PageSide::Left));

    let verso = make_grid(BreakBetween::Verso);
    let forced_ltr = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &verso,
      0.0,
      axes,
      true,
    );
    assert_eq!(forced_ltr.len(), 1, "forced_ltr={forced_ltr:?}");
    assert!(
      (forced_ltr[0].position - track_boundary).abs() < BREAK_EPSILON,
      "forced_ltr={forced_ltr:?}"
    );
    assert_eq!(forced_ltr[0].page_side, Some(PageSide::Left));

    let forced_rtl = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &verso,
      0.0,
      axes,
      false,
    );
    assert_eq!(forced_rtl.len(), 1, "forced_rtl={forced_rtl:?}");
    assert!(
      (forced_rtl[0].position - track_boundary).abs() < BREAK_EPSILON,
      "forced_rtl={forced_rtl:?}"
    );
    assert_eq!(forced_rtl[0].page_side, Some(PageSide::Right));
  }

  #[test]
  fn row_flex_item_page_side_breaks_map_to_line_boundaries() {
    let mut flex_style = ComputedStyle::default();
    flex_style.display = Display::Flex;
    flex_style.flex_direction = FlexDirection::Row;
    let flex_style = Arc::new(flex_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_after = BreakBetween::Recto;
    let break_style = Arc::new(break_style);

    let flex = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![
        // Line 1: full-width item.
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
          vec![],
          break_style,
        ),
        // Line 2: wrap resets main-axis start to 0 (x=0).
        FragmentNode::new_block(Rect::from_xywh(0.0, 40.0, 100.0, 60.0), vec![]),
      ],
      flex_style,
    );

    let forced = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &flex,
      0.0,
      default_axes(),
      true,
    );
    let Some(boundary) = forced
      .iter()
      .find(|boundary| (boundary.position - 40.0).abs() < BREAK_EPSILON)
    else {
      panic!("expected forced boundary at the first flex line end (40px), got {forced:?}");
    };
    assert_eq!(boundary.page_side, Some(PageSide::Right));

    // Ensure recto/verso mapping respects page progression direction.
    let forced_rtl = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &flex,
      0.0,
      default_axes(),
      false,
    );
    let Some(boundary_rtl) = forced_rtl
      .iter()
      .find(|boundary| (boundary.position - 40.0).abs() < BREAK_EPSILON)
    else {
      panic!("expected forced boundary at the first flex line end (40px), got {forced_rtl:?}");
    };
    assert_eq!(boundary_rtl.page_side, Some(PageSide::Left));
  }

  #[test]
  fn row_flex_item_page_side_forced_break_is_mapped_to_flex_line_boundary() {
    let axes = default_axes();
    let line_boundary = 20.0;

    let mut flex_style = ComputedStyle::default();
    flex_style.display = Display::Flex;
    flex_style.flex_direction = FlexDirection::Row;
    let flex_style = Arc::new(flex_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut breaking_style = ComputedStyle::default();
    breaking_style.display = Display::Block;
    breaking_style.break_after = BreakBetween::Right;
    let breaking_style = Arc::new(breaking_style);

    // First line: two items laid out in row direction.
    // - The first (breaking) item is shorter in the block axis.
    // - The second item extends the line's block-end edge to 20px.
    let mut item1 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 60.0, 10.0),
      vec![],
      breaking_style,
    );
    item1.content = FragmentContent::Block { box_id: Some(1) };

    let mut item2 = FragmentNode::new_block_styled(
      Rect::from_xywh(60.0, 0.0, 40.0, 20.0),
      vec![],
      Arc::clone(&item_style),
    );
    item2.content = FragmentContent::Block { box_id: Some(2) };

    // Second line: a wrapped item whose main-axis start resets, placed after a 10px gap.
    let mut item3 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 30.0, 100.0, 20.0),
      vec![],
      item_style,
    );
    item3.content = FragmentContent::Block { box_id: Some(3) };

    let flex = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 50.0),
      vec![item1, item2, item3],
      flex_style,
    );

    let forced = collect_forced_boundaries_for_pagination_with_axes(&flex, 0.0, axes);
    assert_eq!(forced.len(), 1, "forced={forced:?}");
    let boundary = &forced[0];
    assert!(
      (boundary.position - line_boundary).abs() < BREAK_EPSILON,
      "expected forced boundary at the end edge of the first flex line ({line_boundary}), got {boundary:?}"
    );
    assert_eq!(boundary.page_side, Some(PageSide::Right));
  }

  #[test]
  fn conflicting_page_side_breaks_merge_to_unspecified() {
    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut left_style = ComputedStyle::default();
    left_style.display = Display::Block;
    left_style.break_after = BreakBetween::Left;
    let left_style = Arc::new(left_style);

    let mut right_style = ComputedStyle::default();
    right_style.display = Display::Block;
    right_style.break_before = BreakBetween::Right;
    let right_style = Arc::new(right_style);

    let mut left_again_style = ComputedStyle::default();
    left_again_style.display = Display::Block;
    left_again_style.break_before = BreakBetween::Left;
    let left_again_style = Arc::new(left_again_style);

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![
        FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 60.0), vec![], left_style),
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 60.0, 100.0, 60.0),
          vec![],
          right_style,
        ),
        FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 60.0, 100.0, 60.0),
          vec![],
          left_again_style,
        ),
      ],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 60.0), (60.0, 120.0)],
      columns: Vec::new(),
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
          row_start: 2,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
        GridItemFragmentationData {
          box_id: 3,
          row_start: 2,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let forced = collect_forced_boundaries_for_pagination_with_axes_and_page_progression(
      &grid,
      0.0,
      default_axes(),
      true,
    );
    let Some(boundary) = forced
      .iter()
      .find(|boundary| (boundary.position - 60.0).abs() < BREAK_EPSILON)
    else {
      panic!("expected forced boundary at the 60px track edge, got {forced:?}");
    };
    assert_eq!(
      boundary.page_side, None,
      "conflicting side constraints should merge to no side requirement"
    );
  }

  #[test]
  fn forced_break_inside_spanning_grid_item_does_not_split_row_band() {
    let fragmentainer_size = 100.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_after = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    // Item A spans both row tracks and contains a forced page break inside the first row band.
    let mut a_part1 =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 30.0), vec![], break_style);
    a_part1.content = FragmentContent::Block { box_id: Some(10) };
    let mut a_part2 = FragmentNode::new_block(Rect::from_xywh(0.0, 30.0, 100.0, 90.0), vec![]);
    a_part2.content = FragmentContent::Block { box_id: Some(11) };

    let mut item_a = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![a_part1, a_part2],
      Arc::clone(&item_style),
    );
    item_a.content = FragmentContent::Block { box_id: Some(1) };

    // Item B is a normal sibling in the first row track.
    let mut item_b =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 60.0), vec![], item_style);
    item_b.content = FragmentContent::Block { box_id: Some(2) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![item_a, item_b],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 60.0), (60.0, 120.0)],
      columns: Vec::new(),
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![
        GridItemFragmentationData {
          box_id: 1,
          row_start: 1,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
        GridItemFragmentationData {
          box_id: 2,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();
    let forced = collect_forced_boundaries_for_pagination_with_axes(&grid, 0.0, default_axes());

    assert!(
      boundaries
        .iter()
        .all(|b| (*b - 30.0).abs() > BREAK_EPSILON || *b <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not become global forced boundaries: {boundaries:?}"
    );
    assert!(
      forced
        .iter()
        .all(|b| (b.position - 30.0).abs() > BREAK_EPSILON || b.position <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not be reported as forced boundaries: {forced:?}"
    );
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_break - 60.0).abs() < BREAK_EPSILON,
      "expected row band atomicity to force the first break to the 60px track boundary, got {first_break} (boundaries={boundaries:?})"
    );

    // After applying the grid forced-break shift pass, the continuation content after the internal
    // forced break should be translated forward (blank space insertion).
    let mut shifted = grid.clone();
    apply_grid_parallel_flow_forced_break_shifts(
      &mut shifted,
      default_axes(),
      fragmentainer_size,
      FragmentationContext::Page,
    );
    let shifted_part2_start = shifted.children[0].children[1].bounds.y();
    assert!(
      (shifted_part2_start - 100.0).abs() < BREAK_EPSILON,
      "expected the continuation content to be shifted to the next fragmentainer boundary (y≈100), got y={shifted_part2_start}"
    );
  }

  #[test]
  fn avoid_inside_in_spanning_grid_item_does_not_merge_row_band_atomicity() {
    let fragmentainer_size = 100.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut avoid_style = ComputedStyle::default();
    avoid_style.display = Display::Block;
    avoid_style.break_inside = BreakInside::AvoidPage;
    let avoid_style = Arc::new(avoid_style);

    // The avoid-inside block crosses the row boundary at 60px (40→80). If we include it in the
    // grid container's atomic ranges it can merge the row band ranges and prevent breaking between
    // the two 60px tracks.
    let mut avoid_block =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 40.0, 100.0, 40.0), vec![], avoid_style);
    avoid_block.content = FragmentContent::Block { box_id: Some(10) };

    let mut item = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![avoid_block],
      item_style,
    );
    item.content = FragmentContent::Block { box_id: Some(1) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![item],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 60.0), (60.0, 120.0)],
      columns: Vec::new(),
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 1,
        row_end: 3,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);

    assert!(
      (first_break - 60.0).abs() < BREAK_EPSILON,
      "expected row band atomicity to preserve the 60px track boundary, got {first_break} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn forced_column_break_inside_spanning_grid_item_does_not_split_row_band() {
    let fragmentainer_size = 100.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_after = BreakBetween::Column;
    let break_style = Arc::new(break_style);

    // Item A spans both row tracks and contains a forced column break inside the first row band.
    let mut a_part1 =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 30.0), vec![], break_style);
    a_part1.content = FragmentContent::Block { box_id: Some(20) };
    let mut a_part2 = FragmentNode::new_block(Rect::from_xywh(0.0, 30.0, 100.0, 90.0), vec![]);
    a_part2.content = FragmentContent::Block { box_id: Some(21) };

    let mut item_a = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![a_part1, a_part2],
      Arc::clone(&item_style),
    );
    item_a.content = FragmentContent::Block { box_id: Some(3) };

    // Item B is a normal sibling in the first row track.
    let mut item_b =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 60.0), vec![], item_style);
    item_b.content = FragmentContent::Block { box_id: Some(4) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![item_a, item_b],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 60.0), (60.0, 120.0)],
      columns: Vec::new(),
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![
        GridItemFragmentationData {
          box_id: 3,
          row_start: 1,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
        GridItemFragmentationData {
          box_id: 4,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Column,
      default_axes(),
      false,
      None,
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();

    assert!(
      boundaries
        .iter()
        .all(|b| (*b - 30.0).abs() > BREAK_EPSILON || *b <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not become global forced boundaries: {boundaries:?}"
    );
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_break - 60.0).abs() < BREAK_EPSILON,
      "expected row band atomicity to force the first break to the 60px track boundary, got {first_break} (boundaries={boundaries:?})"
    );

    // After applying the grid forced-break shift pass, the continuation content after the internal
    // forced break should be translated forward (blank space insertion).
    let mut shifted = grid.clone();
    apply_grid_parallel_flow_forced_break_shifts(
      &mut shifted,
      default_axes(),
      fragmentainer_size,
      FragmentationContext::Column,
    );
    let shifted_part2_start = shifted.children[0].children[1].bounds.y();
    assert!(
      (shifted_part2_start - 100.0).abs() < BREAK_EPSILON,
      "expected the continuation content to be shifted to the next fragmentainer boundary (y≈100), got y={shifted_part2_start}"
    );
  }

  #[test]
  fn avoid_inside_in_spanning_grid_item_does_not_merge_row_band_atomicity_in_columns() {
    let fragmentainer_size = 100.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut avoid_style = ComputedStyle::default();
    avoid_style.display = Display::Block;
    avoid_style.break_inside = BreakInside::AvoidColumn;
    let avoid_style = Arc::new(avoid_style);

    let mut avoid_block =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 40.0, 100.0, 40.0), vec![], avoid_style);
    avoid_block.content = FragmentContent::Block { box_id: Some(20) };

    let mut item = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![avoid_block],
      item_style,
    );
    item.content = FragmentContent::Block { box_id: Some(1) };

    let mut sibling_style = ComputedStyle::default();
    sibling_style.display = Display::Block;
    let sibling_style = Arc::new(sibling_style);
    let mut sibling = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 60.0),
      vec![],
      sibling_style,
    );
    sibling.content = FragmentContent::Block { box_id: Some(2) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 120.0),
      vec![item, sibling],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 60.0), (60.0, 120.0)],
      columns: Vec::new(),
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![
        GridItemFragmentationData {
          box_id: 1,
          row_start: 1,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
        GridItemFragmentationData {
          box_id: 2,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Column,
      default_axes(),
      false,
      None,
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);

    assert!(
      (first_break - 60.0).abs() < BREAK_EPSILON,
      "expected row band atomicity to preserve the 60px track boundary, got {first_break} (boundaries={boundaries:?})"
    );
  }

  #[test]
  fn forced_break_inside_spanning_grid_item_does_not_split_column_band_in_vertical_writing_mode() {
    let fragmentainer_size = 100.0;
    let axes =
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalLr, Direction::Ltr);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    grid_style.writing_mode = WritingMode::VerticalLr;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    item_style.writing_mode = WritingMode::VerticalLr;
    let item_style = Arc::new(item_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.writing_mode = WritingMode::VerticalLr;
    break_style.break_after = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    // Item A spans both column tracks and contains a forced page break inside the first column band.
    let mut a_part1 =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 30.0, 100.0), vec![], break_style);
    a_part1.content = FragmentContent::Block { box_id: Some(30) };
    let mut a_part2 = FragmentNode::new_block(Rect::from_xywh(30.0, 0.0, 90.0, 100.0), vec![]);
    a_part2.content = FragmentContent::Block { box_id: Some(31) };

    let mut item_a = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 120.0, 100.0),
      vec![a_part1, a_part2],
      Arc::clone(&item_style),
    );
    item_a.content = FragmentContent::Block { box_id: Some(5) };

    // Item B is a normal sibling in the first column track.
    let mut item_b =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 60.0, 100.0), vec![], item_style);
    item_b.content = FragmentContent::Block { box_id: Some(6) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 120.0, 100.0),
      vec![item_a, item_b],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: Vec::new(),
      columns: vec![(0.0, 60.0), (60.0, 120.0)],
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![
        GridItemFragmentationData {
          box_id: 5,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 3,
        },
        GridItemFragmentationData {
          box_id: 6,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      axes,
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();
    let forced = collect_forced_boundaries_for_pagination_with_axes(&grid, 0.0, axes);

    assert!(
      boundaries
        .iter()
        .all(|b| (*b - 30.0).abs() > BREAK_EPSILON || *b <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not become global forced boundaries: {boundaries:?}"
    );
    assert!(
      forced
        .iter()
        .all(|b| (b.position - 30.0).abs() > BREAK_EPSILON || b.position <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not be reported as forced boundaries: {forced:?}"
    );
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_break - 60.0).abs() < BREAK_EPSILON,
      "expected column band atomicity to force the first break to the 60px track boundary, got {first_break} (boundaries={boundaries:?})"
    );

    // After applying the grid forced-break shift pass, the continuation content after the internal
    // forced break should be translated forward (blank space insertion).
    let mut shifted = grid.clone();
    apply_grid_parallel_flow_forced_break_shifts(
      &mut shifted,
      axes,
      fragmentainer_size,
      FragmentationContext::Page,
    );
    let shifted_part2_start = shifted.children[0].children[1].bounds.x();
    assert!(
      (shifted_part2_start - 100.0).abs() < BREAK_EPSILON,
      "expected the continuation content to be shifted to the next fragmentainer boundary (x≈100), got x={shifted_part2_start}"
    );
  }

  #[test]
  fn grid_item_break_hints_are_honoured_without_grid_tracks() {
    // Regression: some fragment trees carry only `grid_fragmentation` placement info (used to
    // identify in-flow grid items) without physical `grid_tracks` ranges. In that case, the
    // authored break hints on the grid items themselves must still be treated as break
    // opportunities at the item boundaries.
    let fragmentainer_size = 100.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut first_style = ComputedStyle::default();
    first_style.display = Display::Block;
    first_style.break_after = BreakBetween::Page;
    let first_style = Arc::new(first_style);

    let mut first =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 100.0, 20.0), vec![], first_style);
    first.content = FragmentContent::Block { box_id: Some(1) };

    let mut second_style = ComputedStyle::default();
    second_style.display = Display::Block;
    let second_style = Arc::new(second_style);

    let mut second =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 20.0, 100.0, 20.0), vec![], second_style);
    second.content = FragmentContent::Block { box_id: Some(2) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
      vec![first, second],
      grid_style,
    );
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
          row_start: 2,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer.boundaries(fragmentainer_size, total_extent).unwrap();

    assert!(
      analyzer.is_forced_break_at(20.0),
      "expected a forced break opportunity at the first grid item boundary (pos=20), got {boundaries:?}"
    );
    assert!(
      boundaries
        .iter()
        .any(|b| (*b - 20.0).abs() < BREAK_EPSILON && *b > BREAK_EPSILON),
      "expected pagination to include a boundary at the first grid item (pos=20), got {boundaries:?}"
    );
  }

  #[test]
  fn forced_breaks_inside_grid_items_are_suppressed_without_grid_tracks() {
    // Some fragment trees carry only `grid_fragmentation` placement info (used to identify in-flow
    // grid item children) without `grid_tracks` coordinate ranges. Forced breaks inside such items
    // must still be suppressed so they don't become global forced boundaries.
    let fragmentainer_size = 40.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_before = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    let first = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 25.0), vec![]);
    let second =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 25.0, 100.0, 5.0), vec![], break_style);
    let item = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 60.0),
      1,
      vec![first, second],
    );

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 60.0),
      vec![item],
      grid_style,
    );
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 1,
        row_end: 3,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      default_axes(),
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();

    assert!(
      boundaries
        .iter()
        .all(|b| (*b - 25.0).abs() > BREAK_EPSILON || *b <= BREAK_EPSILON),
      "expected forced breaks inside grid items not to become global forced boundaries, got {boundaries:?}"
    );
    assert!(
      (boundaries[1] - fragmentainer_size).abs() < BREAK_EPSILON,
      "expected the first break to fall at the fragmentainer limit when no forced breaks escape, got {boundaries:?}"
    );
    assert!(
      (boundaries.last().copied().unwrap_or(0.0) - 60.0).abs() < BREAK_EPSILON,
      "expected boundaries to end at the grid content extent (60px), got {boundaries:?}"
    );
  }

  #[test]
  fn grid_item_break_before_is_respected_without_grid_tracks() {
    let fragmentainer_size = 100.0;
    let axes = default_axes();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_before = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    let mut item1 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
      vec![],
      Arc::clone(&item_style),
    );
    item1.content = FragmentContent::Block { box_id: Some(1) };

    // Leave a large gap before item2 so pagination would normally prefer the fragmentainer limit
    // over the between-sibling boundary; the forced break must still win.
    let mut item2 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 40.0, 100.0, 10.0),
      vec![],
      break_style,
    );
    item2.content = FragmentContent::Block { box_id: Some(2) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 60.0),
      vec![item1, item2],
      grid_style,
    );
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
          row_start: 2,
          row_end: 3,
          column_start: 1,
          column_end: 2,
        },
      ],
    }));

    let expected = 40.0;

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      axes,
      true,
      Some(fragmentainer_size),
    );
    assert!(
      analyzer.is_forced_break_at(expected),
      "expected a forced break opportunity at {expected}px when `grid_fragmentation` is present without grid tracks"
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer.boundaries(fragmentainer_size, total_extent).unwrap();
    let forced = collect_forced_boundaries_for_pagination_with_axes(&grid, 0.0, axes);

    assert!(
      boundaries
        .iter()
        .any(|b| (*b - expected).abs() < BREAK_EPSILON),
      "expected pagination boundaries to include the forced break at the grid item start (y=40), got {boundaries:?}"
    );
    let Some(forced_boundary) = forced
      .iter()
      .find(|b| (b.position - expected).abs() < BREAK_EPSILON)
    else {
      panic!(
        "expected forced boundaries to include a break at the grid item start (y=40), got {forced:?}"
      );
    };
    assert_eq!(
      forced_boundary.page_side, None,
      "expected `break-before: page` not to set a page side hint"
    );
  }

  #[test]
  fn grid_item_break_before_side_hint_is_preserved_without_grid_tracks() {
    let fragmentainer_size = 100.0;
    let axes = default_axes();

    for (break_before, expected_side) in [
      (BreakBetween::Left, Some(PageSide::Left)),
      (BreakBetween::Right, Some(PageSide::Right)),
    ] {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = Display::Grid;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.display = Display::Block;
      let item_style = Arc::new(item_style);

      let mut break_style = ComputedStyle::default();
      break_style.display = Display::Block;
      break_style.break_before = break_before;
      let break_style = Arc::new(break_style);

      let mut item1 = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
        vec![],
        Arc::clone(&item_style),
      );
      item1.content = FragmentContent::Block { box_id: Some(1) };

      let mut item2 = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 40.0, 100.0, 10.0),
        vec![],
        break_style,
      );
      item2.content = FragmentContent::Block { box_id: Some(2) };

      let mut grid = FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 60.0),
        vec![item1, item2],
        grid_style,
      );
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
            row_start: 2,
            row_end: 3,
            column_start: 1,
            column_end: 2,
          },
        ],
      }));

      let expected = 40.0;
      let forced = collect_forced_boundaries_for_pagination_with_axes(&grid, 0.0, axes);
      let Some(boundary) = forced
        .iter()
        .find(|b| (b.position - expected).abs() < BREAK_EPSILON)
      else {
        panic!(
          "expected forced boundaries to include a break at the grid item start (y=40), got {forced:?}"
        );
      };
      assert_eq!(
        boundary.page_side, expected_side,
        "expected page-side hint {expected_side:?} for `break-before: {break_before:?}`, got {boundary:?}"
      );

      // Also ensure boundary resolution honours the forced break position (even though we cannot
      // encode the page side hint in break opportunities).
      let mut analyzer = FragmentationAnalyzer::new(
        &grid,
        FragmentationContext::Page,
        axes,
        true,
        Some(fragmentainer_size),
      );
      let total_extent = analyzer.content_extent().max(fragmentainer_size);
      let boundaries = analyzer.boundaries(fragmentainer_size, total_extent).unwrap();
      assert!(
        boundaries
          .iter()
          .any(|b| (*b - expected).abs() < BREAK_EPSILON),
        "expected pagination boundaries to include the forced break at the grid item start (y=40), got {boundaries:?}"
      );
    }
  }

  #[test]
  fn forced_break_inside_spanning_grid_item_does_not_split_column_band_in_block_negative_writing_mode(
  ) {
    let fragmentainer_size = 100.0;
    let axes =
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr);
    let axis = axis_from_fragment_axes(axes);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    grid_style.writing_mode = WritingMode::VerticalRl;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    item_style.writing_mode = WritingMode::VerticalRl;
    let item_style = Arc::new(item_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.writing_mode = WritingMode::VerticalRl;
    break_style.break_after = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    // Item A spans both column tracks and contains a forced page break inside the first column band.
    // In `writing-mode: vertical-rl`, block progression is right-to-left, so the first 30px of the
    // flow coordinate system corresponds to the rightmost 30px of the physical box.
    let mut a_part1 =
      FragmentNode::new_block_styled(Rect::from_xywh(90.0, 0.0, 30.0, 100.0), vec![], break_style);
    a_part1.content = FragmentContent::Block { box_id: Some(40) };
    let mut a_part2 = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 90.0, 100.0), vec![]);
    a_part2.content = FragmentContent::Block { box_id: Some(41) };

    let mut item_a = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 120.0, 100.0),
      vec![a_part1, a_part2],
      Arc::clone(&item_style),
    );
    item_a.content = FragmentContent::Block { box_id: Some(7) };

    // Item B is a normal sibling in the first column track.
    let mut item_b =
      FragmentNode::new_block_styled(Rect::from_xywh(60.0, 0.0, 60.0, 100.0), vec![], item_style);
    item_b.content = FragmentContent::Block { box_id: Some(8) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 120.0, 100.0),
      vec![item_a, item_b],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: Vec::new(),
      columns: vec![(0.0, 60.0), (60.0, 120.0)],
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![
        GridItemFragmentationData {
          box_id: 7,
          row_start: 1,
          row_end: 2,
          column_start: 1,
          column_end: 3,
        },
        GridItemFragmentationData {
          box_id: 8,
          row_start: 1,
          row_end: 2,
          column_start: 2,
          column_end: 3,
        },
      ],
    }));

    let mut analyzer = FragmentationAnalyzer::new(
      &grid,
      FragmentationContext::Page,
      axes,
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .unwrap();
    let forced = collect_forced_boundaries_for_pagination_with_axes(&grid, 0.0, axes);

    assert!(
      boundaries
        .iter()
        .all(|b| (*b - 30.0).abs() > BREAK_EPSILON || *b <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not become global forced boundaries: {boundaries:?}"
    );
    assert!(
      forced
        .iter()
        .all(|b| (b.position - 30.0).abs() > BREAK_EPSILON || b.position <= BREAK_EPSILON),
      "forced breaks inside spanning grid items must not be reported as forced boundaries: {forced:?}"
    );
    let first_break = boundaries
      .iter()
      .copied()
      .find(|b| *b > BREAK_EPSILON)
      .unwrap_or(total_extent);
    assert!(
      (first_break - 60.0).abs() < BREAK_EPSILON,
      "expected column band atomicity to force the first break to the 60px track boundary, got {first_break} (boundaries={boundaries:?})"
    );

    // After applying the grid forced-break shift pass, the continuation content after the internal
    // forced break should be shifted so it lands on the next fragmentainer boundary in the *flow*
    // coordinate space.
    let mut shifted = grid.clone();
    apply_grid_parallel_flow_forced_break_shifts(
      &mut shifted,
      axes,
      fragmentainer_size,
      FragmentationContext::Page,
    );
    let shifted_item_a = &shifted.children[0];
    let shifted_part2 = &shifted_item_a.children[1];
    let part2_flow_start = axis
      .flow_range(
        0.0,
        axis.block_size(&shifted_item_a.bounds),
        &shifted_part2.bounds,
      )
      .0;
    assert!(
      (part2_flow_start - 100.0).abs() < BREAK_EPSILON,
      "expected the continuation content to be shifted to the next fragmentainer boundary (flow≈100), got flow={part2_flow_start} (bounds={:?})",
      shifted_part2.bounds
    );
  }

  #[test]
  fn parallel_grid_items_clip_first_fragment_to_remaining_fragmentainer_space() {
    let fragmentainer_size = 100.0;

    let leading = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), vec![]);

    // Create a tall single-track grid item so it must fragment across pages.
    let item_child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![]);
    let mut item =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![item_child]);
    item.content = FragmentContent::Block { box_id: Some(1) };

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 50.0, 100.0, 50.0),
      vec![item],
      Arc::new(grid_style),
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 50.0)],
      columns: Vec::new(),
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 1,
        row_end: 2,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let root =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 250.0), vec![leading, grid]);

    let fragments = fragment_tree(&root, &FragmentationOptions::new(fragmentainer_size)).unwrap();
    assert!(
      fragments.len() >= 2,
      "expected pagination to create multiple pages, got {fragments:?}"
    );

    let first = &fragments[0];
    let grid_first = first
      .children
      .iter()
      .find(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|s| matches!(s.display, Display::Grid))
      })
      .expect("expected grid container to appear on the first page");
    let item_first = grid_first
      .children
      .first()
      .expect("expected grid item fragment on the first page");
    let item_max_y = grid_first.bounds.y() + item_first.bounds.max_y();
    assert!(
      item_max_y <= fragmentainer_size + BREAK_EPSILON,
      "expected the grid item fragment to be clipped to the page height (<= {fragmentainer_size}), got item_max_y={item_max_y} (grid={:?}, item={:?})",
      grid_first.bounds,
      item_first.bounds
    );

    let second = &fragments[1];
    let grid_second = second
      .children
      .iter()
      .find(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|s| matches!(s.display, Display::Grid))
      })
      .expect("expected grid container to appear on the second page");
    let item_second = grid_second
      .children
      .first()
      .expect("expected grid item continuation on the second page");
    assert!(
      item_second.slice_info.slice_offset > BREAK_EPSILON,
      "expected grid item continuation to have non-zero slice offset, got {:?}",
      item_second.slice_info
    );
  }

  #[test]
  fn parallel_grid_items_clip_first_fragment_to_remaining_fragmentainer_space_in_block_negative_writing_mode(
  ) {
    let fragmentainer_size = 100.0;
    let leading_block_size = 50.0;

    let root_style = Arc::new({
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style.writing_mode = WritingMode::VerticalRl;
      style
    });

    // In `writing-mode: vertical-rl`, the block axis is horizontal and progresses right-to-left.
    // The first page (flow range 0..100) corresponds to the rightmost 100px of the root.
    //
    // Place a leading block occupying the first 50px of flow, then place the grid container so it
    // begins 50px into the first fragmentainer.
    let leading = FragmentNode::new_block(Rect::from_xywh(200.0, 0.0, 50.0, 20.0), vec![]);

    let item_child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 20.0), vec![]);
    // Make the item span 200px in the block axis while aligning its end edge to the grid container
    // track (so the physical x-start is negative inside the 50px-wide grid container).
    let mut item =
      FragmentNode::new_block(Rect::from_xywh(-150.0, 0.0, 200.0, 20.0), vec![item_child]);
    item.content = FragmentContent::Block { box_id: Some(1) };

    let grid_style = Arc::new({
      let mut style = ComputedStyle::default();
      style.display = Display::Grid;
      style.writing_mode = WritingMode::VerticalRl;
      style
    });
    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(150.0, 0.0, 50.0, 20.0),
      vec![item],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: Vec::new(),
      columns: vec![(0.0, 50.0)],
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 1,
        row_end: 2,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 250.0, 20.0),
      vec![leading, grid],
      root_style,
    );

    let fragments = fragment_tree(&root, &FragmentationOptions::new(fragmentainer_size)).unwrap();
    assert!(
      fragments.len() >= 2,
      "expected pagination to create multiple pages, got {fragments:?}"
    );

    let first = &fragments[0];
    let grid_first = first
      .children
      .iter()
      .find(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|s| matches!(s.display, Display::Grid))
      })
      .expect("expected grid container to appear on the first page");
    let item_first = grid_first
      .children
      .first()
      .expect("expected grid item fragment on the first page");

    // The first item fragment should only contain the remaining 50px of the fragmentainer after the
    // leading block. Without offset-aware clipping, the item fragment would be treated as a full
    // 100px slice and overflow into the leading content region.
    assert!(
      item_first.bounds.width() <= (fragmentainer_size - leading_block_size) + BREAK_EPSILON,
      "expected the grid item fragment to be clipped to the remaining fragmentainer width (<= {}), got {:?}",
      fragmentainer_size - leading_block_size,
      item_first.bounds
    );
    let item_abs_min_x = grid_first.bounds.x() + item_first.bounds.x();
    assert!(
      item_abs_min_x >= -BREAK_EPSILON,
      "expected the grid item fragment to stay within the page clip window (min_x>=0), got min_x={item_abs_min_x} (grid={:?}, item={:?})",
      grid_first.bounds,
      item_first.bounds
    );

    let second = &fragments[1];
    let grid_second = second
      .children
      .iter()
      .find(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|s| matches!(s.display, Display::Grid))
      })
      .expect("expected grid container to appear on the second page");
    let item_second = grid_second
      .children
      .first()
      .expect("expected grid item continuation on the second page");
    assert!(
      item_second.slice_info.slice_offset > BREAK_EPSILON,
      "expected grid item continuation to have non-zero slice offset, got {:?}",
      item_second.slice_info
    );
  }

  #[test]
  fn parallel_grid_items_clip_first_fragment_when_item_starts_in_continued_grid_fragment() {
    let fragmentainer_size = 100.0;

    // The grid container spans multiple pages; the single-track grid item begins at y=130, so its
    // start is on the second page at an offset of 30px within the fragmentainer slice [100, 200].
    //
    // Regression: parallel grid-item clipping must respect that offset, limiting the first item
    // fragment on the start page to 70px instead of treating it as a full 100px fragment.
    let item_child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![]);
    let mut item =
      FragmentNode::new_block(Rect::from_xywh(0.0, 130.0, 100.0, 200.0), vec![item_child]);
    item.content = FragmentContent::Block { box_id: Some(1) };

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 200.0),
      vec![item],
      Arc::new(grid_style),
    );
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 2,
        row_end: 3,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 400.0), vec![grid]);
    let fragments = fragment_tree(&root, &FragmentationOptions::new(fragmentainer_size)).unwrap();
    assert!(
      fragments.len() >= 3,
      "expected pagination to create multiple pages, got {fragments:?}"
    );

    let start_page = &fragments[1];
    let grid_start = start_page
      .children
      .first()
      .expect("expected grid container to appear on the item start page");
    let item_start = grid_start
      .children
      .first()
      .expect("expected grid item fragment on the start page");
    assert!(
      (item_start.bounds.height() - 70.0).abs() < 0.01,
      "expected item fragment height to equal remaining space (70px), got {:?}",
      item_start.bounds
    );
    assert!(
      item_start.bounds.max_y() <= fragmentainer_size + BREAK_EPSILON,
      "expected item fragment to stay within the page clip window (max_y<=100), got {:?}",
      item_start.bounds
    );

    let next_page = &fragments[2];
    let grid_next = next_page
      .children
      .first()
      .expect("expected grid container to appear on the continuation page");
    let item_next = grid_next
      .children
      .first()
      .expect("expected grid item continuation on the next page");
    assert!(
      (item_next.slice_info.slice_offset - 70.0).abs() < 0.01,
      "expected continuation slice offset to be 70px, got {:?}",
      item_next.slice_info
    );
  }

  #[test]
  fn grid_parallel_flow_forced_break_shifts_respect_item_offset() {
    let axes = default_axes();
    let axis = axis_from_fragment_axes(axes);
    let fragmentainer_size = 100.0;

    let leading = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 80.0), vec![]);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_after = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    let mut first = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 10.0), vec![]);
    first.style = Some(break_style);
    let second = FragmentNode::new_block(Rect::from_xywh(0.0, 10.0, 100.0, 10.0), vec![]);
    let item = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 20.0), vec![first, second]);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 80.0, 100.0, 20.0),
      vec![item],
      Arc::new(grid_style),
    );
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 1,
        row_end: 2,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let mut root =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![leading, grid]);

    apply_grid_parallel_flow_forced_break_shifts(
      &mut root,
      axes,
      fragmentainer_size,
      FragmentationContext::Page,
    );

    let grid = &root.children[1];
    let item = &grid.children[0];
    let shifted_second = &item.children[1];

    assert!(
      (shifted_second.bounds.y() - 20.0).abs() < BREAK_EPSILON,
      "expected second block to shift down by 10px (local y≈20), got y={}",
      shifted_second.bounds.y()
    );

    let root_block_size = axis.block_size(&root.bounds);
    let grid_abs_start = axis.flow_range(0.0, root_block_size, &grid.bounds).0;
    let item_abs_start = axis
      .flow_range(grid_abs_start, axis.block_size(&grid.bounds), &item.bounds)
      .0;
    let second_abs_start = axis
      .flow_range(
        item_abs_start,
        axis.block_size(&item.bounds),
        &shifted_second.bounds,
      )
      .0;
    assert!(
      (second_abs_start - 100.0).abs() < BREAK_EPSILON,
      "expected second block to start at global flow pos 100, got {second_abs_start}"
    );

    let required = grid_item_parallel_flow_required_block_size(
      item,
      axes,
      fragmentainer_size,
      item_abs_start,
      FragmentationContext::Page,
    );
    assert!(
      (required - 30.0).abs() < BREAK_EPSILON,
      "expected required block size to be 30px (20 + 10 blank), got {required}"
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
  fn grid_parallel_flow_forced_break_blank_insertion_uses_absolute_fragmentainer_boundaries() {
    let axes = default_axes();
    let axis = axis_from_fragment_axes(axes);
    let fragmentainer_size = 100.0;

    let leading =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 30.0), 1, vec![]);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_before = BreakBetween::Page;
    let break_style = Arc::new(break_style);

    let mut first =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 90.0), 10, vec![]);
    first.style = Some(Arc::clone(&block_style));

    let mut second =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 90.0, 100.0, 20.0), 11, vec![]);
    second.style = Some(break_style);

    let mut item = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 110.0),
      20,
      vec![first, second],
    );
    item.style = Some(block_style);

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 30.0, 100.0, 110.0),
      vec![item],
      grid_style,
    );
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 20,
        row_start: 1,
        row_end: 2,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let root =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 140.0), vec![leading, grid]);

    fn find_node<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_node(child, id) {
          return Some(found);
        }
      }
      None
    }

    fn find_abs_start(
      node: &FragmentNode,
      id: usize,
      abs_start: f32,
      axis: &FragmentAxis,
      parent_block_size: f32,
    ) -> Option<f32> {
      if node.box_id() == Some(id) {
        return Some(abs_start);
      }
      for child in node.children.iter() {
        let child_abs_start = axis
          .flow_range(abs_start, parent_block_size, &child.bounds)
          .0;
        if let Some(found) = find_abs_start(
          child,
          id,
          child_abs_start,
          axis,
          axis.block_size(&child.bounds),
        ) {
          return Some(found);
        }
      }
      None
    }

    // Apply blank-insertion modelling for forced breaks inside parallel-flow grid items.
    let mut shifted = root.clone();
    apply_grid_parallel_flow_forced_break_shifts(
      &mut shifted,
      axes,
      fragmentainer_size,
      FragmentationContext::Page,
    );

    // The forced break occurs at absolute flow position 120 (= 30 offset + 90 in-flow). With a 100px
    // fragmentainer size, the continuation must start at the next *global* boundary (200), not at
    // 30+100=130.
    let second_abs_start =
      find_abs_start(&shifted, 11, 0.0, &axis, axis.block_size(&shifted.bounds))
        .expect("expected to find forced-break child in fragment tree");
    assert!(
      (second_abs_start - 200.0).abs() < BREAK_EPSILON,
      "expected forced-break continuation to start at the next global page boundary (200), got {second_abs_start}"
    );

    // The grid item must report its required block size using the same absolute-alignment logic so
    // descendants can be clipped onto subsequent fragmentainers.
    let item_node = find_node(&shifted, 20).expect("expected to find grid item");
    let item_abs_start = find_abs_start(&shifted, 20, 0.0, &axis, axis.block_size(&shifted.bounds))
      .expect("expected to find grid item abs start");
    let required = grid_item_parallel_flow_required_block_size(
      item_node,
      axes,
      fragmentainer_size,
      item_abs_start,
      FragmentationContext::Page,
    );
    assert!(
      (required - 190.0).abs() < BREAK_EPSILON,
      "expected 80px of blank insertion to reach the 200px boundary (required=190), got {required}"
    );

    // The overall pagination extent should now span three fragmentainers: [0,100], [100,200], and
    // [200,220].
    let mut analyzer = FragmentationAnalyzer::new(
      &shifted,
      FragmentationContext::Page,
      axes,
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .expect("expected boundaries");
    assert_eq!(
      boundaries.len().saturating_sub(1),
      3,
      "boundaries={boundaries:?}"
    );
    assert!(
      boundaries
        .iter()
        .any(|b| (*b - 200.0).abs() < BREAK_EPSILON),
      "expected a page boundary at 200 after blank insertion, got {boundaries:?}"
    );
  }

  #[test]
  fn float_parallel_flow_trimming_keeps_physical_block_end_fixed_in_block_negative_writing_mode() {
    // Regression: trimming blank space inserted by forced breaks inside floats must preserve the
    // physical block-end edge when block progression is reversed (e.g. `writing-mode: vertical-rl`).
    //
    // In block-negative modes, the flow start edge is the *physical end* (right edge for the X block
    // axis). When we shrink a clipped float fragment based on the children that actually painted in
    // that fragmentainer, we need to shift the physical block-start so the end edge remains fixed.
    let axes =
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr);
    let axis = axis_from_fragment_axes(axes);

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.writing_mode = WritingMode::VerticalRl;
    float_style.float = Float::Left;
    let float_style = Arc::new(float_style);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.writing_mode = WritingMode::VerticalRl;
    let child_style = Arc::new(child_style);

    // Synthetic float:
    // - Border box spans 250px in the block axis.
    // - Descendant overflow (simulating a forced break shift) extends an additional 190px, for a
    //   total logical bounding box of 440px.
    //
    // Clip the float to the "middle page" fragmentainer slice [200, 400]. Only 10px of actual
    // content overlaps this slice; the remaining 190px is blank space inserted by the forced break.
    // Trimming should shrink to 10px while keeping the physical end edge anchored.
    let part1 = FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 0.0, 210.0, 10.0),
      vec![],
      Arc::clone(&child_style),
    );
    // This fragment sits after an injected forced break shift (flow position 400..440), so it
    // contributes to the float's logical bounding box but is outside the middle slice.
    let part2 = FragmentNode::new_block_styled(
      Rect::from_xywh(-190.0, 0.0, 40.0, 10.0),
      vec![],
      child_style,
    );

    let mut float = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 250.0, 10.0),
      vec![part1, part2],
      float_style,
    );
    // `FragmentNode::new_*` initialises slice metadata for the Y block axis; overwrite to match the
    // X fragmentation axis used by `writing-mode: vertical-rl`.
    float.slice_info = FragmentSliceInfo::single(250.0);

    let clipped = clip_node(
      &float,
      &axis,
      200.0,
      400.0,
      0.0,
      200.0,
      400.0,
      axis.block_size(&float.bounds),
      1,
      3,
      FragmentationContext::Page,
      200.0,
      axes,
    )
    .unwrap()
    .expect("expected float to overlap the clipped fragmentainer slice");

    assert!(
      (clipped.bounds.width() - 10.0).abs() < 0.01,
      "expected trimmed float fragment to shrink to the painted extent (10px), got {:?}",
      clipped.bounds
    );
    assert!(
      (clipped.bounds.x() - 190.0).abs() < 0.01,
      "expected trimmed float fragment to remain anchored to the physical block-end edge (x≈190), got {:?}",
      clipped.bounds
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
  fn column_fragmentation_prefers_between_sibling_break_over_limit() {
    let fragmentainer_size = 100.0;
    let options = FragmentationOptions::new(fragmentainer_size).with_columns(2, 0.0);

    let a = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 60.0),
      1,
      vec![],
    );
    let b = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 80.0, 100.0, 40.0),
      2,
      vec![],
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 120.0), vec![a, b]);

    let fragments = fragment_tree(&root, &options).expect("fragment tree");
    assert_eq!(fragments.len(), 2, "fragments={fragments:#?}");

    let col0_ids: Vec<_> = fragments[0].children.iter().map(box_id).collect();
    assert_eq!(
      col0_ids,
      vec![Some(1)],
      "expected the overflowing block to move to the next column (not be clipped), got col0_ids={col0_ids:?}"
    );

    let col1_ids: Vec<_> = fragments[1].children.iter().map(box_id).collect();
    assert_eq!(
      col1_ids,
      vec![Some(2)],
      "expected the moved block to appear as a whole in the next column, got col1_ids={col1_ids:?}"
    );

    let b_fragment = &fragments[1].children[0];
    assert!(
      b_fragment.slice_info.is_first && b_fragment.slice_info.is_last,
      "expected moved block to be unfragmented, got slice_info={:?}",
      b_fragment.slice_info
    );
    assert!(
      (b_fragment.bounds.y() - 0.0).abs() < 0.01,
      "expected moved block to start at the top of the column, got y={}",
      b_fragment.bounds.y()
    );
    assert!(
      (b_fragment.bounds.height() - 40.0).abs() < 0.01,
      "expected moved block to preserve its full height (40px), got h={}",
      b_fragment.bounds.height()
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

  #[test]
  fn column_fragmentation_allows_early_sibling_breaks_to_avoid_slicing_next_block() {
    let fragmentainer_size = 200.0;

    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    let style = Arc::new(style);

    // The second child starts far enough before the fragmentainer limit (200) that the "prefer the
    // hard limit over early sibling boundaries" heuristic would otherwise trigger and slice the
    // child even though it fits entirely in the next column.
    let mut child1 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 50.0),
      vec![],
      Arc::clone(&style),
    );
    child1.content = FragmentContent::Block { box_id: Some(1) };

    let mut child2 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 100.0, 100.0, 150.0),
      vec![],
      Arc::clone(&style),
    );
    child2.content = FragmentContent::Block { box_id: Some(2) };

    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 250.0),
      vec![child1, child2],
      style,
    );

    let options = FragmentationOptions::new(fragmentainer_size).with_columns(2, 0.0);
    let fragments = fragment_tree(&root, &options).expect("fragment tree");
    assert_eq!(fragments.len(), 2, "expected two column fragments, got {fragments:?}");

    let col0 = &fragments[0];
    let col1 = &fragments[1];

    let col0_ids: Vec<_> = col0.children.iter().map(box_id).collect();
    assert_eq!(
      col0_ids,
      vec![Some(1)],
      "expected the second child to be moved wholly to the next column, got {col0_ids:?}"
    );

    let col1_ids: Vec<_> = col1.children.iter().map(box_id).collect();
    assert_eq!(
      col1_ids,
      vec![Some(2)],
      "expected the second child to appear wholly in the next column, got {col1_ids:?}"
    );

    let child2_fragment = &col1.children[0];
    assert!(
      child2_fragment.slice_info.is_first,
      "expected the moved child to start a fresh slice in the next column, got {:?}",
      child2_fragment.slice_info
    );
    assert!(
      child2_fragment.slice_info.slice_offset.abs() < 0.01,
      "expected the moved child slice to have offset 0 in the next column, got {:?}",
      child2_fragment.slice_info
    );
  }

  #[test]
  fn column_grid_parallel_item_forced_break_does_not_force_container_columns() {
    let fragmentainer_size = 50.0;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    let mut break_style = ComputedStyle::default();
    break_style.display = Display::Block;
    break_style.break_after = BreakBetween::Column;
    let break_style = Arc::new(break_style);

    let mut part_style = ComputedStyle::default();
    part_style.display = Display::Block;
    let part_style = Arc::new(part_style);

    let mut part1 =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 50.0, 10.0), vec![], break_style);
    part1.content = FragmentContent::Block { box_id: Some(11) };

    let mut part2 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 10.0, 50.0, 40.0),
      vec![],
      Arc::clone(&part_style),
    );
    part2.content = FragmentContent::Block { box_id: Some(12) };

    let mut item1 = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      vec![part1, part2],
      Arc::clone(&item_style),
    );
    item1.content = FragmentContent::Block { box_id: Some(1) };

    let mut item2 = FragmentNode::new_block_styled(
      Rect::from_xywh(50.0, 0.0, 50.0, 50.0),
      vec![{
        let mut leaf = FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          vec![],
          Arc::clone(&item_style),
        );
        leaf.content = FragmentContent::Block { box_id: Some(21) };
        leaf
      }],
      item_style,
    );
    item2.content = FragmentContent::Block { box_id: Some(2) };

    let mut root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 50.0),
      vec![item1, item2],
      grid_style,
    );
    root.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 50.0)],
      columns: vec![(0.0, 50.0), (50.0, 100.0)],
    }));
    root.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
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

    // Break opportunities inside the first grid item must not force the grid container's column
    // boundaries. Without parallel-flow suppression, the forced break at flow pos 10 would become
    // a mandatory column break for the whole container.
    let mut analyzer = FragmentationAnalyzer::new(
      &root,
      FragmentationContext::Column,
      default_axes(),
      true,
      Some(fragmentainer_size),
    );
    let total_extent = analyzer.content_extent().max(fragmentainer_size);
    let boundaries = analyzer
      .boundaries(fragmentainer_size, total_extent)
      .expect("boundaries");
    assert!(
      !boundaries
        .iter()
        .any(|b| (*b - 10.0).abs() < BREAK_EPSILON),
      "expected the grid item's forced break to be suppressed from the container flow, got boundaries={boundaries:?}"
    );

    let options = FragmentationOptions::new(fragmentainer_size).with_columns(2, 0.0);
    let fragments = fragment_tree(&root, &options).expect("fragment tree");
    assert_eq!(
      fragments.len(),
      2,
      "expected the forced break inside the parallel grid item to create a continuation column"
    );

    let col0 = &fragments[0];
    let col1 = &fragments[1];

    let col0_ids: Vec<_> = col0.children.iter().map(box_id).collect();
    assert_eq!(
      col0_ids,
      vec![Some(1), Some(2)],
      "expected both grid items to remain in the first column fragment, got {col0_ids:?}"
    );

    let col1_ids: Vec<_> = col1.children.iter().map(box_id).collect();
    assert_eq!(
      col1_ids,
      vec![Some(1)],
      "expected only the breaking grid item to continue in later columns, got {col1_ids:?}"
    );

    let item1_col0 = &col0.children[0];
    let item1_col1 = &col1.children[0];
    let item1_col0_child_ids: Vec<_> = item1_col0.children.iter().map(box_id).collect();
    let item1_col1_child_ids: Vec<_> = item1_col1.children.iter().map(box_id).collect();
    assert_eq!(
      item1_col0_child_ids,
      vec![Some(11)],
      "expected the breaking grid item to contain only pre-break content in the first column, got {item1_col0_child_ids:?}"
    );
    assert_eq!(
      item1_col1_child_ids,
      vec![Some(12)],
      "expected the breaking grid item continuation content to appear in later columns, got {item1_col1_child_ids:?}"
    );
    assert!(
      (item1_col1.children[0].bounds.y() - 0.0).abs() < 0.01,
      "expected continuation content to be rebased to the top of the grid item slice in the next column, got y={}",
      item1_col1.children[0].bounds.y()
    );
  }

  #[test]
  fn column_grid_parallel_item_uses_independent_clipping_path() {
    let axes = default_axes();
    let axis = axis_from_fragment_axes(axes);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    let item_style = Arc::new(item_style);

    // Place the grid item starting partway through the second fragmentainer slice [50, 100] so the
    // "local" fragment index (relative to the item's own fragmentation flow) differs from the
    // global fragment index passed to `clip_node`.
    let mut item = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 60.0, 100.0, 20.0),
      vec![{
        let mut leaf = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 20.0), vec![]);
        leaf.content = FragmentContent::Block { box_id: Some(101) };
        leaf
      }],
      item_style,
    );
    item.content = FragmentContent::Block { box_id: Some(1) };

    let mut grid = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 80.0),
      vec![item],
      grid_style,
    );
    grid.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(60.0, 80.0)],
      columns: vec![(0.0, 100.0)],
    }));
    grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
      items: vec![GridItemFragmentationData {
        box_id: 1,
        row_start: 1,
        row_end: 2,
        column_start: 1,
        column_end: 2,
      }],
    }));

    let clipped = clip_node(
      &grid,
      &axis,
      50.0,
      100.0,
      0.0,
      50.0,
      100.0,
      axis.block_size(&grid.bounds),
      1,
      2,
      FragmentationContext::Column,
      50.0,
      axes,
    )
    .expect("clip")
    .expect("expected grid container to overlap the fragment slice");

    assert_eq!(clipped.children.len(), 1);
    let item_fragment = &clipped.children[0];
    assert_eq!(
      item_fragment.fragment_index, 0,
      "expected the grid item to be clipped via the parallel flow path (local fragment index), got {}",
      item_fragment.fragment_index
    );
  }
}
