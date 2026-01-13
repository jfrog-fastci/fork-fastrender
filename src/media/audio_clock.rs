use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::media::clock::MediaClock;

/// Audio device clock that can be queried between audio callbacks.
///
/// Many audio backends only update a frame counter once per callback. If callers query the clock
/// between callbacks, the time appears to "stair-step" which can introduce jitter in A/V
/// synchronisation (e.g. video frame scheduling).
///
/// [`InterpolatedAudioClock`] addresses this by combining:
/// - a monotonic `frames_written` counter (updated once per audio callback), and
/// - the system monotonic time at the end of the last callback
///
/// The returned time is extrapolated linearly between callbacks and clamped to at most one callback
/// duration to avoid runaway when callbacks pause.
#[derive(Debug)]
pub struct InterpolatedAudioClock {
  sample_rate_hz: u32,
  start: Instant,

  /// Total number of audio frames written/advanced so far.
  frames_written: AtomicU64,

  /// `Instant` (encoded as nanoseconds since `start`, plus one) at the end of the last callback.
  ///
  /// Stored as `nanos + 1` so `0` can represent "unset".
  last_callback_end_nanos_plus_one: AtomicU64,

  /// Frames written in the most recent callback (used to compute the clamp duration).
  last_callback_frames: AtomicU32,

  /// Optional audio timeline at the end of the last callback, derived from backend timestamps
  /// (encoded as nanoseconds, plus one).
  ///
  /// When set, this is preferred over `frames_written / sample_rate` as the base time.
  last_device_time_nanos_plus_one: AtomicU64,

  /// Monotonic guard so `now()` never goes backwards even if readers observe inconsistent
  /// intermediate states.
  last_now_nanos: AtomicU64,
}

impl InterpolatedAudioClock {
  /// Creates a new interpolated audio clock.
  ///
  /// `sample_rate_hz` must match the configured audio output sample rate.
  pub fn new(sample_rate_hz: u32) -> Self {
    assert!(sample_rate_hz != 0, "sample_rate_hz must be nonzero");
    Self {
      sample_rate_hz,
      start: Instant::now(),
      frames_written: AtomicU64::new(0),
      last_callback_end_nanos_plus_one: AtomicU64::new(0),
      last_callback_frames: AtomicU32::new(0),
      last_device_time_nanos_plus_one: AtomicU64::new(0),
      last_now_nanos: AtomicU64::new(0),
    }
  }

  #[must_use]
  pub fn sample_rate_hz(&self) -> u32 {
    self.sample_rate_hz
  }

  /// Returns the total number of frames written so far.
  pub fn frames_written(&self) -> u64 {
    self.frames_written.load(Ordering::Relaxed)
  }

  /// Advances the playhead by `frames` without enabling wall-time interpolation.
  ///
  /// This is intended for synthetic/deterministic backends (e.g. `NullAudioBackend`) that advance
  /// time based on an injected clock rather than real audio callbacks.
  ///
  /// It intentionally resets the interpolation state so [`Self::now`] is derived purely from the
  /// frame counter.
  pub fn advance_frames(&self, frames: u64) {
    if frames == 0 {
      return;
    }

    // Disable interpolation by clamping the \"time since last callback\" window to 0 frames.
    //
    // We still set `last_callback_end_nanos_plus_one` to a non-zero value so `is_started()` becomes
    // true once the playhead advances.
    self
      .last_callback_end_nanos_plus_one
      .store(1, Ordering::Relaxed);
    self
      .last_callback_frames
      .store(0, Ordering::Relaxed);
    self
      .last_device_time_nanos_plus_one
      .store(0, Ordering::Relaxed);

    // Publish the counter update last with Release so readers that Acquire-load `frames_written`
    // observe the above metadata consistently.
    let _ = self
      .frames_written
      .fetch_update(Ordering::Release, Ordering::Relaxed, |current| {
        Some(current.saturating_add(frames))
      });
  }

  /// Updates the clock at the end of an audio callback using the current system time.
  pub fn on_callback_end(&self, frames_in_callback: u32) {
    self.on_callback_end_at(Instant::now(), frames_in_callback, None);
  }

  /// Updates the clock at the end of an audio callback using the current system time and an
  /// optional backend-provided device timestamp.
  ///
  /// `device_time_at_end` is the audio timeline timestamp that corresponds to the end of this
  /// callback (i.e. the end of the frames produced by this callback). Backends that can provide
  /// accurate timestamps (e.g. CPAL on some platforms) should pass `Some(...)`; otherwise pass
  /// `None` to fall back to the frame counter.
  pub fn on_callback_end_with_device_time(
    &self,
    frames_in_callback: u32,
    device_time_at_end: Duration,
  ) {
    self.on_callback_end_at(
      Instant::now(),
      frames_in_callback,
      Some(device_time_at_end),
    );
  }

  /// Same as [`Self::on_callback_end`] but with an explicit `callback_end` instant.
  ///
  /// This is primarily intended for deterministic unit tests.
  pub fn on_callback_end_at(
    &self,
    callback_end: Instant,
    frames_in_callback: u32,
    device_time_at_end: Option<Duration>,
  ) {
    let callback_end_nanos = duration_to_nanos_u64(
      callback_end
        .checked_duration_since(self.start)
        .unwrap_or(Duration::ZERO),
    );
    self
      .last_callback_end_nanos_plus_one
      .store(callback_end_nanos.saturating_add(1), Ordering::Relaxed);
    self
      .last_callback_frames
      .store(frames_in_callback, Ordering::Relaxed);

    let device_time_nanos_plus_one = device_time_at_end.map_or(0, |duration| {
      duration_to_nanos_u64(duration).saturating_add(1)
    });
    self
      .last_device_time_nanos_plus_one
      .store(device_time_nanos_plus_one, Ordering::Relaxed);

    // Publish the update last so readers can `Acquire` this value and see the callback metadata
    // update consistently.
    let _prev = self
      .frames_written
      .fetch_add(u64::from(frames_in_callback), Ordering::Release);
  }

  /// Returns the current audio time.
  pub fn now(&self) -> Duration {
    self.now_at(Instant::now())
  }

  /// Same as [`Self::now`] but with an explicit `now` instant.
  ///
  /// This is primarily intended for deterministic unit tests.
  pub fn now_at(&self, now: Instant) -> Duration {
    // Load the frame counter first with Acquire so subsequent reads see a consistent snapshot of
    // callback metadata published before the Release `fetch_add` in `on_callback_end_at`.
    let frames_written = self.frames_written.load(Ordering::Acquire);

    let base_nanos = match self
      .last_device_time_nanos_plus_one
      .load(Ordering::Relaxed)
    {
      0 => frames_to_nanos(frames_written, self.sample_rate_hz),
      device => device.saturating_sub(1),
    };

    let last_callback_end_plus_one = self
      .last_callback_end_nanos_plus_one
      .load(Ordering::Relaxed);

    let predicted_nanos = if last_callback_end_plus_one == 0 {
      base_nanos
    } else {
      let last_callback_end_nanos = last_callback_end_plus_one.saturating_sub(1);
      let now_nanos_since_start = duration_to_nanos_u64(
        now.checked_duration_since(self.start)
          .unwrap_or(Duration::ZERO),
      );
      let elapsed_nanos = now_nanos_since_start.saturating_sub(last_callback_end_nanos);

      let last_callback_frames = u64::from(self.last_callback_frames.load(Ordering::Relaxed));
      let max_elapsed_nanos = frames_to_nanos(last_callback_frames, self.sample_rate_hz);
      let clamped_elapsed_nanos = elapsed_nanos.min(max_elapsed_nanos);

      base_nanos.saturating_add(clamped_elapsed_nanos)
    };

    // Ensure monotonicity even if readers observe inconsistent intermediate states.
    let previous = self
      .last_now_nanos
      .fetch_max(predicted_nanos, Ordering::Relaxed);
    let output_nanos = previous.max(predicted_nanos);
    Duration::from_nanos(output_nanos)
  }
}

impl MediaClock for InterpolatedAudioClock {
  fn now(&self) -> Duration {
    InterpolatedAudioClock::now(self)
  }

  fn is_started(&self) -> bool {
    // Treat the clock as started once we've observed at least one audio callback.
    self
      .last_callback_end_nanos_plus_one
      .load(Ordering::Relaxed)
      != 0
  }
}

fn frames_to_nanos(frames: u64, sample_rate_hz: u32) -> u64 {
  if sample_rate_hz == 0 {
    return 0;
  }
  let nanos = (u128::from(frames) * 1_000_000_000u128) / u128::from(sample_rate_hz);
  u64::try_from(nanos).unwrap_or(u64::MAX)
}

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn interpolates_smoothly_between_callbacks() {
    // 1000Hz means 1 frame == 1ms, keeping expected values exact.
    let clock = InterpolatedAudioClock::new(1000);
    let start = clock.start;

    let frames_per_callback = 100u32; // 100ms
    let callback_period = Duration::from_millis(100);

    // First callback ends at t=100ms.
    let t1 = start + callback_period;
    clock.on_callback_end_at(t1, frames_per_callback, None);

    let mut last = Duration::ZERO;
    for offset_ms in (100..200).step_by(10) {
      let t = start + Duration::from_millis(offset_ms);
      let reported = clock.now_at(t);
      let expected = Duration::from_millis(offset_ms);
      assert_eq!(reported, expected);
      assert!(reported >= last);
      last = reported;
    }

    // Second callback ends at t=200ms.
    let t2 = start + callback_period * 2;
    clock.on_callback_end_at(t2, frames_per_callback, None);

    for offset_ms in (200..300).step_by(10) {
      let t = start + Duration::from_millis(offset_ms);
      let reported = clock.now_at(t);
      let expected = Duration::from_millis(offset_ms);
      assert_eq!(reported, expected);
      assert!(reported >= last);
      last = reported;
    }
  }

  #[test]
  fn clamps_when_callbacks_stop() {
    let clock = InterpolatedAudioClock::new(1000);
    let start = clock.start;

    let frames_per_callback = 100u32; // 100ms
    let callback_period = Duration::from_millis(100);

    // First callback ends at t=100ms => base is 100ms.
    let t1 = start + callback_period;
    clock.on_callback_end_at(t1, frames_per_callback, None);

    // With no further callbacks, interpolation should be clamped to one callback duration.
    // At t=1000ms, elapsed since last callback is 900ms, but clamp is 100ms.
    let reported = clock.now_at(start + Duration::from_millis(1000));
    assert_eq!(reported, Duration::from_millis(200));

    // Even later, it should not keep increasing.
    let reported2 = clock.now_at(start + Duration::from_millis(5000));
    assert_eq!(reported2, Duration::from_millis(200));

    // When callbacks resume, the clock should not go backwards.
    let t2 = start + Duration::from_millis(1100);
    clock.on_callback_end_at(t2, frames_per_callback, None);
    let reported3 = clock.now_at(t2);
    assert_eq!(reported3, Duration::from_millis(200));
  }

  #[test]
  fn prefers_device_timestamps_when_available() {
    let clock = InterpolatedAudioClock::new(1000);
    let start = clock.start;

    // At t=100ms, pretend the backend says the audio timeline is at 80ms.
    let t1 = start + Duration::from_millis(100);
    clock.on_callback_end_at(
      t1,
      100,
      Some(Duration::from_millis(80)),
    );

    // The base should be 80ms, plus elapsed since callback end.
    let t_query = start + Duration::from_millis(150);
    let reported = clock.now_at(t_query);
    assert_eq!(reported, Duration::from_millis(130));
  }
}
