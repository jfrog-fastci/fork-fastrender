use fastrender::ui::{ViewportThrottle, ViewportThrottleConfig, ViewportUpdate};
use std::time::{Duration, Instant};

fn min_interval_for(max_hz: u32) -> Duration {
  let max_hz = max_hz.max(1) as u64;
  let nanos_per_tick = (1_000_000_000u64 + max_hz - 1) / max_hz;
  Duration::from_nanos(nanos_per_tick.max(1))
}

#[test]
fn viewport_throttle_enforces_max_rate() {
  let config = ViewportThrottleConfig {
    max_hz: 100,
    debounce: Duration::from_millis(25),
  };
  let min_interval = min_interval_for(config.max_hz);
  let mut throttle = ViewportThrottle::with_config(config);

  let t0 = Instant::now();
  let mut emitted_at: Vec<Instant> = Vec::new();

  // Simulate a 1kHz resize stream for 50ms.
  for ms in 0..=50_u64 {
    let now = t0 + Duration::from_millis(ms);

    if let Some(_value) = throttle.poll(now) {
      emitted_at.push(now);
    }

    if let Some(_value) = throttle.push_desired(now, (ms as u32, 600), 1.0) {
      emitted_at.push(now);
    }
  }

  // Ensure we never emit more frequently than the throttle interval.
  assert!(
    !emitted_at.is_empty(),
    "expected at least one emitted update under a resize stream"
  );
  for window in emitted_at.windows(2) {
    let prev_at = window[0];
    let next_at = window[1];
    assert!(
      next_at.duration_since(prev_at) >= min_interval,
      "expected emissions to be spaced by >= {min_interval:?}, got {prev_at:?} then {next_at:?}"
    );
  }
}

#[test]
fn viewport_throttle_emits_final_update() {
  let config = ViewportThrottleConfig {
    max_hz: 100,
    debounce: Duration::from_millis(20),
  };
  let min_interval = min_interval_for(config.max_hz);
  let mut throttle = ViewportThrottle::with_config(config);
  let t0 = Instant::now();

  assert_eq!(
    throttle.push_desired(t0, (100, 100), 1.0),
    Some(ViewportUpdate::new((100, 100), 1.0)),
    "first update should be emitted immediately"
  );

  // Burst updates inside the interval: should be coalesced.
  assert_eq!(throttle.push_desired(t0 + Duration::from_millis(1), (200, 100), 1.0), None);
  assert_eq!(throttle.push_desired(t0 + Duration::from_millis(2), (300, 100), 1.0), None);

  let updated_at = t0 + Duration::from_millis(2);
  let expected_deadline = updated_at + config.debounce;
  assert!(
    expected_deadline >= t0 + min_interval,
    "test expects debounce to be the limiting factor (deadline should be >= rate-limit interval)"
  );
  assert_eq!(throttle.next_deadline(), Some(expected_deadline));
  assert_eq!(throttle.poll(expected_deadline - Duration::from_millis(1)), None);

  // Once due, the latest pending value should be emitted.
  assert_eq!(
    throttle.poll(expected_deadline),
    Some(ViewportUpdate::new((300, 100), 1.0))
  );
  assert!(throttle.next_deadline().is_none());
}

#[test]
fn viewport_throttle_deadline_respects_rate_limit() {
  let config = ViewportThrottleConfig {
    max_hz: 2,
    debounce: Duration::from_millis(20),
  };
  let min_interval = min_interval_for(config.max_hz);
  let mut throttle = ViewportThrottle::with_config(config);

  let t0 = Instant::now();
  assert_eq!(
    throttle.push_desired(t0, (100, 100), 1.0),
    Some(ViewportUpdate::new((100, 100), 1.0)),
    "first update should be emitted immediately"
  );

  let updated_at = t0 + Duration::from_millis(10);
  assert_eq!(throttle.push_desired(updated_at, (200, 100), 1.0), None);

  let debounce_deadline = updated_at + config.debounce;
  let expected_deadline = t0 + min_interval;
  assert!(
    expected_deadline > debounce_deadline,
    "test expects the rate-limit interval to be the limiting factor"
  );
  assert_eq!(throttle.next_deadline(), Some(expected_deadline));

  // Debounce has elapsed but the rate-limit window hasn't, so nothing is emitted.
  assert_eq!(throttle.poll(debounce_deadline), None);
  assert_eq!(throttle.poll(expected_deadline - Duration::from_millis(1)), None);

  // Once the rate-limit window elapses, the latest pending value should be emitted.
  assert_eq!(
    throttle.poll(expected_deadline),
    Some(ViewportUpdate::new((200, 100), 1.0))
  );
  assert!(throttle.next_deadline().is_none());
}
