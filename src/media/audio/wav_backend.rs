use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
<<<<<<< HEAD
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
=======
use std::sync::atomic::{AtomicU32, Ordering};
>>>>>>> 897b22fe8 (feat(interaction): focus <video controls> in tab navigation)
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

<<<<<<< HEAD
use super::ring_buffer::AudioRingBuffer;
use super::{
  audio_engine_config, duration_to_frames_ceil, frames_to_duration, AudioBackend, AudioClock,
  AudioEngineConfig, AudioOutputInfo, AudioSink, AudioStreamConfig,
};
use super::limits::MAX_BUFFERED_DURATION;
use crate::debug::trace::TraceHandle;
use crate::media::audio_clock::InterpolatedAudioClock;
=======
use super::{AudioBackend, AudioClock, AudioSink, AudioStreamConfig};
use crate::media::audio_clock::InterpolatedAudioClock;
use crate::media::audio::ring_buffer::AudioRingBuffer;
>>>>>>> 897b22fe8 (feat(interaction): focus <video controls> in tab navigation)

/// Offline audio backend that mixes to a fixed output format and writes into a `.wav` file.
///
/// This backend is intended for deterministic audio debugging and unit tests:
/// - Sinks queue `f32` samples like the CPAL backend.
/// - Mixing + file IO only happens when the caller explicitly invokes [`Self::render`].
pub struct WavAudioBackend {
  config: AudioStreamConfig,
  max_buffered_duration: Duration,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  writer: Mutex<WavWriter>,
  trace: TraceHandle,
}

impl WavAudioBackend {
  /// Create a new WAV backend writing 16-bit PCM to `path`.
  ///
  /// The output format and buffering are derived from the current [`AudioEngineConfig`].
  pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
<<<<<<< HEAD
    let cfg = audio_engine_config();
    Self::new_with_engine_config(path, &cfg)
  }

  /// Like [`Self::new`], but wires up tracing spans into the provided handle.
  pub fn new_with_trace<P: AsRef<Path>>(path: P, trace: TraceHandle) -> io::Result<Self> {
    let cfg = audio_engine_config();
    Self::new_with_engine_config_and_trace(path, &cfg, trace)
  }

  /// Create a WAV backend using an explicit [`AudioEngineConfig`].
  pub fn new_with_engine_config<P: AsRef<Path>>(
    path: P,
    engine_cfg: &AudioEngineConfig,
  ) -> io::Result<Self> {
    Self::new_with_engine_config_and_trace(path, engine_cfg, TraceHandle::default())
  }

  /// Like [`Self::new_with_engine_config`], but wires up tracing spans into the provided handle.
  pub fn new_with_engine_config_and_trace<P: AsRef<Path>>(
    path: P,
    engine_cfg: &AudioEngineConfig,
    trace: TraceHandle,
  ) -> io::Result<Self> {
    let config = AudioStreamConfig::new(
      engine_cfg.default_sample_rate_hz,
      engine_cfg.default_channels,
    );
    Self::new_with_output_config_and_trace(
      path,
      config,
      engine_cfg.per_stream_max_buffered_duration,
      trace,
    )
  }

  /// Create a WAV backend with an explicit stream format and per-sink buffer limit.
  pub fn new_with_output_config<P: AsRef<Path>>(
    path: P,
    config: AudioStreamConfig,
    max_buffered_duration: Duration,
  ) -> io::Result<Self> {
    Self::new_with_output_config_and_trace(path, config, max_buffered_duration, TraceHandle::default())
  }

  /// Like [`Self::new_with_output_config`], but wires up tracing spans into the provided handle.
  pub fn new_with_output_config_and_trace<P: AsRef<Path>>(
    path: P,
    config: AudioStreamConfig,
    max_buffered_duration: Duration,
    trace: TraceHandle,
  ) -> io::Result<Self> {
    let max_buffered_duration = max_buffered_duration.min(MAX_BUFFERED_DURATION);
    let mixer = Arc::new(MixerState::new(config));
    let clock = Arc::new(InterpolatedAudioClock::new(config.sample_rate_hz.max(1)));
=======
    let config = AudioStreamConfig::new(48_000, 2);
    let mixer = Arc::new(MixerState::new(config));
    let clock = Arc::new(InterpolatedAudioClock::new(config.sample_rate_hz));
>>>>>>> 897b22fe8 (feat(interaction): focus <video controls> in tab navigation)

    let mut file = File::create(path)?;
    write_wav_header(&mut file, config, 0)?;
    file.flush()?;

    Ok(Self {
      config,
      max_buffered_duration,
      mixer,
      clock,
      writer: Mutex::new(WavWriter { file, data_bytes: 0 }),
      trace,
    })
  }

  /// Mix and write `frames` output frames.
  ///
  /// Samples are written as little-endian 16-bit PCM and appended to the `data` chunk. The WAV
  /// header's chunk sizes are kept up-to-date after every call so the output file is valid even if
  /// the process exits early.
  pub fn render(&self, frames: usize) -> io::Result<()> {
    if frames == 0 {
      return Ok(());
    }

    let channels = usize::from(self.config.channels.max(1));
    let samples = frames.saturating_mul(channels);
    if samples == 0 {
      return Ok(());
    }

    let trace_enabled = self.trace.is_enabled();
    let mut callback_span = if trace_enabled {
      let mut span = self.trace.try_span("audio.callback", "audio");
      if let Some(span) = span.as_mut() {
        span.arg_u64("frames", frames as u64);
      }
      span
    } else {
      None
    };

    // Lock the writer for the duration of the render. This ensures single-consumer semantics for
    // the ring buffers (only one `render()` at a time) and keeps file writes serialized.
    let mut writer = self.writer.lock();

    let mut mix: Vec<f32> = vec![0.0; samples];
    let mix_span = if trace_enabled {
      self.trace.try_span("audio.mix", "audio")
    } else {
      None
    };
    self.mixer.mix_into(&mut mix);
    drop(mix_span);

    // Convert to 16-bit PCM bytes.
    let mut pcm = vec![0u8; samples * 2];
    for (idx, sample) in mix.iter().enumerate() {
      let v = f32_to_i16(*sample);
      let [lo, hi] = v.to_le_bytes();
      let base = idx * 2;
      pcm[base] = lo;
      pcm[base + 1] = hi;
    }

    writer.file.write_all(&pcm)?;
    writer.data_bytes = writer
      .data_bytes
      .checked_add(pcm.len() as u64)
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAV data size overflow"))?;

    let data_bytes = writer.data_bytes;
    update_wav_header(&mut writer.file, self.config, data_bytes)?;
    writer.file.flush()?;

<<<<<<< HEAD
    let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
    let new_frames_written = self
      .clock
      .frames_written()
      .saturating_add(u64::from(frames_u32));
    let device_time_at_end = frames_to_duration(self.config.sample_rate_hz, new_frames_written);
    self
      .clock
      .on_callback_end_with_device_time(frames_u32, device_time_at_end);

    drop(callback_span);
=======
    // Treat each `render()` call as an audio callback for clocking purposes.
    let mut remaining = frames;
    while remaining > 0 {
      let chunk = remaining.min(u32::MAX as usize) as u32;
      self.clock.on_callback_end(chunk);
      remaining -= chunk as usize;
    }
>>>>>>> 897b22fe8 (feat(interaction): focus <video controls> in tab navigation)

    Ok(())
  }
}

impl AudioBackend for WavAudioBackend {
  fn output_config(&self) -> AudioStreamConfig {
    self.config
  }

  fn output_info(&self) -> AudioOutputInfo {
    AudioOutputInfo {
      config: self.config,
      callback_frames: None,
      estimated_output_latency: Duration::ZERO,
      backend_name: "wav",
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
    Box::new(WavAudioSink { state: sink })
  }
}

struct WavWriter {
  file: File,
  data_bytes: u64,
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
    dst.fill(0.0);
    let sinks = self.sinks.read();
    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };
      if sink.paused.load(Ordering::Relaxed) {
        continue;
      }
      let gain_bits = sink.volume_bits.load(Ordering::Relaxed);
      let gain = f32::from_bits(gain_bits);
      sink.buffer.pop_add_into(dst, gain);
    }
  }

  #[allow(dead_code)]
  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
  }
}

struct SinkState {
  config: AudioStreamConfig,
  buffer: AudioRingBuffer,
  volume_bits: AtomicU32,
  paused: AtomicBool,
}

impl SinkState {
  fn new(config: AudioStreamConfig, max_buffered_duration: Duration) -> Self {
    let max_buffered_duration = max_buffered_duration.min(MAX_BUFFERED_DURATION);
    let channels = usize::from(config.channels.max(1));
    let max_frames = duration_to_frames_ceil(config.sample_rate_hz, max_buffered_duration);
    let max_frames = usize::try_from(max_frames).unwrap_or(usize::MAX);
    let capacity = max_frames.saturating_mul(channels).max(1);
    Self {
      config,
      buffer: AudioRingBuffer::new(capacity),
      volume_bits: AtomicU32::new(1.0f32.to_bits()),
      paused: AtomicBool::new(false),
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

  fn set_paused(&self, paused: bool) {
    self.paused.store(paused, Ordering::Relaxed);
  }

  fn flush(&self) {
    self.buffer.pop_discard(usize::MAX);
  }
}

struct WavAudioSink {
  state: Arc<SinkState>,
}

impl AudioSink for WavAudioSink {
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

  fn set_paused(&self, paused: bool) {
    self.state.set_paused(paused);
  }

  fn flush(&self) {
    self.state.flush();
  }
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

fn update_wav_header(file: &mut File, config: AudioStreamConfig, data_bytes: u64) -> io::Result<()> {
  let data_bytes_u32 = u32::try_from(data_bytes).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      "WAV data chunk exceeds 4GiB; size does not fit in WAV header",
    )
  })?;

  // Preserve the append position.
  let end_pos = file.seek(SeekFrom::End(0))?;
  file.seek(SeekFrom::Start(0))?;
  write_wav_header(file, config, data_bytes_u32)?;
  file.seek(SeekFrom::Start(end_pos))?;
  Ok(())
}

fn write_wav_header(file: &mut File, config: AudioStreamConfig, data_bytes: u32) -> io::Result<()> {
  let channels = config.channels.max(1);
  let sample_rate = config.sample_rate_hz;
  let bits_per_sample: u16 = 16;
  let block_align: u16 = (u32::from(channels) * u32::from(bits_per_sample) / 8) as u16;
  let byte_rate: u32 = sample_rate.saturating_mul(u32::from(block_align));

  // RIFF chunk size excludes the 8-byte RIFF header itself.
  let riff_chunk_size = 36u32.saturating_add(data_bytes);

  let mut header = [0u8; 44];
  header[0..4].copy_from_slice(b"RIFF");
  header[4..8].copy_from_slice(&riff_chunk_size.to_le_bytes());
  header[8..12].copy_from_slice(b"WAVE");
  header[12..16].copy_from_slice(b"fmt ");
  header[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size.
  header[20..22].copy_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
  header[22..24].copy_from_slice(&channels.to_le_bytes());
  header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
  header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
  header[32..34].copy_from_slice(&block_align.to_le_bytes());
  header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
  header[36..40].copy_from_slice(b"data");
  header[40..44].copy_from_slice(&data_bytes.to_le_bytes());

  file.write_all(&header)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use crate::media::audio::AudioBackend;
  use std::convert::TryInto;
  use std::time::Duration;

<<<<<<< HEAD
  use super::*;
  use crate::debug::trace::TraceHandle;
  use crate::media::audio::test_signal;
=======
  use super::{AudioBackend, WavAudioBackend};
>>>>>>> 897b22fe8 (feat(interaction): focus <video controls> in tab navigation)

  #[test]
  fn wav_audio_backend_writes_valid_header_and_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");

    {
      let backend = WavAudioBackend::new(&path).unwrap();
      let sink = backend.create_sink();

      // 4 stereo frames -> 8 samples -> 16 bytes of 16-bit PCM.
      let samples =
        test_signal::impulse_duration(Duration::from_millis(4), /* sample_rate */ 1000, 2);
      assert_eq!(samples.len(), 8);
      assert_eq!(sink.push_interleaved_f32(&samples), samples.len());

      backend.render(4).unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() >= 44);

    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(&bytes[36..40], b"data");

    let riff_chunk_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let fmt_chunk_size = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let audio_format = u16::from_le_bytes(bytes[20..22].try_into().unwrap());
    let channels = u16::from_le_bytes(bytes[22..24].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let byte_rate = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    let block_align = u16::from_le_bytes(bytes[32..34].try_into().unwrap());
    let bits_per_sample = u16::from_le_bytes(bytes[34..36].try_into().unwrap());
    let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().unwrap());

    assert_eq!(fmt_chunk_size, 16);
    assert_eq!(audio_format, 1);
    assert_eq!(channels, 2);
    assert_eq!(sample_rate, 48_000);
    assert_eq!(bits_per_sample, 16);
    assert_eq!(block_align, 4);
    assert_eq!(byte_rate, 48_000 * 4);

    let expected_data_bytes = 4u32 * 2 * 2;
    assert_eq!(data_bytes, expected_data_bytes);
    assert_eq!(riff_chunk_size, 36 + expected_data_bytes);
    assert_eq!(bytes.len(), 44 + expected_data_bytes as usize);

    // The impulse is full-scale on the first frame for both channels, followed by silence.
    let mut pcm = bytes[44..].chunks_exact(2).map(|chunk| {
      let arr: [u8; 2] = chunk.try_into().unwrap();
      i16::from_le_bytes(arr)
    });
    assert_eq!(pcm.next(), Some(i16::MAX));
    assert_eq!(pcm.next(), Some(i16::MAX));
    assert!(pcm.all(|v| v == 0));
  }

  #[test]
  fn wav_backend_respects_max_buffered_duration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.wav");

    let backend = WavAudioBackend::new_with_output_config(
      &path,
      AudioStreamConfig::new(10, 1),
      Duration::from_millis(300),
    )
    .expect("backend");
    let sink = backend.create_sink();

    let accepted = sink.push_interleaved_f32(&vec![1.0f32; 10]);
    assert_eq!(accepted, 3);
  }

  #[test]
  fn wav_backend_clock_advances_after_render() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wav_path = dir.path().join("out.wav");
    let backend = WavAudioBackend::new(&wav_path).expect("backend");

    let output = backend.output_config();
    let AudioClock::OutputFrames { clock } = backend.clock() else {
      panic!("expected WavAudioBackend to return an OutputFrames clock");
    };

    assert_eq!(clock.sample_rate_hz(), output.sample_rate_hz);
    assert_eq!(clock.frames_written(), 0);

    backend.render(10).expect("render");

    assert_eq!(clock.frames_written(), 10);
  }

  #[test]
  fn wav_backend_trace_emits_audio_callback_and_mix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wav_path = dir.path().join("out.wav");
    let trace = TraceHandle::enabled_with_max_events(32);

    let backend = WavAudioBackend::new_with_output_config_and_trace(
      &wav_path,
      AudioStreamConfig::new(48_000, 2),
      Duration::from_millis(250),
      trace.clone(),
    )
    .expect("backend");
    let sink = backend.create_sink();

    // Ensure mixing has audio to consume.
    let samples = vec![0.25f32; 48_000 * 2];
    assert_eq!(sink.push_interleaved_f32(&samples), samples.len());

    for _ in 0..4 {
      backend.render(240).expect("render");
    }

    let trace_path = dir.path().join("trace.json");
    trace.write_chrome_trace(&trace_path).expect("write trace");

    let json = std::fs::read_to_string(&trace_path).expect("read trace");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse trace");
    let trace_events = value["traceEvents"]
      .as_array()
      .expect("traceEvents array");
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
  }
}
