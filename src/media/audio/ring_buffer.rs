use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy)]
pub(crate) struct GainRamp {
  pub(crate) current_gain: f32,
  pub(crate) target_gain: f32,
  pub(crate) step: f32,
  pub(crate) frames_remaining: u32,
}

impl GainRamp {
  pub(crate) fn advance_frame(&mut self) {
    if self.frames_remaining == 0 {
      return;
    }
    self.current_gain += self.step;
    self.frames_remaining -= 1;
    if self.frames_remaining == 0 {
      self.current_gain = self.target_gain;
      self.step = 0.0;
    }
  }
}
/// A minimal-lock ring buffer for interleaved f32 PCM samples.
///
/// - The audio callback thread is the sole consumer.
/// - Producers are serialized via a small mutex; the consumer never locks.
pub struct AudioRingBuffer {
  buf: Box<[UnsafeCell<f32>]>,
  capacity: usize,
  read: AtomicUsize,
  write: AtomicUsize,
  write_lock: Mutex<()>,
}

unsafe impl Send for AudioRingBuffer {}
unsafe impl Sync for AudioRingBuffer {}

impl AudioRingBuffer {
  #[must_use]
  pub fn new(capacity: usize) -> Self {
    let capacity = capacity.max(1);
    let mut buf = Vec::with_capacity(capacity);
    buf.resize_with(capacity, || UnsafeCell::new(0.0));
    Self {
      buf: buf.into_boxed_slice(),
      capacity,
      read: AtomicUsize::new(0),
      write: AtomicUsize::new(0),
      write_lock: Mutex::new(()),
    }
  }

  /// Push samples into the ring buffer, returning the number of samples accepted.
  pub fn push(&self, samples: &[f32]) -> usize {
    let _guard = self.write_lock.lock();
    self.push_locked(samples)
  }

  /// Returns the number of samples currently buffered.
  ///
  /// This is safe to call from the real-time consumer thread (no locks, no allocation).
  #[must_use]
  pub fn buffered_samples(&self) -> usize {
    let read = self.read.load(Ordering::Relaxed);
    let write = self.write.load(Ordering::Acquire);
    let available = write.wrapping_sub(read);
    if available > self.capacity {
      0
    } else {
      available
    }
  }
  fn push_locked(&self, samples: &[f32]) -> usize {
    let read = self.read.load(Ordering::Acquire);
    let write = self.write.load(Ordering::Relaxed);
    let used = write.wrapping_sub(read);
    if used >= self.capacity {
      return 0;
    }
    let free = self.capacity - used;
    let to_write = samples.len().min(free);
    if to_write == 0 {
      return 0;
    }

    let mut pos = write % self.capacity;
    let first = to_write.min(self.capacity - pos);
    for (i, sample) in samples.iter().take(first).enumerate() {
      // Safety: producer + consumer access disjoint indices as long as we don't overrun.
      unsafe {
        *self.buf[pos + i].get() = *sample;
      }
    }
    pos = 0;
    for (i, sample) in samples
      .iter()
      .skip(first)
      .take(to_write - first)
      .enumerate()
    {
      unsafe {
        *self.buf[pos + i].get() = *sample;
      }
    }

    self
      .write
      .store(write.wrapping_add(to_write), Ordering::Release);
    to_write
  }

  /// Pop as many samples as available (up to `dst.len()`) and add them into `dst`.
  pub fn pop_add_into(&self, dst: &mut [f32], gain: f32) {
    // `gain == 0.0` represents a muted sink. Muting must *not* behave like pausing:
    // we still advance the read cursor to keep device-time and queued audio aligned,
    // and to avoid unbounded buffering/backpressure when the caller keeps pushing
    // samples while muted.
    if dst.is_empty() {
      return;
    }
    // Defensively treat non-finite/denormal gains as silence so we never poison the mix.
    // (`gain` originates from user volume, but the atomic bit-pattern may still be corrupted.)
    //
    // Note: we still drain the ring buffer even when the effective gain is 0, so muting does not
    // behave like pausing.
    let gain = if gain.is_finite() && gain.is_normal() {
      gain
    } else {
      0.0
    };

    let read = self.read.load(Ordering::Relaxed);
    let write = self.write.load(Ordering::Acquire);
    let available = write.wrapping_sub(read);

    if available == 0 {
      return;
    }

    // If indices have wrapped in a way that breaks invariants, drop buffered audio.
    if available > self.capacity {
      self.read.store(write, Ordering::Release);
      return;
    }

    let to_read = dst.len().min(available);
    if to_read == 0 {
      return;
    }

    if gain != 0.0 {
      let mut pos = read % self.capacity;
      let first = to_read.min(self.capacity - pos);
      for i in 0..first {
        let sample = unsafe { *self.buf[pos + i].get() };
        // Avoid NaN poisoning and denormal slow paths:
        // - treat non-normal samples (NaN/Inf/0/subnormals) as silence
        // - flush any non-normal scaled output to silence too
        if sample.is_normal() {
          let scaled = sample * gain;
          if scaled.is_normal() {
            // If earlier mixing produced a subnormal/NaN/Inf at this slot, reset it to silence
            // before accumulating so it doesn't poison the rest of the mix and doesn't trigger
            // denormal slow paths.
            let cur = dst[i];
            if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
              dst[i] = 0.0;
            }
            dst[i] += scaled;
          }
        }
      }
      pos = 0;
      for i in 0..(to_read - first) {
        let sample = unsafe { *self.buf[pos + i].get() };
        if sample.is_normal() {
          let scaled = sample * gain;
          if scaled.is_normal() {
            let idx = first + i;
            let cur = dst[idx];
            if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
              dst[idx] = 0.0;
            }
            dst[idx] += scaled;
          }
        }
      }
    }

    self
      .read
      .store(read.wrapping_add(to_read), Ordering::Release);
  }

  /// Pop as many frames as available (up to `dst.len()`) and add them into `dst`, applying a
  /// per-frame gain ramp.
  ///
  /// `channels` must match the interleaving used by producers. Gain is applied uniformly across all
  /// channels of a frame, avoiding L/R mismatches.
  pub fn pop_add_into_ramped(&self, dst: &mut [f32], channels: usize, ramp: &mut GainRamp) {
    if dst.is_empty() || channels == 0 {
      return;
    }

    let read = self.read.load(Ordering::Relaxed);
    let write = self.write.load(Ordering::Acquire);
    let available = write.wrapping_sub(read);

    if available == 0 {
      return;
    }

    if available > self.capacity {
      self.read.store(write, Ordering::Release);
      return;
    }

    let mut to_read = dst.len().min(available);
    // Keep frame alignment so ramping applies equally across channels.
    to_read -= to_read % channels;
    if to_read == 0 {
      return;
    }

    let frames = to_read / channels;
    let mut pos = read % self.capacity;
    let mut dst_idx = 0usize;

    for _ in 0..frames {
      let gain = if ramp.current_gain.is_finite() && ramp.current_gain.is_normal() {
        ramp.current_gain
      } else {
        0.0
      };
      if gain == 0.0 {
        // Fast-path for silence: advance indices without touching memory.
        for _ in 0..channels {
          dst_idx += 1;
          pos += 1;
          if pos == self.capacity {
            pos = 0;
          }
        }
      } else {
        for _ in 0..channels {
          let sample = unsafe { *self.buf[pos].get() };
          // Avoid NaN poisoning and denormal slow paths:
          // - treat non-normal samples (NaN/Inf/0/subnormals) as silence
          // - flush any non-normal scaled output to silence too
          if sample.is_normal() {
            let scaled = sample * gain;
            if scaled.is_normal() {
              let cur = dst[dst_idx];
              if !cur.is_finite() || (cur != 0.0 && !cur.is_normal()) {
                dst[dst_idx] = 0.0;
              }
              dst[dst_idx] += scaled;
            }
          }
          dst_idx += 1;
          pos += 1;
          if pos == self.capacity {
            pos = 0;
          }
        }
      }

      ramp.advance_frame();
    }

    self
      .read
      .store(read.wrapping_add(to_read), Ordering::Release);
  }

  /// Like [`Self::pop_add_into_ramped`], but when the buffer underruns (runs out of samples before
  /// filling `dst`) it applies a short fade-out over the tail of the available audio so the output
  /// converges to silence before the implicit zeros.
  ///
  /// Returns `true` if an underrun occurred (i.e. fewer than `dst.len()` samples were available).
  pub(crate) fn pop_add_into_ramped_declick_underrun(
    &self,
    dst: &mut [f32],
    channels: usize,
    ramp: &mut GainRamp,
    fade_frames: u32,
  ) -> bool {
    if dst.is_empty() || channels == 0 {
      return false;
    }

    let requested = dst.len() - (dst.len() % channels);
    if requested == 0 {
      return false;
    }

    let read = self.read.load(Ordering::Relaxed);
    let write = self.write.load(Ordering::Acquire);
    let available = write.wrapping_sub(read);

    if available == 0 {
      return true;
    }

    // If indices have wrapped in a way that breaks invariants, drop buffered audio.
    if available > self.capacity {
      self.read.store(write, Ordering::Release);
      return true;
    }

    // Keep frame alignment: if producers ever push a non-multiple of `channels` (shouldn't happen),
    // drop the corrupted buffer so we don't get stuck with a permanent partial frame.
    if available % channels != 0 {
      self.read.store(write, Ordering::Release);
      return true;
    }

    let mut to_read = requested.min(available);
    to_read -= to_read % channels;
    if to_read == 0 {
      self.read.store(write, Ordering::Release);
      return true;
    }

    let did_underrun = to_read < requested;
    if !did_underrun {
      self.pop_add_into_ramped(&mut dst[..requested], channels, ramp);
      return false;
    }

    let frames_available = to_read / channels;
    let fade_frames = usize::try_from(fade_frames).unwrap_or(usize::MAX);
    // Always fade at least one frame so we avoid a hard step when we have any audio at all.
    let fade_frames = fade_frames.min(frames_available).max(1);
    let head_frames = frames_available.saturating_sub(fade_frames);

    let head_samples = head_frames.saturating_mul(channels);
    let fade_samples = fade_frames.saturating_mul(channels);

    if head_samples != 0 {
      self.pop_add_into_ramped(&mut dst[..head_samples], channels, ramp);
    }

    if fade_samples != 0 {
      ramp.target_gain = 0.0;
      let fade_frames_u32 = u32::try_from(fade_frames).unwrap_or(u32::MAX).max(1);
      if (ramp.current_gain - ramp.target_gain).abs() <= f32::EPSILON {
        ramp.current_gain = ramp.target_gain;
        ramp.step = 0.0;
        ramp.frames_remaining = 0;
      } else {
        ramp.frames_remaining = fade_frames_u32;
        ramp.step = (ramp.target_gain - ramp.current_gain) / ramp.frames_remaining as f32;
      }
      self.pop_add_into_ramped(
        &mut dst[head_samples..head_samples + fade_samples],
        channels,
        ramp,
      );
    }

    true
  }

  /// Returns `true` if the buffer currently contains any samples.
  ///
  /// This is intended for fast-path "maybe audible" checks. It intentionally does not attempt to
  /// repair broken invariants (that recovery is handled by the consumer during `pop_*` calls).
  #[inline]
  pub fn has_data(&self) -> bool {
    let read = self.read.load(Ordering::Relaxed);
    let write = self.write.load(Ordering::Acquire);
    write.wrapping_sub(read) != 0
  }

  /// Discard up to `max` samples from the buffer without reading/mixing them.
  ///
  /// This is a constant-time drain path used for muted sinks (or when the output is silent) where
  /// we still want to advance the sink's buffered audio to avoid unbounded growth.
  pub fn pop_discard(&self, max: usize) {
    if max == 0 {
      return;
    }

    let read = self.read.load(Ordering::Relaxed);
    let write = self.write.load(Ordering::Acquire);
    let available = write.wrapping_sub(read);

    if available == 0 {
      return;
    }

    // If indices have wrapped in a way that breaks invariants, drop buffered audio.
    if available > self.capacity {
      self.read.store(write, Ordering::Release);
      return;
    }

    let to_read = max.min(available);
    self
      .read
      .store(read.wrapping_add(to_read), Ordering::Release);
  }

  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.buffered_samples() == 0
  }
}

#[cfg(test)]
mod tests {
  use super::{AudioRingBuffer, GainRamp};

  #[test]
  fn ring_buffer_roundtrip() {
    let rb = AudioRingBuffer::new(8);
    assert_eq!(rb.push(&[1.0, 2.0, 3.0, 4.0]), 4);

    let mut out = vec![0.0; 4];
    rb.pop_add_into(&mut out, 1.0);
    assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);

    // Underflow should leave dst untouched.
    let mut out2 = vec![0.0; 4];
    rb.pop_add_into(&mut out2, 1.0);
    assert_eq!(out2, vec![0.0, 0.0, 0.0, 0.0]);
  }

  #[test]
  fn ring_buffer_gain_is_applied() {
    let rb = AudioRingBuffer::new(4);
    assert_eq!(rb.push(&[1.0, 1.0]), 2);

    let mut out = vec![0.0; 2];
    rb.pop_add_into(&mut out, 0.5);
    assert_eq!(out, vec![0.5, 0.5]);
  }
  #[test]
  fn ring_buffer_drops_nan_and_subnormal_samples() {
    let rb = AudioRingBuffer::new(8);
    let sub = f32::from_bits(1);
    assert!(!sub.is_normal());
    assert_eq!(rb.push(&[f32::NAN, sub, 1.0]), 3);

    let mut out = vec![0.0; 3];
    rb.pop_add_into(&mut out, 1.0);
    assert_eq!(out, vec![0.0, 0.0, 1.0]);
  }

  #[test]
  fn ring_buffer_ramped_mix_drops_nan_and_subnormal_samples() {
    let rb = AudioRingBuffer::new(8);
    let sub = f32::from_bits(1);
    assert!(!sub.is_normal());
    assert_eq!(rb.push(&[f32::NAN, sub, 1.0, 2.0]), 4);

    let mut ramp = GainRamp {
      current_gain: 1.0,
      target_gain: 1.0,
      step: 0.0,
      frames_remaining: 0,
    };

    let mut out = vec![0.0; 4];
    rb.pop_add_into_ramped(&mut out, 1, &mut ramp);
    assert_eq!(out, vec![0.0, 0.0, 1.0, 2.0]);
  }

  #[test]
  fn ring_buffer_ramped_drains_when_gain_is_zero() {
    // Use stereo so we exercise the channel alignment logic in `pop_add_into_ramped`.
    let channels = 2usize;
    // 200ms worth of 48kHz frames.
    let frames_total = 48_000 / 5;
    let frames_half = frames_total / 2;
    let samples_total = frames_total * channels;
    let samples_half = frames_half * channels;

    let rb = AudioRingBuffer::new(samples_total * 2);
    assert_eq!(rb.push(&vec![1.0; samples_total]), samples_total);

    // Muted playback for 100ms should still advance the read cursor (drain).
    let mut muted_out = vec![0.0; samples_half];
    let mut ramp = GainRamp {
      current_gain: 0.0,
      target_gain: 0.0,
      step: 0.0,
      frames_remaining: 0,
    };
    rb.pop_add_into_ramped(&mut muted_out, channels, &mut ramp);
    assert_eq!(muted_out, vec![0.0; samples_half]);

    // Unmuting should play immediately without backlog: only the remaining 100ms should be present.
    let mut out = vec![0.0; samples_total];
    let mut ramp = GainRamp {
      current_gain: 1.0,
      target_gain: 1.0,
      step: 0.0,
      frames_remaining: 0,
    };
    rb.pop_add_into_ramped(&mut out, channels, &mut ramp);
    assert_eq!(&out[..samples_half], &vec![1.0; samples_half][..]);
    assert_eq!(&out[samples_half..], &vec![0.0; samples_half][..]);
  }

  #[test]
  fn ring_buffer_ramped_declick_fades_tail_on_underrun() {
    let rb = AudioRingBuffer::new(64);
    // 10 mono frames at amplitude 1.0.
    assert_eq!(rb.push(&vec![1.0; 10]), 10);

    let mut ramp = GainRamp {
      current_gain: 1.0,
      target_gain: 1.0,
      step: 0.0,
      frames_remaining: 0,
    };

    // Request more than available -> underrun. Fade the last 4 frames.
    let mut out = vec![0.0; 20];
    let did_underrun = rb.pop_add_into_ramped_declick_underrun(&mut out, 1, &mut ramp, 4);
    assert!(did_underrun);

    // First 6 frames untouched.
    assert!(out[..6].iter().all(|v| (*v - 1.0).abs() < 1e-6));
    // Last 4 available frames are faded: 1.0, 0.75, 0.5, 0.25.
    let faded = &out[6..10];
    let expected = [1.0f32, 0.75, 0.5, 0.25];
    for (got, exp) in faded.iter().zip(expected) {
      assert!((*got - exp).abs() < 1e-6, "expected {exp}, got {got}");
    }
    // Remaining requested frames are silence.
    assert!(out[10..].iter().all(|v| *v == 0.0));

    // Buffer should be drained and ramp should end at 0 gain.
    assert!(rb.is_empty());
    assert!((ramp.current_gain - 0.0).abs() < 1e-6);
    assert!((ramp.target_gain - 0.0).abs() < 1e-6);
    assert_eq!(ramp.frames_remaining, 0);
  }

  #[test]
  fn ring_buffer_drains_when_gain_is_zero() {
    // 200ms worth of mono 48kHz samples.
    let total = 48_000 / 5;
    let half = total / 2;

    let rb = AudioRingBuffer::new(total * 2);
    assert_eq!(rb.push(&vec![1.0; total]), total);

    // Muted playback for 100ms should still consume samples.
    let mut muted_out = vec![0.0; half];
    rb.pop_add_into(&mut muted_out, 0.0);
    assert_eq!(muted_out, vec![0.0; half]);

    // Unmuting should play immediately (no backlog): only the remaining 100ms should be present.
    let mut out = vec![0.0; total];
    rb.pop_add_into(&mut out, 1.0);
    assert_eq!(&out[..half], &vec![1.0; half][..]);
    assert_eq!(&out[half..], &vec![0.0; half][..]);
  }

  #[test]
  fn audio_gain_ramp_monotonic() {
    let channels = 1usize;
    let ramp_frames = 16u32;
    let frames_to_mix = ramp_frames as usize + 1;

    let rb = AudioRingBuffer::new(frames_to_mix * channels);
    assert_eq!(rb.push(&vec![1.0; frames_to_mix * channels]), frames_to_mix);

    let mut ramp = GainRamp {
      current_gain: 1.0,
      target_gain: 0.0,
      step: (0.0 - 1.0) / ramp_frames as f32,
      frames_remaining: ramp_frames,
    };

    let mut out = vec![0.0; frames_to_mix];
    rb.pop_add_into_ramped(&mut out, channels, &mut ramp);

    assert!(out[0] > out[1], "ramp should begin decreasing immediately");
    for win in out.windows(2) {
      assert!(
        win[1] <= win[0] + f32::EPSILON,
        "ramp output should be monotonically non-increasing ({} -> {})",
        win[0],
        win[1]
      );
    }
    assert_eq!(out[frames_to_mix - 1], 0.0);
  }

  #[test]
  fn audio_gain_ramp_length_is_respected() {
    let channels = 1usize;
    let ramp_frames = 8u32;

    let rb = AudioRingBuffer::new((ramp_frames as usize + 2) * channels);
    assert_eq!(
      rb.push(&vec![1.0; (ramp_frames as usize + 1) * channels]),
      ramp_frames as usize + 1
    );

    let mut ramp = GainRamp {
      current_gain: 1.0,
      target_gain: 0.0,
      step: (0.0 - 1.0) / ramp_frames as f32,
      frames_remaining: ramp_frames,
    };

    // Process exactly `ramp_frames` frames; the ramp should reach the target afterwards, but the
    // last output sample still uses the pre-advance gain.
    let mut out = vec![0.0; ramp_frames as usize];
    rb.pop_add_into_ramped(&mut out, channels, &mut ramp);
    assert_eq!(ramp.frames_remaining, 0);
    assert_eq!(ramp.current_gain, 0.0);
    assert!(
      out.last().copied().unwrap_or(0.0) > 0.0,
      "last ramped sample should still be >0 before the post-ramp frame"
    );

    // The next frame should be fully muted.
    let mut out2 = vec![0.0; 1];
    rb.pop_add_into_ramped(&mut out2, channels, &mut ramp);
    assert_eq!(out2[0], 0.0);
  }
}
