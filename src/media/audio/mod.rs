//! Audio output backends and decoder-facing PCM ingestion.
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

mod config;
#[cfg(feature = "audio_cpal")]
mod cpal_backend;
mod latency;
mod null_backend;
#[cfg(feature = "audio_cpal")]
mod ring_buffer;
#[cfg(feature = "audio_cpal")]
mod thread_priority;
pub mod convert;
pub mod drift;
pub mod mixer;
pub mod queue;
pub mod timed_queue;
pub mod types;
#[cfg(feature = "audio_wav")]
mod wav;

pub use config::{
  audio_engine_config, set_audio_engine_config, with_audio_engine_config, AudioEngineConfig,
  AudioEngineConfigGuard,
};
#[cfg(feature = "audio_cpal")]
pub use cpal_backend::CpalAudioBackend;
pub use convert::convert_to_f32_interleaved;
pub use latency::{
  duration_to_frames_ceil, duration_to_frames_floor, frames_to_duration, latency_from_timestamps,
};
pub use drift::{DriftController, DriftControllerConfig};
pub use null_backend::NullAudioBackend;
pub use queue::{pcm_f32_queue, PcmF32QueueConsumer, PcmF32QueueProducer};
pub use timed_queue::{PushError, ReadResult, TimedAudioQueue, TimedAudioSegment};

pub use mixer::{AudioMixer, AudioStreamId, AudioStreamParams};
pub use types::{AudioBuffer, AudioSamples, ChannelLayout, SampleFormat};

/// Decoder-facing audio enqueue handle.
///
/// This is currently an alias for the producer side of a bounded SPSC PCM queue.
pub type AudioStreamHandle = PcmF32QueueProducer;

#[cfg(feature = "audio_wav")]
pub use wav::WavAudioBackend;

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
  // --------------------------------------------------------------------------
  // Backend / device errors
  // --------------------------------------------------------------------------
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

  // --------------------------------------------------------------------------
  // Decoder-facing buffer validation/conversion errors
  // --------------------------------------------------------------------------
  #[error("invalid channel count {channels}")]
  InvalidChannels { channels: usize },

  #[error("invalid sample rate {sample_rate}")]
  InvalidSampleRate { sample_rate: u32 },

  #[error(
    "audio buffer format/layout mismatch with data: format={format:?} data_format={data_format:?} layout={layout:?} data_layout={data_layout:?}"
  )]
  BufferMetadataMismatch {
    format: SampleFormat,
    data_format: SampleFormat,
    layout: ChannelLayout,
    data_layout: ChannelLayout,
  },

  #[error(
    "interleaved buffer has {len_samples} samples which is not divisible by channel count {channels}"
  )]
  InvalidInterleavedLength { len_samples: usize, channels: usize },

  #[error("planar buffer expected {channels} planes but got {planes}")]
  InvalidPlaneCount { channels: usize, planes: usize },

  #[error(
    "planar buffer plane {plane} has {len_samples} samples but expected {expected_samples}"
  )]
  InvalidPlaneLength {
    plane: usize,
    len_samples: usize,
    expected_samples: usize,
  },

  #[error(
    "audio buffer config mismatch: expected {expected_channels}ch@{expected_sample_rate_hz}Hz but got {channels}ch@{sample_rate_hz}Hz"
  )]
  StreamConfigMismatch {
    expected_channels: usize,
    expected_sample_rate_hz: u32,
    channels: usize,
    sample_rate_hz: u32,
  },
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
    Self::new_best_effort_with_config(&audio_engine_config())
  }

  /// Like [`Self::new_best_effort`], but uses the provided configuration instead of reading
  /// process-wide defaults.
  #[must_use]
  pub fn new_best_effort_with_config(cfg: &AudioEngineConfig) -> Box<dyn AudioBackend> {
    #[cfg(feature = "audio_cpal")]
    {
      use std::sync::Once;
      static WARN_ONCE: Once = Once::new();

      match CpalAudioBackend::new_with_config(cfg) {
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

    Box::new(NullAudioBackend::new_with_defaults(cfg.default_sample_rate_hz, cfg.default_channels))
  }
}

/// High-level audio engine that owns an output backend and its configuration.
///
/// This is the intended entry point for media playback code. It centralizes all tunables and
/// provides a consistent configuration surface across different backends.
pub struct AudioEngine {
  config: Arc<AudioEngineConfig>,
  backend: Box<dyn AudioBackend>,
}

impl AudioEngine {
  /// Create an [`AudioEngine`] using a "best effort" backend selection policy.
  #[must_use]
  pub fn new_best_effort(config: Arc<AudioEngineConfig>) -> Self {
    let backend = <dyn AudioBackend>::new_best_effort_with_config(&config);
    Self { config, backend }
  }

  /// Convenience constructor that uses the currently active configuration.
  ///
  /// By default this parses `FASTR_AUDIO_*` environment variables, but unit tests can install an
  /// override via [`set_audio_engine_config`].
  #[must_use]
  pub fn init_from_env() -> Self {
    Self::new_best_effort(audio_engine_config())
  }

  #[must_use]
  pub fn config(&self) -> &AudioEngineConfig {
    &self.config
  }

  #[must_use]
  pub fn backend(&self) -> &dyn AudioBackend {
    &*self.backend
  }
}

impl PcmF32QueueProducer {
  /// Push decoder-provided PCM samples in a variety of common formats/layouts.
  ///
  /// Input is validated and normalized to interleaved `f32` internally before enqueueing.
  pub fn push_audio(&mut self, buffer: AudioBuffer<'_>) -> Result<(), AudioError> {
    let expected_channels = self.channels();
    let expected_sample_rate_hz = self.sample_rate_hz();
    if buffer.channels != expected_channels || buffer.sample_rate != expected_sample_rate_hz {
      return Err(AudioError::StreamConfigMismatch {
        expected_channels,
        expected_sample_rate_hz,
        channels: buffer.channels,
        sample_rate_hz: buffer.sample_rate,
      });
    }

    // Avoid an intermediate allocation for already-normalized data.
    if let AudioSamples::InterleavedF32(samples) = buffer.data {
      if samples.len() % expected_channels != 0 {
        return Err(AudioError::InvalidInterleavedLength {
          len_samples: samples.len(),
          channels: expected_channels,
        });
      }
      return self.push_pcm_f32(samples, buffer.pts);
    }

    let converted = convert_to_f32_interleaved(&buffer)?;
    self.push_pcm_f32(&converted, buffer.pts)
  }

  /// Convenience helper for pushing interleaved `f32` PCM into the queue.
  pub fn push_pcm_f32(&mut self, samples: &[f32], pts: Option<Duration>) -> Result<(), AudioError> {
    let channels = self.channels();
    if channels == 0 {
      return Err(AudioError::InvalidChannels { channels });
    }
    if samples.len() % channels != 0 {
      return Err(AudioError::InvalidInterleavedLength {
        len_samples: samples.len(),
        channels,
      });
    }
    if let Some(pts) = pts {
      self.push(samples, pts);
    } else {
      self.push_without_pts(samples);
    }
    Ok(())
  }
}

#[cfg(all(test, feature = "audio_cpal"))]
mod audio_cpal_compile_tests {
  use super::CpalAudioBackend;

  /// Compile-only sanity check for the `audio_cpal` feature.
  ///
  /// This test must not attempt to open an audio device; it exists purely to ensure the optional
  /// backend type is available and is thread-safe.
  #[test]
  fn audio_cpal_feature_compiles() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CpalAudioBackend>();
  }
}
