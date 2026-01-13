//! Signed media timestamps.
//!
//! Containers can legally use negative timestamps (e.g. decode offsets). Rust's [`Duration`] is
//! unsigned, so the media pipeline uses [`MediaTimestamp`] for values that can be negative.

use std::time::Duration;

/// Signed media timestamp represented as nanoseconds from an arbitrary origin.
///
/// This is intentionally a tiny, Copy-friendly wrapper around `i64` so it can be used throughout
/// demux/decode layers without losing sign information.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MediaTimestamp {
  nanos: i64,
}

impl MediaTimestamp {
  /// Zero timestamp.
  pub const ZERO: Self = Self { nanos: 0 };

  /// Create a timestamp from a raw nanosecond count.
  #[inline]
  pub const fn from_nanos(nanos: i64) -> Self {
    Self { nanos }
  }

  /// Return the raw nanosecond count.
  #[inline]
  pub const fn as_nanos(self) -> i64 {
    self.nanos
  }

  /// Checked addition with a positive [`Duration`].
  #[inline]
  pub fn checked_add(self, d: Duration) -> Option<Self> {
    let delta = duration_to_nanos_i64(d)?;
    self.nanos.checked_add(delta).map(Self::from_nanos)
  }

  /// Checked subtraction with a positive [`Duration`].
  #[inline]
  pub fn checked_sub(self, d: Duration) -> Option<Self> {
    let delta = duration_to_nanos_i64(d)?;
    self.nanos.checked_sub(delta).map(Self::from_nanos)
  }

  /// Saturating addition with a positive [`Duration`].
  #[inline]
  pub fn saturating_add(self, d: Duration) -> Self {
    match duration_to_nanos_i64(d) {
      Some(delta) => Self::from_nanos(self.nanos.saturating_add(delta)),
      None => Self::from_nanos(i64::MAX),
    }
  }

  /// Saturating subtraction with a positive [`Duration`].
  #[inline]
  pub fn saturating_sub(self, d: Duration) -> Self {
    match duration_to_nanos_i64(d) {
      Some(delta) => Self::from_nanos(self.nanos.saturating_sub(delta)),
      None => Self::from_nanos(i64::MIN),
    }
  }
}

fn duration_to_nanos_i64(d: Duration) -> Option<i64> {
  i64::try_from(d.as_nanos()).ok()
}

