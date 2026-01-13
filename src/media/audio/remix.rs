//! Channel remixing helpers for interleaved f32 PCM.
//!
//! # Policy
//!
//! This module provides a deterministic, allocation-free helper for converting interleaved f32
//! sample buffers between channel counts.
//!
//! The intent is to let decoders produce mono or multi-channel audio while still being able to
//! submit audio to an output sink that has a fixed channel count.
//!
//! Mixing/remixing rules implemented by [`remix_interleaved_f32`]:
//!
//! - **mono → stereo** (`1 → 2`): duplicate the mono sample into L/R.
//! - **stereo → mono** (`2 → 1`): average: `0.5 * (L + R)`.
//! - **N → stereo** (`N → 2`, `N > 2`): downmix by averaging all input channels equally and write
//!   the same value into both L/R.
//! - **stereo → N** (`2 → N`, `N > 2`): replicate stereo into the first two channels and write
//!   `0.0` into remaining channels.
//! - **mono → N** (`1 → N`, `N > 2`): equivalent to `mono → stereo → N` (first two channels are the
//!   mono sample; remaining channels are `0.0`).
//! - **N → mono** (`N → 1`, `N > 2`): downmix by averaging all input channels equally.
//! - **other N ↔ M** (`N > 2`, `M > 2`, `N != M`): copy the first `min(N, M)` channels and set any
//!   remaining output channels to `0.0`. Extra input channels are dropped.
//!
//! ## Finite math
//!
//! Input samples that are `NaN` or infinite are treated as `0.0`. Any non-finite intermediate
//! results are also written as `0.0`.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RemixError {
  #[error("in_channels must be greater than 0")]
  ZeroInputChannels,
  #[error("out_channels must be greater than 0")]
  ZeroOutputChannels,
  #[error("input buffer length {len} is not a multiple of in_channels {channels}")]
  InputLenNotMultiple { len: usize, channels: usize },
  #[error("output buffer length {len} is not a multiple of out_channels {channels}")]
  OutputLenNotMultiple { len: usize, channels: usize },
  #[error("frame count mismatch (input has {in_frames} frames, output has {out_frames} frames)")]
  FrameCountMismatch { in_frames: usize, out_frames: usize },
}

#[inline]
fn sanitize_f32(value: f32) -> f32 {
  if value.is_finite() { value } else { 0.0 }
}

/// Remix interleaved f32 PCM from `in_channels` to `out_channels`.
///
/// `input` and `out` must represent the same number of frames.
pub fn remix_interleaved_f32(
  input: &[f32],
  in_channels: u16,
  out: &mut [f32],
  out_channels: u16,
) -> Result<(), RemixError> {
  let in_channels = usize::from(in_channels);
  let out_channels = usize::from(out_channels);

  if in_channels == 0 {
    return Err(RemixError::ZeroInputChannels);
  }
  if out_channels == 0 {
    return Err(RemixError::ZeroOutputChannels);
  }

  if input.len() % in_channels != 0 {
    return Err(RemixError::InputLenNotMultiple {
      len: input.len(),
      channels: in_channels,
    });
  }
  if out.len() % out_channels != 0 {
    return Err(RemixError::OutputLenNotMultiple {
      len: out.len(),
      channels: out_channels,
    });
  }

  let in_frames = input.len() / in_channels;
  let out_frames = out.len() / out_channels;
  if in_frames != out_frames {
    return Err(RemixError::FrameCountMismatch {
      in_frames,
      out_frames,
    });
  }

  // Fast paths for common cases.
  match (in_channels, out_channels) {
    (ic, oc) if ic == oc => {
      // Same channel count: copy + sanitize.
      for (dst, src) in out.iter_mut().zip(input.iter()) {
        *dst = sanitize_f32(*src);
      }
      return Ok(());
    }
    (1, 2) => {
      // mono -> stereo
      for (frame_idx, sample) in input.iter().enumerate() {
        let sample = sanitize_f32(*sample);
        let out_idx = frame_idx * 2;
        out[out_idx] = sample;
        out[out_idx + 1] = sample;
      }
      return Ok(());
    }
    (2, 1) => {
      // stereo -> mono
      for frame in 0..in_frames {
        let in_idx = frame * 2;
        let l = sanitize_f32(input[in_idx]);
        let r = sanitize_f32(input[in_idx + 1]);
        out[frame] = sanitize_f32(0.5 * (l + r));
      }
      return Ok(());
    }
    (2, oc) if oc > 2 => {
      // stereo -> N: replicate first two channels, zero the rest.
      for frame in 0..in_frames {
        let in_idx = frame * 2;
        let out_idx = frame * oc;
        out[out_idx] = sanitize_f32(input[in_idx]);
        out[out_idx + 1] = sanitize_f32(input[in_idx + 1]);
        out[out_idx + 2..out_idx + oc].fill(0.0);
      }
      return Ok(());
    }
    (1, oc) if oc > 2 => {
      // mono -> N: equivalent to mono -> stereo -> N.
      for frame in 0..in_frames {
        let sample = sanitize_f32(input[frame]);
        let out_idx = frame * oc;
        out[out_idx] = sample;
        out[out_idx + 1] = sample;
        out[out_idx + 2..out_idx + oc].fill(0.0);
      }
      return Ok(());
    }
    (ic, 2) if ic > 2 => {
      // N -> stereo: average all channels and duplicate into L/R.
      let inv = 1.0 / ic as f32;
      for frame in 0..in_frames {
        let in_idx = frame * ic;
        let mut sum = 0.0f32;
        for ch in 0..ic {
          sum += sanitize_f32(input[in_idx + ch]);
        }
        let mixed = sanitize_f32(sum * inv);
        let out_idx = frame * 2;
        out[out_idx] = mixed;
        out[out_idx + 1] = mixed;
      }
      return Ok(());
    }
    (ic, 1) if ic > 1 => {
      // N -> mono: average all channels.
      let inv = 1.0 / ic as f32;
      for frame in 0..in_frames {
        let in_idx = frame * ic;
        let mut sum = 0.0f32;
        for ch in 0..ic {
          sum += sanitize_f32(input[in_idx + ch]);
        }
        out[frame] = sanitize_f32(sum * inv);
      }
      return Ok(());
    }
    _ => {}
  }

  // Fallback: copy the first min(in_channels, out_channels) channels; zero-fill the rest.
  // Note that this path only runs for the uncommon `N>2 ↔ M>2, N!=M` conversions. Conversions to
  // mono or stereo have explicit policies above.
  let min_channels = in_channels.min(out_channels);
  for frame in 0..in_frames {
    let in_idx = frame * in_channels;
    let out_idx = frame * out_channels;
    for ch in 0..min_channels {
      out[out_idx + ch] = sanitize_f32(input[in_idx + ch]);
    }
    if out_channels > min_channels {
      out[out_idx + min_channels..out_idx + out_channels].fill(0.0);
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{remix_interleaved_f32, RemixError};

  #[test]
  fn audio_remix_mono_to_stereo_duplicates() {
    let input = [0.0f32, 1.0, -1.0];
    let mut out = [0.0f32; 6];
    remix_interleaved_f32(&input, 1, &mut out, 2).unwrap();
    assert_eq!(out, [0.0, 0.0, 1.0, 1.0, -1.0, -1.0]);
  }

  #[test]
  fn audio_remix_stereo_to_mono_averages() {
    let input = [1.0f32, -1.0, 0.5, 0.25];
    let mut out = [0.0f32; 2];
    remix_interleaved_f32(&input, 2, &mut out, 1).unwrap();
    assert_eq!(out[0], 0.0);
    assert!((out[1] - 0.375).abs() < 1e-6);
  }

  #[test]
  fn audio_remix_n_to_stereo_downmixes_by_averaging() {
    // 3 channels, 2 frames: [1,2,3] and [4,5,6].
    let input = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
    let mut out = [0.0f32; 4];
    remix_interleaved_f32(&input, 3, &mut out, 2).unwrap();
    assert_eq!(out, [2.0, 2.0, 5.0, 5.0]);
  }

  #[test]
  fn audio_remix_stereo_to_n_replica_and_zero_fill() {
    let input = [1.0f32, 2.0, 3.0, 4.0];
    let mut out = [9.0f32; 8];
    remix_interleaved_f32(&input, 2, &mut out, 4).unwrap();
    assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 3.0, 4.0, 0.0, 0.0]);
  }

  #[test]
  fn audio_remix_sanitizes_non_finite_values() {
    let input = [f32::NAN, f32::INFINITY];
    let mut out = [1.0f32; 4];
    remix_interleaved_f32(&input, 1, &mut out, 2).unwrap();
    assert_eq!(out, [0.0, 0.0, 0.0, 0.0]);
  }

  #[test]
  fn audio_remix_validates_buffer_sizes() {
    let input = [0.0f32; 3];
    let mut out = [0.0f32; 4];

    assert_eq!(
      remix_interleaved_f32(&input, 2, &mut out, 2).unwrap_err(),
      RemixError::InputLenNotMultiple { len: 3, channels: 2 }
    );

    let input = [0.0f32; 4];
    assert_eq!(
      remix_interleaved_f32(&input, 2, &mut out, 3).unwrap_err(),
      RemixError::OutputLenNotMultiple { len: 4, channels: 3 }
    );

    let mut out = [0.0f32; 2];
    assert_eq!(
      remix_interleaved_f32(&input, 2, &mut out, 2).unwrap_err(),
      RemixError::FrameCountMismatch {
        in_frames: 2,
        out_frames: 1,
      }
    );
  }
}
