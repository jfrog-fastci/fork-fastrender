//! Audio output backends and audio clock exposure.
//!
//! The audio backend is responsible for two things:
//!
//! * providing an [`AudioSink`] to accept interleaved PCM samples for playback, and
//! * exposing an [`AudioClock`] so the rest of the media pipeline can sync video and
//!   `HTMLMediaElement.currentTime` to what the user hears.
//!
//! When audio is present, audio device time is the **master clock** for A/V sync. The UI tick should
//! only wake the pipeline up; it must not be used as a time source.
//!
//! Output latency is exposed via [`AudioOutputInfo::estimated_latency`]. Backends that derive time
//! from callback frame counts (`AudioClock::OutputFrames`) can be ahead of “what the user hears” by
//! a roughly-constant buffer duration; callers should treat this as a constant offset (not drift)
//! and compensate using the estimated latency.
//!
//! See `docs/media_clocking.md` for the broader clocking model and recommended sync tolerances.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;

#[cfg(feature = "audio_cpal")]
mod cpal_backend;
mod latency;
mod null_backend;
#[cfg(feature = "audio_cpal")]
mod ring_buffer;
#[cfg(feature = "audio_cpal")]
mod thread_priority;
pub mod mixer;
pub mod queue;
pub mod timed_queue;

#[cfg(feature = "audio_cpal")]
pub use cpal_backend::CpalAudioBackend;
pub use latency::{
  duration_to_frames_ceil, duration_to_frames_floor, frames_to_duration, latency_from_timestamps,
};
pub use null_backend::NullAudioBackend;
pub use queue::{pcm_f32_queue, PcmF32QueueConsumer, PcmF32QueueProducer};
pub use timed_queue::{PushError, ReadResult, TimedAudioQueue, TimedAudioSegment};

pub use mixer::{AudioMixer, AudioStreamId, AudioStreamParams};

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

/// Information about the active audio output device/stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioOutputInfo {
  pub sample_rate_hz: u32,
  pub channels: u16,
  /// The number of frames the backend expects per callback, when known.
  pub callback_frames: Option<u32>,
  /// Best-effort estimate of the latency between writing samples in the callback and the samples
  /// being heard at the output device.
  pub estimated_latency: Duration,
}

impl AudioOutputInfo {
  /// Returns the estimated latency expressed in frames, rounding up.
  #[must_use]
  pub fn estimated_latency_frames(&self) -> u64 {
    duration_to_frames_ceil(self.sample_rate_hz, self.estimated_latency)
  }

  #[must_use]
  pub fn stream_config(&self) -> AudioStreamConfig {
    AudioStreamConfig::new(self.sample_rate_hz, self.channels)
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
  Instant { start: Instant, sample_rate_hz: u32 },
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
      } => duration_to_frames_floor(*sample_rate_hz, start.elapsed()),
    }
  }

  #[must_use]
  /// Return the audio backend's current time estimate.
  ///
  /// This is intended to be used as (or to derive) the master clock for A/V sync.
  ///
  /// Note: this is currently a best-effort estimate and does **not** apply an output latency model
  /// by itself. Callers should subtract [`AudioOutputInfo::estimated_latency`] when they need a
  /// "time heard" estimate.
  pub fn time(&self) -> Duration {
    match self {
      Self::OutputFrames {
        frames_played,
        sample_rate_hz,
      } => {
        let frames = frames_played.load(Ordering::Relaxed);
        frames_to_duration(*sample_rate_hz, frames)
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

  /// Returns information about the active output stream, including the estimated output latency.
  ///
  /// Backends should provide best-effort values even when the underlying API does not expose
  /// explicit latency information.
  fn output_info(&self) -> AudioOutputInfo {
    let cfg = self.output_config();
    AudioOutputInfo {
      sample_rate_hz: cfg.sample_rate_hz,
      channels: cfg.channels,
      callback_frames: None,
      estimated_latency: Duration::ZERO,
    }
  }

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
