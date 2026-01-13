//! Media timestamp helpers.
//!
//! This module is the canonical “timebase math” for the media pipeline: converting container
//! timestamps (typically i64 ticks in an arbitrary timebase) into `Duration` and back.
//!
//! Containers can legally use negative PTS/DTS values (e.g. decode offsets). Because Rust's
//! [`Duration`] is unsigned, the [`ticks_to_duration`] convenience helper necessarily clamps
//! negative values to `Duration::ZERO`. Callers that need to preserve negative timestamps should use
//! the signed [`MediaTimestamp`] APIs instead.
//!
//! See `docs/media_clocking.md` for the overall A/V clocking model (audio master clock, tick as
//! wake-up only).

use super::timestamp::MediaTimestamp;
use std::time::Duration;

/// A rational timebase describing the duration of a single "tick".
///
/// This matches the common container convention (e.g. FFmpeg's `AVRational`):
/// `num / den` seconds per tick.
///
/// Examples:
/// - 90kHz PTS: `Timebase { num: 1, den: 90_000 }`
/// - Millisecond ticks: `Timebase { num: 1, den: 1_000 }`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
  /// Numerator (seconds per tick).
  pub num: u32,
  /// Denominator (seconds per tick).
  pub den: u32,
}

impl Timebase {
  #[inline]
  pub const fn new(num: u32, den: u32) -> Self {
    Self { num, den }
  }

  /// Convenience for common `1/hz` timebases.
  #[inline]
  pub const fn hz(ticks_per_second: u32) -> Self {
    Self { num: 1, den: ticks_per_second }
  }
}

/// Convert container ticks (which may be negative) into a [`Duration`].
///
/// `Duration` cannot represent negative values; negative `ticks` saturate to
/// `Duration::ZERO`.
///
/// For very large values this saturates to `Duration::MAX` instead of panicking
/// or overflowing.
pub fn ticks_to_duration(ticks: i64, timebase: Timebase) -> Duration {
  if ticks <= 0 {
    return Duration::ZERO;
  }
  if timebase.num == 0 {
    return Duration::ZERO;
  }
  if timebase.den == 0 {
    // Invalid/infinite timebase: any non-zero tick count corresponds to an
    // unrepresentably large duration.
    return Duration::MAX;
  }

  // Compute ticks * num / den seconds, tracking the remainder for sub-second
  // precision. Use i128 to avoid overflow (no panics).
  let total = (ticks as i128) * (timebase.num as i128);
  let den = timebase.den as i128;

  let secs = total / den;
  let rem = total % den;

  // Round the fractional part to the nearest nanosecond to minimize error.
  // `rem` is in units of `1/den` seconds.
  let nanos_i128 = ((rem * 1_000_000_000i128) + (den / 2)) / den;

  // Rounding can carry into the next second.
  let (secs, nanos_i128) = if nanos_i128 >= 1_000_000_000i128 {
    (secs.saturating_add(1), nanos_i128 - 1_000_000_000i128)
  } else {
    (secs, nanos_i128)
  };

  if secs > u64::MAX as i128 {
    return Duration::MAX;
  }

  // Safe: nanos_i128 is in [0, 1e9).
  Duration::new(secs as u64, nanos_i128 as u32)
}

/// Convert a [`Duration`] into ticks for the provided timebase.
///
/// This is a best-effort conversion: it rounds to the nearest tick and saturates
/// to `i64::MAX` on overflow.
pub fn duration_to_ticks(d: Duration, timebase: Timebase) -> i64 {
  if d.is_zero() {
    return 0;
  }
  if timebase.den == 0 {
    // Infinite seconds-per-tick -> ~0 ticks per second.
    return 0;
  }
  if timebase.num == 0 {
    // Zero seconds-per-tick -> infinite tick rate.
    return i64::MAX;
  }

  let total_nanos: u128 = (d.as_secs() as u128)
    .saturating_mul(1_000_000_000u128)
    .saturating_add(d.subsec_nanos() as u128);

  let numerator = match total_nanos.checked_mul(timebase.den as u128) {
    Some(v) => v,
    None => return i64::MAX,
  };

  let denom = (timebase.num as u128) * 1_000_000_000u128;
  if denom == 0 {
    return i64::MAX;
  }

  // Round to nearest tick.
  let ticks = numerator
    .saturating_add(denom / 2)
    .checked_div(denom)
    .unwrap_or(u128::MAX);

  if ticks > i64::MAX as u128 {
    i64::MAX
  } else {
    ticks as i64
  }
}

/// Convert container ticks into a signed [`MediaTimestamp`].
///
/// Unlike [`ticks_to_duration`], this preserves negative values.
///
/// The conversion rounds to the nearest nanosecond and saturates to `i64::MIN..=i64::MAX` on
/// overflow.
pub fn ticks_to_timestamp(ticks: i64, timebase: Timebase) -> MediaTimestamp {
  if ticks == 0 {
    return MediaTimestamp::ZERO;
  }
  if timebase.num == 0 {
    return MediaTimestamp::ZERO;
  }
  if timebase.den == 0 {
    return if ticks > 0 {
      MediaTimestamp::from_nanos(i64::MAX)
    } else {
      MediaTimestamp::from_nanos(i64::MIN)
    };
  }

  let numerator = (ticks as i128)
    .saturating_mul(timebase.num as i128)
    .saturating_mul(1_000_000_000i128);
  let den = timebase.den as i128;

  // Round to nearest nanosecond (ties away from zero) so that `timestamp_to_ticks` round-trips
  // cleanly for both positive and negative values.
  let nanos_i128 = if numerator >= 0 {
    (numerator + (den / 2)) / den
  } else {
    (numerator - (den / 2)) / den
  };

  let nanos = if nanos_i128 > i64::MAX as i128 {
    i64::MAX
  } else if nanos_i128 < i64::MIN as i128 {
    i64::MIN
  } else {
    nanos_i128 as i64
  };

  MediaTimestamp::from_nanos(nanos)
}

/// Convert a signed [`MediaTimestamp`] into container ticks for the provided timebase.
///
/// This is a best-effort conversion: it rounds to the nearest tick and saturates to
/// `i64::{MIN,MAX}` on overflow.
pub fn timestamp_to_ticks(ts: MediaTimestamp, timebase: Timebase) -> i64 {
  let nanos = ts.as_nanos();
  if nanos == 0 {
    return 0;
  }
  if timebase.den == 0 {
    // Infinite seconds-per-tick -> ~0 ticks per second.
    return 0;
  }
  if timebase.num == 0 {
    // Zero seconds-per-tick -> infinite tick rate.
    return if nanos >= 0 { i64::MAX } else { i64::MIN };
  }

  let numerator = (nanos as i128).saturating_mul(timebase.den as i128);
  let denom = (timebase.num as i128).saturating_mul(1_000_000_000i128);
  if denom == 0 {
    return if nanos >= 0 { i64::MAX } else { i64::MIN };
  }

  // Round to nearest tick (ties away from zero).
  let ticks_i128 = if numerator >= 0 {
    (numerator + (denom / 2)) / denom
  } else {
    (numerator - (denom / 2)) / denom
  };

  if ticks_i128 > i64::MAX as i128 {
    i64::MAX
  } else if ticks_i128 < i64::MIN as i128 {
    i64::MIN
  } else {
    ticks_i128 as i64
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ticks_to_duration_common_timebases() {
    let tb_90k = Timebase::new(1, 90_000);
    assert_eq!(ticks_to_duration(90_000, tb_90k), Duration::from_secs(1));
    assert_eq!(ticks_to_duration(45_000, tb_90k), Duration::from_millis(500));
    assert_eq!(ticks_to_duration(0, tb_90k), Duration::ZERO);
    assert_eq!(ticks_to_duration(-1, tb_90k), Duration::ZERO);

    let tb_ms = Timebase::new(1, 1_000);
    assert_eq!(ticks_to_duration(1, tb_ms), Duration::from_millis(1));
    assert_eq!(ticks_to_duration(1_000, tb_ms), Duration::from_secs(1));
  }

  #[test]
  fn duration_to_ticks_common_timebases() {
    let tb_90k = Timebase::new(1, 90_000);
    assert_eq!(duration_to_ticks(Duration::from_secs(1), tb_90k), 90_000);
    assert_eq!(duration_to_ticks(Duration::from_millis(500), tb_90k), 45_000);
    assert_eq!(duration_to_ticks(Duration::from_millis(1), tb_90k), 90);

    let tb_ms = Timebase::new(1, 1_000);
    assert_eq!(duration_to_ticks(Duration::from_millis(1), tb_ms), 1);
    assert_eq!(duration_to_ticks(Duration::from_secs(1), tb_ms), 1_000);
  }

  #[test]
  fn ticks_to_duration_saturates() {
    let huge = Timebase::new(u32::MAX, 1);
    assert_eq!(ticks_to_duration(i64::MAX, huge), Duration::MAX);
  }

  #[test]
  fn duration_to_ticks_saturates() {
    let tb_1 = Timebase::new(1, 1);
    assert_eq!(duration_to_ticks(Duration::MAX, tb_1), i64::MAX);
  }

  #[test]
  fn round_trip_ticks_duration_ticks_within_one_tick() {
    let tb = Timebase::new(1, 90_000);
    let samples: &[i64] = &[
      0,
      1,
      2,
      10,
      11,
      89,
      90,
      91,
      999,
      1_000,
      1_001,
      44_999,
      45_000,
      45_001,
      89_999,
      90_000,
      90_001,
      180_000,
    ];

    for &ticks in samples {
      let d = ticks_to_duration(ticks, tb);
      let ticks2 = duration_to_ticks(d, tb);
      let diff = (ticks2 - ticks).abs();
      assert!(
        diff <= 1,
        "round-trip drift too large: ticks={ticks} -> {d:?} -> {ticks2} (diff={diff})"
      );
    }
  }

  #[test]
  fn negative_ticks_round_trip_via_timestamp_within_one_tick() {
    let tb = Timebase::new(1, 90_000);
    let samples: &[i64] = &[
      -180_000,
      -90_001,
      -90_000,
      -89_999,
      -45_001,
      -45_000,
      -44_999,
      -1_001,
      -1_000,
      -999,
      -91,
      -90,
      -89,
      -11,
      -10,
      -2,
      -1,
      0,
      1,
      2,
      10,
      11,
      89,
      90,
      91,
    ];

    for &ticks in samples {
      let ts = ticks_to_timestamp(ticks, tb);
      let ticks2 = timestamp_to_ticks(ts, tb);
      let diff = (ticks2 - ticks).abs();
      assert!(
        diff <= 1,
        "round-trip drift too large: ticks={ticks} -> {ts:?} -> {ticks2} (diff={diff})"
      );
    }
  }

  #[test]
  fn ticks_to_timestamp_saturates_on_overflow() {
    let huge = Timebase::new(u32::MAX, 1);
    assert_eq!(
      ticks_to_timestamp(i64::MAX, huge),
      MediaTimestamp::from_nanos(i64::MAX)
    );
    assert_eq!(
      ticks_to_timestamp(i64::MIN, huge),
      MediaTimestamp::from_nanos(i64::MIN)
    );
  }

  #[test]
  fn timestamp_to_ticks_saturates_on_overflow() {
    let huge_rate = Timebase::new(1, u32::MAX);
    assert_eq!(
      timestamp_to_ticks(MediaTimestamp::from_nanos(i64::MAX), huge_rate),
      i64::MAX
    );
    assert_eq!(
      timestamp_to_ticks(MediaTimestamp::from_nanos(i64::MIN), huge_rate),
      i64::MIN
    );
  }

  #[test]
  fn media_timestamp_checked_add_sub_overflow() {
    let near_max = MediaTimestamp::from_nanos(i64::MAX - 1);
    assert_eq!(near_max.checked_add(Duration::from_nanos(2)), None);

    let near_min = MediaTimestamp::from_nanos(i64::MIN + 1);
    assert_eq!(near_min.checked_sub(Duration::from_nanos(2)), None);

    // A Duration that cannot fit in i64 nanoseconds should also return None.
    assert_eq!(MediaTimestamp::ZERO.checked_add(Duration::MAX), None);
  }
}
