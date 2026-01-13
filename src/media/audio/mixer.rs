use super::timed_queue::TimedAudioQueue;
use super::{AudioClock, AudioOutputInfo};
use std::collections::HashMap;
use std::time::Duration;

pub type AudioStreamId = u64;

#[derive(Debug, Clone, Copy)]
pub struct AudioStreamParams {
  /// Signed nanosecond offset applied to the shared device clock.
  ///
  /// `stream_pts = device_pts + pts_offset_ns`.
  pub pts_offset_ns: i64,
  pub gain: f32,
}

#[derive(Debug)]
struct AudioStreamState {
  params: AudioStreamParams,
  queue: TimedAudioQueue,
}

#[derive(Debug)]
pub struct AudioMixer {
  channels: u16,
  sample_rate: u32,
  max_buffered_duration: Duration,
  streams: HashMap<AudioStreamId, AudioStreamState>,
  scratch: Vec<f32>,
}

impl AudioMixer {
  pub fn new(channels: u16, sample_rate: u32, max_buffered_duration: Duration) -> Self {
    Self {
      channels,
      sample_rate,
      max_buffered_duration,
      streams: HashMap::new(),
      scratch: Vec::new(),
    }
  }

  pub fn add_stream(&mut self, id: AudioStreamId, params: AudioStreamParams) {
    let queue = TimedAudioQueue::new(self.channels, self.sample_rate, self.max_buffered_duration);
    self.streams.insert(id, AudioStreamState { params, queue });
  }

  pub fn remove_stream(&mut self, id: AudioStreamId) {
    self.streams.remove(&id);
  }

  pub fn stream_queue_mut(&mut self, id: AudioStreamId) -> Option<&mut TimedAudioQueue> {
    self.streams.get_mut(&id).map(|stream| &mut stream.queue)
  }

  pub fn set_stream_params(&mut self, id: AudioStreamId, params: AudioStreamParams) {
    if let Some(stream) = self.streams.get_mut(&id) {
      stream.params = params;
    }
  }

  pub fn mix_into(&mut self, out: &mut [f32], device_pts: Duration, frames: usize) {
    let channels = self.channels as usize;
    let needed = frames.saturating_mul(channels);
    assert!(
      out.len() >= needed,
      "AudioMixer output buffer too small: need {needed} samples, got {}",
      out.len()
    );
    out[..needed].fill(0.0);
    if self.streams.is_empty() {
      return;
    }

    if self.scratch.len() < needed {
      self.scratch.resize(needed, 0.0);
    }

    for stream in self.streams.values_mut() {
      self.scratch[..needed].fill(0.0);
      let target_pts = apply_signed_offset(device_pts, stream.params.pts_offset_ns);
      stream
        .queue
        .read_into(&mut self.scratch[..needed], target_pts, frames);
      for (dst, src) in out[..needed].iter_mut().zip(self.scratch[..needed].iter()) {
        *dst += *src * stream.params.gain;
      }
    }
  }

  /// Mix into `out` aligned to an absolute device frame counter.
  ///
  /// `device_frame` should typically represent the *playback* (heard) frame index on the output
  /// device clock, not merely the callback/write cursor. Callers can compute this using
  /// [`AudioClock::frames`] and [`AudioOutputInfo::estimated_latency_frames`].
  pub fn mix_into_frames(&mut self, out: &mut [f32], device_frame: u64, frames: usize) {
    let channels = self.channels as usize;
    let needed = frames.saturating_mul(channels);
    assert!(
      out.len() >= needed,
      "AudioMixer output buffer too small: need {needed} samples, got {}",
      out.len()
    );
    out[..needed].fill(0.0);
    if self.streams.is_empty() {
      return;
    }

    if self.scratch.len() < needed {
      self.scratch.resize(needed, 0.0);
    }

    for stream in self.streams.values_mut() {
      self.scratch[..needed].fill(0.0);
      let target_frame =
        apply_signed_offset_frames(device_frame, stream.params.pts_offset_ns, self.sample_rate);
      stream
        .queue
        .read_into_frames(&mut self.scratch[..needed], target_frame, frames);
      for (dst, src) in out[..needed].iter_mut().zip(self.scratch[..needed].iter()) {
        *dst += *src * stream.params.gain;
      }
    }
  }

  /// Convenience wrapper that mixes based on an [`AudioClock`] + output latency estimate.
  pub fn mix_into_from_backend_clock(
    &mut self,
    out: &mut [f32],
    clock: &AudioClock,
    output_info: &AudioOutputInfo,
    frames: usize,
  ) {
    let played = clock.frames();
    let latency = output_info.estimated_latency_frames();
    let device_frame = played.saturating_sub(latency);
    self.mix_into_frames(out, device_frame, frames);
  }
}

fn apply_signed_offset(base: Duration, offset_ns: i64) -> Duration {
  if offset_ns >= 0 {
    base + Duration::from_nanos(offset_ns as u64)
  } else {
    base.saturating_sub(Duration::from_nanos((-offset_ns) as u64))
  }
}

fn apply_signed_offset_frames(base_frame: u64, offset_ns: i64, sample_rate: u32) -> u64 {
  if offset_ns == 0 || sample_rate == 0 {
    return base_frame;
  }
  const NANOS_PER_SEC: i128 = 1_000_000_000;
  let sr = i128::from(sample_rate);
  let ns = i128::from(offset_ns);
  let abs_ns = ns.unsigned_abs() as i128;
  let abs_frames = (abs_ns.saturating_mul(sr) + (NANOS_PER_SEC / 2)) / NANOS_PER_SEC;
  let signed_frames = if ns >= 0 { abs_frames } else { -abs_frames };
  let shifted = i128::from(base_frame).saturating_add(signed_frames);
  if shifted <= 0 {
    0
  } else if shifted >= i128::from(u64::MAX) {
    u64::MAX
  } else {
    shifted as u64
  }
}
