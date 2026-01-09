use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde_json::{Map as JsonMap, Value as JsonValue};

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

fn current_thread_numeric_id() -> u64 {
  static NEXT_ID: AtomicU64 = AtomicU64::new(1);
  static IDS: OnceLock<Mutex<HashMap<std::thread::ThreadId, u64>>> = OnceLock::new();

  let thread_id = std::thread::current().id();
  let ids = IDS.get_or_init(|| Mutex::new(HashMap::new()));
  let mut ids = match ids.lock() {
    Ok(guard) => guard,
    Err(err) => err.into_inner(),
  };
  *ids
    .entry(thread_id)
    .or_insert_with(|| NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone, Default)]
pub(crate) struct TraceHandle {
  inner: Option<Arc<TraceState>>,
}

impl TraceHandle {
  pub(crate) fn enabled() -> Self {
    Self {
      inner: Some(Arc::new(TraceState::new())),
    }
  }

  pub(crate) fn disabled() -> Self {
    Self { inner: None }
  }

  pub(crate) fn is_enabled(&self) -> bool {
    self.inner.is_some()
  }

  pub(crate) fn span(&self, name: &'static str, cat: &'static str) -> TraceSpan {
    match &self.inner {
      Some(state) => TraceSpan::new(state.clone(), Cow::Borrowed(name), cat),
      None => TraceSpan::noop(),
    }
  }

  pub(crate) fn span_owned(&self, name: String, cat: &'static str) -> TraceSpan {
    match &self.inner {
      Some(state) => TraceSpan::new(state.clone(), Cow::Owned(name), cat),
      None => TraceSpan::noop(),
    }
  }

  pub(crate) fn write_chrome_trace(&self, path: &Path) -> std::io::Result<()> {
    let Some(state) = &self.inner else {
      return Ok(());
    };

    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
      }
    }

    let events = match state.events.lock() {
      Ok(events) => events.clone(),
      Err(err) => err.into_inner().clone(),
    };
    let mut file = std::fs::File::create(path)?;
    let trace_file = TraceFile {
      trace_events: events,
    };
    serde_json::to_writer(&mut file, &trace_file)?;
    file.write_all(b"\n")
  }
}

struct TraceState {
  start: Instant,
  events: Mutex<Vec<TraceEvent>>,
}

impl TraceState {
  fn new() -> Self {
    Self {
      start: Instant::now(),
      events: Mutex::new(Vec::new()),
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
    let ts = start.duration_since(self.start).as_micros() as u64;
    let dur = end.duration_since(start).as_micros() as u64;
    let tid = current_thread_numeric_id();
    if let Ok(mut events) = self.events.lock() {
      events.push(TraceEvent {
        name: name.into_owned(),
        cat: cat.to_string(),
        ph: "X",
        ts,
        dur,
        pid: std::process::id(),
        tid,
        args,
      });
    }
  }
}

pub(crate) struct TraceSpan {
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
  pub(crate) fn arg_u64(&mut self, key: &'static str, value: u64) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::Number(value.into()));
  }

  #[inline]
  pub(crate) fn arg_i64(&mut self, key: &'static str, value: i64) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::Number(value.into()));
  }

  #[inline]
  pub(crate) fn arg_bool(&mut self, key: &'static str, value: bool) {
    let Some(_state) = &self.state else {
      return;
    };
    let args = self.args.get_or_insert_with(TraceArgs::new);
    args.insert(key.to_string(), JsonValue::Bool(value));
  }

  #[inline]
  pub(crate) fn arg_str(&mut self, key: &'static str, value: &str) {
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
  name: String,
  cat: String,
  ph: &'static str,
  ts: u64,
  dur: u64,
  pid: u32,
  tid: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  args: Option<TraceArgs>,
}

#[derive(Serialize)]
struct TraceFile {
  #[serde(rename = "traceEvents")]
  trace_events: Vec<TraceEvent>,
}
