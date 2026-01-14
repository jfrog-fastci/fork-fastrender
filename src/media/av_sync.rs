use crate::debug::runtime;
use std::time::Duration;

/// Environment variable override for [`AvSyncConfig::tolerance`].
pub const ENV_AV_SYNC_TOLERANCE_MS: &str = "FASTR_AV_SYNC_TOLERANCE_MS";
/// Environment variable override for [`AvSyncConfig::max_late`].
pub const ENV_AV_SYNC_MAX_LATE_MS: &str = "FASTR_AV_SYNC_MAX_LATE_MS";
/// Environment variable override for [`AvSyncConfig::max_early`].
pub const ENV_AV_SYNC_MAX_EARLY_MS: &str = "FASTR_AV_SYNC_MAX_EARLY_MS";

/// Environment variable override for [`AvSyncConfig::tolerance`] (preferred).
pub const ENV_FASTRENDER_AVSYNC_IN_SYNC_MS: &str = "FASTRENDER_AVSYNC_IN_SYNC_MS";
/// Environment variable override for [`AvSyncConfig::max_late`] (preferred).
pub const ENV_FASTRENDER_AVSYNC_DROP_LATE_MS: &str = "FASTRENDER_AVSYNC_DROP_LATE_MS";
/// Environment variable override for [`AvSyncConfig::max_early`] (preferred).
pub const ENV_FASTRENDER_AVSYNC_DELAY_EARLY_MS: &str = "FASTRENDER_AVSYNC_DELAY_EARLY_MS";

/// Default in-sync tolerance, in milliseconds.
pub const DEFAULT_AV_SYNC_TOLERANCE_MS: u64 = 20;
/// Default drop threshold (video late), in milliseconds.
pub const DEFAULT_AV_SYNC_MAX_LATE_MS: u64 = 80;
/// Default hold threshold (video early), in milliseconds.
pub const DEFAULT_AV_SYNC_MAX_EARLY_MS: u64 = 40;

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
  /// In-sync window: if `|frame_pts - master_time| <= tolerance`, treat the frame as in-sync and
  /// present it.
  pub tolerance: Duration,
  /// If a frame is this far behind the master clock, it is dropped.
  pub max_late: Duration,
  /// If a frame is this far ahead of the master clock, the scheduler should wait.
  pub max_early: Duration,
}

impl Default for AvSyncConfig {
  fn default() -> Self {
    Self {
      tolerance: Duration::from_millis(DEFAULT_AV_SYNC_TOLERANCE_MS),
      max_late: Duration::from_millis(DEFAULT_AV_SYNC_MAX_LATE_MS),
      max_early: Duration::from_millis(DEFAULT_AV_SYNC_MAX_EARLY_MS),
    }
  }
}

impl AvSyncConfig {
  /// Load A/V sync thresholds from environment variables, falling back to defaults.
  ///
  /// Invalid values are ignored (defaults remain in effect) and will emit a warning.
  pub fn from_env() -> Self {
    let toggles = runtime::runtime_toggles();
    let mut out = Self::default();
    out.tolerance = parse_env_duration_ms_or_default(
      toggles.get(ENV_AV_SYNC_TOLERANCE_MS),
      DEFAULT_AV_SYNC_TOLERANCE_MS,
      ENV_AV_SYNC_TOLERANCE_MS,
    );
    out.max_late = parse_env_duration_ms_or_default(
      toggles.get(ENV_AV_SYNC_MAX_LATE_MS),
      DEFAULT_AV_SYNC_MAX_LATE_MS,
      ENV_AV_SYNC_MAX_LATE_MS,
    );
    out.max_early = parse_env_duration_ms_or_default(
      toggles.get(ENV_AV_SYNC_MAX_EARLY_MS),
      DEFAULT_AV_SYNC_MAX_EARLY_MS,
      ENV_AV_SYNC_MAX_EARLY_MS,
    );

    // Newer env var names (preferred). We read these directly (instead of via `RuntimeToggles`)
    // because they intentionally do not use the `FASTR_` prefix.
    let in_sync_ms = std::env::var(ENV_FASTRENDER_AVSYNC_IN_SYNC_MS).ok();
    let drop_late_ms = std::env::var(ENV_FASTRENDER_AVSYNC_DROP_LATE_MS).ok();
    let delay_early_ms = std::env::var(ENV_FASTRENDER_AVSYNC_DELAY_EARLY_MS).ok();
    out.apply_fast_render_env_overrides(
      in_sync_ms.as_deref(),
      drop_late_ms.as_deref(),
      delay_early_ms.as_deref(),
    );

    out
  }

  fn from_fast_render_env_values(
    in_sync_ms: Option<&str>,
    drop_late_ms: Option<&str>,
    delay_early_ms: Option<&str>,
  ) -> Self {
    let mut out = Self::default();
    out.apply_fast_render_env_overrides(in_sync_ms, drop_late_ms, delay_early_ms);
    out
  }

  fn apply_fast_render_env_overrides(
    &mut self,
    in_sync_ms: Option<&str>,
    drop_late_ms: Option<&str>,
    delay_early_ms: Option<&str>,
  ) {
    // Important: these overrides are best-effort. Invalid values are ignored so callers can
    // experiment without breaking playback or requiring configuration resets.
    if let Ok(Some(ms)) = parse_env_ms(in_sync_ms) {
      self.tolerance = Duration::from_millis(ms);
    }
    if let Ok(Some(ms)) = parse_env_ms(drop_late_ms) {
      self.max_late = Duration::from_millis(ms);
    }
    if let Ok(Some(ms)) = parse_env_ms(delay_early_ms) {
      self.max_early = Duration::from_millis(ms);
    }
  }
}

/// Backwards-compatible A/V sync decision enum used by older call sites (e.g. the minimal
/// `MediaPlayer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvSyncDecision {
  /// Present this frame now.
  Present,
  /// Keep the previous frame and wake up after the provided duration to retry.
  Hold { wake_after: Duration },
  /// Drop this frame; it's too late and a newer one should be tried.
  Drop,
}

/// Backwards-compatible helper to decide how to handle a video frame with presentation timestamp
/// `pts` at the current master/timeline time `timeline_now`.
pub fn decide_video_frame(
  pts: Duration,
  timeline_now: Duration,
  cfg: &AvSyncConfig,
) -> AvSyncDecision {
  match decide(timeline_now, pts, cfg) {
    VideoSyncAction::PresentNow => AvSyncDecision::Present,
    VideoSyncAction::Drop => AvSyncDecision::Drop,
    VideoSyncAction::WaitUntil(wait) => AvSyncDecision::Hold {
      wake_after: clamp_wake_after(wait),
    },
  }
}

fn clamp_wake_after(wait: Duration) -> Duration {
  let wake_after = if wait <= Duration::from_millis(1) {
    Duration::ZERO
  } else {
    wait
  };
  wake_after.min(AV_SYNC_WAKE_AFTER_MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseEnvMsError {
  Invalid,
  Negative,
  TooLarge,
}

fn parse_env_ms(raw: Option<&str>) -> Result<Option<u64>, ParseEnvMsError> {
  let raw = match raw {
    Some(v) => v.trim(),
    None => return Ok(None),
  };
  if raw.is_empty() {
    return Ok(None);
  }

  let raw = raw.replace('_', "");
  let parsed = raw.parse::<i128>().map_err(|_| ParseEnvMsError::Invalid)?;
  if parsed < 0 {
    return Err(ParseEnvMsError::Negative);
  }
  let parsed: u64 = parsed.try_into().map_err(|_| ParseEnvMsError::TooLarge)?;
  Ok(Some(parsed))
}

fn parse_env_duration_ms_or_default(raw: Option<&str>, default_ms: u64, key: &str) -> Duration {
  match parse_env_ms(raw) {
    Ok(Some(ms)) => Duration::from_millis(ms),
    Ok(None) => Duration::from_millis(default_ms),
    Err(err) => {
      eprintln!(
        "warning: ignoring invalid {key}={:?} ({err:?}); using default {default_ms}ms",
        raw
      );
      Duration::from_millis(default_ms)
    }
  }
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
  if frame_pts >= master_time {
    // Early / on-time.
    let early_by = frame_pts.saturating_sub(master_time);
    if early_by <= cfg.tolerance {
      VideoSyncAction::PresentNow
    } else if early_by > cfg.max_early {
      VideoSyncAction::WaitUntil(early_by)
    } else {
      // Slightly early: present rather than holding to avoid excessive jitter/holding.
      VideoSyncAction::PresentNow
    }
  } else {
    // Late.
    let late_by = master_time.saturating_sub(frame_pts);
    if late_by <= cfg.tolerance {
      VideoSyncAction::PresentNow
    } else if late_by > cfg.max_late {
      VideoSyncAction::Drop
    } else {
      // Slightly late: present so we can catch up without dropping.
      VideoSyncAction::PresentNow
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime;
  use std::collections::HashMap;
  use std::sync::Arc;

  fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
  }

  #[test]
  fn av_sync_config_from_env_parses_overrides_and_allows_underscores() {
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
      (ENV_AV_SYNC_TOLERANCE_MS.to_string(), "1_234".to_string()),
      (ENV_AV_SYNC_MAX_LATE_MS.to_string(), "50".to_string()),
      (ENV_AV_SYNC_MAX_EARLY_MS.to_string(), "60".to_string()),
    ])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let cfg = AvSyncConfig::from_env();
      assert_eq!(cfg.tolerance, ms(1_234));
      assert_eq!(cfg.max_late, ms(50));
      assert_eq!(cfg.max_early, ms(60));
    });
  }

  #[test]
  fn av_sync_env_parses_valid_values() {
    let cfg = AvSyncConfig::from_fast_render_env_values(Some("5"), Some("100"), Some("1_250"));
    assert_eq!(cfg.tolerance, ms(5));
    assert_eq!(cfg.max_late, ms(100));
    assert_eq!(cfg.max_early, ms(1_250));
  }

  #[test]
  fn av_sync_env_ignores_invalid_values_and_keeps_defaults() {
    let default = AvSyncConfig::default();

    // Invalid values (non-numeric / negative / empty) should be ignored.
    let cfg = AvSyncConfig::from_fast_render_env_values(Some("nope"), Some("-10"), Some(""));
    assert_eq!(cfg, default);

    // Mixed values: apply the valid one; ignore the invalid ones.
    let cfg = AvSyncConfig::from_fast_render_env_values(Some("20"), Some("bad"), Some("-3"));
    assert_eq!(cfg.tolerance, ms(20));
    assert_eq!(cfg.max_late, default.max_late);
    assert_eq!(cfg.max_early, default.max_early);
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
    let wake = suggest_wake_after(now, Some(ms(5_000)), &cfg).expect("expected wake suggestion");
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

    // Just outside tolerance, but below max_early: present.
    assert_eq!(
      decide(
        master,
        master + cfg.tolerance + Duration::from_nanos(1),
        &cfg
      ),
      VideoSyncAction::PresentNow
    );

    // Exactly at max_early: still present (hold is strictly greater-than).
    assert_eq!(
      decide(master, master + cfg.max_early, &cfg),
      VideoSyncAction::PresentNow
    );

    // Beyond max_early: hold until the target PTS.
    assert_eq!(
      decide(
        master,
        master + cfg.max_early + Duration::from_nanos(1),
        &cfg
      ),
      VideoSyncAction::WaitUntil(cfg.max_early + Duration::from_nanos(1))
    );
  }

  #[test]
  fn holds_only_when_frame_is_very_early() {
    let cfg = AvSyncConfig {
      tolerance: Duration::from_millis(5),
      max_late: Duration::from_millis(50),
      max_early: Duration::from_millis(30),
    };

    let master = Duration::from_secs(1);

    // Early beyond tolerance, but not "very early".
    let pts = master + Duration::from_millis(10);
    assert_eq!(decide(master, pts, &cfg), VideoSyncAction::PresentNow);

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
