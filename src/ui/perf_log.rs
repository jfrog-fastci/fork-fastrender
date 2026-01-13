use crate::ui::messages::TabId;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENV_PERF_LOG: &str = "FASTR_PERF_LOG";
const PERF_LOG_MAX_EVENTS: usize = 10_000;

fn parse_env_bool(raw: Option<&str>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  match trimmed.to_ascii_lowercase().as_str() {
    "0" | "false" | "no" | "off" => false,
    _ => true,
  }
}

/// Return true if perf logging is enabled for this process.
pub fn perf_log_enabled() -> bool {
  // Do not cache the result in a `OnceLock`: integration tests temporarily mutate environment
  // variables via scoped guards, and caching would leak the first-read value across unrelated tests
  // in the same process.
  parse_env_bool(std::env::var(ENV_PERF_LOG).ok().as_deref())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PerfLogEvent {
  TabSwitch {
    from_tab: u64,
    to_tab: u64,
    cached: bool,
    latency_ms: u64,
  },
}

#[derive(Default)]
struct PerfLogState {
  events: Mutex<VecDeque<PerfLogEvent>>,
}

static PERF_LOG_STATE: OnceLock<PerfLogState> = OnceLock::new();

fn perf_log_state() -> Option<&'static PerfLogState> {
  if !perf_log_enabled() {
    return None;
  }
  Some(PERF_LOG_STATE.get_or_init(PerfLogState::default))
}

/// Emit a structured performance log event into a process-global buffer when perf logging is
/// enabled (`FASTR_PERF_LOG=1`).
pub fn emit_perf_log(event: PerfLogEvent) {
  let Some(state) = perf_log_state() else {
    return;
  };

  let mut events = state
    .events
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  if events.len() >= PERF_LOG_MAX_EVENTS {
    events.pop_front();
  }
  events.push_back(event);
}

/// Drain all buffered perf log events.
///
/// This is intended for integration tests and debug tooling.
pub fn drain_perf_log_events() -> Vec<PerfLogEvent> {
  let Some(state) = perf_log_state() else {
    return Vec::new();
  };
  let mut events = state
    .events
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  events.drain(..).collect()
}

/// Tracks end-to-end tab switch latency from "user initiated activation" to "first frame presented".
#[derive(Debug, Default)]
pub struct TabSwitchLatencyTracker {
  pending: Option<PendingTabSwitch>,
  last: Option<TabSwitchLatency>,
}

#[derive(Debug, Clone, Copy)]
struct PendingTabSwitch {
  from_tab: TabId,
  to_tab: TabId,
  cached: bool,
  start: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabSwitchLatency {
  pub from_tab: TabId,
  pub to_tab: TabId,
  pub cached: bool,
  pub latency: Duration,
}

impl TabSwitchLatency {
  pub fn latency_ms(&self) -> u64 {
    self.latency.as_millis().min(u128::from(u64::MAX)) as u64
  }
}

impl TabSwitchLatencyTracker {
  pub fn new() -> Self {
    Self::default()
  }

  /// Start measuring a tab switch.
  pub fn start(&mut self, from_tab: TabId, to_tab: TabId, cached: bool) {
    self.pending = Some(PendingTabSwitch {
      from_tab,
      to_tab,
      cached,
      start: Instant::now(),
    });
  }

  pub fn last(&self) -> Option<TabSwitchLatency> {
    self.last
  }

  /// Notify the tracker that `tab_id` has been presented (i.e. a frame is visible in the UI).
  ///
  /// When this matches the currently pending tab switch, the tracker computes latency, emits a
  /// `tab_switch` perf log event, stores the result, and returns it.
  pub fn mark_tab_presented(&mut self, tab_id: TabId) -> Option<TabSwitchLatency> {
    let pending = self.pending?;
    if pending.to_tab != tab_id {
      return None;
    }

    let latency = pending.start.elapsed();
    let result = TabSwitchLatency {
      from_tab: pending.from_tab,
      to_tab: pending.to_tab,
      cached: pending.cached,
      latency,
    };
    self.pending = None;
    self.last = Some(result);

    emit_perf_log(PerfLogEvent::TabSwitch {
      from_tab: pending.from_tab.0,
      to_tab: pending.to_tab.0,
      cached: pending.cached,
      latency_ms: result.latency_ms(),
    });

    Some(result)
  }
}
