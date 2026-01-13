//! Media clocking primitives (master clock + per-stream timeline mapping).
//!
//! This module defines the clock abstraction used by media playback for A/V sync and
//! `HTMLMediaElement.currentTime` bookkeeping.
//!
//! Key idea: the UI/event-loop tick is only a wake-up mechanism; it is **not** a time source. Media
//! time is derived from a chosen master clock (audio device time when audio is present).
//!
//! For the full intended clocking model and drift-bug checklist, see `docs/media_clocking.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Abstraction over the master clock used for A/V sync and HTMLMediaElement timekeeping.
///
/// The clock origin is intentionally unspecified; callers should only compute deltas.
///
/// # Semantics
///
/// When this clock is backed by an audio output device (the preferred A/V sync master), `now()`
/// should represent the time of the audio that is being *heard* at the output (speaker), not merely
/// the time at which samples are being *written* into the device callback.
///
/// Backends that only observe callback time or output frame counters should compensate using a
/// best-effort output latency estimate (see `AudioBackend::output_info()` in `src/media/audio/`).
pub trait MediaClock: Send + Sync + 'static {
  fn now(&self) -> Duration;
}

/// Shared clock representing the output device's timebase (e.g. audio hardware clock).
///
/// The audio mixer can share a single instance across all audio streams.
pub type AudioDeviceClock = dyn MediaClock;

/// Default real-time implementation of [`AudioDeviceClock`], backed by [`Instant`].
///
/// This is a sensible fallback for non-audio environments, but it does **not** account for audio
/// output latency. When used as an audio master clock, it should be considered “time of samples
/// being written” rather than “time of samples being heard”.
#[derive(Debug)]
pub struct RealAudioDeviceClock {
  start: Instant,
}

impl Default for RealAudioDeviceClock {
  fn default() -> Self {
    Self {
      start: Instant::now(),
    }
  }
}

impl MediaClock for RealAudioDeviceClock {
  fn now(&self) -> Duration {
    self.start.elapsed()
  }
}

/// A per-stream media clock derived from a shared [`AudioDeviceClock`].
///
/// Each media element gets its own instance so `currentTime` can be tracked independently even
/// when all audio is mixed into a single output device.
pub struct AudioStreamClock {
  device_clock: Arc<AudioDeviceClock>,
  /// Device timestamp corresponding to `start_media_time` (in nanoseconds).
  start_device_time: AtomicU64,
  /// Media timestamp corresponding to `start_device_time` (in nanoseconds).
  start_media_time: AtomicU64,
  /// Playback rate as IEEE-754 bits.
  ///
  /// Stored as f64 bits inside an atomic so audio/video threads can read it without locks.
  rate: AtomicU64,
  /// Last returned timestamp (in nanoseconds) used to clamp to monotonic if the device clock jumps
  /// backwards.
  last_now: AtomicU64,
}

impl std::fmt::Debug for AudioStreamClock {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AudioStreamClock")
      .field("device_clock", &"<dyn MediaClock>")
      .field(
        "start_device_time",
        &Duration::from_nanos(self.start_device_time.load(Ordering::Relaxed)),
      )
      .field(
        "start_media_time",
        &Duration::from_nanos(self.start_media_time.load(Ordering::Relaxed)),
      )
      .field("rate", &self.rate())
      .finish()
  }
}

impl AudioStreamClock {
  pub fn new(device_clock: Arc<AudioDeviceClock>, start_media_time: Duration) -> Self {
    let start_device_time = device_clock.now();
    let start_media_nanos = duration_to_nanos_u64(start_media_time);
    Self {
      device_clock,
      start_device_time: AtomicU64::new(duration_to_nanos_u64(start_device_time)),
      start_media_time: AtomicU64::new(start_media_nanos),
      rate: AtomicU64::new(1.0_f64.to_bits()),
      last_now: AtomicU64::new(start_media_nanos),
    }
  }

  pub fn rate(&self) -> f64 {
    f64::from_bits(self.rate.load(Ordering::Relaxed))
  }

  /// Adjust the playback rate while keeping the returned `now()` continuous.
  pub fn set_rate(&self, new_rate: f64) {
    // Keep behaviour deterministic and panic-free; treat invalid rates as 0 (paused).
    let new_rate = if new_rate.is_finite() && new_rate > 0.0 {
      new_rate
    } else {
      0.0
    };

    // Capture current mapping so the rate change does not introduce a discontinuity.
    let media_now = self.now();
    let device_now = self.device_clock.now();

    let media_now_nanos = duration_to_nanos_u64(media_now);
    self.start_device_time.store(duration_to_nanos_u64(device_now), Ordering::Relaxed);
    self.start_media_time.store(media_now_nanos, Ordering::Relaxed);
    self.last_now.store(media_now_nanos, Ordering::Relaxed);
    self.rate.store(new_rate.to_bits(), Ordering::Relaxed);
  }

  /// Resets the mapping so that `now()` returns `new_media_time` at the current device time.
  pub fn seek(&self, new_media_time: Duration) {
    let device_now = self.device_clock.now();
    let new_media_nanos = duration_to_nanos_u64(new_media_time);
    self.start_device_time.store(duration_to_nanos_u64(device_now), Ordering::Relaxed);
    self.start_media_time.store(new_media_nanos, Ordering::Relaxed);
    self.last_now.store(new_media_nanos, Ordering::Relaxed);
  }

  fn compute_now_nanos(&self) -> u64 {
    let device_now = duration_to_nanos_u64(self.device_clock.now());
    let start_device = self.start_device_time.load(Ordering::Relaxed);
    let start_media = self.start_media_time.load(Ordering::Relaxed);
    let rate = self.rate();

    let delta_device_nanos = device_now.saturating_sub(start_device);
    let scaled_delta_nanos = scale_nanos(delta_device_nanos, rate);
    let candidate = start_media.saturating_add(scaled_delta_nanos);

    // Clamp to monotonic if the device clock jumps backwards or the mapping changes unexpectedly.
    let mut last = self.last_now.load(Ordering::Relaxed);
    loop {
      if candidate <= last {
        return last;
      }

      match self
        .last_now
        .compare_exchange_weak(last, candidate, Ordering::Relaxed, Ordering::Relaxed)
      {
        Ok(_) => return candidate,
        Err(observed) => last = observed,
      }
    }
  }
}

impl MediaClock for AudioStreamClock {
  fn now(&self) -> Duration {
    Duration::from_nanos(self.compute_now_nanos())
  }
}

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  // Duration::as_nanos returns u128.
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn scale_nanos(nanos: u64, rate: f64) -> u64 {
  if !(rate.is_finite()) || rate <= 0.0 {
    return 0;
  }
  // Keep the mapping stable by rounding once (from the same absolute origin) instead of
  // incrementally accumulating floating-point error.
  let scaled = (nanos as f64) * rate;
  if !(scaled.is_finite()) || scaled <= 0.0 {
    return 0;
  }
  // Round to the nearest nanosecond.
  u64::try_from(scaled.round() as u128).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Debug, Default)]
  struct FakeDeviceClock {
    now_nanos: AtomicU64,
  }

  impl FakeDeviceClock {
    fn set_now(&self, now: Duration) {
      self
        .now_nanos
        .store(duration_to_nanos_u64(now), Ordering::Relaxed);
    }

    fn advance(&self, delta: Duration) {
      let delta_nanos = duration_to_nanos_u64(delta);
      let _ = self
        .now_nanos
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
          Some(current.saturating_add(delta_nanos))
        });
    }
  }

  impl MediaClock for FakeDeviceClock {
    fn now(&self) -> Duration {
      Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
    }
  }

  #[test]
  fn maps_device_clock_to_media_clock() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(10));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(5));

    device_clock.advance(Duration::from_secs(3));
    assert_eq!(stream_clock.now(), Duration::from_secs(8));
  }

  #[test]
  fn clamps_monotonic_if_device_clock_goes_backwards() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(0));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(0));

    device_clock.set_now(Duration::from_secs(5));
    assert_eq!(stream_clock.now(), Duration::from_secs(5));

    // Device clock jumps backwards: stream clock must not.
    device_clock.set_now(Duration::from_secs(4));
    assert_eq!(stream_clock.now(), Duration::from_secs(5));

    device_clock.set_now(Duration::from_secs(6));
    assert_eq!(stream_clock.now(), Duration::from_secs(6));
  }

  #[test]
  fn seek_resets_start_times() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(0));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(0));

    device_clock.set_now(Duration::from_secs(5));
    assert_eq!(stream_clock.now(), Duration::from_secs(5));

    // Seek back to 1s.
    stream_clock.seek(Duration::from_secs(1));
    assert_eq!(stream_clock.now(), Duration::from_secs(1));

    // 1s of device time advances 1s of media time at rate 1.
    device_clock.set_now(Duration::from_secs(6));
    assert_eq!(stream_clock.now(), Duration::from_secs(2));
  }

  #[test]
  fn changing_rate_is_continuous_and_does_not_drift() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(0));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(0));

    device_clock.set_now(Duration::from_secs(10));
    assert_eq!(stream_clock.now(), Duration::from_secs(10));

    // Slow down to half speed; current time should remain continuous.
    stream_clock.set_rate(0.5);
    assert_eq!(stream_clock.now(), Duration::from_secs(10));

    device_clock.set_now(Duration::from_secs(12));
    assert_eq!(stream_clock.now(), Duration::from_secs(11));

    // Drift test: rate=1.25 (5/4) should map 1ms -> 1.25ms exactly.
    let drift_device_clock = Arc::new(FakeDeviceClock::default());
    drift_device_clock.set_now(Duration::from_secs(0));
    let drift_clock = AudioStreamClock::new(drift_device_clock.clone(), Duration::from_secs(0));
    drift_clock.set_rate(1.25);

    for i in 0..10_000_u64 {
      drift_device_clock.set_now(Duration::from_millis(i));
      let expected_nanos = i.saturating_mul(1_250_000);
      assert_eq!(drift_clock.now(), Duration::from_nanos(expected_nanos));
    }
  }
}
