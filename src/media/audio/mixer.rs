use super::convert::sanitize_buffer_in_place;
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
    let queue = TimedAudioQueue::with_max_buffered_duration(
      self.channels,
      self.sample_rate,
      self.max_buffered_duration,
    );
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
    assert!( // fastrender-allow-panic
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
      let gain = stream.params.gain;
      if gain == 0.0 || !gain.is_finite() || !gain.is_normal() {
        continue;
      }
      for (dst, src) in out[..needed].iter_mut().zip(self.scratch[..needed].iter()) {
        // Avoid NaN poisoning and denormal slow paths by dropping non-normal values
        // (NaN/Inf/0/subnormals) before they can reach the accumulation math.
        let src = *src;
        if !src.is_normal() {
          continue;
        }
        let scaled = src * gain;
        if scaled.is_normal() {
          let cur = *dst;
          if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
            *dst = 0.0;
          }
          *dst += scaled;
        }
      }
    }

    // Final pass to ensure we never hand NaN/Inf/denormals to the device callback.
    sanitize_buffer_in_place(&mut out[..needed]);
  }

  /// Mix into `out` aligned to an absolute device frame counter.
  ///
  /// `device_frame` is the absolute frame index on the *output timeline* corresponding to the
  /// first frame in `out`.
  ///
  /// Notes:
  /// - Some backends expose a frame counter for "frames written into the callback". That value is a
  ///   good `device_frame` when mixing *the next output buffer*.
  /// - For A/V sync or `currentTime`, callers often need a "time heard now" estimate, which is
  ///   typically `frames_written - estimated_latency_frames`.
  pub fn mix_into_frames(&mut self, out: &mut [f32], device_frame: u64, frames: usize) {
    let channels = self.channels as usize;
    let needed = frames.saturating_mul(channels);
    assert!( // fastrender-allow-panic
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
      let gain = stream.params.gain;
      if gain == 0.0 || !gain.is_finite() || !gain.is_normal() {
        continue;
      }
      for (dst, src) in out[..needed].iter_mut().zip(self.scratch[..needed].iter()) {
        let src = *src;
        if !src.is_normal() {
          continue;
        }
        let scaled = src * gain;
        if scaled.is_normal() {
          let cur = *dst;
          if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
            *dst = 0.0;
          }
          *dst += scaled;
        }
      }
    }

    sanitize_buffer_in_place(&mut out[..needed]);
  }

  /// Convenience wrapper that mixes based on an [`AudioClock`] + output latency estimate.
  ///
  /// This uses `clock.frames() - output_info.estimated_latency_frames()` to align to an estimated
  /// "frames currently being heard" timeline.
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

  /// Convenience wrapper that mixes based on the backend's raw output-frame counter.
  ///
  /// This aligns `device_frame` to `clock.frames()` without applying the latency estimate. This is
  /// typically appropriate when mixing the *next* buffer to hand to the output backend (the backend
  /// will play it after its internal latency).
  pub fn mix_into_from_output_frames(&mut self, out: &mut [f32], clock: &AudioClock, frames: usize) {
    let device_frame = clock.frames();
    self.mix_into_frames(out, device_frame, frames);
  }
}

fn apply_signed_offset(base: Duration, offset_ns: i64) -> Duration {
  if offset_ns >= 0 {
    base.saturating_add(Duration::from_nanos(offset_ns as u64))
  } else {
    base.saturating_sub(Duration::from_nanos(offset_ns.unsigned_abs()))
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::media::audio::{test_signal, AudioStreamConfig, TimedAudioSegment};
  use crate::media::audio_clock::InterpolatedAudioClock;
  use std::sync::Arc;

  fn seg(start_ms: u64, samples: &[f32], sample_rate: u32) -> TimedAudioSegment {
    TimedAudioSegment {
      start_pts: Duration::from_millis(start_ms),
      samples: samples.to_vec(),
      channels: 1,
      sample_rate,
    }
  }

  #[test]
  fn audio_mixer_apply_signed_offset_saturates_on_overflow() {
    let res = std::panic::catch_unwind(|| apply_signed_offset(Duration::MAX, 1));
    assert!(res.is_ok(), "apply_signed_offset should not panic on overflow");
    assert_eq!(res.unwrap(), Duration::MAX);
  }

  #[test]
  fn mixes_multiple_streams_with_gain() {
    let mut mixer = AudioMixer::new(1, 10, Duration::from_secs(10));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 1.0,
      },
    );
    mixer.add_stream(
      2,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 0.5,
      },
    );

    mixer
      .stream_queue_mut(1)
      .unwrap()
      .push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0], 10))
      .unwrap();
    mixer
      .stream_queue_mut(2)
      .unwrap()
      .push_segment(seg(0, &[10.0, 10.0, 10.0, 10.0], 10))
      .unwrap();

    let mut out = vec![0.0; 4];
    mixer.mix_into_frames(&mut out, 0, 4);
    assert_eq!(out, vec![6.0, 7.0, 8.0, 9.0]);
  }

  #[test]
  fn mixer_drops_nan_and_subnormal_samples() {
    let mut mixer = AudioMixer::new(1, 10, Duration::from_secs(10));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 1.0,
      },
    );

    let sub = f32::from_bits(1);
    assert!(!sub.is_normal());
    mixer
      .stream_queue_mut(1)
      .unwrap()
      .push_segment(seg(0, &[f32::NAN, sub, 1.0], 10))
      .unwrap();

    let mut out = vec![0.0; 3];
    mixer.mix_into_frames(&mut out, 0, 3);
    assert_eq!(out, vec![0.0, 0.0, 1.0]);
  }

  #[test]
  fn stream_offset_inserts_silence() {
    let mut mixer = AudioMixer::new(1, 10, Duration::from_secs(10));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 1.0,
      },
    );
    // Offset stream 2 backward by 200ms = 2 frames at 10Hz (delays it relative to device time).
    mixer.add_stream(
      2,
      AudioStreamParams {
        pts_offset_ns: -200_000_000,
        gain: 1.0,
      },
    );

    mixer
      .stream_queue_mut(1)
      .unwrap()
      .push_segment(seg(1000, &[1.0, 2.0, 3.0, 4.0], 10))
      .unwrap();
    mixer
      .stream_queue_mut(2)
      .unwrap()
      .push_segment(seg(1000, &[10.0, 11.0, 12.0, 13.0], 10))
      .unwrap();

    // Mix 4 frames starting at device frame 10 (1s). Stream 2 should contribute on frames 2..4 of
    // this window because it is delayed by 2 frames.
    let mut out = vec![0.0; 4];
    mixer.mix_into_frames(&mut out, 10, 4);
    assert_eq!(out, vec![1.0, 2.0, 13.0, 15.0]);
  }

  #[test]
  fn mix_from_backend_clock_accounts_for_latency() {
    let mut mixer = AudioMixer::new(1, 10, Duration::from_secs(10));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 1.0,
      },
    );
    mixer
      .stream_queue_mut(1)
      .unwrap()
      .push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0], 10))
      .unwrap();

    let inner = Arc::new(InterpolatedAudioClock::new(10));
    inner.advance_frames(4);
    let clock = AudioClock::OutputFrames { clock: inner };
    let output_info = AudioOutputInfo {
      config: AudioStreamConfig::new(10, 1),
      callback_frames: None,
      // Latency of 200ms = 2 frames.
      estimated_output_latency: Duration::from_millis(200),
      backend_name: "test",
    };
    assert_eq!(output_info.estimated_latency_frames(), 2);

    let mut out = vec![0.0; 2];
    // played=4, latency=2 => device_frame=2 => should read samples [3,4].
    mixer.mix_into_from_backend_clock(&mut out, &clock, &output_info, 2);
    assert_eq!(out, vec![3.0, 4.0]);
  }

  #[test]
  fn mix_from_output_frames_uses_write_cursor() {
    let mut mixer = AudioMixer::new(1, 10, Duration::from_secs(10));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 1.0,
      },
    );
    mixer
      .stream_queue_mut(1)
      .unwrap()
      .push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 10))
      .unwrap();

    let inner = Arc::new(InterpolatedAudioClock::new(10));
    inner.advance_frames(4);
    let clock = AudioClock::OutputFrames { clock: inner };
    assert_eq!(clock.frames(), 4);

    let mut out = vec![0.0; 2];
    // frames_written=4 => next buffer should start at frame 4 (samples 5 and 6).
    mixer.mix_into_from_output_frames(&mut out, &clock, 2);
    assert_eq!(out, vec![5.0, 6.0]);
  }

  #[test]
  fn mixer_sums_streams_with_gain() {
    let sample_rate = 1_000;
    let channels = 1;
    let duration = Duration::from_millis(5);
    let impulse = test_signal::impulse(duration, sample_rate, channels);
    let frames = impulse.len() / channels as usize;

    let mut mixer = AudioMixer::new(channels, sample_rate, Duration::from_secs(1));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 0.5,
      },
    );
    mixer.add_stream(
      2,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 0.5,
      },
    );

    for id in [1_u64, 2_u64] {
      mixer
        .stream_queue_mut(id)
        .unwrap()
        .push_segment(TimedAudioSegment {
          start_pts: Duration::ZERO,
          samples: impulse.clone(),
          channels,
          sample_rate,
        })
        .unwrap();
    }

    let mut out = vec![0.0; frames * channels as usize];
    mixer.mix_into_frames(&mut out, 0, frames);

    assert_eq!(out.len(), frames * channels as usize);
    assert_eq!(out[0], 1.0, "0.5 + 0.5 gain should sum to full-scale");
    assert!(out[1..].iter().all(|v| *v == 0.0));

    // Once drained, subsequent mixes should produce silence.
    let mut out2 = vec![1.0; frames * channels as usize];
    mixer.mix_into_frames(&mut out2, frames as u64, frames);
    assert!(out2.iter().all(|v| *v == 0.0));
  }

  #[test]
  fn muted_stream_still_drains_queue() {
    // 10Hz => 1 frame == 100ms (keeps the math exact and makes the test easy to reason about).
    let mut mixer = AudioMixer::new(1, 10, Duration::from_secs(10));
    mixer.add_stream(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 0.0, // muted
      },
    );

    mixer
      .stream_queue_mut(1)
      .unwrap()
      .push_segment(seg(0, &[1.0, 1.0, 1.0, 1.0], 10))
      .unwrap();
    assert_eq!(mixer.stream_queue_mut(1).unwrap().buffered_frames(), 4);

    // Mixing 2 frames while muted must still consume those frames from the queue.
    let mut muted_out = vec![0.0; 2];
    mixer.mix_into_frames(&mut muted_out, 0, 2);
    assert_eq!(muted_out, vec![0.0, 0.0]);
    assert_eq!(mixer.stream_queue_mut(1).unwrap().buffered_frames(), 2);

    // Unmuting should play immediately without a backlog.
    mixer.set_stream_params(
      1,
      AudioStreamParams {
        pts_offset_ns: 0,
        gain: 1.0,
      },
    );
    let mut out = vec![0.0; 4];
    mixer.mix_into_frames(&mut out, 2, 4);
    assert_eq!(out, vec![1.0, 1.0, 0.0, 0.0]);
    assert_eq!(mixer.stream_queue_mut(1).unwrap().buffered_frames(), 0);
  }
}
