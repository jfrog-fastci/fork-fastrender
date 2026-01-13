use std::time::{Duration, Instant};

/// UI-thread scheduler for throttling/clamping expensive "send full store snapshot" messages to the
/// profile autosave worker.
///
/// The profile autosave worker already debounces *disk writes*, but the UI thread can still pay the
/// O(n) cost of cloning large stores (history/bookmarks) on every mutation if it eagerly sends a
/// full snapshot each time.
///
/// This scheduler keeps UI-thread work close to O(1) per mutation by:
/// - tracking whether there is a pending update to send,
/// - enforcing a minimum interval between sends, and
/// - coalescing multiple mutations into a single send containing the latest state.
#[derive(Debug, Clone)]
pub struct AutosaveSendScheduler {
  min_interval: Duration,
  pending: bool,
  last_sent_at: Option<Instant>,
}

impl AutosaveSendScheduler {
  pub fn new(min_interval: Duration) -> Self {
    Self {
      min_interval,
      pending: false,
      last_sent_at: None,
    }
  }

  /// Mark that the underlying store has mutated and the latest snapshot should eventually be sent
  /// to the autosave worker.
  pub fn mark_dirty(&mut self) {
    self.pending = true;
  }

  pub fn has_pending(&self) -> bool {
    self.pending
  }

  /// Returns the earliest `Instant` at which the next send is allowed (based on `min_interval`).
  ///
  /// The caller can use this to arm the event loop (e.g. `ControlFlow::WaitUntil`) so the UI thread
  /// wakes up when it is time to send the pending snapshot.
  pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
    if !self.pending {
      return None;
    }

    let Some(last_sent_at) = self.last_sent_at else {
      return Some(now);
    };

    let due = last_sent_at + self.min_interval;
    Some(due.max(now))
  }

  /// Whether a pending snapshot should be sent right now (subject to the minimum interval).
  pub fn should_send(&self, now: Instant) -> bool {
    if !self.pending {
      return false;
    }

    let Some(last_sent_at) = self.last_sent_at else {
      return true;
    };

    now.duration_since(last_sent_at) >= self.min_interval
  }

  /// Consume the pending flag if a send is allowed right now.
  ///
  /// Returns `true` if the caller should send a snapshot *now* (and then clone/send the store).
  pub fn take_if_due(&mut self, now: Instant) -> bool {
    if !self.should_send(now) {
      return false;
    }
    self.pending = false;
    self.last_sent_at = Some(now);
    true
  }

  /// Consume the pending flag and allow an immediate send, bypassing the minimum interval.
  ///
  /// Returns `true` if there was a pending snapshot to send.
  pub fn take_force(&mut self, now: Instant) -> bool {
    if !self.pending {
      return false;
    }
    self.pending = false;
    self.last_sent_at = Some(now);
    true
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn no_sends_when_no_changes() {
    let mut scheduler = AutosaveSendScheduler::new(Duration::from_millis(250));
    let now = Instant::now();

    assert!(!scheduler.take_if_due(now));
    assert_eq!(scheduler.next_deadline(now), None);
    assert!(!scheduler.has_pending());
  }

  #[test]
  fn coalesces_multiple_mutations_into_one_send_per_interval() {
    let mut scheduler = AutosaveSendScheduler::new(Duration::from_millis(500));
    let t0 = Instant::now();

    scheduler.mark_dirty();
    assert!(scheduler.take_if_due(t0), "first mutation should send immediately");

    // Multiple mutations within the interval should not trigger additional sends.
    scheduler.mark_dirty();
    scheduler.mark_dirty();
    assert!(
      !scheduler.take_if_due(t0 + Duration::from_millis(100)),
      "should rate limit sends within interval"
    );

    // The scheduler should ask to wake at last_sent + interval.
    assert_eq!(
      scheduler.next_deadline(t0 + Duration::from_millis(100)),
      Some(t0 + Duration::from_millis(500))
    );

    // Once the interval elapses, one send covers all queued mutations.
    assert!(scheduler.take_if_due(t0 + Duration::from_millis(500)));
    assert!(!scheduler.has_pending());
  }

  #[test]
  fn flush_bypasses_rate_limit() {
    let mut scheduler = AutosaveSendScheduler::new(Duration::from_millis(500));
    let t0 = Instant::now();

    scheduler.mark_dirty();
    assert!(scheduler.take_if_due(t0));

    // New mutation arrives too soon for a normal send.
    scheduler.mark_dirty();
    let t1 = t0 + Duration::from_millis(10);
    assert!(!scheduler.take_if_due(t1));

    // Flush forces a send immediately.
    assert!(scheduler.take_force(t1));
    assert!(!scheduler.has_pending());

    // Nothing pending: force send should be a no-op.
    assert!(!scheduler.take_force(t1));
  }
}

