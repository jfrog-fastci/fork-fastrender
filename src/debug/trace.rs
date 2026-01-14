use serde::ser::SerializeMap;
use serde::Serialize;
use std::borrow::Cow;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

#[derive(Clone)]
struct TraceArgs {
  entries: Vec<TraceArg>,
}

#[derive(Clone)]
struct TraceArg {
  key: &'static str,
  value: TraceValue,
}

#[derive(Clone)]
enum TraceValue {
  U64(u64),
  I64(i64),
  Bool(bool),
  String(String),
  StaticStr(&'static str),
}

impl Serialize for TraceValue {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match self {
      TraceValue::U64(value) => serializer.serialize_u64(*value),
      TraceValue::I64(value) => serializer.serialize_i64(*value),
      TraceValue::Bool(value) => serializer.serialize_bool(*value),
      TraceValue::String(value) => serializer.serialize_str(value),
      TraceValue::StaticStr(value) => serializer.serialize_str(value),
    }
  }
}

impl TraceArgs {
  fn with_capacity(capacity: usize) -> Self {
    let mut entries = Vec::new();
    // Use a fallible reserve so enabling tracing under memory pressure doesn't abort the process.
    // If the reservation fails, we still record best-effort events until the vector needs to grow.
    let _ = entries.try_reserve_exact(capacity);
    Self { entries }
  }

  fn new() -> Self {
    // Most events only record a handful of args; pre-allocate a small fixed size to avoid repeated
    // growth reallocations in hot paths (including audio callbacks).
    Self::with_capacity(8)
  }

  #[inline]
  fn insert(&mut self, key: &'static str, value: TraceValue) {
    // Preserve the previous `serde_json::Map` semantics: later inserts overwrite earlier values for
    // the same key (and avoid emitting duplicate keys in the serialized JSON object).
    for entry in &mut self.entries {
      if entry.key == key {
        entry.value = value;
        return;
      }
    }
    // Best-effort: if allocating space for the arg list fails, drop this arg instead of aborting.
    if self.entries.len() == self.entries.capacity() && self.entries.try_reserve(1).is_err() {
      return;
    }
    self.entries.push(TraceArg { key, value });
  }
}

impl Default for TraceArgs {
  fn default() -> Self {
    Self::new()
  }
}

impl Serialize for TraceArgs {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    let mut map = serializer.serialize_map(Some(self.entries.len()))?;
    for entry in &self.entries {
      map.serialize_entry(entry.key, &entry.value)?;
    }
    map.end()
  }
}

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

  pub fn try_span(&self, name: &'static str, cat: &'static str) -> Option<TraceSpan> {
    let state = self.inner.as_ref()?;
    if state.is_full() {
      state.dropped_events.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    Some(TraceSpan::new(state.clone(), Cow::Borrowed(name), cat))
  }

  pub fn span(&self, name: &'static str, cat: &'static str) -> TraceSpan {
    self.try_span(name, cat).unwrap_or_else(TraceSpan::noop)
  }

  pub fn try_span_owned(&self, name: String, cat: &'static str) -> Option<TraceSpan> {
    let state = self.inner.as_ref()?;
    if state.is_full() {
      state.dropped_events.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    Some(TraceSpan::new(state.clone(), Cow::Owned(name), cat))
  }

  pub fn span_owned(&self, name: String, cat: &'static str) -> TraceSpan {
    self
      .try_span_owned(name, cat)
      .unwrap_or_else(TraceSpan::noop)
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

    // Avoid holding the event mutex while serializing potentially large trace JSON. This lets hot
    // paths (including audio callbacks) keep recording events without blocking on file IO.
    let events_snapshot = {
      let events = match state.events.lock() {
        Ok(events) => events,
        Err(err) => err.into_inner(),
      };
      events.clone()
    };

    let mut file = std::fs::File::create(path)?;
    let trace_file = TraceFile {
      trace_events: &events_snapshot,
      fastrender_trace_max_events: max_events,
      fastrender_trace_dropped_events: dropped_events,
    };
    serde_json::to_writer(&mut file, &trace_file)?;
    file.write_all(b"\n")
  }
}

struct TraceState {
  start: Instant,
  pid: u32,
  max_events: usize,
  dropped_events: AtomicU64,
  event_count: AtomicU64,
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
      pid: std::process::id(),
      max_events,
      dropped_events: AtomicU64::new(0),
      event_count: AtomicU64::new(0),
      events: Mutex::new(events),
    }
  }

  #[inline]
  fn is_full(&self) -> bool {
    self.event_count.load(Ordering::Relaxed) >= self.max_events as u64
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
    // This is called from `TraceSpan::drop`, which can run in hot paths (including audio
    // callbacks). Never block on a contended lock; drop the event instead.
    let mut events = match self.events.try_lock() {
      Ok(events) => events,
      Err(std::sync::TryLockError::Poisoned(err)) => err.into_inner(),
      Err(std::sync::TryLockError::WouldBlock) => {
        self.dropped_events.fetch_add(1, Ordering::Relaxed);
        return;
      }
    };
    if events.len() >= self.max_events {
      self.dropped_events.fetch_add(1, Ordering::Relaxed);
      return;
    }
    // Best-effort: if we couldn't pre-reserve the full trace buffer (or memory is tight), avoid
    // aborting on vector growth. Drop the event if we can't allocate space.
    if events.len() == events.capacity() && events.try_reserve(1).is_err() {
      self.dropped_events.fetch_add(1, Ordering::Relaxed);
      return;
    }
    events.push(TraceEvent {
      name,
      cat,
      ph: "X",
      ts,
      dur,
      pid: self.pid,
      tid,
      args,
    });
    self.event_count.fetch_add(1, Ordering::Relaxed);
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
  pub fn ensure_args_capacity(&mut self, capacity: usize) {
    let Some(_state) = &self.state else {
      return;
    };
    if self.args.is_some() {
      return;
    }
    self.args = Some(TraceArgs::with_capacity(capacity));
  }

  #[inline]
  pub fn arg_u64(&mut self, key: &'static str, value: u64) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key, TraceValue::U64(value));
  }

  #[inline]
  pub fn arg_i64(&mut self, key: &'static str, value: i64) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key, TraceValue::I64(value));
  }

  #[inline]
  pub fn arg_bool(&mut self, key: &'static str, value: bool) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key, TraceValue::Bool(value));
  }

  #[inline]
  pub fn arg_str(&mut self, key: &'static str, value: &str) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key, TraceValue::String(cap_trace_string(value)));
  }

  #[inline]
  pub fn arg_static_str(&mut self, key: &'static str, value: &'static str) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key, TraceValue::StaticStr(value));
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
    let trace_events = value["traceEvents"].as_array().expect("traceEvents array");
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

  #[test]
  fn trace_span_static_str_arg_roundtrips() {
    let handle = TraceHandle::enabled_with_max_events(8);
    {
      let mut span = handle.span("test", "cat");
      span.arg_static_str("source", "Microtask");
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("trace.json");
    handle.write_chrome_trace(&path).expect("write trace");
    let json = std::fs::read_to_string(&path).expect("read trace");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse trace json");
    let trace_events = value["traceEvents"].as_array().expect("traceEvents array");
    let event = trace_events
      .iter()
      .find(|event| event["name"].as_str() == Some("test"))
      .expect("expected test span");
    assert_eq!(event["args"]["source"].as_str(), Some("Microtask"));
  }
}
