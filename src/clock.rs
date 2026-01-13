use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Abstraction over a monotonic clock.
///
/// Multiple subsystems (e.g. the JavaScript event loop, animation timelines, and future media
/// pipelines) need a notion of "current time" that advances monotonically.
///
/// To keep unit tests deterministic, FastRender uses an injectable clock rather than calling
/// `Instant::now()` directly.
///
/// Deterministic hosts can additionally call [`Clock::advance`] to fast-forward time without
/// sleeping in real time. Not all clock implementations support this; the default implementation is
/// a no-op.
pub trait Clock: Send + Sync + 'static {
  /// Returns a monotonically increasing timestamp.
  ///
  /// The absolute origin is unspecified; callers should only compare/compute deltas.
  fn now(&self) -> Duration;

  /// Best-effort deterministic time advancement.
  ///
  /// Virtual/test clocks can implement this to advance their internal timestamp by `delta`
  /// (typically saturating on overflow).
  ///
  /// Real-time clocks should ignore this: they are driven by the platform clock and cannot be
  /// advanced deterministically.
  fn advance(&self, _delta: Duration) {}
}

/// A real-time monotonic clock backed by [`Instant`].
#[derive(Debug)]
pub struct RealClock {
  start: Instant,
}

impl Default for RealClock {
  fn default() -> Self {
    Self {
      start: Instant::now(),
    }
  }
}

impl Clock for RealClock {
  fn now(&self) -> Duration {
    self.start.elapsed()
  }
}

/// A deterministic clock for tests.
///
/// Time only advances when [`Clock::advance`] (or [`VirtualClock::set_now`]) is called.
#[derive(Debug, Default)]
pub struct VirtualClock {
  now_nanos: AtomicU64,
}

impl VirtualClock {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn now(&self) -> Duration {
    Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
  }

  pub fn set_now(&self, now: Duration) {
    let nanos = duration_to_nanos_u64(now);
    self.now_nanos.store(nanos, Ordering::Relaxed);
  }

  fn advance_inner(&self, delta: Duration) {
    let delta = duration_to_nanos_u64(delta);
    let _ = self
      .now_nanos
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
      });
  }

  /// Advances the current timestamp by `delta`, saturating on overflow.
  ///
  /// Prefer [`Clock::advance`] when holding this clock behind a trait object (`Arc<dyn Clock>`).
  pub fn advance(&self, delta: Duration) {
    self.advance_inner(delta);
  }
}

impl Clock for VirtualClock {
  fn now(&self) -> Duration {
    Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
  }

  fn advance(&self, delta: Duration) {
    self.advance_inner(delta);
  }
}

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  // `Duration::as_nanos` returns `u128`.
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  #[test]
  fn clock_advance_updates_virtual_clock_through_trait_object() {
    let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new());
    assert_eq!(clock.now(), Duration::ZERO);

    clock.advance(Duration::from_millis(5));
    assert_eq!(clock.now(), Duration::from_millis(5));
  }

  #[test]
  fn clock_advance_is_noop_for_real_clock() {
    let clock: Arc<dyn Clock> = Arc::new(RealClock::default());
    let t0 = clock.now();

    // `RealClock` cannot be deterministically advanced; this should be a no-op and must not panic.
    clock.advance(Duration::from_secs(10));
    let t1 = clock.now();
    assert!(t1 >= t0);
  }
}
