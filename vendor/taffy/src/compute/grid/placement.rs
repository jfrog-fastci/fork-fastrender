//! Implements placing items in the grid and resolving the implicit grid.
//! <https://www.w3.org/TR/css-grid-1/#placement>
use super::types::{CellOccupancyMatrix, CellOccupancyState, GridItem};
use super::limits::{clamp_grid_area_to_implicit_grid_limit, max_implicit_tracks_per_side};
use super::{NamedLineResolver, OriginZeroLine};
use crate::geometry::Line;
use crate::geometry::{AbsoluteAxis, InBothAbsAxis};
use crate::style::{AlignItems, GridAreaAxis, GridAutoFlow, OriginZeroGridPlacementWithNamedSpan};
use crate::tree::NodeId;
use crate::util::check_layout_abort;
use crate::util::sys::Vec;
use crate::{CoreStyle, GridItemStyle};
use core::cmp::{max, min};

#[inline]
fn add_i32_clamped(line: OriginZeroLine, delta: i32) -> OriginZeroLine {
  OriginZeroLine((line.0 as i32 + delta).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
}

#[inline]
fn clamp_span_to_explicit_tracks(
  span: Line<OriginZeroLine>,
  explicit_track_count: u16,
) -> Line<OriginZeroLine> {
  // Subgrids do not generate implicit tracks in the subgridded axis. When an item's placement would
  // otherwise extend beyond the explicit grid (e.g. `grid-column: 2` in a 1-track subgrid), clamp
  // the resolved span back into the explicit track range. This matches browser behaviour where the
  // item falls back into the available tracks rather than expanding the grid.
  // `OriginZeroLine` is backed by i16, so extremely large explicit grids are already outside the
  // representable coordinate range. Clamp the effective track count to avoid i16 wrapping.
  let track_count = min(explicit_track_count, i16::MAX as u16) as i32;
  if track_count <= 0 {
    return Line {
      start: OriginZeroLine(0),
      end: OriginZeroLine(0),
    };
  }

  let mut span_len = (span.end.0 as i32) - (span.start.0 as i32);
  if span_len <= 0 {
    span_len = 1;
  }
  if span_len > track_count {
    span_len = track_count;
  }

  let max_start = track_count - span_len;
  let start = (span.start.0 as i32).clamp(0, max_start);
  Line {
    start: OriginZeroLine(start as i16),
    end: OriginZeroLine((start + span_len) as i16),
  }
}

#[inline]
fn grid_area_axis(axis: AbsoluteAxis) -> GridAreaAxis {
  match axis {
    AbsoluteAxis::Horizontal => GridAreaAxis::Column,
    AbsoluteAxis::Vertical => GridAreaAxis::Row,
  }
}

#[inline]
fn axis_item<T>(items: &InBothAbsAxis<T>, axis: AbsoluteAxis) -> &T {
  match axis {
    AbsoluteAxis::Horizontal => &items.horizontal,
    AbsoluteAxis::Vertical => &items.vertical,
  }
}

#[inline]
fn placement_is_definite<S: crate::CheapCloneStr>(
  placement: &Line<OriginZeroGridPlacementWithNamedSpan<S>>,
) -> bool {
  use OriginZeroGridPlacementWithNamedSpan as GP;
  matches!(&placement.start, GP::Line(_)) || matches!(&placement.end, GP::Line(_))
}

fn resolve_definite_grid_lines<S: crate::CheapCloneStr>(
  placement: &Line<OriginZeroGridPlacementWithNamedSpan<S>>,
  explicit_track_count: u16,
) -> Line<OriginZeroLine> {
  use OriginZeroGridPlacementWithNamedSpan as GP;
  let span = match (&placement.start, &placement.end) {
    (GP::Line(line1), GP::Line(line2)) => {
      if line1 == line2 {
        Line {
          start: *line1,
          end: add_i32_clamped(*line1, 1),
        }
      } else {
        Line {
          start: min(*line1, *line2),
          end: max(*line1, *line2),
        }
      }
    }
    (GP::Line(line), GP::Span(span)) => Line {
      start: *line,
      end: add_i32_clamped(*line, *span as i32),
    },
    (GP::Line(line), GP::Auto) => Line {
      start: *line,
      end: add_i32_clamped(*line, 1),
    },
    (GP::Span(span), GP::Line(line)) => Line {
      start: add_i32_clamped(*line, -(*span as i32)),
      end: *line,
    },
    (GP::Auto, GP::Line(line)) => Line {
      start: add_i32_clamped(*line, -1),
      end: *line,
    },
    _ => panic!("resolve_definite_grid_lines should only be called on definite grid tracks"),
  };

  clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count)
}

#[inline]
fn normalize_resolved_span(span: Line<OriginZeroLine>) -> Line<OriginZeroLine> {
  let mut start = span.start;
  let mut end = span.end;
  if start.0 > end.0 {
    core::mem::swap(&mut start, &mut end);
  }
  if start == end {
    // Use clamped arithmetic so we don't panic in debug builds when `start` is already at
    // `i16::MAX` (in which case `start + 1` would overflow/wrap).
    end = add_i32_clamped(start, 1);
  }
  Line { start, end }
}

#[inline]
fn fixed_indefinite_span<S: crate::CheapCloneStr>(
  placement: &Line<OriginZeroGridPlacementWithNamedSpan<S>>,
) -> u16 {
  use OriginZeroGridPlacementWithNamedSpan as GP;
  match (&placement.start, &placement.end) {
    (GP::Auto, GP::Auto) => 1,
    (GP::Auto, GP::Span(span)) | (GP::Span(span), GP::Auto) | (GP::Span(span), GP::Span(_)) => {
      *span
    }
    (GP::NamedSpan(_, _), _) | (_, GP::NamedSpan(_, _)) => {
      panic!("fixed_indefinite_span cannot be computed for NamedSpan placements")
    }
    (GP::Line(_), _) | (_, GP::Line(_)) => {
      panic!("fixed_indefinite_span should only be called on indefinite grid tracks")
    }
  }
}

#[inline]
fn initial_candidate<S: crate::CheapCloneStr>(
  placement: &Line<OriginZeroGridPlacementWithNamedSpan<S>>,
  cursor: OriginZeroLine,
  explicit_track_count: u16,
) -> OriginZeroLine {
  use OriginZeroGridPlacementWithNamedSpan as GP;
  match (&placement.start, &placement.end) {
    (GP::NamedSpan(_, span), GP::Auto) => {
      let candidate = add_i32_clamped(cursor, *span as i32);
      clamp_grid_area_to_implicit_grid_limit(
        Line {
          start: candidate,
          end: add_i32_clamped(candidate, 1),
        },
        explicit_track_count,
      )
      .start
    }
    _ => cursor,
  }
}

fn resolve_indefinite_grid_lines<S: crate::CheapCloneStr>(
  placement: &Line<OriginZeroGridPlacementWithNamedSpan<S>>,
  candidate: OriginZeroLine,
  named_line_resolver: &NamedLineResolver<S>,
  axis: AbsoluteAxis,
  explicit_track_count: u16,
) -> Line<OriginZeroLine> {
  use OriginZeroGridPlacementWithNamedSpan as GP;
  let axis = grid_area_axis(axis);
  let span = match (&placement.start, &placement.end) {
    (GP::Auto, GP::NamedSpan(name, span)) => {
      let start = candidate;
      let end = named_line_resolver.resolve_named_span_end_line(name, *span, axis, start);
      Line { start, end }
    }
    (GP::NamedSpan(name, span), GP::Auto) => {
      let end = candidate;
      let start = named_line_resolver.resolve_named_span_start_line(name, *span, axis, end);
      Line { start, end }
    }
    _ => {
      let span = fixed_indefinite_span(placement);
      let start = candidate;
      Line {
        start,
        end: add_i32_clamped(start, span as i32),
      }
    }
  };

  let span = normalize_resolved_span(span);
  clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count)
}

/// 8.5. Grid Item Placement Algorithm
/// Place items into the grid, generating new rows/column into the implicit grid as required
///
/// [Specification](https://www.w3.org/TR/css-grid-2/#auto-placement-algo)
pub(super) fn place_grid_items<'a, S, ChildIter>(
  cell_occupancy_matrix: &mut CellOccupancyMatrix,
  items: &mut Vec<GridItem>,
  children_iter: impl Fn() -> ChildIter,
  grid_auto_flow: GridAutoFlow,
  align_items: AlignItems,
  justify_items: AlignItems,
  named_line_resolver: &NamedLineResolver<<S as CoreStyle>::CustomIdent>,
  disallow_implicit_tracks: InBothAbsAxis<bool>,
  get_child_subgrid_auto_span: impl Fn(NodeId) -> InBothAbsAxis<Option<u16>>,
) where
  S: GridItemStyle + 'a,
  ChildIter: Iterator<Item = (usize, NodeId, S)>,
{
  let primary_axis = grid_auto_flow.primary_axis();
  let secondary_axis = primary_axis.other_axis();

  let explicit_track_counts = InBothAbsAxis {
    horizontal: cell_occupancy_matrix
      .track_counts(AbsoluteAxis::Horizontal)
      .explicit,
    vertical: cell_occupancy_matrix
      .track_counts(AbsoluteAxis::Vertical)
      .explicit,
  };
  let clamp_span = |axis: AbsoluteAxis, span: Line<OriginZeroLine>| {
    let span = if disallow_implicit_tracks.get(axis) {
      clamp_span_to_explicit_tracks(span, explicit_track_counts.get(axis))
    } else {
      span
    };

    clamp_grid_area_to_implicit_grid_limit(span, explicit_track_counts.get(axis))
  };

  let map_child_style_to_origin_zero_placement = {
    let explicit_col_count = cell_occupancy_matrix
      .track_counts(AbsoluteAxis::Horizontal)
      .explicit;
    let explicit_row_count = cell_occupancy_matrix
      .track_counts(AbsoluteAxis::Vertical)
      .explicit;
    move |(index, node, style): (usize, NodeId, S)| -> (_, _, _, S) {
      let origin_zero_placement = InBothAbsAxis {
        horizontal: named_line_resolver
          .resolve_column_names(&style.grid_column())
          .into_origin_zero(explicit_col_count),
        vertical: named_line_resolver
          .resolve_row_names(&style.grid_row())
          .into_origin_zero(explicit_row_count),
      };

      // CSS Grid 2: <https://drafts.csswg.org/css-grid-2/#grid-span>
      //
      // If a grid item is a subgrid container in an axis and its placement in that axis is fully
      // automatic (both edges `auto` with no explicit span), then the default span is derived from
      // the subgrid's `<line-name-list>` (line count - 1).
      //
      // FastRender stores the subgrid line-name lists on the *container* style, not on the grid
      // item style exposed via the `GridItemStyle` trait. Query the container info from the tree
      // via the supplied callback.
      let subgrid_auto_span = get_child_subgrid_auto_span(node);
      let mut origin_zero_placement = origin_zero_placement;
      use OriginZeroGridPlacementWithNamedSpan as GP;
      if let Some(span) = subgrid_auto_span.horizontal {
        if matches!(origin_zero_placement.horizontal.start, GP::Auto)
          && matches!(origin_zero_placement.horizontal.end, GP::Auto)
        {
          origin_zero_placement.horizontal.end = GP::Span(span);
        }
      }
      if let Some(span) = subgrid_auto_span.vertical {
        if matches!(origin_zero_placement.vertical.start, GP::Auto)
          && matches!(origin_zero_placement.vertical.end, GP::Auto)
        {
          origin_zero_placement.vertical.end = GP::Span(span);
        }
      }
      (index, node, origin_zero_placement, style)
    }
  };

  // Collect all children first. The placement algorithm operates over multiple passes, but
  // each pass preserves the original order of items within that category.
  let all_children: Vec<_> = children_iter()
    .map(map_child_style_to_origin_zero_placement)
    .collect();

  // Initialize auto-placement cursor
  let primary_neg_tracks = cell_occupancy_matrix
    .track_counts(primary_axis)
    .negative_implicit as i16;
  let secondary_neg_tracks = cell_occupancy_matrix
    .track_counts(secondary_axis)
    .negative_implicit as i16;
  let grid_start_position = (
    OriginZeroLine(-primary_neg_tracks),
    OriginZeroLine(-secondary_neg_tracks),
  );
  let mut grid_position = grid_start_position;

  // 8.5. Grid Item Placement Algorithm
  // Step 1. Place all definitely positioned items (both axes definite)
  for (index, child_node, child_placement, style) in all_children.iter() {
    let primary_definite = placement_is_definite(axis_item(child_placement, primary_axis));
    let secondary_definite = placement_is_definite(axis_item(child_placement, secondary_axis));

    if primary_definite && secondary_definite {
      #[cfg(all(test, feature = "debug"))]
      println!("Definite Item {}\n==============", index);

      let (primary_span, secondary_span) =
        place_definite_grid_item(child_placement, primary_axis, explicit_track_counts);
      let primary_span = clamp_span(primary_axis, primary_span);
      let secondary_span = clamp_span(secondary_axis, secondary_span);
      record_grid_placement(
        cell_occupancy_matrix,
        items,
        *child_node,
        *index,
        style,
        align_items,
        justify_items,
        primary_axis,
        primary_span,
        secondary_span,
        CellOccupancyState::DefinitelyPlaced,
      );
    }
  }

  // Step 2/3. Place items with one axis definite and the other auto
  for (index, child_node, child_placement, style) in all_children.iter() {
    let primary_definite = placement_is_definite(axis_item(child_placement, primary_axis));
    let secondary_definite = placement_is_definite(axis_item(child_placement, secondary_axis));

    if primary_definite == secondary_definite {
      continue;
    }

    #[cfg(all(test, feature = "debug"))]
    println!("Definite One Axis Item {}\n==============", index);

    // Determine which axis is definite and call the appropriate placement function
    let (primary_span, secondary_span) = if secondary_definite {
      // Secondary axis definite, primary auto - use existing function
      place_definite_secondary_axis_item(
        &*cell_occupancy_matrix,
        child_placement,
        grid_auto_flow,
        named_line_resolver,
      )
    } else {
      // Primary axis definite, secondary auto
      let primary_placement = resolve_definite_grid_lines(
        axis_item(child_placement, primary_axis),
        explicit_track_counts.get(primary_axis),
      );
      let secondary_start = match grid_auto_flow.is_dense() {
        true => cell_occupancy_matrix
          .track_counts(primary_axis.other_axis())
          .implicit_start_line(),
        false => grid_position.1,
      };

      // Find first free secondary position for this primary span
      let secondary_placement = axis_item(child_placement, primary_axis.other_axis());
      let mut sec_idx = initial_candidate(
        secondary_placement,
        secondary_start,
        cell_occupancy_matrix
          .track_counts(primary_axis.other_axis())
          .explicit,
      );
      let mut last_secondary_span: Option<Line<OriginZeroLine>> = None;
      loop {
        check_layout_abort();
        let secondary_span = resolve_indefinite_grid_lines(
          secondary_placement,
          sec_idx,
          named_line_resolver,
          primary_axis.other_axis(),
          cell_occupancy_matrix
            .track_counts(primary_axis.other_axis())
            .explicit,
        );
        if cell_occupancy_matrix.line_area_is_unoccupied(
          primary_axis,
          primary_placement,
          secondary_span,
        ) {
          break (primary_placement, secondary_span);
        }
        // If the resolved span stops changing, we've hit the UA-defined implicit-grid clamp
        // and further searching cannot produce a new placement. Return the clamped placement even
        // if it overlaps, rather than spinning indefinitely.
        if last_secondary_span == Some(secondary_span) {
          break (primary_placement, secondary_span);
        }
        last_secondary_span = Some(secondary_span);
        sec_idx += 1;
      }
    };
    let primary_span = clamp_span(primary_axis, primary_span);
    let secondary_span = clamp_span(secondary_axis, secondary_span);

    record_grid_placement(
      cell_occupancy_matrix,
      items,
      *child_node,
      *index,
      style,
      align_items,
      justify_items,
      primary_axis,
      primary_span,
      secondary_span,
      CellOccupancyState::AutoPlaced,
    );

    // Update cursor for next item (if not dense mode)
    if !grid_auto_flow.is_dense() {
      grid_position = (primary_span.end, secondary_span.start);
    }
  }

  // Step 4 ("auto-placement of the remaining items") should start scanning from the start of the
  // implicit grid, not from the cursor position left behind by steps 2/3.
  //
  // Carrying the cursor forward can cause auto-placed items to skip earlier available cells,
  // leaving leading holes. Real-world layouts (e.g. sticky-footer grids where the footer is
  // explicitly placed on the second row while the main content wrapper is auto-placed) rely on
  // auto items filling the first row even when a later item has a definite row placement.
  grid_position = grid_start_position;

  // Step 4. Auto placement of the remaining items
  for (index, child_node, child_placement, style) in all_children.iter() {
    let primary_definite = placement_is_definite(axis_item(child_placement, primary_axis));
    let secondary_definite = placement_is_definite(axis_item(child_placement, secondary_axis));

    if primary_definite || secondary_definite {
      continue;
    }

    #[cfg(all(test, feature = "debug"))]
    println!("\nAuto Item {}\n==============", index);

    // Compute placement
    let (primary_span, secondary_span) = place_indefinitely_positioned_item(
      &*cell_occupancy_matrix,
      child_placement,
      grid_auto_flow,
      grid_position,
      named_line_resolver,
    );
    let primary_span = clamp_span(primary_axis, primary_span);
    let secondary_span = clamp_span(secondary_axis, secondary_span);

    // Record item
    record_grid_placement(
      cell_occupancy_matrix,
      items,
      *child_node,
      *index,
      style,
      align_items,
      justify_items,
      primary_axis,
      primary_span,
      secondary_span,
      CellOccupancyState::AutoPlaced,
    );

    // Update cursor for next auto item
    grid_position = match grid_auto_flow.is_dense() {
      true => grid_start_position,
      false => (primary_span.end, secondary_span.start),
    }
  }
}

/// 8.5. Grid Item Placement Algorithm
/// Place a single definitely placed item into the grid
fn place_definite_grid_item<I: crate::CheapCloneStr>(
  placement: &InBothAbsAxis<Line<OriginZeroGridPlacementWithNamedSpan<I>>>,
  primary_axis: AbsoluteAxis,
  explicit_track_counts: InBothAbsAxis<u16>,
) -> (Line<OriginZeroLine>, Line<OriginZeroLine>) {
  // Resolve spans to tracks
  let primary_span = resolve_definite_grid_lines(
    axis_item(placement, primary_axis),
    explicit_track_counts.get(primary_axis),
  );
  let secondary_span = resolve_definite_grid_lines(
    axis_item(placement, primary_axis.other_axis()),
    explicit_track_counts.get(primary_axis.other_axis()),
  );

  (primary_span, secondary_span)
}

/// 8.5. Grid Item Placement Algorithm
/// Step 2. Place remaining children with definite secondary axis positions
fn place_definite_secondary_axis_item<I: crate::CheapCloneStr>(
  cell_occupancy_matrix: &CellOccupancyMatrix,
  placement: &InBothAbsAxis<Line<OriginZeroGridPlacementWithNamedSpan<I>>>,
  auto_flow: GridAutoFlow,
  named_line_resolver: &NamedLineResolver<I>,
) -> (Line<OriginZeroLine>, Line<OriginZeroLine>) {
  let primary_axis = auto_flow.primary_axis();
  let secondary_axis = primary_axis.other_axis();

  let secondary_axis_placement = resolve_definite_grid_lines(
    axis_item(placement, secondary_axis),
    cell_occupancy_matrix.track_counts(secondary_axis).explicit,
  );
  let primary_axis_grid_start_line = cell_occupancy_matrix
    .track_counts(primary_axis)
    .implicit_start_line();
  let starting_position = match auto_flow.is_dense() {
    true => primary_axis_grid_start_line,
    false => cell_occupancy_matrix
      .last_of_type(
        primary_axis,
        secondary_axis_placement.start,
        CellOccupancyState::AutoPlaced,
      )
      .unwrap_or(primary_axis_grid_start_line),
  };

  let primary_axis_placement_spec = axis_item(placement, primary_axis);
  let mut position: OriginZeroLine = initial_candidate(
    primary_axis_placement_spec,
    starting_position,
    cell_occupancy_matrix.track_counts(primary_axis).explicit,
  );
  let mut last_primary_span: Option<Line<OriginZeroLine>> = None;
  loop {
    check_layout_abort();
    let primary_axis_placement = resolve_indefinite_grid_lines(
      primary_axis_placement_spec,
      position,
      named_line_resolver,
      primary_axis,
      cell_occupancy_matrix.track_counts(primary_axis).explicit,
    );

    let does_fit = cell_occupancy_matrix.line_area_is_unoccupied(
      primary_axis,
      primary_axis_placement,
      secondary_axis_placement,
    );

    if does_fit {
      return (primary_axis_placement, secondary_axis_placement);
    } else {
      if last_primary_span == Some(primary_axis_placement) {
        // Clamped to the UA limit and unable to find a free cell: fall back to the clamped span.
        return (primary_axis_placement, secondary_axis_placement);
      }
      last_primary_span = Some(primary_axis_placement);
      position += 1;
    }
  }
}

/// 8.5. Grid Item Placement Algorithm
/// Step 4. Position the remaining grid items.
fn place_indefinitely_positioned_item<I: crate::CheapCloneStr>(
  cell_occupancy_matrix: &CellOccupancyMatrix,
  placement: &InBothAbsAxis<Line<OriginZeroGridPlacementWithNamedSpan<I>>>,
  auto_flow: GridAutoFlow,
  grid_position: (OriginZeroLine, OriginZeroLine),
  named_line_resolver: &NamedLineResolver<I>,
) -> (Line<OriginZeroLine>, Line<OriginZeroLine>) {
  let primary_axis = auto_flow.primary_axis();

  let primary_placement_style = axis_item(placement, primary_axis);
  let secondary_placement_style = axis_item(placement, primary_axis.other_axis());

  let has_definite_primary_axis_position = placement_is_definite(primary_placement_style);
  let primary_axis_grid_start_line = cell_occupancy_matrix
    .track_counts(primary_axis)
    .implicit_start_line();
  let primary_axis_grid_end_line = cell_occupancy_matrix
    .track_counts(primary_axis)
    .implicit_end_line();
  let secondary_axis_grid_start_line = cell_occupancy_matrix
    .track_counts(primary_axis.other_axis())
    .implicit_start_line();

  let line_area_is_occupied = |primary_span, secondary_span| {
    !cell_occupancy_matrix.line_area_is_unoccupied(primary_axis, primary_span, secondary_span)
  };

  let (mut primary_idx, mut secondary_idx) = (
    initial_candidate(
      primary_placement_style,
      grid_position.0,
      cell_occupancy_matrix.track_counts(primary_axis).explicit,
    ),
    initial_candidate(
      secondary_placement_style,
      grid_position.1,
      cell_occupancy_matrix
        .track_counts(primary_axis.other_axis())
        .explicit,
    ),
  );

  if has_definite_primary_axis_position {
    let primary_span = resolve_definite_grid_lines(
      primary_placement_style,
      cell_occupancy_matrix.track_counts(primary_axis).explicit,
    );

    // Compute secondary axis starting position for search
    secondary_idx = match auto_flow.is_dense() {
      // If auto-flow is dense then we always search from the first track
      true => initial_candidate(
        secondary_placement_style,
        secondary_axis_grid_start_line,
        cell_occupancy_matrix
          .track_counts(primary_axis.other_axis())
          .explicit,
      ),
      false => {
        if primary_span.start < primary_idx {
          secondary_idx + 1
        } else {
          secondary_idx
        }
      }
    };

    // Item has fixed primary axis position: so we simply increment the secondary axis position
    // until we find a space that the item fits in
    let mut last_secondary_span: Option<Line<OriginZeroLine>> = None;
    loop {
      check_layout_abort();
      let secondary_span = resolve_indefinite_grid_lines(
        secondary_placement_style,
        secondary_idx,
        named_line_resolver,
        primary_axis.other_axis(),
        cell_occupancy_matrix
          .track_counts(primary_axis.other_axis())
          .explicit,
      );

      // If area is occupied, increment the index and try again
      if line_area_is_occupied(primary_span, secondary_span) {
        if last_secondary_span == Some(secondary_span) {
          // Clamped to the UA limit and unable to find a free cell: fall back to the clamped span.
          return (primary_span, secondary_span);
        }
        last_secondary_span = Some(secondary_span);
        secondary_idx += 1;
        continue;
      }

      // Once we find a free space, return that position
      return (primary_span, secondary_span);
    }
  } else {
    // Item does not have any fixed axis, so we search along the primary axis until we hit the end of the already
    // existent tracks, and then we reset the primary axis back to zero and increment the secondary axis index.
    // We continue in this vein until we find a space that the item fits in.
    let implicit_track_limit = max_implicit_tracks_per_side() as usize;
    let max_primary_tracks = (cell_occupancy_matrix.track_counts(primary_axis).explicit as usize)
      .saturating_add(implicit_track_limit.saturating_mul(2));
    let max_secondary_tracks = (cell_occupancy_matrix
      .track_counts(primary_axis.other_axis())
      .explicit as usize)
      .saturating_add(implicit_track_limit.saturating_mul(2));
    // Upper bound on the number of distinct cell placements within the UA-limited grid.
    //
    // If we exceed this while still failing to find an unoccupied area, then clamping has prevented
    // further progress and we should stop searching to avoid an infinite loop.
    let max_attempts = max_primary_tracks
      .saturating_mul(max_secondary_tracks)
      .max(1);
    let mut attempts = 0usize;
    loop {
      check_layout_abort();
      let primary_span = resolve_indefinite_grid_lines(
        primary_placement_style,
        primary_idx,
        named_line_resolver,
        primary_axis,
        cell_occupancy_matrix.track_counts(primary_axis).explicit,
      );
      let secondary_span = resolve_indefinite_grid_lines(
        secondary_placement_style,
        secondary_idx,
        named_line_resolver,
        primary_axis.other_axis(),
        cell_occupancy_matrix
          .track_counts(primary_axis.other_axis())
          .explicit,
      );

      // If the primary index is out of bounds, then increment the secondary index and reset the primary
      // index back to the start of the grid
      let primary_out_of_bounds = primary_span.end > primary_axis_grid_end_line;
      if primary_out_of_bounds {
        secondary_idx += 1;
        primary_idx = initial_candidate(
          primary_placement_style,
          primary_axis_grid_start_line,
          cell_occupancy_matrix.track_counts(primary_axis).explicit,
        );
        continue;
      }

      // If area is occupied, increment the primary index and try again
      if line_area_is_occupied(primary_span, secondary_span) {
        attempts = attempts.saturating_add(1);
        if attempts >= max_attempts {
          // The placement algorithm is cycling due to clamping at the UA-defined implicit-grid
          // limit. Return the clamped placement (even if it overlaps) rather than looping forever.
          return (primary_span, secondary_span);
        }
        primary_idx += 1;
        continue;
      }

      // Once we find a free space that's in bounds, return that position
      return (primary_span, secondary_span);
    }
  }
}

/// Record the grid item in both CellOccupancyMatric and the GridItems list
/// once a definite placement has been determined
#[allow(clippy::too_many_arguments)]
fn record_grid_placement<S: GridItemStyle>(
  cell_occupancy_matrix: &mut CellOccupancyMatrix,
  items: &mut Vec<GridItem>,
  node: NodeId,
  index: usize,
  style: S,
  parent_align_items: AlignItems,
  parent_justify_items: AlignItems,
  primary_axis: AbsoluteAxis,
  primary_span: Line<OriginZeroLine>,
  secondary_span: Line<OriginZeroLine>,
  placement_type: CellOccupancyState,
) {
  #[cfg(all(test, feature = "debug"))]
  println!("BEFORE placement:");
  #[cfg(all(test, feature = "debug"))]
  println!("{cell_occupancy_matrix:?}");

  // Mark area of grid as occupied
  cell_occupancy_matrix.mark_area_as(primary_axis, primary_span, secondary_span, placement_type);

  // Create grid item
  let (col_span, row_span) = match primary_axis {
    AbsoluteAxis::Horizontal => (primary_span, secondary_span),
    AbsoluteAxis::Vertical => (secondary_span, primary_span),
  };
  items.push(GridItem::new_with_placement_style_and_order(
    node,
    col_span,
    row_span,
    style,
    parent_align_items,
    parent_justify_items,
    index as u16,
  ));

  #[cfg(all(test, feature = "debug"))]
  println!("AFTER placement:");
  #[cfg(all(test, feature = "debug"))]
  println!("{cell_occupancy_matrix:?}");
  #[cfg(all(test, feature = "debug"))]
  println!("\n");
}

#[cfg(test)]
mod tests {

  mod test_placement_algorithm {
    use crate::compute::grid::implicit_grid::compute_grid_size_estimate;
    use crate::compute::grid::types::TrackCounts;
    use crate::compute::grid::util::*;
    use crate::compute::grid::CellOccupancyMatrix;
    use crate::compute::grid::NamedLineResolver;
    use crate::prelude::*;
    use crate::style::GridAutoFlow;

    use super::super::place_grid_items;

    type ExpectedPlacement = (i16, i16, i16, i16);

    fn placement_test_runner(
      explicit_col_count: u16,
      explicit_row_count: u16,
      children: Vec<(usize, Style, ExpectedPlacement)>,
      expected_col_counts: TrackCounts,
      expected_row_counts: TrackCounts,
      flow: GridAutoFlow,
    ) {
      // Setup test
      let children_iter = || {
        children
          .iter()
          .map(|(index, style, _)| (*index, NodeId::from(*index), style))
      };
      let child_styles_iter = children
        .iter()
        .map(|(index, style, _)| (NodeId::from(*index), style));
      let estimated_sizes = compute_grid_size_estimate(
        explicit_col_count,
        explicit_row_count,
        child_styles_iter,
        |_| crate::geometry::InBothAbsAxis {
          horizontal: None,
          vertical: None,
        },
      );
      let mut items = Vec::new();
      let mut cell_occupancy_matrix =
        CellOccupancyMatrix::with_track_counts(estimated_sizes.0, estimated_sizes.1);
      let mut name_resolver = NamedLineResolver::new(&Style::DEFAULT, 0, 0);
      name_resolver.set_explicit_column_count(explicit_col_count);
      name_resolver.set_explicit_row_count(explicit_row_count);

      // Run placement algorithm
      place_grid_items(
        &mut cell_occupancy_matrix,
        &mut items,
        children_iter,
        flow,
        AlignSelf::Start,
        AlignSelf::Start,
        // TODO: actually test named line resolution
        &name_resolver,
        crate::geometry::InBothAbsAxis {
          horizontal: false,
          vertical: false,
        },
        |_| crate::geometry::InBothAbsAxis {
          horizontal: None,
          vertical: None,
        },
      );

      // Assert that each item has been placed in the right location
      assert_eq!(items.len(), children.len());
      for (idx, (id, _style, expected_placement)) in children.iter().enumerate() {
        let node_id = NodeId::from(*id);
        let item = items
          .iter()
          .find(|item| item.node == node_id)
          .unwrap_or_else(|| panic!("Missing placed item for node {node_id:?}"));
        let actual_placement = (
          item.column.start,
          item.column.end,
          item.row.start,
          item.row.end,
        );
        assert_eq!(
          actual_placement,
          (*expected_placement).into_oz(),
          "Item {idx} (0-indexed, node {node_id:?})"
        );
      }

      // Assert that the correct number of implicit rows have been generated
      let actual_row_counts =
        *cell_occupancy_matrix.track_counts(crate::compute::grid::AbsoluteAxis::Vertical);
      assert_eq!(actual_row_counts, expected_row_counts, "row track counts");
      let actual_col_counts =
        *cell_occupancy_matrix.track_counts(crate::compute::grid::AbsoluteAxis::Horizontal);
      assert_eq!(
        actual_col_counts, expected_col_counts,
        "column track counts"
      );
    }

    #[test]
    fn test_only_fixed_placement() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![
          // node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (line(1), auto(), line(1), auto()).into_grid_child(),
            (0, 1, 0, 1),
          ),
          (
            2,
            (line(-4), auto(), line(-3), auto()).into_grid_child(),
            (-1, 0, 0, 1),
          ),
          (
            3,
            (line(-3), auto(), line(-4), auto()).into_grid_child(),
            (0, 1, -1, 0),
          ),
          (
            4,
            (line(3), span(2), line(5), auto()).into_grid_child(),
            (2, 4, 4, 5),
          ),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 1,
        explicit: 2,
        positive_implicit: 2,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 1,
        explicit: 2,
        positive_implicit: 3,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_placement_spanning_origin() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![
          // node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (line(-1), line(-1), line(-1), line(-1)).into_grid_child(),
            (2, 3, 2, 3),
          ),
          (
            2,
            (line(-1), span(2), line(-1), span(2)).into_grid_child(),
            (2, 4, 2, 4),
          ),
          (
            3,
            (line(-4), line(-4), line(-4), line(-4)).into_grid_child(),
            (-1, 0, -1, 0),
          ),
          (
            4,
            (line(-4), span(2), line(-4), span(2)).into_grid_child(),
            (-1, 1, -1, 1),
          ),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 1,
        explicit: 2,
        positive_implicit: 2,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 1,
        explicit: 2,
        positive_implicit: 2,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_only_auto_placement_row_flow() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        let auto_child = (auto(), auto(), auto(), auto()).into_grid_child();
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (1, auto_child.clone(), (0, 1, 0, 1)),
          (2, auto_child.clone(), (1, 2, 0, 1)),
          (3, auto_child.clone(), (0, 1, 1, 2)),
          (4, auto_child.clone(), (1, 2, 1, 2)),
          (5, auto_child.clone(), (0, 1, 2, 3)),
          (6, auto_child.clone(), (1, 2, 2, 3)),
          (7, auto_child.clone(), (0, 1, 3, 4)),
          (8, auto_child.clone(), (1, 2, 3, 4)),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 2,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_only_auto_placement_column_flow() {
      let flow = GridAutoFlow::Column;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        let auto_child = (auto(), auto(), auto(), auto()).into_grid_child();
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (1, auto_child.clone(), (0, 1, 0, 1)),
          (2, auto_child.clone(), (0, 1, 1, 2)),
          (3, auto_child.clone(), (1, 2, 0, 1)),
          (4, auto_child.clone(), (1, 2, 1, 2)),
          (5, auto_child.clone(), (2, 3, 0, 1)),
          (6, auto_child.clone(), (2, 3, 1, 2)),
          (7, auto_child.clone(), (3, 4, 0, 1)),
          (8, auto_child.clone(), (3, 4, 1, 2)),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 2,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_oversized_item() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (span(5), auto(), auto(), auto()).into_grid_child(),
            (0, 5, 0, 1),
          ),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 3,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_span_then_named_span_conflict_handling() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![(
          1,
          (
            span(2),
            GridPlacement::NamedSpan("foo".to_string(), 2),
            auto(),
            auto(),
          )
            .into_grid_child(),
          (0, 2, 0, 1),
        )]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_named_span_then_span_conflict_handling() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![(
          1,
          (
            GridPlacement::NamedSpan("foo".to_string(), 2),
            span(2),
            auto(),
            auto(),
          )
            .into_grid_child(),
          (0, 1, 0, 1),
        )]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_named_span_then_named_span_conflict_handling() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![(
          1,
          (
            GridPlacement::NamedSpan("foo".to_string(), 2),
            GridPlacement::NamedSpan("foo".to_string(), 2),
            auto(),
            auto(),
          )
            .into_grid_child(),
          (0, 1, 0, 1),
        )]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_fixed_in_secondary_axis() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (span(2), auto(), line(1), auto()).into_grid_child(),
            (0, 2, 0, 1),
          ),
          (
            2,
            (auto(), auto(), line(2), auto()).into_grid_child(),
            (0, 1, 1, 2),
          ),
          (
            3,
            (auto(), auto(), line(1), auto()).into_grid_child(),
            (2, 3, 0, 1),
          ),
          (
            4,
            (auto(), auto(), line(4), auto()).into_grid_child(),
            (0, 1, 3, 4),
          ),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 1,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 2,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_definite_in_secondary_axis_with_fully_definite_negative() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (
            2,
            (auto(), auto(), line(2), auto()).into_grid_child(),
            (0, 1, 1, 2),
          ),
          (
            1,
            (line(-4), auto(), line(2), auto()).into_grid_child(),
            (-1, 0, 1, 2),
          ),
          (
            3,
            (auto(), auto(), line(1), auto()).into_grid_child(),
            (-1, 0, 0, 1),
          ),
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 1,
        explicit: 2,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_dense_packing_algorithm() {
      let flow = GridAutoFlow::RowDense;
      let explicit_col_count = 4;
      let explicit_row_count = 4;
      let children = {
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (line(2), auto(), line(1), auto()).into_grid_child(),
            (1, 2, 0, 1),
          ), // Definitely positioned in column 2
          (
            2,
            (span(2), auto(), auto(), auto()).into_grid_child(),
            (2, 4, 0, 1),
          ), // Spans 2 columns, so positioned after item 1
          (
            3,
            (auto(), auto(), auto(), auto()).into_grid_child(),
            (0, 1, 0, 1),
          ), // Spans 1 column, so should be positioned before item 1
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 4,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 4,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_sparse_packing_algorithm() {
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 4;
      let explicit_row_count = 4;
      let children = {
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (auto(), span(3), auto(), auto()).into_grid_child(),
            (0, 3, 0, 1),
          ), // Width 3
          (
            2,
            (auto(), span(3), auto(), auto()).into_grid_child(),
            (0, 3, 1, 2),
          ), // Width 3 (wraps to next row)
          (
            3,
            (auto(), span(1), auto(), auto()).into_grid_child(),
            (3, 4, 1, 2),
          ), // Width 1 (uses second row as we're already on it)
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 4,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 4,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_auto_placement_in_negative_tracks() {
      let flow = GridAutoFlow::RowDense;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = {
        vec![
          // output order, node, style (grid coords), expected_placement (oz coords)
          (
            1,
            (line(-5), auto(), line(1), auto()).into_grid_child(),
            (-2, -1, 0, 1),
          ), // Row 1. Definitely positioned in column -2
          (
            2,
            (auto(), auto(), line(2), auto()).into_grid_child(),
            (-2, -1, 1, 2),
          ), // Row 2. Auto positioned in column -2
          (
            3,
            (auto(), auto(), auto(), auto()).into_grid_child(),
            (-1, 0, 0, 1),
          ), // Row 1. Auto positioned in column -1
        ]
      };
      let expected_cols = TrackCounts {
        negative_implicit: 2,
        explicit: 2,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_overlarge_grid_clamps_extreme_line_numbers() {
      // Exercise CSS Grid 2 §"Limiting Large Grids" clamping behaviour.
      //
      // These placements would otherwise attempt to allocate an enormous implicit grid.
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = vec![(
        1,
        (line(i16::MAX), auto(), line(i16::MIN), auto()).into_grid_child(),
        // With the test UA limit (see `compute/grid/limits.rs`), this is clamped into the
        // last column track and first row track.
        (33, 34, -32, -31),
      )];
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 32,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 32,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_overlarge_grid_clamp_span_and_reposition_semantics() {
      // Spec example (scaled down to the unit test UA limit):
      //   - If the grid area spans outside the limited grid, clamp the overflowing edge.
      //   - If the grid area is completely outside, truncate to 1 and reposition into the last track.
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = vec![(
        1,
        (line(100), line(120), line(20), line(80)).into_grid_child(),
        // Column is completely outside the max line -> last track.
        // Row spans outside -> clamp end to max line.
        (33, 34, 19, 34),
      )];
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 32,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 32,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_overlarge_grid_clamps_named_line_resolution() {
      // Ensure clamping is applied after named line resolution, even if the resolved line index is
      // far outside the limited grid.
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = vec![(
        1,
        (
          GridPlacement::NamedLine("foo".to_string(), 5000),
          auto(),
          auto(),
          auto(),
        )
          .into_grid_child(),
        // Named line fallback resolves to a far-outside line; it should be clamped to the last column track.
        (33, 34, 0, 1),
      )];
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 32,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_overlarge_grid_named_line_index_does_not_wrap_on_overflow() {
      // Regression: very large named-line indices should not overflow i16 arithmetic and wrap
      // to the opposite side of the grid.
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 2;
      let explicit_row_count = 2;
      let children = vec![(
        1,
        (
          GridPlacement::NamedLine("foo".to_string(), i16::MAX),
          auto(),
          auto(),
          auto(),
        )
          .into_grid_child(),
        // Extremely large positive indices clamp to the last column track (positive side).
        (33, 34, 0, 1),
      )];
      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 32,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 2,
        positive_implicit: 0,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_overlarge_grid_does_not_spin_forever_when_limited_grid_is_full() {
      // When the UA-defined implicit grid clamp is reached and the remaining cells are occupied,
      // the placement algorithm must not loop forever trying to find new implicit tracks. Instead,
      // it should fall back to placing into the clamped area (even if that overlaps).
      //
      // This is important for hostile inputs that might otherwise hang when cancellation is not
      // installed (or when the cancellation stride is large).
      let flow = GridAutoFlow::Row;
      let explicit_col_count = 1;
      let explicit_row_count = 1;

      let auto_child = (auto(), auto(), auto(), auto()).into_grid_child();
      let mut children = Vec::new();
      // With the unit test UA limit (32 positive implicit tracks), the limited grid can fit
      // `explicit_row_count + limit` = 33 rows. Add one extra item to force overflow.
      for idx in 0..34i16 {
        let row = if idx < 33 { idx } else { 32 };
        children.push((idx as usize + 1, auto_child.clone(), (0, 1, row, row + 1)));
      }

      let expected_cols = TrackCounts {
        negative_implicit: 0,
        explicit: 1,
        positive_implicit: 0,
      };
      let expected_rows = TrackCounts {
        negative_implicit: 0,
        explicit: 1,
        positive_implicit: 32,
      };
      placement_test_runner(
        explicit_col_count,
        explicit_row_count,
        children,
        expected_cols,
        expected_rows,
        flow,
      );
    }

    #[test]
    fn test_overlarge_grid_indefinite_resolution_does_not_panic_at_i16_max() {
      // Regression: `normalize_resolved_span` used `start + 1` (i16) which can overflow when the
      // candidate line is already clamped to `i16::MAX`, causing debug builds to panic before the
      // overlarge-grid clamp can reposition the span.
      use crate::geometry::AbsoluteAxis;
      use crate::compute::grid::OriginZeroLine;
      use crate::style::OriginZeroGridPlacementWithNamedSpan as GP;

      let placement = Line {
        start: GP::Auto,
        end: GP::Auto,
      };

      // Use a huge explicit grid size so the limited grid's max line is clamped to i16::MAX.
      let explicit_tracks = i16::MAX as u16;
      let resolver = NamedLineResolver::<String>::from_line_names(
        Vec::<Vec<String>>::new(),
        Vec::<Vec<String>>::new(),
        explicit_tracks,
        explicit_tracks,
      );

      let resolved = super::super::resolve_indefinite_grid_lines(
        &placement,
        OriginZeroLine(i16::MAX),
        &resolver,
        AbsoluteAxis::Horizontal,
        explicit_tracks,
      );

      assert_eq!(resolved.start.0, i16::MAX - 1);
      assert_eq!(resolved.end.0, i16::MAX);
    }
  }
}
