use std::time::{Duration, Instant};

/// Minimum interval between egui-driven repaints when `repaint_after == Duration::ZERO`.
///
/// Egui can request "immediate" repaints for things like focus changes or animated widgets.
/// Requesting redraw continuously with no delay can turn into a busy loop on some platforms, so we
/// clamp to a 60fps-ish cadence by default.
pub const MIN_EGUI_REPAINT_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintSchedule {
  /// Whether the caller should request a redraw immediately (e.g. `window.request_redraw()`).
  pub request_redraw_now: bool,
  /// When to wake up next to request a redraw.
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
}
