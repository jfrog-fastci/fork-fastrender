use std::time::{Duration, Instant};

use fastrender::media::av_sync::{suggest_wake_after, AvSyncConfig, AV_SYNC_WAKE_AFTER_MAX};
use fastrender::ui::repaint_scheduler::{plan_worker_wake_after, MIN_WORKER_WAKE_INTERVAL};

#[test]
fn media_wake_sequence_is_throttled_capped_and_cancelled() {
  // Small stateful scheduler that mimics the browser UI's media wake scheduling logic: it stores a
  // pending deadline and rate-limits immediate requests relative to the last wake.
  #[derive(Debug, Default)]
  struct FakeUiMediaWakeScheduler {
    last_wake: Option<Instant>,
    next_deadline: Option<Instant>,
  }

  impl FakeUiMediaWakeScheduler {
    fn request(&mut self, now: Instant, after: Duration) {
      let plan = plan_worker_wake_after(now, after, self.last_wake);
      self.next_deadline = if plan.wake_now { Some(now) } else { plan.next_deadline };
    }
  }

  let base = Instant::now();
  let mut scheduler = FakeUiMediaWakeScheduler::default();

  // -------------------------------------------------------------------------
  // 1) `after=0` is throttled by MIN_WORKER_WAKE_INTERVAL.
  // -------------------------------------------------------------------------
  // Simulate that we *just* woke at `base`, then receive an immediate re-wake request.
  scheduler.last_wake = Some(base);
  scheduler.request(base, Duration::ZERO);
  assert_eq!(
    scheduler.next_deadline,
    Some(base + MIN_WORKER_WAKE_INTERVAL),
    "expected immediate wake requests to be rate-limited"
  );

  // -------------------------------------------------------------------------
  // 2) Long sleeps are capped by the AvSync wake cap.
  // -------------------------------------------------------------------------
  let cfg = AvSyncConfig::default();
  let master_time = Duration::ZERO;
  let next_pts = Some(Duration::from_secs(5));
  let wake_after =
    suggest_wake_after(master_time, next_pts, &cfg).expect("expected wake suggestion");
  assert_eq!(
    wake_after, AV_SYNC_WAKE_AFTER_MAX,
    "expected extremely large waits to be capped"
  );

  scheduler.last_wake = None;
  scheduler.request(base, wake_after);
  assert_eq!(scheduler.next_deadline, Some(base + AV_SYNC_WAKE_AFTER_MAX));

  // -------------------------------------------------------------------------
  // 3) Cancel clears the scheduled wake.
  // -------------------------------------------------------------------------
  scheduler.request(base, Duration::MAX);
  assert_eq!(scheduler.next_deadline, None);
}

