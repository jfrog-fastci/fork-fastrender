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

/// Result of a [`TimedAudioQueue::pop_into`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PopResult {
  /// Frames of real audio copied into the output buffer.
  pub audio_frames: u64,
  /// Frames of silence inserted into the output buffer.
  pub silence_frames: u64,
  /// Frames dropped from the queue due to late packets / overlaps during this pop.
  pub dropped_frames: u64,
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
    self
      .start_frame
      .saturating_add(self.remaining_frames(channels))
  }

  fn trim_prefix_frames(&mut self, frames: u64, channels: usize) -> u64 {
    if frames == 0 || channels == 0 {
      return 0;
    }
    let available = self.remaining_frames(channels);
    if available == 0 {
      return 0;
    }
    let to_trim = frames.min(available);
    let skip_samples = (to_trim as usize).saturating_mul(channels);
    self.offset_samples = self.offset_samples.saturating_add(skip_samples);
    self.start_frame = self.start_frame.saturating_add(to_trim);
    to_trim
  }
}

#[derive(Debug)]
pub struct TimedAudioQueue {
  channels: u16,
  sample_rate: u32,
  max_buffered_frames: u64,
  segments: VecDeque<Segment>,
  cursor_frame: Option<u64>,
  inserted_silence_frames: u64,
  dropped_frames: u64,
}

impl TimedAudioQueue {
  /// Number of frames of tolerance allowed between successive `target_pts` values and the internal
  /// cursor.
  ///
  /// In real playback, callers may derive `target_pts` from clocks that are not perfectly
  /// sample-aligned (e.g. wall-clock based timing, or conversions that round to whole nanoseconds).
  /// Those can introduce ±1 frame of jitter when mapped back into frames. Treating that jitter as a
  /// seek would cause repeated cursor resets and audible glitches.
  const TARGET_PTS_TOLERANCE_FRAMES: u64 = 1;

  /// Create a new queue with an unbounded internal buffer.
  ///
  /// This is the simplest constructor and matches the typical decoder-facing usage: packets can be
  /// pushed in arbitrary timestamp order and the consumer can request aligned PCM via `pop_into`.
  ///
  /// Note: for real-time playback pipelines that want to bound memory usage, prefer
  /// [`Self::with_max_buffered_duration`].
  #[must_use]
  pub fn new(channels: u16, sample_rate: u32) -> Self {
    Self::with_max_buffered_duration(channels, sample_rate, Duration::ZERO)
  }

  /// Create a new queue with a maximum buffered duration.
  ///
  /// When the predicted buffered audio exceeds `max_buffered_duration`, pushes return
  /// [`PushError::Backpressure`].
  pub fn with_max_buffered_duration(
    channels: u16,
    sample_rate: u32,
    max_buffered_duration: Duration,
  ) -> Self {
    assert!(channels > 0, "channels must be non-zero"); // fastrender-allow-panic
    assert!(sample_rate > 0, "sample_rate must be non-zero"); // fastrender-allow-panic
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
      inserted_silence_frames: 0,
      dropped_frames: 0,
    }
  }

  /// Convenience constructor with an unbounded internal buffer (no backpressure).
  ///
  /// Equivalent to `TimedAudioQueue::new(channels, sample_rate)`.
  #[must_use]
  pub fn new_unbounded(channels: u16, sample_rate: u32) -> Self {
    Self::new(channels, sample_rate)
  }

  pub fn channels(&self) -> u16 {
    self.channels
  }

  pub fn sample_rate(&self) -> u32 {
    self.sample_rate
  }

  /// Alias for [`Self::sample_rate`], naming the unit explicitly.
  #[must_use]
  pub fn sample_rate_hz(&self) -> u32 {
    self.sample_rate
  }

  #[must_use]
  pub fn inserted_silence_frames(&self) -> u64 {
    self.inserted_silence_frames
  }

  #[must_use]
  pub fn dropped_frames(&self) -> u64 {
    self.dropped_frames
  }

  pub fn buffered_frames(&self) -> u64 {
    let channels = self.channels as usize;
    self
      .segments
      .iter()
      .map(|segment| segment.remaining_frames(channels))
      .sum()
  }

  /// Push a packet (interleaved f32 PCM) at the given PTS.
  ///
  /// This is a convenience wrapper around [`Self::push_segment`].
  pub fn push_packet(&mut self, start_pts: Duration, samples: &[f32]) -> Result<(), PushError> {
    let channels = usize::from(self.channels);
    if channels == 0 || (samples.len() % channels) != 0 {
      return Err(PushError::InvalidSamples);
    }
    self.push_segment(TimedAudioSegment {
      start_pts: start_pts,
      samples: samples.to_vec(),
      channels: self.channels,
      sample_rate: self.sample_rate,
    })
  }

  pub fn reset_cursor(&mut self, target_pts: Duration) {
    let target_frame = duration_to_frames(target_pts, self.sample_rate);
    self.reset_cursor_frames(target_frame);
  }

  pub fn reset_cursor_frames(&mut self, target_frame: u64) {
    self.cursor_frame = Some(target_frame);
    self.prune_before_frame(target_frame);
  }

  pub fn clear(&mut self) {
    self.cursor_frame = None;
    self.segments.clear();
    self.inserted_silence_frames = 0;
    self.dropped_frames = 0;
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
        self.dropped_frames = self.dropped_frames.saturating_add(frames);
        return Ok(());
      }
      if start_frame < cursor_frame {
        let skip_frames = cursor_frame - start_frame;
        let trimmed = internal.trim_prefix_frames(skip_frames, channels);
        self.dropped_frames = self.dropped_frames.saturating_add(trimmed);
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
          self.dropped_frames = self
            .dropped_frames
            .saturating_add(internal.remaining_frames(channels));
          return Ok(());
        }
        let trimmed = internal.trim_prefix_frames(overlap, channels);
        self.dropped_frames = self.dropped_frames.saturating_add(trimmed);
      }
    }

    let add_frames = internal.remaining_frames(channels);
    if add_frames > 0 && self.max_buffered_frames != u64::MAX {
      let buffered = self.buffered_frames();
      let internal_end = internal.end_frame(channels);
      // If the new segment overlaps *later* segments, `normalize()` will trim/drop those later
      // segments. Account for those freed frames in the backpressure calculation so callers don't
      // see spurious backpressure when pushing "replacement" audio that simply covers existing
      // buffered ranges.
      let mut freed_frames: u64 = 0;
      for seg in self.segments.iter().skip(insert_idx) {
        if seg.start_frame >= internal_end {
          break;
        }
        let seg_end = seg.end_frame(channels);
        let overlap_end = seg_end.min(internal_end);
        if overlap_end > seg.start_frame {
          freed_frames = freed_frames.saturating_add(overlap_end - seg.start_frame);
        }
      }

      let predicted = buffered
        .saturating_add(add_frames)
        .saturating_sub(freed_frames);

      if predicted > self.max_buffered_frames {
        return Err(PushError::Backpressure);
      }
    }

    self.segments.insert(insert_idx, internal);
    self.normalize();
    Ok(())
  }

  /// Pop interleaved samples aligned to `target_pts` into `out`.
  ///
  /// This is a convenience wrapper around [`Self::read_into`] that derives the requested frame
  /// count from `out.len()` and reports per-call statistics, while also updating the queue's
  /// cumulative `inserted_silence_frames` / `dropped_frames` counters.
  pub fn pop_into(&mut self, target_pts: Duration, out: &mut [f32]) -> PopResult {
    let channels = self.channels as usize;
    if channels == 0 {
      out.fill(0.0);
      return PopResult::default();
    }
    let frames = out.len() / channels;
    if frames == 0 {
      out.fill(0.0);
      return PopResult::default();
    }
    // Ensure the entire buffer is deterministically initialized even if the caller passes a slice
    // whose length is not a multiple of `channels`.
    out.fill(0.0);

    let dropped_before = self.dropped_frames;
    let res = self.read_into(out, target_pts, frames);
    let dropped_now = self.dropped_frames.saturating_sub(dropped_before);

    PopResult {
      audio_frames: res.frames_audio as u64,
      silence_frames: res.frames_silence as u64,
      dropped_frames: dropped_now,
    }
  }

  pub fn read_into(&mut self, out: &mut [f32], target_pts: Duration, frames: usize) -> ReadResult {
    let target_frame = duration_to_frames(target_pts, self.sample_rate);
    self.read_into_frames_with_tolerance(out, target_frame, frames, Self::TARGET_PTS_TOLERANCE_FRAMES)
  }

  /// Reads samples aligned to an absolute frame index on the queue's timeline.
  ///
  /// This is a lower-level variant of [`Self::read_into`] that avoids `Duration` conversions. It is
  /// useful when the caller already has a sample-accurate frame clock (e.g. an audio backend
  /// playhead counter).
  pub fn read_into_frames(&mut self, out: &mut [f32], target_frame: u64, frames: usize) -> ReadResult {
    // When the caller supplies explicit frame counts, treat any mismatch as a real discontinuity.
    // Frame clocks are expected to be sample-accurate, so we don't apply the `target_pts` tolerance
    // here.
    self.read_into_frames_with_tolerance(out, target_frame, frames, 0)
  }

  fn read_into_frames_with_tolerance(
    &mut self,
    out: &mut [f32],
    target_frame: u64,
    frames: usize,
    tolerance_frames: u64,
  ) -> ReadResult {
    let channels = self.channels as usize;
    let needed_samples = frames
      .checked_mul(channels)
      .expect("frames * channels should not overflow"); // fastrender-allow-unwrap
    assert!( // fastrender-allow-panic
      out.len() >= needed_samples,
      "TimedAudioQueue output buffer too small: need {needed_samples} samples, got {}",
      out.len()
    );

    match self.cursor_frame {
      None => {
        self.cursor_frame = Some(target_frame);
      }
      Some(cursor) if cursor != target_frame => {
        let diff = cursor.abs_diff(target_frame);
        if diff > tolerance_frames {
          self.reset_cursor_frames(target_frame);
        }
      }
      _ => {}
    }

    let start_frame = self.cursor_frame.expect("cursor_frame initialized"); // fastrender-allow-unwrap
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
        self.dropped_frames = self
          .dropped_frames
          .saturating_add(front.remaining_frames(channels));
        self.segments.pop_front();
        continue;
      }

      if front.start_frame < cur_frame {
        let overlap = cur_frame - front.start_frame;
        let trimmed = front.trim_prefix_frames(overlap, channels);
        self.dropped_frames = self.dropped_frames.saturating_add(trimmed);
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
      let _ = front.trim_prefix_frames(to_copy_frames as u64, channels);
      if front.remaining_frames(channels) == 0 {
        self.segments.pop_front();
      }
    }

    self.cursor_frame = Some(end_frame);
    self.prune_before_frame(end_frame);

    let frames_silence = frames.saturating_sub(frames_audio);
    self.inserted_silence_frames = self
      .inserted_silence_frames
      .saturating_add(frames_silence as u64);
    ReadResult {
      frames,
      frames_audio,
      frames_silence,
    }
  }

  fn prune_before_frame(&mut self, frame: u64) {
    let channels = self.channels as usize;
    loop {
      let Some(front) = self.segments.front_mut() else {
        break;
      };

      if front.end_frame(channels) <= frame {
        self.dropped_frames = self
          .dropped_frames
          .saturating_add(front.remaining_frames(channels));
        self.segments.pop_front();
        continue;
      }

      if front.start_frame < frame {
        let skip = frame - front.start_frame;
        let trimmed = front.trim_prefix_frames(skip, channels);
        self.dropped_frames = self.dropped_frames.saturating_add(trimmed);
        if front.remaining_frames(channels) == 0 {
          self.segments.pop_front();
          continue;
        }
      }

      // Because segments are kept in timestamp order and normalized to be non-overlapping, at most
      // one segment (the new front) can straddle `frame`.
      break;
    }
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
            self.dropped_frames = self.dropped_frames.saturating_add(seg.remaining_frames(channels));
            continue;
          }
          let trimmed = seg.trim_prefix_frames(overlap, channels);
          self.dropped_frames = self.dropped_frames.saturating_add(trimmed);
        }

        if last.end_frame(channels) == seg.start_frame
          && last.offset_samples == 0
          && seg.offset_samples == 0
        {
          let tail = normalized
            .pop_back()
            .expect("last segment exists"); // fastrender-allow-unwrap
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
  if sample_rate == 0 {
    return 0;
  }
  let nanos = duration.as_nanos();
  let sr = sample_rate as u128;
  // Round to the nearest frame to avoid consistent drift from truncation.
  let scaled = nanos.saturating_mul(sr);
  let frames = scaled.saturating_add(NANOS_PER_SEC / 2) / NANOS_PER_SEC;
  u64::try_from(frames).unwrap_or(u64::MAX)
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

  fn seg_stereo(start_ms: u64, samples: &[f32]) -> TimedAudioSegment {
    TimedAudioSegment {
      start_pts: Duration::from_millis(start_ms),
      samples: samples.to_vec(),
      channels: 2,
      sample_rate: 10,
    }
  }

  #[test]
  fn contiguous_segments_read_back_continuously() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
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
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
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
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
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
  fn timed_audio_queue_exact_alignment() {
    let mut q = TimedAudioQueue::new(2, 10);
    let samples = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
    q.push_packet(Duration::ZERO, &samples).unwrap();

    let mut out = vec![0.0; samples.len()];
    q.pop_into(Duration::ZERO, &mut out);

    assert_eq!(out, samples);
    assert_eq!(q.inserted_silence_frames(), 0);
    assert_eq!(q.dropped_frames(), 0);
  }

  #[test]
  fn timed_audio_queue_gap_insertion() {
    let mut q = TimedAudioQueue::new(1, 10);
    q.push_packet(Duration::from_secs(1), &[1.0, 2.0, 3.0])
      .unwrap();

    // Request 1s (10 frames) of silence + 3 frames of audio.
    let mut out = vec![0.0; 13];
    q.pop_into(Duration::ZERO, &mut out);

    assert_eq!(out[..10], [0.0; 10]);
    assert_eq!(out[10..], [1.0, 2.0, 3.0]);
    assert_eq!(q.inserted_silence_frames(), 10);
    assert_eq!(q.dropped_frames(), 0);
  }

  #[test]
  fn timed_audio_queue_overlap_drops() {
    let mut q = TimedAudioQueue::new(1, 10);
    q.push_packet(Duration::ZERO, &[1.0, 2.0, 3.0, 4.0, 5.0])
      .unwrap(); // frames 0..5
    q.push_packet(Duration::from_millis(300), &[10.0, 11.0, 12.0, 13.0])
      .unwrap(); // frames 3..7 (overlaps 3..5)

    let mut out = vec![0.0; 7];
    q.pop_into(Duration::ZERO, &mut out);

    assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 12.0, 13.0]);
    assert_eq!(q.inserted_silence_frames(), 0);
    assert_eq!(q.dropped_frames(), 2);
  }

  #[test]
  fn timed_audio_queue_out_of_order_packets() {
    let mut q = TimedAudioQueue::new(1, 10);
    // Push later audio first.
    q.push_packet(Duration::from_secs(1), &[10.0, 11.0]).unwrap();
    q.push_packet(Duration::ZERO, &[1.0, 2.0]).unwrap();

    let mut out = vec![0.0; 12];
    q.pop_into(Duration::ZERO, &mut out);

    assert_eq!(out[..2], [1.0, 2.0]);
    assert_eq!(out[2..10], [0.0; 8]);
    assert_eq!(out[10..], [10.0, 11.0]);
    assert_eq!(q.inserted_silence_frames(), 8);
    assert_eq!(q.dropped_frames(), 0);
  }

  #[test]
  fn timed_audio_queue_partial_consumption_continues() {
    let mut q = TimedAudioQueue::new(1, 10);
    q.push_packet(Duration::ZERO, &[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();

    let mut out1 = vec![0.0; 2];
    q.pop_into(Duration::ZERO, &mut out1);
    assert_eq!(out1, [1.0, 2.0]);

    // Next read starts exactly at frame 2 => 200ms at 10Hz.
    let mut out2 = vec![0.0; 3];
    q.pop_into(Duration::from_millis(200), &mut out2);
    assert_eq!(out2, [3.0, 4.0, 5.0]);
    assert_eq!(q.inserted_silence_frames(), 0);
    assert_eq!(q.dropped_frames(), 0);
  }

  #[test]
  fn reset_cursor_seeks_forward() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
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
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
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

  #[test]
  fn overlap_with_future_segment_does_not_trigger_spurious_backpressure() {
    // 10 Hz sample rate => 1 frame = 100 ms. Buffer cap: 20 frames.
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(2));

    // Existing audio covers frames 10..20 (1s..2s).
    q.push_segment(seg(
      1000,
      &[
        100.0, 101.0, 102.0, 103.0, 104.0, 105.0, 106.0, 107.0, 108.0, 109.0,
      ],
    ))
    .unwrap();

    // New audio covers frames 0..15 (0s..1.5s), overlapping the existing segment by 5 frames.
    // Total buffered frames after trimming overlap should be 15 + 5 = 20 (fits exactly).
    q.push_segment(seg(
      0,
      &[
        0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0,
      ],
    ))
    .unwrap();

    assert_eq!(q.buffered_frames(), 20);

    let mut out = vec![0.0; 20];
    q.read_into(&mut out, Duration::ZERO, 20);
    assert_eq!(
      out,
      vec![
        0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0,
        105.0, 106.0, 107.0, 108.0, 109.0,
      ]
    );
  }

  #[test]
  fn stereo_gap_inserts_silence() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(2, 10, Duration::from_secs(10));
    // 1 frame of stereo audio (2 samples), then another frame after a 1-frame gap.
    q.push_segment(seg_stereo(0, &[1.0, 2.0])).unwrap();
    q.push_segment(seg_stereo(200, &[3.0, 4.0])).unwrap();

    // Read 3 frames from t=0.
    let mut out = vec![0.0; 3 * 2];
    let res = q.read_into(&mut out, Duration::ZERO, 3);
    assert_eq!(res.frames_audio, 2);
    assert_eq!(res.frames_silence, 1);
    assert_eq!(out, vec![1.0, 2.0, 0.0, 0.0, 3.0, 4.0]);
  }

  #[test]
  fn invalid_interleaved_sample_count_rejected() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(2, 10, Duration::from_secs(10));
    // 3 samples cannot be evenly divided into 2-channel frames.
    let err = q.push_segment(seg_stereo(0, &[1.0, 2.0, 3.0])).unwrap_err();
    assert_eq!(err, PushError::InvalidSamples);
  }

  #[test]
  fn format_mismatch_rejected() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
    // Wrong channel count.
    let err = q.push_segment(TimedAudioSegment {
      start_pts: Duration::ZERO,
      samples: vec![1.0, 2.0],
      channels: 2,
      sample_rate: 10,
    })
    .unwrap_err();
    assert_eq!(err, PushError::FormatMismatch);

    // Wrong sample rate.
    let err = q.push_segment(TimedAudioSegment {
      start_pts: Duration::ZERO,
      samples: vec![1.0, 2.0],
      channels: 1,
      sample_rate: 11,
    })
    .unwrap_err();
    assert_eq!(err, PushError::FormatMismatch);
  }

  #[test]
  fn read_into_frames_can_seek_and_skip_audio() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
      .unwrap();

    let mut out1 = vec![0.0; 2];
    q.read_into_frames(&mut out1, 0, 2);
    assert_eq!(out1, vec![1.0, 2.0]);

    // Jump forward by one frame (target=3 instead of expected=2). Because this uses a frame-based
    // clock, any mismatch is treated as a real discontinuity and the skipped frame is dropped.
    let mut out2 = vec![0.0; 2];
    q.read_into_frames(&mut out2, 3, 2);
    assert_eq!(out2, vec![4.0, 5.0]);
  }

  #[test]
  fn backpressure_enforced_by_buffered_frames() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(1)); // 10 frames max.
    q.push_segment(seg(0, &[1.0; 10])).unwrap();

    // Exceeds capacity by 1 frame.
    assert_eq!(
      q.push_segment(seg(1000, &[2.0])),
      Err(PushError::Backpressure)
    );

    // Consume half the buffer, then pushing additional frames should succeed.
    let mut out = vec![0.0; 5];
    q.read_into(&mut out, Duration::ZERO, 5);
    assert_eq!(out, vec![1.0; 5]);

    q.push_segment(seg(1000, &[2.0; 5])).unwrap();
    assert_eq!(q.buffered_frames(), 10);
  }

  #[test]
  fn out_of_order_contiguous_segments_merge() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
    q.push_segment(seg(400, &[5.0, 6.0, 7.0, 8.0]))
      .unwrap();
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0]))
      .unwrap();

    assert_eq!(q.segments.len(), 1, "contiguous segments should merge even if pushed out-of-order");

    let mut out = vec![0.0; 8];
    q.read_into(&mut out, Duration::ZERO, 8);
    assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
  }

  #[test]
  fn out_of_order_overlap_trims_later_segment() {
    let mut q = TimedAudioQueue::with_max_buffered_duration(1, 10, Duration::from_secs(10));
    // Overlapping segment pushed first.
    q.push_segment(seg(200, &[5.0, 6.0, 7.0, 8.0]))
      .unwrap();
    // Earlier segment pushed later should take precedence for the overlap region.
    q.push_segment(seg(0, &[1.0, 2.0, 3.0, 4.0]))
      .unwrap();

    let mut out = vec![0.0; 6];
    q.read_into(&mut out, Duration::ZERO, 6);
    assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 7.0, 8.0]);
  }
}
