use serde::Serialize;
use std::borrow::Cow;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Map as JsonMap, Value as JsonValue};

const DEFAULT_MAX_TRACE_EVENTS: usize = 200_000;
const TRACE_MAX_EVENTS_ENV: &str = "FASTR_TRACE_MAX_EVENTS";

/// Maximum number of bytes captured for trace metadata strings.
///
/// Trace data is primarily for debugging and perf analysis, and may include URLs or other
/// attacker-controlled strings. We cap these so tracing cannot be used as an OOM vector.
const MAX_TRACE_METADATA_STRING_BYTES: usize = 1024;

fn cap_trace_string(value: &str) -> String {
  if value.len() <= MAX_TRACE_METADATA_STRING_BYTES {
    return value.to_string();
  }

  let mut end = MAX_TRACE_METADATA_STRING_BYTES;
  while end > 0 && !value.is_char_boundary(end) {
    end -= 1;
  }
  value[..end].to_string()
}

type TraceArgs = JsonMap<String, JsonValue>;

fn max_trace_events_from_env() -> Option<usize> {
  let raw = std::env::var_os(TRACE_MAX_EVENTS_ENV)?;
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  let parsed: u64 = trimmed.parse().ok()?;
  usize::try_from(parsed).ok()
}

static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
  // Assign a stable numeric ID per thread without a global map/mutex, so tracing from
  // real-time-ish contexts (e.g. audio callbacks) avoids additional lock contention.
  static TRACE_THREAD_ID: u64 = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
}

fn current_thread_numeric_id() -> u64 {
  TRACE_THREAD_ID.with(|id| *id)
}

#[derive(Clone, Default)]
pub struct TraceHandle {
  inner: Option<Arc<TraceState>>,
}

impl std::fmt::Debug for TraceHandle {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TraceHandle")
      .field("enabled", &self.is_enabled())
      .finish()
  }
}

impl TraceHandle {
  pub fn enabled() -> Self {
    let max_events = max_trace_events_from_env().unwrap_or(DEFAULT_MAX_TRACE_EVENTS);
    Self::enabled_with_max_events(max_events)
  }

  pub fn enabled_with_max_events(max_events: usize) -> Self {
    Self {
      inner: Some(Arc::new(TraceState::new(max_events))),
    }
  }

  pub fn disabled() -> Self {
    Self { inner: None }
  }

  pub fn is_enabled(&self) -> bool {
    self.inner.is_some()
  }

  pub fn span(&self, name: &'static str, cat: &'static str) -> TraceSpan {
    match &self.inner {
      Some(state) => TraceSpan::new(state.clone(), Cow::Borrowed(name), cat),
      None => TraceSpan::noop(),
    }
  }

  pub fn span_owned(&self, name: String, cat: &'static str) -> TraceSpan {
    match &self.inner {
      Some(state) => TraceSpan::new(state.clone(), Cow::Owned(name), cat),
      None => TraceSpan::noop(),
    }
  }

  pub fn write_chrome_trace(&self, path: &Path) -> std::io::Result<()> {
    let Some(state) = &self.inner else {
      return Ok(());
    };

    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
      }
    }

    let dropped_events = state.dropped_events.load(Ordering::Relaxed);
    let max_events = state.max_events as u64;
    let mut file = std::fs::File::create(path)?;
    let events = match state.events.lock() {
      Ok(events) => events,
      Err(err) => err.into_inner(),
    };
    let trace_file = TraceFile {
      trace_events: events.as_slice(),
      fastrender_trace_max_events: max_events,
      fastrender_trace_dropped_events: dropped_events,
    };
    serde_json::to_writer(&mut file, &trace_file)?;
    file.write_all(b"\n")
  }
}

struct TraceState {
  start: Instant,
  max_events: usize,
  dropped_events: AtomicU64,
  events: Mutex<Vec<TraceEvent>>,
}

impl TraceState {
  fn new(max_events: usize) -> Self {
    // Pre-allocate the event buffer up-front so recording spans in hot paths (including audio
    // callbacks) doesn't trigger vector growth allocations.
    //
    // Use `try_reserve_exact` so absurd `max_events` values (or memory pressure) don't abort the
    // process while merely enabling tracing.
    let mut events = Vec::new();
    let _ = events.try_reserve_exact(max_events);
    Self {
      start: Instant::now(),
      max_events,
      dropped_events: AtomicU64::new(0),
      events: Mutex::new(events),
    }
  }

  fn push_event(&self, name: Cow<'static, str>, cat: &'static str, start: Instant, end: Instant) {
    self.push_event_with_args(name, cat, start, end, None);
  }

  fn push_event_with_args(
    &self,
    name: Cow<'static, str>,
    cat: &'static str,
    start: Instant,
    end: Instant,
    args: Option<TraceArgs>,
  ) {
    let ts = start.saturating_duration_since(self.start).as_micros() as u64;
    let dur = end.saturating_duration_since(start).as_micros() as u64;
    let tid = current_thread_numeric_id();
    let mut events = match self.events.lock() {
      Ok(events) => events,
      Err(err) => err.into_inner(),
    };
    if events.len() >= self.max_events {
      self.dropped_events.fetch_add(1, Ordering::Relaxed);
      return;
    }
    events.push(TraceEvent {
      name,
      cat,
      ph: "X",
      ts,
      dur,
      pid: std::process::id(),
      tid,
      args,
    });
  }
}

pub struct TraceSpan {
  state: Option<Arc<TraceState>>,
  name: Cow<'static, str>,
  cat: &'static str,
  start: Option<Instant>,
  args: Option<TraceArgs>,
}

impl TraceSpan {
  fn new(state: Arc<TraceState>, name: Cow<'static, str>, cat: &'static str) -> Self {
    Self {
      state: Some(state),
      name,
      cat,
      start: Some(Instant::now()),
      args: None,
    }
  }

  fn noop() -> Self {
    Self {
      state: None,
      name: Cow::Borrowed(""),
      cat: "",
      start: None,
      args: None,
    }
  }

  #[inline]
  pub fn arg_u64(&mut self, key: &'static str, value: u64) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::Number(value.into()));
  }

  #[inline]
  pub fn arg_i64(&mut self, key: &'static str, value: i64) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::Number(value.into()));
  }

  #[inline]
  pub fn arg_bool(&mut self, key: &'static str, value: bool) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::Bool(value));
  }

  #[inline]
  pub fn arg_str(&mut self, key: &'static str, value: &str) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::String(cap_trace_string(value)));
  }
}

impl Drop for TraceSpan {
  fn drop(&mut self) {
    if let (Some(state), Some(start)) = (&self.state, self.start) {
      state.push_event_with_args(
        self.name.clone(),
        self.cat,
        start,
        Instant::now(),
        self.args.take(),
      );
    }
  }
}

#[derive(Serialize, Clone)]
struct TraceEvent {
  name: Cow<'static, str>,
  cat: &'static str,
  ph: &'static str,
  ts: u64,
  dur: u64,
  pid: u32,
  tid: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  args: Option<TraceArgs>,
}

#[derive(Serialize)]
struct TraceFile<'a> {
  #[serde(rename = "traceEvents")]
  trace_events: &'a [TraceEvent],
  #[serde(rename = "fastrenderTraceMaxEvents")]
  fastrender_trace_max_events: u64,
  #[serde(rename = "fastrenderTraceDroppedEvents")]
  fastrender_trace_dropped_events: u64,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trace_event_cap_drops_excess_and_writes_valid_json() {
    let max_events = 10;
    let generated_events = 25;
    let handle = TraceHandle::enabled_with_max_events(max_events);

    for _ in 0..generated_events {
      let _span = handle.span("test", "cat");
    }

    let state = handle.inner.as_ref().expect("trace enabled");
    let events = match state.events.lock() {
      Ok(events) => events,
      Err(err) => err.into_inner(),
    };
    assert_eq!(events.len(), max_events);
    drop(events);
    assert_eq!(
      state.dropped_events.load(Ordering::Relaxed),
      (generated_events - max_events) as u64
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("trace.json");
    handle.write_chrome_trace(&path).expect("write trace");

    let json = std::fs::read_to_string(&path).expect("read trace");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse trace json");
    let trace_events = value["traceEvents"]
      .as_array()
      .expect("traceEvents array");
    assert_eq!(trace_events.len(), max_events);
    assert_eq!(
      value["fastrenderTraceMaxEvents"]
        .as_u64()
        .expect("max events metadata"),
      max_events as u64
    );
    assert_eq!(
      value["fastrenderTraceDroppedEvents"]
        .as_u64()
        .expect("dropped events metadata"),
      (generated_events - max_events) as u64
    );
  }
}
