use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::convert::sanitize_mix_sample;

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
    for (i, sample) in samples.iter().skip(first).take(to_write - first).enumerate() {
      unsafe {
        *self.buf[pos + i].get() = *sample;
      }
    }

    self.write.store(write.wrapping_add(to_write), Ordering::Release);
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
    let gain = if gain.is_finite() && gain.is_normal() { gain } else { 0.0 };

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
        // Sanitize per-sample before accumulation so malformed data cannot poison the entire mix
        // and so we never feed denormals back into subsequent mixing math.
        dst[i] += sanitize_mix_sample(sample * gain);
      }
      pos = 0;
      for i in 0..(to_read - first) {
        let sample = unsafe { *self.buf[pos + i].get() };
        dst[first + i] += sanitize_mix_sample(sample * gain);
      }
    }

    self.read.store(read.wrapping_add(to_read), Ordering::Release);
  }
}

#[cfg(test)]
mod tests {
  use super::AudioRingBuffer;

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
}
