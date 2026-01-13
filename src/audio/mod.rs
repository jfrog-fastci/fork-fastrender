use crate::clock::Clock;
use crate::debug::runtime::runtime_toggles;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;
use thiserror::Error;

#[inline]
fn sanitize_mix_sample(x: f32) -> f32 {
  if !x.is_finite() {
    return 0.0;
  }
  // Flush subnormals (and +/-0) to avoid denormal slowdowns in mixing math.
  if !x.is_normal() {
    return 0.0;
  }
  x
}

#[inline]
fn sanitize_mix_buffer_in_place(buf: &mut [f32]) {
  for sample in buf {
    *sample = sanitize_mix_sample(*sample);
  }
}

const DEFAULT_GAIN_RAMP_DURATION_MS: u32 = 10;

fn gain_ramp_frames(sample_rate_hz: u32) -> u32 {
  // 5–20ms tends to be enough to hide abrupt gain changes without making UI feel laggy.
  // Use 10ms as a conservative default.
  let frames = (u64::from(sample_rate_hz).saturating_mul(u64::from(DEFAULT_GAIN_RAMP_DURATION_MS))
    / 1000) as u32;
  frames.max(1)
}

fn sanitize_unit_f32(value: f32) -> f32 {
  if value.is_finite() {
    value.clamp(0.0, 1.0)
  } else {
    0.0
  }
}

#[derive(Debug, Clone, Copy)]
struct GainRamp {
  current_gain: f32,
  target_gain: f32,
  step: f32,
  frames_remaining: u32,
}

impl GainRamp {
  fn new(initial_gain: f32) -> Self {
    Self {
      current_gain: initial_gain,
      target_gain: initial_gain,
      step: 0.0,
      frames_remaining: 0,
    }
  }

  fn set_target(&mut self, target_gain: f32, ramp_frames: u32) {
    let target_gain = sanitize_unit_f32(target_gain);
    self.target_gain = target_gain;

    // If we're already effectively at the target, snap.
    if (self.current_gain - target_gain).abs() <= f32::EPSILON {
      self.current_gain = target_gain;
      self.step = 0.0;
      self.frames_remaining = 0;
      return;
    }

    let ramp_frames = ramp_frames.max(1);
    self.frames_remaining = ramp_frames;
    self.step = (target_gain - self.current_gain) / ramp_frames as f32;
  }

  fn gain(&self) -> f32 {
    self.current_gain
  }

  fn advance_frame(&mut self) {
    if self.frames_remaining == 0 {
      return;
    }
    self.current_gain += self.step;
    self.frames_remaining -= 1;
    if self.frames_remaining == 0 {
      // Clamp away any accumulated floating-point error.
      self.current_gain = self.target_gain;
      self.step = 0.0;
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct VolumeControl {
  unmuted_volume: f32,
  muted: bool,
  ramp: GainRamp,
}

impl VolumeControl {
  fn new(initial_volume: f32) -> Self {
    let initial_volume = sanitize_unit_f32(initial_volume);
    Self {
      unmuted_volume: initial_volume,
      muted: false,
      ramp: GainRamp::new(initial_volume),
    }
  }

  fn gain(&self) -> f32 {
    self.ramp.gain()
  }

  fn set_volume(&mut self, volume: f32, ramp_frames: u32) {
    let volume = sanitize_unit_f32(volume);
    self.unmuted_volume = volume;
    let target = if self.muted { 0.0 } else { volume };
    self.ramp.set_target(target, ramp_frames);
  }

  fn set_muted(&mut self, muted: bool, ramp_frames: u32) {
    if self.muted == muted {
      return;
    }
    self.muted = muted;
    let target = if muted { 0.0 } else { self.unmuted_volume };
    self.ramp.set_target(target, ramp_frames);
  }

  fn advance_frame(&mut self) {
    self.ramp.advance_frame();
  }
}

#[derive(Debug)]
pub struct AudioMixer {
  sample_rate_hz: u32,
  channels: usize,
  streams: Mutex<Vec<Weak<AudioStreamInner>>>,
  master: Mutex<VolumeControl>,
  gain_ramp_frames: u32,
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
      master: Mutex::new(VolumeControl::new(1.0)),
      gain_ramp_frames: gain_ramp_frames(sample_rate_hz),
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

  /// Sets the mixer output volume (master gain) in the range `[0.0, 1.0]`.
  pub fn set_volume(&self, volume: f32) {
    let mut master = self.master.lock();
    master.set_volume(volume, self.gain_ramp_frames);
  }

  /// Mutes/unmutes the mixer output.
  ///
  /// Unmuting restores the previously set volume (as a smooth ramp).
  pub fn set_muted(&self, muted: bool) {
    let mut master = self.master.lock();
    master.set_muted(muted, self.gain_ramp_frames);
  }

  #[must_use]
  pub fn create_stream(&self) -> AudioStreamHandle {
    let inner = Arc::new(AudioStreamInner {
      sample_rate_hz: self.sample_rate_hz,
      channels: self.channels,
      gain_ramp_frames: self.gain_ramp_frames,
      state: Mutex::new(AudioStreamState::new(self.gain_ramp_frames)),
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

    {
      // Important: keep this allocation-free for audio callbacks. We retain dead streams in-place
      // without collecting into a temporary Vec.
      let mut guard = self.streams.lock();
      guard.retain(|weak| {
        if let Some(stream) = weak.upgrade() {
          stream.mix_into(out);
          true
        } else {
          false
        }
      });
    }

    // Apply master gain after mixing so gain changes affect amplitude but never affect stream drain
    // or time progression.
    let mut master = self.master.lock();
    let frames = out.len() / self.channels;
    for frame in 0..frames {
      let gain_raw = master.gain();
      let gain = if gain_raw.is_finite() && (gain_raw == 0.0 || gain_raw.is_normal()) {
        gain_raw
      } else {
        0.0
      };
      let base = frame * self.channels;
      for ch in 0..self.channels {
        let idx = base + ch;
        let sample = out[idx];
        let sample = if sample.is_finite() && (sample == 0.0 || sample.is_normal()) {
          sample
        } else {
          0.0
        };
        let scaled = sample * gain;
        out[idx] = if scaled.is_finite() && (scaled == 0.0 || scaled.is_normal()) {
          scaled
        } else {
          0.0
        };
      }
      master.advance_frame();
    }

    // Ensure the mixed output cannot contain NaN/Inf/denormals, even if an upstream decoder
    // misbehaves or in the face of extreme cancellation.
    sanitize_mix_buffer_in_place(out);
  }
}

#[derive(Debug, Clone)]
pub struct AudioStreamHandle {
  inner: Arc<AudioStreamInner>,
}

impl AudioStreamHandle {
  /// Configures the output preroll/latency model for this stream.
  ///
  /// The stream's `current_time()` is defined as the time of the audio that is reaching the
  /// speakers (or would, on a real backend). Many real audio backends have a constant latency
  /// between "samples written to the output callback" and "samples actually audible". To keep
  /// `current_time()` aligned with first audible audio, we subtract a configurable preroll from the
  /// played frame counter.
  ///
  /// A non-zero preroll means:
  /// - `current_time()` remains at `base_pts` until at least `preroll` worth of frames have been
  ///   rendered.
  /// - after that, `current_time()` advances normally.
  ///
  /// This is safe to call at any time, but changing it during playback may introduce a discontinuity
  /// in `current_time()`. Typical usage is to set it before `play()`.
  pub fn set_preroll(&self, preroll: Duration) {
    let preroll_frames = duration_to_frames_floor(preroll, self.inner.sample_rate_hz);
    let mut state = self.inner.state.lock();
    state.preroll_frames = preroll_frames;
  }

  /// Returns `true` once the stream has started producing *audible* audio.
  ///
  /// With `preroll=0`, this becomes `true` after the first frame has been rendered.
  /// With `preroll>0`, it becomes `true` once enough frames have been rendered to cover the preroll
  /// latency.
  #[must_use]
  pub fn playback_started(&self) -> bool {
    let state = self.inner.state.lock();
    playback_started(state.preroll_frames, state.played_frames)
  }

  /// Returns the device-time offset at which playback became audible.
  ///
  /// This is `None` until [`Self::playback_started`] is true.
  #[must_use]
  pub fn playback_started_at(&self) -> Option<Duration> {
    let state = self.inner.state.lock();
    if playback_started(state.preroll_frames, state.played_frames) {
      Some(frames_to_duration(state.preroll_frames, self.inner.sample_rate_hz))
    } else {
      None
    }
  }

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

    // Sanitize decoded samples before they enter the queue so malformed values cannot poison the
    // mixer (NaN propagation) and so we never store denormals.
    let mut samples = samples;
    for sample in &mut samples {
      *sample = sanitize_mix_sample(*sample);
    }

    let mut state = self.inner.state.lock();
    if state.eos {
      return Err(AudioStreamEnqueueError::StreamFinished);
    }
    state.queue.extend(samples);
    Ok(())
  }

  /// Mark this stream as end-of-stream (EOS): no further samples will be enqueued.
  ///
  /// Playback is considered *ended/drained* once EOS is set **and** the mixer has consumed all
  /// queued samples.
  pub fn finish(&self) {
    let mut state = self.inner.state.lock();
    state.eos = true;
  }

  /// Alias for [`Self::finish`].
  pub fn set_eos(&self) {
    self.finish();
  }

  /// Starts the stream if it is not already playing.
  ///
  /// This is idempotent.
  pub fn play(&self) {
    let mut state = self.inner.state.lock();
    state.is_playing = true;
  }

  /// Sets the per-stream volume in the range `[0.0, 1.0]`.
  pub fn set_volume(&self, volume: f32) {
    let mut state = self.inner.state.lock();
    state.volume.set_volume(volume, self.inner.gain_ramp_frames);
  }

  /// Mutes/unmutes the stream output.
  ///
  /// Unmuting restores the previously set volume (as a smooth ramp).
  pub fn set_muted(&self, muted: bool) {
    let mut state = self.inner.state.lock();
    state.volume.set_muted(muted, self.inner.gain_ramp_frames);
  }

  /// Sets the stream's "group" volume in the range `[0.0, 1.0]`.
  ///
  /// This is intended for higher-level mixers (e.g. per-tab volume) to apply an additional gain
  /// multiplier without needing to fold it into the per-stream volume.
  pub fn set_group_volume(&self, volume: f32) {
    let mut state = self.inner.state.lock();
    state
      .group_volume
      .set_volume(volume, self.inner.gain_ramp_frames);
  }

  /// Mutes/unmutes the stream's "group" gain.
  pub fn set_group_muted(&self, muted: bool) {
    let mut state = self.inner.state.lock();
    state
      .group_volume
      .set_muted(muted, self.inner.gain_ramp_frames);
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
    // Seeking resets stream completion state so new samples can be pushed for the new timeline.
    state.eos = false;
  }

  /// Alias for [`Self::seek_to`].
  pub fn set_base_pts(&self, base_pts: Duration) {
    self.seek_to(base_pts);
  }

  #[must_use]
  pub fn current_time(&self) -> Duration {
    let state = self.inner.state.lock();
    // `played_frames` advances when we render audio into the backend/output buffer.
    // To align this with "audio the user can hear", subtract preroll/latency.
    let audible_frames = state.played_frames.saturating_sub(state.preroll_frames);
    let played = frames_to_duration(audible_frames, self.inner.sample_rate_hz);
    state.base_pts.saturating_add(played)
  }

  /// Returns `Some(final_time)` once EOS has been set and the queued audio has fully drained.
  #[must_use]
  pub fn ended(&self) -> Option<Duration> {
    let state = self.inner.state.lock();
    if state.eos && state.queue.is_empty() {
      let played = frames_to_duration(state.played_frames, self.inner.sample_rate_hz);
      Some(state.base_pts.saturating_add(played))
    } else {
      None
    }
  }

  /// Returns `true` once EOS has been set and the queued audio has fully drained.
  #[must_use]
  pub fn is_drained(&self) -> bool {
    self.ended().is_some()
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStreamEnqueueError {
  InvalidInterleavedSampleCount { len: usize, channels: usize },
  StreamFinished,
}

#[derive(Debug)]
struct AudioStreamInner {
  sample_rate_hz: u32,
  channels: usize,
  gain_ramp_frames: u32,
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
    for frame in 0..frames_to_mix {
      let gain_raw = state.volume.gain() * state.group_volume.gain();
      // Treat non-finite/denormal gains as silence so we never poison the mix. Still drain queued
      // samples so muting/corruption does not behave like pausing.
      let gain = if gain_raw.is_finite() && (gain_raw == 0.0 || gain_raw.is_normal()) {
        gain_raw
      } else {
        0.0
      };
      let out_base = frame * self.channels;
      for ch in 0..self.channels {
        // The queue length check above guarantees availability, but keep this robust.
        let Some(sample) = state.queue.pop_front() else {
          break;
        };
        // Avoid NaN poisoning / denormal slow paths by dropping non-normal samples before they
        // reach the hot multiply/add loop.
        if !sample.is_normal() {
          continue;
        }
        if gain == 0.0 {
          continue;
        }
        let scaled = sample * gain;
        if !scaled.is_normal() {
          continue;
        }

        let out_idx = out_base + ch;
        let cur = out[out_idx];
        if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
          out[out_idx] = 0.0;
        }
        out[out_idx] += scaled;
      }
      state.volume.advance_frame();
      state.group_volume.advance_frame();
    }

    state.played_frames = state.played_frames.saturating_add(frames_to_mix as u64);
  }
}

#[derive(Debug)]
struct AudioStreamState {
  is_playing: bool,
  base_pts: Duration,
  /// Output latency/preroll, expressed in frames at `sample_rate_hz`.
  preroll_frames: u64,
  played_frames: u64,
  queue: VecDeque<f32>,
  eos: bool,
  volume: VolumeControl,
  group_volume: VolumeControl,
}

impl AudioStreamState {
  fn new(_gain_ramp_frames: u32) -> Self {
    Self {
      is_playing: false,
      base_pts: Duration::ZERO,
      preroll_frames: 0,
      played_frames: 0,
      queue: VecDeque::new(),
      eos: false,
      volume: VolumeControl::new(1.0),
      group_volume: VolumeControl::new(1.0),
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

fn duration_to_frames_floor(duration: Duration, sample_rate_hz: u32) -> u64 {
  if sample_rate_hz == 0 {
    return 0;
  }
  let frames = duration.as_nanos().saturating_mul(sample_rate_hz as u128) / 1_000_000_000u128;
  u64::try_from(frames).unwrap_or(u64::MAX)
}

fn playback_started(preroll_frames: u64, played_frames: u64) -> bool {
  if preroll_frames == 0 {
    played_frames > 0
  } else {
    played_frames >= preroll_frames
  }
}

// === Backends =================================================================

#[derive(Error, Debug)]
pub enum AudioBackendError {
  #[error("unknown audio backend: {0}")]
  UnknownBackend(String),

  #[error("FASTR_AUDIO_WAV_PATH must be set when FASTR_AUDIO_BACKEND=wav")]
  MissingWavPath,

  #[error(transparent)]
  Io(#[from] io::Error),
}

pub type AudioBackendResult<T> = std::result::Result<T, AudioBackendError>;

pub trait AudioBackend: Send + Sync {
  fn mixer(&self) -> &AudioMixer;

  fn create_stream(&self) -> AudioStreamHandle {
    self.mixer().create_stream()
  }

  /// Render `frames` frames of mixed output.
  ///
  /// Implementations may have side effects (e.g. writing to a file) but should always return the
  /// mixed samples for test/debug inspection.
  fn render_frames(&self, frames: usize) -> AudioBackendResult<Vec<f32>>;

  /// Test/debug helper: render the number of frames implied by a clock delta.
  ///
  /// Callers advance a [`VirtualClock`](crate::clock::VirtualClock) (or any [`Clock`]) and then
  /// call this with a mutable `last_time` cursor. This makes audio drain behaviour deterministic
  /// without relying on wall-clock sleeps.
  fn render_for_clock(&self, clock: &dyn Clock, last_time: &mut Duration) -> AudioBackendResult<Vec<f32>> {
    let now = clock.now();
    let delta = now.saturating_sub(*last_time);
    *last_time = now;
    let frames = duration_to_frames_floor(delta, self.mixer().sample_rate_hz()) as usize;
    self.render_frames(frames)
  }
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

  /// Test helper: render the number of frames implied by a clock delta.
  ///
  /// Callers advance a [`crate::js::VirtualClock`] and then call this with a mutable `last_time`
  /// cursor. This makes audio drain behaviour deterministic without relying on wall-clock sleeps.
  #[cfg(test)]
  pub fn render_for_clock(
    &self,
    clock: &crate::js::VirtualClock,
    last_time: &mut Duration,
  ) -> Vec<f32> {
    let now = clock.now();
    let delta = now.saturating_sub(*last_time);
    *last_time = now;
    let frames = duration_to_frames_floor(delta, self.mixer.sample_rate_hz()) as usize;
    self.render(frames)
  }
}

impl AudioBackend for NullAudioBackend {
  fn mixer(&self) -> &AudioMixer {
    &self.mixer
  }

  fn render_frames(&self, frames: usize) -> AudioBackendResult<Vec<f32>> {
    Ok(self.render(frames))
  }
}

#[derive(Debug)]
struct WavState {
  file: File,
  data_bytes_written: u64,
}

/// Deterministic offline audio backend that writes 16-bit PCM `.wav`.
///
/// Intended for CI + media regression tests where OS audio devices are unavailable.
#[derive(Debug)]
pub struct WavAudioBackend {
  mixer: AudioMixer,
  path: PathBuf,
  state: Mutex<WavState>,
}

impl WavAudioBackend {
  pub fn new(
    sample_rate_hz: u32,
    channels: usize,
    path: impl AsRef<Path>,
  ) -> AudioBackendResult<Self> {
    let path = path.as_ref().to_path_buf();
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
      }
    }

    let mut file = File::create(&path)?;
    write_pcm16_wav_header(&mut file, sample_rate_hz, channels, 0)?;

    Ok(Self {
      mixer: AudioMixer::new(sample_rate_hz, channels),
      path,
      state: Mutex::new(WavState {
        file,
        data_bytes_written: 0,
      }),
    })
  }

  #[must_use]
  pub fn path(&self) -> &Path {
    &self.path
  }

  fn finalize_header(&self) -> io::Result<()> {
    let mut state = self.state.lock();
    let end_pos = state.file.seek(SeekFrom::End(0))?;
    state.file.seek(SeekFrom::Start(0))?;

    let data_size_u32 = u32::try_from(state.data_bytes_written).unwrap_or(u32::MAX);
    write_pcm16_wav_header(
      &mut state.file,
      self.mixer.sample_rate_hz(),
      self.mixer.channels(),
      data_size_u32,
    )?;

    state.file.seek(SeekFrom::Start(end_pos))?;
    state.file.flush()?;
    Ok(())
  }
}

impl Drop for WavAudioBackend {
  fn drop(&mut self) {
    // Best effort: Drop cannot report errors. Tests verify header correctness.
    let _ = self.finalize_header();
  }
}

impl AudioBackend for WavAudioBackend {
  fn mixer(&self) -> &AudioMixer {
    &self.mixer
  }

  fn render_frames(&self, frames: usize) -> AudioBackendResult<Vec<f32>> {
    let mixed = self.mixer.mix(frames);

    let mut buf = Vec::with_capacity(mixed.len() * 2);
    for &sample in &mixed {
      let pcm = f32_to_pcm16(sample);
      buf.extend_from_slice(&pcm.to_le_bytes());
    }

    let mut state = self.state.lock();
    state.file.write_all(&buf)?;
    state.data_bytes_written = state
      .data_bytes_written
      .saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
    Ok(mixed)
  }
}

fn f32_to_pcm16(sample: f32) -> i16 {
  let clamped = sample.clamp(-1.0, 1.0);
  if clamped <= -1.0 {
    i16::MIN
  } else if clamped >= 1.0 {
    i16::MAX
  } else {
    (clamped * (i16::MAX as f32)).round() as i16
  }
}

fn write_pcm16_wav_header(
  mut w: impl Write,
  sample_rate_hz: u32,
  channels: usize,
  data_bytes: u32,
) -> io::Result<()> {
  let channels_u16 = u16::try_from(channels).unwrap_or(u16::MAX);
  let bits_per_sample: u16 = 16;
  let block_align: u16 = channels_u16.saturating_mul(bits_per_sample / 8);
  let byte_rate: u32 = sample_rate_hz.saturating_mul(u32::from(block_align));

  // RIFF chunk.
  w.write_all(b"RIFF")?;
  w.write_all(&(36u32.saturating_add(data_bytes)).to_le_bytes())?;
  w.write_all(b"WAVE")?;

  // fmt chunk.
  w.write_all(b"fmt ")?;
  w.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size.
  w.write_all(&1u16.to_le_bytes())?; // PCM format tag.
  w.write_all(&channels_u16.to_le_bytes())?;
  w.write_all(&sample_rate_hz.to_le_bytes())?;
  w.write_all(&byte_rate.to_le_bytes())?;
  w.write_all(&block_align.to_le_bytes())?;
  w.write_all(&bits_per_sample.to_le_bytes())?;

  // data chunk.
  w.write_all(b"data")?;
  w.write_all(&data_bytes.to_le_bytes())?;
  Ok(())
}

/// Create an audio backend based on the active runtime toggles (`FASTR_*` env vars).
///
/// - `FASTR_AUDIO_BACKEND=wav` + `FASTR_AUDIO_WAV_PATH=...` → [`WavAudioBackend`]
/// - otherwise → [`NullAudioBackend`]
pub fn audio_backend_from_env(
  sample_rate_hz: u32,
  channels: usize,
) -> AudioBackendResult<Box<dyn AudioBackend>> {
  // Prefer the active runtime toggles so library users (and tests) can override env-derived
  // behavior without mutating the process environment.
  let toggles = runtime_toggles();
  let backend = toggles
    .get("FASTR_AUDIO_BACKEND")
    .unwrap_or("null")
    .trim()
    .to_ascii_lowercase();

  match backend.as_str() {
    "" | "null" | "none" | "off" => Ok(Box::new(NullAudioBackend::new(sample_rate_hz, channels))),
    "wav" => {
      let Some(path) = toggles.get("FASTR_AUDIO_WAV_PATH") else {
        return Err(AudioBackendError::MissingWavPath);
      };
      Ok(Box::new(WavAudioBackend::new(
        sample_rate_hz,
        channels,
        path,
      )?))
    }
    other => Err(AudioBackendError::UnknownBackend(other.to_string())),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;
  use std::thread;

  fn all_samples_eq(samples: &[f32], expected: f32) -> bool {
    samples
      .iter()
      .all(|sample| (*sample - expected).abs() < f32::EPSILON)
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

  #[test]
  fn eos_drains_and_freezes_clock() {
    use crate::js::VirtualClock;

    let clock = VirtualClock::new();
    let backend = NullAudioBackend::new(48_000, 1);
    let stream = backend.create_stream();

    stream.enqueue_samples(vec![1.0; 48_000]).unwrap();
    stream.finish();
    // Enqueue after EOS should be rejected.
    assert_eq!(
      stream.enqueue_samples(vec![1.0; 1]),
      Err(AudioStreamEnqueueError::StreamFinished)
    );

    stream.play();
    assert_eq!(stream.ended(), None);

    let mut last = Duration::ZERO;
    clock.advance(Duration::from_millis(500));
    let out0 = backend.render_for_clock(&clock, &mut last);
    assert!(all_samples_eq(&out0, 1.0));
    assert_eq!(stream.current_time(), Duration::from_millis(500));
    assert_eq!(stream.ended(), None);

    clock.advance(Duration::from_millis(500));
    let out1 = backend.render_for_clock(&clock, &mut last);
    assert!(all_samples_eq(&out1, 1.0));
    assert_eq!(stream.current_time(), Duration::from_secs(1));
    assert_eq!(stream.ended(), Some(Duration::from_secs(1)));
    assert!(stream.is_drained());

    // Once drained, additional time should not advance the stream clock.
    clock.advance(Duration::from_secs(1));
    let out2 = backend.render_for_clock(&clock, &mut last);
    assert!(all_samples_eq(&out2, 0.0));
    assert_eq!(stream.current_time(), Duration::from_secs(1));

    // Seeking clears EOS so playback can be restarted.
    stream.seek_to(Duration::ZERO);
    assert_eq!(stream.ended(), None);
    stream.enqueue_samples(vec![2.0; 48_000]).unwrap();
    let out3 = backend.render(24_000);
    assert!(all_samples_eq(&out3, 2.0));
  }

  #[test]
  fn volume_changes_are_ramped() {
    // Use a small sample rate so the ramp spans a small, test-friendly number of frames.
    let backend = NullAudioBackend::new(1_000, 1);
    let ramp_frames = gain_ramp_frames(backend.mixer().sample_rate_hz()) as usize;

    let stream = backend.create_stream();
    stream.play();
    stream.enqueue_samples(vec![1.0; 1_000]).unwrap();

    // Confirm baseline.
    let baseline = backend.render(1);
    assert!(all_samples_eq(&baseline, 1.0));

    // Drop volume to zero and verify we ramp over multiple frames instead of stepping immediately.
    stream.set_volume(0.0);
    let out = backend.render(ramp_frames + 1);

    assert_eq!(out.len(), ramp_frames + 1);
    assert!(
      out[0] > 0.9,
      "expected first frame to still be near previous gain (got {})",
      out[0]
    );
    assert!(
      out[1] < out[0] && out[1] > 0.0,
      "expected a gradual ramp, not a single-step drop (got first two samples: {}, {})",
      out[0],
      out[1]
    );

    for w in out.windows(2) {
      assert!(w[1] <= w[0] + 1e-6, "gain must be monotonic decreasing");
    }

    let last = *out.last().unwrap();
    assert!(
      last.abs() <= 1e-6,
      "expected ramp to reach (near) zero after {} frames (got {})",
      ramp_frames,
      last
    );
  }

  #[test]
  fn audio_preroll_delays_stream_timeline_until_audible() {
    let backend = NullAudioBackend::new(48_000, 1);
    let stream = backend.create_stream();
    stream.set_preroll(Duration::from_millis(100));
    stream.enqueue_samples(vec![1.0; 48_000]).unwrap();

    stream.play();
    assert_eq!(stream.current_time(), Duration::ZERO);
    assert!(!stream.playback_started());
    assert_eq!(stream.playback_started_at(), None);

    // Render just under the preroll threshold: timeline should remain near the start.
    let _ = backend.render(4_799);
    assert_eq!(stream.current_time(), Duration::ZERO);
    assert!(!stream.playback_started());

    // Cross the threshold: playback is now considered audible (timeline starts).
    let _ = backend.render(1);
    assert_eq!(stream.current_time(), Duration::ZERO);
    assert!(stream.playback_started());
    assert_eq!(stream.playback_started_at(), Some(Duration::from_millis(100)));

    // After the preroll, timeline should advance with rendered frames.
    let _ = backend.render(480); // +10ms
    assert_eq!(stream.current_time(), Duration::from_millis(10));
  }
}
