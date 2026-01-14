use std::time::{Duration, Instant};

/// Minimum interval between egui-driven repaints when `repaint_after == Duration::ZERO`.
///
/// Egui can request "immediate" repaints for things like focus changes or animated widgets.
/// Requesting redraw continuously with no delay can turn into a busy loop on some platforms, so we
/// clamp to a 60fps-ish cadence by default.
pub const MIN_EGUI_REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// Minimum interval between worker-driven wakeups when `after == Duration::ZERO`.
///
/// Render workers may request "immediate" wakeups for time-based updates like video frame
/// presentation. Scheduling zero-delay wakeups in a tight loop can turn into a busy loop on some
/// platforms, so we clamp to a small minimum interval.
pub const MIN_WORKER_WAKE_INTERVAL: Duration = Duration::from_millis(4);

/// Minimum interval between worker-driven redraw requests (e.g. `WorkerToUi::FrameReady` bursts).
///
/// The render worker can emit multiple stage updates / frames in rapid succession (notably during
/// resize). Requesting a redraw for every message can turn into a busy loop, so we coalesce to a
/// ~60fps cadence by default.
pub const MIN_WORKER_REDRAW_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintSchedule {
  /// Whether the caller should request a redraw immediately (e.g. `window.request_redraw()`).
  pub request_redraw_now: bool,
  /// When to wake up next to request a redraw.
  pub next_deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeSchedule {
  /// Whether the caller should wake/run immediately.
  pub wake_now: bool,
  /// When to wake up next (if not waking immediately).
  pub next_deadline: Option<Instant>,
}

/// Plan the next repaint request based on egui's [`egui::FullOutput::repaint_after`] value.
///
/// - `repaint_after == Duration::ZERO` means "as soon as possible".
/// - `repaint_after == Duration::MAX` is treated as "no repaint requested".
///
/// The returned plan is rate-limited by `min_interval` relative to the last time we requested an
/// egui-driven redraw (`last_redraw_request`), preventing 100% CPU busy loops when egui keeps
/// requesting immediate repaints.
pub fn plan_egui_repaint_with_min_interval(
  now: Instant,
  repaint_after: Duration,
  last_redraw_request: Option<Instant>,
  min_interval: Duration,
) -> RepaintSchedule {
  if repaint_after == Duration::MAX {
    return RepaintSchedule {
      request_redraw_now: false,
      next_deadline: None,
    };
  }

  let desired_deadline = if repaint_after == Duration::ZERO {
    Some(now)
  } else {
    now.checked_add(repaint_after)
  };

  let Some(desired_deadline) = desired_deadline else {
    return RepaintSchedule {
      request_redraw_now: false,
      next_deadline: None,
    };
  };

  let earliest_allowed = last_redraw_request
    .and_then(|last| last.checked_add(min_interval))
    .unwrap_or(now);

  let effective_deadline = desired_deadline.max(earliest_allowed);

  if effective_deadline <= now {
    RepaintSchedule {
      request_redraw_now: true,
      next_deadline: None,
    }
  } else {
    RepaintSchedule {
      request_redraw_now: false,
      next_deadline: Some(effective_deadline),
    }
  }
}

pub fn plan_egui_repaint(
  now: Instant,
  repaint_after: Duration,
  last_redraw_request: Option<Instant>,
) -> RepaintSchedule {
  plan_egui_repaint_with_min_interval(
    now,
    repaint_after,
    last_redraw_request,
    MIN_EGUI_REPAINT_INTERVAL,
  )
}

/// Plan a worker-driven redraw request ("as soon as possible"), rate-limited relative to the last
/// worker-driven request.
///
/// The returned `next_deadline` is coalesced with any `existing_deadline` by keeping the earliest
/// one. This ensures bursts of worker messages don't keep pushing the wakeup time forward.
pub fn plan_worker_redraw_with_min_interval(
  now: Instant,
  last_redraw_request: Option<Instant>,
  existing_deadline: Option<Instant>,
  min_interval: Duration,
) -> RepaintSchedule {
  // If we already had a pending wakeup and it's due, request immediately so the caller can clear it
  // without waiting for a separate timer event.
  if existing_deadline.is_some_and(|deadline| deadline <= now) {
    return RepaintSchedule {
      request_redraw_now: true,
      next_deadline: None,
    };
  }

  let plan =
    plan_egui_repaint_with_min_interval(now, Duration::ZERO, last_redraw_request, min_interval);
  if plan.request_redraw_now {
    return plan;
  }

  let next_deadline = earliest_deadline(existing_deadline, plan.next_deadline);
  if next_deadline.is_some_and(|deadline| deadline <= now) {
    RepaintSchedule {
      request_redraw_now: true,
      next_deadline: None,
    }
  } else {
    RepaintSchedule {
      request_redraw_now: false,
      next_deadline,
    }
  }
}

pub fn plan_worker_redraw(
  now: Instant,
  last_redraw_request: Option<Instant>,
  existing_deadline: Option<Instant>,
) -> RepaintSchedule {
  plan_worker_redraw_with_min_interval(
    now,
    last_redraw_request,
    existing_deadline,
    MIN_WORKER_REDRAW_INTERVAL,
  )
}

/// Plan the next wakeup request based on a worker-provided `after` duration.
///
/// - `after == Duration::ZERO` means "as soon as possible".
/// - `after == Duration::MAX` is treated as "no wakeup requested".
///
/// The returned plan is rate-limited by `min_interval` relative to the last time we woke due to a
/// worker request (`last_wake`), preventing tight-loop busy waits when a buggy worker keeps
/// requesting immediate wakeups.
pub fn plan_worker_wake_after_with_min_interval(
  now: Instant,
  after: Duration,
  last_wake: Option<Instant>,
  min_interval: Duration,
) -> WakeSchedule {
  if after == Duration::MAX {
    return WakeSchedule {
      wake_now: false,
      next_deadline: None,
    };
  }

  let desired_deadline = if after == Duration::ZERO {
    Some(now)
  } else {
    now.checked_add(after)
  };

  let Some(desired_deadline) = desired_deadline else {
    return WakeSchedule {
      wake_now: false,
      next_deadline: None,
    };
  };

  let earliest_allowed = last_wake
    .and_then(|last| last.checked_add(min_interval))
    .unwrap_or(now);

  let effective_deadline = desired_deadline.max(earliest_allowed);

  if effective_deadline <= now {
    WakeSchedule {
      wake_now: true,
      next_deadline: None,
    }
  } else {
    WakeSchedule {
      wake_now: false,
      next_deadline: Some(effective_deadline),
    }
  }
}

pub fn plan_worker_wake_after(now: Instant, after: Duration, last_wake: Option<Instant>) -> WakeSchedule {
  plan_worker_wake_after_with_min_interval(now, after, last_wake, MIN_WORKER_WAKE_INTERVAL)
}

/// Plan the next [`crate::ui::UiToWorker::Tick`] delivery for a tab based on a worker-provided
/// `next_tick` hint.
///
/// - `next_tick == None` means "no tick needed".
/// - `next_tick == Some(Duration::ZERO)` means "as soon as possible".
///
/// The returned plan is rate-limited by `min_interval` relative to the last time we delivered a
/// tick (`last_tick`) to prevent busy-loop tick scheduling when `next_tick == Some(Duration::ZERO)`
/// repeatedly.
pub fn plan_next_tick_with_min_interval(
  now: Instant,
  next_tick: Option<Duration>,
  last_tick: Option<Instant>,
  min_interval: Duration,
) -> WakeSchedule {
  match next_tick {
    Some(after) => plan_worker_wake_after_with_min_interval(now, after, last_tick, min_interval),
    None => WakeSchedule {
      wake_now: false,
      next_deadline: None,
    },
  }
}

/// Like [`plan_next_tick_with_min_interval`], using [`MIN_WORKER_WAKE_INTERVAL`] as the default
/// minimum interval.
pub fn plan_next_tick(now: Instant, next_tick: Option<Duration>, last_tick: Option<Instant>) -> WakeSchedule {
  plan_next_tick_with_min_interval(now, next_tick, last_tick, MIN_WORKER_WAKE_INTERVAL)
}

pub fn earliest_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
  match (a, b) {
    (Some(a), Some(b)) => Some(a.min(b)),
    (Some(a), None) => Some(a),
    (None, Some(b)) => Some(b),
    (None, None) => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn immediate_repaint_requests_redraw_when_not_rate_limited() {
    let now = Instant::now();
    let plan =
      plan_egui_repaint_with_min_interval(now, Duration::ZERO, None, Duration::from_millis(16));
    assert!(plan.request_redraw_now);
    assert_eq!(plan.next_deadline, None);
  }

  #[test]
  fn immediate_repaint_is_throttled_when_too_soon() {
    let now = Instant::now();
    let min = Duration::from_millis(16);
    let plan = plan_egui_repaint_with_min_interval(now, Duration::ZERO, Some(now), min);
    assert!(!plan.request_redraw_now);
    assert_eq!(plan.next_deadline, Some(now + min));
  }

  #[test]
  fn earliest_deadline_picks_the_soonest() {
    let now = Instant::now();
    let animation = Some(now + Duration::from_millis(30));
    let egui = Some(now + Duration::from_millis(10));
    assert_eq!(earliest_deadline(animation, egui), egui);
  }

  #[test]
  fn earliest_deadline_handles_none() {
    let now = Instant::now();
    let deadline = Some(now + Duration::from_millis(5));
    assert_eq!(earliest_deadline(None, None), None);
    assert_eq!(earliest_deadline(deadline, None), deadline);
    assert_eq!(earliest_deadline(None, deadline), deadline);
  }

  #[test]
  fn duration_max_means_no_repaint() {
    let now = Instant::now();
    let plan = plan_egui_repaint_with_min_interval(now, Duration::MAX, None, Duration::from_millis(16));
    assert!(!plan.request_redraw_now);
    assert_eq!(plan.next_deadline, None);
  }

  #[test]
  fn non_zero_repaint_after_schedules_deadline() {
    let now = Instant::now();
    let delay = Duration::from_millis(25);
    let plan = plan_egui_repaint_with_min_interval(now, delay, None, Duration::from_millis(16));
    assert!(!plan.request_redraw_now);
    assert_eq!(plan.next_deadline, Some(now + delay));
  }

  #[test]
  fn worker_redraw_is_throttled_and_does_not_push_out_existing_deadline() {
    let base = Instant::now();
    let min = Duration::from_millis(16);

    // We already requested a worker redraw at `base`, so the earliest allowed next request is
    // `base + min`. Assume we've already scheduled a wakeup there.
    let existing_deadline = Some(base + min);
    let now = base + Duration::from_millis(1);

    let plan = plan_worker_redraw_with_min_interval(now, Some(base), existing_deadline, min);
    assert!(!plan.request_redraw_now);
    assert_eq!(plan.next_deadline, existing_deadline);
  }

  #[test]
  fn worker_redraw_requests_immediately_when_existing_deadline_is_due() {
    let base = Instant::now();
    let min = Duration::from_millis(16);
    let existing_deadline = Some(base - Duration::from_millis(1));

    let plan =
      plan_worker_redraw_with_min_interval(base, Some(base - min), existing_deadline, min);
    assert!(plan.request_redraw_now);
    assert_eq!(plan.next_deadline, None);
  }

  #[test]
  fn worker_wake_immediate_is_rate_limited() {
    let now = Instant::now();
    let min = Duration::from_millis(4);
    let plan = plan_worker_wake_after_with_min_interval(now, Duration::ZERO, Some(now), min);
    assert!(!plan.wake_now);
    assert_eq!(plan.next_deadline, Some(now + min));
  }

  #[test]
  fn worker_wake_duration_max_means_no_wake() {
    let now = Instant::now();
    let plan = plan_worker_wake_after_with_min_interval(now, Duration::MAX, None, Duration::from_millis(4));
    assert!(!plan.wake_now);
    assert_eq!(plan.next_deadline, None);
  }

  #[test]
  fn next_tick_none_means_no_wake() {
    let now = Instant::now();
    let min = Duration::from_millis(4);
    let plan = plan_next_tick_with_min_interval(now, None, None, min);
    assert!(!plan.wake_now);
    assert_eq!(plan.next_deadline, None);
  }

  #[test]
  fn next_tick_zero_wakes_immediately_when_not_rate_limited() {
    let now = Instant::now();
    let min = Duration::from_millis(4);
    let plan = plan_next_tick_with_min_interval(now, Some(Duration::ZERO), None, min);
    assert!(plan.wake_now);
    assert_eq!(plan.next_deadline, None);
  }

  #[test]
  fn next_tick_zero_is_rate_limited_when_too_soon() {
    let now = Instant::now();
    let min = Duration::from_millis(4);
    let plan = plan_next_tick_with_min_interval(now, Some(Duration::ZERO), Some(now), min);
    assert!(!plan.wake_now);
    assert_eq!(plan.next_deadline, Some(now + min));
  }

  #[test]
  fn next_tick_respects_min_interval_even_when_delay_is_smaller() {
    let now = Instant::now();
    let min = Duration::from_millis(4);
    let plan = plan_next_tick_with_min_interval(now, Some(Duration::from_millis(1)), Some(now), min);
    assert!(!plan.wake_now);
    assert_eq!(plan.next_deadline, Some(now + min));
  }

  #[test]
  fn worker_wake_overflow_is_treated_as_no_wake() {
    let now = Instant::now();
    // Extremely large duration that should overflow `Instant::checked_add` on all practical
    // platforms.
    let huge = Duration::from_secs(u64::MAX);
    let plan = plan_worker_wake_after_with_min_interval(now, huge, None, Duration::from_millis(4));
    assert!(!plan.wake_now);
    assert_eq!(plan.next_deadline, None);
  }
}
