//! Video A/V sync decision helper.
//!
//! Given the current media timeline position (`timeline_now`) and a candidate decoded video frame
//! timestamp (`pts`), this module decides whether the renderer should:
//! - present the frame now,
//! - hold the previously presented frame and wake up later, or
//! - drop the frame because it is too late.
//!
//! This is intentionally small and deterministic so it can be used from both real playback code and
//! deterministic tests. The intended timing model is documented in `docs/media_clocking.md`.

use std::time::Duration;

/// Environment variable override for [`AvSyncConfig::in_sync_window`].
pub const ENV_AVSYNC_IN_SYNC_MS: &str = "FASTRENDER_AVSYNC_IN_SYNC_MS";
/// Environment variable override for [`AvSyncConfig::drop_late_threshold`].
pub const ENV_AVSYNC_DROP_LATE_MS: &str = "FASTRENDER_AVSYNC_DROP_LATE_MS";
/// Environment variable override for [`AvSyncConfig::delay_early_threshold`].
pub const ENV_AVSYNC_DELAY_EARLY_MS: &str = "FASTRENDER_AVSYNC_DELAY_EARLY_MS";

/// Default `AvSyncConfig.in_sync_window`, in milliseconds.
pub const DEFAULT_AV_SYNC_TOLERANCE_MS: u64 = 20;
/// Default `AvSyncConfig.drop_late_threshold`, in milliseconds.
pub const DEFAULT_AV_SYNC_MAX_LATE_MS: u64 = 80;
/// Default `AvSyncConfig.delay_early_threshold`, in milliseconds.
pub const DEFAULT_AV_SYNC_MAX_EARLY_MS: u64 = 40;

/// Default tolerances for video A/V sync decisions.
///
/// These defaults match the "Recommended default tolerances" section in `docs/media_clocking.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvSyncConfig {
  /// If `|video_pts - timeline_now| <= in_sync_window`, treat the frame as in-sync and present it.
  pub in_sync_window: Duration,
  /// If `timeline_now - video_pts > drop_late_threshold`, the frame is considered too late and
  /// should be dropped.
  pub drop_late_threshold: Duration,
  /// If `video_pts - timeline_now > delay_early_threshold`, the frame is considered too early and
  /// the renderer should keep the previous frame and wake up later.
  pub delay_early_threshold: Duration,
}

impl Default for AvSyncConfig {
  fn default() -> Self {
    Self {
      in_sync_window: Duration::from_millis(DEFAULT_AV_SYNC_TOLERANCE_MS),
      drop_late_threshold: Duration::from_millis(DEFAULT_AV_SYNC_MAX_LATE_MS),
      delay_early_threshold: Duration::from_millis(DEFAULT_AV_SYNC_MAX_EARLY_MS),
    }
  }
}

impl AvSyncConfig {
  /// Load A/V sync thresholds from the process environment.
  ///
  /// Environment variables are optional; invalid values are ignored and the defaults remain in
  /// effect.
  pub fn from_env() -> Self {
    Self::from_env_overrides(
      std::env::var(ENV_AVSYNC_IN_SYNC_MS).ok().as_deref(),
      std::env::var(ENV_AVSYNC_DROP_LATE_MS).ok().as_deref(),
      std::env::var(ENV_AVSYNC_DELAY_EARLY_MS).ok().as_deref(),
    )
  }

  fn from_env_overrides(
    in_sync_ms: Option<&str>,
    drop_late_ms: Option<&str>,
    delay_early_ms: Option<&str>,
  ) -> Self {
    let mut out = Self::default();

    if let Ok(Some(ms)) = parse_env_ms(in_sync_ms) {
      out.in_sync_window = Duration::from_millis(ms);
    }
    if let Ok(Some(ms)) = parse_env_ms(drop_late_ms) {
      out.drop_late_threshold = Duration::from_millis(ms);
    }
    if let Ok(Some(ms)) = parse_env_ms(delay_early_ms) {
      out.delay_early_threshold = Duration::from_millis(ms);
    }

    out
  }
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

/// What the video renderer should do with a candidate decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvSyncDecision {
  /// Present this frame now.
  Present,
  /// Keep the previous frame and wake up after the provided duration to retry.
  Hold { wake_after: Duration },
  /// Drop this frame; it's too late and a newer one should be tried.
  Drop,
}

/// Decide how to handle a video frame with presentation timestamp `pts` at the current media
/// timeline time `timeline_now`.
///
/// This function intentionally uses only `Duration`/integer math (no floats) and is panic-free even
/// for `Duration::ZERO` / `Duration::MAX` inputs.
pub fn decide_video_frame(pts: Duration, timeline_now: Duration, cfg: &AvSyncConfig) -> AvSyncDecision {
  if pts >= timeline_now {
    let early_by = pts.saturating_sub(timeline_now);
    if early_by <= cfg.in_sync_window {
      AvSyncDecision::Present
    } else if early_by > cfg.delay_early_threshold {
      // Wake up close to when this frame should be presented.
      AvSyncDecision::Hold { wake_after: early_by }
    } else {
      // Slightly early but below the delay threshold: present to avoid excessive holding/jitter.
      AvSyncDecision::Present
    }
  } else {
    let late_by = timeline_now.saturating_sub(pts);
    if late_by <= cfg.in_sync_window {
      AvSyncDecision::Present
    } else if late_by > cfg.drop_late_threshold {
      AvSyncDecision::Drop
    } else {
      // Slightly late but below the drop threshold: present so we can catch up without dropping.
      AvSyncDecision::Present
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
  }

  #[test]
  fn av_sync_present_within_in_sync_window() {
    let cfg = AvSyncConfig::default();
    let now = ms(1_000);

    assert_eq!(decide_video_frame(now, now, &cfg), AvSyncDecision::Present);

    // Early/late within ±20ms.
    assert_eq!(decide_video_frame(now + ms(19), now, &cfg), AvSyncDecision::Present);
    assert_eq!(decide_video_frame(now - ms(19), now, &cfg), AvSyncDecision::Present);

    // Exactly at the boundary.
    assert_eq!(decide_video_frame(now + ms(20), now, &cfg), AvSyncDecision::Present);
    assert_eq!(decide_video_frame(now - ms(20), now, &cfg), AvSyncDecision::Present);
  }

  #[test]
  fn av_sync_present_just_outside_in_sync_window_but_below_action_thresholds() {
    let cfg = AvSyncConfig::default();
    let now = ms(1_000);

    // Outside the 20ms in-sync window but not early enough to hold (<= 40ms).
    assert_eq!(decide_video_frame(now + ms(21), now, &cfg), AvSyncDecision::Present);
    assert_eq!(decide_video_frame(now + ms(40), now, &cfg), AvSyncDecision::Present);

    // Outside the 20ms in-sync window but not late enough to drop (<= 80ms).
    assert_eq!(decide_video_frame(now - ms(21), now, &cfg), AvSyncDecision::Present);
    assert_eq!(decide_video_frame(now - ms(80), now, &cfg), AvSyncDecision::Present);
  }

  #[test]
  fn av_sync_hold_when_frame_is_too_early() {
    let cfg = AvSyncConfig::default();
    let now = ms(1_000);

    // Just past the early-hold threshold (strictly greater than 40ms).
    assert_eq!(
      decide_video_frame(now + ms(41), now, &cfg),
      AvSyncDecision::Hold { wake_after: ms(41) }
    );

    // Typical "very early" frame.
    assert_eq!(
      decide_video_frame(ms(100), ms(0), &cfg),
      AvSyncDecision::Hold { wake_after: ms(100) }
    );
  }

  #[test]
  fn av_sync_drop_when_frame_is_too_late() {
    let cfg = AvSyncConfig::default();
    let now = ms(1_000);

    // Just past the late-drop threshold (strictly greater than 80ms).
    assert_eq!(decide_video_frame(now - ms(81), now, &cfg), AvSyncDecision::Drop);

    // Typical "very late" frame (including pts=0 edge case).
    assert_eq!(decide_video_frame(Duration::ZERO, ms(200), &cfg), AvSyncDecision::Drop);
  }

  #[test]
  fn av_sync_duration_extremes_do_not_panic() {
    let cfg = AvSyncConfig::default();

    // pts=0, now=0 should be fine.
    assert_eq!(
      decide_video_frame(Duration::ZERO, Duration::ZERO, &cfg),
      AvSyncDecision::Present
    );

    // Very large durations should not overflow/underflow.
    assert_eq!(
      decide_video_frame(Duration::MAX, Duration::ZERO, &cfg),
      AvSyncDecision::Hold {
        wake_after: Duration::MAX
      }
    );
  }

  #[test]
  fn parse_env_ms_allows_unset() {
    assert_eq!(parse_env_ms(None), Ok(None));
    assert_eq!(parse_env_ms(Some("")), Ok(None));
    assert_eq!(parse_env_ms(Some("   ")), Ok(None));
  }

  #[test]
  fn parse_env_ms_parses_u64_values() {
    assert_eq!(parse_env_ms(Some("0")), Ok(Some(0)));
    assert_eq!(parse_env_ms(Some("5")), Ok(Some(5)));
    assert_eq!(parse_env_ms(Some("+5")), Ok(Some(5)));
    assert_eq!(parse_env_ms(Some("1_000")), Ok(Some(1000)));
    assert_eq!(parse_env_ms(Some("  42  ")), Ok(Some(42)));
  }

  #[test]
  fn parse_env_ms_rejects_invalid_values() {
    assert_eq!(parse_env_ms(Some("nope")), Err(ParseEnvMsError::Invalid));
    assert_eq!(parse_env_ms(Some("12ms")), Err(ParseEnvMsError::Invalid));
    assert_eq!(parse_env_ms(Some("-1")), Err(ParseEnvMsError::Negative));
    assert_eq!(
      parse_env_ms(Some("18446744073709551616")),
      Err(ParseEnvMsError::TooLarge)
    );
  }

  #[test]
  fn av_sync_env_defaults_when_unset() {
    let cfg = AvSyncConfig::from_env_overrides(None, None, None);
    assert_eq!(cfg, AvSyncConfig::default());
  }

  #[test]
  fn av_sync_env_parses_valid_values() {
    let cfg = AvSyncConfig::from_env_overrides(Some("5"), Some("6"), Some("7"));
    assert_eq!(cfg.in_sync_window, Duration::from_millis(5));
    assert_eq!(cfg.drop_late_threshold, Duration::from_millis(6));
    assert_eq!(cfg.delay_early_threshold, Duration::from_millis(7));
  }

  #[test]
  fn av_sync_env_parses_whitespace_plus_and_underscores() {
    let cfg = AvSyncConfig::from_env_overrides(Some(" +1_000 "), Some("\t80"), Some("40\n"));
    assert_eq!(cfg.in_sync_window, Duration::from_millis(1000));
    assert_eq!(cfg.drop_late_threshold, Duration::from_millis(80));
    assert_eq!(cfg.delay_early_threshold, Duration::from_millis(40));
  }

  #[test]
  fn av_sync_env_ignores_invalid_values() {
    let default = AvSyncConfig::default();
    let cfg = AvSyncConfig::from_env_overrides(Some("nope"), Some("-1"), Some(""));
    assert_eq!(cfg.in_sync_window, default.in_sync_window);
    assert_eq!(cfg.drop_late_threshold, default.drop_late_threshold);
    assert_eq!(cfg.delay_early_threshold, default.delay_early_threshold);
  }

  #[test]
  fn av_sync_env_ignores_invalid_independently() {
    let default = AvSyncConfig::default();
    let cfg = AvSyncConfig::from_env_overrides(Some("21"), Some("broken"), Some("41"));
    assert_eq!(cfg.in_sync_window, Duration::from_millis(21));
    assert_eq!(cfg.drop_late_threshold, default.drop_late_threshold);
    assert_eq!(cfg.delay_early_threshold, Duration::from_millis(41));
  }
}
