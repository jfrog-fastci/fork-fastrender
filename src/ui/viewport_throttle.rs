use std::time::{Duration, Instant};

/// Configuration knobs for [`ViewportThrottle`].
#[derive(Debug, Clone, Copy)]
pub struct ViewportThrottleConfig {
  /// Maximum number of `ViewportChanged` updates allowed per second while the viewport is changing.
  pub max_hz: u32,
  /// Debounce window used to detect the end of a resize/viewport-change burst.
  ///
  /// When no new viewport value arrives for `debounce`, the throttle will emit the latest pending
  /// viewport.
  pub debounce: Duration,
}

impl Default for ViewportThrottleConfig {
  fn default() -> Self {
    Self {
      // 30Hz keeps the worker fed without spamming it during window drags.
      max_hz: 30,
      // ~50-100ms feels responsive while still avoiding "render every intermediate pixel" during
      // resize.
      debounce: Duration::from_millis(80),
    }
  }
}

/// A `(viewport_css, dpr)` pair suitable for emission to the render worker.
///
/// We store the DPR as raw bits so the type can be `Eq` and used in deterministic unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportUpdate {
  pub viewport_css: (u32, u32),
  dpr_bits: u32,
}

impl ViewportUpdate {
  pub fn new(viewport_css: (u32, u32), dpr: f32) -> Self {
    Self {
      viewport_css,
      dpr_bits: dpr.to_bits(),
    }
  }

  pub fn dpr(self) -> f32 {
    f32::from_bits(self.dpr_bits)
  }
}

/// A small time-based coalescer for viewport updates.
///
/// Callers feed it successive desired viewport values and then emit at most `max_hz` updates per
/// second, with a final debounce emission shortly after the viewport settles.
#[derive(Debug)]
pub struct ViewportThrottle {
  config: ViewportThrottleConfig,
  min_interval: Duration,

  last_sent: Option<ViewportUpdate>,
  last_sent_at: Option<Instant>,

  pending: Option<ViewportUpdate>,
  pending_updated_at: Option<Instant>,
}

impl Default for ViewportThrottle {
  fn default() -> Self {
    Self::new()
  }
}

impl ViewportThrottle {
  pub fn new() -> Self {
    Self::with_config(ViewportThrottleConfig::default())
  }

  pub fn with_config(config: ViewportThrottleConfig) -> Self {
    let max_hz = config.max_hz.max(1) as u64;
    // Use ceiling division so we never exceed `max_hz` due to rounding.
    let nanos_per_tick = (1_000_000_000u64 + max_hz - 1) / max_hz;
    let min_interval = Duration::from_nanos(nanos_per_tick.max(1));

    Self {
      config,
      min_interval,
      last_sent: None,
      last_sent_at: None,
      pending: None,
      pending_updated_at: None,
    }
  }

  pub fn config(&self) -> ViewportThrottleConfig {
    self.config
  }

  /// Drop all internal state so the next viewport is emitted immediately.
  pub fn reset(&mut self) {
    self.last_sent = None;
    self.last_sent_at = None;
    self.pending = None;
    self.pending_updated_at = None;
  }

  /// Feed a new desired viewport value.
  ///
  /// Returns `Some(update)` when the caller should emit an immediate `ViewportChanged`.
  pub fn push_desired(
    &mut self,
    now: Instant,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> Option<ViewportUpdate> {
    self.push_desired_update(now, ViewportUpdate::new(viewport_css, dpr))
  }

  fn push_desired_update(&mut self, now: Instant, desired: ViewportUpdate) -> Option<ViewportUpdate> {
    // If the viewport is back to the last-sent value, clear any pending update.
    if self.last_sent == Some(desired) {
      self.pending = None;
      self.pending_updated_at = None;
      return None;
    }

    self.pending = Some(desired);
    self.pending_updated_at = Some(now);

    // First emission is immediate.
    let Some(last_sent_at) = self.last_sent_at else {
      return self.emit_pending(now);
    };

    // During a continuous resize, rate-limit emissions to `max_hz`.
    if now.duration_since(last_sent_at) >= self.min_interval {
      return self.emit_pending(now);
    }

    None
  }

  /// Poll for a pending debounced emission.
  ///
  /// Callers should call this periodically (e.g. when a `ControlFlow::WaitUntil` deadline fires).
  pub fn poll(&mut self, now: Instant) -> Option<ViewportUpdate> {
    let Some(_) = self.pending else {
      return None;
    };
    let updated_at = self.pending_updated_at?;

    if now < updated_at + self.config.debounce {
      return None;
    }

    if let Some(last_sent_at) = self.last_sent_at {
      if now.duration_since(last_sent_at) < self.min_interval {
        return None;
      }
    }

    self.emit_pending(now)
  }

  /// Force an immediate emission of the latest pending viewport value (if any).
  pub fn force_send_now(&mut self, now: Instant) -> Option<ViewportUpdate> {
    if self.pending.is_none() {
      return None;
    }
    self.emit_pending(now)
  }

  /// Return the next timestamp at which [`Self::poll`] might emit.
  pub fn next_deadline(&self) -> Option<Instant> {
    let updated_at = self.pending_updated_at?;
    let mut deadline = updated_at + self.config.debounce;

    if let Some(last_sent_at) = self.last_sent_at {
      let rate_limit_deadline = last_sent_at + self.min_interval;
      if rate_limit_deadline > deadline {
        deadline = rate_limit_deadline;
      }
    }

    Some(deadline)
  }

  fn emit_pending(&mut self, now: Instant) -> Option<ViewportUpdate> {
    let update = self.pending.take()?;
    self.pending_updated_at = None;
    self.last_sent = Some(update);
    self.last_sent_at = Some(now);
    Some(update)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cfg_for_tests() -> ViewportThrottleConfig {
    ViewportThrottleConfig {
      max_hz: 10,
      debounce: Duration::from_millis(200),
    }
  }

  fn min_interval_for(max_hz: u32) -> Duration {
    let max_hz = max_hz.max(1) as u64;
    let nanos_per_tick = (1_000_000_000u64 + max_hz - 1) / max_hz;
    Duration::from_nanos(nanos_per_tick.max(1))
  }

  #[test]
  fn emits_first_update_immediately() {
    let mut throttle = ViewportThrottle::with_config(cfg_for_tests());
    let t0 = Instant::now();

    let out = throttle.push_desired(t0, (800, 600), 2.0);
    assert_eq!(out, Some(ViewportUpdate::new((800, 600), 2.0)));
    assert!(throttle.next_deadline().is_none());
  }

  #[test]
  fn rate_limits_intermediate_updates() {
    let mut throttle = ViewportThrottle::with_config(ViewportThrottleConfig {
      max_hz: 2,
      debounce: Duration::from_millis(600),
    });
    let t0 = Instant::now();

    assert_eq!(
      throttle.push_desired(t0, (100, 100), 1.0),
      Some(ViewportUpdate::new((100, 100), 1.0))
    );

    // Within the 500ms interval, nothing should be emitted.
    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(100), (101, 100), 1.0), None);
    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(200), (102, 100), 1.0), None);

    // Once the interval elapses, the next update emits immediately.
    assert_eq!(
      throttle.push_desired(t0 + Duration::from_millis(500), (103, 100), 1.0),
      Some(ViewportUpdate::new((103, 100), 1.0))
    );
  }

  #[test]
  fn emits_final_update_after_debounce() {
    let mut throttle = ViewportThrottle::with_config(cfg_for_tests());
    let t0 = Instant::now();

    assert_eq!(
      throttle.push_desired(t0, (100, 100), 1.0),
      Some(ViewportUpdate::new((100, 100), 1.0))
    );

    // New desired value arrives within the rate-limit interval: it's queued.
    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(10), (200, 100), 1.0), None);

    let expected_deadline = t0 + Duration::from_millis(10) + cfg_for_tests().debounce;
    assert_eq!(throttle.next_deadline(), Some(expected_deadline));

    // Before the debounce window: nothing.
    assert_eq!(throttle.poll(expected_deadline - Duration::from_millis(1)), None);
    // After the debounce window: emit the final value.
    assert_eq!(
      throttle.poll(expected_deadline),
      Some(ViewportUpdate::new((200, 100), 1.0))
    );
    assert!(throttle.next_deadline().is_none());
  }

  #[test]
  fn handles_rapid_oscillation_deterministically() {
    let mut throttle = ViewportThrottle::with_config(cfg_for_tests());
    let t0 = Instant::now();

    assert_eq!(
      throttle.push_desired(t0, (100, 100), 1.0),
      Some(ViewportUpdate::new((100, 100), 1.0))
    );

    // Oscillate values rapidly inside the rate limit; only the *last* desired should win.
    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(10), (120, 100), 1.0), None);
    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(20), (140, 100), 1.0), None);
    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(30), (120, 100), 1.0), None);

    let deadline = t0 + Duration::from_millis(30) + cfg_for_tests().debounce;
    assert_eq!(throttle.poll(deadline), Some(ViewportUpdate::new((120, 100), 1.0)));
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
}
