use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use super::limits::{MAX_BUFFERED_DURATION, MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};
use super::{
  duration_to_frames_ceil, AudioBackend, AudioClock, AudioEngineConfig, AudioOutputInfo, AudioSink,
  AudioStreamConfig,
};
use crate::clock::{Clock, RealClock, VirtualClock};
use crate::debug::trace::TraceHandle;
use crate::media::audio_clock::InterpolatedAudioClock;

/// A silent audio backend intended for headless runs and deterministic tests.
///
/// This backend is driven by an injected monotonic [`Clock`]. It advances a simulated output
/// playhead even when no samples are queued (silence), making it suitable as a stable master clock
/// in A/V sync tests.
pub struct NullAudioBackend {
  config: AudioStreamConfig,
  max_buffered_duration: Duration,
  estimated_output_latency: Duration,
  clock: Arc<dyn Clock>,
  output_clock: Arc<InterpolatedAudioClock>,
  dropped_samples: Arc<AtomicU64>,
  underrun_samples: Arc<AtomicU64>,
  state: Mutex<BackendState>,
  trace: TraceHandle,
}

impl std::fmt::Debug for NullAudioBackend {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("NullAudioBackend")
      .field("config", &self.config)
      .field("frames_played", &self.output_clock.frames_written())
      .field(
        "dropped_samples",
        &self.dropped_samples.load(Ordering::Relaxed),
      )
      .field(
        "underrun_samples",
        &self.underrun_samples.load(Ordering::Relaxed),
      )
      .finish()
  }
}

#[derive(Debug)]
struct BackendState {
  /// Last injected-clock timestamp accounted for by the output playhead.
  ///
  /// This is advanced by the integer number of frames that fit into the elapsed time so fractional
  /// remainder is preserved and the mapping stays deterministic regardless of pump frequency.
  last_clock_now: Duration,
  sinks: Vec<Weak<SinkState>>,
}

impl NullAudioBackend {
  /// Creates a default `48kHz stereo` null backend driven by real time.
  #[must_use]
  pub fn new() -> Self {
    Self::new_with_defaults(48_000, 2)
  }

  /// Creates a default `48kHz stereo` null backend driven by real time, with tracing enabled.
  #[must_use]
  pub fn new_with_trace(trace: TraceHandle) -> Self {
    Self::new_with_defaults_and_trace(48_000, 2, trace)
  }

  /// Creates a null backend driven by real time with the provided stream configuration.
  #[must_use]
  pub fn new_with_defaults(sample_rate_hz: u32, channels: u16) -> Self {
    Self::new_with_defaults_and_trace_and_max_buffered_duration(
      sample_rate_hz,
      channels,
      TraceHandle::default(),
      Duration::from_secs(2),
    )
  }

  /// Like [`Self::new_with_defaults`], but installs a [`TraceHandle`] used for profiling spans.
  #[must_use]
  pub fn new_with_defaults_and_trace(
    sample_rate_hz: u32,
    channels: u16,
    trace: TraceHandle,
  ) -> Self {
    Self::new_with_defaults_and_trace_and_max_buffered_duration(
      sample_rate_hz,
      channels,
      trace,
      Duration::from_secs(2),
    )
  }

  /// Creates a null backend driven by real time with the provided stream configuration and queue
  /// buffer limit.
  #[must_use]
  pub fn new_with_defaults_and_max_buffered_duration(
    sample_rate_hz: u32,
    channels: u16,
    max_buffered_duration: Duration,
  ) -> Self {
    Self::new_with_defaults_and_trace_and_max_buffered_duration(
      sample_rate_hz,
      channels,
      TraceHandle::default(),
      max_buffered_duration,
    )
  }

  /// Like [`Self::new_with_defaults_and_trace`], but also configures the per-sink buffer duration.
  #[must_use]
  pub fn new_with_defaults_and_trace_and_max_buffered_duration(
    sample_rate_hz: u32,
    channels: u16,
    trace: TraceHandle,
    max_buffered_duration: Duration,
  ) -> Self {
    let sample_rate_hz = sample_rate_hz.clamp(1, MAX_SAMPLE_RATE_HZ);
    let channels = channels.clamp(1, MAX_CHANNELS);
    Self::new_with_clock_and_trace_and_max_buffered_duration(
      Arc::new(RealClock::default()),
      AudioStreamConfig::new(sample_rate_hz, channels),
      trace,
      max_buffered_duration,
    )
  }

  /// Create a `NullAudioBackend` using an [`AudioEngineConfig`] (sample rate/channels + buffer
  /// duration).
  #[must_use]
  pub fn new_with_config(engine_cfg: &AudioEngineConfig) -> Self {
    Self::new_with_config_and_trace(engine_cfg, TraceHandle::default())
  }

  /// Like [`Self::new_with_config`], but installs a trace handle for profiling spans.
  #[must_use]
  pub fn new_with_config_and_trace(engine_cfg: &AudioEngineConfig, trace: TraceHandle) -> Self {
    Self::new_with_defaults_and_trace_and_max_buffered_duration(
      engine_cfg.default_sample_rate_hz,
      engine_cfg.default_channels,
      trace,
      engine_cfg.per_stream_max_buffered_duration,
    )
  }

  /// Creates a null backend driven by an injected monotonic clock.
  ///
  /// This is intended for deterministic A/V sync tests (e.g. using
  /// [`crate::clock::VirtualClock`]).
  #[must_use]
  pub fn new_with_clock(clock: Arc<dyn Clock>, config: AudioStreamConfig) -> Self {
    Self::new_with_clock_and_trace(clock, config, TraceHandle::default())
  }

  /// Like [`Self::new_with_clock`], but installs a [`TraceHandle`] used for profiling spans.
  #[must_use]
  pub fn new_with_clock_and_trace(
    clock: Arc<dyn Clock>,
    config: AudioStreamConfig,
    trace: TraceHandle,
  ) -> Self {
    Self::new_with_clock_and_trace_and_max_buffered_duration(
      clock,
      config,
      trace,
      Duration::from_secs(2),
    )
  }

  fn new_with_clock_and_max_buffered_duration(
    clock: Arc<dyn Clock>,
    config: AudioStreamConfig,
    max_buffered_duration: Duration,
  ) -> Self {
    Self::new_with_clock_and_trace_and_max_buffered_duration(
      clock,
      config,
      TraceHandle::default(),
      max_buffered_duration,
    )
  }

  fn new_with_clock_and_trace_and_max_buffered_duration(
    clock: Arc<dyn Clock>,
    config: AudioStreamConfig,
    trace: TraceHandle,
    max_buffered_duration: Duration,
  ) -> Self {
    let config = AudioStreamConfig::new(
      config.sample_rate_hz.clamp(1, MAX_SAMPLE_RATE_HZ),
      config.channels.clamp(1, MAX_CHANNELS),
    );
    let max_buffered_duration = max_buffered_duration.min(MAX_BUFFERED_DURATION);
    let now = clock.now();
    Self {
      config,
      max_buffered_duration,
      estimated_output_latency: Duration::ZERO,
      clock,
      output_clock: Arc::new(InterpolatedAudioClock::new(config.sample_rate_hz)),
      dropped_samples: Arc::new(AtomicU64::new(0)),
      underrun_samples: Arc::new(AtomicU64::new(0)),
      state: Mutex::new(BackendState {
        last_clock_now: now,
        sinks: Vec::new(),
      }),
      trace,
    }
  }

  /// Construct a deterministic backend for unit tests.
  ///
  /// This uses a `VirtualClock` that remains fixed unless explicitly advanced, so the simulated
  /// playhead only advances when the test calls [`Self::render`] (or advances the injected clock and
  /// calls [`Self::pump`]).
  #[must_use]
  pub fn new_deterministic_with_defaults(sample_rate_hz: u32, channels: u16) -> Self {
    Self::new_deterministic_with_defaults_and_max_buffered_duration(
      sample_rate_hz,
      channels,
      Duration::from_secs(2),
    )
  }

  #[must_use]
  pub fn new_deterministic_with_defaults_and_max_buffered_duration(
    sample_rate_hz: u32,
    channels: u16,
    max_buffered_duration: Duration,
  ) -> Self {
    let sample_rate_hz = sample_rate_hz.clamp(1, MAX_SAMPLE_RATE_HZ);
    let channels = channels.clamp(1, MAX_CHANNELS);
    Self::new_with_clock_and_max_buffered_duration(
      Arc::new(VirtualClock::new()),
      AudioStreamConfig::new(sample_rate_hz, channels),
      max_buffered_duration,
    )
  }

  /// Convenience helper for creating a deterministic backend with the default 48kHz stereo config.
  #[must_use]
  pub fn new_deterministic() -> Self {
    Self::new_deterministic_with_defaults(48_000, 2)
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

  /// Advances the simulated output device playhead to the injected clock's current timestamp.
  ///
  /// No sleeping or background threads are used; callers are expected to drive time forward by
  /// calling this (or [`AudioBackend::clock`]).
  pub fn pump(&self) {
    let now = self.clock.now();
    let mut state = self.state.lock();
    let delta = now.saturating_sub(state.last_clock_now);
    let frames = duration_to_frames_floor(delta, self.config.sample_rate_hz);
    if frames == 0 {
      return;
    }

    self.consume_frames_locked(&mut state, frames, None);
    self.output_clock.advance_frames(frames);
    state.last_clock_now = state
      .last_clock_now
      .saturating_add(frames_to_duration_floor(frames, self.config.sample_rate_hz));
  }

  /// Renders `frames` audio frames into an interleaved `f32` buffer and advances the playhead.
  ///
  /// This is intended for tests that want to inspect the mixed output. When sinks underrun, the
  /// missing samples are rendered as silence and the playhead still advances.
  #[must_use]
  pub fn render(&self, frames: usize) -> Vec<f32> {
    let channels = self.channels_usize();
    let mut out = vec![0.0; frames.saturating_mul(channels)];
    self.render_into(&mut out);
    out
  }

  /// Mixes queued audio into `out` and advances the playhead by `out.len() / channels` frames.
  pub fn render_into(&self, out: &mut [f32]) {
    let channels = self.channels_usize();
    debug_assert!(
      channels > 0,
      "NullAudioBackend created with invalid channel count"
    );
    debug_assert!(
      out.len() % channels == 0,
      "output buffer must be a multiple of channel count"
    );

    out.fill(0.0);
    let frames = out.len() / channels;
    if frames == 0 {
      return;
    }

    let frames_u64 = u64::try_from(frames).unwrap_or(u64::MAX);
    let mut state = self.state.lock();

    if self.trace.is_enabled() {
      let mut callback_span = self.trace.try_span("audio.callback", "audio");
      if let Some(span) = callback_span.as_mut() {
        span.arg_u64("frames", frames_u64);
      }
      let mix_span = self.trace.try_span("audio.mix", "audio");
      self.consume_frames_locked(&mut state, frames_u64, Some(out));
      drop(mix_span);
      drop(callback_span);
    } else {
      self.consume_frames_locked(&mut state, frames_u64, Some(out));
    }

    self.output_clock.advance_frames(frames_u64);
    state.last_clock_now = state
      .last_clock_now
      .saturating_add(frames_to_duration_floor(
        frames_u64,
        self.config.sample_rate_hz,
      ));
  }

  #[must_use]
  pub fn dropped_samples(&self) -> u64 {
    self.dropped_samples.load(Ordering::Relaxed)
  }

  #[must_use]
  pub fn underrun_samples(&self) -> u64 {
    self.underrun_samples.load(Ordering::Relaxed)
  }

  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
  }

  fn consume_frames_locked(
    &self,
    state: &mut BackendState,
    frames: u64,
    mut out: Option<&mut [f32]>,
  ) {
    if frames == 0 {
      return;
    }

    let channels = self.channels_usize();
    if channels == 0 {
      return;
    }

    let frames_usize = usize::try_from(frames).unwrap_or(usize::MAX);
    let samples_requested = frames_usize.saturating_mul(channels);
    if let Some(ref out) = out {
      debug_assert_eq!(out.len(), samples_requested);
    }

    // Collect strong references and clean up dropped sinks.
    let sinks: Vec<Arc<SinkState>> = {
      let mut strong = Vec::with_capacity(state.sinks.len());
      state.sinks.retain(|weak| {
        if let Some(sink) = weak.upgrade() {
          strong.push(sink);
          true
        } else {
          false
        }
      });
      strong
    };

    for sink in sinks {
      sink.consume(samples_requested, out.as_deref_mut());
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

  fn output_info(&self) -> AudioOutputInfo {
    AudioOutputInfo {
      config: self.config,
      callback_frames: None,
      estimated_output_latency: self.estimated_output_latency,
      backend_name: "null",
    }
  }

  fn clock(&self) -> AudioClock {
    self.pump();
    AudioClock::OutputFrames {
      clock: self.output_clock.clone(),
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    let sink = Arc::new(SinkState::new(
      self.config,
      self.max_buffered_duration,
      self.dropped_samples.clone(),
      self.underrun_samples.clone(),
    ));

    let mut state = self.state.lock();
    state.sinks.retain(|weak| weak.upgrade().is_some());
    state.sinks.push(Arc::downgrade(&sink));

    Box::new(NullAudioSink { state: sink })
  }
}

struct SinkState {
  config: AudioStreamConfig,
  capacity_samples: usize,
  queue: Mutex<VecDeque<f32>>,
  volume_bits: AtomicU32,
  paused: AtomicBool,
  dropped_samples: AtomicU64,
  underrun_samples: AtomicU64,
  total_dropped_samples: Arc<AtomicU64>,
  total_underrun_samples: Arc<AtomicU64>,
}

impl std::fmt::Debug for SinkState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SinkState")
      .field("config", &self.config)
      .field("capacity_samples", &self.capacity_samples)
      .field("queue_len", &self.queue.lock().len())
      .field(
        "volume",
        &f32::from_bits(self.volume_bits.load(Ordering::Relaxed)),
      )
      .field(
        "dropped_samples",
        &self.dropped_samples.load(Ordering::Relaxed),
      )
      .field(
        "underrun_samples",
        &self.underrun_samples.load(Ordering::Relaxed),
      )
      .finish()
  }
}

impl SinkState {
  fn new(
    config: AudioStreamConfig,
    max_buffered_duration: Duration,
    total_dropped_samples: Arc<AtomicU64>,
    total_underrun_samples: Arc<AtomicU64>,
  ) -> Self {
    let channels = usize::from(config.channels.max(1));
    let max_buffered_duration = max_buffered_duration.min(MAX_BUFFERED_DURATION);
    let max_frames = duration_to_frames_ceil(config.sample_rate_hz, max_buffered_duration);
    let max_frames = usize::try_from(max_frames).unwrap_or(usize::MAX);
    let capacity_samples = max_frames.saturating_mul(channels).max(1);
    Self {
      config,
      capacity_samples,
      queue: Mutex::new(VecDeque::with_capacity(capacity_samples)),
      volume_bits: AtomicU32::new(1.0f32.to_bits()),
      paused: AtomicBool::new(false),
      dropped_samples: AtomicU64::new(0),
      underrun_samples: AtomicU64::new(0),
      total_dropped_samples,
      total_underrun_samples,
    }
  }

  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
  }

  fn set_volume(&self, volume: f32) {
    let volume = if volume.is_finite() {
      volume.clamp(0.0, 1.0)
    } else {
      0.0
    };
    self.volume_bits.store(volume.to_bits(), Ordering::Relaxed);
  }

  fn set_paused(&self, paused: bool) {
    self.paused.store(paused, Ordering::Relaxed);
  }

  fn flush(&self) {
    self.queue.lock().clear();
  }

  fn push(&self, samples: &[f32]) -> usize {
    let channels = self.channels_usize();
    let usable_len = samples.len() - (samples.len() % channels);
    if usable_len == 0 {
      return 0;
    }

    let mut queue = self.queue.lock();
    let free_samples = self.capacity_samples.saturating_sub(queue.len());
    let free_frames = free_samples / channels;
    let in_frames = (usable_len / channels).min(MAX_FRAMES_PER_PUSH);
    let accepted_frames = in_frames.min(free_frames);
    let accepted_samples = accepted_frames.saturating_mul(channels);
    if accepted_samples > 0 {
      queue.extend(&samples[..accepted_samples]);
    }

    let dropped = usable_len - accepted_samples;
    if dropped > 0 {
      let dropped_u64 = dropped as u64;
      self
        .dropped_samples
        .fetch_add(dropped_u64, Ordering::Relaxed);
      self
        .total_dropped_samples
        .fetch_add(dropped_u64, Ordering::Relaxed);
    }

    accepted_samples
  }

  fn consume(&self, samples_requested: usize, out: Option<&mut [f32]>) {
    if samples_requested == 0 {
      return;
    }

    if self.paused.load(Ordering::Relaxed) {
      return;
    }

    let gain = f32::from_bits(self.volume_bits.load(Ordering::Relaxed));

    let mut queue = self.queue.lock();
    let available = queue.len();
    let to_read = samples_requested.min(available);

    let underrun = samples_requested - to_read;
    if underrun > 0 {
      let underrun_u64 = underrun as u64;
      self
        .underrun_samples
        .fetch_add(underrun_u64, Ordering::Relaxed);
      self
        .total_underrun_samples
        .fetch_add(underrun_u64, Ordering::Relaxed);
    }

    if to_read == 0 {
      return;
    }

    match out {
      Some(out) if gain != 0.0 => {
        for (dst, sample) in out.iter_mut().take(to_read).zip(queue.drain(..to_read)) {
          *dst += sample * gain;
        }
      }
      _ => {
        // Either there is no output buffer (pump) or gain==0.
        let _ = queue.drain(..to_read);
      }
    }
  }
}

struct NullAudioSink {
  state: Arc<SinkState>,
}

impl std::fmt::Debug for NullAudioSink {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("NullAudioSink")
      .field("config", &self.state.config)
      .finish()
  }
}

impl AudioSink for NullAudioSink {
  fn config(&self) -> AudioStreamConfig {
    self.state.config
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    self.state.push(samples)
  }

  fn set_volume(&self, volume: f32) {
    self.state.set_volume(volume);
  }

  fn set_paused(&self, paused: bool) {
    self.state.set_paused(paused);
  }

  fn flush(&self) {
    self.state.flush();
  }
}

fn duration_to_frames_floor(duration: Duration, sample_rate_hz: u32) -> u64 {
  if sample_rate_hz == 0 {
    return 0;
  }
  let nanos = duration.as_nanos();
  let frames = nanos.saturating_mul(sample_rate_hz as u128) / 1_000_000_000u128;
  u64::try_from(frames).unwrap_or(u64::MAX)
}

fn frames_to_duration_floor(frames: u64, sample_rate_hz: u32) -> Duration {
  if sample_rate_hz == 0 {
    return Duration::ZERO;
  }
  let nanos = (frames as u128)
    .saturating_mul(1_000_000_000u128)
    .checked_div(sample_rate_hz as u128)
    .unwrap_or(0);
  Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::trace::TraceHandle;
  use crate::clock::VirtualClock;

  #[test]
  fn null_audio_backend_advancing_clock_advances_frames_played() {
    let clock = Arc::new(VirtualClock::new());
    let backend =
      NullAudioBackend::new_with_clock(clock.clone(), AudioStreamConfig::new(48_000, 2));

    backend.pump();
    assert_eq!(backend.clock().frames(), 0);

    clock.advance(Duration::from_secs(1));
    backend.pump();
    assert_eq!(backend.clock().frames(), 48_000);
  }

  #[test]
  fn null_audio_backend_time_does_not_advance_if_clock_does_not() {
    let clock = Arc::new(VirtualClock::new());
    let backend = NullAudioBackend::new_with_clock(clock, AudioStreamConfig::new(48_000, 2));

    backend.pump();
    let frames0 = backend.clock().frames();
    backend.pump();
    let frames1 = backend.clock().frames();
    assert_eq!(frames0, frames1);
  }

  #[test]
  fn null_audio_backend_render_consumes_samples_and_renders_silence_on_underrun() {
    let clock = Arc::new(VirtualClock::new());
    let backend = NullAudioBackend::new_with_clock(clock, AudioStreamConfig::new(48_000, 1));
    let sink = backend.create_sink();

    assert_eq!(sink.push_interleaved_f32(&[1.0, 2.0, 3.0]), 3);
    let out0 = backend.render(5);
    assert_eq!(out0, vec![1.0, 2.0, 3.0, 0.0, 0.0]);
    assert_eq!(backend.clock().frames(), 5);
    assert_eq!(backend.underrun_samples(), 2);

    // Queue should have been drained.
    let out1 = backend.render(1);
    assert_eq!(out1, vec![0.0]);
    assert_eq!(backend.clock().frames(), 6);
  }

  #[test]
  fn trace_null_audio_backend_mix_records_events_and_respects_cap() {
    let max_events = 12;
    let trace = TraceHandle::enabled_with_max_events(max_events);

    let clock = Arc::new(VirtualClock::new());
    let backend = NullAudioBackend::new_with_clock_and_trace(
      clock,
      AudioStreamConfig::new(48_000, 2),
      trace.clone(),
    );

    let sink = backend.create_sink();
    // Queue some audio so the mix path has work to do.
    let samples = vec![1.0f32; 48_000 * 2];
    assert_eq!(sink.push_interleaved_f32(&samples), samples.len());

    let callbacks = 32usize;
    for _ in 0..callbacks {
      let _ = backend.render(240);
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("trace.json");
    trace.write_chrome_trace(&path).expect("write trace");

    let json = std::fs::read_to_string(&path).expect("read trace");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse trace json");

    let trace_events = value["traceEvents"].as_array().expect("traceEvents array");
    assert_eq!(trace_events.len(), max_events);

    let names: Vec<&str> = trace_events
      .iter()
      .filter_map(|event| event["name"].as_str())
      .collect();
    assert!(
      names.iter().any(|name| *name == "audio.callback"),
      "expected audio.callback span in trace"
    );
    assert!(
      names.iter().any(|name| *name == "audio.mix"),
      "expected audio.mix span in trace"
    );

    let generated_events = callbacks * 2;
    assert_eq!(
      value["fastrenderTraceDroppedEvents"]
        .as_u64()
        .expect("dropped events metadata"),
      (generated_events - max_events) as u64
    );
  }

  #[test]
  fn null_audio_backend_respects_max_buffered_duration() {
    let clock = Arc::new(VirtualClock::new());
    let backend = NullAudioBackend::new_with_clock_and_max_buffered_duration(
      clock,
      AudioStreamConfig::new(10, 1),
      Duration::from_millis(300),
    );
    let sink = backend.create_sink();

    let samples = vec![1.0f32; 10];
    let accepted = sink.push_interleaved_f32(&samples);
    assert_eq!(accepted, 3);
    assert_eq!(backend.dropped_samples(), 7);
  }
}
