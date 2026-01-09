use super::{Backend, BackendInit, HostEnvironment};
use crate::wpt_report::WptReport;
use crate::RunError;
use rquickjs::{Context, Object, Runtime};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct QuickJsBackend {
  rt: Option<Runtime>,
  ctx: Option<Context>,
  deadline: Option<Instant>,
  timed_out: bool,
  max_tasks: usize,
  max_microtasks: usize,
  tasks_executed: usize,
  microtasks_executed: usize,
}

impl QuickJsBackend {
  pub fn new() -> Self {
    Self::default()
  }

  fn ctx(&self) -> Result<&Context, RunError> {
    self
      .ctx
      .as_ref()
      .ok_or_else(|| RunError::Js("quickjs backend is not initialized".to_string()))
  }

  fn rt(&self) -> Result<&Runtime, RunError> {
    self
      .rt
      .as_ref()
      .ok_or_else(|| RunError::Js("quickjs backend is not initialized".to_string()))
  }

  fn drain_microtasks_internal(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(());
    }

    let max_microtasks = self.max_microtasks;
    let mut executed = self.microtasks_executed;
    let rt = self.rt()?;

    loop {
      if executed >= max_microtasks {
        self.timed_out = true;
        self.microtasks_executed = executed;
        return Ok(());
      }

      match rt.execute_pending_job() {
        Ok(true) => {
          executed += 1;
          continue;
        }
        Ok(false) => {
          self.microtasks_executed = executed;
          return Ok(());
        }
        Err(err) => {
          self.microtasks_executed = executed;
          return Err(RunError::Js(err.to_string()));
        }
      }
    }
  }

  fn report_json(&self) -> Result<Option<String>, RunError> {
    self.ctx()?.with(|ctx| {
      let globals = ctx.globals();
      globals
        .get::<_, Option<String>>("__fastrender_wpt_report_json")
        .map_err(|e| RunError::Js(e.to_string()))
    })
  }

  fn clear_report_json(&self) -> Result<(), RunError> {
    self.ctx()?.with(|ctx| {
      let globals = ctx.globals();
      globals
        .set("__fastrender_wpt_report_json", ())
        .map_err(|e| RunError::Js(e.to_string()))
    })
  }
}

impl Backend for QuickJsBackend {
  fn init_realm(
    &mut self,
    init: BackendInit,
    _host: Option<&mut dyn HostEnvironment>,
  ) -> Result<(), RunError> {
    self.deadline = Some(Instant::now() + init.timeout);
    self.timed_out = false;
    self.max_tasks = init.max_tasks;
    self.max_microtasks = init.max_microtasks;
    self.tasks_executed = 0;
    self.microtasks_executed = 0;

    let rt = Runtime::new().map_err(|e| RunError::Js(e.to_string()))?;

    // Interrupt handler for per-test wall-time.
    let deadline = self.deadline.expect("deadline is set");
    let deadline = Arc::new(deadline);
    rt.set_interrupt_handler(Some(Box::new({
      let deadline = Arc::clone(&deadline);
      move || Instant::now() >= *deadline
    })));

    let ctx = Context::full(&rt).map_err(|e| RunError::Js(e.to_string()))?;

    ctx.with(|ctx| -> Result<(), RunError> {
      let globals = ctx.globals();

      install_window_shims(ctx.clone(), &globals, &init.test_url)?;
      // The QuickJS backend largely relies on JS shims for browser-ish globals. Provide a tiny DOM
      // surface so `.window.js` smoke tests can interact with `document.head`/`document.body`.
      eval_script(ctx.clone(), DOM_SHIM).map_err(RunError::Js)?;
      eval_script(ctx.clone(), TIMER_SHIM).map_err(RunError::Js)?;
      eval_script(ctx.clone(), FASTR_REPORT_HOOK).map_err(RunError::Js)?;

      Ok(())
    })?;

    self.rt = Some(rt);
    self.ctx = Some(ctx);

    Ok(())
  }

  fn eval_script(&mut self, source: &str, name: &str) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(());
    }

    let source_with_url = format!("{source}\n//# sourceURL={name}\n");

    let result = self
      .ctx()?
      .with(|ctx| ctx.eval::<(), _>(source_with_url).map_err(|e| RunError::Js(e.to_string())));

    match result {
      Ok(()) => {
        // Microtask checkpoint after every script evaluation.
        self.drain_microtasks_internal()?;
        Ok(())
      }
      Err(err) => Err(err),
    }
  }

  fn drain_microtasks(&mut self) -> Result<(), RunError> {
    self.drain_microtasks_internal()
  }

  fn poll_event_loop(&mut self) -> Result<bool, RunError> {
    if self.timed_out {
      return Ok(false);
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(false);
    }
    if self.tasks_executed >= self.max_tasks {
      self.timed_out = true;
      return Ok(false);
    }

    let ran: i32 = self.ctx()?.with(|ctx| {
      ctx
        .eval("__fastrender_poll_timers()")
        .map_err(|e| RunError::Js(e.to_string()))
    })?;

    let ran = ran.max(0) as usize;
    if ran == 0 {
      return Ok(false);
    }

    self.tasks_executed = self.tasks_executed.saturating_add(ran);
    if self.tasks_executed > self.max_tasks {
      self.timed_out = true;
      return Ok(false);
    }

    // Microtask checkpoint after a task.
    self.drain_microtasks_internal()?;
    Ok(true)
  }

  fn take_report(&mut self) -> Result<Option<WptReport>, RunError> {
    let Some(json) = self.report_json()? else {
      return Ok(None);
    };

    // The runner's smoke tests sometimes call `__fastrender_wpt_report` directly with a minimal
    // payload (e.g. `{ file_status: "pass" }`). Normalize that to the full `WptReport` shape.
    let value: serde_json::Value = serde_json::from_str(&json)
      .map_err(|e| RunError::Js(format!("failed to parse report payload as JSON: {e}")))?;
    let normalized = match value {
      serde_json::Value::Object(mut obj) => {
        obj
          .entry("file_status".to_string())
          .or_insert_with(|| serde_json::Value::String("error".to_string()));
        obj
          .entry("harness_status".to_string())
          .or_insert_with(|| serde_json::Value::String("ok".to_string()));
        serde_json::Value::Object(obj)
      }
      serde_json::Value::String(s) => serde_json::json!({
        "file_status": s,
        "harness_status": "ok"
      }),
      other => serde_json::json!({
        "file_status": "error",
        "harness_status": "error",
        "message": format!("unexpected report payload type: {}", other)
      }),
    };

    let report: WptReport = serde_json::from_value(normalized)
      .map_err(|e| RunError::Js(format!("failed to decode report payload: {e}")))?;

    // Ensure subsequent polls return None.
    let _ = self.clear_report_json();

    Ok(Some(report))
  }

  fn is_timed_out(&self) -> bool {
    if self.timed_out {
      return true;
    }
    let Some(deadline) = self.deadline else {
      return true;
    };
    Instant::now() >= deadline
  }

  fn idle_wait(&mut self) {
    if self.timed_out {
      return;
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return;
    }
    // QuickJS timer shim uses wall clock time, so sleeping advances timers.
    std::thread::sleep(Duration::from_millis(1));
  }
}

fn eval_script<'js>(ctx: rquickjs::Ctx<'js>, source: &str) -> Result<(), String> {
  ctx.eval::<(), _>(source).map_err(|e| e.to_string())
}

fn install_window_shims<'js>(
  ctx: rquickjs::Ctx<'js>,
  globals: &Object<'js>,
  href: &str,
) -> Result<(), RunError> {
  globals
    .set("window", globals.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("self", globals.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;

  let location = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  location
    .set("href", href)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("location", location.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;

  let document = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  document
    .set("URL", href)
    .map_err(|e| RunError::Js(e.to_string()))?;
  document
    .set("location", location)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("document", document)
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Tiny console shim.
  let console = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  let log = rquickjs::Function::new(ctx, |msg: String| {
    eprintln!("[wpt] {msg}");
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  console
    .set("log", log)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("console", console)
    .map_err(|e| RunError::Js(e.to_string()))?;

  Ok(())
}

const TIMER_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.__fastrender_poll_timers === "function") return;

  var next_id = 1;
  var timers = new Map(); // id -> { cb, args, due, interval }

  function nowMs() {
    return Date.now();
  }

  function normalizeDelay(ms) {
    var n = Number(ms);
    if (!isFinite(n) || isNaN(n)) n = 0;
    if (n < 0) n = 0;
    return n;
  }

  function normalizeInterval(ms) {
    var n = normalizeDelay(ms);
    // Avoid 0ms busy-loops.
    if (n === 0) n = 1;
    return n;
  }

  function addTimer(cb, ms, interval, args) {
    var id = next_id++;
    timers.set(id, { cb: cb, args: args, due: nowMs() + ms, interval: interval });
    return id;
  }

  g.setTimeout = function (cb, ms /*, ...args */) {
    var delay = normalizeDelay(ms);
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    return addTimer(cb, delay, null, args);
  };

  g.setInterval = function (cb, ms /*, ...args */) {
    var interval = normalizeInterval(ms);
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    return addTimer(cb, interval, interval, args);
  };

  function clearTimer(id) {
    timers.delete(Number(id));
  }

  g.clearTimeout = clearTimer;
  g.clearInterval = clearTimer;

  if (typeof g.queueMicrotask !== "function") {
    g.queueMicrotask = function (cb) {
      // Schedule a Promise job so the host can drain it via `execute_pending_job`.
      Promise.resolve().then(cb);
    };
  }

  g.__fastrender_poll_timers = function () {
    var now = nowMs();
    var due_ids = [];
    timers.forEach(function (entry, id) {
      if (entry.due <= now) due_ids.push(id);
    });
    due_ids.sort(function (a, b) { return a - b; });

    for (var i = 0; i < due_ids.length; i++) {
      var id = due_ids[i];
      var entry = timers.get(id);
      if (!entry) continue;

      if (entry.interval != null) {
        entry.due = now + entry.interval;
        timers.set(id, entry);
      } else {
        timers.delete(id);
      }

      if (typeof entry.cb === "function") {
        entry.cb.apply(g, entry.args);
      } else if (typeof entry.cb === "string") {
        throw new Error("setTimeout/setInterval string handler is not supported");
      } else {
        throw new Error("timer callback is not callable");
      }
    }

    return due_ids.length;
  };
})();
"#;

// Minimal DOM shims for the QuickJS backend.
//
// Unlike the vm-js backend (which wires DOM-ish objects through Rust host bindings), QuickJS uses
// JavaScript shims. The smoke corpus needs `document.createElement`, `document.head`, and
// `document.body` with a basic `appendChild`/`childNodes` API.
const DOM_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (!g.document) return;
  if (g.document.body && g.document.head && typeof g.document.createElement === "function") return;

  function makeNode(tag) {
    var node = {};
    node.tagName = String(tag).toUpperCase();
    node.childNodes = [];
    node.appendChild = function (child) {
      node.childNodes.push(child);
      return child;
    };
    node.removeChild = function (child) {
      var idx = node.childNodes.indexOf(child);
      if (idx >= 0) node.childNodes.splice(idx, 1);
      return child;
    };
    return node;
  }

  g.document.createElement = function (tag) {
    return makeNode(tag);
  };
  g.document.head = makeNode("head");
  g.document.body = makeNode("body");
})();
"#;

const FASTR_REPORT_HOOK: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  g.__fastrender_wpt_report_json = null;

  g.__fastrender_wpt_report = function (payload) {
    try {
      g.__fastrender_wpt_report_json = JSON.stringify(payload);
    } catch (e) {
      // Fall back to a minimal error payload so the runner can terminate deterministically.
      try {
        g.__fastrender_wpt_report_json = JSON.stringify({
          file_status: "error",
          harness_status: "error",
          message: String(e && e.message ? e.message : e),
          stack: String(e && e.stack ? e.stack : ""),
          subtests: []
        });
      } catch (_e) {
        g.__fastrender_wpt_report_json = '{"file_status":"error","harness_status":"error","message":"report serialization failed","stack":null,"subtests":[]}';
      }
    }
  };
})();
"#;
