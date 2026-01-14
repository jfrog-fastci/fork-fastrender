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
//! Real-time audio callbacks (e.g. the CPAL output callback) must never unwind across the callback
//! boundary; see `panic_guard` for helpers that catch panics and output silence.
//!
//! See `docs/media_clocking.md` for the broader clocking model and recommended sync tolerances.
use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::media::clock::MediaClock;
use super::DecodedAudioChunk;
use crate::debug::trace::TraceHandle;
use crate::media::audio_clock::InterpolatedAudioClock;

mod config;
#[cfg(feature = "audio_cpal")]
mod cpal_backend;
mod error;
mod latency;
#[cfg(any(feature = "audio_cpal", test))]
mod panic_guard;
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
mod stream;
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
pub use error::{AudioError, AudioErrorKind, AudioSampleFormat, Result};
pub use latency::{
  duration_to_frames_ceil, duration_to_frames_floor, frames_to_duration, latency_from_timestamps,
};
pub use drift::{DriftController, DriftControllerConfig, DriftResampler};
pub use engine::{AudioEngine, AudioEngineSink, AudioEngineTestGuard, AudioGroupId, AudioSinkHandle};
pub use null_backend::NullAudioBackend;
pub use ring_buffer::AudioRingBuffer;
pub use queue::{pcm_f32_queue, PcmF32Queue, PcmF32QueueConsumer, PcmF32QueueProducer};
pub use stream::{AudioStreamError, AudioStreamHandle};
pub use timed_queue::{PopResult, PushError, ReadResult, TimedAudioQueue, TimedAudioSegment};

pub use mixer::{AudioMixer, AudioStreamId, AudioStreamParams};
pub use types::{AudioBuffer, AudioSamples, ChannelLayout, SampleFormat, SampleLayout};

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
  pub fn push_decoded_chunk(
    &mut self,
    chunk: DecodedAudioChunk,
  ) -> std::result::Result<(), PushError> {
    self.push_segment(chunk.into())
  }
}

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

  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
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

  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
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

impl std::fmt::Display for AudioStreamConfig {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}Hz {}ch", self.sample_rate_hz, self.channels)
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
      // Wall-time based clocks (e.g. null backend) are started immediately.
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

    // Wall-time based clocks are started immediately.
    let instant_clock = AudioClock::Instant {
      start: Instant::now(),
      sample_rate_hz: 48_000,
    };
    assert!(instant_clock.is_started());
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

  /// Queue PCM samples for playback in one of the supported input formats/layouts.
  ///
  /// This validates buffer lengths, converts the input into the internal f32 interleaved
  /// representation, and sanitizes decoded samples.
  ///
  /// For maximum throughput when you already have interleaved f32, prefer
  /// [`AudioSink::push_interleaved_f32`].
  fn push_buffer(&self, buffer: &AudioBuffer<'_>) -> Result<usize> {
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

    let cfg = self.config();
    let expected_channels = usize::from(cfg.channels.max(1));
    let expected_sample_rate_hz = cfg.sample_rate_hz;

    if buffer.channels != expected_channels || buffer.sample_rate != expected_sample_rate_hz {
      return Err(AudioError::StreamConfigMismatch {
        expected_channels,
        expected_sample_rate_hz,
        channels: buffer.channels,
        sample_rate_hz: buffer.sample_rate,
      });
    }

    let converted = convert_to_f32_interleaved(buffer)?;
    Ok(self.push_interleaved_f32(&converted))
  }

  /// Sets the playback volume (gain) for this sink.
  ///
  /// Setting volume to `0.0` is a mute: the sink must still drain queued audio so media time
  /// continues advancing and buffered-duration/backpressure statistics remain meaningful. Only
  /// an explicit pause should stop draining.
  fn set_volume(&self, volume: f32);

  /// Notifies the sink that the queued audio stream experienced a discontinuity (seek/flush/etc).
  ///
  /// Implementations may apply a short fade to avoid audible clicks when playback resumes.
  fn notify_discontinuity(&self) {}
}

struct ThreadLocalConvertState {
  converter: convert::AudioConverter,
  buf: Vec<f32>,
}

impl Default for ThreadLocalConvertState {
  fn default() -> Self {
    Self {
      converter: convert::AudioConverter::new(),
      buf: Vec::new(),
    }
  }
}

thread_local! {
  static CONVERT_STATE: RefCell<ThreadLocalConvertState> = RefCell::new(ThreadLocalConvertState::default());
}

impl dyn AudioSink {
  /// Like [`AudioSink::push_interleaved_f32`], but accepts an explicit input format.
  ///
  /// If `sample_rate_hz` / `channels` do not match the sink's output config, the samples are
  /// converted (channel remix + linear resampling) before being queued.
  ///
  /// Note: the current resampler is a linear-interpolation MVP; it is not band-limited and may
  /// introduce audible artifacts.
  pub fn push_interleaved_f32_with_format(
    &self,
    samples: &[f32],
    sample_rate_hz: u32,
    channels: u16,
  ) -> usize {
    if samples.is_empty() || sample_rate_hz == 0 || channels == 0 {
      return 0;
    }

    let out_cfg = self.config();
    if out_cfg.sample_rate_hz == sample_rate_hz && out_cfg.channels == channels {
      return self.push_interleaved_f32(samples);
    }

    let in_channels = usize::from(channels);
    let frames = samples.len() / in_channels;
    let samples = &samples[..frames * in_channels];
    if samples.is_empty() {
      return 0;
    }

    let out_channels = usize::from(out_cfg.channels.max(1));
    CONVERT_STATE.with(|cell| {
      let mut state = cell.borrow_mut();
      let ThreadLocalConvertState { converter, buf } = &mut *state;
      converter.convert_f32_into(
        samples,
        sample_rate_hz,
        in_channels,
        out_cfg.sample_rate_hz,
        out_channels,
        buf,
      );
      self.push_interleaved_f32(buf.as_slice())
    })
  }
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
    Self::new_best_effort_with_config_and_trace(&audio_engine_config(), TraceHandle::default())
  }

  /// Like [`Self::new_best_effort`], but uses the provided configuration instead of reading
  /// process-wide defaults.
  #[must_use]
  pub fn new_best_effort_with_config(cfg: &AudioEngineConfig) -> Box<dyn AudioBackend> {
    Self::new_best_effort_with_config_and_trace(cfg, TraceHandle::default())
  }

  /// Like [`Self::new_best_effort`], but wires up audio tracing spans into the provided handle.
  #[must_use]
  pub fn new_best_effort_with_trace(trace: TraceHandle) -> Box<dyn AudioBackend> {
    Self::new_best_effort_with_config_and_trace(&audio_engine_config(), trace)
  }

  /// Construct a backend using the provided configuration + trace handle.
  #[must_use]
  pub fn new_best_effort_with_config_and_trace(
    cfg: &AudioEngineConfig,
    _trace: TraceHandle,
  ) -> Box<dyn AudioBackend> {
    #[cfg(feature = "audio_cpal")]
    {
      use std::sync::Once;
      static WARN_ONCE: Once = Once::new();

      match CpalAudioBackend::new_with_config_and_trace(cfg, _trace.clone()) {
        Ok(backend) => return Box::new(backend),
        Err(err) => {
          // Device-unavailable is expected in CI/headless runs; avoid spamming a warning in that
          // common case.
          if err.kind() != AudioErrorKind::DeviceUnavailable {
            WARN_ONCE.call_once(|| {
              eprintln!(
                "warning: failed to initialize CPAL audio backend ({err}); falling back to NullAudioBackend"
              );
            });
          }
        }
      }
    }

    Box::new(NullAudioBackend::new_with_config_and_trace(cfg, _trace))
  }

  /// Returns a shared monotonic clock suitable for driving media time when audio is present.
  ///
  /// The returned clock is derived from the backend's timebase (often output-frame counts) and does
  /// **not** apply output latency compensation. Callers that need an estimate of "time heard"
  /// should subtract [`AudioOutputInfo::estimated_output_latency`].
  #[must_use]
  pub fn device_clock(&self) -> Arc<super::AudioDeviceClock> {
    Arc::new(self.clock())
  }
}
impl PcmF32QueueProducer {
  /// Push decoder-provided PCM samples in a variety of common formats/layouts.
  ///
  /// Input is validated and normalized to interleaved `f32` internally before enqueueing.
  ///
  /// If the buffer's `(channels, sample_rate)` do not match the queue/device config, this performs
  /// a best-effort conversion (channel remix + linear resampling).
  ///
  /// Note: the current resampler is a linear-interpolation MVP; it is not band-limited and may
  /// introduce audible artifacts.
  pub fn push_audio(&mut self, buffer: AudioBuffer<'_>) -> Result<()> {
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
    let needs_convert =
      buffer.channels != expected_channels || buffer.sample_rate != expected_sample_rate_hz;

    // Fast path: already interleaved f32 and no remix/resample needed.
    if let AudioSamples::InterleavedF32(samples) = buffer.data {
      if buffer.channels == 0 {
        return Err(AudioError::InvalidChannels {
          channels: buffer.channels,
        });
      }
      if buffer.sample_rate == 0 {
        return Err(AudioError::InvalidSampleRate {
          sample_rate: buffer.sample_rate,
        });
      }
      let data_format = buffer.data.format();
      let data_layout = buffer.data.layout();
      if buffer.format != data_format || buffer.layout != data_layout {
        return Err(AudioError::BufferMetadataMismatch {
          format: buffer.format,
          data_format,
          layout: buffer.layout,
          data_layout,
        });
      }
      if samples.len() % buffer.channels != 0 {
        return Err(AudioError::InvalidInterleavedLength {
          len_samples: samples.len(),
          channels: buffer.channels,
        });
      }

      if !needs_convert {
        return self.push_pcm_f32(samples, buffer.pts);
      }

      return CONVERT_STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let ThreadLocalConvertState { converter, buf } = &mut *state;
        converter.convert_f32_into(
          samples,
          buffer.sample_rate,
          buffer.channels,
          expected_sample_rate_hz,
          expected_channels,
          buf,
        );
        self.push_pcm_f32(buf.as_slice(), buffer.pts)
      });
    }

    let converted = convert_to_f32_interleaved(&buffer)?;
    if !needs_convert {
      return self.push_pcm_f32(&converted, buffer.pts);
    }

    CONVERT_STATE.with(|cell| {
      let mut state = cell.borrow_mut();
      let ThreadLocalConvertState { converter, buf } = &mut *state;
      converter.convert_f32_into(
        &converted,
        buffer.sample_rate,
        buffer.channels,
        expected_sample_rate_hz,
        expected_channels,
        buf,
      );
      self.push_pcm_f32(buf.as_slice(), buffer.pts)
    })
  }

  /// Convenience helper for pushing interleaved `f32` PCM into the queue.
  pub fn push_pcm_f32(&mut self, samples: &[f32], pts: Option<Duration>) -> Result<()> {
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
