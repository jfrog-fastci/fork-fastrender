//! Audio stream helpers for feeding decoded PCM into an [`AudioSink`].
//!
//! # Playback rate support (MVP)
//!
//! `HTMLMediaElement.playbackRate` needs to affect what the user hears. For now we implement this
//! as naive resampling (speed + pitch shift) by treating `playbackRate` as an extra sample-rate
//! multiplier:
//!
//! - The output sink/device runs at a fixed sample rate (e.g. 48 kHz).
//! - If `playbackRate = 2.0`, we resample as if the decoded audio had a 2× higher sample rate, so
//!   the same media segment is represented by ~1/2 as many output frames.
//!
//! This is *not* time-stretching: pitch changes with speed. The resampler is intentionally simple
//! (linear interpolation) for an MVP.

use super::resample::resample_interleaved_f32_linear_with_playback_rate;
use super::{AudioSink, AudioStreamConfig};
use crate::debug::trace::TraceHandle;
use crate::media::clock::{AudioDeviceClock, AudioStreamClock, MediaClock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AudioStreamError {
  #[error("decoded sample buffer length {len} is not a multiple of channel count {channels}")]
  InvalidInterleavedSampleCount { len: usize, channels: usize },
  #[error(
    "decoded channel count {decoded_channels} does not match sink channel count {sink_channels}"
  )]
  ChannelCountMismatch {
    decoded_channels: u16,
    sink_channels: u16,
  },
  #[error("decoded sample rate must be non-zero")]
  InvalidDecodedSampleRate,
}

fn sanitize_playback_rate(rate: f64) -> f64 {
  if rate.is_finite() && rate > 0.0 {
    rate
  } else {
    0.0
  }
}

#[derive(Clone)]
pub struct AudioStreamHandle {
  inner: Arc<AudioStreamInner>,
}

struct AudioStreamInner {
  decoded_config: AudioStreamConfig,
  sink: Arc<dyn AudioSink>,
  playback_rate_bits: AtomicU64,
  clock: AudioStreamClock,
  trace: TraceHandle,
}

impl AudioStreamHandle {
  /// Creates a new audio stream for decoded PCM described by `decoded_config`.
  ///
  /// The stream will resample to the sink's output format and drive an [`AudioStreamClock`] derived
  /// from `device_clock`.
  pub fn new(
    decoded_config: AudioStreamConfig,
    sink: Arc<dyn AudioSink>,
    device_clock: Arc<AudioDeviceClock>,
    start_media_time: Duration,
  ) -> Result<Self, AudioStreamError> {
    Self::new_with_trace(
      decoded_config,
      sink,
      device_clock,
      start_media_time,
      TraceHandle::default(),
    )
  }

  /// Like [`Self::new`], but installs a [`TraceHandle`] used to profile resampling work.
  pub fn new_with_trace(
    decoded_config: AudioStreamConfig,
    sink: Arc<dyn AudioSink>,
    device_clock: Arc<AudioDeviceClock>,
    start_media_time: Duration,
    trace: TraceHandle,
  ) -> Result<Self, AudioStreamError> {
    if decoded_config.sample_rate_hz == 0 {
      return Err(AudioStreamError::InvalidDecodedSampleRate);
    }

    let sink_cfg = sink.config();
    if sink_cfg.channels != decoded_config.channels {
      return Err(AudioStreamError::ChannelCountMismatch {
        decoded_channels: decoded_config.channels,
        sink_channels: sink_cfg.channels,
      });
    }

    let clock = AudioStreamClock::new(device_clock, start_media_time);

    Ok(Self {
      inner: Arc::new(AudioStreamInner {
        decoded_config,
        sink,
        playback_rate_bits: AtomicU64::new(1.0_f64.to_bits()),
        clock,
        trace,
      }),
    })
  }

  #[must_use]
  pub fn clock(&self) -> &AudioStreamClock {
    &self.inner.clock
  }

  #[must_use]
  pub fn current_time(&self) -> Duration {
    self.inner.clock.now()
  }

  #[must_use]
  pub fn playback_rate(&self) -> f64 {
    f64::from_bits(self.inner.playback_rate_bits.load(Ordering::Relaxed))
  }

  /// Sets the playback rate for both the resampler and the stream clock.
  pub fn set_playback_rate(&self, rate: f64) {
    let rate = sanitize_playback_rate(rate);
    self
      .inner
      .playback_rate_bits
      .store(rate.to_bits(), Ordering::Relaxed);
    self.inner.clock.set_rate(rate);
  }

  /// Resets the stream clock mapping.
  ///
  /// Note: this handle is stateless (per-call resampling), so there is no interpolation state to
  /// flush beyond updating the clock.
  pub fn seek(&self, new_media_time: Duration) {
    self.inner.clock.seek(new_media_time);
  }

  /// Push decoded interleaved f32 PCM into the stream.
  ///
  /// The input must match `decoded_config` provided at construction time.
  pub fn push_interleaved_f32(&self, decoded_samples: &[f32]) -> Result<usize, AudioStreamError> {
    let channels = usize::from(self.inner.decoded_config.channels.max(1));
    if decoded_samples.len() % channels != 0 {
      return Err(AudioStreamError::InvalidInterleavedSampleCount {
        len: decoded_samples.len(),
        channels,
      });
    }

    let rate = self.playback_rate();
    if rate == 0.0 {
      return Ok(0);
    }

    let sink_cfg = self.inner.sink.config();
    let in_rate_hz = self.inner.decoded_config.sample_rate_hz;
    let out_rate_hz = sink_cfg.sample_rate_hz;

    // Fast path: no resampling needed.
    if rate == 1.0 && in_rate_hz == out_rate_hz {
      return Ok(self.inner.sink.push_interleaved_f32(decoded_samples));
    }

    let input_frames = decoded_samples.len() / channels;
    if input_frames == 0 {
      return Ok(0);
    }

    let step = (in_rate_hz as f64) * rate / (out_rate_hz as f64);
    if !(step.is_finite()) || step <= 0.0 {
      return Ok(0);
    }

    // Compute the maximum number of output frames such that the final interpolation does not need
    // to read beyond the last input frame (`idx + 1`).
    let output_frames = if input_frames <= 1 {
      1
    } else {
      (((input_frames - 1) as f64) / step).floor().max(0.0) as usize + 1
    };

    let trace_enabled = self.inner.trace.is_enabled();
    let mut resample_span = if trace_enabled {
      let mut span = self.inner.trace.span("audio.resample", "audio");
      span.arg_u64("input_frames", input_frames as u64);
      span.arg_u64("output_frames", output_frames as u64);
      span.arg_u64("channels", channels as u64);
      span.arg_u64("input_rate_hz", in_rate_hz as u64);
      span.arg_u64("output_rate_hz", out_rate_hz as u64);
      // Avoid float args; encode the playback rate as milli-units for easy inspection.
      let rate_milli = (rate * 1000.0).round();
      if rate_milli.is_finite() && rate_milli >= 0.0 {
        span.arg_u64("playback_rate_milli", rate_milli as u64);
      }
      Some(span)
    } else {
      None
    };

    let out = resample_interleaved_f32_linear_with_playback_rate(
      decoded_samples,
      channels,
      in_rate_hz,
      out_rate_hz,
      rate,
      output_frames,
    );

    drop(resample_span);

    Ok(self.inner.sink.push_interleaved_f32(&out))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use parking_lot::Mutex;
  use std::sync::atomic::{AtomicU64, Ordering};

  #[derive(Debug)]
  struct FakeSink {
    config: AudioStreamConfig,
    samples: Mutex<Vec<f32>>,
    frames_played: Arc<AtomicU64>,
  }

  impl FakeSink {
    fn take_samples(&self) -> Vec<f32> {
      std::mem::take(&mut *self.samples.lock())
    }
  }

  impl AudioSink for FakeSink {
    fn config(&self) -> AudioStreamConfig {
      self.config
    }

    fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
      self.samples.lock().extend_from_slice(samples);
      let channels = usize::from(self.config.channels.max(1));
      let frames = (samples.len() / channels) as u64;
      self.frames_played.fetch_add(frames, Ordering::Relaxed);
      samples.len()
    }

    fn set_volume(&self, _volume: f32) {}
  }

  #[derive(Debug)]
  struct FramesDeviceClock {
    frames_played: Arc<AtomicU64>,
    sample_rate_hz: u32,
  }

  impl MediaClock for FramesDeviceClock {
    fn now(&self) -> Duration {
      let frames = self.frames_played.load(Ordering::Relaxed);
      if self.sample_rate_hz == 0 {
        return Duration::ZERO;
      }
      let nanos = (frames as u128)
        .saturating_mul(1_000_000_000u128)
        .checked_div(self.sample_rate_hz as u128)
        .unwrap_or(0);
      Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
  }

  #[test]
  fn playback_rate_gt_1_shrinks_output_and_advances_clock_faster() {
    let frames_played = Arc::new(AtomicU64::new(0));
    let sink = Arc::new(FakeSink {
      config: AudioStreamConfig::new(48_000, 1),
      samples: Mutex::new(Vec::new()),
      frames_played: frames_played.clone(),
    });
    let sink_dyn: Arc<dyn AudioSink> = sink.clone();

    let device_clock: Arc<AudioDeviceClock> = Arc::new(FramesDeviceClock {
      frames_played: frames_played.clone(),
      sample_rate_hz: 48_000,
    });

    let decoded_cfg = AudioStreamConfig::new(48_000, 1);
    let stream =
      AudioStreamHandle::new(decoded_cfg, sink_dyn, device_clock, Duration::ZERO).unwrap();
    stream.set_playback_rate(2.0);

    // 2 seconds of decoded audio at 48 kHz.
    let decoded: Vec<f32> = (0..96_000).map(|i| i as f32).collect();
    stream.push_interleaved_f32(&decoded).unwrap();

    let out = sink.take_samples();
    // playbackRate=2 means we should output half as many device samples/frames.
    assert_eq!(out.len(), 48_000);
    assert_eq!(out[0], 0.0);
    assert_eq!(out[1], 2.0);
    assert_eq!(out[2], 4.0);
    assert_eq!(out[3], 6.0);

    // Device played 1 second (48k frames); media time should report 2 seconds at rate 2.
    assert_eq!(stream.current_time(), Duration::from_secs(2));
  }

  #[test]
  fn resample_emits_audio_resample_trace_event() {
    let trace = TraceHandle::enabled_with_max_events(16);

    let frames_played = Arc::new(AtomicU64::new(0));
    let sink = Arc::new(FakeSink {
      config: AudioStreamConfig::new(48_000, 1),
      samples: Mutex::new(Vec::new()),
      frames_played: frames_played.clone(),
    });
    let sink_dyn: Arc<dyn AudioSink> = sink.clone();

    let device_clock: Arc<AudioDeviceClock> = Arc::new(FramesDeviceClock {
      frames_played: frames_played.clone(),
      sample_rate_hz: 48_000,
    });

    // Force resampling by using a different decoded sample rate than the sink.
    let decoded_cfg = AudioStreamConfig::new(44_100, 1);
    let stream = AudioStreamHandle::new_with_trace(
      decoded_cfg,
      sink_dyn,
      device_clock,
      Duration::ZERO,
      trace.clone(),
    )
    .unwrap();

    let decoded: Vec<f32> = (0..44_100).map(|i| i as f32).collect();
    stream.push_interleaved_f32(&decoded).unwrap();

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("trace.json");
    trace.write_chrome_trace(&path).expect("write trace");
    let json = std::fs::read_to_string(&path).expect("read trace");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse trace json");

    let trace_events = value["traceEvents"]
      .as_array()
      .expect("traceEvents array");
    let names: Vec<&str> = trace_events
      .iter()
      .filter_map(|event| event["name"].as_str())
      .collect();
    assert!(
      names.iter().any(|name| *name == "audio.resample"),
      "expected audio.resample span in trace"
    );
  }
}
