use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::Duration;

#[derive(Debug)]
pub struct AudioMixer {
  sample_rate_hz: u32,
  channels: usize,
  streams: Mutex<Vec<Weak<AudioStreamInner>>>,
}

impl AudioMixer {
  #[must_use]
  pub fn new(sample_rate_hz: u32, channels: usize) -> Self {
    debug_assert!(sample_rate_hz > 0, "sample_rate_hz must be non-zero");
    debug_assert!(channels > 0, "channels must be non-zero");
    Self {
      sample_rate_hz,
      channels,
      streams: Mutex::new(Vec::new()),
    }
  }

  #[must_use]
  pub fn sample_rate_hz(&self) -> u32 {
    self.sample_rate_hz
  }

  #[must_use]
  pub fn channels(&self) -> usize {
    self.channels
  }

  #[must_use]
  pub fn create_stream(&self) -> AudioStreamHandle {
    let inner = Arc::new(AudioStreamInner {
      sample_rate_hz: self.sample_rate_hz,
      channels: self.channels,
      state: Mutex::new(AudioStreamState::new()),
    });

    self.streams.lock().push(Arc::downgrade(&inner));

    AudioStreamHandle { inner }
  }

  /// Mixes `frames` audio frames into a newly allocated interleaved `f32` buffer.
  ///
  /// The returned buffer has a length of `frames * channels`.
  #[must_use]
  pub fn mix(&self, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0; frames.saturating_mul(self.channels)];
    self.mix_into(&mut out);
    out
  }

  /// Mixes audio into `out`, which must be interleaved `f32` samples.
  ///
  /// `out.len()` must be a multiple of `channels()`.
  pub fn mix_into(&self, out: &mut [f32]) {
    debug_assert!(
      self.channels > 0,
      "AudioMixer created with invalid channel count"
    );
    debug_assert!(
      out.len() % self.channels == 0,
      "output buffer must be a multiple of channel count"
    );

    let streams: Vec<Arc<AudioStreamInner>> = {
      let mut guard = self.streams.lock();
      let mut strong = Vec::with_capacity(guard.len());
      guard.retain(|weak| {
        if let Some(stream) = weak.upgrade() {
          strong.push(stream);
          true
        } else {
          false
        }
      });
      strong
    };

    for stream in streams {
      stream.mix_into(out);
    }
  }
}

#[derive(Debug, Clone)]
pub struct AudioStreamHandle {
  inner: Arc<AudioStreamInner>,
}

impl AudioStreamHandle {
  /// Enqueues interleaved `f32` samples for playback.
  ///
  /// The input must have a length that is a multiple of the stream's channel count.
  pub fn enqueue_samples(&self, samples: Vec<f32>) -> Result<(), AudioStreamEnqueueError> {
    if samples.len() % self.inner.channels != 0 {
      return Err(AudioStreamEnqueueError::InvalidInterleavedSampleCount {
        len: samples.len(),
        channels: self.inner.channels,
      });
    }

    let mut state = self.inner.state.lock();
    state.queue.extend(samples);
    Ok(())
  }

  /// Starts the stream if it is not already playing.
  ///
  /// This is idempotent.
  pub fn play(&self) {
    let mut state = self.inner.state.lock();
    state.is_playing = true;
  }

  /// Pauses the stream if it is not already paused.
  ///
  /// While paused, the stream contributes silence to the mixer and does not drain its queue.
  ///
  /// This is idempotent.
  pub fn pause(&self) {
    let mut state = self.inner.state.lock();
    state.is_playing = false;
  }

  /// Drops all queued samples immediately.
  ///
  /// This does not change the stream clock mapping (see [`Self::seek_to`]).
  pub fn flush(&self) {
    let mut state = self.inner.state.lock();
    state.queue.clear();
  }

  /// Flushes queued samples and resets the stream clock mapping so the media time jumps to
  /// `base_pts`.
  ///
  /// This is intended to be called when an `HTMLMediaElement` seeks.
  pub fn seek_to(&self, base_pts: Duration) {
    let mut state = self.inner.state.lock();
    state.queue.clear();
    state.base_pts = base_pts;
    state.played_frames = 0;
  }

  /// Alias for [`Self::seek_to`].
  pub fn set_base_pts(&self, base_pts: Duration) {
    self.seek_to(base_pts);
  }

  #[must_use]
  pub fn current_time(&self) -> Duration {
    let state = self.inner.state.lock();
    let played = frames_to_duration(state.played_frames, self.inner.sample_rate_hz);
    state.base_pts.saturating_add(played)
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStreamEnqueueError {
  InvalidInterleavedSampleCount { len: usize, channels: usize },
}

#[derive(Debug)]
struct AudioStreamInner {
  sample_rate_hz: u32,
  channels: usize,
  state: Mutex<AudioStreamState>,
}

impl AudioStreamInner {
  fn mix_into(&self, out: &mut [f32]) {
    let frames_requested = out.len() / self.channels;

    let mut state = self.state.lock();
    if !state.is_playing {
      return;
    }

    let available_frames = state.queue.len() / self.channels;
    let frames_to_mix = available_frames.min(frames_requested);
    if frames_to_mix == 0 {
      return;
    }

    // Mix by consuming samples from the front of the queue.
    //
    // Important semantics:
    // - We only advance the playhead by the number of frames actually consumed (so underflow
    //   behaves like a stalled clock, not a drifting one).
    // - When paused, we return early above so we neither drain the queue nor advance the clock.
    let samples_to_mix = frames_to_mix.saturating_mul(self.channels);
    for out_sample in out.iter_mut().take(samples_to_mix) {
      if let Some(sample) = state.queue.pop_front() {
        *out_sample += sample;
      } else {
        break;
      }
    }

    state.played_frames = state.played_frames.saturating_add(frames_to_mix as u64);
  }
}

#[derive(Debug)]
struct AudioStreamState {
  is_playing: bool,
  base_pts: Duration,
  played_frames: u64,
  queue: VecDeque<f32>,
}

impl AudioStreamState {
  fn new() -> Self {
    Self {
      is_playing: false,
      base_pts: Duration::ZERO,
      played_frames: 0,
      queue: VecDeque::new(),
    }
  }
}

fn frames_to_duration(frames: u64, sample_rate_hz: u32) -> Duration {
  if sample_rate_hz == 0 {
    // Defensive: a zero sample rate would already violate the AudioMixer contract.
    return Duration::ZERO;
  }

  let nanos = (frames as u128)
    .saturating_mul(1_000_000_000u128)
    .checked_div(sample_rate_hz as u128)
    .unwrap_or(0);

  Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// A backend that does not talk to a real audio device.
///
/// Tests can call [`Self::render`] to simulate audio callbacks.
#[derive(Debug)]
pub struct NullAudioBackend {
  mixer: AudioMixer,
}

impl NullAudioBackend {
  #[must_use]
  pub fn new(sample_rate_hz: u32, channels: usize) -> Self {
    Self {
      mixer: AudioMixer::new(sample_rate_hz, channels),
    }
  }

  #[must_use]
  pub fn mixer(&self) -> &AudioMixer {
    &self.mixer
  }

  #[must_use]
  pub fn create_stream(&self) -> AudioStreamHandle {
    self.mixer.create_stream()
  }

  #[must_use]
  pub fn render(&self, frames: usize) -> Vec<f32> {
    self.mixer.mix(frames)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;
  use std::thread;

  fn all_samples_eq(samples: &[f32], expected: f32) -> bool {
    samples.iter().all(|sample| (*sample - expected).abs() < f32::EPSILON)
  }

  #[test]
  fn paused_stream_clock_freezes() {
    let backend = NullAudioBackend::new(48_000, 1);
    let stream = backend.create_stream();
    stream.enqueue_samples(vec![1.0; 48_000]).unwrap();

    stream.play();
    let _ = backend.render(24_000);
    assert_eq!(stream.current_time(), Duration::from_millis(500));

    stream.pause();
    let out = backend.render(24_000);
    assert!(all_samples_eq(&out, 0.0));
    assert_eq!(stream.current_time(), Duration::from_millis(500));
  }

  #[test]
  fn paused_stream_does_not_drain_queue() {
    let backend = NullAudioBackend::new(48_000, 1);
    let stream = backend.create_stream();
    stream.enqueue_samples(vec![1.0; 48_000]).unwrap();

    stream.pause();
    let out0 = backend.render(24_000);
    assert!(all_samples_eq(&out0, 0.0));
    assert_eq!(stream.current_time(), Duration::ZERO);

    stream.play();
    let out1 = backend.render(48_000);
    assert!(all_samples_eq(&out1, 1.0));
    assert_eq!(stream.current_time(), Duration::from_secs(1));
  }

  #[test]
  fn seek_flushes_buffered_audio_and_resets_clock_mapping() {
    let backend = NullAudioBackend::new(48_000, 1);
    let stream = backend.create_stream();

    stream.enqueue_samples(vec![1.0; 48_000]).unwrap();
    stream.play();
    let out0 = backend.render(24_000);
    assert!(all_samples_eq(&out0, 1.0));
    assert_eq!(stream.current_time(), Duration::from_millis(500));

    stream.seek_to(Duration::from_secs(10));
    assert_eq!(stream.current_time(), Duration::from_secs(10));

    // The remaining queued `1.0` samples should have been dropped.
    let out1 = backend.render(24_000);
    assert!(all_samples_eq(&out1, 0.0));
    assert_eq!(stream.current_time(), Duration::from_secs(10));

    stream.enqueue_samples(vec![2.0; 48_000]).unwrap();
    let out2 = backend.render(24_000);
    assert!(all_samples_eq(&out2, 2.0));
    assert_eq!(stream.current_time(), Duration::from_millis(10_500));
  }

  #[test]
  fn flush_is_safe_concurrently_with_mixing() {
    let mixer = Arc::new(AudioMixer::new(48_000, 1));
    let stream = mixer.create_stream();
    stream.play();
    stream.enqueue_samples(vec![1.0; 48_000]).unwrap();

    let mixer_for_mix = Arc::clone(&mixer);
    let stream_for_flush = stream.clone();
    let mix_thread = thread::spawn(move || {
      for _ in 0..200 {
        let _ = mixer_for_mix.mix(240);
      }
    });

    let flush_thread = thread::spawn(move || {
      for _ in 0..200 {
        stream_for_flush.flush();
        let _ = stream_for_flush.enqueue_samples(vec![1.0; 240]);
      }
    });

    mix_thread.join().unwrap();
    flush_thread.join().unwrap();
  }
}

