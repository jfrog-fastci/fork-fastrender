use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::{Mutex, RwLock};

use super::{AudioBackend, AudioClock, AudioError, AudioSink, AudioStreamConfig};
use crate::media::audio::ring_buffer::AudioRingBuffer;
use cpal::traits::{HostTrait, StreamTrait};

pub struct CpalAudioBackend {
  config: AudioStreamConfig,
  mixer: Arc<MixerState>,
  frames_played: Arc<AtomicU64>,
  // `cpal::Stream` is not guaranteed to be `Sync` across all platforms; keep it behind a mutex so
  // the backend can be shared (`dyn AudioBackend: Sync`) while still keeping the stream alive.
  _stream: Mutex<cpal::Stream>,
}

impl CpalAudioBackend {
  pub fn new() -> Result<Self, AudioError> {
    let host = cpal::default_host();
    let device = host
      .default_output_device()
      .ok_or(AudioError::NoOutputDevice)?;

    let (stream_config, sample_format) = select_output_stream_config(&device)?;
    let config = AudioStreamConfig::new(stream_config.sample_rate.0, stream_config.channels);
    let frames_played = Arc::new(AtomicU64::new(0));
    let mixer = Arc::new(MixerState::new(config));

    let stream = build_stream(&device, &stream_config, sample_format, mixer.clone(), frames_played.clone())?;
    stream
      .play()
      .map_err(|err| AudioError::StreamPlayFailed(err.to_string()))?;

    Ok(Self {
      config,
      mixer,
      frames_played,
      _stream: Mutex::new(stream),
    })
  }
}

impl AudioBackend for CpalAudioBackend {
  fn output_config(&self) -> AudioStreamConfig {
    self.config
  }

  fn clock(&self) -> AudioClock {
    AudioClock::OutputFrames {
      frames_played: self.frames_played.clone(),
      sample_rate_hz: self.config.sample_rate_hz,
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    let sink = Arc::new(SinkState::new(self.config));
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
      let gain_bits = sink.volume_bits.load(Ordering::Relaxed);
      let gain = f32::from_bits(gain_bits);
      sink.buffer.pop_add_into(dst, gain);
    }
  }

  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
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
    self.state.buffer.push(&samples[..usable_len])
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
        max_rate
      };
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
  Ok((config, sample_format))
}

fn build_stream(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  sample_format: cpal::SampleFormat,
  mixer: Arc<MixerState>,
  frames_played: Arc<AtomicU64>,
) -> Result<cpal::Stream, AudioError> {
  match sample_format {
    cpal::SampleFormat::F32 => build_stream_typed::<f32>(device, config, mixer, frames_played),
    cpal::SampleFormat::I16 => build_stream_typed::<i16>(device, config, mixer, frames_played),
    cpal::SampleFormat::U16 => build_stream_typed::<u16>(device, config, mixer, frames_played),
    other => Err(AudioError::UnsupportedSampleFormat(format!("{other:?}"))),
  }
}

fn build_stream_typed<T>(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  mixer: Arc<MixerState>,
  frames_played: Arc<AtomicU64>,
) -> Result<cpal::Stream, AudioError>
where
  T: OutputSample,
{
  use cpal::traits::DeviceTrait;

  let channels = mixer.channels_usize();
  let mut mix_buf: Vec<f32> = Vec::new();

  let err_cb = |err| {
    eprintln!("warning: CPAL output stream error: {err}");
  };

  let stream = device
    .build_output_stream(
      config,
      move |output: &mut [T], _info| {
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
          frames_played.fetch_add(frames, Ordering::Relaxed);
        }
      },
      err_cb,
      None,
    )
    .map_err(|err| AudioError::StreamBuildFailed(err.to_string()))?;

  Ok(stream)
}

fn sanitize_f32(value: f32) -> f32 {
  if value.is_finite() {
    value.clamp(-1.0, 1.0)
  } else {
    0.0
  }
}

fn f32_to_i16(value: f32) -> i16 {
  let value = sanitize_f32(value);
  (value * i16::MAX as f32) as i16
}

fn f32_to_u16(value: f32) -> u16 {
  let value = sanitize_f32(value);
  let shifted = value * 0.5 + 0.5;
  (shifted * u16::MAX as f32) as u16
}

trait OutputSample: cpal::Sample {
  fn from_mixed_f32(value: f32) -> Self;
}

impl OutputSample for f32 {
  fn from_mixed_f32(value: f32) -> Self {
    sanitize_f32(value)
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
  use super::{f32_to_i16, f32_to_u16, sanitize_f32};

  #[test]
  fn sanitize_handles_nan_and_clamps() {
    assert_eq!(sanitize_f32(f32::NAN), 0.0);
    assert_eq!(sanitize_f32(2.0), 1.0);
    assert_eq!(sanitize_f32(-2.0), -1.0);
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
