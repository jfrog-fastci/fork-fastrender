use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Monotonic timestamp since an unspecified origin.
pub type Instant = Duration;

pub trait Clock: Send + Sync + 'static {
  fn now(&self) -> Instant;
}

#[derive(Debug)]
pub struct RealClock {
  start: std::time::Instant,
}

impl RealClock {
  pub fn new() -> Self {
    Self::with_start(std::time::Instant::now())
  }

  pub fn with_start(start: std::time::Instant) -> Self {
    Self { start }
  }

  /// Returns the clock's origin in `std::time::Instant` space.
  pub fn origin(&self) -> std::time::Instant {
    self.start
  }
}

impl Default for RealClock {
  fn default() -> Self {
    Self::new()
  }
}

impl Clock for RealClock {
  fn now(&self) -> Instant {
    self.start.elapsed()
  }
}

#[derive(Debug)]
pub struct VirtualClock {
  now_nanos: AtomicU64,
}

impl VirtualClock {
  pub fn new() -> Self {
    Self {
      now_nanos: AtomicU64::new(0),
    }
  }

  pub fn now(&self) -> Instant {
    nanos_to_duration(self.now_nanos.load(Ordering::Relaxed))
  }

  /// Sets the current timestamp.
  ///
  /// This is intended for tests; callers are responsible for preserving
  /// monotonicity when setting the time backwards would be problematic.
  pub fn set_now(&self, now: Instant) {
    self.now_nanos
      .store(duration_to_nanos_saturating(now), Ordering::Relaxed);
  }

  /// Advances the current time by `duration`, saturating on overflow.
  pub fn advance(&self, duration: Duration) {
    let delta = duration_to_nanos_saturating(duration);
    let _ = self.now_nanos.fetch_update(
      Ordering::Relaxed,
      Ordering::Relaxed,
      |current| Some(current.saturating_add(delta)),
    );
  }
}

impl Default for VirtualClock {
  fn default() -> Self {
    Self::new()
  }
}

impl Clock for VirtualClock {
  fn now(&self) -> Instant {
    VirtualClock::now(self)
  }
}

fn duration_to_nanos_saturating(duration: Duration) -> u64 {
  let nanos = duration.as_nanos();
  if nanos > u64::MAX as u128 {
    u64::MAX
  } else {
    nanos as u64
  }
}

fn nanos_to_duration(nanos: u64) -> Duration {
  Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn virtual_clock_advance_increases_now() {
    let clock = VirtualClock::new();
    assert_eq!(clock.now(), Duration::ZERO);

    clock.advance(Duration::from_millis(500));
    assert_eq!(clock.now(), Duration::from_millis(500));

    clock.advance(Duration::from_secs(2));
    assert_eq!(clock.now(), Duration::from_millis(2500));
  }

  #[test]
  fn virtual_clock_set_now_sets_exact_time() {
    let clock = VirtualClock::new();
    clock.set_now(Duration::from_millis(123));
    assert_eq!(clock.now(), Duration::from_millis(123));

    clock.set_now(Duration::from_nanos(7));
    assert_eq!(clock.now(), Duration::from_nanos(7));
  }

  #[test]
  fn virtual_clock_advance_saturates_on_overflow() {
    let clock = VirtualClock::new();
    clock.set_now(Duration::from_nanos(u64::MAX - 1));
    clock.advance(Duration::from_nanos(10));
    assert_eq!(clock.now(), Duration::from_nanos(u64::MAX));
  }

  #[test]
  fn duration_to_nanos_saturating_handles_huge_durations() {
    let huge = Duration::from_secs(u64::MAX);
    assert_eq!(duration_to_nanos_saturating(huge), u64::MAX);
  }
}
