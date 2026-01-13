//! This module is a partial implementation of the CSS Grid Level 1 specification
//! <https://www.w3.org/TR/css-grid-1>
use crate::geometry::{AbsoluteAxis, AbstractAxis, InBothAbsAxis};
use crate::geometry::{Line, Point, Rect, Size};
use crate::style::{
  AlignItems, AlignSelf, AvailableSpace, GridTemplateArea, GridTemplateComponent, LengthPercentage,
  Overflow, Position,
};
use crate::tree::{
  Layout, LayoutInput, LayoutOutput, LayoutPartialTreeExt, NodeId, RunMode, SizingMode,
};
use crate::util::debug::debug_log;
#[cfg(not(feature = "std"))]
use crate::util::sys::Map;
use crate::util::sys::{f32_max, GridTrackVec, Vec};
use crate::util::MaybeMath;
use crate::util::{MaybeResolve, ResolveOrZero};
use crate::{
  style_helpers::*, AlignContent, BoxGenerationMode, BoxSizing, CoreStyle, GridContainerStyle,
  GridItemStyle, JustifyContent, LayoutGridContainer, Style, TrackSizingFunction,
};
use crate::{
  sys::{format, DefaultCheapStr},
  tree::LayoutPartialTree,
};
use alignment::{align_and_position_item, align_tracks};
use explicit_grid::{
  compute_explicit_grid_size_in_axis, initialize_grid_tracks, AutoRepeatStrategy,
};
use implicit_grid::compute_grid_size_estimate;
use placement::place_grid_items;
#[cfg(feature = "std")]
use rustc_hash::FxHashMap;
use track_sizing::{
  determine_if_item_crosses_flexible_or_intrinsic_tracks, resolve_item_baselines,
  resolve_item_track_indexes, track_sizing_algorithm,
};
use types::{
  CellOccupancyMatrix, GridItem, GridTrack, GridTrackKind, NamedLineResolver, TrackCounts,
};

// Subgrid overrides rely on thread-local storage to pass resolved track sizes from a parent grid
// container to a subgrid child during the same layout run. This is only available when `std` is
// enabled. In `no_std` builds we fall back to no-ops so that the crate continues to compile.
#[cfg(feature = "std")]
thread_local! {
    static SUBGRID_OVERRIDES: std::cell::RefCell<SubgridOverrideMap> =
        std::cell::RefCell::new(Default::default());
    static SUBGRID_OVERRIDE_DEPTH: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

type Ident = DefaultCheapStr;

#[derive(Clone, Debug)]
struct SubgridAxisOverride {
  track_sizes: Vec<f32>,
  line_names: Vec<Vec<Ident>>, // Always using owned identifiers for line names
  gap: f32,
}

#[derive(Clone, Debug)]
struct SubgridOverride {
  rows: Option<SubgridAxisOverride>,
  columns: Option<SubgridAxisOverride>,
  // Named grid areas inherited from an ancestor `grid-template-areas` declaration, clamped into the
  // subgrid's coordinate space. Used to propagate area-derived implicit line names through nested
  // subgrids per CSS Grid 2 §7.12.1 “subgrid-area-inheritance”.
  template_areas: Vec<GridTemplateArea<Ident>>,
}

#[cfg(feature = "std")]
type SubgridOverrideMap = FxHashMap<NodeId, SubgridOverride>;

#[cfg(not(feature = "std"))]
type SubgridOverrideMap = Map<NodeId, SubgridOverride>;

/// A scoped guard that isolates subgrid overrides for the duration of a layout run.
///
/// Subgrid overrides are stored in a thread-local map keyed by `NodeId`. Layout implementations
/// may be re-entrant (for example when a measure function triggers a nested Taffy layout run on the
/// same thread). Without scoping, nested layout runs can clobber the outer run's overrides (both by
/// clearing the map and due to `NodeId` collisions between independent trees).
///
/// This guard ensures:
/// - Outermost layout runs start with an empty overrides map and clear it on exit.
/// - Nested layout runs swap the current map out and restore it on exit.
#[cfg(feature = "std")]
pub(crate) struct SubgridOverrideGuard {
  previous: Option<SubgridOverrideMap>,
}

#[cfg(feature = "std")]
impl SubgridOverrideGuard {
  pub(crate) fn new() -> Self {
    let depth_before = SUBGRID_OVERRIDE_DEPTH.with(|depth| {
      let prev = depth.get();
      depth.set(prev + 1);
      prev
    });

    if depth_before == 0 {
      // Outermost layout run: clear any stale overrides from a previous run.
      SUBGRID_OVERRIDES.with(|map| map.borrow_mut().clear());
      Self { previous: None }
    } else {
      // Nested layout run: isolate the current overrides map so we don't clobber the outer run.
      let previous = SUBGRID_OVERRIDES.with(|map| std::mem::take(&mut *map.borrow_mut()));
      Self {
        previous: Some(previous),
      }
    }
  }
}

#[cfg(feature = "std")]
impl Drop for SubgridOverrideGuard {
  fn drop(&mut self) {
    SUBGRID_OVERRIDE_DEPTH.with(|depth| {
      let current = depth.get();
      debug_assert!(current > 0);
      depth.set(current.saturating_sub(1));
    });

    SUBGRID_OVERRIDES.with(|map| {
      if let Some(previous) = self.previous.take() {
        *map.borrow_mut() = previous;
      } else {
        // Outermost layout run: clear to avoid leaking state into subsequent runs.
        map.borrow_mut().clear();
      }
    });
  }
}

#[cfg(feature = "std")]
fn store_subgrid_override(node: NodeId, data: SubgridOverride) {
  SUBGRID_OVERRIDES.with(|map| {
    map.borrow_mut().insert(node, data);
  });
}

#[cfg(feature = "std")]
fn lookup_subgrid_override(node: NodeId) -> Option<SubgridOverride> {
  // Subgrid children may be laid out multiple times during track sizing / intrinsic measurement.
  // Keep overrides in the map for the duration of the layout run so every invocation can apply
  // the resolved track sizes/gaps. The outermost layout guard clears the map on exit.
  SUBGRID_OVERRIDES.with(|map| map.borrow().get(&node).cloned())
}

#[cfg(not(feature = "std"))]
fn store_subgrid_override(_node: NodeId, _data: SubgridOverride) {}

#[cfg(not(feature = "std"))]
fn lookup_subgrid_override(_node: NodeId) -> Option<SubgridOverride> {
  None
}

#[cfg(all(test, feature = "std"))]
pub(crate) fn subgrid_overrides_len() -> usize {
  SUBGRID_OVERRIDES.with(|map| map.borrow().len())
}

#[cfg(all(test, not(feature = "std")))]
pub(crate) fn subgrid_overrides_len() -> usize {
  0
}

#[cfg(test)]
pub(crate) fn insert_dummy_subgrid_override(node: NodeId) {
  store_subgrid_override(
    node,
    SubgridOverride {
      rows: None,
      columns: None,
      template_areas: Vec::new(),
    },
  );
}

fn apply_subgrid_override(style: &mut Style, data: &SubgridOverride) {
  if let Some(rows) = &data.rows {
    style.subgrid_rows = true;
    style.grid_template_rows = rows
      .track_sizes
      .iter()
      .map(|size| {
        GridTemplateComponent::Single(TrackSizingFunction::from(LengthPercentage::length(*size)))
      })
      .collect();
    style.grid_template_row_names = rows.line_names.clone();
    style.subgrid_row_names = rows.line_names.clone();
    style.grid_auto_rows.clear();
    style.gap.height = LengthPercentage::length(rows.gap);
  }
  if let Some(cols) = &data.columns {
    style.subgrid_columns = true;
    style.grid_template_columns = cols
      .track_sizes
      .iter()
      .map(|size| {
        GridTemplateComponent::Single(TrackSizingFunction::from(LengthPercentage::length(*size)))
      })
      .collect();
    style.grid_template_column_names = cols.line_names.clone();
    style.subgrid_column_names = cols.line_names.clone();
    style.grid_auto_columns.clear();
    style.gap.width = LengthPercentage::length(cols.gap);
  }
}

fn to_ident_line_names(names: &[Vec<Ident>]) -> Vec<Vec<Ident>> {
  names
    .iter()
    .map(|set| set.iter().cloned().collect())
    .collect()
}

fn merge_line_names(base: &mut [Vec<Ident>], extra: &[Vec<Ident>]) {
  for (i, extra_names) in extra.iter().enumerate() {
    if let Some(target) = base.get_mut(i) {
      target.extend(extra_names.iter().cloned());
    }
  }
}

fn inherited_line_names(
  span: u16,
  start: OriginZeroLine,
  parent_names: &[Vec<Ident>],
  extra: &[Vec<Ident>],
) -> Vec<Vec<Ident>> {
  let mut result: Vec<Vec<Ident>> = Vec::with_capacity(span as usize + 1);
  for i in 0..=span {
    let mut names: Vec<Ident> = Vec::new();
    let global_index = start.0 + i as i16;
    if global_index >= 0 {
      if let Some(parent) = parent_names.get(global_index as usize) {
        names.extend(parent.iter().cloned());
      }
    }
    result.push(names);
  }

  merge_line_names(&mut result, extra);
  result
}

fn inherited_area_line_names(
  span: u16,
  start: OriginZeroLine,
  parent_areas: &[GridTemplateArea<Ident>],
  axis: AbstractAxis,
) -> Vec<Vec<Ident>> {
  use core::cmp::{max, min};

  let mut result: Vec<Vec<Ident>> = Vec::with_capacity(span as usize + 1);
  for _ in 0..=span {
    result.push(Vec::new());
  }

  if parent_areas.is_empty() {
    return result;
  }

  let sub_start = start;
  let sub_end = start + span;

  for area in parent_areas.iter() {
    let (axis_start, axis_end) = match axis {
      AbstractAxis::Inline => (area.column_start, area.column_end),
      AbstractAxis::Block => (area.row_start, area.row_end),
    };

    // Convert 1-indexed grid line numbers into OriginZero coordinates, clamping to the i16 range
    // used by `OriginZeroLine`.
    let axis_start_oz = OriginZeroLine(
      ((axis_start as i32).saturating_sub(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );
    let axis_end_oz = OriginZeroLine(
      ((axis_end as i32).saturating_sub(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );

    // CSS Grid 2 §7.12.1 “subgrid-area-inheritance”: when a subgrid begins/ends inside a named
    // grid area, clamp the area's implicit edge line names to the subgrid boundaries.
    let clamped_start = max(axis_start_oz, sub_start);
    let clamped_end = min(axis_end_oz, sub_end);
    if clamped_end <= clamped_start {
      continue;
    }

    let local_start = (clamped_start.0 - sub_start.0) as usize;
    let local_end = (clamped_end.0 - sub_start.0) as usize;

    let base: &str = area.name.as_ref();
    let start_name = Ident::from(format!("{base}-start"));
    let end_name = Ident::from(format!("{base}-end"));

    if let Some(target) = result.get_mut(local_start) {
      target.push(start_name);
    }
    if let Some(target) = result.get_mut(local_end) {
      target.push(end_name);
    }
  }

  result
}

fn inherited_template_areas_for_subgrid(
  subgrid_row_span: Line<OriginZeroLine>,
  subgrid_col_span: Line<OriginZeroLine>,
  parent_areas: &[GridTemplateArea<Ident>],
  swap_axes: bool,
) -> Vec<GridTemplateArea<Ident>> {
  use core::cmp::{max, min};

  if parent_areas.is_empty() {
    return Vec::new();
  }

  // If the parent and this subgrid disagree on `axes_swapped`, then the wrapper has transposed how
  // the subgrid's local row/column coordinate space maps to the parent. In that case, treat the
  // parent's columns as the subgrid's rows and the parent's rows as the subgrid's columns.
  let (subgrid_row_span, subgrid_col_span) = if swap_axes {
    (subgrid_col_span, subgrid_row_span)
  } else {
    (subgrid_row_span, subgrid_col_span)
  };

  let mut result: Vec<GridTemplateArea<Ident>> = Vec::new();
  for area in parent_areas.iter() {
    let (row_start, row_end, col_start, col_end) = if swap_axes {
      (area.column_start, area.column_end, area.row_start, area.row_end)
    } else {
      (area.row_start, area.row_end, area.column_start, area.column_end)
    };

    // Convert 1-indexed grid line numbers into OriginZero coordinates, clamping to the i16 range
    // used by `OriginZeroLine`.
    let row_start_oz = OriginZeroLine(
      ((row_start as i32).saturating_sub(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );
    let row_end_oz = OriginZeroLine(
      ((row_end as i32).saturating_sub(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );
    let col_start_oz = OriginZeroLine(
      ((col_start as i32).saturating_sub(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );
    let col_end_oz = OriginZeroLine(
      ((col_end as i32).saturating_sub(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    );

    // Only areas that overlap the subgrid in *both* axes should be propagated.
    let clamped_row_start = max(row_start_oz, subgrid_row_span.start);
    let clamped_row_end = min(row_end_oz, subgrid_row_span.end);
    let clamped_col_start = max(col_start_oz, subgrid_col_span.start);
    let clamped_col_end = min(col_end_oz, subgrid_col_span.end);
    if clamped_row_end <= clamped_row_start || clamped_col_end <= clamped_col_start {
      continue;
    }

    let local_row_start = (clamped_row_start.0 - subgrid_row_span.start.0) as i32 + 1;
    let local_row_end = (clamped_row_end.0 - subgrid_row_span.start.0) as i32 + 1;
    let local_col_start = (clamped_col_start.0 - subgrid_col_span.start.0) as i32 + 1;
    let local_col_end = (clamped_col_end.0 - subgrid_col_span.start.0) as i32 + 1;

    let (Ok(row_start), Ok(row_end), Ok(column_start), Ok(column_end)) = (
      u16::try_from(local_row_start),
      u16::try_from(local_row_end),
      u16::try_from(local_col_start),
      u16::try_from(local_col_end),
    ) else {
      continue;
    };

    if row_end <= row_start || column_end <= column_start {
      continue;
    }

    result.push(GridTemplateArea {
      name: area.name.clone(),
      row_start,
      row_end,
      column_start,
      column_end,
    });
  }

  result
}

fn collect_child_subgrid_line_names(style: &Style, axis: AbstractAxis) -> Vec<Vec<Ident>> {
  // `Style::{subgrid_column_names, subgrid_row_names}` are stored in the same coordinate space as
  // `grid_template_{columns,rows}`. The FastRender integration transposes these fields when the CSS
  // inline axis is vertical so that:
  // - `*_column_*` always corresponds to the physical horizontal axis
  // - `*_row_*` always corresponds to the physical vertical axis
  //
  // That means we can treat `AbstractAxis::Inline` as "horizontal" and `AbstractAxis::Block` as
  // "vertical" regardless of `axes_swapped`.
  match axis {
    AbstractAxis::Inline => to_ident_line_names(&style.subgrid_column_names),
    AbstractAxis::Block => to_ident_line_names(&style.subgrid_row_names),
  }
}

#[inline]
fn subgrid_auto_span_from_line_name_list_len(line_name_list_len: usize) -> u16 {
  let tracks = line_name_list_len.saturating_sub(1).max(1);
  (tracks.min(u16::MAX as usize)) as u16
}

#[inline]
fn child_subgrid_auto_span<
  Tree: LayoutGridContainer + LayoutPartialTree<CustomIdent = DefaultCheapStr>,
>(
  tree: &Tree,
  node: NodeId,
) -> InBothAbsAxis<Option<u16>> {
  let style = tree.get_grid_container_style(node);
  let horizontal = style.is_column_subgrid().then(|| {
    let len = style
      .subgrid_column_names()
      .map(|names| names.len())
      .unwrap_or(0);
    subgrid_auto_span_from_line_name_list_len(len)
  });
  let vertical = style.is_row_subgrid().then(|| {
    let len = style
      .subgrid_row_names()
      .map(|names| names.len())
      .unwrap_or(0);
    subgrid_auto_span_from_line_name_list_len(len)
  });
  InBothAbsAxis {
    horizontal,
    vertical,
  }
}

/// Maximum recursion depth when collecting virtual grid items for nested subgrids.
///
/// Taffy layout trees are expected to be acyclic, but we defensively cap the depth to avoid
/// pathological behaviour on malformed trees.
const MAX_SUBGRID_VIRTUAL_ITEM_DEPTH: usize = 64;

/// Mapping from a subgrid container's local grid coordinate space to an ancestor grid's coordinate
/// space for one axis.
///
/// When an axis is subgridded all the way up to the ancestor grid, the mapping is a simple
/// translation (local line + offset). If the subgrid chain is broken for an axis, descendants no
/// longer participate in that axis's track sizing; we clamp them to the ancestor-space span of the
/// container that broke the chain so that their contributions can still be considered in the other
/// axis.
#[derive(Clone, Copy, Debug)]
struct SubgridVirtualItemMapping {
  inherited: bool,
  offset: OriginZeroLine,
  fallback_span: Line<OriginZeroLine>,
}

impl SubgridVirtualItemMapping {
  #[inline]
  fn map_span(self, span: Line<OriginZeroLine>) -> Line<OriginZeroLine> {
    if self.inherited {
      Line {
        start: span.start + self.offset,
        end: span.end + self.offset,
      }
    } else {
      self.fallback_span
    }
  }
}

/// Mapping from a subgrid container's local grid coordinate space to an ancestor grid's coordinate
/// space, including whether the row/column axes are swapped.
///
/// When `axes_swapped` is false:
/// - local columns (inline / physical X) map to ancestor columns
/// - local rows (block / physical Y) map to ancestor rows
///
/// When `axes_swapped` is true the axes are swapped:
/// - local columns map to ancestor rows
/// - local rows map to ancestor columns
#[derive(Clone, Copy, Debug)]
struct SubgridVirtualItemCoordinateMapping {
  axes_swapped: bool,
  horizontal: SubgridVirtualItemMapping,
  vertical: SubgridVirtualItemMapping,
}

impl SubgridVirtualItemCoordinateMapping {
  #[inline]
  fn map_item_spans(
    self,
    row: Line<OriginZeroLine>,
    column: Line<OriginZeroLine>,
  ) -> (Line<OriginZeroLine>, Line<OriginZeroLine>) {
    if self.axes_swapped {
      // Local columns map into ancestor rows; local rows map into ancestor columns.
      let mapped_row = self.horizontal.map_span(column);
      let mapped_col = self.vertical.map_span(row);
      (mapped_row, mapped_col)
    } else {
      let mapped_row = self.vertical.map_span(row);
      let mapped_col = self.horizontal.map_span(column);
      (mapped_row, mapped_col)
    }
  }
}

/// Collects virtual grid items representing descendants of a subgrid item.
///
/// CSS Grid Level 3 specifies that subgrid item contributions should propagate through chains of
/// nested subgrids (so a grandchild inside a subgrid-of-a-subgrid contributes directly to the
/// ancestor's track sizing). Taffy models this by synthesising "virtual" `GridItem`s for subgrid
/// descendants and feeding them into the ancestor's track sizing algorithm.
///
/// This helper performs that synthesis recursively while keeping the work bounded:
/// - We only recurse into grid containers that are themselves subgrids.
/// - Recursion stops once the subgrid chain is broken in both axes (the descendant no longer maps
///   to the ancestor's tracks).
fn collect_subgrid_virtual_items_recursive<
  Tree: LayoutGridContainer + LayoutPartialTree<CustomIdent = DefaultCheapStr>,
>(
  tree: &mut Tree,
  container_item: &GridItem,
  parent_axes_swapped: bool,
  parent_row_names: &[Vec<Ident>],
  parent_col_names: &[Vec<Ident>],
  parent_template_areas: &[GridTemplateArea<Ident>],
  parent_gap: Size<f32>,
  gap_percentage_basis: Size<Option<f32>>,
  parent_mapping: SubgridVirtualItemCoordinateMapping,
  depth: usize,
  out: &mut Vec<GridItem>,
) {
  if depth >= MAX_SUBGRID_VIRTUAL_ITEM_DEPTH {
    return;
  }

  let container_style_owned = tree.clone_grid_container_style(container_item.node);
  if container_style_owned.box_generation_mode() == BoxGenerationMode::None
    || container_style_owned.position() == Position::Absolute
  {
    return;
  }

  let container_style_ref: &Style<_> = &container_style_owned;
  let subgrid_columns = container_style_ref.is_column_subgrid();
  let subgrid_rows = container_style_ref.is_row_subgrid();
  let container_axes_swapped = container_style_ref.axes_swapped();
  if !subgrid_columns && !subgrid_rows {
    return;
  }

  // Determine whether this subgrid shares any axis with the ancestor grid. If the chain is broken
  // in both axes then descendants cannot be mapped to ancestor tracks at all, and their
  // contributions are already accounted for via the intermediate grid item's intrinsic size.
  let swap_axes = parent_axes_swapped != container_axes_swapped;
  let inherits_columns = if swap_axes {
    parent_mapping.vertical.inherited && subgrid_columns
  } else {
    parent_mapping.horizontal.inherited && subgrid_columns
  };
  let inherits_rows = if swap_axes {
    parent_mapping.horizontal.inherited && subgrid_rows
  } else {
    parent_mapping.vertical.inherited && subgrid_rows
  };
  if !inherits_columns && !inherits_rows {
    return;
  }

  // The placement span in the parent that corresponds to this subgrid's local row/column axes can
  // swap when the wrapper had to transpose axes for writing-mode.
  let (row_span, row_start, col_span, col_start) = if swap_axes {
    (
      container_item.column.span(),
      container_item.column.start,
      container_item.row.span(),
      container_item.row.start,
    )
  } else {
    (
      container_item.row.span(),
      container_item.row.start,
      container_item.column.span(),
      container_item.column.start,
    )
  };

  // Map the container's parent-space placement spans into root space.
  let (container_row_in_root_space, container_col_in_root_space) =
    parent_mapping.map_item_spans(container_item.row, container_item.column);

  // Recover the root-space spans for the parent grid's *local* row/column axes, which may already
  // be swapped relative to the root due to an earlier writing-mode mismatch.
  let parent_columns_span_in_root = if parent_mapping.axes_swapped {
    container_row_in_root_space
  } else {
    container_col_in_root_space
  };
  let parent_rows_span_in_root = if parent_mapping.axes_swapped {
    container_col_in_root_space
  } else {
    container_row_in_root_space
  };

  let container_columns_span_in_root = if swap_axes {
    parent_rows_span_in_root
  } else {
    parent_columns_span_in_root
  };
  let container_rows_span_in_root = if swap_axes {
    parent_columns_span_in_root
  } else {
    parent_rows_span_in_root
  };

  let container_mapping = SubgridVirtualItemCoordinateMapping {
    axes_swapped: parent_mapping.axes_swapped ^ swap_axes,
    horizontal: if inherits_columns {
      SubgridVirtualItemMapping {
        inherited: true,
        offset: container_columns_span_in_root.start,
        fallback_span: container_columns_span_in_root,
      }
    } else {
      SubgridVirtualItemMapping {
        inherited: false,
        offset: OriginZeroLine(0),
        fallback_span: container_columns_span_in_root,
      }
    },
    vertical: if inherits_rows {
      SubgridVirtualItemMapping {
        inherited: true,
        offset: container_rows_span_in_root.start,
        fallback_span: container_rows_span_in_root,
      }
    } else {
      SubgridVirtualItemMapping {
        inherited: false,
        offset: OriginZeroLine(0),
        fallback_span: container_rows_span_in_root,
      }
    },
  };

  let child_row_extra = if subgrid_rows {
    collect_child_subgrid_line_names(container_style_ref, AbstractAxis::Block)
  } else {
    Vec::new()
  };
  let child_col_extra = if subgrid_columns {
    collect_child_subgrid_line_names(container_style_ref, AbstractAxis::Inline)
  } else {
    Vec::new()
  };

  let template_areas = if subgrid_rows && subgrid_columns {
    inherited_template_areas_for_subgrid(
      container_item.row,
      container_item.column,
      parent_template_areas,
      swap_axes,
    )
  } else {
    Vec::new()
  };

  // When the parent and this subgrid disagree on `axes_swapped`, the wrapper has effectively
  // transposed the mapping between their row/column coordinate spaces. In that case:
  // - this subgrid's local rows inherit from the parent's columns
  // - this subgrid's local columns inherit from the parent's rows
  let (row_parent_names, col_parent_names) = if swap_axes {
    (parent_col_names, parent_row_names)
  } else {
    (parent_row_names, parent_col_names)
  };

  let (row_area_axis, col_area_axis) = if swap_axes {
    (AbstractAxis::Inline, AbstractAxis::Block)
  } else {
    (AbstractAxis::Block, AbstractAxis::Inline)
  };

  let row_line_names = if subgrid_rows {
    let mut extra = inherited_area_line_names(
      row_span,
      row_start,
      parent_template_areas,
      row_area_axis,
    );
    merge_line_names(&mut extra, &child_row_extra);
    inherited_line_names(row_span, row_start, row_parent_names, &extra)
  } else {
    to_ident_line_names(&container_style_ref.grid_template_row_names)
  };
  let col_line_names = if subgrid_columns {
    let mut extra = inherited_area_line_names(
      col_span,
      col_start,
      parent_template_areas,
      col_area_axis,
    );
    merge_line_names(&mut extra, &child_col_extra);
    inherited_line_names(col_span, col_start, col_parent_names, &extra)
  } else {
    to_ident_line_names(&container_style_ref.grid_template_column_names)
  };

  let mut row_explicit = if subgrid_rows {
    row_span
  } else {
    container_style_ref
      .grid_template_rows()
      .map(|iter| iter.count() as u16)
      .unwrap_or(0)
  };
  if row_explicit == 0 {
    row_explicit = row_line_names.len().saturating_sub(1) as u16;
  }
  if row_explicit == 0 {
    row_explicit = 1;
  }

  let mut col_explicit = if subgrid_columns {
    col_span
  } else {
    container_style_ref
      .grid_template_columns()
      .map(|iter| iter.count() as u16)
      .unwrap_or(0)
  };
  if col_explicit == 0 {
    col_explicit = col_line_names.len().saturating_sub(1) as u16;
  }
  if col_explicit == 0 {
    col_explicit = 1;
  }

  let line_resolver = NamedLineResolver::from_line_names(
    row_line_names.clone(),
    col_line_names.clone(),
    row_explicit,
    col_explicit,
  );

  let mut subgrid_items: Vec<GridItem> = Vec::with_capacity(tree.child_count(container_item.node));
  let row_counts = TrackCounts {
    negative_implicit: 0,
    explicit: row_explicit,
    positive_implicit: 0,
  };
  let col_counts = TrackCounts {
    negative_implicit: 0,
    explicit: col_explicit,
    positive_implicit: 0,
  };
  let mut cell_occupancy_matrix = CellOccupancyMatrix::with_track_counts(col_counts, row_counts);

  let align_items = container_style_ref
    .align_items()
    .unwrap_or(AlignItems::Stretch);
  let justify_items = container_style_ref
    .justify_items()
    .unwrap_or(AlignItems::Stretch);
  let grid_auto_flow = container_style_ref.grid_auto_flow();

  let subgrid_children_iter = || {
    tree
      .child_ids(container_item.node)
      .enumerate()
      .map(|(index, child_node)| (index, child_node, tree.get_grid_child_style(child_node)))
      .filter(|(_, _, style)| {
        style.box_generation_mode() != BoxGenerationMode::None
          && style.position() != Position::Absolute
      })
  };

  place_grid_items(
    &mut cell_occupancy_matrix,
    &mut subgrid_items,
    subgrid_children_iter,
    grid_auto_flow,
    align_items,
    justify_items,
    &line_resolver,
    InBothAbsAxis {
      horizontal: subgrid_columns,
      vertical: subgrid_rows,
    },
    |node| child_subgrid_auto_span(tree, node),
  );

  let resolved_gap = container_style_ref
    .gap
    .resolve_or_zero(gap_percentage_basis, |val, basis| tree.calc(val, basis));
  let inherited_gap_for_columns = if swap_axes {
    parent_gap.height
  } else {
    parent_gap.width
  };
  let inherited_gap_for_rows = if swap_axes {
    parent_gap.width
  } else {
    parent_gap.height
  };
  let container_gap = Size {
    width: if subgrid_columns {
      inherited_gap_for_columns
    } else {
      resolved_gap.width
    },
    height: if subgrid_rows {
      inherited_gap_for_rows
    } else {
      resolved_gap.height
    },
  };

  if subgrid_columns {
    let inherited_gap = inherited_gap_for_columns;
    let desired_gap = if container_style_ref.subgrid_gap_is_normal.width {
      inherited_gap
    } else {
      container_style_ref
        .subgrid_gap
        .width
        .resolve_or_zero(gap_percentage_basis.width, |val, basis| {
          tree.calc(val, basis)
        })
    };
    let half_diff = (desired_gap - inherited_gap) / 2.0;
    apply_subgrid_gap_difference_margin(
      &mut subgrid_items,
      AbstractAxis::Inline,
      half_diff,
      col_span,
    );
  }
  if subgrid_rows {
    let inherited_gap = inherited_gap_for_rows;
    let desired_gap = if container_style_ref.subgrid_gap_is_normal.height {
      inherited_gap
    } else {
      container_style_ref
        .subgrid_gap
        .height
        .resolve_or_zero(gap_percentage_basis.height, |val, basis| {
          tree.calc(val, basis)
        })
    };
    let half_diff = (desired_gap - inherited_gap) / 2.0;
    apply_subgrid_gap_difference_margin(
      &mut subgrid_items,
      AbstractAxis::Block,
      half_diff,
      row_span,
    );
  }

  #[cfg(all(feature = "std", debug_assertions))]
  {
    if std::env::var("FASTR_DEBUG_SUBGRID").is_ok() {
      eprintln!(
        "[subgrid-debug] node={:?} row_span={} col_span={} row_explicit={} col_explicit={} items={}",
        container_item.node,
        row_span,
        col_span,
        row_explicit,
        col_explicit,
        subgrid_items.len()
      );
      for (idx, sub_item) in subgrid_items.iter().enumerate() {
        eprintln!(
          "  item {idx}: row=({},{}) col=({},{})",
          sub_item.row.start.0, sub_item.row.end.0, sub_item.column.start.0, sub_item.column.end.0
        );
      }
    }
  }

  // Convert placements from subgrid coordinates to ancestor coordinates, clamping in any axis
  // where the subgrid chain is broken.
  for (index, mut sub_item) in subgrid_items.into_iter().enumerate() {
    // CSS Grid subgrids with different gaps apply an "extra layer of margin" to their items.
    // The spec says this extra margin accumulates through multiple levels of subgrids, so when we
    // synthesise virtual items for descendants we need to include any extra margin already applied
    // to the subgrid container item.
    sub_item.extra_margin = sub_item.extra_margin + container_item.extra_margin;

    // Recurse into nested subgrids before we mutate the placement coordinates into ancestor space.
    if depth + 1 < MAX_SUBGRID_VIRTUAL_ITEM_DEPTH {
      let child_style_owned = tree.clone_grid_container_style(sub_item.node);
      if child_style_owned.box_generation_mode() != BoxGenerationMode::None
        && child_style_owned.position() != Position::Absolute
      {
        let child_style_ref: &Style<_> = &child_style_owned;
        let child_subgrid_columns = child_style_ref.is_column_subgrid();
        let child_subgrid_rows = child_style_ref.is_row_subgrid();
        let child_axes_swapped = child_style_ref.axes_swapped();
        let swap_child_axes = container_axes_swapped != child_axes_swapped;

        // Only descend if the nested subgrid still shares at least one axis with the ancestor grid.
        let child_inherits_columns = if swap_child_axes {
          container_mapping.vertical.inherited && child_subgrid_columns
        } else {
          container_mapping.horizontal.inherited && child_subgrid_columns
        };
        let child_inherits_rows = if swap_child_axes {
          container_mapping.horizontal.inherited && child_subgrid_rows
        } else {
          container_mapping.vertical.inherited && child_subgrid_rows
        };
        if child_inherits_columns || child_inherits_rows {
          collect_subgrid_virtual_items_recursive(
            tree,
            &sub_item,
            container_axes_swapped,
            &row_line_names,
            &col_line_names,
            template_areas.as_slice(),
            container_gap,
            gap_percentage_basis,
            container_mapping,
            depth + 1,
            out,
          );
        }
      }
    }

    let (mapped_row, mapped_col) = container_mapping.map_item_spans(sub_item.row, sub_item.column);
    sub_item.row = mapped_row;
    sub_item.column = mapped_col;
    sub_item.is_virtual = true;
    sub_item.source_order = index as u16;
    out.push(sub_item);
  }
}

fn apply_subgrid_gap_difference_margin(
  items: &mut [GridItem],
  axis: AbstractAxis,
  half_diff: f32,
  span: u16,
) {
  if half_diff == 0.0 || span == 0 {
    return;
  }
  let span = span as i32;
  for item in items.iter_mut() {
    let placement = item.placement(axis);
    let start = placement.start.0 as i32;
    let end = placement.end.0 as i32;

    if start > 0 {
      match axis {
        AbstractAxis::Inline => item.extra_margin.left += half_diff,
        AbstractAxis::Block => item.extra_margin.top += half_diff,
      }
    }
    if end < span {
      match axis {
        AbstractAxis::Inline => item.extra_margin.right += half_diff,
        AbstractAxis::Block => item.extra_margin.bottom += half_diff,
      }
    }
  }
}

fn collect_subgrid_virtual_items<
  Tree: LayoutGridContainer + LayoutPartialTree<CustomIdent = DefaultCheapStr>,
>(
  tree: &mut Tree,
  items: &[GridItem],
  parent_row_names: &[Vec<Ident>],
  parent_col_names: &[Vec<Ident>],
  parent_template_areas: &[GridTemplateArea<Ident>],
  parent_style: &Style,
  gap_percentage_basis: Size<Option<f32>>,
  final_col_counts: TrackCounts,
  final_row_counts: TrackCounts,
) -> Vec<GridItem> {
  // Track/line-name storage is already transposed into physical space by the wrapper. Treat the
  // Taffy inline axis as physical X (columns vector) and the Taffy block axis as physical Y (rows
  // vector), independent of `axes_swapped`.
  // Subgrid virtual items are usually proportional to the number of in-flow grid items. Preallocate
  // to avoid repeated growth in common cases.
  let mut virtuals = Vec::with_capacity(items.len());

  let parent_gap = parent_style
    .gap
    .resolve_or_zero(gap_percentage_basis, |val, basis| tree.calc(val, basis));

  let root_mapping = SubgridVirtualItemCoordinateMapping {
    axes_swapped: false,
    horizontal: SubgridVirtualItemMapping {
      inherited: true,
      offset: OriginZeroLine(0),
      fallback_span: Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(0),
      },
    },
    vertical: SubgridVirtualItemMapping {
      inherited: true,
      offset: OriginZeroLine(0),
      fallback_span: Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(0),
      },
    },
  };
  let parent_axes_swapped = parent_style.axes_swapped();

  for item in items.iter().filter(|item| !item.is_virtual) {
    collect_subgrid_virtual_items_recursive(
      tree,
      item,
      parent_axes_swapped,
      parent_row_names,
      parent_col_names,
      parent_template_areas,
      parent_gap,
      gap_percentage_basis,
      root_mapping,
      0,
      &mut virtuals,
    );
  }

  // Resolve track indexes for the virtual items against the parent grid counts
  virtuals.retain(|item| {
    item
      .column
      .start
      .try_into_track_vec_index(final_col_counts)
      .is_some()
      && item
        .column
        .end
        .try_into_track_vec_index(final_col_counts)
        .is_some()
      && item
        .row
        .start
        .try_into_track_vec_index(final_row_counts)
        .is_some()
      && item
        .row
        .end
        .try_into_track_vec_index(final_row_counts)
        .is_some()
  });
  resolve_item_track_indexes(&mut virtuals, final_col_counts, final_row_counts);
  virtuals
}

fn record_subgrid_overrides<
  Tree: LayoutGridContainer + LayoutPartialTree<CustomIdent = DefaultCheapStr>,
>(
  tree: &mut Tree,
  items: &[GridItem],
  parent_row_names: &[Vec<Ident>],
  parent_col_names: &[Vec<Ident>],
  parent_template_areas: &[GridTemplateArea<Ident>],
  rows: &[GridTrack],
  columns: &[GridTrack],
  parent_axes_swapped: bool,
  record_rows: bool,
  record_columns: bool,
) {
  #[cfg(feature = "std")]
  let debug_subgrid = std::env::var("FASTR_DEBUG_SUBGRID").is_ok();
  #[cfg(not(feature = "std"))]
  let debug_subgrid = false;
  // Track/line-name storage is already transposed into physical space by the wrapper. However,
  // subgrids may have a different `axes_swapped` from their parent (writing-mode mismatch), which
  // means the wrapper also swapped the mapping between the parent and child row/column coordinate
  // spaces. When that happens, the child's local rows inherit from the parent's columns and the
  // child's local columns inherit from the parent's rows.

  for item in items.iter().filter(|item| !item.is_virtual) {
    // Some WPT cases (e.g. nested subgrids with a writing-mode mismatch at the parent boundary)
    // expect that a subgrid with fully-automatic placement can still see the full set of parent
    // tracks, even when it happens to be auto-placed into a 1-track area.
    //
    // Taffy's core subgrid model normally slices the inherited tracks to the grid item's used
    // span, which can result in clamping (`grid-column: 2` collapses back into column 1) and loss
    // of inherited gaps when the used span is 1.
    //
    // When a subgrid container's placement is fully automatic on an axis (both edges `auto`) and
    // the resolved span is 1, extend the inherited track slice to the end of the parent grid in
    // that axis. This preserves the parent track definitions (and gap) for descendants without
    // affecting the subgrid container's own placement in the parent.
    let child_item_style_owned = tree.clone_grid_child_style(item.node);
    let child_item_style_ref: &Style<_> = &child_item_style_owned;
    let child_style_owned = tree.clone_grid_container_style(item.node);
    let child_style_ref: &Style<_> = &child_style_owned;
    let subgrid_rows = child_style_ref.is_row_subgrid();
    let subgrid_columns = child_style_ref.is_column_subgrid();
    if !subgrid_rows && !subgrid_columns {
      continue;
    }
    let child_axes_swapped = child_style_ref.axes_swapped();
    let swap_axes = parent_axes_swapped != child_axes_swapped;

    let template_areas = if subgrid_rows && subgrid_columns {
      inherited_template_areas_for_subgrid(item.row, item.column, parent_template_areas, swap_axes)
    } else {
      Vec::new()
    };

    let mut rows_override = None;
    let mut cols_override = None;
    if subgrid_rows {
      // Child rows inherit from parent rows unless the axes are swapped between parent/child, in
      // which case child rows inherit from parent columns.
      let uses_parent_rows = !swap_axes;
      let can_record = if uses_parent_rows { record_rows } else { record_columns };
      if can_record {
        let (span_start, mut span, axis, tracks, parent_names) = if uses_parent_rows {
          (item.row.start, item.row.span(), AbstractAxis::Block, rows, parent_row_names)
        } else {
          (
            item.column.start,
            item.column.span(),
            AbstractAxis::Inline,
            columns,
            parent_col_names,
          )
        };

        let mut track_range = item.track_range_excluding_lines(axis);
        if span == 1 {
          let fully_auto = match axis {
            AbstractAxis::Inline => {
              matches!(child_item_style_ref.grid_column.start, crate::style::GridPlacement::Auto)
                && matches!(child_item_style_ref.grid_column.end, crate::style::GridPlacement::Auto)
            }
            AbstractAxis::Block => {
              matches!(child_item_style_ref.grid_row.start, crate::style::GridPlacement::Auto)
                && matches!(child_item_style_ref.grid_row.end, crate::style::GridPlacement::Auto)
            }
          };
          if fully_auto {
            let start_index = match axis {
              AbstractAxis::Inline => item.column_indexes.start as usize,
              AbstractAxis::Block => item.row_indexes.start as usize,
            };
            let end_index = tracks.len().saturating_sub(1);
            if start_index + 1 < end_index {
              track_range = (start_index + 1)..end_index;
            }
          }
        }

        let mut track_sizes: Vec<f32> = Vec::new();
        let mut gap = 0.0;
        for track in tracks[track_range].iter() {
          if track.kind == GridTrackKind::Track {
            track_sizes.push(track.base_size);
          } else if gap == 0.0 {
            gap = track.base_size;
          }
        }
        span = (track_sizes.len().min(u16::MAX as usize)) as u16;
        let child_extra = collect_child_subgrid_line_names(child_style_ref, AbstractAxis::Block);
        let mut extra = inherited_area_line_names(span, span_start, parent_template_areas, axis);
        merge_line_names(&mut extra, &child_extra);
        let line_names = inherited_line_names(span, span_start, parent_names, &extra);
        rows_override = Some(SubgridAxisOverride {
          track_sizes,
          line_names,
          gap,
        });
      }
    }

    if subgrid_columns {
      // Child columns inherit from parent columns unless the axes are swapped between parent/child,
      // in which case child columns inherit from parent rows.
      let uses_parent_columns = !swap_axes;
      let can_record = if uses_parent_columns {
        record_columns
      } else {
        record_rows
      };
      if can_record {
        let (span_start, mut span, axis, tracks, parent_names) = if uses_parent_columns {
          (
            item.column.start,
            item.column.span(),
            AbstractAxis::Inline,
            columns,
            parent_col_names,
          )
        } else {
          (item.row.start, item.row.span(), AbstractAxis::Block, rows, parent_row_names)
        };

        let mut track_range = item.track_range_excluding_lines(axis);
        if span == 1 {
          let fully_auto = match axis {
            AbstractAxis::Inline => {
              matches!(child_item_style_ref.grid_column.start, crate::style::GridPlacement::Auto)
                && matches!(child_item_style_ref.grid_column.end, crate::style::GridPlacement::Auto)
            }
            AbstractAxis::Block => {
              matches!(child_item_style_ref.grid_row.start, crate::style::GridPlacement::Auto)
                && matches!(child_item_style_ref.grid_row.end, crate::style::GridPlacement::Auto)
            }
          };
          if fully_auto {
            let start_index = match axis {
              AbstractAxis::Inline => item.column_indexes.start as usize,
              AbstractAxis::Block => item.row_indexes.start as usize,
            };
            let end_index = tracks.len().saturating_sub(1);
            if start_index + 1 < end_index {
              track_range = (start_index + 1)..end_index;
            }
          }
        }

        let mut track_sizes: Vec<f32> = Vec::new();
        let mut gap = 0.0;
        for track in tracks[track_range].iter() {
          if track.kind == GridTrackKind::Track {
            track_sizes.push(track.base_size);
          } else if gap == 0.0 {
            gap = track.base_size;
          }
        }
        span = (track_sizes.len().min(u16::MAX as usize)) as u16;
        let child_extra = collect_child_subgrid_line_names(child_style_ref, AbstractAxis::Inline);
        let mut extra = inherited_area_line_names(span, span_start, parent_template_areas, axis);
        merge_line_names(&mut extra, &child_extra);
        let line_names = inherited_line_names(span, span_start, parent_names, &extra);
        cols_override = Some(SubgridAxisOverride {
          track_sizes,
          line_names,
          gap,
        });
      }
    }

    if rows_override.is_some() || cols_override.is_some() {
      if debug_subgrid {
        #[cfg(feature = "std")]
        if let Some(override_data) = &rows_override {
          eprintln!(
            "[subgrid-override] node={:?} rows tracks={:?} names={:?} gap={}",
            item.node, override_data.track_sizes, override_data.line_names, override_data.gap
          );
        }
        #[cfg(feature = "std")]
        if let Some(override_data) = &cols_override {
          eprintln!(
            "[subgrid-override] node={:?} cols tracks={:?} names={:?} gap={}",
            item.node, override_data.track_sizes, override_data.line_names, override_data.gap
          );
        }
      }
      store_subgrid_override(
        item.node,
        SubgridOverride {
          rows: rows_override,
          columns: cols_override,
          template_areas,
        },
      );
    }
  }
}

pub(crate) use types::{GridCoordinate, GridLine, OriginZeroLine};

mod alignment;
mod explicit_grid;
mod implicit_grid;
mod limits;
mod placement;
#[cfg(test)]
mod rerun_detection_tests;
#[cfg(all(test, feature = "taffy_tree"))]
mod rerun_measure_tests;
mod track_sizing;
mod types;
mod util;
#[cfg(all(test, feature = "taffy_tree"))]
mod subgrid_axes_swapped_tests;

/// Grid layout algorithm
/// This consists of a few phases:
///   - Resolving the explicit grid
///   - Placing items (which also resolves the implicit grid)
///   - Track (row/column) sizing
///   - Alignment & Final item placement
pub fn compute_grid_layout<Tree>(tree: &mut Tree, node: NodeId, inputs: LayoutInput) -> LayoutOutput
where
  Tree: LayoutGridContainer + LayoutPartialTree<CustomIdent = DefaultCheapStr>,
{
  let LayoutInput {
    known_dimensions,
    parent_size,
    available_space,
    run_mode,
    ..
  } = inputs;

  let mut style_storage = tree.clone_grid_container_style(node);
  let subgrid_override = lookup_subgrid_override(node);
  if let Some(override_data) = subgrid_override.as_ref() {
    apply_subgrid_override(&mut style_storage, override_data);
  }
  let style: &Style<_> = &style_storage;
  let template_areas_for_children: &[GridTemplateArea<Ident>] =
    if style.grid_template_areas.is_empty() {
      subgrid_override
        .as_ref()
        .map(|data| data.template_areas.as_slice())
        .unwrap_or(&[])
    } else {
      style.grid_template_areas.as_slice()
    };

  // 1. Compute "available grid space"
  // https://www.w3.org/TR/css-grid-1/#available-grid-space
  let aspect_ratio = style.aspect_ratio();
  let padding = style
    .padding()
    .resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
  let border = style
    .border()
    .resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
  let padding_border = padding + border;
  let padding_border_size = padding_border.sum_axes();
  let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
    padding_border_size
  } else {
    Size::ZERO
  };

  let min_size = style
    .min_size()
    .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
    .maybe_apply_aspect_ratio(aspect_ratio)
    .maybe_add(box_sizing_adjustment);
  let max_size = style
    .max_size()
    .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
    .maybe_apply_aspect_ratio(aspect_ratio)
    .maybe_add(box_sizing_adjustment);
  let preferred_size = if inputs.sizing_mode == SizingMode::InherentSize {
    style
      .size()
      .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
      .maybe_apply_aspect_ratio(style.aspect_ratio())
      .maybe_add(box_sizing_adjustment)
  } else {
    Size::NONE
  };

  // Scrollbar gutters are reserved when the `overflow` property is set to `Overflow::Scroll`.
  // However, the axis are switched (transposed) because a node that scrolls vertically needs
  // *horizontal* space to be reserved for a scrollbar
  let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
    Overflow::Scroll => style.scrollbar_width(),
    _ => 0.0,
  });
  // TODO: make side configurable based on the `direction` property
  let mut content_box_inset = padding_border;
  content_box_inset.right += scrollbar_gutter.x;
  content_box_inset.bottom += scrollbar_gutter.y;

  let align_content = style.align_content().unwrap_or(AlignContent::Stretch);
  let justify_content = style.justify_content().unwrap_or(JustifyContent::Stretch);
  let align_items = style.align_items();
  let justify_items = style.justify_items();

  // Note: we avoid accessing the grid rows/columns methods more than once as this can
  // cause an expensive-ish computation
  let grid_template_columms = style.grid_template_columns();
  let grid_template_rows = style.grid_template_rows();
  let grid_auto_columms = style.grid_auto_columns();
  let grid_auto_rows = style.grid_auto_rows();

  let constrained_available_space = known_dimensions
    .or(preferred_size)
    .map(|size| size.map(AvailableSpace::Definite))
    .unwrap_or(available_space)
    .maybe_clamp(min_size, max_size)
    .maybe_max(padding_border_size);

  let available_grid_space = Size {
    width: constrained_available_space
      .width
      .map_definite_value(|space| space - content_box_inset.horizontal_axis_sum()),
    height: constrained_available_space
      .height
      .map_definite_value(|space| space - content_box_inset.vertical_axis_sum()),
  };

  let outer_node_size = known_dimensions
    .or(preferred_size)
    .maybe_clamp(min_size, max_size)
    .maybe_max(padding_border_size);
  let mut inner_node_size = Size {
    width: outer_node_size
      .width
      .map(|space| space - content_box_inset.horizontal_axis_sum()),
    height: outer_node_size
      .height
      .map(|space| space - content_box_inset.vertical_axis_sum()),
  };
  let width_was_indefinite = inner_node_size.width.is_none();
  let height_was_indefinite = inner_node_size.height.is_none();

  debug_log!("parent_size", dbg:parent_size);
  debug_log!("outer_node_size", dbg:outer_node_size);
  debug_log!("inner_node_size", dbg:inner_node_size);

  if let (RunMode::ComputeSize, Some(width), Some(height)) =
    (run_mode, outer_node_size.width, outer_node_size.height)
  {
    return LayoutOutput::from_outer_size(Size { width, height });
  }

  let get_child_styles_iter = |node| {
    tree
      .child_ids(node)
      .map(|child_node: NodeId| (child_node, tree.get_grid_child_style(child_node)))
  };
  let child_styles_iter = get_child_styles_iter(node);

  // 2. Resolve the explicit grid

  // This is very similar to the inner_node_size except if the inner_node_size is not definite but the node
  // has a min- or max- size style then that will be used in it's place.
  let auto_fit_container_size = outer_node_size
    .or(max_size)
    .or(min_size)
    .maybe_clamp(min_size, max_size)
    .maybe_max(padding_border_size)
    .maybe_sub(content_box_inset.sum_axes());

  // If the grid container has a definite size or max size in the relevant axis:
  //   - then the number of repetitions is the largest possible positive integer that does not cause the grid to overflow the content
  //     box of its grid container.
  // Otherwise, if the grid container has a definite min size in the relevant axis:
  //   - then the number of repetitions is the smallest possible positive integer that fulfills that minimum requirement
  // Otherwise, the specified track list repeats only once.
  let auto_repeat_fit_strategy = outer_node_size.or(max_size).map(|val| match val {
    Some(_) => AutoRepeatStrategy::MaxRepetitionsThatDoNotOverflow,
    None => AutoRepeatStrategy::MinRepetitionsThatDoOverflow,
  });

  // Compute the number of rows and columns in the explicit grid *template*
  // (explicit tracks from grid_areas are computed separately below)
  let (col_auto_repetition_count, grid_template_col_count) = compute_explicit_grid_size_in_axis(
    &style,
    auto_fit_container_size.width,
    auto_repeat_fit_strategy.width,
    |val, basis| tree.calc(val, basis),
    AbsoluteAxis::Horizontal,
  );
  let (row_auto_repetition_count, grid_template_row_count) = compute_explicit_grid_size_in_axis(
    &style,
    auto_fit_container_size.height,
    auto_repeat_fit_strategy.height,
    |val, basis| tree.calc(val, basis),
    AbsoluteAxis::Vertical,
  );

  // type CustomIdent<'a> = <<Tree as LayoutPartialTree>::CoreContainerStyle<'_> as CoreStyle>::CustomIdent;
  let mut name_resolver =
    NamedLineResolver::new(&style, col_auto_repetition_count, row_auto_repetition_count);

  let explicit_col_count = grid_template_col_count.max(name_resolver.area_column_count());
  let explicit_row_count = grid_template_row_count.max(name_resolver.area_row_count());

  name_resolver.set_explicit_column_count(explicit_col_count);
  name_resolver.set_explicit_row_count(explicit_row_count);
  let parent_row_line_names = name_resolver.expanded_row_line_names();
  let parent_col_line_names = name_resolver.expanded_column_line_names();

  // 3. Implicit Grid: Estimate Track Counts
  // Estimate the number of rows and columns in the implicit grid (= the entire grid)
  // This is necessary as part of placement. Doing it early here is a perf optimisation to reduce allocations.
  let disallow_implicit_columns = style.is_column_subgrid() && grid_template_col_count > 0;
  let disallow_implicit_rows = style.is_row_subgrid() && grid_template_row_count > 0;

  let (mut est_col_counts, mut est_row_counts) = compute_grid_size_estimate(
    explicit_col_count,
    explicit_row_count,
    child_styles_iter,
    |node| child_subgrid_auto_span(tree, node),
  );
  // Subgrids do not create implicit tracks in the subgridded axis. Clamp the initial estimates so
  // auto-placement cannot preallocate (or later rely on) implicit tracks for those axes.
  //
  // Only apply this behaviour when the subgrid axis has already been resolved into concrete track
  // sizes (i.e. we have a non-empty template track list from `apply_subgrid_override`). When
  // subgrid values are encountered without overrides (invalid usage, or during early intrinsic
  // probes), fall back to normal implicit-track behaviour to avoid pathological placement loops.
  if disallow_implicit_columns {
    est_col_counts.negative_implicit = 0;
    est_col_counts.positive_implicit = 0;
  }
  if disallow_implicit_rows {
    est_row_counts.negative_implicit = 0;
    est_row_counts.positive_implicit = 0;
  }

  // 4. Grid Item Placement
  // Match items (children) to a definite grid position (row start/end and column start/end position)
  let mut items = Vec::with_capacity(tree.child_count(node));
  let mut cell_occupancy_matrix =
    CellOccupancyMatrix::with_track_counts(est_col_counts, est_row_counts);
  let in_flow_children_iter = || {
    tree
      .child_ids(node)
      .enumerate()
      .map(|(index, child_node)| (index, child_node, tree.get_grid_child_style(child_node)))
      .filter(|(_, _, style)| {
        style.box_generation_mode() != BoxGenerationMode::None
          && style.position() != Position::Absolute
      })
  };
  place_grid_items(
    &mut cell_occupancy_matrix,
    &mut items,
    in_flow_children_iter,
    style.grid_auto_flow(),
    align_items.unwrap_or(AlignItems::Stretch),
    justify_items.unwrap_or(AlignItems::Stretch),
    &name_resolver,
    InBothAbsAxis {
      horizontal: disallow_implicit_columns,
      vertical: disallow_implicit_rows,
    },
    |node| child_subgrid_auto_span(tree, node),
  );

  // Extract track counts from previous step (auto-placement can expand the number of tracks)
  let final_col_counts = *cell_occupancy_matrix.track_counts(AbsoluteAxis::Horizontal);
  let final_row_counts = *cell_occupancy_matrix.track_counts(AbsoluteAxis::Vertical);

  let gap_percentage_basis = Size {
    width: available_grid_space.width.into_option(),
    height: available_grid_space.height.into_option(),
  };
  let mut virtual_items = collect_subgrid_virtual_items(
    tree,
    &items,
    &parent_row_line_names,
    &parent_col_line_names,
    template_areas_for_children,
    style,
    gap_percentage_basis,
    final_col_counts,
    final_row_counts,
  );
  items.append(&mut virtual_items);

  if let Some(override_data) = subgrid_override.as_ref() {
    if let Some(cols) = &override_data.columns {
      let desired_gap = if style.subgrid_gap_is_normal.width {
        cols.gap
      } else {
        let axis_size = cols.track_sizes.iter().sum::<f32>()
          + cols.gap * (cols.track_sizes.len().saturating_sub(1) as f32);
        style
          .subgrid_gap
          .width
          .resolve_or_zero(Some(axis_size), |val, basis| tree.calc(val, basis))
      };
      let half_diff = (desired_gap - cols.gap) / 2.0;
      apply_subgrid_gap_difference_margin(
        &mut items,
        AbstractAxis::Inline,
        half_diff,
        cols.track_sizes.len().min(u16::MAX as usize) as u16,
      );
    }

    if let Some(rows) = &override_data.rows {
      let desired_gap = if style.subgrid_gap_is_normal.height {
        rows.gap
      } else {
        let axis_size = rows.track_sizes.iter().sum::<f32>()
          + rows.gap * (rows.track_sizes.len().saturating_sub(1) as f32);
        style
          .subgrid_gap
          .height
          .resolve_or_zero(Some(axis_size), |val, basis| tree.calc(val, basis))
      };
      let half_diff = (desired_gap - rows.gap) / 2.0;
      apply_subgrid_gap_difference_margin(
        &mut items,
        AbstractAxis::Block,
        half_diff,
        rows.track_sizes.len().min(u16::MAX as usize) as u16,
      );
    }
  }

  // 5. Initialize Tracks
  // Initialize (explicit and implicit) grid tracks (and gutters)
  // This resolves the min and max track sizing functions for all tracks and gutters
  let mut columns = GridTrackVec::new();
  let mut rows = GridTrackVec::new();
  initialize_grid_tracks(
    &mut columns,
    final_col_counts,
    &style,
    AbsoluteAxis::Horizontal,
    |column_index| cell_occupancy_matrix.column_is_occupied(column_index),
  );
  initialize_grid_tracks(
    &mut rows,
    final_row_counts,
    &style,
    AbsoluteAxis::Vertical,
    |row_index| cell_occupancy_matrix.row_is_occupied(row_index),
  );

  drop(grid_template_rows);
  drop(grid_template_columms);
  drop(grid_auto_rows);
  drop(grid_auto_columms);
  let _ = style;

  // 6. Track Sizing

  // Convert grid placements in origin-zero coordinates to indexes into the GridTrack (rows and columns) vectors
  // This computation is relatively trivial, but it requires the final number of negative (implicit) tracks in
  // each axis, and doing it up-front here means we don't have to keep repeating that calculation
  resolve_item_track_indexes(&mut items, final_col_counts, final_row_counts);

  // For each item, and in each axis, determine whether the item crosses any flexible (fr) tracks
  // Record this as a boolean (per-axis) on each item for later use in the track-sizing algorithm
  determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

  // Determine whether the grid has any baseline-aligned items in either axis.
  //
  // Note: virtual items (subgrid descendants mapped into the parent) participate in baseline
  // alignment and must contribute to track sizing.
  let mut has_align_self_baseline_item = false;
  let mut has_justify_self_baseline_item = false;
  for item in items.iter() {
    has_align_self_baseline_item |= item.align_self == AlignSelf::Baseline;
    has_justify_self_baseline_item |= item.justify_self == AlignSelf::Baseline;
    if has_align_self_baseline_item && has_justify_self_baseline_item {
      break;
    }
  }

  // Baseline shims affect intrinsic size contributions via margins. `justify-self: baseline`
  // requires horizontal baseline information (`first_baselines.x`) which is only available once
  // we've performed baseline measurement. If we delay until the block-axis sizing pass then
  // column sizing won't see the shim unless we happen to trigger a rerun. Precompute the
  // horizontal baseline shims so the first inline-axis track sizing pass can account for them.
  if has_justify_self_baseline_item {
    resolve_item_baselines(tree, AbstractAxis::Block, &mut items, inner_node_size);
  }

  // Run track sizing algorithm for Inline axis
  track_sizing_algorithm(
    tree,
    AbstractAxis::Inline,
    min_size.get(AbstractAxis::Inline),
    max_size.get(AbstractAxis::Inline),
    justify_content,
    align_content,
    available_grid_space,
    inner_node_size,
    &mut columns,
    &mut rows,
    &mut items,
    |track: &GridTrack, parent_size: Option<f32>, tree: &Tree| {
      track
        .max_track_sizing_function
        .definite_value(parent_size, |val, basis| tree.calc(val, basis))
    },
    has_align_self_baseline_item,
  );
  let initial_column_sum = columns.iter().map(|track| track.base_size).sum::<f32>();
  inner_node_size.width = inner_node_size.width.or_else(|| initial_column_sum.into());

  items
    .iter_mut()
    .for_each(|item| item.available_space_cache = None);

  // The block-axis track sizing algorithm measures grid items (RunMode::ComputeSize) to determine
  // intrinsic block-size contributions. Subgrids need the inline-axis (horizontal) track sizes
  // they inherit in order to measure correctly (e.g. text wrapping).
  //
  // FastRender maps writing-mode dependent axes into Taffy's physical axes, so Taffy's inline axis
  // always corresponds to the physical horizontal axis, regardless of `style.axes_swapped`.
  //
  // However, if a subgrid child has a different `axes_swapped` from this grid container (writing-mode
  // mismatch), the wrapper will also transpose how the child's row/column coordinate space maps onto
  // the parent. In that case, the override that depends on the parent's column sizes may need to be
  // stored into the child's *rows* override rather than its columns override.
  //
  // Record overrides for whichever child axis inherits from the parent's inline axis (columns) before
  // the block-axis sizing pass.
  let (record_rows, record_columns) = (false, true);
  record_subgrid_overrides(
    tree,
    &items,
    &parent_row_line_names,
    &parent_col_line_names,
    template_areas_for_children,
    &rows,
    &columns,
    style.axes_swapped(),
    record_rows,
    record_columns,
  );

  // Run track sizing algorithm for Block axis
  track_sizing_algorithm(
    tree,
    AbstractAxis::Block,
    min_size.get(AbstractAxis::Block),
    max_size.get(AbstractAxis::Block),
    align_content,
    justify_content,
    available_grid_space,
    inner_node_size,
    &mut rows,
    &mut columns,
    &mut items,
    |track: &GridTrack, _, _| Some(track.base_size),
    has_justify_self_baseline_item,
  );
  let initial_row_sum = rows.iter().map(|track| track.base_size).sum::<f32>();
  inner_node_size.height = inner_node_size.height.or_else(|| initial_row_sum.into());

  debug_log!("initial_column_sum", dbg:initial_column_sum);
  debug_log!(dbg: columns.iter().map(|track| track.base_size).collect::<Vec<_>>());
  debug_log!("initial_row_sum", dbg:initial_row_sum);
  debug_log!(dbg: rows.iter().map(|track| track.base_size).collect::<Vec<_>>());

  // 6. Compute container size
  let resolved_style_size = known_dimensions.or(preferred_size);
  let container_border_box = Size {
    width: resolved_style_size
      .get(AbstractAxis::Inline)
      .unwrap_or_else(|| initial_column_sum + content_box_inset.horizontal_axis_sum())
      .maybe_clamp(min_size.width, max_size.width)
      .max(padding_border_size.width),
    height: resolved_style_size
      .get(AbstractAxis::Block)
      .unwrap_or_else(|| initial_row_sum + content_box_inset.vertical_axis_sum())
      .maybe_clamp(min_size.height, max_size.height)
      .max(padding_border_size.height),
  };
  let container_content_box = Size {
    width: f32_max(
      0.0,
      container_border_box.width - content_box_inset.horizontal_axis_sum(),
    ),
    height: f32_max(
      0.0,
      container_border_box.height - content_box_inset.vertical_axis_sum(),
    ),
  };

  // If only the container's size has been requested
  if run_mode == RunMode::ComputeSize {
    return LayoutOutput::from_outer_size(container_border_box);
  }

  // Now that the grid container's used size is known, use it as the percentage basis for reruns.
  //
  // Percent track sizing functions behave like `auto` when the container size is indefinite. When
  // that first-pass sizing results in a definite container size (e.g. via intrinsic sizing
  // keywords, shrink-to-fit, min/max clamping, or subgrid overrides), we rerun track sizing so
  // percentages resolve against the final container size.
  if width_was_indefinite {
    inner_node_size.width = Some(container_content_box.width);
  }
  if height_was_indefinite {
    inner_node_size.height = Some(container_content_box.height);
  }
  let available_grid_space_for_rerun = available_grid_space.maybe_set(inner_node_size);

  // Column sizing must be re-run (once) if:
  //   - The grid container's width was initially indefinite and there are any columns with percentage track sizing functions
  //   - Any grid item crossing an intrinsically sized track's min content contribution width has changed
  // TODO: Only rerun sizing for tracks that actually require it rather than for all tracks if any need it.
  let mut rerun_column_sizing;

  let has_percentage_column = columns.iter().any(|track| track.uses_percentage());
  rerun_column_sizing = width_was_indefinite && has_percentage_column;

  if !rerun_column_sizing {
    // For most items, intrinsic inline-size contributions do not depend on block-size resolution.
    // The primary cross-axis dependency is aspect ratio. Avoid a full re-measure scan unless there
    // are any aspect-ratio items that cross intrinsic columns.
    let has_aspect_ratio_crossing_intrinsic_column = items
      .iter()
      .any(|item| item.crosses_intrinsic_column && item.aspect_ratio.is_some());

    if has_aspect_ratio_crossing_intrinsic_column {
      // Precompute prefix sums of the other-axis track sizes so each item's spanned other-axis
      // available space can be computed in O(1) rather than summing over its track span.
      //
      // Note: we include `content_alignment_adjustment` exactly as `GridItem::available_space()` does.
      let mut row_prefix_sum: Vec<f32> = Vec::with_capacity(rows.len() + 1);
      row_prefix_sum.push(0.0);
      let mut running_row_sum = 0.0;
      for track in rows.iter() {
        running_row_sum += track.base_size + track.content_alignment_adjustment;
        row_prefix_sum.push(running_row_sum);
      }
      let gutter_percentage_basis = Some(inner_node_size.width.unwrap_or(0.0));
      let column_percentage_basis = |track: &GridTrack| {
        if track.kind == GridTrackKind::Gutter {
          gutter_percentage_basis
        } else {
          inner_node_size.width
        }
      };
      // Rerun detection probes can be expensive. Build prefix sums for the track predicates we
      // need so per-item checks are O(1) instead of O(span).
      let mut prefix_flex_probe_relevant: Vec<u32> = Vec::with_capacity(columns.len() + 1);
      let mut prefix_nonflex_probe_relevant: Vec<u32> = Vec::with_capacity(columns.len() + 1);
      prefix_flex_probe_relevant.push(0);
      prefix_nonflex_probe_relevant.push(0);
      let mut running_flex = 0u32;
      let mut running_nonflex = 0u32;
      for track in columns.iter() {
        let flex_relevant = track.is_flexible() && track.min_track_sizing_function.is_intrinsic();
        let nonflex_relevant = track.min_track_sizing_function.is_intrinsic()
          || !track
            .max_track_sizing_function
            .has_definite_value(column_percentage_basis(track));

        running_flex += u32::from(flex_relevant);
        running_nonflex += u32::from(nonflex_relevant);
        prefix_flex_probe_relevant.push(running_flex);
        prefix_nonflex_probe_relevant.push(running_nonflex);
      }
      let range_has_any =
        |prefix: &[u32], start: usize, end: usize| -> bool { prefix[end] - prefix[start] > 0 };

      // Note: we must iterate *all* probed items to update their intrinsic-contribution caches.
      // `GridItem` caches are keyed only by axis (not by available-space). If we short-circuit on
      // the first changed item (e.g. via `.any()`), then later probed items would retain stale
      // cached values and the subsequent rerun sizing pass could compute incorrect track sizes.
      let mut min_content_contribution_changed = false;
      for item in items
        .iter_mut()
        .filter(|item| item.crosses_intrinsic_column && item.aspect_ratio.is_some())
        .filter(|item| {
          // Column rerun detection only needs to probe min-content contributions if those contributions
          // could affect the track sizing algorithm for this item.
          //
          // In the flex batch, min-content contributions can matter for flexible tracks whose *minimum*
          // sizing function is intrinsic (step 11.5.1 minimum contributions may depend on min-content
          // sizing, and step 11.5.2 distributes min-content contributions). Growth-limit steps (11.5.5/11.5.6)
          // do not run for flex batches.
          //
          // In the non-flex batch, min-content contributions can affect both:
          // - base sizes via intrinsic minimums / content-based minimums (11.5.1 / 11.5.2)
          // - growth limits via intrinsic maximums (11.5.5)
          let range = item.track_range_excluding_lines(AbstractAxis::Inline);
          if item.crosses_flexible_column {
            range_has_any(&prefix_flex_probe_relevant, range.start, range.end)
          } else {
            range_has_any(&prefix_nonflex_probe_relevant, range.start, range.end)
          }
        })
      {
        let range = item.track_range_excluding_lines(AbstractAxis::Block);
        let other_axis_sum = row_prefix_sum[range.end] - row_prefix_sum[range.start];
        let mut available_space = Size::NONE;
        available_space.height = Some(other_axis_sum);
        let new_min_content_contribution = item.min_content_contribution(
          AbstractAxis::Inline,
          tree,
          available_space,
          inner_node_size,
        );

        let has_changed =
          Some(new_min_content_contribution) != item.min_content_contribution_cache.width;

        item.available_space_cache = Some(available_space);
        item.min_content_contribution_cache.width = Some(new_min_content_contribution);
        item.max_content_contribution_cache.width = None;
        item.minimum_contribution_cache.width = None;

        min_content_contribution_changed |= has_changed;
      }
      rerun_column_sizing = min_content_contribution_changed;
    }
  } else {
    // Clear intrisic width caches
    items.iter_mut().for_each(|item| {
      item.available_space_cache = None;
      item.min_content_contribution_cache.width = None;
      item.max_content_contribution_cache.width = None;
      item.minimum_contribution_cache.width = None;
    });
  }

  if rerun_column_sizing {
    // Re-run track sizing algorithm for Inline axis
    track_sizing_algorithm(
      tree,
      AbstractAxis::Inline,
      min_size.get(AbstractAxis::Inline),
      max_size.get(AbstractAxis::Inline),
      justify_content,
      align_content,
      available_grid_space_for_rerun,
      inner_node_size,
      &mut columns,
      &mut rows,
      &mut items,
      |track: &GridTrack, _, _| Some(track.base_size),
      has_align_self_baseline_item,
    );

    // The first row sizing pass may have already consumed the overrides recorded earlier. Refresh
    // them now that column sizes have been rerun so subsequent block-axis measurements inherit the
    // updated track sizes.
    // As above, refresh the inline-axis (horizontal) overrides after rerunning column sizing so
    // subsequent block-axis measurements of subgrids see the updated track sizes.
    let (record_rows, record_columns) = (false, true);
    record_subgrid_overrides(
      tree,
      &items,
      &parent_row_line_names,
      &parent_col_line_names,
      template_areas_for_children,
      &rows,
      &columns,
      style.axes_swapped(),
      record_rows,
      record_columns,
    );
  }

  // Row sizing must be re-run (once) if:
  //   - The grid container's height was initially indefinite and there are any row tracks with percentage sizing
  //   - The grid container's width was initially indefinite and there are any row gaps with percentage sizing
  //   - Any grid item crossing an intrinsically sized track's min content contribution height has changed
  // TODO: Only rerun sizing for tracks that actually require it rather than for all tracks if any need it.
  let mut rerun_row_sizing;

  let has_percentage_row_track = rows
    .iter()
    .any(|track| track.kind == GridTrackKind::Track && track.uses_percentage());
  let has_percentage_row_gap = rows
    .iter()
    .any(|track| track.kind == GridTrackKind::Gutter && track.uses_percentage());
  rerun_row_sizing = (height_was_indefinite && has_percentage_row_track)
    || (width_was_indefinite && has_percentage_row_gap);

  if !rerun_row_sizing {
    // As with the inline-axis rerun check above, avoid remeasuring every item unless it could
    // legitimately have a cross-axis dependency (aspect ratio).
    let has_aspect_ratio_crossing_intrinsic_row = items
      .iter()
      .any(|item| item.crosses_intrinsic_row && item.aspect_ratio.is_some());

    if has_aspect_ratio_crossing_intrinsic_row {
      // Precompute prefix sums of the other-axis track sizes so each item's spanned other-axis
      // available space can be computed in O(1) rather than summing over its track span.
      //
      // Note: we include `content_alignment_adjustment` exactly as `GridItem::available_space()` does.
      let mut column_prefix_sum: Vec<f32> = Vec::with_capacity(columns.len() + 1);
      column_prefix_sum.push(0.0);
      let mut running_column_sum = 0.0;
      for track in columns.iter() {
        running_column_sum += track.base_size + track.content_alignment_adjustment;
        column_prefix_sum.push(running_column_sum);
      }
      let gutter_percentage_basis = Some(inner_node_size.width.unwrap_or(0.0));
      let row_percentage_basis = |track: &GridTrack| {
        if track.kind == GridTrackKind::Gutter {
          gutter_percentage_basis
        } else {
          inner_node_size.height
        }
      };
      let mut prefix_flex_probe_relevant: Vec<u32> = Vec::with_capacity(rows.len() + 1);
      let mut prefix_nonflex_probe_relevant: Vec<u32> = Vec::with_capacity(rows.len() + 1);
      prefix_flex_probe_relevant.push(0);
      prefix_nonflex_probe_relevant.push(0);
      let mut running_flex = 0u32;
      let mut running_nonflex = 0u32;
      for track in rows.iter() {
        let flex_relevant = track.is_flexible() && track.min_track_sizing_function.is_intrinsic();
        let nonflex_relevant = track.min_track_sizing_function.is_intrinsic()
          || !track
            .max_track_sizing_function
            .has_definite_value(row_percentage_basis(track));

        running_flex += u32::from(flex_relevant);
        running_nonflex += u32::from(nonflex_relevant);
        prefix_flex_probe_relevant.push(running_flex);
        prefix_nonflex_probe_relevant.push(running_nonflex);
      }
      let range_has_any =
        |prefix: &[u32], start: usize, end: usize| -> bool { prefix[end] - prefix[start] > 0 };

      // As with the inline-axis rerun probe, we must iterate all probed items to refresh their
      // axis-specific intrinsic contribution caches before running the rerun sizing pass.
      let mut min_content_contribution_changed = false;
      for item in items
        .iter_mut()
        .filter(|item| item.crosses_intrinsic_row && item.aspect_ratio.is_some())
        .filter(|item| {
          // Row rerun detection only needs to probe min-content contributions if those contributions
          // could affect the track sizing algorithm for this item.
          //
          // The logic mirrors the inline-axis rerun detection:
          // - In the flex batch, min-content contributions can only influence flexible tracks whose
          //   *minimum* sizing function is intrinsic (11.5.1/11.5.2). Growth-limit steps do not run.
          // - In the non-flex batch, min-content contributions can influence both base sizes (11.5.1/11.5.2)
          //   and growth limits (11.5.5).
          let range = item.track_range_excluding_lines(AbstractAxis::Block);
          if item.crosses_flexible_row {
            range_has_any(&prefix_flex_probe_relevant, range.start, range.end)
          } else {
            range_has_any(&prefix_nonflex_probe_relevant, range.start, range.end)
          }
        })
      {
        let range = item.track_range_excluding_lines(AbstractAxis::Inline);
        let other_axis_sum = column_prefix_sum[range.end] - column_prefix_sum[range.start];
        let mut available_space = Size::NONE;
        available_space.width = Some(other_axis_sum);
        let new_min_content_contribution = item.min_content_contribution(
          AbstractAxis::Block,
          tree,
          available_space,
          inner_node_size,
        );

        let has_changed =
          Some(new_min_content_contribution) != item.min_content_contribution_cache.height;

        item.available_space_cache = Some(available_space);
        item.min_content_contribution_cache.height = Some(new_min_content_contribution);
        item.max_content_contribution_cache.height = None;
        item.minimum_contribution_cache.height = None;

        min_content_contribution_changed |= has_changed;
      }
      rerun_row_sizing = min_content_contribution_changed;
    }
  } else {
    items.iter_mut().for_each(|item| {
      // Clear intrinsic height caches
      item.available_space_cache = None;
      item.min_content_contribution_cache.height = None;
      item.max_content_contribution_cache.height = None;
      item.minimum_contribution_cache.height = None;
    });
  }

  if rerun_row_sizing {
    track_sizing_algorithm(
      tree,
      AbstractAxis::Block,
      min_size.get(AbstractAxis::Block),
      max_size.get(AbstractAxis::Block),
      align_content,
      justify_content,
      available_grid_space_for_rerun,
      inner_node_size,
      &mut rows,
      &mut columns,
      &mut items,
      |track: &GridTrack, _, _| Some(track.base_size),
      has_justify_self_baseline_item,
    );
  }

  // Capture subgrid overrides now that track sizes are resolved and drop virtual contributions
  record_subgrid_overrides(
    tree,
    &items,
    &parent_row_line_names,
    &parent_col_line_names,
    template_areas_for_children,
    &rows,
    &columns,
    style.axes_swapped(),
    true,
    true,
  );
  items.retain(|item| !item.is_virtual);

  // 8. Track Alignment

  let start_end_axis_positive = style.start_end_axis_positive();

  // Align columns
  align_tracks(
    container_content_box.get(AbstractAxis::Inline),
    Line {
      start: padding.left,
      end: padding.right,
    },
    Line {
      start: border.left,
      end: border.right,
    },
    &mut columns,
    justify_content,
    start_end_axis_positive.x,
  );
  // Align rows
  align_tracks(
    container_content_box.get(AbstractAxis::Block),
    Line {
      start: padding.top,
      end: padding.bottom,
    },
    Line {
      start: border.top,
      end: border.bottom,
    },
    &mut rows,
    align_content,
    start_end_axis_positive.y,
  );

  // 9. Size, Align, and Position Grid Items

  #[cfg_attr(not(feature = "content_size"), allow(unused_mut))]
  let mut item_content_size_contribution = Size::ZERO;

  // Sort items back into original order to allow them to be matched up with styles
  items.sort_unstable_by_key(|item| item.source_order);

  let container_alignment_styles = InBothAbsAxis {
    horizontal: justify_items,
    vertical: align_items,
  };

  // Position in-flow children (stored in items vector)
  for (index, item) in items.iter_mut().enumerate() {
    let grid_area = Rect {
      top: rows[item.row_indexes.start as usize + 1].offset,
      bottom: rows[item.row_indexes.end as usize].offset,
      left: columns[item.column_indexes.start as usize + 1].offset,
      right: columns[item.column_indexes.end as usize].offset,
    };
    #[cfg_attr(not(feature = "content_size"), allow(unused_variables))]
    let (content_size_contribution, y_position, height) = align_and_position_item(
      tree,
      item.node,
      index as u32,
      grid_area,
      container_alignment_styles,
      item.baseline_shim,
      item.extra_margin,
    );
    item.y_position = y_position;
    item.height = height;

    #[cfg(feature = "content_size")]
    {
      item_content_size_contribution =
        item_content_size_contribution.f32_max(content_size_contribution);
    }
  }

  // Position hidden and absolutely positioned children
  let mut order = items.len() as u32;
  (0..tree.child_count(node)).for_each(|index| {
    let child = tree.get_child_id(node, index);
    let child_style = tree.get_grid_child_style(child);

    // Position hidden child
    if child_style.box_generation_mode() == BoxGenerationMode::None {
      drop(child_style);
      tree.set_unrounded_layout(child, &Layout::with_order(order));
      tree.perform_child_layout(
        child,
        Size::NONE,
        Size::NONE,
        Size::MAX_CONTENT,
        SizingMode::InherentSize,
        Line::FALSE,
      );
      order += 1;
      return;
    }

    // Position absolutely positioned child
    if child_style.position() == Position::Absolute {
      // Convert grid-col-{start/end} into Option's of indexes into the columns vector
      // The Option is None if the style property is Auto and an unresolvable Span
      let maybe_col_indexes = name_resolver
        .resolve_column_names(&child_style.grid_column())
        .into_origin_zero(final_col_counts.explicit)
        .resolve_absolutely_positioned_grid_tracks()
        .map(|maybe_grid_line| {
          maybe_grid_line
            .and_then(|line: OriginZeroLine| line.try_into_track_vec_index(final_col_counts))
        });
      // Convert grid-row-{start/end} into Option's of indexes into the row vector
      // The Option is None if the style property is Auto and an unresolvable Span
      let maybe_row_indexes = name_resolver
        .resolve_row_names(&child_style.grid_row())
        .into_origin_zero(final_row_counts.explicit)
        .resolve_absolutely_positioned_grid_tracks()
        .map(|maybe_grid_line| {
          maybe_grid_line
            .and_then(|line: OriginZeroLine| line.try_into_track_vec_index(final_row_counts))
        });

      let grid_area = Rect {
        top: maybe_row_indexes
          .start
          .map(|index| rows[index].offset)
          .unwrap_or(border.top),
        bottom: maybe_row_indexes
          .end
          .map(|index| rows[index].offset)
          .unwrap_or(container_border_box.height - border.bottom - scrollbar_gutter.y),
        left: maybe_col_indexes
          .start
          .map(|index| columns[index].offset)
          .unwrap_or(border.left),
        right: maybe_col_indexes
          .end
          .map(|index| columns[index].offset)
          .unwrap_or(container_border_box.width - border.right - scrollbar_gutter.x),
      };
      drop(child_style);

      // TODO: Baseline alignment support for absolutely positioned items (should check if is actuallty specified)
      #[cfg_attr(not(feature = "content_size"), allow(unused_variables))]
      let (content_size_contribution, _, _) = align_and_position_item(
        tree,
        child,
        order,
        grid_area,
        container_alignment_styles,
        Point::ZERO,
        Rect::default(),
      );
      #[cfg(feature = "content_size")]
      {
        item_content_size_contribution =
          item_content_size_contribution.f32_max(content_size_contribution);
      }

      order += 1;
    }
  });

  // Set detailed grid information
  #[cfg(feature = "detailed_layout_info")]
  {
    // Expose expanded line-name vectors for integrations that need to resolve named placements
    // outside of Taffy's in-flow grid-item pipeline (e.g. for absolutely positioned static
    // positioning).
    //
    // `NamedLineResolver::expanded_*_line_names()` returns 1-indexed line-name vectors for the
    // explicit grid only. However, detailed track info and grid item placement operate in the
    // *full* grid line coordinate space that includes leading/trailing implicit tracks. Pad the
    // expanded vectors so `line_names[line - 1]` is valid for the same line-number space used by
    // `DetailedGridItemsInfo`.
    fn pad_expanded_line_names(
      mut names: Vec<Vec<String>>,
      track_counts: TrackCounts,
    ) -> Vec<Vec<String>> {
      let leading = track_counts.negative_implicit as usize;
      if leading > 0 {
        let mut padded = Vec::with_capacity(leading + names.len());
        for _ in 0..leading {
          padded.push(Vec::new());
        }
        padded.append(&mut names);
        names = padded;
      }
      let total_tracks = (track_counts
        .negative_implicit
        .saturating_add(track_counts.explicit)
        .saturating_add(track_counts.positive_implicit)) as usize;
      let total_lines = total_tracks.saturating_add(1);
      if names.len() < total_lines {
        names.resize_with(total_lines, Vec::new);
      }
      names
    }

    let row_line_names =
      pad_expanded_line_names(name_resolver.expanded_row_line_names(), final_row_counts);
    let column_line_names =
      pad_expanded_line_names(name_resolver.expanded_column_line_names(), final_col_counts);

    tree.set_detailed_grid_info(
      node,
      DetailedGridInfo {
        rows: DetailedGridTracksInfo::from_grid_tracks_and_track_count(final_row_counts, rows),
        columns: DetailedGridTracksInfo::from_grid_tracks_and_track_count(
          final_col_counts,
          columns,
        ),
        items: items
          .iter()
          .map(DetailedGridItemsInfo::from_grid_item)
          .collect(),
        row_line_names,
        column_line_names,
      },
    );
  }

  // If there are not items then return just the container size (no baseline)
  if items.is_empty() {
    return LayoutOutput::from_outer_size(container_border_box);
  }

  // Determine the grid container baseline(s) (currently we only compute the first baseline)
  let grid_container_baseline: f32 = {
    // Find the first row containing items without sorting.
    //
    // Items are already in `source_order` at this point, so iterating the slice preserves the
    // same tie-breaking behaviour as the previous stable row-start sort.
    let first_row = items
      .iter()
      .map(|item| item.row_indexes.start)
      .min()
      .unwrap();

    let mut first_row_first_item: Option<&GridItem> = None;
    let mut first_row_first_baseline_item: Option<&GridItem> = None;

    for item in items.iter() {
      if item.row_indexes.start != first_row {
        continue;
      }

      if first_row_first_item.is_none() {
        first_row_first_item = Some(item);
      }

      if item.align_self == AlignSelf::Baseline {
        first_row_first_baseline_item = Some(item);
        break;
      }
    }

    let item = first_row_first_baseline_item.unwrap_or_else(|| first_row_first_item.unwrap());
    item.y_position + item.baseline.unwrap_or(item.height)
  };

  LayoutOutput::from_sizes_and_baselines(
    container_border_box,
    item_content_size_contribution,
    Point {
      x: None,
      y: Some(grid_container_baseline),
    },
  )
}

/// Information from the computation of grid
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "detailed_layout_info")]
pub struct DetailedGridInfo {
  /// <https://drafts.csswg.org/css-grid-1/#grid-row>
  pub rows: DetailedGridTracksInfo,
  /// <https://drafts.csswg.org/css-grid-1/#grid-column>
  pub columns: DetailedGridTracksInfo,
  /// <https://drafts.csswg.org/css-grid-1/#grid-items>
  pub items: Vec<DetailedGridItemsInfo>,
  /// Expanded (auto-repeat + area-derived) line names for rows.
  ///
  /// The indexing is the same 1-indexed line-number coordinate space used by
  /// `DetailedGridItemsInfo::{row_start,row_end}`: `row_line_names[line - 1]` yields the line-name
  /// list for that line. To achieve this, the underlying `NamedLineResolver` line-name vectors
  /// (which only cover explicit tracks) are padded with empty entries for any leading/trailing
  /// implicit tracks (`negative_implicit_tracks`/`positive_implicit_tracks`).
  pub row_line_names: Vec<Vec<String>>,
  /// Expanded (auto-repeat + area-derived) line names for columns.
  ///
  /// See `row_line_names` for indexing details.
  pub column_line_names: Vec<Vec<String>>,
}

/// Information from the computation of grids tracks
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "detailed_layout_info")]
pub struct DetailedGridTracksInfo {
  /// Number of leading implicit grid tracks
  pub negative_implicit_tracks: u16,
  /// Number of explicit grid tracks
  pub explicit_tracks: u16,
  /// Number of trailing implicit grid tracks
  pub positive_implicit_tracks: u16,

  /// Gutters between tracks
  pub gutters: Vec<f32>,
  /// The used size of the tracks
  pub sizes: Vec<f32>,
}

#[cfg(feature = "detailed_layout_info")]
impl DetailedGridTracksInfo {
  /// Get the base_size of [`GridTrack`] with a kind [`types::GridTrackKind`]
  #[inline(always)]
  fn grid_track_base_size_of_kind(grid_tracks: &[GridTrack], kind: GridTrackKind) -> Vec<f32> {
    grid_tracks
      .iter()
      .filter_map(|track| match track.kind == kind {
        true => Some(track.base_size),
        false => None,
      })
      .collect()
  }

  /// Get the sizes of the gutters
  fn gutters_from_grid_track_layout(grid_tracks: &[GridTrack]) -> Vec<f32> {
    DetailedGridTracksInfo::grid_track_base_size_of_kind(grid_tracks, GridTrackKind::Gutter)
  }

  /// Get the sizes of the tracks
  fn sizes_from_grid_track_layout(grid_tracks: &[GridTrack]) -> Vec<f32> {
    DetailedGridTracksInfo::grid_track_base_size_of_kind(grid_tracks, GridTrackKind::Track)
  }

  /// Construct DetailedGridTracksInfo from TrackCounts and GridTracks
  fn from_grid_tracks_and_track_count(
    track_count: TrackCounts,
    grid_tracks: Vec<GridTrack>,
  ) -> Self {
    DetailedGridTracksInfo {
      negative_implicit_tracks: track_count.negative_implicit,
      explicit_tracks: track_count.explicit,
      positive_implicit_tracks: track_count.positive_implicit,
      gutters: DetailedGridTracksInfo::gutters_from_grid_track_layout(&grid_tracks),
      sizes: DetailedGridTracksInfo::sizes_from_grid_track_layout(&grid_tracks),
    }
  }
}

/// Grid area information from the placement algorithm
///
/// The values is 1-indexed grid line numbers bounding the area.
/// This matches the Chrome and Firefox's format as of 2nd Jan 2024.
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "detailed_layout_info")]
pub struct DetailedGridItemsInfo {
  /// row-start with 1-indexed grid line numbers
  pub row_start: u16,
  /// row-end with 1-indexed grid line numbers
  pub row_end: u16,
  /// column-start with 1-indexed grid line numbers
  pub column_start: u16,
  /// column-end with 1-indexed grid line numbers
  pub column_end: u16,
}

/// Grid area information from the placement algorithm
#[cfg(feature = "detailed_layout_info")]
impl DetailedGridItemsInfo {
  /// Construct from GridItems
  #[inline(always)]
  fn from_grid_item(grid_item: &GridItem) -> Self {
    /// Conversion from the indexes of Vec<GridTrack> into 1-indexed grid line numbers. See [`GridItem::row_indexes`] or [`GridItem::column_indexes`]
    #[inline(always)]
    fn to_one_indexed_grid_line(grid_track_index: u16) -> u16 {
      grid_track_index / 2 + 1
    }

    DetailedGridItemsInfo {
      row_start: to_one_indexed_grid_line(grid_item.row_indexes.start),
      row_end: to_one_indexed_grid_line(grid_item.row_indexes.end),
      column_start: to_one_indexed_grid_line(grid_item.column_indexes.start),
      column_end: to_one_indexed_grid_line(grid_item.column_indexes.end),
    }
  }
}
