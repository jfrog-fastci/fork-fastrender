use super::{Backend, BackendInit, HostEnvironment};
use crate::cookie_jar::CookieJar;
use crate::dom_shims::install_dom_shims;
use crate::fetch::install_fetch_shims;
use crate::url_shims::install_url_shims;
use crate::window_or_worker_global_scope::{
  forgiving_base64_decode, forgiving_base64_encode, is_secure_context_for_document_url,
  latin1_encode, serialized_origin_for_document_url,
};
use crate::wpt_report::WptReport;
use crate::RunError;
use rquickjs::{Context, Function, Object, Runtime};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct QuickJsBackend {
  rt: Option<Runtime>,
  ctx: Option<Context>,
  cookie_jar: Option<Rc<RefCell<CookieJar>>>,
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
          // `rquickjs` surfaces an interrupt as a regular error; normalize it to a timeout so the
          // runner doesn't misclassify deadline-based termination as a harness error.
          if self.is_timed_out() {
            self.timed_out = true;
            return Ok(());
          }
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
    // Per-test timeouts are enforced via QuickJS's interrupt handler. When tests execute in
    // parallel, runtime/context creation can contend on global locks inside `rquickjs`/QuickJS.
    // Starting the deadline before that init work can cause spurious timeouts (the JS never ran,
    // but the wall clock deadline expired while waiting to initialize).
    //
    // To avoid flakiness, we start the deadline *after* the realm is fully initialized.
    self.deadline = None;
    self.timed_out = false;
    self.max_tasks = init.max_tasks;
    self.max_microtasks = init.max_microtasks;
    self.tasks_executed = 0;
    self.microtasks_executed = 0;

    let rt = Runtime::new().map_err(|e| RunError::Js(e.to_string()))?;

    let ctx = Context::full(&rt).map_err(|e| RunError::Js(e.to_string()))?;

    let cookie_jar = Rc::new(RefCell::new(CookieJar::new()));

    ctx.with(|ctx| -> Result<(), RunError> {
      let globals = ctx.globals();

      install_window_shims(
        ctx.clone(),
        &globals,
        &init.test_url,
        Rc::clone(&cookie_jar),
      )?;
      // Install minimal DOM shims so `.window.js` smoke tests can exercise DOMParsing-style APIs
      // (`innerHTML`, `outerHTML`, DocumentFragment insertion, etc.).
      install_dom_shims(ctx.clone(), &globals).map_err(|e| RunError::Js(e.to_string()))?;
      eval_script(ctx.clone(), TIMER_SHIM).map_err(RunError::Js)?;
      eval_script(ctx.clone(), FASTR_REPORT_HOOK).map_err(RunError::Js)?;

      Ok(())
    })?;

    // Interrupt handler for per-test wall-time. This must be installed *after* realm
    // initialization; see comment above.
    let deadline = Instant::now() + init.timeout;
    self.deadline = Some(deadline);
    let deadline = Arc::new(deadline);
    rt.set_interrupt_handler(Some(Box::new({
      let deadline = Arc::clone(&deadline);
      move || Instant::now() >= *deadline
    })));

    self.rt = Some(rt);
    self.ctx = Some(ctx);
    self.cookie_jar = Some(cookie_jar);

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

    let result = self.ctx()?.with(|ctx| {
      ctx
        .eval::<(), _>(source_with_url)
        .map_err(|e| RunError::Js(e.to_string()))
    });

    match result {
      Ok(()) => {
        // Microtask checkpoint after every script evaluation.
        self.drain_microtasks_internal()?;
        Ok(())
      }
      Err(err) => {
        // `rquickjs` uses the interrupt handler to abort execution, but reports that abort as an
        // exception. Detect that we've hit the per-test deadline and report it as a timeout.
        if self.is_timed_out() {
          self.timed_out = true;
          return Ok(());
        }
        Err(err)
      }
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

    let ran: i32 = match self.ctx()?.with(|ctx| {
      ctx
        .eval("__fastrender_poll_timers()")
        .map_err(|e| RunError::Js(e.to_string()))
    }) {
      Ok(v) => v,
      Err(err) => {
        if self.is_timed_out() {
          self.timed_out = true;
          return Ok(false);
        }
        return Err(err);
      }
    };

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
  cookie_jar: Rc<RefCell<CookieJar>>,
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

  // Host-backed cookie jar used by the DOM shims to implement `document.cookie`.
  let get_cookie = Function::new(ctx.clone(), {
    let cookie_jar = Rc::clone(&cookie_jar);
    move || Ok::<String, rquickjs::Error>(cookie_jar.borrow().cookie_string())
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("__fastrender_get_cookie", get_cookie)
    .map_err(|e| RunError::Js(e.to_string()))?;

  let set_cookie = Function::new(ctx.clone(), {
    let cookie_jar = Rc::clone(&cookie_jar);
    move |cookie_string: String| -> Result<(), rquickjs::Error> {
      cookie_jar.borrow_mut().set_cookie_string(&cookie_string);
      Ok(())
    }
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("__fastrender_set_cookie", set_cookie)
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Tiny console shim.
  let console = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  let log = rquickjs::Function::new(ctx.clone(), |msg: String| {
    eprintln!("[wpt] {msg}");
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  console
    .set("log", log)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("console", console)
    .map_err(|e| RunError::Js(e.to_string()))?;

  // WindowOrWorkerGlobalScope primitives.
  let origin = serialized_origin_for_document_url(href);
  globals
    .set("origin", origin)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("isSecureContext", is_secure_context_for_document_url(href))
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("crossOriginIsolated", false)
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Install JS shims for `reportError` and a tiny DOMException-like throw helper.
  eval_script(ctx.clone(), WINDOW_OR_WORKER_GLOBAL_SCOPE_SHIM).map_err(RunError::Js)?;

  let atob = Function::new(ctx.clone(), |ctx: rquickjs::Ctx<'js>, data: String| {
    let decoded = match forgiving_base64_decode(&data) {
      Ok(bytes) => bytes,
      Err(_) => {
        return Err(throw_dom_exception(
          &ctx,
          "InvalidCharacterError",
          "The string to be decoded is not correctly encoded.",
        ))
      }
    };
    Ok(decoded.iter().map(|&b| b as char).collect::<String>())
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("atob", atob)
    .map_err(|e| RunError::Js(e.to_string()))?;

  let btoa = Function::new(ctx.clone(), |ctx: rquickjs::Ctx<'js>, data: String| {
    let bytes = match latin1_encode(&data) {
      Ok(bytes) => bytes,
      Err(_) => {
        return Err(throw_dom_exception(
          &ctx,
          "InvalidCharacterError",
          "The string to be encoded contains characters outside of the Latin1 range.",
        ))
      }
    };
    let encoded = match forgiving_base64_encode(&bytes) {
      Ok(encoded) => encoded,
      Err(_) => {
        return Err(throw_dom_exception(
          &ctx,
          "InvalidCharacterError",
          "The string to be encoded is too large.",
        ))
      }
    };
    Ok(encoded)
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("btoa", btoa)
    .map_err(|e| RunError::Js(e.to_string()))?;

  install_url_shims(ctx.clone(), globals).map_err(|e| RunError::Js(e.to_string()))?;
  install_fetch_shims(ctx, globals).map_err(|e| RunError::Js(e.to_string()))?;

  Ok(())
}

fn throw_dom_exception<'js>(
  ctx: &rquickjs::Ctx<'js>,
  name: &'static str,
  message: &str,
) -> rquickjs::Error {
  let globals = ctx.globals();
  let Ok(thrower) = globals.get::<_, Function<'js>>("__fastrender_throw_dom_exception") else {
    return rquickjs::Error::new_from_js_message("DOMException", name, message);
  };
  match thrower.call::<_, ()>((name, message)) {
    Ok(_) => rquickjs::Error::new_from_js_message("DOMException", name, message),
    Err(e) => e,
  }
}

const WINDOW_OR_WORKER_GLOBAL_SCOPE_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;

  if (typeof g.__fastrender_throw_dom_exception !== "function") {
    g.__fastrender_throw_dom_exception = function (name, message) {
      throw { name: String(name), message: String(message) };
    };
  }

  if (typeof g.reportError !== "function") {
    g.reportError = function (e) {
      try {
        // `String(Symbol("x"))` is allowed; avoid `e + ""` which throws for Symbols.
        g.console && g.console.log && g.console.log(String(e));
      } catch (_err) {
        try {
          g.console && g.console.log && g.console.log("[reportError]");
        } catch (_err2) {}
      }
    };
  }
})();
"#;

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
