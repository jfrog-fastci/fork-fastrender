use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AudioBackend, AudioClock, AudioOutputInfo, AudioSink, AudioStreamConfig};
use super::limits::{MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};

#[derive(Debug)]
/// A silent audio backend used as a fallback when audio output is unavailable (e.g. CI/headless).
///
/// This backend is not intended to be a high-fidelity audio clock: it currently derives time from
/// `Instant` (see `AudioClock::Instant`), which is sufficient to keep the rest of the pipeline
/// running but is not deterministic. Tests that need deterministic time should inject a virtual
/// master clock (see `docs/media_clocking.md`).
pub struct NullAudioBackend {
  config: AudioStreamConfig,
  estimated_output_latency: Duration,
  start: Instant,
  frames_played: Arc<AtomicU64>,
}

impl NullAudioBackend {
  #[must_use]
  pub fn new() -> Self {
    Self::new_with_defaults(48_000, 2)
  }

  #[must_use]
  pub fn new_with_defaults(sample_rate_hz: u32, channels: u16) -> Self {
    let sample_rate_hz = sample_rate_hz.clamp(1, MAX_SAMPLE_RATE_HZ);
    let channels = channels.clamp(1, MAX_CHANNELS);
    Self {
      config: AudioStreamConfig::new(sample_rate_hz, channels),
      estimated_output_latency: Duration::ZERO,
      start: Instant::now(),
      frames_played: Arc::new(AtomicU64::new(0)),
    }
  }

  /// Create a `NullAudioBackend` with an explicit output-latency model.
  ///
  /// This is primarily intended for deterministic tests of A/V sync behaviour.
  #[must_use]
  pub fn new_with_latency(estimated_output_latency: Duration) -> Self {
    let mut backend = Self::new();
    backend.estimated_output_latency = estimated_output_latency;
    backend
  }
}

impl Default for NullAudioBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl AudioBackend for NullAudioBackend {
  fn output_config(&self) -> AudioStreamConfig {
    self.config
  }

  fn output_info(&self) -> AudioOutputInfo {
    AudioOutputInfo {
      config: self.config,
      callback_frames: None,
      estimated_output_latency: self.estimated_output_latency,
      backend_name: "null",
    }
  }

  fn clock(&self) -> AudioClock {
    AudioClock::Instant {
      start: self.start,
      sample_rate_hz: self.config.sample_rate_hz,
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    Box::new(NullAudioSink {
      config: self.config,
      frames_played: self.frames_played.clone(),
    })
  }
}

#[derive(Debug)]
struct NullAudioSink {
  config: AudioStreamConfig,
  frames_played: Arc<AtomicU64>,
}

impl AudioSink for NullAudioSink {
  fn config(&self) -> AudioStreamConfig {
    self.config
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    let channels = usize::from(self.config.channels);
    if channels == 0 {
      return 0;
    }

    let usable_len = samples.len() - (samples.len() % channels);
    let frames = usable_len / channels;
    let frames = frames.min(MAX_FRAMES_PER_PUSH);
    let accepted_samples = frames * channels;

    self.frames_played.fetch_add(frames as u64, std::sync::atomic::Ordering::Relaxed);
    accepted_samples
  }

  fn set_volume(&self, _volume: f32) {}
}
