//! Lock-free-ish bounded audio queue for interleaved PCM `f32` samples.
//!
//! This module provides a small, reusable building block for audio backends:
//! - **Producer**: a decode/mixer thread that pushes decoded interleaved samples.
//! - **Consumer**: the real-time audio callback that pops into a fixed-size output buffer.
//!
//! ## Real-time constraints
//!
//! The [`PcmF32QueueConsumer::pop_into`] path:
//! - does **not** allocate,
//! - does not take locks,
//! - performs at most two contiguous copies (wrap-around aware).
//!
//! ## Bounded buffering policy
//!
//! The queue enforces a hard maximum capacity (configured at construction time).
//! When the producer attempts to push more frames than there is free capacity, the queue **drops
//! newest samples** (i.e. it truncates the input to the available space).
//!
//! Rationale: this keeps the implementation SPSC-safe without ever overwriting unread samples.
//! Overwriting would require the producer to advance the read pointer (or use per-slot sequence
//! numbers), which complicates correctness in safe Rust.
//!
//! Callers that want "drop-oldest" behavior should explicitly drain/clear the queue before pushing
//! newer audio (e.g. on seek) or implement a higher-level policy.

use parking_lot::Mutex;
use std::cell::UnsafeCell;
use std::cmp;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::convert::sanitize_buffer_in_place;
use super::limits::{MAX_BUFFERED_DURATION, MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};
use super::AudioError;

const NO_PTS_NS: u64 = u64::MAX;

/// Creates a bounded SPSC queue for interleaved PCM `f32` audio.
///
/// The returned handles are:
/// - [`PcmF32QueueProducer`] for pushing samples from a decode/mixer thread.
/// - [`PcmF32QueueConsumer`] for popping samples in a real-time audio callback.
pub fn pcm_f32_queue(
  channels: usize,
  sample_rate_hz: u32,
  max_buffered_frames: usize,
) -> Result<(PcmF32QueueProducer, PcmF32QueueConsumer), AudioError> {
  let inner = Arc::new(PcmF32QueueInner::new(
    channels,
    sample_rate_hz,
    max_buffered_frames,
  )?);
  Ok((
    PcmF32QueueProducer {
      inner: Arc::clone(&inner),
    },
    PcmF32QueueConsumer { inner },
  ))
}

/// A bounded PCM `f32` queue suitable for:
/// - multi-threaded producers (serialized via a small mutex), and
/// - a single real-time consumer (no locks/allocations on pop).
///
/// This is essentially the same underlying queue as [`pcm_f32_queue`], but exposes a single shared
/// handle (`&self` methods) which is often more ergonomic to embed inside backend sink structs.
pub struct PcmF32Queue {
  inner: Arc<PcmF32QueueInner>,
  push_lock: Mutex<()>,
}

impl PcmF32Queue {
  /// Create a new bounded queue with capacity measured in **frames**.
  #[must_use]
  pub fn new(
    channels: usize,
    sample_rate_hz: u32,
    max_buffered_frames: usize,
  ) -> Result<Self, AudioError> {
    Ok(Self {
      inner: Arc::new(PcmF32QueueInner::new(
        channels,
        sample_rate_hz,
        max_buffered_frames,
      )?),
      push_lock: Mutex::new(()),
    })
  }

  /// Push interleaved PCM samples.
  ///
  /// Returns the number of **samples** accepted (the remainder was dropped).
  pub fn push(&self, samples: &[f32], start_pts: Duration) -> usize {
    let _guard = self.push_lock.lock();
    self.inner.push_inner(samples, Some(start_pts))
  }

  /// Push samples without updating the PTS base.
  ///
  /// Returns the number of **samples** accepted.
  pub fn push_without_pts(&self, samples: &[f32]) -> usize {
    let _guard = self.push_lock.lock();
    self.inner.push_inner(samples, None)
  }

  /// Pop samples into `out`.
  ///
  /// Returns the number of **samples** written.
  pub fn pop_into(&self, out: &mut [f32]) -> usize {
    self.inner.pop_into_inner(out)
  }

  /// Pop samples and add them into `dst` after applying `gain`.
  ///
  /// Returns the number of **samples** consumed.
  pub fn pop_add_into(&self, dst: &mut [f32], gain: f32) -> usize {
    self.inner.pop_add_into_inner(dst, gain)
  }

  /// Returns the number of frames currently buffered.
  pub fn buffered_frames(&self) -> usize {
    self.inner.buffered_frames()
  }

  /// Returns the duration currently buffered, based on the configured sample rate.
  pub fn buffered_duration(&self) -> Duration {
    self.inner.buffered_duration()
  }

  /// Returns the PTS of the next frame to be popped, if available.
  pub fn head_pts(&self) -> Option<Duration> {
    self.inner.head_pts()
  }

  /// Maximum number of frames this queue can buffer.
  pub fn capacity_frames(&self) -> usize {
    self.inner.capacity_frames()
  }

  /// Channel count (interleaving factor).
  pub fn channels(&self) -> usize {
    self.inner.channels
  }

  /// Sample rate in Hz.
  pub fn sample_rate_hz(&self) -> u32 {
    self.inner.sample_rate_hz
  }
}

/// Producer-side handle for a bounded PCM `f32` queue.
pub struct PcmF32QueueProducer {
  inner: Arc<PcmF32QueueInner>,
}

impl PcmF32QueueProducer {
  /// Push interleaved PCM `f32` samples into the queue.
  ///
  /// `start_pts` is the presentation timestamp corresponding to the first frame in `samples`.
  /// It is only used when the queue was empty at the time of the push (to establish a new PTS
  /// base). If the queue is not empty, the PTS base is left unchanged.
  ///
  /// If the queue does not have enough free space, the input is truncated and the remainder is
  /// dropped (see module-level docs for the drop policy).
  ///
  /// Returns the number of **samples** accepted.
  pub fn push(&mut self, samples: &[f32], start_pts: Duration) -> usize {
    self.inner.push_inner(samples, Some(start_pts))
  }

  /// Push samples without updating the queue's PTS base.
  ///
  /// Returns the number of **samples** accepted.
  pub fn push_without_pts(&mut self, samples: &[f32]) -> usize {
    self.inner.push_inner(samples, None)
  }

  /// Returns the number of frames currently buffered.
  pub fn buffered_frames(&self) -> usize {
    self.inner.buffered_frames()
  }

  /// Returns the duration currently buffered, based on the configured sample rate.
  pub fn buffered_duration(&self) -> Duration {
    self.inner.buffered_duration()
  }

  /// Maximum number of frames this queue can buffer.
  pub fn capacity_frames(&self) -> usize {
    self.inner.capacity_frames()
  }

  /// Channel count (interleaving factor).
  pub fn channels(&self) -> usize {
    self.inner.channels
  }

  /// Sample rate in Hz.
  pub fn sample_rate_hz(&self) -> u32 {
    self.inner.sample_rate_hz
  }
}

/// Consumer-side handle for a bounded PCM `f32` queue.
pub struct PcmF32QueueConsumer {
  inner: Arc<PcmF32QueueInner>,
}

impl PcmF32QueueConsumer {
  /// Pop interleaved PCM `f32` samples into `out`.
  ///
  /// Returns the number of **samples** written into `out`.
  ///
  /// The queue only pops complete frames (where a "frame" is `channels` interleaved samples). If
  /// `out.len()` is not a multiple of `channels`, the trailing partial frame is left untouched.
  pub fn pop_into(&mut self, out: &mut [f32]) -> usize {
    self.inner.pop_into_inner(out)
  }

  /// Pop samples and add them into `dst` after applying `gain`.
  ///
  /// Returns the number of **samples** consumed.
  pub fn pop_add_into(&mut self, dst: &mut [f32], gain: f32) -> usize {
    self.inner.pop_add_into_inner(dst, gain)
  }

  /// Returns the number of frames currently buffered.
  pub fn buffered_frames(&self) -> usize {
    self.inner.buffered_frames()
  }

  /// Returns the duration currently buffered, based on the configured sample rate.
  pub fn buffered_duration(&self) -> Duration {
    self.inner.buffered_duration()
  }

  /// Returns the PTS of the next frame to be popped, if a PTS base has been established.
  pub fn head_pts(&self) -> Option<Duration> {
    self.inner.head_pts()
  }

  /// Channel count (interleaving factor).
  pub fn channels(&self) -> usize {
    self.inner.channels
  }

  /// Sample rate in Hz.
  pub fn sample_rate_hz(&self) -> u32 {
    self.inner.sample_rate_hz
  }
}

struct PcmF32QueueInner {
  channels: usize,
  sample_rate_hz: u32,
  capacity_frames: u64,
  /// Interleaved samples, length = `capacity_frames * channels`.
  buf: Box<[UnsafeCell<f32>]>,
  read_frame: AtomicU64,
  write_frame: AtomicU64,
  pts_base_frame: AtomicU64,
  pts_base_ns: AtomicU64,
}

impl PcmF32QueueInner {
  fn new(channels: usize, sample_rate_hz: u32, max_buffered_frames: usize) -> Result<Self, AudioError> {
    if channels == 0 {
      return Err(AudioError::invalid_spec("channels must be non-zero"));
    }
    let max_channels = usize::from(MAX_CHANNELS);
    if channels > max_channels {
      return Err(AudioError::invalid_spec(format!(
        "channels {} exceeds MAX_CHANNELS {}",
        channels, max_channels
      )));
    }

    if sample_rate_hz == 0 {
      return Err(AudioError::invalid_spec("sample_rate_hz must be non-zero"));
    }
    if sample_rate_hz > MAX_SAMPLE_RATE_HZ {
      return Err(AudioError::invalid_spec(format!(
        "sample_rate_hz {} exceeds MAX_SAMPLE_RATE_HZ {}",
        sample_rate_hz, MAX_SAMPLE_RATE_HZ
      )));
    }

    if max_buffered_frames == 0 {
      return Err(AudioError::invalid_spec("max_buffered_frames must be non-zero"));
    }

    let max_frames_cap_u64 = super::duration_to_frames_floor(sample_rate_hz, MAX_BUFFERED_DURATION);
    let max_frames_cap = usize::try_from(max_frames_cap_u64).unwrap_or(usize::MAX);
    if max_buffered_frames > max_frames_cap {
      return Err(AudioError::invalid_spec(format!(
        "max_buffered_frames {} exceeds MAX_BUFFERED_DURATION cap {} frames",
        max_buffered_frames, max_frames_cap
      )));
    }

    let len_samples = max_buffered_frames
      .checked_mul(channels)
      .ok_or_else(|| AudioError::invalid_spec("buffer length overflow"))?;

    let mut buf = Vec::with_capacity(len_samples);
    buf.resize_with(len_samples, || UnsafeCell::new(0.0));

    Ok(Self {
      channels,
      sample_rate_hz,
      capacity_frames: max_buffered_frames as u64,
      buf: buf.into_boxed_slice(),
      read_frame: AtomicU64::new(0),
      write_frame: AtomicU64::new(0),
      pts_base_frame: AtomicU64::new(0),
      pts_base_ns: AtomicU64::new(NO_PTS_NS),
    })
  }

  fn capacity_frames(&self) -> usize {
    // `capacity_frames` is constructed from a `usize` and never mutated.
    self.capacity_frames as usize
  }

  fn buffered_frames(&self) -> usize {
    let read = self.read_frame.load(Ordering::Acquire);
    let write = self.write_frame.load(Ordering::Acquire);
    // Invariant: write >= read; and (write - read) <= capacity.
    (write - read) as usize
  }

  fn buffered_duration(&self) -> Duration {
    let frames = self.buffered_frames() as u128;
    let nanos = frames
      .saturating_mul(1_000_000_000u128)
      .checked_div(self.sample_rate_hz as u128)
      .unwrap_or(0);
    Duration::from_nanos(cmp::min(nanos, u128::from(u64::MAX)) as u64)
  }

  fn head_pts(&self) -> Option<Duration> {
    let read_frame = self.read_frame.load(Ordering::Acquire);
    let write_frame = self.write_frame.load(Ordering::Acquire);
    if write_frame == read_frame {
      return None;
    }

    let base_ns = self.pts_base_ns.load(Ordering::Acquire);
    if base_ns == NO_PTS_NS {
      return None;
    }

    let base_frame = self.pts_base_frame.load(Ordering::Acquire);
    let delta_frames = read_frame.saturating_sub(base_frame);

    let delta_ns = (delta_frames as u128)
      .saturating_mul(1_000_000_000u128)
      / (self.sample_rate_hz as u128);
    let ns = (base_ns as u128).saturating_add(delta_ns);
    Some(Duration::from_nanos(cmp::min(ns, u128::from(u64::MAX)) as u64))
  }

  fn buf_ptr(&self) -> *mut f32 {
    // `UnsafeCell<T>` is `repr(transparent)`, so a contiguous `[UnsafeCell<f32>]` has the same
    // layout as `[f32]`. We only ever access disjoint regions from the producer/consumer.
    self.buf.as_ptr().cast::<f32>() as *mut f32
  }

  fn push_inner(&self, samples: &[f32], start_pts: Option<Duration>) -> usize {
    let channels = self.channels;
    let input_frames = (samples.len() / channels) as u64;
    if input_frames == 0 {
      return 0;
    }

    let read = self.read_frame.load(Ordering::Acquire);
    let write = self.write_frame.load(Ordering::Relaxed);

    if read == write {
      // Establish a new PTS base when the queue is empty.
      self.pts_base_frame.store(write, Ordering::Relaxed);
      let ns = start_pts.map_or(NO_PTS_NS, duration_to_ns_saturating);
      self.pts_base_ns.store(ns, Ordering::Relaxed);
    }

    let buffered = write.saturating_sub(read);
    let capacity = self.capacity_frames;
    let free = capacity.saturating_sub(buffered);
    let frames_to_write = cmp::min(
      cmp::min(input_frames, free),
      MAX_FRAMES_PER_PUSH as u64,
    );
    if frames_to_write == 0 {
      return 0;
    }

    let samples_to_write = (frames_to_write as usize) * channels;

    let write_idx_frames = (write % capacity) as usize;
    let first_part_frames = cmp::min(frames_to_write as usize, self.capacity_frames() - write_idx_frames);
    let first_part_samples = first_part_frames * channels;
    let second_part_samples = samples_to_write - first_part_samples;

    unsafe {
      let buf_ptr = self.buf_ptr();

      // First contiguous region.
      std::ptr::copy_nonoverlapping(
        samples.as_ptr(),
        buf_ptr.add(write_idx_frames * channels),
        first_part_samples,
      );

      // Wrap-around region (if any).
      if second_part_samples > 0 {
        std::ptr::copy_nonoverlapping(
          samples.as_ptr().add(first_part_samples),
          buf_ptr,
          second_part_samples,
        );
      }
    }

    // Publish the write after samples are written.
    self
      .write_frame
      .store(write + frames_to_write, Ordering::Release);

    samples_to_write
  }

  fn pop_into_inner(&self, out: &mut [f32]) -> usize {
    let channels = self.channels;
    let requested_frames = (out.len() / channels) as u64;
    if requested_frames == 0 {
      return 0;
    }

    let read = self.read_frame.load(Ordering::Relaxed);
    let write = self.write_frame.load(Ordering::Acquire);
    let available_frames = write.saturating_sub(read);
    let frames_to_read = cmp::min(requested_frames, available_frames);
    if frames_to_read == 0 {
      return 0;
    }

    let samples_to_read = (frames_to_read as usize) * channels;

    let capacity = self.capacity_frames;
    let read_idx_frames = (read % capacity) as usize;
    let first_part_frames = cmp::min(frames_to_read as usize, self.capacity_frames() - read_idx_frames);
    let first_part_samples = first_part_frames * channels;
    let second_part_samples = samples_to_read - first_part_samples;

    unsafe {
      let buf_ptr = self.buf_ptr();

      // First contiguous region.
      std::ptr::copy_nonoverlapping(
        buf_ptr.add(read_idx_frames * channels),
        out.as_mut_ptr(),
        first_part_samples,
      );

      // Wrap-around region.
      if second_part_samples > 0 {
        std::ptr::copy_nonoverlapping(
          buf_ptr,
          out.as_mut_ptr().add(first_part_samples),
          second_part_samples,
        );
      }
    }

    // Sanitize the popped samples before handing them to the audio callback / mixer so malformed
    // decoder output (NaN/Inf/denormals) cannot propagate into the output device buffer.
    sanitize_buffer_in_place(&mut out[..samples_to_read]);

    self
      .read_frame
      .store(read + frames_to_read, Ordering::Release);

    samples_to_read
  }

  fn pop_add_into_inner(&self, dst: &mut [f32], gain: f32) -> usize {
    let channels = self.channels;
    let requested_frames = (dst.len() / channels) as u64;
    if requested_frames == 0 {
      return 0;
    }

    let read = self.read_frame.load(Ordering::Relaxed);
    let write = self.write_frame.load(Ordering::Acquire);
    let available_frames = write.saturating_sub(read);
    let frames_to_read = cmp::min(requested_frames, available_frames);
    if frames_to_read == 0 {
      return 0;
    }

    let samples_to_read = (frames_to_read as usize) * channels;

    // If gain is 0, we can discard without touching `dst`.
    if gain == 0.0 {
      self
        .read_frame
        .store(read + frames_to_read, Ordering::Release);
      return samples_to_read;
    }

    let capacity = self.capacity_frames;
    let read_idx_frames = (read % capacity) as usize;
    let first_part_frames =
      cmp::min(frames_to_read as usize, self.capacity_frames() - read_idx_frames);
    let first_part_samples = first_part_frames * channels;
    let second_part_samples = samples_to_read - first_part_samples;

    unsafe {
      let buf_ptr = self.buf_ptr();
      let dst_ptr = dst.as_mut_ptr();

      // First contiguous region.
      let src_ptr = buf_ptr.add(read_idx_frames * channels);
      for i in 0..first_part_samples {
        let sample = *src_ptr.add(i);
        *dst_ptr.add(i) += sample * gain;
      }

      // Wrap-around region.
      if second_part_samples > 0 {
        let src_ptr = buf_ptr;
        for i in 0..second_part_samples {
          let sample = *src_ptr.add(i);
          *dst_ptr.add(first_part_samples + i) += sample * gain;
        }
      }
    }

    self
      .read_frame
      .store(read + frames_to_read, Ordering::Release);

    samples_to_read
  }
}

unsafe impl Send for PcmF32QueueInner {}
unsafe impl Sync for PcmF32QueueInner {}

fn duration_to_ns_saturating(d: Duration) -> u64 {
  // Duration::as_nanos is u128; saturate so we can keep `u64::MAX` as a sentinel.
  let ns = d.as_nanos();
  if ns >= u128::from(NO_PTS_NS) {
    NO_PTS_NS - 1
  } else {
    ns as u64
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
  use std::thread;

  #[test]
  fn wraparound_preserves_order() {
    let (mut prod, mut cons) = pcm_f32_queue(1, 48_000, 8).expect("queue");

    let a: Vec<f32> = (0..6).map(|v| v as f32).collect();
    assert_eq!(prod.push(&a, Duration::from_secs(0)), 6);

    let mut tmp = [0.0f32; 5];
    let n = cons.pop_into(&mut tmp);
    assert_eq!(n, 5);
    assert_eq!(&tmp, &[0.0, 1.0, 2.0, 3.0, 4.0]);

    let b: Vec<f32> = (6..10).map(|v| v as f32).collect();
    assert_eq!(prod.push(&b, Duration::from_secs(0)), 4);

    let mut out = [0.0f32; 5];
    let n = cons.pop_into(&mut out);
    assert_eq!(n, 5);
    assert_eq!(&out, &[5.0, 6.0, 7.0, 8.0, 9.0]);
    assert_eq!(cons.buffered_frames(), 0);
  }

  #[test]
  fn capacity_drops_newest() {
    let (mut prod, mut cons) = pcm_f32_queue(1, 48_000, 4).expect("queue");

    let input: Vec<f32> = (0..6).map(|v| v as f32).collect();
    assert_eq!(prod.push(&input, Duration::from_secs(0)), 4);

    assert_eq!(cons.buffered_frames(), 4);

    let mut out = [0.0f32; 10];
    let n = cons.pop_into(&mut out);
    assert_eq!(n, 4);
    assert_eq!(&out[..4], &[0.0, 1.0, 2.0, 3.0]);
    assert_eq!(cons.buffered_frames(), 0);
  }

  #[test]
  fn pop_into_sanitizes_nan_and_subnormals() {
    let (mut prod, mut cons) = pcm_f32_queue(1, 48_000, 8);
    let sub = f32::from_bits(1);
    assert!(!sub.is_normal());

    prod.push(&[f32::NAN, sub, 1.0], Duration::ZERO);

    let mut out = [123.0f32; 3];
    let n = cons.pop_into(&mut out);
    assert_eq!(n, 3);
    assert_eq!(out, [0.0, 0.0, 1.0]);
  }

  #[test]
  fn concurrent_spsc_roundtrip() {
    let (mut prod, mut cons) = pcm_f32_queue(1, 48_000, 256).expect("queue");
    let done = Arc::new(AtomicBool::new(false));
    let done2 = Arc::clone(&done);

    let producer = thread::spawn(move || {
      let total = 10_000usize;
      let chunk = 37usize;
      let mut i = 0usize;
      while i < total {
        let n = cmp::min(chunk, total - i);
        // Wait until there is enough room so we never drop.
        while prod.buffered_frames() + n > prod.capacity_frames() {
          thread::yield_now();
        }
        let mut samples = Vec::with_capacity(n);
        for v in i..(i + n) {
          samples.push(v as f32);
        }
        assert_eq!(prod.push(&samples, Duration::from_secs(0)), n);
        i += n;
      }
      done.store(true, AtomicOrdering::Release);
    });

    let consumer = thread::spawn(move || {
      let total = 10_000usize;
      let mut expected = 0usize;
      let mut out = vec![0.0f32; 64];
      while expected < total {
        let n = cons.pop_into(&mut out);
        if n == 0 {
          if done2.load(AtomicOrdering::Acquire) && cons.buffered_frames() == 0 {
            break;
          }
          thread::yield_now();
          continue;
        }
        for &s in &out[..n] {
          assert_eq!(s as usize, expected);
          expected += 1;
        }
      }
      assert_eq!(expected, total);
    });

    producer.join().unwrap();
    consumer.join().unwrap();
  }

  #[test]
  fn pts_is_reported_for_head_when_set_on_empty_push() {
    let (mut prod, cons) = pcm_f32_queue(2, 48_000, 16).expect("queue");
    assert_eq!(cons.head_pts(), None);

    assert_eq!(
      prod.push(&[0.0, 0.0, 1.0, 1.0], Duration::from_millis(123)),
      4
    );
    assert_eq!(cons.head_pts(), Some(Duration::from_millis(123)));
  }

  #[test]
  fn queue_enforces_frame_alignment() {
    let (mut prod, mut cons) = pcm_f32_queue(2, 48_000, 8).expect("queue");
    // 3 samples = 1 full frame (2 samples) + 1 trailing sample dropped.
    assert_eq!(prod.push(&[1.0, 2.0, 3.0], Duration::from_secs(0)), 2);
    let mut out = [0.0f32; 4];
    assert_eq!(cons.pop_into(&mut out), 2);
    assert_eq!(&out[..2], &[1.0, 2.0]);
  }

  #[test]
  fn shared_queue_pop_add_into_applies_gain_and_consumes() {
    let q = PcmF32Queue::new(1, 48_000, 8).unwrap();
    assert_eq!(q.push_without_pts(&[1.0, 1.0, 1.0, 1.0]), 4);
    let mut out = [0.0f32; 4];
    assert_eq!(q.pop_add_into(&mut out, 0.5), 4);
    assert_eq!(out, [0.5, 0.5, 0.5, 0.5]);
    assert_eq!(q.buffered_frames(), 0);
  }

  #[test]
  fn shared_queue_drains_when_gain_is_zero() {
    let q = PcmF32Queue::new(1, 48_000, 8).unwrap();
    assert_eq!(q.push_without_pts(&[1.0; 8]), 8);

    // Muted consumption still advances the read cursor (drains) but does not modify the output.
    let mut muted_out = [123.0f32; 4];
    assert_eq!(q.pop_add_into(&mut muted_out, 0.0), 4);
    assert_eq!(muted_out, [123.0; 4]);
    assert_eq!(q.buffered_frames(), 4);

    // Unmuting should play immediately without backlog: only the remaining frames should mix.
    let mut out = [0.0f32; 8];
    assert_eq!(q.pop_add_into(&mut out, 1.0), 4);
    assert_eq!(out, [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(q.buffered_frames(), 0);
  }

  #[test]
  fn rejects_invalid_queue_specs() {
    assert!(matches!(
      pcm_f32_queue(0, 48_000, 8),
      Err(AudioError::InvalidSpec { .. })
    ));

    let too_many_channels = usize::from(MAX_CHANNELS) + 1;
    assert!(matches!(
      pcm_f32_queue(too_many_channels, 48_000, 8),
      Err(AudioError::InvalidSpec { .. })
    ));

    assert!(matches!(
      pcm_f32_queue(1, 0, 8),
      Err(AudioError::InvalidSpec { .. })
    ));

    assert!(matches!(
      pcm_f32_queue(1, MAX_SAMPLE_RATE_HZ + 1, 8),
      Err(AudioError::InvalidSpec { .. })
    ));

    assert!(matches!(
      pcm_f32_queue(1, 48_000, 0),
      Err(AudioError::InvalidSpec { .. })
    ));
  }

  #[test]
  fn rejects_capacity_above_buffered_duration_cap() {
    let cap_frames_u64 = crate::media::audio::duration_to_frames_floor(48_000, MAX_BUFFERED_DURATION);
    let cap_frames = usize::try_from(cap_frames_u64).unwrap();
    let err = pcm_f32_queue(1, 48_000, cap_frames + 1).unwrap_err();
    assert!(matches!(err, AudioError::InvalidSpec { .. }));
  }

  #[test]
  fn rejects_buffers_larger_than_max_frames_per_push() {
    let cap_frames_u64 = crate::media::audio::duration_to_frames_floor(48_000, MAX_BUFFERED_DURATION);
    let cap_frames = usize::try_from(cap_frames_u64).unwrap();
    let (mut prod, _cons) = pcm_f32_queue(1, 48_000, cap_frames).unwrap();

    let samples = vec![0.0f32; MAX_FRAMES_PER_PUSH + 1];
    let err = prod.push_pcm_f32(&samples, None).unwrap_err();
    assert!(matches!(err, AudioError::InvalidBuffer { .. }));
  }
}
