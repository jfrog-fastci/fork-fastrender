//! This module is not required for spec compliance, but is used as a performance optimisation
//! to reduce the number of allocations required when creating a grid.
use crate::geometry::InBothAbsAxis;
use crate::geometry::Line;
use crate::style::{GenericGridPlacement, GridPlacement};
use crate::tree::NodeId;
use crate::{CheapCloneStr, GridItemStyle};
use core::cmp::{max, min};

use super::types::TrackCounts;
use super::limits::{clamp_grid_area_to_implicit_grid_limit, max_implicit_tracks_per_side};
use super::OriginZeroLine;
use crate::util::check_layout_abort;

/// Estimate the number of rows and columns in the grid
/// This is used as a performance optimisation to pre-size vectors and reduce allocations. It also forms a necessary step
/// in the auto-placement
///   - The estimates for the explicit and negative implicit track counts are exact.
///   - However, the estimates for the positive explicit track count is a lower bound as auto-placement can affect this
///     in ways which are impossible to predict until the auto-placement algorithm is run.
///
/// Note that this function internally mixes use of grid track numbers and grid line numbers
pub(crate) fn compute_grid_size_estimate<'a, S: GridItemStyle + 'a>(
  explicit_col_count: u16,
  explicit_row_count: u16,
  child_styles_iter: impl Iterator<Item = (NodeId, S)>,
  get_child_subgrid_auto_span: impl Fn(NodeId) -> InBothAbsAxis<Option<u16>>,
) -> (TrackCounts, TrackCounts) {
  let implicit_track_limit = max_implicit_tracks_per_side();
  // Iterate over children, producing an estimate of the min and max grid lines (in origin-zero coordinates where)
  // along with the span of each item
  let (col_min, col_max, col_max_span, row_min, row_max, row_max_span) = get_known_child_positions(
    child_styles_iter,
    explicit_col_count,
    explicit_row_count,
    get_child_subgrid_auto_span,
  );

  // Compute *track* count estimates for each axis from:
  //   - The explicit track counts
  //   - The origin-zero coordinate min and max grid line variables
  let negative_implicit_inline_tracks = col_min
    .implied_negative_implicit_tracks()
    .min(implicit_track_limit);
  let explicit_inline_tracks = explicit_col_count;
  let mut positive_implicit_inline_tracks =
    col_max.implied_positive_implicit_tracks(explicit_col_count).min(implicit_track_limit);
  let negative_implicit_block_tracks = row_min
    .implied_negative_implicit_tracks()
    .min(implicit_track_limit);
  let explicit_block_tracks = explicit_row_count;
  let mut positive_implicit_block_tracks =
    row_max.implied_positive_implicit_tracks(explicit_row_count).min(implicit_track_limit);

  // In each axis, adjust positive track estimate if any items have a span that does not fit within
  // the total number of tracks in the estimate
  let tot_inline_tracks = (negative_implicit_inline_tracks as u32)
    + (explicit_inline_tracks as u32)
    + (positive_implicit_inline_tracks as u32);
  if tot_inline_tracks < col_max_span as u32 {
    positive_implicit_inline_tracks =
      (col_max_span as u32 - (explicit_inline_tracks as u32) - (negative_implicit_inline_tracks as u32))
        as u16;
  }
  positive_implicit_inline_tracks = positive_implicit_inline_tracks.min(implicit_track_limit);

  let tot_block_tracks = (negative_implicit_block_tracks as u32)
    + (explicit_block_tracks as u32)
    + (positive_implicit_block_tracks as u32);
  if tot_block_tracks < row_max_span as u32 {
    positive_implicit_block_tracks =
      (row_max_span as u32 - (explicit_block_tracks as u32) - (negative_implicit_block_tracks as u32))
        as u16;
  }
  positive_implicit_block_tracks = positive_implicit_block_tracks.min(implicit_track_limit);

  let column_counts = TrackCounts::from_raw(
    negative_implicit_inline_tracks,
    explicit_inline_tracks,
    positive_implicit_inline_tracks,
  );

  let row_counts = TrackCounts::from_raw(
    negative_implicit_block_tracks,
    explicit_block_tracks,
    positive_implicit_block_tracks,
  );

  (column_counts, row_counts)
}

/// Iterate over children, producing an estimate of the min and max grid *lines* along with the span of each item
///
/// Min and max grid lines are returned in origin-zero coordinates)
/// The span is measured in tracks spanned
fn get_known_child_positions<'a, S: GridItemStyle + 'a>(
  children_iter: impl Iterator<Item = (NodeId, S)>,
  explicit_col_count: u16,
  explicit_row_count: u16,
  get_child_subgrid_auto_span: impl Fn(NodeId) -> InBothAbsAxis<Option<u16>>,
) -> (
  OriginZeroLine,
  OriginZeroLine,
  u16,
  OriginZeroLine,
  OriginZeroLine,
  u16,
) {
  let (mut col_min, mut col_max, mut col_max_span) = (OriginZeroLine(0), OriginZeroLine(0), 0);
  let (mut row_min, mut row_max, mut row_max_span) = (OriginZeroLine(0), OriginZeroLine(0), 0);
  children_iter.for_each(|(node, child_style)| {
    check_layout_abort();
    // Note: that the children reference the lines in between (and around) the tracks not tracks themselves,
    // and thus we must subtract 1 to get an accurate estimate of the number of tracks
    let subgrid_auto_span = get_child_subgrid_auto_span(node);
    let mut column = child_style.grid_column();
    if let Some(span) = subgrid_auto_span.horizontal {
      if matches!(column.start, GridPlacement::Auto) && matches!(column.end, GridPlacement::Auto) {
        column.end = GridPlacement::Span(span);
      }
    }
    let mut row = child_style.grid_row();
    if let Some(span) = subgrid_auto_span.vertical {
      if matches!(row.start, GridPlacement::Auto) && matches!(row.end, GridPlacement::Auto) {
        row.end = GridPlacement::Span(span);
      }
    }

    let (child_col_min, child_col_max, child_col_span) =
      child_min_line_max_line_span::<S::CustomIdent>(column, explicit_col_count);
    let (child_row_min, child_row_max, child_row_span) =
      child_min_line_max_line_span::<S::CustomIdent>(row, explicit_row_count);
    col_min = min(col_min, child_col_min);
    col_max = max(col_max, child_col_max);
    col_max_span = max(col_max_span, child_col_span);
    row_min = min(row_min, child_row_min);
    row_max = max(row_max, child_row_max);
    row_max_span = max(row_max_span, child_row_span);
  });

  (
    col_min,
    col_max,
    col_max_span,
    row_min,
    row_max,
    row_max_span,
  )
}

/// Helper function for `compute_grid_size_estimate`
/// Produces a conservative estimate of the greatest and smallest grid lines used by a single grid item
///
/// Values are returned in origin-zero coordinates
#[inline]
fn child_min_line_max_line_span<S: CheapCloneStr>(
  line: Line<GridPlacement<S>>,
  explicit_track_count: u16,
) -> (OriginZeroLine, OriginZeroLine, u16) {
  use GenericGridPlacement as GP;

  // 8.3.1. Grid Placement Conflict Handling
  // A. If the placement for a grid item contains two lines, and the start line is further end-ward than the end line, swap the two lines.
  // B. If the start line is equal to the end line, remove the end line.
  // C. If the placement contains two spans, remove the one contributed by the end grid-placement property.
  // D. If the placement contains only a span for a named line, replace it with a span of 1.

  // Convert line into origin-zero coordinates before attempting to analyze
  // We ignore named lines here as they are accounted for separately
  let oz_line = line.into_origin_zero_ignoring_named(explicit_track_count);

  #[inline]
  fn add_i32_clamped(line: OriginZeroLine, delta: i32) -> OriginZeroLine {
    OriginZeroLine((line.0 as i32 + delta).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
  }

  // For definite placements, compute a concrete span and clamp it using the same UA-defined
  // overlarge-grid rules as the placement algorithm.
  let (min_line, max_line) = match (oz_line.start, oz_line.end) {
    // Both tracks specified
    (GP::Line(track1), GP::Line(track2)) => {
      let span = if track1 == track2 {
        Line {
          start: track1,
          end: add_i32_clamped(track1, 1),
        }
      } else {
        Line {
          start: min(track1, track2),
          end: max(track1, track2),
        }
      };
      let span = clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count);
      (span.start, span.end)
    }

    // Start track specified
    (GP::Line(track), GP::Auto) => {
      let span = Line {
        start: track,
        end: add_i32_clamped(track, 1),
      };
      let span = clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count);
      (span.start, span.end)
    }
    (GP::Line(track), GP::Span(span_len)) => {
      let span = Line {
        start: track,
        end: add_i32_clamped(track, span_len as i32),
      };
      let span = clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count);
      (span.start, span.end)
    }

    // End track specified
    (GP::Auto, GP::Line(track)) => {
      let span = Line {
        start: add_i32_clamped(track, -1),
        end: track,
      };
      let span = clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count);
      (span.start, span.end)
    }
    (GP::Span(span_len), GP::Line(track)) => {
      let span = Line {
        start: add_i32_clamped(track, -(span_len as i32)),
        end: track,
      };
      let span = clamp_grid_area_to_implicit_grid_limit(span, explicit_track_count);
      (span.start, span.end)
    }

    // Only spans or autos
    // We ignore placement positions here by returning 0 which never affects the estimate.
    (GP::Auto | GP::Span(_), GP::Auto | GP::Span(_)) => (OriginZeroLine(0), OriginZeroLine(0)),
  };

  // Calculate span only for indefinitely placed items as we don't need it for other items (whose
  // required space will be taken into account by min/max line positions).
  let span = match (oz_line.start, oz_line.end) {
    (GP::Auto | GP::Span(_), GP::Auto | GP::Span(_)) => {
      let span = oz_line.indefinite_span();
      let implicit_limit = max_implicit_tracks_per_side();
      // OriginZero line coordinates are i16 and GridTrackVec indices are stored in u16, so the total
      // number of tracks in an axis must not exceed i16::MAX (32767) to remain representable.
      let max_span = explicit_track_count
        .saturating_add(implicit_limit.saturating_mul(2))
        .min(i16::MAX as u16);
      span.min(max_span)
    }
    _ => 1,
  };

  (min_line, max_line, span)
}

#[allow(clippy::bool_assert_comparison)]
#[cfg(test)]
mod tests {
  mod test_child_min_max_line {
    type S = String;
    use super::super::child_min_line_max_line_span;
    use super::super::OriginZeroLine;
    use crate::geometry::Line;
    use crate::style_helpers::*;

    #[test]
    fn child_min_max_line_auto() {
      let (min_col, max_col, span) = child_min_line_max_line_span::<S>(
        Line {
          start: line(5),
          end: span(6),
        },
        6,
      );
      assert_eq!(min_col, OriginZeroLine(4));
      assert_eq!(max_col, OriginZeroLine(10));
      assert_eq!(span, 1);
    }

    #[test]
    fn child_min_max_line_negative_track() {
      let (min_col, max_col, span) = child_min_line_max_line_span::<S>(
        Line {
          start: line(-5),
          end: span(3),
        },
        6,
      );
      assert_eq!(min_col, OriginZeroLine(2));
      assert_eq!(max_col, OriginZeroLine(5));
      assert_eq!(span, 1);
    }
  }

  mod test_initial_grid_sizing {
    use super::super::compute_grid_size_estimate;
    use crate::compute::grid::util::test_helpers::*;
    use crate::geometry::InBothAbsAxis;
    use crate::style_helpers::*;
    use crate::tree::NodeId;

    #[test]
    fn explicit_grid_sizing_with_children() {
      let explicit_col_count = 6;
      let explicit_row_count = 8;
      let child_styles = vec![
        (line(1), span(2), line(2), auto()).into_grid_child(),
        (line(-4), auto(), line(-2), auto()).into_grid_child(),
      ];
      let child_styles = child_styles
        .iter()
        .enumerate()
        .map(|(idx, style)| (NodeId::from(idx), style));
      let (inline, block) =
        compute_grid_size_estimate(explicit_col_count, explicit_row_count, child_styles, |_| {
          InBothAbsAxis {
            horizontal: None,
            vertical: None,
          }
        });
      assert_eq!(inline.negative_implicit, 0);
      assert_eq!(inline.explicit, explicit_col_count);
      assert_eq!(inline.positive_implicit, 0);
      assert_eq!(block.negative_implicit, 0);
      assert_eq!(block.explicit, explicit_row_count);
      assert_eq!(block.positive_implicit, 0);
    }

    #[test]
    fn negative_implicit_grid_sizing() {
      let explicit_col_count = 4;
      let explicit_row_count = 4;
      let child_styles = vec![
        (line(-6), span(2), line(-8), auto()).into_grid_child(),
        (line(4), auto(), line(3), auto()).into_grid_child(),
      ];
      let child_styles = child_styles
        .iter()
        .enumerate()
        .map(|(idx, style)| (NodeId::from(idx), style));
      let (inline, block) =
        compute_grid_size_estimate(explicit_col_count, explicit_row_count, child_styles, |_| {
          InBothAbsAxis {
            horizontal: None,
            vertical: None,
          }
        });
      assert_eq!(inline.negative_implicit, 1);
      assert_eq!(inline.explicit, explicit_col_count);
      assert_eq!(inline.positive_implicit, 0);
      assert_eq!(block.negative_implicit, 3);
      assert_eq!(block.explicit, explicit_row_count);
      assert_eq!(block.positive_implicit, 0);
    }

    #[test]
    fn overlarge_grid_clamps_total_tracks_to_i16_max() {
      // Regression: the overlarge-grid clamp must ensure the total number of tracks in an axis fits
      // within Taffy's `u16` GridTrackVec index space (2 * tracks + 1).
      //
      // In tests, the implicit-track limit is 32. If the explicit grid is close enough to `i16::MAX`,
      // the naive limited grid range would span both negative and positive coordinates and exceed
      // `i16::MAX` tracks (e.g. min=-32, max=32767 => 32799 tracks), which would later wrap `u16`
      // item indexes.
      let explicit_col_count: u16 = i16::MAX as u16 - 32;
      let explicit_row_count: u16 = 1;
      let child_styles = vec![
        // Forces a negative origin-zero line (maps to -32 for this explicit track count).
        (line(i16::MIN), auto(), auto(), auto()).into_grid_child(),
        // Forces the max line to clamp to i16::MAX (span extends past representable end line).
        (line(i16::MAX), span(2), auto(), auto()).into_grid_child(),
      ];
      let child_styles = child_styles
        .iter()
        .enumerate()
        .map(|(idx, style)| (NodeId::from(idx), style));
      let (inline, block) = compute_grid_size_estimate(
        explicit_col_count,
        explicit_row_count,
        child_styles,
        |_| InBothAbsAxis {
          horizontal: None,
          vertical: None,
        },
      );

      assert_eq!(inline.explicit, explicit_col_count);
      assert_eq!(inline.negative_implicit, 0);
      assert_eq!(inline.positive_implicit, 32);
      assert_eq!(inline.len(), i16::MAX as usize);

      assert_eq!(block.negative_implicit, 0);
      assert_eq!(block.explicit, explicit_row_count);
      assert_eq!(block.positive_implicit, 0);
    }
  }
}
