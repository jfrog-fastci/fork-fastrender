use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSyncAction {
  /// Present the frame immediately.
  PresentNow,
  /// Wait for the given duration, then re-check/present.
  ///
  /// The duration is guaranteed to be non-negative (i.e. `Duration::ZERO` is the minimum).
  WaitUntil(Duration),
  /// Drop the frame (it is too late to present).
  Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvSyncConfig {
  /// How much "ahead" of `master_time` we still consider acceptable to present immediately.
  pub tolerance: Duration,
  /// If a frame is this far behind the master clock, it is dropped.
  pub max_late: Duration,
  /// If a frame is this far ahead of the master clock, the scheduler should wait.
  pub max_early: Duration,
}

/// Decide what to do with a decoded video frame (identified by `frame_pts`) given the current
/// master clock time (`master_time`).
///
/// This helper is intentionally independent of decoding/rendering; it is purely drift/threshold
/// logic.
#[must_use]
pub fn decide(master_time: Duration, frame_pts: Duration, cfg: &AvSyncConfig) -> VideoSyncAction {
  // Too-late threshold: `master_time - max_late` (saturating to `0` to avoid underflow).
  let drop_before = master_time.saturating_sub(cfg.max_late);
  if frame_pts < drop_before {
    return VideoSyncAction::Drop;
  }

  // Present-now threshold: `master_time + tolerance` (saturating to avoid overflow).
  let present_until = master_time.saturating_add(cfg.tolerance);
  if frame_pts <= present_until {
    return VideoSyncAction::PresentNow;
  }

  // Frame is early; compute the wait time using saturating arithmetic so jitter never causes a
  // negative duration.
  let wait = frame_pts.saturating_sub(master_time);

  // If the frame is very early (beyond `max_early`), the scheduler should definitely wait.
  // For slightly-early frames (beyond `tolerance`), we also wait until `frame_pts`.
  let wait_threshold = master_time.saturating_add(cfg.max_early);
  if frame_pts > wait_threshold {
    return VideoSyncAction::WaitUntil(wait);
  }
  VideoSyncAction::WaitUntil(wait)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn drops_frames_strictly_later_than_max_late() {
    let cfg = AvSyncConfig {
      tolerance: Duration::from_millis(10),
      max_late: Duration::from_millis(50),
      max_early: Duration::from_millis(10),
    };

    let master = Duration::from_secs(1);
    let just_too_late = master - cfg.max_late - Duration::from_nanos(1);
    assert_eq!(decide(master, just_too_late, &cfg), VideoSyncAction::Drop);

    // Boundary: exactly `master - max_late` is *not* dropped.
    let at_boundary = master - cfg.max_late;
    assert_eq!(
      decide(master, at_boundary, &cfg),
      VideoSyncAction::PresentNow
    );
  }

  #[test]
  fn presents_when_within_tolerance_including_boundary() {
    let cfg = AvSyncConfig {
      tolerance: Duration::from_millis(10),
      max_late: Duration::from_millis(50),
      max_early: Duration::from_millis(40),
    };

    let master = Duration::from_secs(1);

    // On-time.
    assert_eq!(decide(master, master, &cfg), VideoSyncAction::PresentNow);

    // Slightly early (within tolerance).
    assert_eq!(
      decide(master, master + cfg.tolerance, &cfg),
      VideoSyncAction::PresentNow
    );

    // Just outside tolerance: should wait.
    assert_eq!(
      decide(master, master + cfg.tolerance + Duration::from_nanos(1), &cfg),
      VideoSyncAction::WaitUntil(cfg.tolerance + Duration::from_nanos(1))
    );
  }

  #[test]
  fn waits_for_early_frames() {
    let cfg = AvSyncConfig {
      tolerance: Duration::from_millis(5),
      max_late: Duration::from_millis(50),
      max_early: Duration::from_millis(30),
    };

    let master = Duration::from_secs(1);

    // Early beyond tolerance, but not "very early".
    let pts = master + Duration::from_millis(10);
    assert_eq!(
      decide(master, pts, &cfg),
      VideoSyncAction::WaitUntil(Duration::from_millis(10))
    );

    // Beyond max_early also waits (same WaitUntil value).
    let pts = master + cfg.max_early + Duration::from_nanos(1);
    assert_eq!(
      decide(master, pts, &cfg),
      VideoSyncAction::WaitUntil(cfg.max_early + Duration::from_nanos(1))
    );
  }

  #[test]
  fn does_not_panic_on_duration_overflow_edges() {
    let cfg = AvSyncConfig {
      tolerance: Duration::from_secs(10),
      max_late: Duration::from_secs(10),
      max_early: Duration::from_secs(10),
    };

    // Use a very large master time so `master + tolerance` would overflow if not saturating.
    let master = Duration::from_secs(u64::MAX);
    let pts = Duration::from_secs(u64::MAX);

    // Should not panic; exact action isn't critical here, but with saturated thresholds it should
    // be present-now.
    assert_eq!(decide(master, pts, &cfg), VideoSyncAction::PresentNow);
  }
}
