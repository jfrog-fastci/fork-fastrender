use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;

use super::{AudioBackend, AudioClock, AudioSink, AudioStreamConfig};

#[derive(Debug)]
/// A silent audio backend used as a fallback when audio output is unavailable (e.g. CI/headless).
///
/// This backend is not intended to be a high-fidelity audio clock: it currently derives time from
/// `Instant` (see `AudioClock::Instant`), which is sufficient to keep the rest of the pipeline
/// running but is not deterministic. Tests that need deterministic time should inject a virtual
/// master clock (see `docs/media_clocking.md`).
pub struct NullAudioBackend {
  config: AudioStreamConfig,
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
    let sample_rate_hz = sample_rate_hz.max(1);
    let channels = channels.max(1);
    Self {
      config: AudioStreamConfig::new(sample_rate_hz, channels),
      start: Instant::now(),
      frames_played: Arc::new(AtomicU64::new(0)),
    }
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
    if self.config.channels != 0 {
      let frames = (samples.len() / usize::from(self.config.channels)) as u64;
      self.frames_played.fetch_add(frames, std::sync::atomic::Ordering::Relaxed);
    }
    samples.len()
  }

  fn set_volume(&self, _volume: f32) {}
}
