//! Taffy uses two coordinate systems to refer to grid lines (the gaps/gutters between rows/columns):
use super::super::types::TrackCounts;
use crate::geometry::Line;
use core::cmp::{max, Ordering};
use core::ops::{Add, AddAssign, Sub};

/// Represents a grid line position in "CSS Grid Line" coordinates
///
/// "CSS Grid Line" coordinates are those used in grid-row/grid-column in the CSS grid spec:
///   - The line at left hand (or top) edge of the explicit grid is line 1
///     (and counts up from there)
///   - The line at the right hand (or bottom) edge of the explicit grid is -1
///     (and counts down from there)
///   - 0 is not a valid index
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[repr(transparent)]
pub struct GridLine(i16);

impl From<i16> for GridLine {
  fn from(value: i16) -> Self {
    Self(value)
  }
}

impl GridLine {
  /// Returns the underlying i16
  pub fn as_i16(self) -> i16 {
    self.0
  }

  /// Convert into OriginZero coordinates using the specified explicit track count
  pub(crate) fn into_origin_zero_line(self, explicit_track_count: u16) -> OriginZeroLine {
    // `explicit_track_count` is a u16, so `+ 1` can overflow at `u16::MAX`. Use i32 arithmetic to
    // keep this conversion panic-free even for hostile/degenerate inputs.
    let explicit_line_count = (explicit_track_count as i32) + 1;
    let oz_line = match self.0.cmp(&0) {
      Ordering::Greater => (self.0 as i32) - 1,
      Ordering::Less => (self.0 as i32) + explicit_line_count,
      Ordering::Equal => panic!("Grid line of zero is invalid"),
    };
    OriginZeroLine(oz_line.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn into_origin_zero_line_does_not_overflow_on_u16_max_explicit_tracks() {
    // Regression: `explicit_track_count + 1` on u16 can overflow. Ensure large explicit grids don't
    // panic/wrap during conversion.
    let line = GridLine::from(-1);
    let oz = line.into_origin_zero_line(u16::MAX);
    // With an enormous explicit grid, `-1` resolves to a large positive origin-zero line that is
    // clamped into the representable i16 range.
    assert_eq!(oz, OriginZeroLine(i16::MAX));
  }
}

/// Represents a grid line position in "OriginZero" coordinates
///
/// "OriginZero" coordinates are a normalized form:
///   - The line at left hand (or top) edge of the explicit grid is line 0
///   - The next line to the right (or down) is 1, and so on
///   - The next line to the left (or up) is -1, and so on
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct OriginZeroLine(pub i16);

// Add and Sub with Self
impl Add<OriginZeroLine> for OriginZeroLine {
  type Output = Self;
  fn add(self, rhs: OriginZeroLine) -> Self::Output {
    OriginZeroLine(self.0 + rhs.0)
  }
}
impl Sub<OriginZeroLine> for OriginZeroLine {
  type Output = Self;
  fn sub(self, rhs: OriginZeroLine) -> Self::Output {
    OriginZeroLine(self.0 - rhs.0)
  }
}

// Add and Sub with u16
impl Add<u16> for OriginZeroLine {
  type Output = Self;
  fn add(self, rhs: u16) -> Self::Output {
    let sum = self.0 as i32 + rhs as i32;
    let clamped = sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    debug_assert!(
      clamped as i32 == sum,
      "OriginZeroLine overflow: {} + {}",
      self.0,
      rhs
    );
    OriginZeroLine(clamped)
  }
}
impl AddAssign<u16> for OriginZeroLine {
  fn add_assign(&mut self, rhs: u16) {
    *self = *self + rhs;
  }
}
impl Sub<u16> for OriginZeroLine {
  type Output = Self;
  fn sub(self, rhs: u16) -> Self::Output {
    let diff = self.0 as i32 - rhs as i32;
    let clamped = diff.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    debug_assert!(
      clamped as i32 == diff,
      "OriginZeroLine overflow: {} - {}",
      self.0,
      rhs
    );
    OriginZeroLine(clamped)
  }
}

impl OriginZeroLine {
  /// Converts a grid line in OriginZero coordinates into the index of that same grid line in the GridTrackVec.
  pub(crate) fn into_track_vec_index(self, track_counts: TrackCounts) -> usize {
    self
      .try_into_track_vec_index(track_counts)
      .unwrap_or_else(|| {
        if self.0 > 0 {
          panic!("OriginZero grid line cannot be more than the number of positive grid lines");
        } else {
          panic!("OriginZero grid line cannot be less than the number of negative grid lines");
        }
      })
  }

  /// Converts a grid line in OriginZero coordinates into the index of that same grid line in the GridTrackVec.
  ///
  /// This fallible version is used for the placement of absolutely positioned grid items:
  ///
  ///    If a grid-placement property refers to a non-existent line either by explicitly specifying such a line or by
  ///    spanning outside of the existing implicit grid, it is instead treated as specifying auto (instead of creating
  ///    new implicit grid lines).
  ///
  /// The infallible version above if used when placing regular in-flow grid items.
  pub(crate) fn try_into_track_vec_index(self, track_counts: TrackCounts) -> Option<usize> {
    // OriginZero grid line cannot be less than the number of negative grid lines
    let neg_implicit = (track_counts.negative_implicit as i32).clamp(0, i16::MAX as i32);
    if (self.0 as i32) < -neg_implicit {
      return None;
    };
    // OriginZero grid line cannot be more than the number of positive grid lines
    let pos_end_line = (track_counts.explicit as i32 + track_counts.positive_implicit as i32)
      .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    if self.0 > pos_end_line {
      return None;
    };

    let idx = (self.0 as i32) + neg_implicit;
    debug_assert!(idx >= 0, "OriginZeroLine track index must be non-negative");
    Some(2 * (idx as usize))
  }

  /// The minimum number of negative implicit track there must be if a grid item starts at this line.
  pub(crate) fn implied_negative_implicit_tracks(self) -> u16 {
    if self.0 < 0 {
      self.0.unsigned_abs()
    } else {
      0
    }
  }

  /// The minimum number of positive implicit track there must be if a grid item end at this line.
  pub(crate) fn implied_positive_implicit_tracks(self, explicit_track_count: u16) -> u16 {
    let explicit_track_count_i16 =
      (explicit_track_count as i32).clamp(0, i16::MAX as i32) as i16;
    if self.0 > explicit_track_count_i16 {
      // At this point both values are non-negative and `self.0 > explicit_track_count_i16`,
      // so subtraction is safe.
      (self.0 as u16).saturating_sub(explicit_track_count)
    } else {
      0
    }
  }
}

impl Line<OriginZeroLine> {
  /// The number of tracks between the start and end lines
  pub(crate) fn span(self) -> u16 {
    max(self.end.0 - self.start.0, 0) as u16
  }
}

/// A trait for the different coordinates used to define grid lines.
pub trait GridCoordinate: Copy {}
impl GridCoordinate for GridLine {}
impl GridCoordinate for OriginZeroLine {}
