use std::time::Duration;

/// Maximum wake-up delay returned by [`suggest_wake_after`].
///
/// This intentionally caps the delay to a fairly small value so the UI/event-loop layer does not
/// sleep for an excessively long time (e.g. when PTS jumps far ahead due to buffering, seeking, or
/// timestamp discontinuities). Waking periodically ensures we can observe state changes like
/// pause/seek even when there is no other activity.
pub const AV_SYNC_WAKE_AFTER_MAX: Duration = Duration::from_millis(250);

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

/// Suggest when the UI/event-loop should wake up to try presenting the next video frame.
///
/// This helper is intended for **tickless media playback** scheduling: the worker/media pipeline can
/// provide a best-effort "wake me up in ~X" hint, and the UI thread can translate it into a system
/// timer (e.g. winit `ControlFlow::WaitUntil`).
///
/// This value is a **hint** only:
/// - Callers must always re-sample their authoritative clock on wake (audio device time when audio
///   is present; system monotonic time otherwise).
/// - The UI/event-loop layer may ignore, coalesce, rate-limit, or further clamp the returned delay.
///
/// Returns `None` when no wake-up is recommended (e.g. the next frame should be presented/dropped
/// immediately, or `next_frame_pts` is unknown). When a wake-up is returned, it is:
/// - clamped to `0` for extremely small values, and
/// - capped to [`AV_SYNC_WAKE_AFTER_MAX`] to avoid sleeping through state changes.
pub fn suggest_wake_after(
  master_time: Duration,
  next_frame_pts: Option<Duration>,
  cfg: &AvSyncConfig,
) -> Option<Duration> {
  let frame_pts = next_frame_pts?;
  match decide(master_time, frame_pts, cfg) {
    VideoSyncAction::WaitUntil(wait) => {
      // Clamp extremely small values to 0, then cap large values so we don't sleep for a long time
      // and miss state changes (pause/seek, buffering events, etc).
      let wake_after = if wait <= Duration::from_millis(1) {
        Duration::ZERO
      } else {
        wait
      };
      Some(wake_after.min(AV_SYNC_WAKE_AFTER_MAX))
    }
    VideoSyncAction::PresentNow | VideoSyncAction::Drop => None,
  }
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

  fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
  }

  #[test]
  fn av_sync_wake_early_frames_are_capped_to_reasonable_max() {
    let cfg = AvSyncConfig {
      tolerance: ms(20),
      max_late: ms(80),
      max_early: ms(40),
    };
    let now = ms(0);

    // Extremely early frame (e.g. due to discontinuity/buffering). The wake suggestion should be
    // capped so we still wake periodically and can observe state changes.
    let wake =
      suggest_wake_after(now, Some(ms(5_000)), &cfg).expect("expected wake suggestion");
    assert_eq!(wake, AV_SYNC_WAKE_AFTER_MAX);
  }

  #[test]
  fn av_sync_wake_early_frame_just_past_threshold_returns_small_wake_after() {
    let cfg = AvSyncConfig {
      tolerance: ms(20),
      max_late: ms(80),
      max_early: ms(40),
    };
    let now = ms(1_000);
    let pts = now + ms(41);

    assert_eq!(suggest_wake_after(now, Some(pts), &cfg), Some(ms(41)));
  }

  #[test]
  fn av_sync_wake_in_sync_frames_return_none() {
    let cfg = AvSyncConfig {
      tolerance: ms(20),
      max_late: ms(80),
      max_early: ms(40),
    };
    let now = ms(1_000);

    assert_eq!(suggest_wake_after(now, Some(now), &cfg), None);
    assert_eq!(suggest_wake_after(now, Some(now + ms(10)), &cfg), None);
  }

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
