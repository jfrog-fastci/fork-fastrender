//! Audio drift correction / adaptive buffering.
//!
//! Audio playback pipelines commonly have a small long-term mismatch between:
//! - the *stream clock* (how quickly decoded PCM is produced, based on container PTS cadence), and
//! - the *device clock* (how quickly the audio device consumes samples).
//!
//! Even ppm-level mismatches accumulate over time as latency creep (buffer grows) or underruns
//! (buffer shrinks). This module implements a lightweight per-stream controller that keeps the
//! buffered duration near a configured target by returning a small playback-rate adjustment.
//!
//! The intended integration point is the resampler: `effective_ratio = base_ratio *
//! controller.playback_rate()`. The controller is designed for the real-time audio callback path:
//! it is allocation-free and uses bounded, slew-rate limited adjustments.

use super::queue::PcmF32QueueConsumer;

#[derive(Debug, Clone, Copy)]
pub struct DriftControllerConfig {
  /// Desired buffered duration, in seconds (media time).
  pub target_buffer_s: f64,
  /// If the buffer drops below `target - low_watermark`, the controller will start slowing down.
  pub low_watermark_s: f64,
  /// If the buffer rises above `target + high_watermark`, the controller will start speeding up.
  pub high_watermark_s: f64,
  /// Maximum absolute deviation from 1.0 for [`DriftController::playback_rate`].
  ///
  /// For example, `0.01` caps adjustments at ±1%.
  pub max_playback_rate_adjust: f64,
  /// Maximum absolute change in playback rate per second.
  ///
  /// This is a "slew rate" limiter to avoid audible discontinuities.
  pub max_slew_per_s: f64,
  /// Proportional gain, expressed as rate-adjust-per-second-of-error.
  ///
  /// `playback_rate ~= 1.0 + kp * (buffered_s - target_s)` (clamped and slew-limited).
  pub kp: f64,
  /// Integral gain, expressed as rate-adjust-per-(second^2) of accumulated error.
  ///
  /// This term is *gated* by the watermarks to avoid constantly integrating minor jitter.
  pub ki: f64,
}

impl DriftControllerConfig {
  pub fn validate(&self) {
    assert!(self.target_buffer_s.is_finite() && self.target_buffer_s >= 0.0);
    assert!(self.low_watermark_s.is_finite() && self.low_watermark_s >= 0.0);
    assert!(self.high_watermark_s.is_finite() && self.high_watermark_s >= 0.0);
    assert!(self.max_playback_rate_adjust.is_finite() && self.max_playback_rate_adjust >= 0.0);
    assert!(self.max_slew_per_s.is_finite() && self.max_slew_per_s >= 0.0);
    assert!(self.kp.is_finite() && self.kp >= 0.0);
    assert!(self.ki.is_finite() && self.ki >= 0.0);
  }
}

#[derive(Debug, Clone)]
pub struct DriftController {
  cfg: DriftControllerConfig,
  playback_rate: f64,
  integral_error_s: f64,
}

impl DriftController {
  pub fn new(cfg: DriftControllerConfig) -> Self {
    cfg.validate();
    Self {
      cfg,
      playback_rate: 1.0,
      integral_error_s: 0.0,
    }
  }

  /// Current playback rate multiplier in `[1-max, 1+max]`.
  #[inline]
  pub fn playback_rate(&self) -> f64 {
    self.playback_rate
  }

  /// Compute the effective resampling ratio to feed to a resampler.
  #[inline]
  pub fn effective_ratio(&self, base_ratio: f64) -> f64 {
    base_ratio * self.playback_rate
  }

  /// Update the controller from the latest buffer observation.
  ///
  /// - `buffered_s` is the current buffered duration in seconds (typically `queued_frames /
  ///   stream_sample_rate`).
  /// - `dt_s` is the amount of device time that will elapse until the next update (typically one
  ///   callback period).
  ///
  /// Returns the new [`Self::playback_rate`].
  #[inline]
  pub fn update(&mut self, buffered_s: f64, dt_s: f64) -> f64 {
    // Keep this method panic-free and stable even if callers pass bogus values in release builds
    // (e.g. NaN due to upstream math bugs). The audio callback path should never panic.
    if !self.playback_rate.is_finite() || self.playback_rate <= 0.0 {
      self.playback_rate = 1.0;
    }
    if !self.integral_error_s.is_finite() {
      self.integral_error_s = 0.0;
    }

    let buffered_s = if buffered_s.is_finite() && buffered_s >= 0.0 {
      buffered_s
    } else {
      self.cfg.target_buffer_s
    };
    let dt_s = if dt_s.is_finite() && dt_s > 0.0 {
      dt_s
    } else {
      // No time elapsed: keep the current rate.
      self.playback_rate = self.playback_rate.clamp(
        1.0 - self.cfg.max_playback_rate_adjust,
        1.0 + self.cfg.max_playback_rate_adjust,
      );
      return self.playback_rate;
    };

    let error_s = buffered_s - self.cfg.target_buffer_s;

    // Gated integral: only learn a persistent drift offset once the buffer is clearly trending
    // outside the desired band.
    if self.cfg.ki > 0.0
      && (error_s > self.cfg.high_watermark_s || error_s < -self.cfg.low_watermark_s)
    {
      self.integral_error_s += error_s * dt_s;

      // Anti-windup: clamp integral so it cannot drive the output beyond the max adjustment.
      let max_integral = self.cfg.max_playback_rate_adjust / self.cfg.ki;
      self.integral_error_s = self.integral_error_s.clamp(-max_integral, max_integral);
    }

    let mut desired_offset = self.cfg.kp * error_s + self.cfg.ki * self.integral_error_s;
    desired_offset = desired_offset.clamp(
      -self.cfg.max_playback_rate_adjust,
      self.cfg.max_playback_rate_adjust,
    );

    let desired_rate = 1.0 + desired_offset;
    self.playback_rate = slew_towards(
      self.playback_rate,
      desired_rate,
      self.cfg.max_slew_per_s * dt_s,
    );

    // Ensure clamp invariants.
    self.playback_rate = self.playback_rate.clamp(
      1.0 - self.cfg.max_playback_rate_adjust,
      1.0 + self.cfg.max_playback_rate_adjust,
    );

    self.playback_rate
  }
}

#[inline]
fn slew_towards(current: f64, target: f64, max_delta: f64) -> f64 {
  if max_delta <= 0.0 {
    return current;
  }
  if target > current {
    (current + max_delta).min(target)
  } else {
    (current - max_delta).max(target)
  }
}

/// A tiny drift-aware linear resampler that consumes frames from a [`PcmF32QueueConsumer`] at a
/// slightly adjusted rate to keep buffering near a target.
///
/// This is intended to live in the real-time audio callback path:
/// - it performs no allocations after construction,
/// - it only touches a small amount of per-stream state (`phase` + two frames of history),
/// - rate changes are driven by [`DriftController`] and are bounded/slew-limited there.
///
/// The resampling model is intentionally simple:
/// - playback-rate adjustment is implemented by varying the resampling ratio
///   (`effective_ratio = base_ratio * playback_rate`),
/// - pitch changes by the same small factor (±1% default), which is typically preferable to
///   discontinuous drop/dup strategies.
#[derive(Debug, Clone)]
pub struct DriftResampler {
  channels: usize,
  src_rate_hz: u32,
  dst_rate_hz: u32,
  base_ratio: f64,
  controller: DriftController,
  /// Fractional position between `frame0` and `frame1`, in `[0, 1)`.
  phase: f64,
  /// Current input frame (`channels` interleaved samples).
  frame0: Vec<f32>,
  /// Next input frame used for interpolation.
  frame1: Vec<f32>,
  /// Whether `frame0`/`frame1` have been initialized.
  initialized: bool,
  last_input_frames: u64,
}

impl DriftResampler {
  /// Create a resampler that reads from a source stream at `src_rate_hz` and produces output at
  /// `dst_rate_hz`.
  ///
  /// For the common case where the decoder already outputs at the device rate, use
  /// `src_rate_hz == dst_rate_hz` (base ratio = 1.0); drift correction will still compensate for
  /// ppm-level clock mismatches.
  pub fn new(
    channels: usize,
    src_rate_hz: u32,
    dst_rate_hz: u32,
    controller_cfg: DriftControllerConfig,
  ) -> Self {
    assert!(channels > 0, "channels must be > 0"); // fastrender-allow-panic
    assert!(src_rate_hz > 0, "src_rate_hz must be > 0"); // fastrender-allow-panic
    assert!(dst_rate_hz > 0, "dst_rate_hz must be > 0"); // fastrender-allow-panic
    let base_ratio = f64::from(src_rate_hz) / f64::from(dst_rate_hz);
    Self {
      channels,
      src_rate_hz,
      dst_rate_hz,
      base_ratio,
      controller: DriftController::new(controller_cfg),
      phase: 0.0,
      frame0: vec![0.0; channels],
      frame1: vec![0.0; channels],
      initialized: false,
      last_input_frames: 0,
    }
  }

  /// Returns the controller's current playback-rate multiplier.
  #[inline]
  pub fn playback_rate(&self) -> f64 {
    self.controller.playback_rate()
  }

  /// Number of source frames popped from the queue during the last [`Self::pop_into`] call.
  #[inline]
  pub fn last_input_frames_consumed(&self) -> u64 {
    self.last_input_frames
  }

  /// Returns the number of prefetched input frames held internally (0 or 2).
  #[inline]
  pub fn prefetched_frames(&self) -> usize {
    usize::from(self.initialized) * 2
  }

  /// Clears interpolation state (drops prefetched frames and resets phase).
  #[inline]
  pub fn reset(&mut self) {
    self.phase = 0.0;
    self.initialized = false;
  }

  /// Pop interleaved f32 samples from `queue` into `out`, applying drift correction.
  ///
  /// `out.len()` must be a multiple of `channels` (any trailing partial frame is ignored and left
  /// untouched, mirroring `PcmF32QueueConsumer::pop_into` semantics).
  ///
  /// This method always returns the number of samples written (a multiple of `channels`).
  pub fn pop_into(&mut self, queue: &mut PcmF32QueueConsumer, out: &mut [f32]) -> usize {
    let channels = self.channels;
    let out_samples = out.len() - (out.len() % channels);
    let out_frames = out_samples / channels;
    if out_frames == 0 {
      self.last_input_frames = 0;
      return 0;
    }

    debug_assert_eq!(queue.channels(), channels);
    debug_assert_eq!(queue.sample_rate_hz(), self.src_rate_hz);

    let dt_s = out_frames as f64 / f64::from(self.dst_rate_hz);
    let buffered_frames = queue
      .buffered_frames()
      .saturating_add(self.prefetched_frames());
    let buffered_s = buffered_frames as f64 / f64::from(self.src_rate_hz);

    let playback_rate = self.controller.update(buffered_s, dt_s);
    let mut step = self.base_ratio * playback_rate;
    if !(step.is_finite()) || step <= 0.0 {
      step = self.base_ratio;
    }

    // Default to silence; we'll overwrite as we successfully synthesize frames.
    out[..out_samples].fill(0.0);
    self.last_input_frames = 0;

    if !self.initialized {
      if queue.pop_into(&mut self.frame0) != channels {
        return out_samples;
      }
      self.last_input_frames += 1;

      if queue.pop_into(&mut self.frame1) != channels {
        // Not enough data for interpolation yet: hold the first frame until more audio arrives.
        self.frame1.copy_from_slice(&self.frame0);
      } else {
        self.last_input_frames += 1;
      }
      self.initialized = true;
    }

    let mut out_idx = 0usize;
    for _ in 0..out_frames {
      let frac = self.phase as f32;
      for ch in 0..channels {
        let a = self.frame0[ch];
        let b = self.frame1[ch];
        // Even if input frames are sanitized, interpolation can still produce non-normal outputs
        // (e.g. when blending between 0 and a very small normal). Flush those to zero so we never
        // emit denormals/NaNs/Infs into downstream mixing/conversion code paths.
        let sample = a + (b - a) * frac;
        out[out_idx + ch] = if sample.is_normal() { sample } else { 0.0 };
      }
      out_idx += channels;

      self.phase += step;
      while self.phase >= 1.0 {
        self.phase -= 1.0;
        std::mem::swap(&mut self.frame0, &mut self.frame1);
        if queue.pop_into(&mut self.frame1) != channels {
          // Underflow mid-block: hold the last frame rather than discarding already-consumed audio.
          self.frame1.copy_from_slice(&self.frame0);
          self.phase = 0.0;
          break;
        }
        self.last_input_frames += 1;
      }
    }

    out_samples
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::media::audio::pcm_f32_queue;

  fn run_simulation(producer_rate_hz: f64) {
    let stream_rate_hz = 48_000.0_f64;
    let device_rate_hz = 48_000.0_f64;
    let stream_rate_u32 = 48_000u32;

    let target_buffer_s = 0.100;
    let low_watermark_s = 0.020;
    let high_watermark_s = 0.020;

    let cfg = DriftControllerConfig {
      target_buffer_s,
      low_watermark_s,
      high_watermark_s,
      max_playback_rate_adjust: 0.01,
      max_slew_per_s: 0.005,
      kp: 0.05,
      ki: 0.01,
    };

    let callback_frames_out: u32 = 480; // 10ms @ 48kHz
    let dt_s = callback_frames_out as f64 / device_rate_hz;
    let steps = (60.0 / dt_s) as usize;

    let mut controller = DriftController::new(cfg);
    let mut clock = StreamFrameClock::new(stream_rate_u32);

    // Start at the target buffered duration.
    let mut buffered_frames: i64 = (target_buffer_s * stream_rate_hz) as i64;

    // Fixed-point-ish fractional accumulators for deterministic integer production/consumption.
    let mut producer_phase: f64 = 0.0;
    let mut resample_phase: f64 = 0.0;

    let mut min_buffer_s = f64::INFINITY;
    let mut max_buffer_s = f64::NEG_INFINITY;

    let mut last_clock = clock.time();

    for _ in 0..steps {
      // Producer: add decoded PCM frames.
      let desired_produce = producer_rate_hz * dt_s + producer_phase;
      let produce_frames = desired_produce.floor() as i64;
      producer_phase = desired_produce - produce_frames as f64;
      buffered_frames += produce_frames;

      // Consumer: consume frames via resampler at an adjusted ratio.
      let desired_consume =
        callback_frames_out as f64 * controller.playback_rate() + resample_phase;
      let mut consume_frames = desired_consume.floor() as i64;
      resample_phase = desired_consume - consume_frames as f64;

      if consume_frames > buffered_frames {
        // In a real callback we'd output silence; for this deterministic test we clamp.
        consume_frames = buffered_frames;
      }
      buffered_frames -= consume_frames;
      clock.advance_frames(consume_frames as u32);

      let now = clock.time();
      assert!(now >= last_clock, "AudioStreamClock must be monotonic");
      last_clock = now;

      let buffered_s = buffered_frames as f64 / stream_rate_hz;
      min_buffer_s = min_buffer_s.min(buffered_s);
      max_buffer_s = max_buffer_s.max(buffered_s);

      controller.update(buffered_s, dt_s);

      // Sanity: rate bounds should always hold.
      assert!(
        (1.0 - cfg.max_playback_rate_adjust..=1.0 + cfg.max_playback_rate_adjust)
          .contains(&controller.playback_rate())
      );
    }

    // The buffer should remain within the configured band (with a tiny numerical tolerance).
    let tol_s = 0.0005; // 0.5ms
    assert!(
      min_buffer_s >= target_buffer_s - low_watermark_s - tol_s,
      "min_buffer_s={min_buffer_s}"
    );
    assert!(
      max_buffer_s <= target_buffer_s + high_watermark_s + tol_s,
      "max_buffer_s={max_buffer_s}"
    );

    // Audio clock should roughly track the producer cadence (i.e. no long-term drift build-up).
    let expected_clock_s = 60.0 * producer_rate_hz / stream_rate_hz;
    let actual_clock_s = clock.time().as_secs_f64();
    assert!(
      (actual_clock_s - expected_clock_s).abs() < 0.030,
      "actual_clock_s={actual_clock_s} expected_clock_s={expected_clock_s}"
    );
  }

  #[test]
  fn drift_controller_keeps_buffer_bounded_faster_producer() {
    // ~0.04% faster than the device: enough to accumulate seconds of latency over long runs.
    run_simulation(48_020.0);
  }

  #[test]
  fn drift_controller_keeps_buffer_bounded_slower_producer() {
    run_simulation(47_980.0);
  }

  fn run_queue_simulation(producer_rate_hz: f64) {
    let src_rate_hz = 48_000u32;
    let dst_rate_hz = 48_000u32;
    let channels = 1usize;

    let target_buffer_s = 0.100;
    let low_watermark_s = 0.020;
    let high_watermark_s = 0.020;

    let cfg = DriftControllerConfig {
      target_buffer_s,
      low_watermark_s,
      high_watermark_s,
      max_playback_rate_adjust: 0.01,
      max_slew_per_s: 0.005,
      kp: 0.05,
      ki: 0.01,
    };

    let callback_frames_out: usize = 480; // 10ms @ 48kHz
    let dt_s = callback_frames_out as f64 / f64::from(dst_rate_hz);
    let steps = (60.0 / dt_s) as usize;

    // Generous capacity so the test never triggers queue drops.
    let capacity_frames = 48_000usize * 4;
    let (mut prod, mut cons) = pcm_f32_queue(channels, src_rate_hz, capacity_frames).unwrap();

    // Pre-fill to target buffered duration so the controller starts in steady state.
    let target_frames = (target_buffer_s * f64::from(src_rate_hz)) as usize;
    let mut zeros = vec![0.0f32; target_frames.max(callback_frames_out + 32) * channels];
    prod.push(&zeros[..target_frames * channels], Duration::ZERO);

    let mut resampler = DriftResampler::new(channels, src_rate_hz, dst_rate_hz, cfg);
    let mut out = vec![0.0f32; callback_frames_out * channels];

    let mut producer_phase = 0.0f64;
    let mut min_buffer_s = f64::INFINITY;
    let mut max_buffer_s = f64::NEG_INFINITY;

    let mut clock = StreamFrameClock::new(src_rate_hz);
    let mut last_clock = clock.time();

    for _ in 0..steps {
      // Producer: add decoded PCM frames.
      let desired_produce = producer_rate_hz * dt_s + producer_phase;
      let produce_frames = desired_produce.floor() as usize;
      producer_phase = desired_produce - produce_frames as f64;
      if produce_frames > 0 {
        if zeros.len() < produce_frames * channels {
          zeros.resize(produce_frames * channels, 0.0);
        }
        prod.push_without_pts(&zeros[..produce_frames * channels]);
      }

      // Consumer: produce `callback_frames_out` output frames, consuming a variable number of input
      // frames based on drift correction.
      resampler.pop_into(&mut cons, &mut out);
      clock.advance_frames(resampler.last_input_frames_consumed() as u32);

      let now = clock.time();
      assert!(now >= last_clock, "stream clock must be monotonic");
      last_clock = now;

      let buffered_frames = cons
        .buffered_frames()
        .saturating_add(resampler.prefetched_frames());
      let buffered_s = buffered_frames as f64 / f64::from(src_rate_hz);
      min_buffer_s = min_buffer_s.min(buffered_s);
      max_buffer_s = max_buffer_s.max(buffered_s);

      assert!(
        (1.0 - cfg.max_playback_rate_adjust..=1.0 + cfg.max_playback_rate_adjust)
          .contains(&resampler.playback_rate())
      );
    }

    let tol_s = 0.0005;
    assert!(
      min_buffer_s >= target_buffer_s - low_watermark_s - tol_s,
      "min_buffer_s={min_buffer_s}"
    );
    assert!(
      max_buffer_s <= target_buffer_s + high_watermark_s + tol_s,
      "max_buffer_s={max_buffer_s}"
    );

    let expected_clock_s = 60.0 * producer_rate_hz / f64::from(src_rate_hz);
    let actual_clock_s = clock.time().as_secs_f64();
    assert!(
      (actual_clock_s - expected_clock_s).abs() < 0.030,
      "actual_clock_s={actual_clock_s} expected_clock_s={expected_clock_s}"
    );
  }

  #[test]
  fn drift_resampler_with_queue_keeps_buffer_bounded_faster_producer() {
    run_queue_simulation(48_020.0);
  }

  #[test]
  fn drift_resampler_with_queue_keeps_buffer_bounded_slower_producer() {
    run_queue_simulation(47_980.0);
  }

  #[test]
  fn drift_resampler_flushes_subnormal_interpolation_outputs() {
    // Configure the controller to be a no-op so the resampler uses a 1:1 ratio and preserves
    // `phase` exactly as set in the test.
    let cfg = DriftControllerConfig {
      target_buffer_s: 0.0,
      low_watermark_s: 0.0,
      high_watermark_s: 0.0,
      max_playback_rate_adjust: 0.0,
      max_slew_per_s: 0.0,
      kp: 0.0,
      ki: 0.0,
    };

    // Set up a queue with two frames: smallest normal, then zero.
    let (mut prod, mut cons) = pcm_f32_queue(1, 48_000, 16).unwrap();
    prod.push(&[f32::MIN_POSITIVE, 0.0], Duration::ZERO);

    let mut resampler = DriftResampler::new(1, 48_000, 48_000, cfg);
    // Force the first output sample to interpolate halfway between frame0 and frame1, which would
    // normally produce a subnormal value (`0.5 * MIN_POSITIVE`).
    resampler.phase = 0.5;

    let mut out = [1.0f32; 1];
    resampler.pop_into(&mut cons, &mut out);

    assert!(
      resampler.last_input_frames_consumed() >= 2,
      "expected resampler to consume input frames"
    );
    // We flush non-normal outputs to +0.0.
    assert_eq!(out[0].to_bits(), 0.0f32.to_bits());
  }

  /// Monotonic stream-time clock used by the deterministic drift simulations.
  ///
  /// This accumulates `frames/sample_rate` in a way that avoids `f64` rounding drift.
  #[derive(Debug, Clone)]
  struct StreamFrameClock {
    sample_rate_hz: u32,
    nanos: u64,
    nanos_remainder: u32,
    nanos_per_frame: u32,
    nanos_per_frame_remainder: u32,
  }

  impl StreamFrameClock {
    fn new(sample_rate_hz: u32) -> Self {
      assert!(sample_rate_hz > 0);
      let nanos_per_frame = 1_000_000_000u32 / sample_rate_hz;
      let nanos_per_frame_remainder = 1_000_000_000u32 % sample_rate_hz;
      Self {
        sample_rate_hz,
        nanos: 0,
        nanos_remainder: 0,
        nanos_per_frame,
        nanos_per_frame_remainder,
      }
    }

    fn time(&self) -> Duration {
      Duration::from_nanos(self.nanos)
    }

    fn advance_frames(&mut self, frames: u32) {
      if frames == 0 {
        return;
      }

      self.nanos = self
        .nanos
        .saturating_add(frames as u64 * self.nanos_per_frame as u64);

      let sr = self.sample_rate_hz as u64;
      let rem_add = frames as u64 * self.nanos_per_frame_remainder as u64;
      let rem_total = self.nanos_remainder as u64 + rem_add;
      self.nanos = self.nanos.saturating_add(rem_total / sr);
      self.nanos_remainder = (rem_total % sr) as u32;
    }
  }
}
