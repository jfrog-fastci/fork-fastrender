//! Browser-side validation for renderer → browser IPC.
//!
//! Renderers are treated as untrusted: a compromised/buggy renderer must not be able to cause the
//! browser to panic or allocate unbounded memory by sending malformed `FrameReady` messages.
//!
//! This module focuses on validating RGBA8 frame buffers (width/height/byte length) against:
//! - hard caps (`max_dim_px`, `max_bytes`),
//! - the expected pixel dimensions derived from the renderer-reported viewport metadata
//!   (`viewport_css`, `dpr`), and
//! - optional expected dimensions from the last browser→renderer `Resize` command (for multiprocess
//!   architectures).

use super::browser_limits::BrowserLimits;
use super::messages::RenderedFrame;

/// Bytes per pixel for RGBA8 frames.
pub const BYTES_PER_PIXEL: u64 = 4;

/// Default tolerance (in pixels) when comparing expected vs received frame dimensions.
///
/// Some pipelines apply rounding when converting CSS pixels + DPR into device pixels. Allow a tiny
/// tolerance so we can accept off-by-one differences if the renderer/browser disagree about the
/// rounding boundary.
pub const DEFAULT_DIM_TOLERANCE_PX: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameReadyLimits {
  pub max_dim_px: u32,
  pub max_pixels: u64,
  pub max_bytes: u64,
  pub dim_tolerance_px: u32,
}

impl FrameReadyLimits {
  pub fn from_browser_limits(limits: &BrowserLimits) -> Self {
    Self {
      max_dim_px: limits.max_dim_px,
      max_pixels: limits.max_pixels,
      max_bytes: limits.max_pixels.saturating_mul(BYTES_PER_PIXEL),
      dim_tolerance_px: DEFAULT_DIM_TOLERANCE_PX,
    }
  }

  /// Create limits based on the browser UI's configured viewport limits.
  ///
  /// This reads `FASTR_BROWSER_MAX_*` environment variables via [`BrowserLimits::from_env`].
  pub fn from_env() -> Self {
    Self::from_browser_limits(&BrowserLimits::from_env())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameReadyViolation {
  InvalidDimensions {
    width: u32,
    height: u32,
  },
  DimensionsTooLarge {
    width: u32,
    height: u32,
    max_dim_px: u32,
  },
  TooManyPixels {
    width: u32,
    height: u32,
    pixels: u64,
    max_pixels: u64,
  },
  TooManyBytes {
    bytes: u64,
    max_bytes: u64,
  },
  ByteLengthOverflow,
  BufferLengthMismatch {
    expected: u64,
    got: u64,
  },
  InvalidViewportCss {
    viewport_css: (u32, u32),
  },
  InvalidDpr {
    dpr_bits: u32,
  },
  ExpectedDimensionsMismatch {
    expected: (u32, u32),
    got: (u32, u32),
    tolerance_px: u32,
  },
}

impl std::fmt::Display for FrameReadyViolation {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      FrameReadyViolation::InvalidDimensions { width, height } => {
        write!(f, "invalid frame dimensions {width}x{height}")
      }
      FrameReadyViolation::DimensionsTooLarge {
        width,
        height,
        max_dim_px,
      } => write!(
        f,
        "frame dimensions {width}x{height} exceed max_dim_px={max_dim_px}"
      ),
      FrameReadyViolation::TooManyPixels {
        width,
        height,
        pixels,
        max_pixels,
      } => write!(
        f,
        "frame dimensions {width}x{height} ({pixels} pixels) exceed max_pixels={max_pixels}"
      ),
      FrameReadyViolation::TooManyBytes { bytes, max_bytes } => {
        write!(
          f,
          "frame buffer {bytes} bytes exceeds max_bytes={max_bytes}"
        )
      }
      FrameReadyViolation::ByteLengthOverflow => write!(f, "frame byte length overflow"),
      FrameReadyViolation::BufferLengthMismatch { expected, got } => write!(
        f,
        "frame buffer length mismatch: expected {expected} bytes, got {got} bytes"
      ),
      FrameReadyViolation::InvalidViewportCss { viewport_css } => {
        write!(f, "invalid viewport_css={viewport_css:?}")
      }
      FrameReadyViolation::InvalidDpr { dpr_bits } => {
        let dpr = f32::from_bits(*dpr_bits);
        write!(f, "invalid device pixel ratio dpr={dpr}")
      }
      FrameReadyViolation::ExpectedDimensionsMismatch {
        expected,
        got,
        tolerance_px,
      } => write!(
        f,
        "frame dimensions mismatch: expected ~{expected:?} (±{tolerance_px}px), got {got:?}"
      ),
    }
  }
}

/// Compute the expected pixmap size (in device pixels) for a `(viewport_css, dpr)` pair.
///
/// Mirrors the rounding rules used by `BrowserLimits`/renderer (`round(viewport_css * dpr)`),
/// returning an error rather than silently accepting nonsensical metadata (NaN, 0 DPR, 0 viewport).
pub fn expected_pixmap_px_for_viewport(
  viewport_css: (u32, u32),
  dpr: f32,
) -> Result<(u32, u32), FrameReadyViolation> {
  if viewport_css.0 == 0 || viewport_css.1 == 0 {
    return Err(FrameReadyViolation::InvalidViewportCss { viewport_css });
  }
  if !dpr.is_finite() || dpr <= 0.0 {
    return Err(FrameReadyViolation::InvalidDpr {
      dpr_bits: dpr.to_bits(),
    });
  }

  let w = ((viewport_css.0 as f64) * (dpr as f64)).round();
  let h = ((viewport_css.1 as f64) * (dpr as f64)).round();
  let w = w.max(1.0).min(u32::MAX as f64) as u32;
  let h = h.max(1.0).min(u32::MAX as f64) as u32;
  Ok((w, h))
}

/// Validate an untrusted RGBA8 frame buffer.
///
/// Callers should invoke this before mapping shared memory / allocating a backing buffer based on
/// untrusted `(width, height)` metadata.
pub fn validate_rgba8888_frame(
  width: u32,
  height: u32,
  buffer_len: usize,
  expected_dims: Option<(u32, u32)>,
  limits: &FrameReadyLimits,
) -> Result<(), FrameReadyViolation> {
  if width == 0 || height == 0 {
    return Err(FrameReadyViolation::InvalidDimensions { width, height });
  }

  if width > limits.max_dim_px || height > limits.max_dim_px {
    return Err(FrameReadyViolation::DimensionsTooLarge {
      width,
      height,
      max_dim_px: limits.max_dim_px,
    });
  }

  let pixels = (width as u64)
    .checked_mul(height as u64)
    .ok_or(FrameReadyViolation::ByteLengthOverflow)?;

  if pixels > limits.max_pixels {
    return Err(FrameReadyViolation::TooManyPixels {
      width,
      height,
      pixels,
      max_pixels: limits.max_pixels,
    });
  }

  let bytes = pixels
    .checked_mul(BYTES_PER_PIXEL)
    .ok_or(FrameReadyViolation::ByteLengthOverflow)?;

  if bytes > limits.max_bytes {
    return Err(FrameReadyViolation::TooManyBytes {
      bytes,
      max_bytes: limits.max_bytes,
    });
  }

  let got_len = buffer_len as u64;
  if got_len != bytes {
    return Err(FrameReadyViolation::BufferLengthMismatch {
      expected: bytes,
      got: got_len,
    });
  }

  if let Some(expected) = expected_dims {
    let tolerance = limits.dim_tolerance_px;
    if width.abs_diff(expected.0) > tolerance || height.abs_diff(expected.1) > tolerance {
      return Err(FrameReadyViolation::ExpectedDimensionsMismatch {
        expected,
        got: (width, height),
        tolerance_px: tolerance,
      });
    }
  }

  Ok(())
}

/// Validate a [`RenderedFrame`] received from an untrusted renderer.
pub fn validate_rendered_frame_ready(
  frame: &RenderedFrame,
  limits: &FrameReadyLimits,
) -> Result<(), FrameReadyViolation> {
  let expected = expected_pixmap_px_for_viewport(frame.viewport_css, frame.dpr)?;
  validate_rgba8888_frame(
    frame.pixmap.width(),
    frame.pixmap.height(),
    frame.pixmap.data().len(),
    Some(expected),
    limits,
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validate_rgba8888_frame_rejects_buffer_len_mismatch() {
    let limits = FrameReadyLimits {
      max_dim_px: 1024,
      max_pixels: 1024 * 1024,
      max_bytes: 1024 * 1024 * 4,
      dim_tolerance_px: 0,
    };

    // 2x2 RGBA8 should be 16 bytes.
    let err = validate_rgba8888_frame(2, 2, 15, Some((2, 2)), &limits).unwrap_err();
    assert!(matches!(
      err,
      FrameReadyViolation::BufferLengthMismatch { .. }
    ));
  }

  #[test]
  fn validate_rgba8888_frame_rejects_too_many_bytes() {
    let limits = FrameReadyLimits {
      max_dim_px: 1024,
      max_pixels: 1024 * 1024,
      // Artificially low max_bytes so the byte cap triggers before the pixel cap.
      max_bytes: 100,
      dim_tolerance_px: 0,
    };

    // 10x3 RGBA8 is 120 bytes.
    let err = validate_rgba8888_frame(10, 3, 120, Some((10, 3)), &limits).unwrap_err();
    assert!(matches!(err, FrameReadyViolation::TooManyBytes { .. }));
  }

  #[test]
  fn expected_pixmap_px_for_viewport_rejects_invalid_dpr() {
    let err = expected_pixmap_px_for_viewport((10, 10), f32::NAN).unwrap_err();
    assert!(matches!(err, FrameReadyViolation::InvalidDpr { .. }));
  }
}
