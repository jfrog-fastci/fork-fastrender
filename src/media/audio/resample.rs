//! Minimal audio resampling helpers.
//!
//! The media pipeline often decodes audio at 44.1kHz while output devices commonly run at 48kHz.
//! Additionally, `HTMLMediaElement.playbackRate` is most easily implemented as naive resampling
//! (speed + pitch shift).
//!
//! This module provides a deterministic, dependency-free linear resampler for interleaved `f32`
//! PCM audio.

/// Computes the number of input frames required to produce `output_frames` output frames.
///
/// This value is useful for pull-based pipelines ("we need N frames for the audio callback; how
/// much decoded audio must we have buffered?").
///
/// The returned value is sufficient to avoid clamping the interpolation at the end of the input
/// buffer (i.e. the last output frame can safely read `idx + 1`).
#[must_use]
pub fn required_input_frames_for_output_frames(
  output_frames: usize,
  input_sample_rate_hz: u32,
  output_sample_rate_hz: u32,
) -> usize {
  required_input_frames_for_output_frames_with_playback_rate(
    output_frames,
    input_sample_rate_hz,
    output_sample_rate_hz,
    1.0,
  )
}

/// Like [`required_input_frames_for_output_frames`], but includes a playback-rate multiplier.
#[must_use]
pub fn required_input_frames_for_output_frames_with_playback_rate(
  output_frames: usize,
  input_sample_rate_hz: u32,
  output_sample_rate_hz: u32,
  playback_rate: f64,
) -> usize {
  if output_frames == 0 {
    return 0;
  }

  if input_sample_rate_hz == 0 || output_sample_rate_hz == 0 {
    return 0;
  }

  let playback_rate = if playback_rate.is_finite() && playback_rate > 0.0 {
    playback_rate
  } else {
    return 0;
  };

  // Passthrough: a strict match can consume exactly N frames without interpolation look-ahead.
  if playback_rate == 1.0 && input_sample_rate_hz == output_sample_rate_hz {
    return output_frames;
  }

  // We generate output samples at times t = i / output_rate.
  // Those correspond to input positions p = t * input_rate * playback_rate.
  // The last output sample is at i = output_frames - 1.
  let last_i = output_frames.saturating_sub(1);

  if playback_rate == 1.0 {
    // Exact integer math for the common case.
    let num = (last_i as u128).saturating_mul(input_sample_rate_hz as u128);
    let idx = num / output_sample_rate_hz as u128;
    // Need idx and idx+1.
    return usize::try_from(idx.saturating_add(2)).unwrap_or(usize::MAX);
  }

  // Fall back to float math; use ceil to avoid underestimating due to rounding.
  let pos_max = (last_i as f64)
    * (input_sample_rate_hz as f64)
    * playback_rate
    / (output_sample_rate_hz as f64);
  if !(pos_max.is_finite()) || pos_max < 0.0 {
    return 0;
  }

  let idx = pos_max.ceil() as u128;
  usize::try_from(idx.saturating_add(2)).unwrap_or(usize::MAX)
}

/// Linear interpolation resampler for interleaved `f32` PCM samples.
///
/// - `input` is interleaved PCM (`frames * channels` samples).
/// - `channels` is the number of channels in `input` and output.
/// - `output_frames` controls the number of frames produced.
///
/// If sample rates match and `playback_rate == 1.0`, this is a fast passthrough that simply copies
/// `output_frames * channels` samples.
#[must_use]
pub fn resample_interleaved_f32_linear_with_playback_rate(
  input: &[f32],
  channels: usize,
  input_sample_rate_hz: u32,
  output_sample_rate_hz: u32,
  playback_rate: f64,
  output_frames: usize,
) -> Vec<f32> {
  let mut out = Vec::new();
  resample_interleaved_f32_linear_with_playback_rate_into(
    &mut out,
    input,
    channels,
    input_sample_rate_hz,
    output_sample_rate_hz,
    playback_rate,
    output_frames,
  );
  out
}

/// Like [`resample_interleaved_f32_linear_with_playback_rate`], writing into a reusable output
/// buffer.
pub fn resample_interleaved_f32_linear_with_playback_rate_into(
  out: &mut Vec<f32>,
  input: &[f32],
  channels: usize,
  input_sample_rate_hz: u32,
  output_sample_rate_hz: u32,
  playback_rate: f64,
  output_frames: usize,
) {
  out.clear();

  if output_frames == 0 || channels == 0 {
    return;
  }

  if input_sample_rate_hz == 0 || output_sample_rate_hz == 0 {
    return;
  }

  let playback_rate = if playback_rate.is_finite() && playback_rate > 0.0 {
    playback_rate
  } else {
    return;
  };

  let usable_len = input.len() - (input.len() % channels);
  let input = &input[..usable_len];
  let input_frames = input.len() / channels;
  if input_frames == 0 {
    return;
  }

  // Fast path for the overwhelmingly common case (no resampling).
  if playback_rate == 1.0 && input_sample_rate_hz == output_sample_rate_hz {
    let samples_to_copy = output_frames.saturating_mul(channels).min(input.len());
    out.extend_from_slice(&input[..samples_to_copy]);
    return;
  }

  out.resize(output_frames.saturating_mul(channels), 0.0);

  // For playback_rate==1.0 we can compute `idx`/`frac` without floating-point drift.
  if playback_rate == 1.0 {
    let out_rate_u128 = output_sample_rate_hz as u128;
    let in_rate_u128 = input_sample_rate_hz as u128;
    let out_rate_f32 = output_sample_rate_hz as f32;

    for out_i in 0..output_frames {
      let num = (out_i as u128).saturating_mul(in_rate_u128);
      let idx = (num / out_rate_u128) as usize;
      let frac_num = (num % out_rate_u128) as f32;
      let frac = frac_num / out_rate_f32;

      let idx0 = idx.min(input_frames - 1);
      let idx1 = (idx0 + 1).min(input_frames - 1);

      let base0 = idx0 * channels;
      let base1 = idx1 * channels;
      let out_base = out_i * channels;
      for ch in 0..channels {
        let s0 = input[base0 + ch];
        let s1 = input[base1 + ch];
        out[out_base + ch] = s0 + (s1 - s0) * frac;
      }
    }
    return;
  }

  // General path: compute sample positions in floating point.
  let step = (input_sample_rate_hz as f64) * playback_rate / (output_sample_rate_hz as f64);
  if !(step.is_finite()) || step <= 0.0 {
    out.clear();
    return;
  }

  // Compute positions from an absolute origin (out_i * step) rather than incrementally accumulating
  // error.
  for out_i in 0..output_frames {
    let pos = (out_i as f64) * step;
    if !(pos.is_finite()) {
      // Keep output deterministic: if math goes bad, fill remaining with silence.
      break;
    }

    let idx = pos.floor();
    let frac = (pos - idx) as f32;
    let idx = idx as isize;

    let idx0 = idx.clamp(0, (input_frames - 1) as isize) as usize;
    let idx1 = (idx0 + 1).min(input_frames - 1);

    let base0 = idx0 * channels;
    let base1 = idx1 * channels;
    let out_base = out_i * channels;
    for ch in 0..channels {
      let s0 = input[base0 + ch];
      let s1 = input[base1 + ch];
      out[out_base + ch] = s0 + (s1 - s0) * frac;
    }
  }
}

/// Convenience wrapper for the common case (no playback-rate scaling).
#[must_use]
pub fn resample_interleaved_f32_linear(
  input: &[f32],
  channels: usize,
  input_sample_rate_hz: u32,
  output_sample_rate_hz: u32,
  output_frames: usize,
) -> Vec<f32> {
  resample_interleaved_f32_linear_with_playback_rate(
    input,
    channels,
    input_sample_rate_hz,
    output_sample_rate_hz,
    1.0,
    output_frames,
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn audio_resample_passthrough_produces_identical_output() {
    let input_rate = 48_000;
    let output_rate = 48_000;
    let channels = 2;
    let frames = 16;

    let mut input = Vec::with_capacity(frames * channels);
    for i in 0..(frames * channels) {
      input.push((i as f32) * 0.25 - 10.0);
    }

    let out = resample_interleaved_f32_linear_with_playback_rate(
      &input,
      channels,
      input_rate,
      output_rate,
      1.0,
      frames,
    );
    assert_eq!(out, input);

    assert_eq!(
      required_input_frames_for_output_frames(frames, input_rate, output_rate),
      frames
    );
  }

  #[test]
  fn audio_resample_44100_to_48000_ramp_interpolates_as_expected() {
    let input_rate = 44_100;
    let output_rate = 48_000;
    let channels = 1;
    let output_frames = 160;
    let input_frames = required_input_frames_for_output_frames(output_frames, input_rate, output_rate);

    // Ramp signal: sample value == frame index. Linear interpolation should reproduce the exact
    // fractional input position.
    let input: Vec<f32> = (0..input_frames).map(|i| i as f32).collect();

    let out = resample_interleaved_f32_linear(&input, channels, input_rate, output_rate, output_frames);
    assert_eq!(out.len(), output_frames * channels);

    for (out_i, sample) in out.iter().enumerate() {
      let num = (out_i as u64) * (input_rate as u64);
      let idx = num / (output_rate as u64);
      let frac_num = num % (output_rate as u64);
      let expected = (idx as f32) + (frac_num as f32) / (output_rate as f32);
      let err = (*sample - expected).abs();
      assert!(
        err < 1e-5,
        "out[{out_i}] = {sample} != {expected} (err={err})"
      );
    }
  }
}

