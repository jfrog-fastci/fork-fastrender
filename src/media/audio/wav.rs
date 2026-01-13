use std::collections::VecDeque;
use std::convert::TryFrom;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::thread;

use parking_lot::{Condvar, Mutex, RwLock};

use super::convert::sanitize_sample;
use super::{AudioBackend, AudioClock, AudioSink, AudioStreamConfig};
use crate::media::audio_clock::InterpolatedAudioClock;

/// An audio backend that mixes sinks and writes the resulting PCM stream into a `.wav` file.
///
/// This is intended as a *debug* backend (pure Rust, no system libraries). It does not attempt to
/// model real-time playback latency; it writes out whatever audio is submitted by sinks.
pub struct WavAudioBackend {
  config: AudioStreamConfig,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  state: Arc<BackendState>,
  worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl WavAudioBackend {
  /// Create a WAV backend writing to `path`, using a default `48kHz stereo` output configuration.
  pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
    Self::new_with_config(path, AudioStreamConfig::new(48_000, 2))
  }

  /// Create a WAV backend writing to `path`, using `config` as the stream format.
  pub fn new_with_config(path: impl AsRef<Path>, config: AudioStreamConfig) -> std::io::Result<Self> {
    // Defensive: ensure the backend always has a non-zero stream config so downstream calculations
    // (WAV spec, clocking, buffering) never divide by zero.
    let config = AudioStreamConfig::new(config.sample_rate_hz.max(1), config.channels.max(1));

    let path = path.as_ref().to_path_buf();
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
      }
    }

    let channels = config.channels;
    let spec = hound::WavSpec {
      channels,
      sample_rate: config.sample_rate_hz,
      bits_per_sample: 16,
      sample_format: hound::SampleFormat::Int,
    };

    let file = File::create(&path)?;
    let writer = hound::WavWriter::new(BufWriter::new(file), spec)
      .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;

    let mixer = Arc::new(MixerState::new(config));
    let clock = Arc::new(InterpolatedAudioClock::new(config.sample_rate_hz));
    let state = Arc::new(BackendState::new());

    let worker_state = Arc::clone(&state);
    let worker_mixer = Arc::clone(&mixer);
    let worker_clock = Arc::clone(&clock);

    let handle = thread::Builder::new()
      .name("wav-audio-backend".to_string())
      .spawn(move || run_mix_loop(writer, worker_mixer, worker_clock, channels, worker_state))
      .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;

    Ok(Self {
      config,
      mixer,
      clock,
      state,
      worker: Mutex::new(Some(handle)),
    })
  }
}

impl AudioBackend for WavAudioBackend {
  fn output_config(&self) -> AudioStreamConfig {
    self.config
  }

  fn clock(&self) -> AudioClock {
    AudioClock::OutputFrames {
      clock: self.clock.clone(),
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    let sink = Arc::new(SinkState::new(self.config));
    self.mixer.register_sink(&sink);
    Box::new(WavAudioSink {
      state: sink,
      backend_state: Arc::clone(&self.state),
    })
  }
}

impl Drop for WavAudioBackend {
  fn drop(&mut self) {
    self.state.stop.store(true, Ordering::Relaxed);
    self.state.wake.notify_all();

    if let Some(handle) = self.worker.lock().take() {
      let _ = handle.join();
    }
  }
}

#[derive(Debug)]
struct BackendState {
  stop: AtomicBool,
  wake_lock: Mutex<()>,
  wake: Condvar,
}

impl BackendState {
  fn new() -> Self {
    Self {
      stop: AtomicBool::new(false),
      wake_lock: Mutex::new(()),
      wake: Condvar::new(),
    }
  }
}

#[derive(Debug)]
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

  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
  }

  /// Returns the maximum number of queued samples among active sinks.
  fn max_available_samples(&self) -> usize {
    let sinks = self.sinks.read();
    let mut max = 0usize;
    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };
      let len = sink.buffer.lock().len();
      max = max.max(len);
    }
    // Keep alignment in frames.
    let channels = self.channels_usize();
    if channels == 0 {
      return 0;
    }
    max - (max % channels)
  }

  /// Mixes up to `dst.len()` samples by draining from sink buffers.
  ///
  /// Returns `true` if any sink had at least one sample drained.
  fn pop_mix_into(&self, dst: &mut [f32]) -> bool {
    dst.fill(0.0);
    if dst.is_empty() {
      return false;
    }

    let sinks_snapshot: Vec<Arc<SinkState>> = {
      let mut sinks = self.sinks.write();
      let mut strong = Vec::with_capacity(sinks.len());
      sinks.retain(|weak| {
        if let Some(sink) = weak.upgrade() {
          strong.push(sink);
          true
        } else {
          false
        }
      });
      strong
    };

    let mut any = false;
    for sink in sinks_snapshot {
      let gain_raw = f32::from_bits(sink.volume_bits.load(Ordering::Relaxed));
      // Treat non-finite/denormal gain values as silence so we never poison the mix.
      // Still drain the sink buffer so muting/corruption does not behave like pausing.
      let gain = if gain_raw.is_finite() && gain_raw.is_normal() {
        gain_raw
      } else {
        0.0
      };
      let mut buf = sink.buffer.lock();
      let to_read = dst.len().min(buf.len());
      if to_read == 0 {
        continue;
      }
      any = true;
      if gain == 0.0 {
        // Drain without mixing (muted).
        for _ in 0..to_read {
          let _ = buf.pop_front();
        }
        continue;
      }
      for i in 0..to_read {
        let Some(sample) = buf.pop_front() else {
          break;
        };
        // Avoid NaN poisoning / denormal slow paths by dropping non-normal samples before they
        // reach the hot multiply/add loop.
        if !sample.is_normal() {
          continue;
        }
        let scaled = sample * gain;
        if scaled.is_normal() {
          let cur = dst[i];
          if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
            dst[i] = 0.0;
          }
          dst[i] += scaled;
        }
      }
    }

    any
  }
}

#[derive(Debug)]
struct SinkState {
  config: AudioStreamConfig,
  buffer: Mutex<VecDeque<f32>>,
  volume_bits: AtomicU32,
}

impl SinkState {
  fn new(config: AudioStreamConfig) -> Self {
    Self {
      config,
      buffer: Mutex::new(VecDeque::new()),
      volume_bits: AtomicU32::new(1.0f32.to_bits()),
    }
  }

  fn max_buffer_samples(&self) -> usize {
    // Keep roughly 2 seconds of audio per sink (similar to CPAL backend).
    (self.config.sample_rate_hz as usize)
      .saturating_mul(usize::from(self.config.channels.max(1)))
      .saturating_mul(2)
      .max(1)
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

#[derive(Debug)]
struct WavAudioSink {
  state: Arc<SinkState>,
  backend_state: Arc<BackendState>,
}

impl AudioSink for WavAudioSink {
  fn config(&self) -> AudioStreamConfig {
    self.state.config
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    let channels = usize::from(self.state.config.channels.max(1));
    let usable_len = samples.len() - (samples.len() % channels);
    if usable_len == 0 {
      return 0;
    }

    let to_accept = {
      let mut buf = self.state.buffer.lock();
      let max_samples = self.state.max_buffer_samples();
      let free = max_samples.saturating_sub(buf.len());
      let to_accept = usable_len.min(free);
      if to_accept == 0 {
        return 0;
      }
      buf.extend(samples[..to_accept].iter().copied());
      to_accept
    };

    // Lock the wake mutex around notification to avoid missing the wake-up when the writer thread
    // checks for available samples and then transitions into `wait()`.
    let _guard = self.backend_state.wake_lock.lock();
    self.backend_state.wake.notify_one();
    to_accept
  }

  fn set_volume(&self, volume: f32) {
    self.state.set_volume(volume);
  }
}

const CHUNK_FRAMES: usize = 1024;

fn run_mix_loop(
  mut writer: hound::WavWriter<BufWriter<File>>,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  channels: u16,
  state: Arc<BackendState>,
) {
  let chunk_samples = CHUNK_FRAMES.saturating_mul(channels as usize).max(channels as usize);
  let mut mix_buf: Vec<f32> = vec![0.0; chunk_samples];
  let mut warned = false;
  let mut writer_failed = false;

  loop {
    let available = {
      // Use the wake mutex to avoid missing notifications between "check" and "wait".
      let mut guard = state.wake_lock.lock();
      loop {
        let available = mixer.max_available_samples();
        if available != 0 || state.stop.load(Ordering::Relaxed) {
          break available;
        }
        state.wake.wait(&mut guard);
      }
    };

    if available == 0 && state.stop.load(Ordering::Relaxed) {
      break;
    }

    let to_write = available.min(mix_buf.len());
    if to_write == 0 {
      continue;
    }

    if !mixer.pop_mix_into(&mut mix_buf[..to_write]) {
      continue;
    }

    if !writer_failed {
      for sample in mix_buf[..to_write].iter().copied() {
        if writer.write_sample(f32_to_i16(sample)).is_err() {
          writer_failed = true;
          if !warned {
            warned = true;
            eprintln!("warning: WAV audio backend failed to write samples; dropping further audio");
          }
          break;
        }
      }
    }

    let frames = (to_write / channels as usize) as u64;
    clock.advance_frames(frames);
  }

  let _ = writer.finalize();
}

#[inline]
fn f32_to_i16(sample: f32) -> i16 {
  let sample = sanitize_sample(sample);
  (sample * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::media::audio::test_signal;
  use std::time::Duration;

  #[test]
  fn wav_backend_writes_expected_pcm_samples() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.wav");

    let sample_rate = 8_000;
    let channels = 2;
    let duration = Duration::from_millis(10);
    let samples = test_signal::impulse_duration(duration, sample_rate, channels);

    {
      let backend = WavAudioBackend::new_with_config(
        &path,
        AudioStreamConfig::new(sample_rate, channels),
      )
      .expect("backend");
      let sink = backend.create_sink();
      let accepted = sink.push_interleaved_f32(&samples);
      assert_eq!(accepted, samples.len());
    }

    let mut reader = hound::WavReader::open(&path).expect("reader");
    let spec = reader.spec();
    assert_eq!(spec.channels, channels);
    assert_eq!(spec.sample_rate, sample_rate);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);

    let out: Vec<i16> = reader
      .samples::<i16>()
      .map(|s| s.expect("sample"))
      .collect();
    assert_eq!(out.len(), samples.len());

    assert_eq!(out[0], i16::MAX);
    assert_eq!(out[1], i16::MAX);
    assert!(out[2..].iter().all(|v| *v == 0));
  }

  #[test]
  fn pop_mix_into_drains_when_gain_is_zero() {
    let config = AudioStreamConfig::new(48_000, 1);
    let mixer = MixerState::new(config);
    let sink = Arc::new(SinkState::new(config));
    mixer.register_sink(&sink);

    // 200ms worth of mono 48kHz samples.
    let total = 48_000 / 5;
    let half = total / 2;

    sink.buffer.lock().extend(std::iter::repeat(1.0).take(total));
    sink.set_volume(0.0);

    // Muted mixing should still drain samples and return true (so the backend advances time).
    let mut muted_out = vec![0.0; half];
    assert!(mixer.pop_mix_into(&mut muted_out));
    assert_eq!(muted_out, vec![0.0; half]);
    assert_eq!(sink.buffer.lock().len(), total - half);

    // Unmuting should play immediately without backlog (only the remaining samples should mix).
    sink.set_volume(1.0);
    let mut out = vec![0.0; total];
    assert!(mixer.pop_mix_into(&mut out));
    assert_eq!(&out[..half], &vec![1.0; half][..]);
    assert_eq!(&out[half..], &vec![0.0; half][..]);
    assert_eq!(sink.buffer.lock().len(), 0);
  }
}
