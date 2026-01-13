use std::time::Duration;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Converts a frame count at the given sample rate into a [`Duration`].
///
/// This uses integer arithmetic and rounds to the nearest nanosecond.
pub fn frames_to_duration(sample_rate_hz: u32, frames: u64) -> Duration {
  if sample_rate_hz == 0 {
    return Duration::ZERO;
  }

  let rate = sample_rate_hz as u128;
  let total_nanos = (frames as u128)
    .checked_mul(NANOS_PER_SEC)
    .and_then(|v| v.checked_add(rate / 2))
    .and_then(|v| v.checked_div(rate))
    .unwrap_or(u128::MAX);

  let secs = total_nanos / NANOS_PER_SEC;
  let nanos = total_nanos % NANOS_PER_SEC;
  if secs > (u64::MAX as u128) {
    Duration::MAX
  } else {
    // Safe: nanos is in [0, 1e9).
    Duration::new(secs as u64, nanos as u32)
  }
}

/// Converts a [`Duration`] into frames at the given sample rate, rounding down.
pub fn duration_to_frames_floor(sample_rate_hz: u32, duration: Duration) -> u64 {
  if sample_rate_hz == 0 {
    return 0;
  }
  let nanos = duration.as_nanos();
  let frames = nanos.saturating_mul(sample_rate_hz as u128) / NANOS_PER_SEC;
  u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Converts a [`Duration`] into frames at the given sample rate, rounding up.
pub fn duration_to_frames_ceil(sample_rate_hz: u32, duration: Duration) -> u64 {
  if sample_rate_hz == 0 {
    return 0;
  }
  let nanos = duration.as_nanos();
  let frames =
    (nanos.saturating_mul(sample_rate_hz as u128) + (NANOS_PER_SEC - 1)) / NANOS_PER_SEC;
  u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Computes an output latency estimate from audio callback timestamps.
///
/// CPAL exposes both the instant the callback was invoked and the instant the first sample written
/// by that callback will be played back. The delta is the output latency (in time units).
pub fn latency_from_timestamps(callback: Duration, playback: Duration) -> Option<Duration> {
  playback.checked_sub(callback)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn audio_output_info_latency_from_fixed_buffer_is_frames_over_sample_rate() {
    // These values are chosen to be exactly representable as milliseconds.
    assert_eq!(frames_to_duration(48_000, 480), Duration::from_millis(10));
    assert_eq!(frames_to_duration(44_100, 441), Duration::from_millis(10));
  }

  #[test]
  fn frames_to_duration_exact() {
    assert_eq!(frames_to_duration(48_000, 48_000), Duration::from_secs(1));
    assert_eq!(frames_to_duration(48_000, 480), Duration::from_millis(10));
    assert_eq!(frames_to_duration(44_100, 441), Duration::from_millis(10));
  }

  #[test]
  fn duration_to_frames_rounding() {
    assert_eq!(
      duration_to_frames_floor(48_000, Duration::from_millis(10)),
      480
    );
    assert_eq!(
      duration_to_frames_ceil(48_000, Duration::from_millis(10)),
      480
    );

    // 1ns is less than one frame at 48kHz; ceil should return 1, floor should return 0.
    assert_eq!(duration_to_frames_floor(48_000, Duration::from_nanos(1)), 0);
    assert_eq!(duration_to_frames_ceil(48_000, Duration::from_nanos(1)), 1);
  }

  #[test]
  fn latency_from_timestamps_math() {
    assert_eq!(
      latency_from_timestamps(Duration::from_secs(1), Duration::from_millis(1010)),
      Some(Duration::from_millis(10))
    );
    assert_eq!(
      latency_from_timestamps(Duration::from_secs(1), Duration::from_millis(900)),
      None
    );
  }
}
