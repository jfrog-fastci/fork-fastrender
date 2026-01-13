use super::timed_queue::TimedAudioQueue;
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
}

fn apply_signed_offset(base: Duration, offset_ns: i64) -> Duration {
  if offset_ns >= 0 {
    base + Duration::from_nanos(offset_ns as u64)
  } else {
    base.saturating_sub(Duration::from_nanos((-offset_ns) as u64))
  }
}

