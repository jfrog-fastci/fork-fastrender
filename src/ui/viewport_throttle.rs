use std::time::Duration;

/// Deterministic rate limiter for high-frequency viewport updates.
///
/// The windowed browser UI can produce a burst of resize/scale updates (especially during window
/// drags). Forwarding every intermediate viewport to the render worker is wasteful and can make the
/// UI feel janky. `ViewportThrottle` implements a leading+trailing throttle:
/// - the first update is emitted immediately,
/// - subsequent updates within `interval` are coalesced (only the latest is kept),
/// - once `interval` has elapsed, the latest pending update is emitted ("final update sent").
///
/// This type is **time-source agnostic**: callers provide a monotonic `now` value (typically a
/// `Duration` since startup). This makes it easy to unit test without sleeping.
#[derive(Debug, Clone)]
pub struct ViewportThrottle<T> {
  interval: Duration,
  last_sent_at: Option<Duration>,
  pending: Option<T>,
}

impl<T> ViewportThrottle<T> {
  /// Create a new throttle with the given minimum spacing between emissions.
  pub fn new(interval: Duration) -> Self {
    Self {
      interval,
      last_sent_at: None,
      pending: None,
    }
  }

  pub fn interval(&self) -> Duration {
    self.interval
  }

  /// Returns true when an update is pending and will be emitted once the throttle interval elapses.
  pub fn has_pending(&self) -> bool {
    self.pending.is_some()
  }

  /// If an update is pending, return the earliest time at which it can be emitted.
  pub fn next_due_at(&self) -> Option<Duration> {
    if self.pending.is_none() {
      return None;
    }
    // If we have never emitted, allow immediate flush.
    Some(
      self
        .last_sent_at
        .unwrap_or(Duration::ZERO)
        .saturating_add(self.interval),
    )
  }

  /// Offer a new viewport update at time `now`.
  ///
  /// Returns `Some(value)` when the caller should emit the update immediately, or `None` when the
  /// update was throttled and stored as "pending".
  pub fn push(&mut self, now: Duration, value: T) -> Option<T> {
    match self.last_sent_at {
      None => {
        self.last_sent_at = Some(now);
        self.pending = None;
        Some(value)
      }
      Some(last) => {
        let due = last.saturating_add(self.interval);
        if now >= due {
          // It's been long enough; emit the latest value immediately. Any older pending update is
          // obsolete, so drop it.
          self.last_sent_at = Some(now);
          self.pending = None;
          Some(value)
        } else {
          // Within the throttle interval; keep only the latest pending update.
          self.pending = Some(value);
          None
        }
      }
    }
  }

  /// Flush a pending update if the throttle interval has elapsed.
  ///
  /// Returns `Some(value)` when a pending update becomes eligible, otherwise `None`.
  pub fn poll(&mut self, now: Duration) -> Option<T> {
    let Some(value) = self.pending.take() else {
      return None;
    };

    match self.last_sent_at {
      None => {
        self.last_sent_at = Some(now);
        Some(value)
      }
      Some(last) => {
        let due = last.saturating_add(self.interval);
        if now >= due {
          self.last_sent_at = Some(now);
          Some(value)
        } else {
          // Not yet due; re-store.
          self.pending = Some(value);
          None
        }
      }
    }
  }
}

