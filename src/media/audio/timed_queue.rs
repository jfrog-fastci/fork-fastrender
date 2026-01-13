use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TimedAudioSegment {
  pub start_pts: Duration,
  pub samples: Vec<f32>,
  pub channels: u16,
  pub sample_rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
  Backpressure,
  FormatMismatch,
  InvalidSamples,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadResult {
  pub frames: usize,
  pub frames_audio: usize,
  pub frames_silence: usize,
}

#[derive(Debug)]
struct Segment {
  start_frame: u64,
  samples: Vec<f32>,
  offset_samples: usize,
}

impl Segment {
  fn remaining_frames(&self, channels: usize) -> u64 {
    let remaining_samples = self.samples.len().saturating_sub(self.offset_samples);
    (remaining_samples / channels) as u64
  }

  fn end_frame(&self, channels: usize) -> u64 {
    self.start_frame + self.remaining_frames(channels)
  }

  fn trim_prefix_frames(&mut self, frames: u64, channels: usize) {
    let skip_samples = frames as usize * channels;
    self.offset_samples = self.offset_samples.saturating_add(skip_samples);
    self.start_frame = self.start_frame.saturating_add(frames);
  }
}

#[derive(Debug)]
pub struct TimedAudioQueue {
  channels: u16,
  sample_rate: u32,
  max_buffered_frames: u64,
  segments: VecDeque<Segment>,
  cursor_frame: Option<u64>,
}

impl TimedAudioQueue {
  /// Number of frames of tolerance allowed between successive `target_pts` values and the internal
  /// cursor.
  ///
  /// In real playback, `target_pts` may be derived from an audio backend clock that converts frames
  /// ↔ `Duration` via `f64` (see `AudioClock::time`). Those conversions can introduce ±1 frame of
  /// jitter when mapped back into frames. Treating that jitter as a seek would cause repeated
  /// cursor resets and audible glitches.
  const TARGET_PTS_TOLERANCE_FRAMES: u64 = 1;

  pub fn new(channels: u16, sample_rate: u32, max_buffered_duration: Duration) -> Self {
    assert!(channels > 0, "channels must be non-zero");
    assert!(sample_rate > 0, "sample_rate must be non-zero");
    let max_buffered_frames = if max_buffered_duration == Duration::ZERO {
      u64::MAX
    } else {
      duration_to_frames(max_buffered_duration, sample_rate)
    };
    Self {
      channels,
      sample_rate,
      max_buffered_frames,
      segments: VecDeque::new(),
      cursor_frame: None,
    }
  }

  pub fn channels(&self) -> u16 {
    self.channels
  }

  pub fn sample_rate(&self) -> u32 {
    self.sample_rate
  }

  pub fn buffered_frames(&self) -> u64 {
    let channels = self.channels as usize;
    self
      .segments
      .iter()
      .map(|segment| segment.remaining_frames(channels))
      .sum()
  }

  pub fn reset_cursor(&mut self, target_pts: Duration) {
    let target_frame = duration_to_frames(target_pts, self.sample_rate);
    self.cursor_frame = Some(target_frame);
    self.prune_before_frame(target_frame);
  }

  pub fn clear(&mut self) {
    self.cursor_frame = None;
    self.segments.clear();
  }

  pub fn push_segment(&mut self, segment: TimedAudioSegment) -> Result<(), PushError> {
    if segment.channels != self.channels || segment.sample_rate != self.sample_rate {
      return Err(PushError::FormatMismatch);
    }
    let channels = self.channels as usize;
    if segment.samples.len() % channels != 0 {
      return Err(PushError::InvalidSamples);
    }

    if let Some(cursor) = self.cursor_frame {
      self.prune_before_frame(cursor);
    }

    let start_frame = duration_to_frames(segment.start_pts, self.sample_rate);
    let frames = (segment.samples.len() / channels) as u64;
    let end_frame = start_frame.saturating_add(frames);

    let mut internal = Segment {
      start_frame,
      samples: segment.samples,
      offset_samples: 0,
    };

    let cursor_frame = self.cursor_frame.unwrap_or(0);
    if self.cursor_frame.is_some() {
      if end_frame <= cursor_frame {
        return Ok(());
      }
      if start_frame < cursor_frame {
        let skip_frames = cursor_frame - start_frame;
        internal.trim_prefix_frames(skip_frames, channels);
      }
    }

    if internal.remaining_frames(channels) == 0 {
      return Ok(());
    }

    let insert_idx = self
      .segments
      .iter()
      .position(|existing| existing.start_frame > internal.start_frame)
      .unwrap_or(self.segments.len());

    if insert_idx > 0 {
      let prev_end = self.segments[insert_idx - 1].end_frame(channels);
      if internal.start_frame < prev_end {
        let overlap = prev_end - internal.start_frame;
        if overlap >= internal.remaining_frames(channels) {
          return Ok(());
        }
        internal.trim_prefix_frames(overlap, channels);
      }
    }

    let add_frames = internal.remaining_frames(channels);
    if add_frames > 0 && self.max_buffered_frames != u64::MAX {
      let buffered = self.buffered_frames();
      if buffered.saturating_add(add_frames) > self.max_buffered_frames {
        return Err(PushError::Backpressure);
      }
    }

    self.segments.insert(insert_idx, internal);
    self.normalize();
    Ok(())
  }

  pub fn read_into(&mut self, out: &mut [f32], target_pts: Duration, frames: usize) -> ReadResult {
    let channels = self.channels as usize;
    let needed_samples = frames
      .checked_mul(channels)
      .expect("frames * channels should not overflow");
    assert!(
      out.len() >= needed_samples,
      "TimedAudioQueue output buffer too small: need {needed_samples} samples, got {}",
      out.len()
    );

    let target_frame = duration_to_frames(target_pts, self.sample_rate);
    match self.cursor_frame {
      None => {
        self.cursor_frame = Some(target_frame);
      }
      Some(cursor) if cursor != target_frame => {
        let diff = cursor.abs_diff(target_frame);
        if diff > Self::TARGET_PTS_TOLERANCE_FRAMES {
          self.reset_cursor(target_pts);
        }
      }
      _ => {}
    }

    let start_frame = self.cursor_frame.expect("cursor_frame initialized");
    let end_frame = start_frame.saturating_add(frames as u64);

    out[..needed_samples].fill(0.0);

    let mut frames_audio: usize = 0;
    let mut write_samples: usize = 0;
    let mut cur_frame: u64 = start_frame;

    while write_samples < needed_samples {
      let remaining_frames = (needed_samples - write_samples) / channels;
      if remaining_frames == 0 {
        break;
      }

      let Some(front) = self.segments.front_mut() else {
        break;
      };

      if front.end_frame(channels) <= cur_frame {
        self.segments.pop_front();
        continue;
      }

      if front.start_frame < cur_frame {
        let overlap = cur_frame - front.start_frame;
        front.trim_prefix_frames(overlap, channels);
        if front.remaining_frames(channels) == 0 {
          self.segments.pop_front();
        }
        continue;
      }

      if front.start_frame > cur_frame {
        let gap_frames = (front.start_frame - cur_frame) as usize;
        let skip = gap_frames.min(remaining_frames);
        write_samples += skip * channels;
        cur_frame = cur_frame.saturating_add(skip as u64);
        continue;
      }

      let available_frames = front.remaining_frames(channels) as usize;
      let to_copy_frames = available_frames.min(remaining_frames);
      let sample_count = to_copy_frames * channels;
      let src_start = front.offset_samples;
      let src_end = src_start + sample_count;
      out[write_samples..write_samples + sample_count]
        .copy_from_slice(&front.samples[src_start..src_end]);
      write_samples += sample_count;
      frames_audio += to_copy_frames;
      cur_frame = cur_frame.saturating_add(to_copy_frames as u64);
      front.trim_prefix_frames(to_copy_frames as u64, channels);
      if front.remaining_frames(channels) == 0 {
        self.segments.pop_front();
      }
    }

    self.cursor_frame = Some(end_frame);
    self.prune_before_frame(end_frame);
    self.normalize();

    let frames_silence = frames.saturating_sub(frames_audio);
    ReadResult {
      frames,
      frames_audio,
      frames_silence,
    }
  }

  fn prune_before_frame(&mut self, frame: u64) {
    let channels = self.channels as usize;
    let mut new = VecDeque::with_capacity(self.segments.len());
    while let Some(mut seg) = self.segments.pop_front() {
      if seg.end_frame(channels) <= frame {
        continue;
      }
      if seg.start_frame < frame {
        let skip = frame - seg.start_frame;
        seg.trim_prefix_frames(skip, channels);
      }
      if seg.remaining_frames(channels) > 0 {
        new.push_back(seg);
      }
    }
    self.segments = new;
  }

  fn normalize(&mut self) {
    let channels = self.channels as usize;
    if self.segments.len() <= 1 {
      return;
    }

    let mut normalized: VecDeque<Segment> = VecDeque::with_capacity(self.segments.len());
    while let Some(mut seg) = self.segments.pop_front() {
      if seg.remaining_frames(channels) == 0 {
        continue;
      }

      if let Some(last) = normalized.back_mut() {
        let last_end = last.end_frame(channels);
        if seg.start_frame < last_end {
          let overlap = last_end - seg.start_frame;
          if overlap >= seg.remaining_frames(channels) {
            continue;
          }
          seg.trim_prefix_frames(overlap, channels);
        }

        if last.end_frame(channels) == seg.start_frame
          && last.offset_samples == 0
          && seg.offset_samples == 0
        {
          let tail = normalized.pop_back().expect("last segment exists");
          let mut merged_samples = tail.samples;
          merged_samples.extend(seg.samples);
          normalized.push_back(Segment {
            start_frame: tail.start_frame,
            samples: merged_samples,
            offset_samples: 0,
          });
          continue;
        }
      }

      normalized.push_back(seg);
    }

    self.segments = normalized;
  }
}

fn duration_to_frames(duration: Duration, sample_rate: u32) -> u64 {
  const NANOS_PER_SEC: u128 = 1_000_000_000;
  let nanos = duration.as_nanos();
  let sr = sample_rate as u128;
  // Round to the nearest frame to avoid consistent drift from truncation.
  ((nanos * sr + NANOS_PER_SEC / 2) / NANOS_PER_SEC) as u64
}

#[cfg(test)]
mod tests {
  use super::*;

  fn seg(start_ms: u64, samples: &[f32]) -> TimedAudioSegment {
    TimedAudioSegment {
      start_pts: Duration::from_millis(start_ms),
      samples: samples.to_vec(),
      channels: 1,
      sample_rate: 10,
    }
  }

  #[test]
  fn contiguous_segments_read_back_continuously() {
    let mut q = TimedAudioQueue::new(1, 10, Duration::from_secs(10));
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0]))
      .unwrap();
    q.push_segment(seg(400, &[5.0, 6.0, 7.0, 8.0]))
      .unwrap();
    assert_eq!(q.segments.len(), 1, "contiguous segments should merge");

    let mut out = vec![0.0; 8];
    let res = q.read_into(&mut out, Duration::ZERO, 8);
    assert_eq!(
      res,
      ReadResult {
        frames: 8,
        frames_audio: 8,
        frames_silence: 0,
      }
    );
    assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
  }

  #[test]
  fn gap_inserts_silence() {
    let mut q = TimedAudioQueue::new(1, 10, Duration::from_secs(10));
    q.push_segment(seg(0, &[1.0, 2.0])).unwrap();
    q.push_segment(seg(400, &[3.0, 4.0])).unwrap();

    let mut out1 = vec![0.0; 3];
    q.read_into(&mut out1, Duration::ZERO, 3);
    assert_eq!(out1, vec![1.0, 2.0, 0.0]);

    let mut out2 = vec![0.0; 3];
    q.read_into(&mut out2, Duration::from_millis(300), 3);
    assert_eq!(out2, vec![0.0, 3.0, 4.0]);
  }

  #[test]
  fn overlap_drops_samples() {
    let mut q = TimedAudioQueue::new(1, 10, Duration::from_secs(10));
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0]))
      .unwrap();
    // Starts at 200ms (2 frames) and overlaps with the first segment.
    q.push_segment(seg(200, &[5.0, 6.0, 7.0, 8.0]))
      .unwrap();

    let mut out = vec![0.0; 6];
    q.read_into(&mut out, Duration::ZERO, 6);
    assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 7.0, 8.0]);
  }

  #[test]
  fn reset_cursor_seeks_forward() {
    let mut q = TimedAudioQueue::new(1, 10, Duration::from_secs(10));
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
      .unwrap();

    let mut out1 = vec![0.0; 2];
    q.read_into(&mut out1, Duration::ZERO, 2);
    assert_eq!(out1, vec![1.0, 2.0]);

    q.reset_cursor(Duration::from_millis(400));
    let mut out2 = vec![0.0; 2];
    q.read_into(&mut out2, Duration::from_millis(400), 2);
    assert_eq!(out2, vec![5.0, 6.0]);
  }

  #[test]
  fn small_target_pts_jitter_does_not_force_seek() {
    let mut q = TimedAudioQueue::new(1, 10, Duration::from_secs(10));
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
      .unwrap();

    let mut out1 = vec![0.0; 2];
    q.read_into(&mut out1, Duration::ZERO, 2);
    assert_eq!(out1, vec![1.0, 2.0]);

    // We would normally pass `target_pts = 200ms` for the next read (2 frames at 10Hz), but a clock
    // derived from `f64` seconds can produce timestamps that round back to a neighboring frame.
    // 250ms rounds to 3 frames (off by +1), and the queue should still continue smoothly.
    let mut out2 = vec![0.0; 2];
    q.read_into(&mut out2, Duration::from_millis(250), 2);
    assert_eq!(out2, vec![3.0, 4.0]);
  }
}
