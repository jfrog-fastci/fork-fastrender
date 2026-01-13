#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::av_sync::{
  decide, suggest_wake_after, AvSyncConfig, VideoSyncAction, AV_SYNC_WAKE_AFTER_MAX,
};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

const MAX_TIME_MS: u32 = 60_000;
const MAX_THRESHOLD_MS: u16 = 5_000;

#[derive(Arbitrary, Debug)]
struct MediaAvSyncInput {
  timeline_now_ms: u32,
  pts_ms: u32,
  tolerance_ms: u16,
  max_late_ms: u16,
  max_early_ms: u16,
}

fuzz_target!(|input: MediaAvSyncInput| {
  let timeline_now = Duration::from_millis((input.timeline_now_ms % MAX_TIME_MS) as u64);
  let pts = Duration::from_millis((input.pts_ms % MAX_TIME_MS) as u64);

  let cfg = AvSyncConfig {
    tolerance: Duration::from_millis((input.tolerance_ms % MAX_THRESHOLD_MS) as u64),
    max_late: Duration::from_millis((input.max_late_ms % MAX_THRESHOLD_MS) as u64),
    max_early: Duration::from_millis((input.max_early_ms % MAX_THRESHOLD_MS) as u64),
  };

  let action = decide(timeline_now, pts, &cfg);

  if let VideoSyncAction::WaitUntil(wait) = action {
    assert!(wait >= Duration::ZERO);
    assert!(wait != Duration::MAX);
  }

  if let Some(wake_after) = suggest_wake_after(timeline_now, Some(pts), &cfg) {
    assert!(wake_after >= Duration::ZERO);
    assert!(wake_after != Duration::MAX);
    assert!(wake_after <= AV_SYNC_WAKE_AFTER_MAX);
  }
});
