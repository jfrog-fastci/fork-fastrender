//! Audio format conversion utilities.
//!
//! This module provides:
//! - **Sample format/layout normalization** for decoder-facing buffers (planar/interleaved, various
//!   integer formats) via [`convert_to_f32_interleaved`].
//! - **Channel remixing** between common layouts (mono/stereo, N→stereo).
//! - **Sample-rate conversion** using a simple linear-interpolation resampler.
//!
//! Note: the resampler here is an MVP implementation. It is dependency-free and fast, but it is
//! not band-limited and can introduce audible artifacts (aliasing / high-frequency loss).
//!
//! If we *don't* resample when the decoder output rate doesn't match the output device rate,
//! playback will run at the wrong speed (perceived as pitch/time changes). These helpers are
//! intended to make audio output robust in common cases like 44.1kHz↔48kHz and mono↔stereo.
use std::collections::VecDeque;

use super::types::{AudioBuffer, AudioSamples};
use super::limits::{MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};
use super::AudioError;

fn i16_to_f32(sample: i16) -> f32 {
  sample as f32 / 32768.0
}

fn u16_to_f32(sample: u16) -> f32 {
  (sample as f32 - 32768.0) / 32768.0
}

/// Convert an [`AudioBuffer`] into interleaved `f32` PCM.
///
/// This validates the buffer's metadata and layout invariants before converting.
pub fn convert_to_f32_interleaved(buffer: &AudioBuffer<'_>) -> Result<Vec<f32>, AudioError> {
  let max_channels = usize::from(MAX_CHANNELS);
  if buffer.channels == 0 || buffer.channels > max_channels {
    return Err(AudioError::invalid_spec(format!(
      "channels {} is outside supported range 1..={}",
      buffer.channels, max_channels
    )));
  }
  if buffer.sample_rate == 0 || buffer.sample_rate > MAX_SAMPLE_RATE_HZ {
    return Err(AudioError::invalid_spec(format!(
      "sample_rate {} is outside supported range 1..={}",
      buffer.sample_rate, MAX_SAMPLE_RATE_HZ
    )));
  }

  let data_format = buffer.data.format();
  let data_layout = buffer.data.layout();
  if buffer.format != data_format || buffer.layout != data_layout {
    return Err(AudioError::BufferMetadataMismatch {
      format: buffer.format,
      data_format,
      layout: buffer.layout,
      data_layout,
    });
  }

  match buffer.data {
    AudioSamples::InterleavedF32(samples) => {
      validate_interleaved_len(samples.len(), buffer.channels)?;
      validate_frames_limit(samples.len() / buffer.channels)?;
      Ok(samples.to_vec())
    }
    AudioSamples::InterleavedI16(samples) => {
      validate_interleaved_len(samples.len(), buffer.channels)?;
      validate_frames_limit(samples.len() / buffer.channels)?;
      Ok(samples.iter().copied().map(i16_to_f32).collect())
    }
    AudioSamples::InterleavedU16(samples) => {
      validate_interleaved_len(samples.len(), buffer.channels)?;
      validate_frames_limit(samples.len() / buffer.channels)?;
      Ok(samples.iter().copied().map(u16_to_f32).collect())
    }
    AudioSamples::PlanarF32(planes) => planar_to_f32_interleaved(planes, buffer.channels, |s| s),
    AudioSamples::PlanarI16(planes) => planar_to_f32_interleaved(planes, buffer.channels, i16_to_f32),
    AudioSamples::PlanarU16(planes) => planar_to_f32_interleaved(planes, buffer.channels, u16_to_f32),
  }
}

fn validate_interleaved_len(len_samples: usize, channels: usize) -> Result<(), AudioError> {
  if len_samples % channels != 0 {
    return Err(AudioError::InvalidInterleavedLength {
      len_samples,
      channels,
    });
  }
  Ok(())
}

fn validate_frames_limit(frames: usize) -> Result<(), AudioError> {
  if frames > MAX_FRAMES_PER_PUSH {
    return Err(AudioError::invalid_buffer(format!(
      "audio buffer has {} frames which exceeds MAX_FRAMES_PER_PUSH {}",
      frames, MAX_FRAMES_PER_PUSH
    )));
  }
  Ok(())
}

fn planar_to_f32_interleaved<T: Copy>(
  planes: &[&[T]],
  channels: usize,
  to_f32: impl Fn(T) -> f32,
) -> Result<Vec<f32>, AudioError> {
  if planes.len() != channels {
    return Err(AudioError::InvalidPlaneCount {
      channels,
      planes: planes.len(),
    });
  }

  let frames = planes.first().map_or(0, |first_plane| first_plane.len());

  for (i, plane) in planes.iter().enumerate() {
    if plane.len() != frames {
      return Err(AudioError::InvalidPlaneLength {
        plane: i,
        len_samples: plane.len(),
        expected_samples: frames,
      });
    }
  }

  validate_frames_limit(frames)?;

  let len_samples = frames
    .checked_mul(channels)
    .ok_or_else(|| AudioError::invalid_buffer("audio sample count overflow"))?;

  let mut out = Vec::with_capacity(len_samples);
  for frame in 0..frames {
    for chan in 0..channels {
      out.push(to_f32(planes[chan][frame]));
    }
  }
  Ok(out)
}

/// Sanitize a sample before it can be accumulated into a mix buffer.
///
/// This is intended for *intermediate* mixing math, so it intentionally does **not** clamp the
/// amplitude. (Some unit tests and internal mixers use values outside [-1, 1].)
///
/// Goals:
/// - Prevent NaNs/Infs from poisoning the entire mixed output.
/// - Flush subnormals to zero to avoid denormal performance traps in DSP code paths.
#[inline]
pub(crate) fn sanitize_mix_sample(x: f32) -> f32 {
  if !x.is_finite() {
    return 0.0;
  }
  if !x.is_normal() {
    return 0.0;
  }
  x
}

/// Sanitize a decoded/mixed audio sample before it can be converted to the final output format.
///
/// This clamps to the expected output range (typically [-1, 1]) after flushing NaN/Inf/subnormals.
#[inline]
pub(crate) fn sanitize_sample(x: f32) -> f32 {
  // 1) NaN / +/-Inf are never meaningful as PCM.
  if !x.is_finite() {
    return 0.0;
  }

  // 2) `is_normal` rejects subnormals (and also zero). We flush both to zero to
  //    ensure we never emit denormals to the audio device callback.
  if !x.is_normal() {
    return 0.0;
  }

  // 3) Clamp to a sane output range.
  x.clamp(-1.0, 1.0)
}

/// Sanitize an interleaved sample buffer in place.
#[inline]
pub(crate) fn sanitize_buffer_in_place(buf: &mut [f32]) {
  for x in buf {
    *x = sanitize_mix_sample(*x);
  }
}

// ============================================================================
// Channel remixing
// ============================================================================

/// Remix interleaved f32 samples from `in_channels` to `out_channels`.
///
/// The input is expected to be interleaved frames:
/// `[ch0, ch1, ... chN, ch0, ch1, ...]`.
///
/// Supported conversions (MVP):
/// - mono → stereo: duplicate
/// - stereo → mono: average `(L+R)/2`
/// - N → stereo: weighted downmix using the first two channels as L/R and
///   mixing remaining channels equally into both.
///
/// For other conversions we fall back to a naive mapping.
pub fn remix_channels_f32_into(
  input: &[f32],
  in_channels: usize,
  out_channels: usize,
  out: &mut Vec<f32>,
) {
  out.clear();

  if in_channels == 0 || out_channels == 0 {
    return;
  }

  let in_frames = input.len() / in_channels;
  if in_frames == 0 {
    return;
  }

  out.reserve(in_frames * out_channels);

  // Fast path: identical layout.
  if in_channels == out_channels {
    out.extend_from_slice(&input[..in_frames * in_channels]);
    return;
  }

  for frame_idx in 0..in_frames {
    let base = frame_idx * in_channels;

    match (in_channels, out_channels) {
      (1, 2) => {
        let s = input[base];
        out.push(s);
        out.push(s);
      }
      (2, 1) => {
        let l = input[base];
        let r = input[base + 1];
        out.push((l + r) * 0.5);
      }
      (_, 1) => {
        // Average all channels.
        let mut sum = 0.0f32;
        for ch in 0..in_channels {
          sum += input[base + ch];
        }
        out.push(sum / in_channels as f32);
      }
      (1, _) => {
        // Upmix mono → N by duplication.
        let s = input[base];
        for _ in 0..out_channels {
          out.push(s);
        }
      }
      (_, 2) => {
        // N → stereo downmix. Use ch0/ch1 as L/R, and mix remaining channels equally into both
        // outputs.
        //
        // This is intentionally simple: it's "good enough" to make common media (e.g. 5.1) audible
        // on stereo devices without complicated matrices.
        let l0 = input[base];
        let r0 = input[base + 1];

        if in_channels == 2 {
          out.push(l0);
          out.push(r0);
          continue;
        }

        let mut extra_sum = 0.0f32;
        for ch in 2..in_channels {
          extra_sum += input[base + ch];
        }

        // Each "extra" channel contributes with a reduced gain.
        let extra_weight = 0.5f32;
        let norm = 1.0f32 + extra_weight * (in_channels.saturating_sub(2)) as f32;
        let extra = extra_sum * extra_weight;

        out.push((l0 + extra) / norm);
        out.push((r0 + extra) / norm);
      }
      _ => {
        // Naive fallback: copy min(in,out) channels, pad remaining with 0.
        for ch in 0..out_channels {
          let s = if ch < in_channels { input[base + ch] } else { 0.0 };
          out.push(s);
        }
      }
    }
  }
}

/// Convenience wrapper around [`remix_channels_f32_into`] that allocates a new `Vec`.
pub fn remix_channels_f32(input: &[f32], in_channels: usize, out_channels: usize) -> Vec<f32> {
  let mut out = Vec::new();
  remix_channels_f32_into(input, in_channels, out_channels, &mut out);
  out
}

// ============================================================================
// Resampling
// ============================================================================

/// Linear-interpolation resampler for interleaved f32 audio.
///
/// - `input` is interleaved f32 frames.
/// - `channels` is the number of interleaved channels in `input`.
///
/// This is an MVP resampler; it is not band-limited.
pub fn resample_f32_into(
  input: &[f32],
  in_rate: u32,
  out_rate: u32,
  channels: usize,
  out: &mut Vec<f32>,
) {
  out.clear();

  if channels == 0 || in_rate == 0 || out_rate == 0 {
    return;
  }

  let in_frames = input.len() / channels;
  if in_frames == 0 {
    return;
  }

  if in_rate == out_rate {
    out.extend_from_slice(&input[..in_frames * channels]);
    return;
  }

  // Compute the number of output frames to preserve duration (within ~1 frame).
  let out_frames_u64 = ((in_frames as u64) * (out_rate as u64) + (in_rate as u64) / 2) / in_rate as u64;
  let out_frames = out_frames_u64.max(1) as usize;

  out.reserve(out_frames * channels);

  let step = in_rate as f64 / out_rate as f64;
  let mut pos = 0.0f64;

  for _ in 0..out_frames {
    let idx0 = pos.floor() as usize;
    let frac = (pos - idx0 as f64) as f32;

    let idx0 = idx0.min(in_frames - 1);
    let idx1 = (idx0 + 1).min(in_frames - 1);

    let base0 = idx0 * channels;
    let base1 = idx1 * channels;

    for ch in 0..channels {
      let s0 = input[base0 + ch];
      let s1 = input[base1 + ch];
      out.push(s0 + (s1 - s0) * frac);
    }

    pos += step;
  }
}

/// Convenience wrapper around [`resample_f32_into`] that allocates a new `Vec`.
pub fn resample_f32(input: &[f32], in_rate: u32, out_rate: u32, channels: usize) -> Vec<f32> {
  let mut out = Vec::new();
  resample_f32_into(input, in_rate, out_rate, channels, &mut out);
  out
}

// ============================================================================
// High-level conversion helper
// ============================================================================

/// Helper for converting decoded f32 audio into an output device format.
///
/// Stores a reusable scratch buffer so conversion does not allocate per call once warmed up.
#[derive(Default, Debug)]
pub struct AudioConverter {
  scratch: Vec<f32>,
}

impl AudioConverter {
  pub fn new() -> Self {
    Self::default()
  }

  /// Convert interleaved f32 audio from `(in_rate, in_channels)` to `(out_rate, out_channels)`.
  ///
  /// The output is written into `out`, which is cleared first.
  pub fn convert_f32_into(
    &mut self,
    input: &[f32],
    in_rate: u32,
    in_channels: usize,
    out_rate: u32,
    out_channels: usize,
    out: &mut Vec<f32>,
  ) {
    // Fast path: identical format.
    if in_rate == out_rate && in_channels == out_channels {
      out.clear();
      if in_channels == 0 {
        return;
      }
      let frames = input.len() / in_channels;
      out.extend_from_slice(&input[..frames * in_channels]);
      return;
    }

    let needs_remix = in_channels != out_channels;
    let needs_resample = in_rate != out_rate;

    // Prefer doing operations in an order that avoids unnecessary work:
    // - Downmix (channel reduction) *before* resampling.
    // - Upmix (channel expansion) *after* resampling.
    let downmix_first = needs_remix && out_channels <= in_channels;

    match (needs_remix, needs_resample, downmix_first) {
      (true, true, true) => {
        remix_channels_f32_into(input, in_channels, out_channels, &mut self.scratch);
        resample_f32_into(&self.scratch, in_rate, out_rate, out_channels, out);
      }
      (true, true, false) => {
        resample_f32_into(input, in_rate, out_rate, in_channels, &mut self.scratch);
        remix_channels_f32_into(&self.scratch, in_channels, out_channels, out);
      }
      (true, false, _) => {
        remix_channels_f32_into(input, in_channels, out_channels, out);
      }
      (false, true, _) => {
        resample_f32_into(input, in_rate, out_rate, in_channels, out);
      }
      (false, false, _) => unreachable!("handled by early return"),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::media::audio::types::{ChannelLayout, SampleFormat};

  fn assert_f32_slice_eq_eps(actual: &[f32], expected: &[f32], eps: f32) {
    assert_eq!(actual.len(), expected.len());
    for (a, e) in actual.iter().copied().zip(expected.iter().copied()) {
      assert!(
        (a - e).abs() <= eps,
        "expected {e} +/- {eps} but got {a}"
      );
    }
  }

  #[test]
  fn planar_i16_stereo_converts_to_interleaved_f32() {
    let left: [i16; 3] = [-32768, 0, 32767];
    let right: [i16; 3] = [32767, 0, -32768];
    let planes: [&[i16]; 2] = [&left, &right];

    let buffer = AudioBuffer::new(2, 48_000, None, AudioSamples::PlanarI16(&planes));

    let converted = convert_to_f32_interleaved(&buffer).unwrap();
    assert_eq!(converted.len(), 6);

    let max = 32767.0 / 32768.0;
    let expected = [-1.0, max, 0.0, 0.0, max, -1.0];
    assert_f32_slice_eq_eps(&converted, &expected, 1e-6);
  }

  #[test]
  fn malformed_interleaved_lengths_are_rejected() {
    let samples: [i16; 3] = [0, 0, 0];
    let buffer = AudioBuffer {
      format: SampleFormat::I16,
      layout: ChannelLayout::Interleaved,
      channels: 2,
      sample_rate: 44_100,
      pts: None,
      data: AudioSamples::InterleavedI16(&samples),
    };

    let err = convert_to_f32_interleaved(&buffer).unwrap_err();
    assert!(matches!(err, AudioError::InvalidInterleavedLength { .. }));
  }

  #[test]
  fn rejects_buffers_exceeding_max_frames_per_push() {
    let samples = vec![0i16; MAX_FRAMES_PER_PUSH + 1];
    let buffer = AudioBuffer::new(
      1,
      48_000,
      None,
      AudioSamples::InterleavedI16(&samples),
    );

    let err = convert_to_f32_interleaved(&buffer).unwrap_err();
    assert!(matches!(err, AudioError::InvalidBuffer { .. }));
  }

  #[test]
  fn rejects_absurd_specs() {
    let empty: [i16; 0] = [];
    let too_many_channels = usize::from(MAX_CHANNELS) + 1;
    let buffer = AudioBuffer::new(
      too_many_channels,
      48_000,
      None,
      AudioSamples::InterleavedI16(&empty),
    );
    assert!(matches!(
      convert_to_f32_interleaved(&buffer),
      Err(AudioError::InvalidSpec { .. })
    ));

    let buffer = AudioBuffer::new(
      1,
      MAX_SAMPLE_RATE_HZ + 1,
      None,
      AudioSamples::InterleavedI16(&empty),
    );
    assert!(matches!(
      convert_to_f32_interleaved(&buffer),
      Err(AudioError::InvalidSpec { .. })
    ));
  }

  #[test]
  fn sanitize_sample_nan_and_inf_become_zero() {
    assert_eq!(sanitize_sample(f32::NAN), 0.0);
    assert_eq!(sanitize_sample(f32::INFINITY), 0.0);
    assert_eq!(sanitize_sample(f32::NEG_INFINITY), 0.0);
  }

  #[test]
  fn sanitize_sample_subnormals_become_zero() {
    // Smallest positive/negative subnormal numbers.
    let sub_pos = f32::from_bits(1);
    let sub_neg = f32::from_bits(1) * -1.0;
    assert!(!sub_pos.is_normal());
    assert!(!sub_neg.is_normal());
    assert_eq!(sanitize_sample(sub_pos), 0.0);
    assert_eq!(sanitize_sample(sub_neg), 0.0);
  }

  #[test]
  fn sanitize_sample_clamps_large_magnitudes() {
    assert_eq!(sanitize_sample(10.0), 1.0);
    assert_eq!(sanitize_sample(-10.0), -1.0);
    assert_eq!(sanitize_sample(0.5), 0.5);
    assert_eq!(sanitize_sample(-0.5), -0.5);
  }

  #[test]
  fn remix_mono_to_stereo_duplicates_samples() {
    // 2 frames mono: [a, b]
    let input = vec![0.25f32, -0.5f32];
    let out = remix_channels_f32(&input, 1, 2);
    assert_eq!(out, vec![0.25, 0.25, -0.5, -0.5]);
  }

  #[test]
  fn remix_stereo_to_mono_averages_samples() {
    // 2 frames stereo: [(L0,R0), (L1,R1)]
    let input = vec![0.0f32, 1.0f32, 1.0f32, 0.0f32];
    let out = remix_channels_f32(&input, 2, 1);
    assert_eq!(out, vec![0.5, 0.5]);
  }

  #[test]
  fn resampler_duration_is_preserved_within_one_frame_for_sine() {
    let in_rate = 44_100u32;
    let out_rate = 48_000u32;
    let channels = 1usize;

    // 100ms of a 440Hz sine wave.
    let duration_s = 0.1f32;
    let in_frames = (in_rate as f32 * duration_s) as usize;

    let mut input = Vec::with_capacity(in_frames * channels);
    let freq = 440.0f32;
    for n in 0..in_frames {
      let t = n as f32 / in_rate as f32;
      input.push((2.0 * core::f32::consts::PI * freq * t).sin());
    }

    let out = resample_f32(&input, in_rate, out_rate, channels);

    let expected_out_frames_u64 =
      ((in_frames as u64) * (out_rate as u64) + (in_rate as u64) / 2) / (in_rate as u64);
    let expected_out_frames = expected_out_frames_u64.max(1) as usize;

    let out_frames = out.len() / channels;
    assert!(
      out_frames.abs_diff(expected_out_frames) <= 1,
      "out_frames={out_frames} expected≈{expected_out_frames}"
    );
  }

  #[test]
  fn resampler_edge_cases_do_not_panic() {
    // Empty input.
    assert!(resample_f32(&[], 44_100, 48_000, 1).is_empty());
    // Zero channels.
    assert!(resample_f32(&[0.0, 1.0], 44_100, 48_000, 0).is_empty());
    // Zero rates.
    assert!(resample_f32(&[0.0, 1.0], 0, 48_000, 1).is_empty());
    assert!(resample_f32(&[0.0, 1.0], 44_100, 0, 1).is_empty());
    // Non-multiple length should not panic; trailing samples are ignored.
    let out = resample_f32(&[0.0, 1.0, 2.0], 44_100, 48_000, 2);
    assert!(out.len() % 2 == 0);
  }
}

/// Conservative cap for buffered interleaved samples inside [`LinearResampler`].
///
/// Resamplers are often fed attacker-controlled media streams; keep internal buffering bounded so
/// fuzzing and real-world playback cannot accidentally allocate unbounded memory.
const MAX_BUFFERED_SAMPLES: usize = 1_000_000; // ~4MB of f32 PCM.

/// Resample interleaved `f32` PCM using nearest-neighbour sampling.
///
/// - `channels` must match the interleaving in `input`.
/// - `max_output_frames` caps the returned length to avoid huge allocations when upsampling.
#[must_use]
pub fn resample_nearest_interleaved_f32(
  input: &[f32],
  channels: usize,
  in_rate_hz: u32,
  out_rate_hz: u32,
  max_output_frames: usize,
) -> Vec<f32> {
  if channels == 0 || in_rate_hz == 0 || out_rate_hz == 0 || max_output_frames == 0 {
    return Vec::new();
  }

  let usable_len = input.len() - (input.len() % channels);
  let input = &input[..usable_len];
  let input_frames = input.len() / channels;
  if input_frames == 0 {
    return Vec::new();
  }

  let output_frames =
    estimate_output_frames(input_frames, in_rate_hz, out_rate_hz, max_output_frames);
  if output_frames == 0 {
    return Vec::new();
  }

  let mut out = Vec::with_capacity(output_frames.saturating_mul(channels));

  for out_frame in 0..output_frames {
    let idx = map_output_frame_to_input_index(out_frame, in_rate_hz, out_rate_hz);
    let idx = idx.min(input_frames - 1);
    let base = idx.saturating_mul(channels);
    out.extend_from_slice(&input[base..base + channels]);
  }

  out
}

/// Resample interleaved `f32` PCM using linear interpolation.
///
/// This is intended for media playback (not high-quality offline resampling). The implementation
/// prioritizes panic-freedom, bounded allocations, and reasonable numerical behaviour.
#[must_use]
pub fn resample_linear_interleaved_f32(
  input: &[f32],
  channels: usize,
  in_rate_hz: u32,
  out_rate_hz: u32,
  max_output_frames: usize,
) -> Vec<f32> {
  if channels == 0 || in_rate_hz == 0 || out_rate_hz == 0 || max_output_frames == 0 {
    return Vec::new();
  }

  let usable_len = input.len() - (input.len() % channels);
  let input = &input[..usable_len];
  let input_frames = input.len() / channels;
  if input_frames == 0 {
    return Vec::new();
  }

  let output_frames =
    estimate_output_frames(input_frames, in_rate_hz, out_rate_hz, max_output_frames);
  if output_frames == 0 {
    return Vec::new();
  }

  let step = (in_rate_hz as f64) / (out_rate_hz as f64);
  if !(step.is_finite()) || step <= 0.0 {
    return Vec::new();
  }

  let mut out = Vec::with_capacity(output_frames.saturating_mul(channels));

  for out_frame in 0..output_frames {
    let pos = (out_frame as f64) * step;
    if !(pos.is_finite()) {
      break;
    }
    let base = pos.floor();
    if !(base.is_finite()) || base < 0.0 {
      continue;
    }
    let base_idx = base as usize;
    if base_idx >= input_frames {
      break;
    }
    let frac = (pos - base) as f32;
    let frac = if frac.is_finite() {
      frac.clamp(0.0, 1.0)
    } else {
      0.0
    };

    let next_idx = (base_idx + 1).min(input_frames - 1);
    let base_sample = base_idx.saturating_mul(channels);
    let next_sample = next_idx.saturating_mul(channels);

    for ch in 0..channels {
      let a = input[base_sample + ch];
      let b = input[next_sample + ch];
      out.push(lerp_f32(a, b, frac));
    }
  }

  out
}

/// A small stateful linear resampler for interleaved `f32` PCM.
///
/// Callers can stream input in arbitrary chunk sizes; [`process`] maintains fractional position and
/// buffers as needed. Output is capped per call to avoid unbounded work on pathological ratios.
#[derive(Debug, Clone)]
pub struct LinearResampler {
  in_rate_hz: u32,
  out_rate_hz: u32,
  channels: usize,
  step: f64,
  pos: f64,
  buf: VecDeque<f32>,
}

impl LinearResampler {
  #[must_use]
  pub fn new(in_rate_hz: u32, out_rate_hz: u32, channels: usize) -> Self {
    let step = if in_rate_hz == 0 || out_rate_hz == 0 {
      0.0
    } else {
      (in_rate_hz as f64) / (out_rate_hz as f64)
    };
    let step = if step.is_finite() && step > 0.0 {
      step
    } else {
      0.0
    };
    Self {
      in_rate_hz,
      out_rate_hz,
      channels,
      step,
      pos: 0.0,
      buf: VecDeque::new(),
    }
  }

  #[must_use]
  pub fn in_rate_hz(&self) -> u32 {
    self.in_rate_hz
  }

  #[must_use]
  pub fn out_rate_hz(&self) -> u32 {
    self.out_rate_hz
  }

  #[must_use]
  pub fn channels(&self) -> usize {
    self.channels
  }

  pub fn reset(&mut self) {
    self.pos = 0.0;
    self.buf.clear();
  }

  /// Push additional interleaved samples into the internal buffer.
  pub fn push_interleaved_f32(&mut self, input: &[f32]) {
    if self.channels == 0 || input.is_empty() {
      return;
    }

    let usable_len = input.len() - (input.len() % self.channels);
    if usable_len == 0 {
      return;
    }

    let remaining_capacity = MAX_BUFFERED_SAMPLES.saturating_sub(self.buf.len());
    if remaining_capacity == 0 {
      return;
    }

    let mut to_push = usable_len.min(remaining_capacity);
    to_push -= to_push % self.channels;
    if to_push == 0 {
      return;
    }

    self.buf.extend(input[..to_push].iter().copied());
  }

  /// Push `input` and return up to `max_output_frames` resampled frames.
  #[must_use]
  pub fn process(&mut self, input: &[f32], max_output_frames: usize) -> Vec<f32> {
    self.push_interleaved_f32(input);
    self.render(max_output_frames)
  }

  /// Drain resampled output from the internal buffer.
  #[must_use]
  pub fn render(&mut self, max_output_frames: usize) -> Vec<f32> {
    let mut out = Vec::new();
    self.render_into(&mut out, max_output_frames);
    out
  }

  pub fn render_into(&mut self, out: &mut Vec<f32>, max_output_frames: usize) {
    if max_output_frames == 0 || self.channels == 0 || self.step == 0.0 {
      return;
    }

    let channels = self.channels;
    let mut frames_written = 0usize;

    while frames_written < max_output_frames {
      let available_frames = self.buf.len() / channels;
      if available_frames == 0 {
        break;
      }

      // Sample at `pos` in input-frame units.
      let base = self.pos.floor();
      if !(base.is_finite()) || base < 0.0 {
        self.pos = 0.0;
        break;
      }
      let base_idx = base as usize;
      if base_idx >= available_frames {
        // We've advanced past the buffered input. Keep a single frame of history.
        self.drop_frames(available_frames.saturating_sub(1));
        self.pos = 0.0;
        break;
      }
      let frac = (self.pos - base) as f32;
      let frac = if frac.is_finite() {
        frac.clamp(0.0, 1.0)
      } else {
        0.0
      };

      let next_idx = (base_idx + 1).min(available_frames - 1);

      for ch in 0..channels {
        let a = self.sample_at(base_idx, ch);
        let b = self.sample_at(next_idx, ch);
        out.push(lerp_f32(a, b, frac));
      }

      frames_written += 1;
      self.pos += self.step;

      if !(self.pos.is_finite()) || self.pos < 0.0 {
        self.pos = 0.0;
        break;
      }

      // Drop any whole frames that are no longer needed.
      let available_frames = self.buf.len() / channels;
      if available_frames == 0 {
        self.pos = 0.0;
        break;
      }

      let drop_frames_raw = self.pos.floor() as usize;
      let drop_frames = drop_frames_raw.min(available_frames.saturating_sub(1));
      if drop_frames > 0 {
        self.drop_frames(drop_frames);
        self.pos -= drop_frames as f64;
        if self.pos < 0.0 || !(self.pos.is_finite()) {
          self.pos = 0.0;
        }
      }

      // If a pathological step advanced us past the remaining buffer, clamp back.
      let available_frames = self.buf.len() / channels;
      if available_frames == 0 || self.pos >= available_frames as f64 {
        self.pos = 0.0;
      }
    }
  }

  fn sample_at(&self, frame: usize, channel: usize) -> f32 {
    let idx = frame.saturating_mul(self.channels).saturating_add(channel);
    self.buf.get(idx).copied().unwrap_or(0.0)
  }

  fn drop_frames(&mut self, frames: usize) {
    if frames == 0 || self.channels == 0 {
      return;
    }
    let samples = frames.saturating_mul(self.channels).min(self.buf.len());
    for _ in 0..samples {
      let _ = self.buf.pop_front();
    }
  }
}

fn estimate_output_frames(
  input_frames: usize,
  in_rate_hz: u32,
  out_rate_hz: u32,
  max_output_frames: usize,
) -> usize {
  if input_frames == 0 || in_rate_hz == 0 || out_rate_hz == 0 || max_output_frames == 0 {
    return 0;
  }

  let in_rate = in_rate_hz as u128;
  let out_rate = out_rate_hz as u128;
  let input_frames = input_frames as u128;

  let estimated = input_frames
    .saturating_mul(out_rate)
    .saturating_add(in_rate.saturating_sub(1))
    .checked_div(in_rate)
    .unwrap_or(u128::MAX);

  let estimated = usize::try_from(estimated).unwrap_or(usize::MAX);
  estimated.min(max_output_frames)
}

fn map_output_frame_to_input_index(out_frame: usize, in_rate_hz: u32, out_rate_hz: u32) -> usize {
  if in_rate_hz == 0 || out_rate_hz == 0 {
    return 0;
  }
  let n = (out_frame as u128).saturating_mul(in_rate_hz as u128);
  let d = out_rate_hz as u128;
  let idx = n / d;
  usize::try_from(idx).unwrap_or(usize::MAX)
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
  // Allow NaNs to propagate; callers may sanitize at the sink boundary.
  a + (b - a) * t
}
