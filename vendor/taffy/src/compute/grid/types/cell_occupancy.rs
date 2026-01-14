//! Contains CellOccupancyMatrix used to track occupied cells during grid placement
use super::TrackCounts;
use super::super::limits::max_implicit_tracks_per_side;
use crate::compute::grid::OriginZeroLine;
use crate::geometry::AbsoluteAxis;
use crate::geometry::Line;
use crate::util::check_layout_abort;
use crate::util::sys::Vec;
use core::cmp::max;
use core::fmt::Debug;
use core::ops::Range;
use grid::Grid;

/// Maximum number of cells that the dense `grid::Grid` occupancy matrix is allowed to allocate.
///
/// When the UA-limited grid would exceed this, we fall back to a sparse representation to avoid
/// quadratic memory usage for hostile inputs (e.g. extremely large explicit grids).
#[cfg(not(test))]
const MAX_DENSE_CELL_COUNT: u64 = 16_000_000;
#[cfg(test)]
const MAX_DENSE_CELL_COUNT: u64 = 1024;

/// The occupancy state of a single grid cell
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub(crate) enum CellOccupancyState {
  #[default]
  /// Indicates that a grid cell is unoccupied
  Unoccupied,
  /// Indicates that a grid cell is occupied by a definitely placed item
  DefinitelyPlaced,
  /// Indicates that a grid cell is occupied by an item that was placed by the auto placement algorithm
  AutoPlaced,
}

/// A dynamically sized matrix (2d grid) which tracks the occupancy of each grid cell during auto-placement
/// It also keeps tabs on how many tracks there are and which tracks are implicit and which are explicit.
pub(crate) struct CellOccupancyMatrix {
  /// The grid of occupancy states
  inner: CellOccupancyInner,
  /// The counts of implicit and explicit columns
  columns: TrackCounts,
  /// The counts of implicit and explicit rows
  rows: TrackCounts,
}

enum CellOccupancyInner {
  Dense(Grid<CellOccupancyState>),
  Sparse(SparseCellOccupancy),
}

struct SparseCellOccupancy {
  rects: Vec<SparseRect>,
}

#[derive(Clone, Copy)]
struct SparseRect {
  row_span: Line<OriginZeroLine>,
  col_span: Line<OriginZeroLine>,
  kind: CellOccupancyState,
}

#[inline]
fn spans_overlap(a: Line<OriginZeroLine>, b: Line<OriginZeroLine>) -> bool {
  a.start.0 < b.end.0 && a.end.0 > b.start.0
}

#[inline]
fn add_i32_clamped(line: OriginZeroLine, delta: i32) -> OriginZeroLine {
  OriginZeroLine((line.0 as i32 + delta).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
}

/// Debug impl that represents the matrix in a compact 2d text format
impl Debug for CellOccupancyMatrix {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    writeln!(
      f,
      "Rows: neg_implicit={} explicit={} pos_implicit={}",
      self.rows.negative_implicit, self.rows.explicit, self.rows.positive_implicit
    )?;
    writeln!(
      f,
      "Cols: neg_implicit={} explicit={} pos_implicit={}",
      self.columns.negative_implicit, self.columns.explicit, self.columns.positive_implicit
    )?;
    writeln!(f, "State:")?;

    match &self.inner {
      CellOccupancyInner::Dense(grid) => {
        for row_idx in 0..grid.rows() {
          for cell in grid.iter_row(row_idx) {
            let letter = match *cell {
              CellOccupancyState::Unoccupied => '_',
              CellOccupancyState::DefinitelyPlaced => 'D',
              CellOccupancyState::AutoPlaced => 'A',
            };
            write!(f, "{letter}")?;
          }
          writeln!(f)?;
        }
      }
      CellOccupancyInner::Sparse(sparse) => {
        writeln!(f, "<sparse: {} occupied rectangles>", sparse.rects.len())?;
      }
    }

    Ok(())
  }
}

impl CellOccupancyMatrix {
  /// Create a CellOccupancyMatrix given a set of provisional track counts. The grid can expand as needed to fit more tracks,
  /// the provisional track counts represent a best effort attempt to avoid the extra allocations this requires.
  pub fn with_track_counts(columns: TrackCounts, rows: TrackCounts) -> Self {
    // Using a dense occupancy matrix requires O(rows * cols) memory. For very large explicit grids
    // this can result in hostile inputs triggering enormous allocations even when only a few items
    // need placement.
    //
    // Use a dense matrix only when both the current estimate *and* the UA-limited upper bound on
    // possible expansion are reasonably sized.
    let current_cells = (rows.len() as u64).saturating_mul(columns.len() as u64);
    let implicit_limit = max_implicit_tracks_per_side() as u64;
    let max_rows = (rows.explicit as u64).saturating_add(implicit_limit.saturating_mul(2));
    let max_cols = (columns.explicit as u64).saturating_add(implicit_limit.saturating_mul(2));
    let max_cells = max_rows.saturating_mul(max_cols);

    let inner = if current_cells <= MAX_DENSE_CELL_COUNT && max_cells <= MAX_DENSE_CELL_COUNT {
      CellOccupancyInner::Dense(Grid::new(rows.len(), columns.len()))
    } else {
      CellOccupancyInner::Sparse(SparseCellOccupancy { rects: Vec::new() })
    };

    Self { inner, rows, columns }
  }

  /// Determines whether the specified area fits within the tracks currently represented by the matrix
  pub fn is_area_in_range(
    &self,
    primary_axis: AbsoluteAxis,
    primary_range: Range<i16>,
    secondary_range: Range<i16>,
  ) -> bool {
    let primary_len = self.track_counts(primary_axis).len().min(i32::MAX as usize) as i32;
    let secondary_len = self
      .track_counts(primary_axis.other_axis())
      .len()
      .min(i32::MAX as usize) as i32;

    if primary_range.start < 0 || (primary_range.end as i32) > primary_len {
      return false;
    }
    if secondary_range.start < 0 || (secondary_range.end as i32) > secondary_len {
      return false;
    }
    true
  }

  /// Expands the grid (potentially in all 4 directions) in order to ensure that the specified range fits within the allocated space
  fn expand_to_fit_range(&mut self, row_range: Range<i16>, col_range: Range<i16>) {
    // Calculate number of rows and columns missing to accommodate ranges (if any)
    let req_negative_rows =
      max(-(row_range.start as i32), 0).min(u16::MAX as i32) as u16;
    let req_positive_rows = max(
      (row_range.end as i32) - (self.rows.len().min(i32::MAX as usize) as i32),
      0,
    )
    .min(u16::MAX as i32) as u16;
    let req_negative_cols =
      max(-(col_range.start as i32), 0).min(u16::MAX as i32) as u16;
    let req_positive_cols = max(
      (col_range.end as i32) - (self.columns.len().min(i32::MAX as usize) as i32),
      0,
    )
    .min(u16::MAX as i32) as u16;

    // Update the backing storage only in the dense representation. The sparse representation is
    // not dimensioned and only needs the TrackCounts updates below.
    if let CellOccupancyInner::Dense(grid) = &mut self.inner {
      let old_row_count = grid.rows();
      let old_col_count = grid.cols();
      let new_row_count =
        old_row_count + (req_negative_rows as usize) + (req_positive_rows as usize);
      let new_col_count =
        old_col_count + (req_negative_cols as usize) + (req_positive_cols as usize);

      // If expansion would exceed the dense-cell limit, degrade to sparse mode. This avoids a
      // potential OOM and keeps layout abortable even for hostile inputs.
      let new_cells = (new_row_count as u64).saturating_mul(new_col_count as u64);
      if new_cells > MAX_DENSE_CELL_COUNT {
        // Convert only the occupied cells into sparse rectangles. This preserves correctness for
        // typical cases (sparse occupancy) while avoiding further quadratic growth.
        let mut sparse = SparseCellOccupancy { rects: Vec::new() };
        for row in 0..old_row_count {
          check_layout_abort();
          for col in 0..old_col_count {
            check_layout_abort();
            let kind = *grid.get(row, col).unwrap();
            if kind == CellOccupancyState::Unoccupied {
              continue;
            }
            let row_line = self.rows.track_to_prev_oz_line(row.min(u16::MAX as usize) as u16);
            let col_line = self
              .columns
              .track_to_prev_oz_line(col.min(u16::MAX as usize) as u16);
            sparse.rects.push(SparseRect {
              row_span: Line {
                start: row_line,
                end: add_i32_clamped(row_line, 1),
              },
              col_span: Line {
                start: col_line,
                end: add_i32_clamped(col_line, 1),
              },
              kind,
            });
          }
        }
        self.inner = CellOccupancyInner::Sparse(sparse);
      } else {
        let mut data = Vec::with_capacity(new_row_count.saturating_mul(new_col_count));

        // Push new negative rows
        for _ in 0..req_negative_rows as usize {
          check_layout_abort();
          for _ in 0..new_col_count {
            data.push(CellOccupancyState::Unoccupied);
          }
        }

        // Push existing rows
        for row in 0..old_row_count {
          check_layout_abort();
          // Push new negative columns
          for _ in 0..req_negative_cols {
            data.push(CellOccupancyState::Unoccupied);
          }
          // Push existing columns
          for col in 0..old_col_count {
            data.push(*grid.get(row, col).unwrap());
          }
          // Push new positive columns
          for _ in 0..req_positive_cols {
            data.push(CellOccupancyState::Unoccupied);
          }
        }

        // Push new positive rows
        for _ in 0..req_positive_rows as usize {
          check_layout_abort();
          for _ in 0..new_col_count {
            data.push(CellOccupancyState::Unoccupied);
          }
        }

        // Update self with new data
        *grid = Grid::from_vec(data, new_col_count);
      }
    }

    self.rows.negative_implicit = self.rows.negative_implicit.saturating_add(req_negative_rows);
    self.rows.positive_implicit = self.rows.positive_implicit.saturating_add(req_positive_rows);
    self.columns.negative_implicit = self
      .columns
      .negative_implicit
      .saturating_add(req_negative_cols);
    self.columns.positive_implicit = self
      .columns
      .positive_implicit
      .saturating_add(req_positive_cols);
  }

  /// Mark an area of the matrix as occupied, expanding the allocated space as necessary to accommodate the passed area.
  pub fn mark_area_as(
    &mut self,
    primary_axis: AbsoluteAxis,
    primary_span: Line<OriginZeroLine>,
    secondary_span: Line<OriginZeroLine>,
    value: CellOccupancyState,
  ) {
    let (row_span, column_span) = match primary_axis {
      AbsoluteAxis::Horizontal => (secondary_span, primary_span),
      AbsoluteAxis::Vertical => (primary_span, secondary_span),
    };

    let mut col_range = self.columns.oz_line_range_to_track_range(column_span);
    let mut row_range = self.rows.oz_line_range_to_track_range(row_span);

    // Check that if the resolved ranges fit within the allocated grid. And if they don't then expand the grid to fit
    // and then re-resolve the ranges once the grid has been expanded as the resolved indexes may have changed
    let is_in_range = self.is_area_in_range(
      AbsoluteAxis::Horizontal,
      col_range.clone(),
      row_range.clone(),
    );
    if !is_in_range {
      self.expand_to_fit_range(row_range.clone(), col_range.clone());
      col_range = self.columns.oz_line_range_to_track_range(column_span);
      row_range = self.rows.oz_line_range_to_track_range(row_span);
    }

    match &mut self.inner {
      CellOccupancyInner::Dense(grid) => {
        for x in row_range {
          check_layout_abort();
          for y in col_range.clone() {
            *grid.get_mut(x as usize, y as usize).unwrap() = value;
          }
        }
      }
      CellOccupancyInner::Sparse(sparse) => {
        if value != CellOccupancyState::Unoccupied {
          sparse.rects.push(SparseRect {
            row_span,
            col_span: column_span,
            kind: value,
          });
        }
      }
    }
  }

  /// Determines whether a grid area specified by the bounding grid lines in OriginZero coordinates
  /// is entirely unnocupied. Returns true if all grid cells within the grid area are unnocupied, else false.
  pub fn line_area_is_unoccupied(
    &self,
    primary_axis: AbsoluteAxis,
    primary_span: Line<OriginZeroLine>,
    secondary_span: Line<OriginZeroLine>,
  ) -> bool {
    let primary_range = self
      .track_counts(primary_axis)
      .oz_line_range_to_track_range(primary_span);
    let secondary_range = self
      .track_counts(primary_axis.other_axis())
      .oz_line_range_to_track_range(secondary_span);
    self.track_area_is_unoccupied(primary_axis, primary_range, secondary_range)
  }

  /// Determines whether a grid area specified by a range of indexes into this CellOccupancyMatrix
  /// is entirely unnocupied. Returns true if all grid cells within the grid area are unnocupied, else false.
  pub fn track_area_is_unoccupied(
    &self,
    primary_axis: AbsoluteAxis,
    primary_range: Range<i16>,
    secondary_range: Range<i16>,
  ) -> bool {
    let (row_range, col_range) = match primary_axis {
      AbsoluteAxis::Horizontal => (secondary_range, primary_range),
      AbsoluteAxis::Vertical => (primary_range, secondary_range),
    };

    match &self.inner {
      CellOccupancyInner::Dense(grid) => {
        // Search for occupied cells in the specified area. Out of bounds cells are considered unoccupied.
        for x in row_range {
          check_layout_abort();
          for y in col_range.clone() {
            match grid.get(x as usize, y as usize) {
              None | Some(CellOccupancyState::Unoccupied) => continue,
              _ => return false,
            }
          }
        }
      }
      CellOccupancyInner::Sparse(sparse) => {
        // Out-of-bounds cells are considered unoccupied. Clamp the track ranges into the current
        // matrix dimensions before converting them back to line spans for overlap testing.
        let row_len = self.rows.len().min(i16::MAX as usize) as i16;
        let col_len = self.columns.len().min(i16::MAX as usize) as i16;
        let row_start = row_range.start.max(0).min(row_len);
        let row_end = row_range.end.max(0).min(row_len);
        let col_start = col_range.start.max(0).min(col_len);
        let col_end = col_range.end.max(0).min(col_len);

        if row_start >= row_end || col_start >= col_end {
          return true;
        }

        let row_span = self
          .rows
          .track_range_to_oz_line_range(row_start..row_end);
        let col_span = self
          .columns
          .track_range_to_oz_line_range(col_start..col_end);

        for rect in sparse.rects.iter() {
          check_layout_abort();
          if rect.kind == CellOccupancyState::Unoccupied {
            continue;
          }
          if spans_overlap(rect.row_span, row_span) && spans_overlap(rect.col_span, col_span) {
            return false;
          }
        }
      }
    }

    true
  }

  /// Determines whether the specified row contains any items
  pub fn row_is_occupied(&self, row_index: usize) -> bool {
    match &self.inner {
      CellOccupancyInner::Dense(grid) => {
        if row_index >= grid.rows() {
          return false;
        }
        grid
          .iter_row(row_index)
          .any(|cell| !matches!(cell, CellOccupancyState::Unoccupied))
      }
      CellOccupancyInner::Sparse(sparse) => {
        if row_index >= self.rows.len() {
          return false;
        }
        let row_line = self
          .rows
          .track_to_prev_oz_line(row_index.min(u16::MAX as usize) as u16);
        let row_span = Line {
          start: row_line,
          end: add_i32_clamped(row_line, 1),
        };
        sparse
          .rects
          .iter()
          .any(|rect| rect.kind != CellOccupancyState::Unoccupied && spans_overlap(rect.row_span, row_span))
      }
    }
  }

  /// Determines whether the specified column contains any items
  pub fn column_is_occupied(&self, column_index: usize) -> bool {
    match &self.inner {
      CellOccupancyInner::Dense(grid) => {
        if column_index >= grid.cols() {
          return false;
        }
        grid
          .iter_col(column_index)
          .any(|cell| !matches!(cell, CellOccupancyState::Unoccupied))
      }
      CellOccupancyInner::Sparse(sparse) => {
        if column_index >= self.columns.len() {
          return false;
        }
        let col_line = self
          .columns
          .track_to_prev_oz_line(column_index.min(u16::MAX as usize) as u16);
        let col_span = Line {
          start: col_line,
          end: add_i32_clamped(col_line, 1),
        };
        sparse
          .rects
          .iter()
          .any(|rect| rect.kind != CellOccupancyState::Unoccupied && spans_overlap(rect.col_span, col_span))
      }
    }
  }

  /// Returns the track counts of this CellOccunpancyMatrix in the relevant axis
  pub fn track_counts(&self, track_type: AbsoluteAxis) -> &TrackCounts {
    match track_type {
      AbsoluteAxis::Horizontal => &self.columns,
      AbsoluteAxis::Vertical => &self.rows,
    }
  }

  /// Given an axis and a track index
  /// Search backwards from the end of the track and find the last grid cell matching the specified state (if any)
  /// Return the index of that cell or None.
  pub fn last_of_type(
    &self,
    track_type: AbsoluteAxis,
    start_at: OriginZeroLine,
    kind: CellOccupancyState,
  ) -> Option<OriginZeroLine> {
    let other_track_counts = self.track_counts(track_type.other_axis());
    let track_computed_index = other_track_counts.oz_line_to_next_track(start_at);

    match &self.inner {
      CellOccupancyInner::Dense(grid) => {
        let limit = match track_type {
          AbsoluteAxis::Horizontal => grid.rows(),
          AbsoluteAxis::Vertical => grid.cols(),
        };
        // Index out of bounds: no track to search
        if track_computed_index < 0 || (track_computed_index as usize) >= limit {
          return None;
        }

        let maybe_index = match track_type {
          AbsoluteAxis::Horizontal => grid
            .iter_row(track_computed_index as usize)
            .rposition(|item| *item == kind),
          AbsoluteAxis::Vertical => grid
            .iter_col(track_computed_index as usize)
            .rposition(|item| *item == kind),
        };

        let primary_track_counts = self.track_counts(track_type);
        maybe_index.map(|idx| {
          let idx_u16 = idx.min(u16::MAX as usize) as u16;
          primary_track_counts.track_to_prev_oz_line(idx_u16)
        })
      }
      CellOccupancyInner::Sparse(sparse) => {
        let limit = other_track_counts.len();
        // Index out of bounds: no track to search
        if track_computed_index < 0 || (track_computed_index as usize) >= limit {
          return None;
        }

        let secondary_span = Line {
          start: start_at,
          end: add_i32_clamped(start_at, 1),
        };

        let mut best: Option<OriginZeroLine> = None;
        for rect in sparse.rects.iter() {
          if rect.kind != kind {
            continue;
          }
          let (primary_span, rect_secondary_span) = match track_type {
            AbsoluteAxis::Horizontal => (rect.col_span, rect.row_span),
            AbsoluteAxis::Vertical => (rect.row_span, rect.col_span),
          };
          if !spans_overlap(rect_secondary_span, secondary_span) {
            continue;
          }

          // The last occupied cell in the primary axis corresponds to the last track start line,
          // i.e. `end - 1` in origin-zero line coordinates.
          let last_start = (primary_span.end.0 as i32 - 1)
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
          let line = OriginZeroLine(last_start);
          best = Some(match best {
            None => line,
            Some(prev) => max(prev, line),
          });
        }
        best
      }
    }
  }

  #[cfg(test)]
  fn is_sparse(&self) -> bool {
    matches!(self.inner, CellOccupancyInner::Sparse(_))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn is_area_in_range_does_not_wrap_when_track_counts_exceed_i16_max() {
    // Regression: casting `len()` to i16 can wrap negative for large explicit grids, causing the
    // occupancy matrix to incorrectly think small ranges are out-of-bounds (and expand massively).
    let columns = TrackCounts::from_raw(0, 40_000, 0);
    let rows = TrackCounts::from_raw(0, 1, 0);
    let mut matrix = CellOccupancyMatrix::with_track_counts(columns, rows);

    assert!(matrix.is_area_in_range(AbsoluteAxis::Horizontal, 0..1, 0..1));

    matrix.mark_area_as(
      AbsoluteAxis::Horizontal,
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      CellOccupancyState::AutoPlaced,
    );

    assert_eq!(matrix.columns.positive_implicit, 0);
    assert_eq!(matrix.columns.negative_implicit, 0);
  }

  #[test]
  fn last_of_type_converts_result_using_primary_axis_track_counts() {
    // Regression: last_of_type previously converted the found cell index using the *other* axis'
    // TrackCounts, which produces wrong OriginZero lines when the two axes have different negative
    // implicit offsets.
    let columns = TrackCounts::from_raw(0, 4, 0);
    let rows = TrackCounts::from_raw(1, 2, 0);
    let mut matrix = CellOccupancyMatrix::with_track_counts(columns, rows);

    // Mark the cell at (row line 0, col line 2) as occupied.
    matrix.mark_area_as(
      AbsoluteAxis::Horizontal,
      Line {
        start: OriginZeroLine(2),
        end: OriginZeroLine(3),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      CellOccupancyState::AutoPlaced,
    );

    let out = matrix.last_of_type(
      AbsoluteAxis::Horizontal,
      OriginZeroLine(0),
      CellOccupancyState::AutoPlaced,
    );
    assert_eq!(out, Some(OriginZeroLine(2)));
  }

  #[test]
  fn last_of_type_uses_correct_matrix_dimension_for_bounds_check() {
    // Regression: last_of_type previously compared a column index against the matrix row count,
    // incorrectly returning None for non-square matrices.
    let columns = TrackCounts::from_raw(0, 5, 0);
    let rows = TrackCounts::from_raw(0, 2, 0);
    let mut matrix = CellOccupancyMatrix::with_track_counts(columns, rows);

    matrix.mark_area_as(
      AbsoluteAxis::Horizontal,
      Line {
        start: OriginZeroLine(4),
        end: OriginZeroLine(5),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      CellOccupancyState::AutoPlaced,
    );

    let out = matrix.last_of_type(
      AbsoluteAxis::Vertical,
      OriginZeroLine(4),
      CellOccupancyState::AutoPlaced,
    );
    assert_eq!(out, Some(OriginZeroLine(0)));
  }

  #[test]
  fn uses_sparse_representation_for_large_grids() {
    // The dense occupancy matrix uses O(rows * cols) memory. Ensure that sufficiently large grids
    // switch to the sparse representation to avoid hostile allocations.
    let columns = TrackCounts::from_raw(0, 50, 0);
    let rows = TrackCounts::from_raw(0, 50, 0);
    let mut matrix = CellOccupancyMatrix::with_track_counts(columns, rows);
    assert!(matrix.is_sparse());

    // Mark a single cell and ensure queries work correctly.
    matrix.mark_area_as(
      AbsoluteAxis::Horizontal,
      Line {
        start: OriginZeroLine(5),
        end: OriginZeroLine(6),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      CellOccupancyState::AutoPlaced,
    );

    assert!(matrix.column_is_occupied(5));
    assert!(matrix.row_is_occupied(0));
    assert_eq!(
      matrix.last_of_type(AbsoluteAxis::Horizontal, OriginZeroLine(0), CellOccupancyState::AutoPlaced),
      Some(OriginZeroLine(5))
    );
    assert!(!matrix.line_area_is_unoccupied(
      AbsoluteAxis::Horizontal,
      Line {
        start: OriginZeroLine(5),
        end: OriginZeroLine(6),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
    ));
  }
}
