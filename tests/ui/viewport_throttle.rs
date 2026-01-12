use fastrender::ui::viewport_throttle::ViewportThrottle;
use std::time::Duration;

#[test]
fn viewport_throttle_enforces_max_rate() {
  let mut throttle = ViewportThrottle::new(Duration::from_millis(10));

  let mut emitted: Vec<Duration> = Vec::new();

  // Simulate a 1kHz resize stream for 50ms.
  for ms in 0..=50_u64 {
    let now = Duration::from_millis(ms);

    if let Some(_value) = throttle.poll(now) {
      emitted.push(now);
    }

    if let Some(_value) = throttle.push(now, ms) {
      emitted.push(now);
    }
  }

  // Ensure we never emit more frequently than the throttle interval.
  for window in emitted.windows(2) {
    let prev = window[0];
    let next = window[1];
    assert!(
      next >= prev + Duration::from_millis(10),
      "expected emissions to be spaced by >= 10ms, got {prev:?} then {next:?}"
    );
  }
}

#[test]
fn viewport_throttle_emits_final_update() {
  let mut throttle = ViewportThrottle::new(Duration::from_millis(10));

  assert_eq!(
    throttle.push(Duration::from_millis(0), 1),
    Some(1),
    "first update should be emitted immediately"
  );

  // Burst updates inside the interval: should be coalesced.
  assert_eq!(throttle.push(Duration::from_millis(1), 2), None);
  assert_eq!(throttle.push(Duration::from_millis(2), 3), None);

  // Not due yet.
  assert_eq!(throttle.poll(Duration::from_millis(9)), None);

  // Once due, the latest pending value should be emitted.
  assert_eq!(throttle.poll(Duration::from_millis(10)), Some(3));
  assert!(!throttle.has_pending(), "expected pending update to be cleared");
}

