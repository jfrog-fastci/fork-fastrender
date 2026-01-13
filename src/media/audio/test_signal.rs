use std::f32::consts::PI;
use std::time::Duration;

use super::duration_to_frames_floor;

/// Generate an interleaved sine-wave buffer.
///
/// Returned samples are `f32` in the `[-1.0, 1.0]` range (inclusive, modulo floating-point
/// rounding), interleaved per-frame (i.e. `[L0, R0, L1, R1, ...]`).
pub fn sine(freq_hz: f32, duration: Duration, sample_rate: u32, channels: u16) -> Vec<f32> {
  assert!(sample_rate > 0, "sample_rate must be > 0");
  assert!(channels > 0, "channels must be > 0");
  assert!(freq_hz.is_finite(), "freq_hz must be finite");

  let frames =
    usize::try_from(duration_to_frames_floor(sample_rate, duration)).expect("frame count overflow");
  let channels_usize = channels as usize;

  let sr = sample_rate as f32;
  let omega = 2.0 * PI * freq_hz / sr;

  let mut out = Vec::with_capacity(frames * channels_usize);
  for i in 0..frames {
    let v = (omega * i as f32).sin();
    out.extend(std::iter::repeat(v).take(channels_usize));
  }
  out
}

/// Generate an interleaved impulse buffer.
///
/// The first frame is `1.0` on every channel; all remaining frames are `0.0`.
pub fn impulse(duration: Duration, sample_rate: u32, channels: u16) -> Vec<f32> {
  assert!(sample_rate > 0, "sample_rate must be > 0");
  assert!(channels > 0, "channels must be > 0");

  let frames =
    usize::try_from(duration_to_frames_floor(sample_rate, duration)).expect("frame count overflow");
  let channels_usize = channels as usize;
  let mut out = vec![0.0; frames * channels_usize];
  if frames > 0 {
    for ch in 0..channels_usize {
      out[ch] = 1.0;
    }
  }
  out
}

/// Generate an interleaved linear ramp buffer.
///
/// The returned signal linearly interpolates from `-1.0` (first frame) to `1.0` (last frame).
///
/// For `0` frames this returns an empty buffer. For `1` frame this returns a single frame of
/// silence (`0.0`) to avoid the ambiguous "start vs end" value.
pub fn ramp(duration: Duration, sample_rate: u32, channels: u16) -> Vec<f32> {
  assert!(sample_rate > 0, "sample_rate must be > 0");
  assert!(channels > 0, "channels must be > 0");

  let frames =
    usize::try_from(duration_to_frames_floor(sample_rate, duration)).expect("frame count overflow");
  let channels_usize = channels as usize;

  if frames == 0 {
    return Vec::new();
  }
  if frames == 1 {
    return vec![0.0; channels_usize];
  }

  let mut out = Vec::with_capacity(frames * channels_usize);
  let denom = (frames - 1) as f32;
  for i in 0..frames {
    let t = i as f32 / denom;
    let v = -1.0 + 2.0 * t;
    out.extend(std::iter::repeat(v).take(channels_usize));
  }
  out
}

#[cfg(test)]
mod tests {
  use super::{impulse, ramp, sine};
  use std::time::Duration;

  #[test]
  fn sine_has_expected_length_and_is_bounded() {
    let sample_rate = 48_000;
    let channels = 2;
    let duration = Duration::from_millis(100);

    let buf = sine(440.0, duration, sample_rate, channels);
    assert_eq!(buf.len(), 9_600);
    assert!(
      buf.iter().all(|v| v.is_finite()),
      "all generated samples should be finite"
    );
    assert!(
      buf.iter().all(|v| v.abs() <= 1.000_001),
      "sine samples should stay within [-1, 1]"
    );
  }

  #[test]
  fn impulse_has_expected_shape() {
    let sample_rate = 1_000;
    let channels = 1;
    let duration = Duration::from_millis(10);

    let buf = impulse(duration, sample_rate, channels);
    assert_eq!(buf.len(), 10);
    assert_eq!(buf[0], 1.0);
    assert!(buf[1..].iter().all(|v| *v == 0.0));
  }

  #[test]
  fn ramp_has_expected_endpoints() {
    let sample_rate = 10;
    let channels = 1;
    let duration = Duration::from_millis(500); // 5 frames at 10Hz

    let buf = ramp(duration, sample_rate, channels);
    assert_eq!(buf.len(), 5);
    let eps = 1e-6;
    assert!((buf[0] - (-1.0)).abs() < eps);
    assert!((buf[2] - 0.0).abs() < eps);
    assert!((buf[4] - 1.0).abs() < eps);
    assert!(buf.iter().all(|v| v.abs() <= 1.000_001));
  }
}
