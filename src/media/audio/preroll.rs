use std::collections::VecDeque;

/// A tiny, deterministic preroll + drain queue for interleaved audio samples.
///
/// This is intentionally minimal: it does not resample, it does not know about timestamps, and it
/// does not push silence. It simply holds samples until playback starts (preroll) and then allows
/// the caller to pull fixed-size frame blocks until drained.
#[derive(Debug, Clone)]
pub struct PrerollQueue {
  channels: u16,
  preroll_frames: usize,
  started: bool,
  eos: bool,
  buf: VecDeque<f32>,
}

impl PrerollQueue {
  pub fn new(channels: u16, preroll_frames: usize) -> Self {
    assert!(channels > 0, "channels must be > 0");
    Self {
      channels,
      preroll_frames,
      started: false,
      eos: false,
      buf: VecDeque::new(),
    }
  }

  pub fn channels(&self) -> u16 {
    self.channels
  }

  pub fn preroll_frames(&self) -> usize {
    self.preroll_frames
  }

  pub fn has_started(&self) -> bool {
    self.started
  }

  pub fn queued_frames(&self) -> usize {
    self.buf.len() / self.channels as usize
  }

  pub fn set_eos(&mut self) {
    self.eos = true;
  }

  pub fn is_drained(&self) -> bool {
    self.eos && self.started && self.buf.is_empty()
  }

  pub fn push(&mut self, samples: &[f32]) {
    let channels_usize = self.channels as usize;
    assert_eq!(
      samples.len() % channels_usize,
      0,
      "pushed samples must be frame-aligned (len % channels == 0)"
    );
    self.buf.extend(samples.iter().copied());
  }

  /// Pop up to `max_frames` frames from the queue.
  ///
  /// - Before preroll is satisfied, this returns an empty buffer (unless EOS was set).
  /// - Once started, it returns as many frames as are currently available up to `max_frames`.
  pub fn pop_frames(&mut self, max_frames: usize) -> Vec<f32> {
    if max_frames == 0 {
      return Vec::new();
    }

    if !self.started {
      if self.eos || self.queued_frames() >= self.preroll_frames {
        self.started = true;
      } else {
        return Vec::new();
      }
    }

    let frames = self.queued_frames().min(max_frames);
    let samples_to_pop = frames * self.channels as usize;

    let mut out = Vec::with_capacity(samples_to_pop);
    for _ in 0..samples_to_pop {
      out.push(self.buf.pop_front().expect("buffer length checked"));
    }
    out
  }
}

#[cfg(test)]
mod tests {
  use super::PrerollQueue;
  use crate::media::audio::test_signal;
  use std::time::Duration;

  #[test]
  fn preroll_holds_back_until_threshold() {
    let sample_rate = 1_000;
    let channels = 1;

    let mut q = PrerollQueue::new(channels, 4);
    q.push(&test_signal::impulse_duration(
      Duration::from_millis(3),
      sample_rate,
      channels,
    ));

    let out = q.pop_frames(10);
    assert!(out.is_empty(), "should not output before preroll is satisfied");

    q.push(&[0.0]); // one more frame to meet preroll
    let out = q.pop_frames(10);
    assert_eq!(out.len(), 4);
    assert_eq!(out[0], 1.0);
    assert!(out[1..].iter().all(|v| *v == 0.0));
  }

  #[test]
  fn eos_bypasses_preroll_and_drains() {
    let sample_rate = 1_000;
    let channels = 1;

    let mut q = PrerollQueue::new(channels, 10);
    q.push(&test_signal::impulse_duration(
      Duration::from_millis(3),
      sample_rate,
      channels,
    ));
    q.set_eos();

    let out = q.pop_frames(10);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0], 1.0);

    assert!(q.is_drained(), "queue should be drained after consuming all samples");
    assert!(q.pop_frames(10).is_empty());
  }
}
