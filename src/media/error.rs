use thiserror::Error;

/// Errors produced by media clock/timeline mapping code.
///
/// This type is currently used by early media plumbing and is intended to stay stable so callers
/// can branch on clock failures without relying on string matching.
#[derive(Error, Debug)]
pub enum MediaClockError {
  #[error("invalid playback rate: {playback_rate}")]
  InvalidPlaybackRate { playback_rate: f64 },

  #[error("invalid media timebase: denominator must be non-zero")]
  InvalidTimebaseDenominator,

  #[error("invalid media clock sample rate: {sample_rate_hz}Hz (must be non-zero)")]
  InvalidSampleRate { sample_rate_hz: u32 },
}

impl From<MediaClockError> for crate::error::Error {
  fn from(err: MediaClockError) -> Self {
    crate::error::Error::Other(format!("media clock error: {err}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn media_clock_error_display_is_informative() {
    let err = MediaClockError::InvalidPlaybackRate {
      playback_rate: f64::NAN,
    };
    let msg = err.to_string();
    assert!(msg.contains("playback rate"));
  }
}

