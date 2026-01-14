//! Guardrails for handling extremely large implicit grids.
//!
//! CSS Grid Level 2 explicitly allows UAs to clamp implicit grid growth to avoid
//! unbounded memory usage (see §"Limiting Large Grids").
//! <https://drafts.csswg.org/css-grid-2/#overlarge-grids>
use crate::geometry::Line;
use core::cmp::{max, min};
use core::mem::swap;
use super::OriginZeroLine;

/// Default maximum number of implicit tracks allowed on each side of the explicit grid.
///
/// This is a UA-defined limit to prevent hostile inputs (e.g. `grid-column: 1 / 100000`)
/// from causing the placement occupancy matrix to attempt enormous allocations.
///
/// The limit applies per axis.
#[cfg(not(test))]
const DEFAULT_MAX_IMPLICIT_TRACKS_PER_SIDE: u16 = 1000;

/// Use a much smaller limit in tests so unit tests remain fast/deterministic while still
/// exercising the clamping behaviour.
#[cfg(test)]
const DEFAULT_MAX_IMPLICIT_TRACKS_PER_SIDE: u16 = 32;

#[cfg(all(feature = "std", not(test)))]
const ENV_MAX_IMPLICIT_TRACKS_PER_SIDE: &str = "TAFFY_MAX_IMPLICIT_GRID_TRACKS_PER_SIDE";

#[cfg(all(feature = "std", not(test)))]
use std::sync::Once;
#[cfg(all(feature = "std", not(test)))]
use std::sync::atomic::{AtomicUsize, Ordering};

// Cache the env-var value (if any) so we don't re-read/parse it on every clamping call.
#[cfg(all(feature = "std", not(test)))]
static MAX_IMPLICIT_TRACKS_PER_SIDE: AtomicUsize =
  AtomicUsize::new(DEFAULT_MAX_IMPLICIT_TRACKS_PER_SIDE as usize);
#[cfg(all(feature = "std", not(test)))]
static MAX_IMPLICIT_TRACKS_INIT: Once = Once::new();

/// Returns the UA-defined maximum number of implicit tracks that may be generated on each side of
/// the explicit grid.
///
/// In `std` builds (but not in tests), this can be overridden via the environment variable
/// `TAFFY_MAX_IMPLICIT_GRID_TRACKS_PER_SIDE`.
pub(super) fn max_implicit_tracks_per_side() -> u16 {
  #[cfg(all(feature = "std", not(test)))]
  {
    MAX_IMPLICIT_TRACKS_INIT.call_once(|| {
      let parsed = std::env::var(ENV_MAX_IMPLICIT_TRACKS_PER_SIDE)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|v| *v > 0)
        // Constrain the limit so that it can be represented in OriginZeroLine (i16).
        .map(|v| v.min(i16::MAX as u16))
        .unwrap_or(DEFAULT_MAX_IMPLICIT_TRACKS_PER_SIDE);

      MAX_IMPLICIT_TRACKS_PER_SIDE.store(parsed as usize, Ordering::Relaxed);
    });

    MAX_IMPLICIT_TRACKS_PER_SIDE.load(Ordering::Relaxed) as u16
  }

  #[cfg(any(not(feature = "std"), test))]
  {
    DEFAULT_MAX_IMPLICIT_TRACKS_PER_SIDE
  }
}

#[inline]
fn add_i32_clamped(line: OriginZeroLine, delta: i32) -> OriginZeroLine {
  OriginZeroLine((line.0 as i32 + delta).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
}

/// Clamp a resolved grid area to the UA-defined implicit grid limits.
///
/// This implements the spec's "clamp a grid area" rules:
/// <https://drafts.csswg.org/css-grid-2/#overlarge-grids>
///
/// The limit is expressed as the maximum number of implicit tracks that may be generated on each
/// side of the explicit grid. The limited grid therefore spans:
///   - from `-limit` to `explicit + limit` in OriginZero line coordinates.
pub(super) fn clamp_grid_area_to_implicit_grid_limit(
  span: Line<OriginZeroLine>,
  explicit_track_count: u16,
) -> Line<OriginZeroLine> {
  let limit = max_implicit_tracks_per_side().min(i16::MAX as u16) as i32;
  let mut min_line = OriginZeroLine(-(limit as i16));
  let max_line = OriginZeroLine(
    (explicit_track_count as i32 + limit).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
  );

  // Taffy's internal GridTrackVec indices are stored in `u16` (see `GridItem::{row_indexes,column_indexes}`),
  // which means we can only represent at most `i16::MAX` (32767) total tracks in a single axis because
  // `GridTrackVec` includes both tracks and gutters/lines (2 * tracks + 1).
  //
  // When the limited grid range spans both negative and positive origin-zero coordinates, the number of
  // tracks between `min_line` and `max_line` can exceed `i16::MAX` even though each individual line
  // coordinate is representable in `i16` (e.g. `min_line=-32`, `max_line=32767` => 32799 tracks).
  //
  // Clamp the minimum line so that `max_line - min_line <= i16::MAX`, ensuring the resulting grid can
  // be indexed without `u16` wrapping.
  let span_len = (max_line.0 as i32) - (min_line.0 as i32);
  if span_len > i16::MAX as i32 {
    let new_min = (max_line.0 as i32) - (i16::MAX as i32);
    min_line = OriginZeroLine(new_min as i16);
  }

  // Normalize span (ensure start <= end and span >= 1 track).
  let mut start = span.start;
  let mut end = span.end;
  if start.0 > end.0 {
    swap(&mut start, &mut end);
  }
  if start == end {
    end = add_i32_clamped(start, 1);
  }

  // If the grid area would be placed completely outside the limited grid,
  // truncate the span to 1 and reposition it into the last track on that side.
  if end.0 <= min_line.0 {
    let start = min_line;
    return Line {
      start,
      end: add_i32_clamped(start, 1),
    };
  }
  if start.0 >= max_line.0 {
    let end = max_line;
    return Line {
      start: add_i32_clamped(end, -1),
      end,
    };
  }

  // Otherwise, the grid area intersects the limited grid. Clamp any overflowing edge back into
  // range.
  let start = max(start, min_line);
  let end = min(end, max_line);
  Line { start, end }
}
