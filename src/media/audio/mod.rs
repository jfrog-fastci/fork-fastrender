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
//! Output latency is exposed via [`AudioOutputInfo::estimated_output_latency`]. Backends that derive time
//! from callback frame counts (`AudioClock::OutputFrames`) can be ahead of “what the user hears” by
//! a roughly-constant buffer duration; callers should treat this as a constant offset (not drift)
//! and compensate using the estimated latency.
//!
//! See `docs/media_clocking.md` for the broader clocking model and recommended sync tolerances.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::clock::MediaClock;
use super::DecodedAudioChunk;
use crate::media::audio_clock::InterpolatedAudioClock;

mod config;
#[cfg(feature = "audio_cpal")]
mod cpal_backend;
mod latency;
pub mod engine;
pub mod limits;
mod mixer_decision;
mod null_backend;
mod restart;
pub mod remix;
mod ring_buffer;
#[cfg(feature = "audio_cpal")]
mod thread_priority;
pub mod convert;
pub mod drift;
pub mod mixer;
pub mod queue;
#[cfg(test)]
pub mod preroll;
pub mod resample;
pub mod test_signal;
pub mod timed_queue;
pub mod types;
#[cfg(any(feature = "audio_wav", test))]
mod wav_backend;

/// Legacy audio mixer/backends that were previously exposed as the top-level `audio` module.
///
/// Prefer the modern `crate::media::audio` APIs for new code.
#[deprecated(note = "Legacy audio APIs (previously the top-level `audio` module). Prefer `crate::media::audio` for new code.")]
pub mod legacy;

pub use config::{
  audio_engine_config, set_audio_engine_config, with_audio_engine_config, AudioEngineConfig,
  AudioEngineConfigGuard,
};
#[cfg(feature = "audio_cpal")]
pub use cpal_backend::{list_output_devices, CpalAudioBackend};
pub use convert::convert_to_f32_interleaved;
pub use remix::{remix_interleaved_f32, RemixError};
pub use latency::{
  duration_to_frames_ceil, duration_to_frames_floor, frames_to_duration, latency_from_timestamps,
};
pub use drift::{DriftController, DriftControllerConfig, DriftResampler};
pub use engine::{AudioEngine, AudioEngineSink, AudioGroupId};
pub use null_backend::NullAudioBackend;
pub use queue::{pcm_f32_queue, PcmF32Queue, PcmF32QueueConsumer, PcmF32QueueProducer};
pub use timed_queue::{PopResult, PushError, ReadResult, TimedAudioQueue, TimedAudioSegment};

pub use mixer::{AudioMixer, AudioStreamId, AudioStreamParams};
pub use types::{AudioBuffer, AudioSamples, ChannelLayout, SampleFormat};

impl From<DecodedAudioChunk> for TimedAudioSegment {
  fn from(chunk: DecodedAudioChunk) -> Self {
    Self {
      start_pts: Duration::from_nanos(chunk.pts_ns),
      samples: chunk.samples,
      channels: chunk.channels,
      sample_rate: chunk.sample_rate_hz,
    }
  }
}

impl TimedAudioQueue {
  /// Convenience helper for pushing a decoded PCM chunk with an explicit PTS.
  pub fn push_decoded_chunk(&mut self, chunk: DecodedAudioChunk) -> Result<(), PushError> {
    self.push_segment(chunk.into())
  }
}

/// Decoder-facing audio enqueue handle.
///
/// This is currently an alias for the producer side of a bounded SPSC PCM queue.
///
/// For timestamp-aware buffering (gaps/overlaps), use [`TimedAudioQueue`] instead.
pub type AudioStreamHandle = PcmF32QueueProducer;

#[cfg(any(feature = "audio_wav", test))]
pub use wav_backend::WavAudioBackend;

/// Opaque identifier for an output audio device.
///
/// CPAL does not expose a cross-platform stable device UUID. We therefore identify devices by:
/// - their reported name, and
/// - an ordinal for the Nth occurrence of that name in the host's output device enumeration.
///
/// This is stable enough for:
/// - presenting a device list in a settings UI, and
/// - re-selecting a previously chosen device later in the same session (or across runs) when the
///   host enumerates devices consistently.
///
/// If the device disappears, selection fails with [`AudioError::OutputDeviceNotFound`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioDeviceId {
  pub name: String,
  pub ordinal: u32,
}

impl std::fmt::Display for AudioDeviceId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // `URL_SAFE_NO_PAD` is unambiguous and avoids introducing separators that would complicate
    // parsing (e.g. ':' characters).
    let encoded =
      base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.name.as_bytes());
    write!(f, "{}:{}", self.ordinal, encoded)
  }
}

impl std::str::FromStr for AudioDeviceId {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let (ordinal_str, encoded_name) = s
      .split_once(':')
      .ok_or_else(|| "missing ':' delimiter".to_string())?;
    let ordinal: u32 = ordinal_str
      .parse()
      .map_err(|_| "invalid device ordinal".to_string())?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
      .decode(encoded_name)
      .map_err(|err| format!("invalid base64 device name: {err}"))?;
    let name = String::from_utf8(decoded)
      .map_err(|err| format!("device name is not valid UTF-8: {err}"))?;
    Ok(Self { name, ordinal })
  }
}

/// Lightweight output device metadata for presentation in a UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
  pub id: AudioDeviceId,
  pub name: String,
}

/// Selector describing which output device to use when constructing a CPAL backend.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceSelector {
  /// Use the host's default output device.
  Default,
  /// Use a specific output device (as returned by [`list_output_devices`] when the `audio_cpal`
  /// feature is enabled).
  Device(AudioDeviceId),
}

impl std::fmt::Display for DeviceSelector {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Default => write!(f, "default"),
      Self::Device(id) => write!(f, "device:{id}"),
    }
  }
}

impl std::str::FromStr for DeviceSelector {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s == "default" {
      return Ok(Self::Default);
    }
    let Some(raw) = s.strip_prefix("device:") else {
      return Err("unsupported device selector format".to_string());
    };
    let id = raw.parse::<AudioDeviceId>()?;
    Ok(Self::Device(id))
  }
}

pub(crate) fn next_device_id_for_name(
  seen: &mut std::collections::HashMap<String, u32>,
  name: &str,
) -> AudioDeviceId {
  let ordinal = seen.get(name).copied().unwrap_or(0);
  seen.insert(name.to_string(), ordinal.saturating_add(1));
  AudioDeviceId {
    name: name.to_string(),
    ordinal,
  }
}

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
  /// Output stream configuration (sample rate + channel count).
  pub config: AudioStreamConfig,
  /// The number of frames the backend expects per callback, when known.
  pub callback_frames: Option<u32>,
  /// Best-effort estimate of the latency between writing samples in the callback and the samples
  /// being heard at the output device.
  pub estimated_output_latency: Duration,
  /// Backend identifier for debugging/telemetry.
  pub backend_name: &'static str,
}

impl AudioOutputInfo {
  /// Returns the estimated latency expressed in frames, rounding up.
  #[must_use]
  pub fn estimated_latency_frames(&self) -> u64 {
    duration_to_frames_ceil(self.config.sample_rate_hz, self.estimated_output_latency)
  }

  #[must_use]
  pub fn stream_config(&self) -> AudioStreamConfig {
    self.config
  }
}

#[derive(Clone, Debug)]
pub enum AudioClock {
  /// Clock derived from the number of frames the output backend reports as delivered.
  OutputFrames { clock: Arc<InterpolatedAudioClock> },
  /// Clock derived from wall time (fallback for backends without an output playhead counter).
  Instant {
    start: Instant,
    sample_rate_hz: u32,
  },
}

impl AudioClock {
  #[must_use]
  pub fn sample_rate_hz(&self) -> u32 {
    match self {
      Self::OutputFrames { clock } => clock.sample_rate_hz(),
      Self::Instant { sample_rate_hz, .. } => *sample_rate_hz,
    }
  }

  #[must_use]
  pub fn frames(&self) -> u64 {
    match self {
      Self::OutputFrames { clock } => clock.frames_written(),
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
  /// by itself. Callers should subtract [`AudioOutputInfo::estimated_output_latency`] when they need a
  /// "time heard" estimate.
  pub fn time(&self) -> Duration {
    match self {
      Self::OutputFrames { clock } => clock.now(),
      Self::Instant { start, .. } => start.elapsed(),
    }
  }
}

impl MediaClock for AudioClock {
  fn now(&self) -> Duration {
    self.time()
  }

  fn is_started(&self) -> bool {
    match self {
      Self::OutputFrames { clock } => clock.is_started(),
      Self::Instant { .. } => true,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::media::clock::MediaClock;
  use std::collections::HashMap;
  use std::sync::Arc;
  use std::time::{Duration, Instant};

  #[test]
  fn audio_clock_output_frames_tracks_frames_and_started_state() {
    let raw = Arc::new(InterpolatedAudioClock::new(48_000));
    let clock = AudioClock::OutputFrames { clock: raw.clone() };

    assert!(!clock.is_started());
    assert_eq!(clock.frames(), 0);
    assert_eq!(clock.sample_rate_hz(), 48_000);

    // Simulate a callback that produced 480 frames (10ms at 48kHz).
    let callback_end = Instant::now();
    raw.on_callback_end_at(callback_end, 480, None);

    assert!(clock.is_started());
    assert_eq!(clock.frames(), 480);
  }

  #[test]
  fn audio_clock_output_frames_time_48k() {
    fn assert_frames_map_to_time(frames_in_callback: u32, expected: Duration) {
      let raw = Arc::new(InterpolatedAudioClock::new(48_000));
      let clock = AudioClock::OutputFrames { clock: raw.clone() };

      // Drive the interpolated clock deterministically via explicit callback end instants.
      let callback_end = Instant::now();
      raw.on_callback_end_at(callback_end, frames_in_callback, None);

      assert_eq!(clock.frames(), u64::from(frames_in_callback));

      // At exactly the callback end instant, no interpolation is applied; the time should match
      // `frames / sample_rate` precisely.
      assert_eq!(raw.now_at(callback_end), expected);

      // `AudioClock::time()` uses `Instant::now()` internally, which is inherently nondeterministic.
      // Verify it stays within a tight window around a same-thread query.
      let before = Instant::now();
      let expected_before = raw.now_at(before);
      let observed = clock.time();
      let after = Instant::now();
      let expected_after = raw.now_at(after);
      assert!(
        observed >= expected_before && observed <= expected_after,
        "time out of bounds: expected {expected_before:?}..={expected_after:?}, got {observed:?}"
      );
    }

    assert_frames_map_to_time(48_000, Duration::from_secs(1));
    assert_frames_map_to_time(24_000, Duration::from_millis(500));
    assert_frames_map_to_time(1, Duration::from_nanos(20_833));
  }

  #[test]
  fn audio_clock_output_frames_large_values_do_not_panic() {
    let raw = Arc::new(InterpolatedAudioClock::new(48_000));
    let clock = AudioClock::OutputFrames { clock: raw.clone() };

    // Advance the clock to a frame count where `frames * 1_000_000_000` would overflow a `u64`
    // (if implemented with non-widening arithmetic).
    //
    // This should not panic in debug builds due to intermediate overflow.
    let start = Instant::now();
    for i in 0..5u32 {
      raw.on_callback_end_at(start + Duration::from_millis(u64::from(i)), u32::MAX, None);
    }

    assert_eq!(clock.frames(), u64::from(u32::MAX) * 5);

    let _ = raw.now_at(start + Duration::from_millis(4));
    let _ = clock.time();
  }

  #[test]
  fn cpal_device_select_id_roundtrip_parses_base64_names() {
    let id = AudioDeviceId {
      name: "Built-in Output: Speakers 🔊".to_string(),
      ordinal: 2,
    };
    let encoded = id.to_string();
    let parsed: AudioDeviceId = encoded.parse().unwrap();
    assert_eq!(parsed, id);
  }

  #[test]
  fn cpal_device_select_selector_roundtrip() {
    let selector = DeviceSelector::Device(AudioDeviceId {
      name: "Headphones".to_string(),
      ordinal: 0,
    });
    let encoded = selector.to_string();
    let parsed: DeviceSelector = encoded.parse().unwrap();
    assert_eq!(parsed, selector);
  }

  #[test]
  fn cpal_device_select_assigns_ordinals_per_name() {
    let mut seen = HashMap::new();
    let a0 = next_device_id_for_name(&mut seen, "A");
    let b0 = next_device_id_for_name(&mut seen, "B");
    let a1 = next_device_id_for_name(&mut seen, "A");
    assert_eq!(
      a0,
      AudioDeviceId {
        name: "A".to_string(),
        ordinal: 0
      }
    );
    assert_eq!(
      b0,
      AudioDeviceId {
        name: "B".to_string(),
        ordinal: 0
      }
    );
    assert_eq!(
      a1,
      AudioDeviceId {
        name: "A".to_string(),
        ordinal: 1
      }
    );
  }
}
#[derive(Debug, Error)]
pub enum AudioError {
  // --------------------------------------------------------------------------
  // Backend / device errors
  // --------------------------------------------------------------------------
  #[error("no default output audio device is available")]
  NoOutputDevice,
  #[error("failed to enumerate output audio devices: {0}")]
  OutputDeviceEnumerationFailed(String),
  #[error("output audio device not found ({selector})")]
  OutputDeviceNotFound { selector: DeviceSelector },
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
  #[error("invalid audio spec: {reason}")]
  InvalidSpec { reason: String },

  #[error("invalid audio buffer: {reason}")]
  InvalidBuffer { reason: String },

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

impl AudioError {
  pub fn invalid_spec(reason: impl Into<String>) -> Self {
    Self::InvalidSpec {
      reason: reason.into(),
    }
  }

  pub fn invalid_buffer(reason: impl Into<String>) -> Self {
    Self::InvalidBuffer {
      reason: reason.into(),
    }
  }
}

pub trait AudioSink: Send + Sync {
  fn config(&self) -> AudioStreamConfig;

  /// Queue interleaved f32 PCM samples for playback.
  ///
  /// Samples must be at the sink/backend output sample rate and channel count.
  /// Returns the number of samples accepted (the remainder, if any, was dropped).
  fn push_interleaved_f32(&self, samples: &[f32]) -> usize;

  /// Queue interleaved f32 PCM samples for playback, resampling as needed.
  ///
  /// This is a convenience helper for callers that are producing decoded audio at a sample rate
  /// different from the output device (or applying an `HTMLMediaElement.playbackRate`-style speed
  /// change).
  ///
  /// Notes:
  /// - The input must be interleaved with the sink's channel count (no channel remixing is
  ///   performed).
  /// - `playback_rate` is implemented by scaling the effective input sample rate (i.e. naive
  ///   resampling/pitch shift).
  /// - Returns the number of *output* samples accepted by the sink (at the sink's sample rate).
  fn push_interleaved_f32_resampled(
    &self,
    samples: &[f32],
    input_sample_rate_hz: u32,
    playback_rate: f64,
  ) -> usize {
    let cfg = self.config();
    let channels = usize::from(cfg.channels.max(1));
    let usable_len = samples.len() - (samples.len() % channels);
    let samples = &samples[..usable_len];
    if samples.is_empty() {
      return 0;
    }

    if input_sample_rate_hz == cfg.sample_rate_hz && playback_rate == 1.0 {
      return self.push_interleaved_f32(samples);
    }

    let input_frames = samples.len() / channels;
    if input_frames == 0 {
      return 0;
    }

    let effective_in_rate = (input_sample_rate_hz as f64) * playback_rate;
    if !(effective_in_rate.is_finite()) || effective_in_rate <= 0.0 {
      return 0;
    }

    // Preserve duration: frames_out / output_rate == frames_in / (input_rate * playback_rate)
    let out_frames_f =
      (input_frames as f64) * (cfg.sample_rate_hz as f64) / (input_sample_rate_hz as f64) / playback_rate;
    let out_frames = if out_frames_f.is_finite() && out_frames_f > 0.0 {
      // Prefer truncation to avoid overshooting the provided input range.
      out_frames_f.floor() as usize
    } else {
      0
    };

    if out_frames == 0 {
      return 0;
    }

    let resampled = resample::resample_interleaved_f32_linear_with_playback_rate(
      samples,
      channels,
      input_sample_rate_hz,
      cfg.sample_rate_hz,
      playback_rate,
      out_frames,
    );
    self.push_interleaved_f32(&resampled)
  }

  /// Sets the playback volume (gain) for this sink.
  ///
  /// Setting volume to `0.0` is a mute: the sink must still drain queued audio so media time
  /// continues advancing and buffered-duration/backpressure statistics remain meaningful. Only
  /// an explicit pause should stop draining.
  fn set_volume(&self, volume: f32);
}

pub trait AudioBackend: Send + Sync {
  fn output_config(&self) -> AudioStreamConfig;

  /// Returns information about the active output stream, including the estimated output latency.
  ///
  /// Backends should provide best-effort values even when the underlying API does not expose
  /// explicit latency information.
  fn output_info(&self) -> AudioOutputInfo {
    let config = self.output_config();
    AudioOutputInfo {
      config,
      callback_frames: None,
      estimated_output_latency: Duration::ZERO,
      backend_name: "unknown",
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

impl PcmF32QueueProducer {
  /// Push decoder-provided PCM samples in a variety of common formats/layouts.
  ///
  /// Input is validated and normalized to interleaved `f32` internally before enqueueing.
  pub fn push_audio(&mut self, buffer: AudioBuffer<'_>) -> Result<(), AudioError> {
    // Treat decoder-provided metadata as untrusted; reject absurd values early before any
    // conversion/normalization work.
    let max_channels = usize::from(limits::MAX_CHANNELS);
    if buffer.channels == 0 || buffer.channels > max_channels {
      return Err(AudioError::invalid_spec(format!(
        "channels {} is outside supported range 1..={}",
        buffer.channels, max_channels
      )));
    }
    if buffer.sample_rate == 0 || buffer.sample_rate > limits::MAX_SAMPLE_RATE_HZ {
      return Err(AudioError::invalid_spec(format!(
        "sample_rate {} is outside supported range 1..={}",
        buffer.sample_rate,
        limits::MAX_SAMPLE_RATE_HZ
      )));
    }

    let expected_channels = self.channels();
    let expected_sample_rate_hz = self.sample_rate_hz();
    if buffer.channels != expected_channels {
      return Err(AudioError::StreamConfigMismatch {
        expected_channels,
        expected_sample_rate_hz,
        channels: buffer.channels,
        sample_rate_hz: buffer.sample_rate,
      });
    }

    let mut converted: Option<Vec<f32>> = None;
    let samples: &[f32] = match buffer.data {
      AudioSamples::InterleavedF32(samples) => {
        if samples.len() % expected_channels != 0 {
          return Err(AudioError::InvalidInterleavedLength {
            len_samples: samples.len(),
            channels: expected_channels,
          });
        }
        samples
      }
      _ => {
        converted = Some(convert_to_f32_interleaved(&buffer)?);
        converted.as_ref().unwrap() // fastrender-allow-unwrap
      }
    };

    if buffer.sample_rate == expected_sample_rate_hz {
      return self.push_pcm_f32(samples, buffer.pts);
    }

    // Resample decoded audio to the queue/device sample rate. This is a simple linear
    // interpolation resampler (good enough for an MVP); higher-quality band-limited resampling can
    // be layered later if needed.
    let input_frames = samples.len() / expected_channels;
    if input_frames == 0 {
      return Ok(());
    }

    let out_frames_u128 =
      (input_frames as u128).saturating_mul(expected_sample_rate_hz as u128);
    let out_frames_u128 = out_frames_u128
      .saturating_add((buffer.sample_rate as u128) / 2)
      .checked_div(buffer.sample_rate as u128)
      .unwrap_or(u128::MAX);
    let out_frames = usize::try_from(out_frames_u128).unwrap_or(usize::MAX);
    if out_frames > limits::MAX_FRAMES_PER_PUSH {
      return Err(AudioError::invalid_buffer(format!(
        "resampled buffer would have {out_frames} frames which exceeds MAX_FRAMES_PER_PUSH {}",
        limits::MAX_FRAMES_PER_PUSH
      )));
    }

    let resampled = resample::resample_interleaved_f32_linear(
      samples,
      expected_channels,
      buffer.sample_rate,
      expected_sample_rate_hz,
      out_frames,
    );
    self.push_pcm_f32(&resampled, buffer.pts)
  }

  /// Convenience helper for pushing interleaved `f32` PCM into the queue.
  pub fn push_pcm_f32(&mut self, samples: &[f32], pts: Option<Duration>) -> Result<(), AudioError> {
    let channels = self.channels();
    if channels == 0 {
      return Err(AudioError::invalid_spec("queue channel count must be non-zero"));
    }
    if samples.len() % channels != 0 {
      return Err(AudioError::invalid_buffer(format!(
        "interleaved sample buffer length {} is not divisible by channels {}",
        samples.len(),
        channels
      )));
    }
    let frames = samples.len() / channels;
    if frames > limits::MAX_FRAMES_PER_PUSH {
      return Err(AudioError::invalid_buffer(format!(
        "audio buffer has {} frames which exceeds MAX_FRAMES_PER_PUSH {}",
        frames,
        limits::MAX_FRAMES_PER_PUSH
      )));
    }
    if let Some(pts) = pts {
      self.push(samples, pts);
    } else {
      self.push_without_pts(samples);
    }
    Ok(())
  }
}
<<<<<<< HEAD

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
=======
>>>>>>> 4429ac38 (feat(audio): add deterministic channel remix helpers)
