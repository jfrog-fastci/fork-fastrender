use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use super::{AudioBackend, AudioClock, AudioOutputInfo, AudioSink, AudioStreamConfig};
use super::limits::{MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};
use crate::media::audio::ring_buffer::AudioRingBuffer;
use crate::media::audio_clock::InterpolatedAudioClock;

/// A silent audio backend used as a fallback when audio output is unavailable (e.g. CI/headless).
///
/// By default, the backend:
/// - derives its clock from wall time (`AudioClock::Instant`), and
/// - discards pushed audio immediately.
///
/// For deterministic unit tests that need to inspect mixed output and verify draining semantics,
/// construct the backend with [`Self::new_deterministic_with_defaults`] and drive playback via
/// [`Self::render`].
#[derive(Debug)]
pub struct NullAudioBackend {
  config: AudioStreamConfig,
  estimated_output_latency: Duration,
  start: Instant,

  // Non-deterministic mode: used only for coarse metrics (e.g. frames pushed).
  frames_played: Arc<AtomicU64>,

  // Deterministic mode: software mixer + output-frame clock driven by `render()`.
  mixer: Option<Arc<MixerState>>,
  clock: Option<Arc<InterpolatedAudioClock>>,
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
      mixer: None,
      clock: None,
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

  /// Construct a deterministic variant of the null backend for unit tests.
  ///
  /// This enables a software mixer and exposes an `AudioClock::OutputFrames` clock that advances
  /// when [`Self::render`] is called.
  #[must_use]
  pub fn new_deterministic_with_defaults(sample_rate_hz: u32, channels: u16) -> Self {
    let mut backend = Self::new_with_defaults(sample_rate_hz, channels);
    backend.mixer = Some(Arc::new(MixerState::new(backend.config)));
    backend.clock = Some(Arc::new(InterpolatedAudioClock::new(backend.config.sample_rate_hz)));
    backend
  }

  /// Convenience helper for creating a deterministic backend with the default 48kHz stereo config.
  #[must_use]
  pub fn new_deterministic() -> Self {
    Self::new_deterministic_with_defaults(48_000, 2)
  }

  /// Simulate an output callback that requests `frames` frames of audio.
  ///
  /// Returns an interleaved `f32` buffer with length `frames * channels`.
  ///
  /// This is only meaningful for the deterministic backend variant created via
  /// [`Self::new_deterministic`] / [`Self::new_deterministic_with_defaults`]. In non-deterministic
  /// mode it returns silence.
  pub fn render(&self, frames: usize) -> Vec<f32> {
    let channels = usize::from(self.config.channels.max(1));
    let mut out = vec![0.0f32; frames.saturating_mul(channels)];

    if let Some(mixer) = &self.mixer {
      mixer.mix_into(&mut out);
    }

    // Advance the output-frame clock even if the output is silent (muted / underflow), matching
    // real device behaviour.
    if let Some(clock) = &self.clock {
      let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
      clock.on_callback_end(frames_u32);
      self
        .frames_played
        .fetch_add(u64::from(frames_u32), Ordering::Relaxed);
    }

    out
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
    if let Some(clock) = &self.clock {
      AudioClock::OutputFrames {
        clock: clock.clone(),
      }
    } else {
      AudioClock::Instant {
        start: self.start,
        sample_rate_hz: self.config.sample_rate_hz,
      }
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    if let Some(mixer) = &self.mixer {
      let sink = Arc::new(SinkState::new(self.config));
      mixer.register_sink(&sink);
      Box::new(DeterministicNullAudioSink { state: sink })
    } else {
      Box::new(NullAudioSink {
        config: self.config,
        frames_played: self.frames_played.clone(),
      })
    }
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

// -----------------------------------------------------------------------------
// Deterministic mixer implementation (tests)
// -----------------------------------------------------------------------------

#[derive(Debug)]
struct MixerState {
  sinks: RwLock<Vec<Weak<SinkState>>>,
}

impl MixerState {
  fn new(_config: AudioStreamConfig) -> Self {
    Self {
      sinks: RwLock::new(Vec::new()),
    }
  }

  fn register_sink(&self, sink: &Arc<SinkState>) {
    let mut sinks = self.sinks.write();
    sinks.retain(|weak| weak.upgrade().is_some());
    sinks.push(Arc::downgrade(sink));
  }

  fn mix_into(&self, dst: &mut [f32]) {
    let sinks = self.sinks.read();
    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };
      let gain_bits = sink.volume_bits.load(Ordering::Relaxed);
      let gain = f32::from_bits(gain_bits);
      sink.buffer.pop_add_into(dst, gain);
    }
  }
}

struct SinkState {
  config: AudioStreamConfig,
  buffer: AudioRingBuffer,
  volume_bits: AtomicU32,
}

impl SinkState {
  fn new(config: AudioStreamConfig) -> Self {
    let capacity = (config.sample_rate_hz as usize)
      .saturating_mul(usize::from(config.channels.max(1)))
      .saturating_mul(2); // ~2 seconds of audio.
    Self {
      config,
      buffer: AudioRingBuffer::new(capacity),
      volume_bits: AtomicU32::new(1.0f32.to_bits()),
    }
  }

  fn set_volume(&self, volume: f32) {
    let volume = if volume.is_finite() {
      volume.clamp(0.0, 1.0)
    } else {
      0.0
    };
    self.volume_bits.store(volume.to_bits(), Ordering::Relaxed);
  }
}

struct DeterministicNullAudioSink {
  state: Arc<SinkState>,
}

impl AudioSink for DeterministicNullAudioSink {
  fn config(&self) -> AudioStreamConfig {
    self.state.config
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    let channels = usize::from(self.state.config.channels.max(1));
    let usable_len = samples.len() - (samples.len() % channels);
    self.state.buffer.push(&samples[..usable_len])
  }

  fn set_volume(&self, volume: f32) {
    self.state.set_volume(volume);
  }
}
