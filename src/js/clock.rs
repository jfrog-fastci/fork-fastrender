use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Abstraction over a monotonic clock used by the JS event loop.
///
/// The HTML event loop (and timer APIs like `setTimeout`) are defined in terms of a "current time"
/// that advances monotonically. To keep unit tests deterministic, FastRender's scheduler uses an
/// injectable clock rather than calling `Instant::now()` directly.
pub trait Clock: Send + Sync + 'static {
  /// Returns a monotonically increasing timestamp.
  ///
  /// The absolute origin is unspecified; callers should only compare/compute deltas.
  fn now(&self) -> Duration;
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
/// Time only advances when [`VirtualClock::advance`] (or [`VirtualClock::set_now`]) is called.
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

  pub fn advance(&self, delta: Duration) {
    let delta = duration_to_nanos_u64(delta);
    let _ = self
      .now_nanos
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
      });
  }
}

impl Clock for VirtualClock {
  fn now(&self) -> Duration {
    Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
  }
}

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  // Duration::as_nanos returns u128.
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}
