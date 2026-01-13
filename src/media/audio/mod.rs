use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;

#[cfg(feature = "audio_cpal")]
mod cpal_backend;
mod null_backend;
#[cfg(feature = "audio_cpal")]
mod ring_buffer;

#[cfg(feature = "audio_cpal")]
pub use cpal_backend::CpalAudioBackend;
pub use null_backend::NullAudioBackend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioStreamConfig {
  pub sample_rate_hz: u32,
  pub channels: u16,
}

impl AudioStreamConfig {
  pub const fn new(sample_rate_hz: u32, channels: u16) -> Self {
    Self {
      sample_rate_hz,
      channels,
    }
  }
}

#[derive(Clone, Debug)]
pub enum AudioClock {
  /// Clock derived from the number of frames the output backend reports as delivered.
  OutputFrames {
    frames_played: Arc<AtomicU64>,
    sample_rate_hz: u32,
  },
  /// Clock derived from wall time (used by `NullAudioBackend`).
  Instant {
    start: Instant,
    sample_rate_hz: u32,
  },
}

impl AudioClock {
  #[must_use]
  pub fn sample_rate_hz(&self) -> u32 {
    match self {
      Self::OutputFrames { sample_rate_hz, .. } | Self::Instant { sample_rate_hz, .. } => {
        *sample_rate_hz
      }
    }
  }

  #[must_use]
  pub fn frames(&self) -> u64 {
    match self {
      Self::OutputFrames { frames_played, .. } => frames_played.load(Ordering::Relaxed),
      Self::Instant {
        start,
        sample_rate_hz,
      } => {
        let seconds = start.elapsed().as_secs_f64();
        (seconds * f64::from(*sample_rate_hz)) as u64
      }
    }
  }

  #[must_use]
  pub fn time(&self) -> Duration {
    match self {
      Self::OutputFrames {
        frames_played,
        sample_rate_hz,
      } => {
        let frames = frames_played.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / f64::from(*sample_rate_hz))
      }
      Self::Instant { start, .. } => start.elapsed(),
    }
  }
}

#[derive(Debug, Error)]
pub enum AudioError {
  #[error("no default output audio device is available")]
  NoOutputDevice,
  #[error("failed to enumerate output audio configs: {0}")]
  OutputConfigEnumerationFailed(String),
  #[error("failed to load default output audio config: {0}")]
  DefaultOutputConfigFailed(String),
  #[error("failed to build audio output stream: {0}")]
  StreamBuildFailed(String),
  #[error("unsupported output audio sample format: {0}")]
  UnsupportedSampleFormat(String),
  #[error("failed to start output audio stream: {0}")]
  StreamPlayFailed(String),
}

pub trait AudioSink: Send + Sync {
  fn config(&self) -> AudioStreamConfig;

  /// Queue interleaved f32 PCM samples for playback.
  ///
  /// Samples must be at the sink/backend output sample rate and channel count.
  /// Returns the number of samples accepted (the remainder, if any, was dropped).
  fn push_interleaved_f32(&self, samples: &[f32]) -> usize;

  fn set_volume(&self, volume: f32);
}

pub trait AudioBackend: Send + Sync {
  fn output_config(&self) -> AudioStreamConfig;

  fn clock(&self) -> AudioClock;

  fn create_sink(&self) -> Box<dyn AudioSink>;
}

impl dyn AudioBackend {
  /// Construct an audio backend suitable for interactive browsing sessions.
  ///
  /// This prefers the CPAL output backend when available and falls back to a null backend
  /// (silence) when audio devices are unavailable. The fallback path is intended to keep
  /// headless/CI runs stable.
  #[must_use]
  pub fn new_best_effort() -> Box<dyn AudioBackend> {
    #[cfg(feature = "audio_cpal")]
    {
      use std::sync::Once;
      static WARN_ONCE: Once = Once::new();

      match CpalAudioBackend::new() {
        Ok(backend) => return Box::new(backend),
        Err(err) => {
          WARN_ONCE.call_once(|| {
            eprintln!(
              "warning: failed to initialize CPAL audio backend ({err}); falling back to NullAudioBackend"
            );
          });
        }
      }
    }

    Box::new(NullAudioBackend::new())
  }
}

