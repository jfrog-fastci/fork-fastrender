use super::types::{AudioBuffer, AudioSamples};
use super::AudioError;

fn i16_to_f32(sample: i16) -> f32 {
  sample as f32 / 32768.0
}

fn u16_to_f32(sample: u16) -> f32 {
  (sample as f32 - 32768.0) / 32768.0
}

pub fn convert_to_f32_interleaved(buffer: &AudioBuffer<'_>) -> Result<Vec<f32>, AudioError> {
  if buffer.channels == 0 {
    return Err(AudioError::InvalidChannels {
      channels: buffer.channels,
    });
  }
  if buffer.sample_rate == 0 {
    return Err(AudioError::InvalidSampleRate {
      sample_rate: buffer.sample_rate,
    });
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
      Ok(samples.to_vec())
    }
    AudioSamples::InterleavedI16(samples) => {
      validate_interleaved_len(samples.len(), buffer.channels)?;
      Ok(samples.iter().copied().map(i16_to_f32).collect())
    }
    AudioSamples::InterleavedU16(samples) => {
      validate_interleaved_len(samples.len(), buffer.channels)?;
      Ok(samples.iter().copied().map(u16_to_f32).collect())
    }
    AudioSamples::PlanarF32(planes) => planar_to_f32_interleaved(planes, buffer.channels, |s| s),
    AudioSamples::PlanarI16(planes) => {
      planar_to_f32_interleaved(planes, buffer.channels, i16_to_f32)
    }
    AudioSamples::PlanarU16(planes) => {
      planar_to_f32_interleaved(planes, buffer.channels, u16_to_f32)
    }
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

  let frames = planes
    .first()
    .map_or(0, |first_plane| first_plane.len());

  for (i, plane) in planes.iter().enumerate() {
    if plane.len() != frames {
      return Err(AudioError::InvalidPlaneLength {
        plane: i,
        len_samples: plane.len(),
        expected_samples: frames,
      });
    }
  }

  let mut out = Vec::with_capacity(frames * channels);
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

    let buffer = AudioBuffer::new(
      2,
      48_000,
      None,
      AudioSamples::PlanarI16(&planes),
    );

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
}
