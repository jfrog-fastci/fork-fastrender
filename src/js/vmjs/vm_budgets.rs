use crate::js::JsExecutionOptions;
use crate::render_control;
use std::time::{Duration, Instant};
use vm_js::Budget;

const DEFAULT_CHECK_TIME_EVERY: u32 = 100;

fn min_duration(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
  match (a, b) {
    (Some(a), Some(b)) => Some(if a < b { a } else { b }),
    (Some(a), None) => Some(a),
    (None, Some(b)) => Some(b),
    (None, None) => None,
  }
}

fn root_deadline_remaining_timeout() -> Option<Duration> {
  let Some(deadline) = render_control::root_deadline() else {
    return None;
  };

  match deadline.remaining_timeout() {
    Some(remaining) => Some(remaining),
    None => {
      // `remaining_timeout` returns `None` both when no timeout is configured and when the timeout
      // has elapsed. Preserve the "elapsed" case so we can force an immediate DeadlineExceeded.
      if deadline.timeout_limit().is_some() {
        Some(Duration::ZERO)
      } else {
        None
      }
    }
  }
}

/// Derive a `vm-js` per-run [`Budget`] for running attacker-controlled JavaScript.
///
/// This combines FastRender's host-level run limits with the root render deadline (if any) so a
/// single hostile script cannot hang the process.
pub fn vm_budget_for_js_run(opts: JsExecutionOptions) -> Budget {
  let deadline_duration = min_duration(
    opts.event_loop_run_limits.max_wall_time,
    root_deadline_remaining_timeout(),
  );

  let deadline = deadline_duration.and_then(|d| Instant::now().checked_add(d));

  // `vm-js` only checks wall time every N ticks. If the computed deadline is already expired (or
  // zero-duration), force it to check on the first `tick()` so we fail fast.
  let mut check_time_every = DEFAULT_CHECK_TIME_EVERY;
  if deadline.is_some_and(|d| Instant::now() >= d) {
    check_time_every = 1;
  }

  Budget {
    fuel: opts.max_instruction_count,
    deadline,
    check_time_every,
  }
}

