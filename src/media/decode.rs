//! Decoder hardening helpers (video + audio).
//!
//! The demuxers produce compressed packets. Decoders can then output decoded video frames and audio
//! samples. These helpers ensure decoded output cannot trigger unbounded allocations in the renderer
//! (e.g. RGBA conversion buffers or audio sample vectors).

use super::{MediaError, MediaLimits, MediaResult};

/// Enforce decoded video frame limits.
///
/// Returns the required RGBA byte length (`width * height * 4`) when the frame is accepted.
pub fn validate_video_frame_dimensions(
  width: u32,
  height: u32,
  limits: &MediaLimits,
) -> MediaResult<usize> {
  if width == 0 || height == 0 {
    return Err(MediaError::decode(format!(
      "video frame has invalid dimensions: {width}x{height}"
    )));
  }

  // Apply both the configurable `MediaLimits` *and* the hard caps in `video_limits` to prevent
  // accidental OOM when callers relax config values.
  let hard_max = super::video_limits::MAX_VIDEO_DIMENSION;
  let (cfg_max_w, cfg_max_h) = limits.max_video_dimensions;
  let max_w = cfg_max_w.min(hard_max);
  let max_h = cfg_max_h.min(hard_max);
  if width > max_w || height > max_h {
    return Err(MediaError::resource_too_large(format!(
      "video frame dimensions {width}x{height} exceed max_video_dimensions {max_w}x{max_h}"
    )));
  }

  let rgba_bytes = (width as u64)
    .checked_mul(height as u64)
    .and_then(|px| px.checked_mul(4))
    .ok_or_else(|| MediaError::resource_too_large("video frame byte size overflow"))?;
  let rgba_usize = usize::try_from(rgba_bytes).map_err(|_| {
    MediaError::resource_too_large(format!(
      "video RGBA byte size {rgba_bytes} does not fit usize"
    ))
  })?;

  let max_rgba_bytes = limits
    .max_rgba_bytes
    .min(super::video_limits::MAX_VIDEO_FRAME_BYTES);
  if rgba_usize > max_rgba_bytes {
    return Err(MediaError::resource_too_large(format!(
      "video RGBA byte size {rgba_usize} exceeds max_rgba_bytes {max_rgba_bytes}"
    )));
  }

  Ok(rgba_usize)
}

/// Enforce decoded audio sample limits.
pub fn validate_audio_samples_per_packet(
  output_samples: usize,
  limits: &MediaLimits,
) -> MediaResult<()> {
  if output_samples > limits.max_audio_samples_per_packet {
    return Err(MediaError::resource_too_large(format!(
      "decoded audio samples {output_samples} exceed max_audio_samples_per_packet {}",
      limits.max_audio_samples_per_packet
    )));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rejects_large_video_frames() {
    let mut limits = MediaLimits::default();
    limits.max_video_dimensions = (100, 100);
    let err = validate_video_frame_dimensions(101, 1, &limits).unwrap_err();
    assert!(matches!(err, MediaError::ResourceTooLarge(_)));
  }

  #[test]
  fn rejects_large_rgba_allocations() {
    let mut limits = MediaLimits::default();
    limits.max_video_dimensions = (10_000, 10_000);
    limits.max_rgba_bytes = 100;
    let err = validate_video_frame_dimensions(10, 10, &limits).unwrap_err();
    assert!(matches!(err, MediaError::ResourceTooLarge(_)));
  }

  #[test]
  fn rejects_large_audio_buffers() {
    let mut limits = MediaLimits::default();
    limits.max_audio_samples_per_packet = 10;
    let err = validate_audio_samples_per_packet(11, &limits).unwrap_err();
    assert!(matches!(err, MediaError::ResourceTooLarge(_)));
  }
}
