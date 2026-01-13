use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use super::{
  frames_to_duration, next_device_id_for_name, AudioBackend, AudioClock, AudioDeviceInfo,
  AudioEngineConfig, AudioError, AudioOutputInfo, AudioSink, AudioStreamConfig, DeviceSelector,
};
use super::convert::sanitize_sample;
use crate::media::audio_clock::InterpolatedAudioClock;
use super::limits::{MAX_BUFFERED_DURATION, MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};
use crate::media::audio::ring_buffer::{AudioRingBuffer, GainRamp};
use cpal::traits::{HostTrait, StreamTrait};

pub fn list_output_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
  use cpal::traits::DeviceTrait;

  let host = cpal::default_host();
  let devices = host
    .output_devices()
    .map_err(|err| AudioError::OutputDeviceEnumerationFailed(err.to_string()))?;

  let mut seen = std::collections::HashMap::<String, u32>::new();
  let mut out = Vec::new();
  for device in devices {
    let Ok(name) = device.name() else {
      continue;
    };
    let id = next_device_id_for_name(&mut seen, &name);
    out.push(AudioDeviceInfo { id, name });
  }
  Ok(out)
}

fn select_output_device(host: &cpal::Host, selector: &DeviceSelector) -> Result<cpal::Device, AudioError> {
  use cpal::traits::DeviceTrait;

  match selector {
    DeviceSelector::Default => host
      .default_output_device()
      .ok_or(AudioError::NoOutputDevice),
    DeviceSelector::Device(target) => {
      let devices = host
        .output_devices()
        .map_err(|err| AudioError::OutputDeviceEnumerationFailed(err.to_string()))?;
      let mut seen = std::collections::HashMap::<String, u32>::new();
      for device in devices {
        let Ok(name) = device.name() else {
          continue;
        };
        let id = next_device_id_for_name(&mut seen, &name);
        if &id == target {
          return Ok(device);
        }
      }
      Err(AudioError::OutputDeviceNotFound {
        selector: selector.clone(),
      })
    }
  }
}

/// CPAL-based audio output backend (cross-platform).
///
/// Clocking notes:
/// - The exposed `AudioClock::OutputFrames` is derived from the number of frames written into the
///   CPAL output callback.
/// - This is a best-effort clock and does not currently model backend/device output latency, so it
///   may be ahead of “what the user hears” by a roughly constant buffer duration.
///
/// See `docs/media_clocking.md` for the intended A/V sync model (audio as master clock, tick as
/// wake-up only).

const DEFAULT_GAIN_RAMP_DURATION_MS: u32 = 10;

fn gain_ramp_frames(sample_rate_hz: u32) -> u32 {
  let frames = (u64::from(sample_rate_hz).saturating_mul(u64::from(DEFAULT_GAIN_RAMP_DURATION_MS))
    / 1000) as u32;
  frames.max(1)
}
pub struct CpalAudioBackend {
  config: AudioStreamConfig,
  max_buffered_duration: Duration,
  fixed_callback_frames: Option<u32>,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  // `cpal::Stream` is neither `Send` nor `Sync`, so it cannot live inside a `Send + Sync`
  // `AudioBackend` implementation. Keep the stream on a dedicated thread and control its lifetime
  // via a shutdown channel + join handle.
  shutdown_tx: std::sync::mpsc::Sender<()>,
  stream_thread: Mutex<Option<JoinHandle<()>>>,
}

impl CpalAudioBackend {
  pub fn new() -> Result<Self, AudioError> {
    Self::new_with_device(DeviceSelector::Default)
  }

  pub fn new_with_device(selector: DeviceSelector) -> Result<Self, AudioError> {
    Self::new_with_config_and_device(&super::audio_engine_config(), selector)
  }

  pub fn new_with_config(engine_cfg: &AudioEngineConfig) -> Result<Self, AudioError> {
    Self::new_with_config_and_device(engine_cfg, DeviceSelector::Default)
  }

  fn new_with_config_and_device(
    engine_cfg: &AudioEngineConfig,
    selector: DeviceSelector,
  ) -> Result<Self, AudioError> {
    // This comes from process-wide configuration (env vars), so clamp it defensively. The queue and
    // sink buffers must never be able to allocate unbounded memory.
    let max_buffered_duration = engine_cfg
      .per_stream_max_buffered_duration
      .min(MAX_BUFFERED_DURATION);

    // `cpal::Stream` is not `Send`/`Sync`, so it cannot live inside a `Send + Sync`
    // `AudioBackend` implementation. Keep the stream on a dedicated thread and control its
    // lifetime via a shutdown channel + join handle.
    type ReadyState = (
      AudioStreamConfig,
      Option<u32>,
      Arc<AtomicU32>,
      Arc<AtomicU64>,
      Arc<MixerState>,
      Arc<InterpolatedAudioClock>,
    );
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<ReadyState, AudioError>>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let thread = std::thread::spawn(move || {
      let selector = selector;
      let init = (|| -> Result<(ReadyState, cpal::Stream), AudioError> {
        let host = cpal::default_host();
        let device = select_output_device(&host, &selector)?;

        let (stream_config, sample_format) = select_output_stream_config(&device)?;
        let config = AudioStreamConfig::new(stream_config.sample_rate.0, stream_config.channels);
        let fixed_callback_frames = match stream_config.buffer_size {
          cpal::BufferSize::Fixed(frames) => Some(frames),
          cpal::BufferSize::Default => None,
        };
        let last_callback_frames = Arc::new(AtomicU32::new(0));

        // Start with a conservative estimate; the callback will refine this using timestamps (when
        // available) or observed callback sizes.
        let initial_latency = fixed_callback_frames
          .map(|frames| frames_to_duration(config.sample_rate_hz, frames as u64))
          .unwrap_or_else(|| frames_to_duration(config.sample_rate_hz, 1024));
        let estimated_latency_nanos =
          Arc::new(AtomicU64::new(duration_to_nanos_u64(initial_latency)));

        let clock = Arc::new(InterpolatedAudioClock::new(config.sample_rate_hz));
        let mixer = Arc::new(MixerState::new(config));

        let stream = build_stream(
          &device,
          &stream_config,
          sample_format,
          mixer.clone(),
          clock.clone(),
          fixed_callback_frames,
          last_callback_frames.clone(),
          estimated_latency_nanos.clone(),
        )?;
        stream
          .play()
          .map_err(|err| AudioError::StreamPlayFailed(err.to_string()))?;

        Ok((
          (
            config,
            fixed_callback_frames,
            last_callback_frames,
            estimated_latency_nanos,
            mixer,
            clock,
          ),
          stream,
        ))
      })();

      let (ready, _stream) = match init {
        Ok(ok) => ok,
        Err(err) => {
          let _ = ready_tx.send(Err(err));
          return;
        }
      };

      let _ = ready_tx.send(Ok(ready));
      // Keep the stream alive until shutdown is requested.
      let _ = shutdown_rx.recv();
      drop(_stream);
    });

    let (
      config,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      mixer,
      clock,
    ) = match ready_rx.recv() {
      Ok(Ok(ok)) => ok,
      Ok(Err(err)) => {
        let _ = thread.join();
        return Err(err);
      }
      Err(_) => {
        let _ = thread.join();
        return Err(AudioError::StreamBuildFailed(
          "cpal audio thread terminated unexpectedly".to_string(),
        ));
      }
    };

    Ok(Self {
      config,
      max_buffered_duration,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      mixer,
      clock,
      shutdown_tx,
      stream_thread: Mutex::new(Some(thread)),
    })
  }
}

impl Drop for CpalAudioBackend {
  fn drop(&mut self) {
    let _ = self.shutdown_tx.send(());
    if let Some(handle) = self.stream_thread.lock().take() {
      // Avoid panicking if the backend is dropped on its own stream thread.
      if handle.thread().id() != std::thread::current().id() {
        let _ = handle.join();
      }
    }
  }
}

impl AudioBackend for CpalAudioBackend {
  fn output_config(&self) -> AudioStreamConfig {
    self.config
  }

  fn output_info(&self) -> AudioOutputInfo {
    let callback_frames = self.fixed_callback_frames.or_else(|| match self
      .last_callback_frames
      .load(Ordering::Relaxed)
    {
      0 => None,
      v => Some(v),
    });

    AudioOutputInfo {
      config: self.config,
      callback_frames,
      estimated_output_latency: Duration::from_nanos(
        self.estimated_latency_nanos.load(Ordering::Relaxed),
      ),
      backend_name: "cpal",
    }
  }

  fn clock(&self) -> AudioClock {
    AudioClock::OutputFrames {
      clock: self.clock.clone(),
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    let sink = Arc::new(SinkState::new(self.config, self.max_buffered_duration));
    self.mixer.register_sink(&sink);
    Box::new(CpalAudioSink { state: sink })
  }
}

struct MixerState {
  config: AudioStreamConfig,
  sinks: RwLock<Vec<Weak<SinkState>>>,
}

impl MixerState {
  fn new(config: AudioStreamConfig) -> Self {
    Self {
      config,
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
      sink.mix_into(dst);
    }
  }

  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
  }
}

struct SinkState {
  config: AudioStreamConfig,
  buffer: AudioRingBuffer,
  volume_target_bits: AtomicU32,
  ramp_target_bits: AtomicU32,
  ramp_current_bits: AtomicU32,
  ramp_step_bits: AtomicU32,
  ramp_remaining_frames: AtomicU32,
  ramp_frames: u32,
}

impl SinkState {
  fn new(config: AudioStreamConfig, max_buffered: Duration) -> Self {
    let channels = usize::from(config.channels.max(1));
    let frames = super::duration_to_frames_ceil(config.sample_rate_hz, max_buffered);
    let frames = usize::try_from(frames).unwrap_or(usize::MAX);
    let capacity = frames.saturating_mul(channels).max(1);
    let ramp_frames = gain_ramp_frames(config.sample_rate_hz);
    Self {
      config,
      buffer: AudioRingBuffer::new(capacity),
      volume_target_bits: AtomicU32::new(1.0f32.to_bits()),
      ramp_target_bits: AtomicU32::new(1.0f32.to_bits()),
      ramp_current_bits: AtomicU32::new(1.0f32.to_bits()),
      ramp_step_bits: AtomicU32::new(0.0f32.to_bits()),
      ramp_remaining_frames: AtomicU32::new(0),
      ramp_frames,
    }
  }

  fn set_volume(&self, volume: f32) {
    let volume = if volume.is_finite() {
      volume.clamp(0.0, 1.0)
    } else {
      0.0
    };
    self
      .volume_target_bits
      .store(volume.to_bits(), Ordering::Relaxed);
  }

  fn mix_into(&self, dst: &mut [f32]) {
    let channels = usize::from(self.config.channels.max(1));
    if channels == 0 || dst.is_empty() {
      return;
    }

    let desired_target_bits = self.volume_target_bits.load(Ordering::Relaxed);
    let mut ramp_target_bits = self.ramp_target_bits.load(Ordering::Relaxed);

    let mut current = f32::from_bits(self.ramp_current_bits.load(Ordering::Relaxed));
    let mut target = f32::from_bits(ramp_target_bits);
    let mut step = f32::from_bits(self.ramp_step_bits.load(Ordering::Relaxed));
    let mut remaining = self.ramp_remaining_frames.load(Ordering::Relaxed);

    if desired_target_bits != ramp_target_bits {
      ramp_target_bits = desired_target_bits;
      target = f32::from_bits(desired_target_bits);

      if (current - target).abs() <= f32::EPSILON {
        current = target;
        step = 0.0;
        remaining = 0;
      } else {
        remaining = self.ramp_frames;
        step = (target - current) / remaining as f32;
      }
    }

    let mut ramp = GainRamp {
      current_gain: current,
      target_gain: target,
      step,
      frames_remaining: remaining,
    };

    self.buffer.pop_add_into_ramped(dst, channels, &mut ramp);

    self
      .ramp_target_bits
      .store(ramp_target_bits, Ordering::Relaxed);
    self
      .ramp_current_bits
      .store(ramp.current_gain.to_bits(), Ordering::Relaxed);
    self
      .ramp_step_bits
      .store(ramp.step.to_bits(), Ordering::Relaxed);
    self
      .ramp_remaining_frames
      .store(ramp.frames_remaining, Ordering::Relaxed);
  }
}

struct CpalAudioSink {
  state: Arc<SinkState>,
}

impl AudioSink for CpalAudioSink {
  fn config(&self) -> AudioStreamConfig {
    self.state.config
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    let channels = usize::from(self.state.config.channels.max(1));
    let usable_len = samples.len() - (samples.len() % channels);
    let frames = usable_len / channels;
    let frames = frames.min(MAX_FRAMES_PER_PUSH);
    let capped_len = frames * channels;
    self.state.buffer.push(&samples[..capped_len])
  }

  fn set_volume(&self, volume: f32) {
    self.state.set_volume(volume);
  }
}

fn select_output_stream_config(
  device: &cpal::Device,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), AudioError> {
  use cpal::traits::DeviceTrait;

  let mut best: Option<(cpal::SupportedStreamConfig, (u8, u8, u8))> = None;

  if let Ok(configs) = device.supported_output_configs() {
    for range in configs {
      let fmt_score = match range.sample_format() {
        cpal::SampleFormat::F32 => 3,
        cpal::SampleFormat::I16 => 2,
        cpal::SampleFormat::U16 => 1,
        _ => 0,
      };
      if fmt_score == 0 {
        continue;
      }

      let channels = range.channels();
      if channels == 0 || channels > MAX_CHANNELS {
        continue;
      }
      let channel_score = match channels {
        2 => 2,
        1 => 1,
        _ => 0,
      };

      let min_rate = range.min_sample_rate().0;
      let max_rate = range.max_sample_rate().0;
      let chosen_rate = if min_rate <= 48_000 && 48_000 <= max_rate {
        48_000
      } else if min_rate <= 44_100 && 44_100 <= max_rate {
        44_100
      } else {
        let capped_max = max_rate.min(MAX_SAMPLE_RATE_HZ);
        if capped_max < min_rate {
          continue;
        }
        capped_max
      };
      if chosen_rate == 0 {
        continue;
      }
      let rate_score = if chosen_rate == 48_000 {
        2
      } else if chosen_rate == 44_100 {
        1
      } else {
        0
      };

      let cfg = range.with_sample_rate(cpal::SampleRate(chosen_rate));
      let score = (fmt_score, channel_score, rate_score);

      match best.as_ref() {
        Some((_, best_score)) if *best_score >= score => {}
        _ => best = Some((cfg, score)),
      }
    }
  }

  let supported = if let Some((cfg, _)) = best {
    cfg
  } else {
    device
      .default_output_config()
      .map_err(|err| AudioError::DefaultOutputConfigFailed(err.to_string()))?
  };

  let sample_format = supported.sample_format();
  let config: cpal::StreamConfig = supported.into();
  if config.channels == 0 || config.channels > MAX_CHANNELS {
    return Err(AudioError::invalid_spec(format!(
      "unsupported output channel count {}",
      config.channels
    )));
  }
  if config.sample_rate.0 == 0 || config.sample_rate.0 > MAX_SAMPLE_RATE_HZ {
    return Err(AudioError::invalid_spec(format!(
      "unsupported output sample rate {}",
      config.sample_rate.0
    )));
  }
  Ok((config, sample_format))
}

fn build_stream(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  sample_format: cpal::SampleFormat,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  fixed_callback_frames: Option<u32>,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
) -> Result<cpal::Stream, AudioError> {
  match sample_format {
    cpal::SampleFormat::F32 => build_stream_typed::<f32>(
      device,
      config,
      mixer,
      clock,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
    ),
    cpal::SampleFormat::I16 => build_stream_typed::<i16>(
      device,
      config,
      mixer,
      clock,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
    ),
    cpal::SampleFormat::U16 => build_stream_typed::<u16>(
      device,
      config,
      mixer,
      clock,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
    ),
    other => Err(AudioError::UnsupportedSampleFormat(format!("{other:?}"))),
  }
}

fn build_stream_typed<T>(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  fixed_callback_frames: Option<u32>,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
) -> Result<cpal::Stream, AudioError>
where
  T: OutputSample + cpal::SizedSample,
{
  use cpal::traits::DeviceTrait;

  let channels = mixer.channels_usize();
  let mut mix_buf: Vec<f32> = Vec::new();
  let mut playback_origin = None;
  let sample_rate_hz = mixer.config.sample_rate_hz;

  let err_cb = |err| {
    eprintln!("warning: CPAL output stream error: {err}");
  };

  let stream = device
    .build_output_stream(
      config,
      move |output: &mut [T], info| {
        super::thread_priority::promote_current_thread_for_audio();
        let ts = info.timestamp();
        let latency = ts.playback.duration_since(&ts.callback);
        if mix_buf.len() < output.len() {
          mix_buf.resize(output.len(), 0.0);
        }
        let mix = &mut mix_buf[..output.len()];
        mix.fill(0.0);

        mixer.mix_into(mix);

        for (out, sample) in output.iter_mut().zip(mix.iter()) {
          *out = T::from_mixed_f32(*sample);
        }

        if channels != 0 {
          let frames = (output.len() / channels) as u64;
          let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
          last_callback_frames.store(frames_u32, Ordering::Relaxed);
          let callback_end = Instant::now();

          // Prefer CPAL's device timestamps (when monotonic) as the base time, falling back to a
          // pure frame counter when unavailable.
          let device_time_at_end = {
            let playback = ts.playback;
            let buffer_duration = frames_to_duration(sample_rate_hz, frames);

            match playback_origin.as_ref() {
              Some(origin) => playback
                .duration_since(origin)
                .map(|since_origin| since_origin.saturating_add(buffer_duration)),
              None => {
                playback_origin = Some(playback);
                Some(buffer_duration)
              }
            }
          };

          clock.on_callback_end_at(callback_end, frames_u32, device_time_at_end);
        }

        // Best-effort latency estimate:
        // - prefer CPAL timestamps when available (callback vs playback instant),
        // - otherwise fall back to observed callback buffer size (only when buffer size isn't fixed).
        if let Some(latency) = latency {
          estimated_latency_nanos.store(duration_to_nanos_u64(latency), Ordering::Relaxed);
        } else if fixed_callback_frames.is_none() && channels != 0 {
          let frames = (output.len() / channels) as u64;
          let latency = frames_to_duration(sample_rate_hz, frames);
          estimated_latency_nanos.store(duration_to_nanos_u64(latency), Ordering::Relaxed);
        }
      },
      err_cb,
      None,
    )
    .map_err(|err| AudioError::StreamBuildFailed(err.to_string()))?;

  Ok(stream)
}

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn f32_to_i16(value: f32) -> i16 {
  let value = sanitize_sample(value);
  (value * i16::MAX as f32) as i16
}

fn f32_to_u16(value: f32) -> u16 {
  let value = sanitize_sample(value);
  let shifted = value * 0.5 + 0.5;
  (shifted * u16::MAX as f32) as u16
}

trait OutputSample: cpal::Sample + cpal::SizedSample {
  fn from_mixed_f32(value: f32) -> Self;
}

impl OutputSample for f32 {
  fn from_mixed_f32(value: f32) -> Self {
    sanitize_sample(value)
  }
}

impl OutputSample for i16 {
  fn from_mixed_f32(value: f32) -> Self {
    f32_to_i16(value)
  }
}

impl OutputSample for u16 {
  fn from_mixed_f32(value: f32) -> Self {
    f32_to_u16(value)
  }
}

#[cfg(test)]
mod tests {
  use super::{f32_to_i16, f32_to_u16};
  use crate::media::audio::convert::sanitize_sample;

  #[test]
  fn sanitize_handles_nan_and_clamps() {
    assert_eq!(sanitize_sample(f32::NAN), 0.0);
    assert_eq!(sanitize_sample(2.0), 1.0);
    assert_eq!(sanitize_sample(-2.0), -1.0);
  }

  #[test]
  fn converts_f32_to_i16() {
    assert_eq!(f32_to_i16(0.0), 0);
    assert_eq!(f32_to_i16(1.0), i16::MAX);
    assert_eq!(f32_to_i16(-1.0), -i16::MAX);
  }

  #[test]
  fn converts_f32_to_u16() {
    assert_eq!(f32_to_u16(0.0), u16::MAX / 2);
    assert_eq!(f32_to_u16(1.0), u16::MAX);
    assert_eq!(f32_to_u16(-1.0), 0);
  }
}
