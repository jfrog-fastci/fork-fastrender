//! Deterministic audio test signal generators.
//!
//! These helpers are intended for unit tests and debugging tools that need a known source of PCM
//! data without depending on external assets.

use std::time::Duration;

use super::duration_to_frames_floor;

/// Generate an interleaved sine wave buffer (frames-based).
///
/// Returned samples are `f32` in the `[-1.0, 1.0]` range (inclusive, modulo floating-point
/// rounding), interleaved per frame (i.e. `[L0, R0, L1, R1, ...]`).
///
/// The output is guaranteed to be finite and clamped to `[-1.0, 1.0]`.
#[must_use]
pub fn sine_wave(freq_hz: f32, sample_rate: u32, channels: u16, frames: usize) -> Vec<f32> {
  let channels_usize = usize::from(channels);
  let Some(len) = frames.checked_mul(channels_usize) else {
    return Vec::new();
  };
  if len == 0 {
    return Vec::new();
  }

  // Defensive: invalid inputs should never produce NaNs/Infs (useful for fuzzing and debug
  // tooling). Degrade to silence instead of panicking.
  if sample_rate == 0 || channels == 0 || !freq_hz.is_finite() {
    return vec![0.0; len];
  }

  let phase_inc = std::f32::consts::TAU * freq_hz / (sample_rate as f32);
  if !phase_inc.is_finite() {
    return vec![0.0; len];
  }

  let mut out = Vec::with_capacity(len);
  let mut phase = 0.0_f32;
  for _ in 0..frames {
    let mut v = phase.sin();
    if !v.is_finite() {
      v = 0.0;
    } else if v > 1.0 {
      v = 1.0;
    } else if v < -1.0 {
      v = -1.0;
    }

    out.extend(std::iter::repeat(v).take(channels_usize));
    // Keep phase bounded for long buffers.
    phase = (phase + phase_inc).rem_euclid(std::f32::consts::TAU);
  }

  out
}

/// Generate an interleaved impulse buffer (frames-based).
///
/// The first frame is `1.0` on every channel; all remaining frames are `0.0`.
///
/// The output is guaranteed to be finite and within `[-1.0, 1.0]`.
#[must_use]
pub fn impulse(sample_rate: u32, channels: u16, frames: usize) -> Vec<f32> {
  let _ = sample_rate;

  let channels_usize = usize::from(channels);
  let Some(len) = frames.checked_mul(channels_usize) else {
    return Vec::new();
  };
  if len == 0 {
    return Vec::new();
  }

  let mut out = vec![0.0; len];
  for ch in 0..channels_usize {
    out[ch] = 1.0;
  }
  out
}

/// Generate an interleaved sine wave buffer for a given duration.
///
/// This is a convenience wrapper that computes `frames` via [`duration_to_frames_floor`] and then
/// delegates to [`sine_wave`].
#[must_use]
pub fn sine(freq_hz: f32, duration: Duration, sample_rate: u32, channels: u16) -> Vec<f32> {
  let frames = match usize::try_from(duration_to_frames_floor(sample_rate, duration)) {
    Ok(v) => v,
    Err(_) => return Vec::new(),
  };
  sine_wave(freq_hz, sample_rate, channels, frames)
}

/// Generate an interleaved impulse buffer for a given duration.
///
/// Convenience wrapper over [`impulse`] that computes `frames` via [`duration_to_frames_floor`].
#[must_use]
pub fn impulse_duration(duration: Duration, sample_rate: u32, channels: u16) -> Vec<f32> {
  let frames = match usize::try_from(duration_to_frames_floor(sample_rate, duration)) {
    Ok(v) => v,
    Err(_) => return Vec::new(),
  };
  impulse(sample_rate, channels, frames)
}

/// Generate an interleaved linear ramp buffer for a given duration.
///
/// The returned signal linearly interpolates from `-1.0` (first frame) to `1.0` (last frame).
///
/// For `0` frames this returns an empty buffer. For `1` frame this returns a single frame of
/// silence (`0.0`) to avoid the ambiguous "start vs end" value.
#[must_use]
pub fn ramp(duration: Duration, sample_rate: u32, channels: u16) -> Vec<f32> {
  let frames = match usize::try_from(duration_to_frames_floor(sample_rate, duration)) {
    Ok(v) => v,
    Err(_) => return Vec::new(),
  };
  let channels_usize = usize::from(channels);

  if frames == 0 || channels_usize == 0 {
    return Vec::new();
  }
  if frames == 1 {
    return vec![0.0; channels_usize];
  }

  let Some(len) = frames.checked_mul(channels_usize) else {
    return Vec::new();
  };

  let mut out = Vec::with_capacity(len);
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
  use super::*;

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

    let buf = impulse_duration(duration, sample_rate, channels);
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

  #[test]
  fn audio_test_signal_generators_are_finite_and_bounded() {
    let sample_rate = 48_000;
    let channels = 2;
    let frames = 1_000;

    let sine = sine_wave(440.0, sample_rate, channels, frames);
    assert_eq!(sine.len(), frames * usize::from(channels));
    assert!(sine
      .iter()
      .all(|sample| sample.is_finite() && *sample >= -1.0 && *sample <= 1.0));
    // First sample should be sin(0) = 0.
    assert!(sine[0].abs() <= 1e-6);
    // Channels are duplicated per frame.
    for frame in sine.chunks_exact(usize::from(channels)) {
      assert!(frame.iter().all(|sample| *sample == frame[0]));
    }

    // Invalid inputs should degrade to silence without NaNs/Infs.
    let nan_freq = sine_wave(f32::NAN, sample_rate, channels, frames);
    assert!(nan_freq.iter().all(|sample| *sample == 0.0));
    let zero_sr = sine_wave(440.0, 0, channels, frames);
    assert!(zero_sr.iter().all(|sample| *sample == 0.0));

    let imp = impulse(sample_rate, channels, frames);
    assert_eq!(imp.len(), frames * usize::from(channels));
    assert!(imp
      .iter()
      .all(|sample| sample.is_finite() && *sample >= -1.0 && *sample <= 1.0));

    for ch in 0..usize::from(channels) {
      assert_eq!(imp[ch], 1.0);
    }
    assert!(imp[usize::from(channels)..].iter().all(|sample| *sample == 0.0));

    // Empty cases.
    assert!(sine_wave(440.0, sample_rate, 0, frames).is_empty());
    assert!(sine_wave(440.0, sample_rate, channels, 0).is_empty());
    assert!(impulse(sample_rate, 0, frames).is_empty());
    assert!(impulse(sample_rate, channels, 0).is_empty());
  }
}
