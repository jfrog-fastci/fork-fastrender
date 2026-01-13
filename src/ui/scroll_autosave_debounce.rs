use std::time::{Duration, Instant};

/// Schedule and debounce scroll-driven session autosaves.
///
/// The windowed browser persists tab scroll offsets (`BrowserSessionTab::scroll_css`) by building a
/// full session snapshot on the UI thread and sending it to the background autosave worker.
///
/// Scroll updates can arrive at high frequency (trackpads, momentum scroll, async scroll
/// corrections). Snapshot generation is expensive, so callers should debounce scroll-driven
/// autosaves with a trailing-edge timer:
/// - Each scroll change resets the deadline to `now + debounce`.
/// - Once the deadline elapses, a single autosave is considered "due".
///
/// This module provides small pure helpers so the scheduling logic can be unit tested without
/// depending on the windowed browser event loop.

/// Record a scroll viewport change, returning the new `(pending_deadline, due)` state.
#[inline]
pub fn note_scroll_change(now: Instant, debounce: Duration) -> (Option<Instant>, bool) {
  (Some(now + debounce), false)
}

/// Promote a pending scroll autosave deadline to `due` once it elapses.
#[inline]
pub fn poll_deadline(
  pending_deadline: Option<Instant>,
  due: bool,
  now: Instant,
) -> (Option<Instant>, bool) {
  if due {
    // Once due, keep the flag set until the caller consumes it. Keep `pending_deadline` cleared so
    // callers don't need to special-case the "due + pending" state.
    return (None, true);
  }

  match pending_deadline {
    Some(deadline) if now >= deadline => (None, true),
    other => (other, false),
  }
}

/// Consume a due flag, clearing both the due and pending deadline state.
#[inline]
pub fn take_due(pending_deadline: Option<Instant>, due: bool) -> (Option<Instant>, bool, bool) {
  if due {
    (None, false, true)
  } else {
    (pending_deadline, false, false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn multiple_scroll_changes_reset_deadline_trailing_edge() {
    let debounce = Duration::from_millis(800);
    let t0 = Instant::now();

    let (deadline0, due0) = note_scroll_change(t0, debounce);
    assert_eq!(deadline0, Some(t0 + debounce));
    assert!(!due0);

    let t1 = t0 + Duration::from_millis(100);
    let (deadline1, due1) = note_scroll_change(t1, debounce);
    assert_eq!(deadline1, Some(t1 + debounce));
    assert!(!due1);
    assert_ne!(deadline1, deadline0, "expected deadline to be reset on new scroll change");
  }

  #[test]
  fn single_scroll_change_produces_exactly_one_autosave_after_idle() {
    let debounce = Duration::from_millis(750);
    let t0 = Instant::now();

    let (mut pending, mut due) = note_scroll_change(t0, debounce);
    // Before the deadline, nothing should be due.
    (pending, due) = poll_deadline(pending, due, t0 + debounce - Duration::from_millis(1));
    assert_eq!(pending, Some(t0 + debounce));
    assert!(!due);

    // At/after the deadline, an autosave becomes due exactly once.
    (pending, due) = poll_deadline(pending, due, t0 + debounce);
    assert_eq!(pending, None);
    assert!(due);

    let (pending2, due2, fired) = take_due(pending, due);
    assert!(fired);
    assert_eq!(pending2, None);
    assert!(!due2);

    // No further autosaves should become due without another scroll change.
    let (pending3, due3) = poll_deadline(pending2, due2, t0 + debounce + Duration::from_secs(5));
    assert_eq!(pending3, None);
    assert!(!due3);
  }
}

