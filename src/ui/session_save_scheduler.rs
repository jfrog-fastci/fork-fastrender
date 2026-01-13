use std::time::{Duration, Instant};

/// UI-thread scheduler for debounced session snapshot generation.
///
/// The browser's session autosave pipeline has two expensive steps:
/// 1) Building a `BrowserSession` snapshot on the UI thread (cloning tab/window state).
/// 2) Writing the snapshot to disk (handled asynchronously + debounced by `SessionAutosave`).
///
/// `SessionSaveScheduler` addresses (1) by allowing callers to cheaply mark the session as dirty
/// during high-frequency events (e.g. window resize/move), and only build a full snapshot at most
/// once per debounce window.
#[derive(Debug, Clone)]
pub struct SessionSaveScheduler {
  debounce: Duration,
  next_flush_at: Option<Instant>,
}

impl Default for SessionSaveScheduler {
  fn default() -> Self {
    Self::new()
  }
}

impl SessionSaveScheduler {
  const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(400);

  pub fn new() -> Self {
    Self::with_debounce(Self::DEFAULT_DEBOUNCE)
  }

  pub fn with_debounce(debounce: Duration) -> Self {
    Self {
      debounce,
      next_flush_at: None,
    }
  }

  pub fn debounce(&self) -> Duration {
    self.debounce
  }

  /// Mark the session as dirty, scheduling a flush if one is not already pending.
  ///
  /// This method is designed to be cheap so it can be called in response to frequent UI events.
  pub fn mark_dirty(&mut self, now: Instant) {
    if self.next_flush_at.is_none() {
      self.next_flush_at = Some(now + self.debounce);
    }
  }

  /// Return the next time the caller should wake up to flush the pending snapshot.
  pub fn next_deadline(&self, _now: Instant) -> Option<Instant> {
    self.next_flush_at
  }

  /// Returns `true` when the debounce deadline has elapsed and a flush should occur.
  pub fn should_flush(&self, now: Instant) -> bool {
    self
      .next_flush_at
      .is_some_and(|deadline| now >= deadline)
  }

  /// Clear the pending flush flag.
  ///
  /// Call this after [`Self::should_flush`] returns true and the caller has performed the actual
  /// snapshot build + autosave request.
  pub fn take_pending(&mut self) -> bool {
    self.next_flush_at.take().is_some()
  }
}

/// Returns `true` when a persisted session may have changed based on `session_revision()` deltas.
///
/// Front-ends can capture `BrowserAppState::session_revision()` before and after processing a batch
/// of events, and schedule an autosave when this returns `true`.
pub fn session_dirty_from_revision_delta(before: u64, after: u64) -> bool {
  before != after
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn next_deadline_none_when_idle() {
    let scheduler = SessionSaveScheduler::with_debounce(Duration::from_millis(100));
    let now = Instant::now();
    assert_eq!(scheduler.next_deadline(now), None);
    assert!(!scheduler.should_flush(now));
  }

  #[test]
  fn coalesces_multiple_dirty_marks_into_single_flush() {
    let mut scheduler = SessionSaveScheduler::with_debounce(Duration::from_millis(200));
    let t0 = Instant::now();

    scheduler.mark_dirty(t0);
    assert_eq!(scheduler.next_deadline(t0), Some(t0 + Duration::from_millis(200)));

    // Additional dirty marks inside the debounce window should not create additional flushes (or
    // extend the current one).
    scheduler.mark_dirty(t0 + Duration::from_millis(50));
    assert_eq!(scheduler.next_deadline(t0), Some(t0 + Duration::from_millis(200)));

    assert!(!scheduler.should_flush(t0 + Duration::from_millis(199)));
    assert!(scheduler.should_flush(t0 + Duration::from_millis(200)));

    assert!(scheduler.take_pending());
    assert_eq!(scheduler.next_deadline(t0 + Duration::from_millis(200)), None);
    assert!(!scheduler.should_flush(t0 + Duration::from_millis(400)));
  }

  #[test]
  fn dirty_after_flush_schedules_next_window() {
    let mut scheduler = SessionSaveScheduler::with_debounce(Duration::from_millis(100));
    let t0 = Instant::now();

    scheduler.mark_dirty(t0);
    assert!(scheduler.should_flush(t0 + Duration::from_millis(100)));
    assert!(scheduler.take_pending());

    // A new dirty event after the flush should schedule a new deadline.
    let t1 = t0 + Duration::from_millis(101);
    scheduler.mark_dirty(t1);
    assert_eq!(scheduler.next_deadline(t1), Some(t1 + Duration::from_millis(100)));
    assert!(!scheduler.should_flush(t1 + Duration::from_millis(99)));
    assert!(scheduler.should_flush(t1 + Duration::from_millis(100)));
  }

  #[test]
  fn session_dirty_from_revision_delta_detects_change() {
    assert!(!session_dirty_from_revision_delta(5, 5));
    assert!(session_dirty_from_revision_delta(5, 6));
  }
}
