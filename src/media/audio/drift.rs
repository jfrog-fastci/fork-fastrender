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

use std::time::Duration;

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
    debug_assert!(buffered_s.is_finite() && buffered_s >= 0.0);
    debug_assert!(dt_s.is_finite() && dt_s > 0.0);

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

    // Ensure clamp invariants even if dt_s is 0 or NaN in release builds.
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

/// Monotonic media-time clock for a single audio stream.
///
/// This clock is typically advanced by the number of *input* frames consumed (i.e. the stream
/// timebase), not the number of device frames produced.
#[derive(Debug, Clone)]
pub struct AudioStreamClock {
  sample_rate_hz: u32,
  // Total frames consumed in the stream timebase.
  frames: u64,
  // Cached `floor(frames * 1e9 / sample_rate_hz)` in nanoseconds, with remainder tracked
  // separately to keep the clock monotonic and deterministic.
  nanos: u64,
  nanos_remainder: u32,
  nanos_per_frame: u32,
  nanos_per_frame_remainder: u32,
}

impl AudioStreamClock {
  pub fn new(sample_rate_hz: u32) -> Self {
    assert!(sample_rate_hz > 0);
    let nanos_per_frame = 1_000_000_000u32 / sample_rate_hz;
    let nanos_per_frame_remainder = 1_000_000_000u32 % sample_rate_hz;
    Self {
      sample_rate_hz,
      frames: 0,
      nanos: 0,
      nanos_remainder: 0,
      nanos_per_frame,
      nanos_per_frame_remainder,
    }
  }

  #[inline]
  pub fn time(&self) -> Duration {
    Duration::from_nanos(self.nanos)
  }

  #[inline]
  pub fn frames(&self) -> u64 {
    self.frames
  }

  #[inline]
  pub fn sample_rate_hz(&self) -> u32 {
    self.sample_rate_hz
  }

  #[inline]
  pub fn advance_frames(&mut self, frames: u32) {
    if frames == 0 {
      return;
    }
    self.frames += frames as u64;

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

#[cfg(test)]
mod tests {
  use super::*;

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
    let mut clock = AudioStreamClock::new(stream_rate_u32);

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
}
