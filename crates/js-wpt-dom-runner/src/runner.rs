use crate::discover::TestCase;
use crate::meta::parse_leading_meta;
use crate::timer_event_loop::{QueueLimits, TimerEventLoop, TimerExecution};
use crate::wpt_fs::{WptFs, WptFsError};
use crate::wpt_report::WptReport;
use regex::Regex;
use rquickjs::{Context, Function, Object, Runtime};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct RunnerConfig {
  pub default_timeout: Duration,
  pub long_timeout: Duration,
  pub max_tasks: usize,
  pub max_microtasks: usize,
}

impl Default for RunnerConfig {
  fn default() -> Self {
    Self {
      default_timeout: Duration::from_secs(5),
      long_timeout: Duration::from_secs(30),
      max_tasks: 100_000,
      max_microtasks: 100_000,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
  Pass,
  Fail(String),
  Skip(String),
  Error(String),
  Timeout,
}

#[derive(Debug, Clone)]
pub struct RunResult {
  pub outcome: RunOutcome,
  pub wpt_report: Option<WptReport>,
}

pub type RunResultResult = Result<RunResult, RunError>;

#[derive(Debug, Error)]
pub enum RunError {
  #[error("WPT fs error: {0}")]
  Fs(#[from] WptFsError),
  #[error("JS runtime error: {0}")]
  Js(String),
  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct Runner {
  fs: WptFs,
  config: RunnerConfig,
}

impl Runner {
  pub fn new(fs: WptFs, config: RunnerConfig) -> Self {
    Self { fs, config }
  }

  pub fn fs(&self) -> &WptFs {
    &self.fs
  }

  pub fn run_test(&self, test: &TestCase) -> RunResultResult {
    if let Some(reason) = test.kind.skip_reason() {
      return Ok(RunResult {
        outcome: RunOutcome::Skip(reason.to_string()),
        wpt_report: None,
      });
    }
    if !test.kind.is_runnable_in_window() {
      return Ok(RunResult {
        outcome: RunOutcome::Skip("unsupported test kind".to_string()),
        wpt_report: None,
      });
    }

    match test.kind {
      crate::discover::TestKind::Html => self.run_html_test_in_window(test),
      _ => self.run_js_test_in_window(test),
    }
  }

  fn run_js_test_in_window(&self, test: &TestCase) -> RunResultResult {
    let test_source = self.fs.read_to_string(&test.path)?;
    let meta = parse_leading_meta(&test_source);

    let timeout = meta.timeout.unwrap_or(self.config.default_timeout);

    // `// META: timeout=long` maps to the runner's `long_timeout` instead of a hard-coded value.
    let timeout = if meta
      .directives
      .iter()
      .any(|d| matches!(d, crate::meta::MetaDirective::TimeoutLong))
    {
      self.config.long_timeout
    } else {
      timeout
    };

    let base_dir = id_dir(&test.id);

    let mut scripts = Vec::new();
    // Always load testharness.js + FastRender reporter shim first.
    scripts.push(ScriptToRun::External(
      "/resources/testharness.js".to_string(),
    ));
    scripts.push(ScriptToRun::External(
      "/resources/fastrender_testharness_report.js".to_string(),
    ));
    for url in meta.scripts {
      if url == "/resources/testharness.js"
        || url == "resources/testharness.js"
        || url == "/resources/fastrender_testharness_report.js"
        || url == "resources/fastrender_testharness_report.js"
        || url == "/resources/testharnessreport.js"
        || url == "resources/testharnessreport.js"
      {
        continue;
      }
      scripts.push(ScriptToRun::External(url));
    }
    scripts.push(ScriptToRun::Inline(test_source));

    self.run_scripts_in_window(test, &base_dir, scripts, timeout)
  }

  fn run_html_test_in_window(&self, test: &TestCase) -> RunResultResult {
    let html_source = self.fs.read_to_string(&test.path)?;
    let base_dir = id_dir(&test.id);

    let mut scripts = Vec::new();
    scripts.push(ScriptToRun::External(
      "/resources/testharness.js".to_string(),
    ));
    scripts.push(ScriptToRun::External(
      "/resources/fastrender_testharness_report.js".to_string(),
    ));

    for script in parse_html_scripts(&html_source)? {
      match script {
        ScriptToRun::External(url)
          if url == "/resources/testharness.js"
            || url == "resources/testharness.js"
            || url == "/resources/fastrender_testharness_report.js"
            || url == "resources/fastrender_testharness_report.js"
            || url == "/resources/testharnessreport.js"
            || url == "resources/testharnessreport.js" =>
        {
          continue;
        }
        other => scripts.push(other),
      }
    }

    self.run_scripts_in_window(test, &base_dir, scripts, self.config.default_timeout)
  }

  fn run_scripts_in_window(
    &self,
    test: &TestCase,
    base_dir: &str,
    scripts: Vec<ScriptToRun>,
    timeout: Duration,
  ) -> RunResultResult {
    let timer_loop = Rc::new(RefCell::new(TimerEventLoop::new(QueueLimits::default())));
    let rt = Runtime::new().map_err(|e| RunError::Js(e.to_string()))?;

    // Interrupt handler for per-test wall-time.
    let deadline = Instant::now() + timeout;
    let deadline = Arc::new(deadline);
    rt.set_interrupt_handler(Some(Box::new({
      let deadline = Arc::clone(&deadline);
      move || Instant::now() >= *deadline
    })));

    let ctx = Context::full(&rt).map_err(|e| RunError::Js(e.to_string()))?;

    let test_url = test.url();

    // One-time realm initialization + script execution.
    let init = ctx.with(|ctx| -> Result<(), RunError> {
      let globals = ctx.globals();
      install_window_shims(ctx.clone(), &globals, &test_url)?;

      install_timer_host_fns(ctx.clone(), &globals, Rc::clone(&timer_loop))?;

      // Install JS shims for timers + EventTarget.
      ctx
        .eval::<(), _>(HOST_SHIMS)
        .map_err(|e| RunError::Js(e.to_string()))?;

      // Define the host hook used by `fastrender_testharness_report.js` to emit a result payload.
      ctx
        .eval::<(), _>(FASTR_REPORT_HOOK)
        .map_err(|e| RunError::Js(e.to_string()))?;

      // Load / evaluate scripts in-order. If a script throws, attempt to surface it as a harness
      // error via `window.onerror` so the reporter can emit a deterministic payload.
      for script in scripts {
        let src = match script {
          ScriptToRun::External(url) => {
            let path = self.fs.resolve_url(base_dir, &url)?;
            self.fs.read_to_string(&path)?
          }
          ScriptToRun::Inline(src) => src,
        };

        if let Err(err) = ctx.eval::<(), _>(src) {
          let msg = err.to_string();
          // If the interrupt handler fired, treat as a timeout and propagate.
          if msg.contains("interrupted") || msg.contains("Interrupt") {
            return Err(RunError::Js(msg));
          }

          // Best-effort: call `window.onerror(message, source, lineno, colno, error)` to let the
          // harness mark a file-level error and run completion callbacks.
          let onerror: Option<rquickjs::Function> = globals
            .get("onerror")
            .map_err(|e| RunError::Js(e.to_string()))?;
          if let Some(onerror) = onerror {
            let _ = onerror.call::<_, ()>((msg.clone(), "", 0i32, 0i32, msg.clone()));
            break;
          }

          return Err(RunError::Js(msg));
        }
      }

      Ok(())
    });

    if let Err(err) = init {
      let outcome = match err {
        RunError::Js(msg) if msg.contains("interrupted") || msg.contains("Interrupt") => {
          RunOutcome::Timeout
        }
        RunError::Js(msg) => RunOutcome::Error(msg),
        other => return Err(other),
      };
      return Ok(RunResult {
        outcome,
        wpt_report: None,
      });
    }

    let mut tasks_executed: usize = 0;
    let mut microtasks_executed: usize = 0;

    // Drive the minimal event loop until the reporter hook is called or we time out.
    let (outcome, wpt_report) = loop {
      if Instant::now() >= *deadline {
        break (RunOutcome::Timeout, None);
      }

      // Run any queued Promise jobs (microtasks).
      match drain_promise_jobs_limited(&rt, &mut microtasks_executed, self.config.max_microtasks) {
        Ok(()) => {}
        Err(PromiseDrainError::MaxMicrotasks) => break (RunOutcome::Timeout, None),
        Err(PromiseDrainError::Js(msg)) => {
          if is_interrupt_error(&msg) {
            break (RunOutcome::Timeout, None);
          }
          break (RunOutcome::Error(msg), None);
        }
      }

      if tasks_executed >= self.config.max_tasks {
        break (RunOutcome::Timeout, None);
      }

      let did_report = ctx.with(|ctx| -> Result<bool, RunError> {
        let globals = ctx.globals();
        let did_report: Option<bool> = globals
          .get("__fastrender_wpt_report_called")
          .map_err(|e| RunError::Js(e.to_string()))?;
        Ok(did_report.unwrap_or(false))
      });
      let did_report = match did_report {
        Ok(v) => v,
        Err(RunError::Js(msg)) if is_interrupt_error(&msg) => break (RunOutcome::Timeout, None),
        Err(RunError::Js(msg)) => break (RunOutcome::Error(msg), None),
        Err(other) => return Err(other),
      };

      if did_report {
        let report = ctx.with(|ctx| -> Result<Option<String>, RunError> {
          let globals = ctx.globals();
          let json: Option<String> = globals
            .get("__fastrender_wpt_report_json")
            .map_err(|e| RunError::Js(e.to_string()))?;
          Ok(json)
        });
        let json = match report {
          Ok(v) => v,
          Err(RunError::Js(msg)) if msg.contains("interrupted") || msg.contains("Interrupt") => {
            break (RunOutcome::Timeout, None)
          }
          Err(RunError::Js(msg)) => break (RunOutcome::Error(msg), None),
          Err(other) => return Err(other),
        };

        let Some(json) = json else {
          break (
            RunOutcome::Error("missing fastrender testharness report".to_string()),
            None,
          );
        };

        let parsed: WptReport = match serde_json::from_str(&json) {
          Ok(v) => v,
          Err(err) => {
            break (
              RunOutcome::Error(format!(
                "failed to parse fastrender testharness report: {err}"
              )),
              None,
            )
          }
        };

        let msg = parsed
          .message
          .clone()
          .or_else(|| first_nonpass_message(&parsed.subtests));

        let outcome = match parsed.file_status.as_str() {
          "pass" => RunOutcome::Pass,
          "fail" => RunOutcome::Fail(msg.unwrap_or_else(|| "test failed".to_string())),
          "timeout" => RunOutcome::Timeout,
          "error" => RunOutcome::Error(msg.unwrap_or_else(|| "harness error".to_string())),
          other => RunOutcome::Error(format!("unknown file_status: {other}")),
        };

        break (outcome, Some(parsed));
      }

      let task = {
        let mut loop_state = timer_loop.borrow_mut();
        loop_state
          .queue_due_timers()
          .map_err(|e| RunError::Js(e.to_string()))?;

        if let Some(task) = loop_state.pop_task() {
          task
        } else if let Some(due) = loop_state.next_timer_due() {
          if due > loop_state.now() {
            loop_state.advance_to(due);
          }
          continue;
        } else {
          break (RunOutcome::Timeout, None);
        }
      };

      tasks_executed += 1;

      let exec: Option<TimerExecution> = timer_loop.borrow_mut().begin_timer_task(task);
      let Some(exec) = exec else {
        continue;
      };
      let prev_timer_nesting_level = exec.prev_timer_nesting_level;

      // Invoke the callback in JS, then (for intervals) reschedule.
      let invoke_res = ctx.with(|ctx| -> Result<(), RunError> {
        let globals = ctx.globals();
        let invoke: Function = globals
          .get("__fastrender_invoke_timer")
          .map_err(|e| RunError::Js(e.to_string()))?;
        invoke
          .call::<(i32,), ()>((exec.id,))
          .map_err(|e| RunError::Js(e.to_string()))?;
        Ok(())
      });
      match invoke_res {
        Ok(()) => {}
        Err(RunError::Js(msg)) if is_interrupt_error(&msg) => break (RunOutcome::Timeout, None),
        Err(RunError::Js(msg)) => break (RunOutcome::Error(msg), None),
        Err(other) => return Err(other),
      }

      timer_loop.borrow_mut().finish_timer_task(exec);

      match drain_promise_jobs_limited(&rt, &mut microtasks_executed, self.config.max_microtasks) {
        Ok(()) => {}
        Err(PromiseDrainError::MaxMicrotasks) => break (RunOutcome::Timeout, None),
        Err(PromiseDrainError::Js(msg)) => {
          if is_interrupt_error(&msg) {
            break (RunOutcome::Timeout, None);
          }
          break (RunOutcome::Error(msg), None);
        }
      }

      timer_loop
        .borrow_mut()
        .set_timer_nesting_level(prev_timer_nesting_level);
    };

    // If the interrupt handler fired, QuickJS surfaces it as an eval error. Map it to Timeout.
    Ok(RunResult {
      outcome,
      wpt_report,
    })
  }
}

fn id_dir(id: &str) -> String {
  match id.rsplit_once('/') {
    Some((dir, _file)) => dir.to_string(),
    None => String::new(),
  }
}

fn install_window_shims<'js>(
  ctx: rquickjs::Ctx<'js>,
  globals: &Object<'js>,
  href: &str,
) -> Result<(), RunError> {
  // `window` / `self` should refer to the global object in a window realm.
  globals
    .set("window", globals.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("self", globals.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Minimal `location` object.
  let location = Object::new(ctx.clone()).map_err(|e| RunError::Js(e.to_string()))?;
  location
    .set("href", href)
    .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("location", location.clone())
    .map_err(|e| RunError::Js(e.to_string()))?;

  // Minimal `document` object.
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

  // A tiny console shim to help debugging failing tests.
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

#[derive(Debug)]
enum PromiseDrainError {
  MaxMicrotasks,
  Js(String),
}

fn drain_promise_jobs_limited(
  rt: &Runtime,
  microtasks_executed: &mut usize,
  max_microtasks: usize,
) -> Result<(), PromiseDrainError> {
  loop {
    if *microtasks_executed >= max_microtasks {
      return Err(PromiseDrainError::MaxMicrotasks);
    }
    match rt.execute_pending_job() {
      Ok(true) => {
        *microtasks_executed += 1;
        continue;
      }
      Ok(false) => return Ok(()),
      Err(err) => return Err(PromiseDrainError::Js(err.to_string())),
    }
  }
}

fn is_interrupt_error(msg: &str) -> bool {
  msg.contains("interrupted") || msg.contains("Interrupt")
}

fn normalize_delay_ms(ms: f64) -> Duration {
  let ms = if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
  let millis = ms.trunc() as u64;
  Duration::from_millis(millis)
}

fn install_timer_host_fns<'js>(
  ctx: rquickjs::Ctx<'js>,
  globals: &Object<'js>,
  timer_loop: Rc<RefCell<TimerEventLoop>>,
) -> Result<(), RunError> {
  let set_timeout = Function::new(ctx.clone(), {
    let timer_loop = Rc::clone(&timer_loop);
    move |ms: f64| -> rquickjs::Result<i32> {
      let delay = normalize_delay_ms(ms);
      let mut timer_loop = timer_loop.borrow_mut();
      timer_loop
        .set_timeout(delay)
        .map_err(|e| rquickjs::Error::new_from_js_message("TimerEventLoop", "TimerId", e.to_string()))
    }
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("__fastrender_set_timeout", set_timeout)
    .map_err(|e| RunError::Js(e.to_string()))?;

  let set_interval = Function::new(ctx.clone(), {
    let timer_loop = Rc::clone(&timer_loop);
    move |ms: f64| -> rquickjs::Result<i32> {
      let interval = normalize_delay_ms(ms);
      let mut timer_loop = timer_loop.borrow_mut();
      timer_loop
        .set_interval(interval)
        .map_err(|e| rquickjs::Error::new_from_js_message("TimerEventLoop", "TimerId", e.to_string()))
    }
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("__fastrender_set_interval", set_interval)
    .map_err(|e| RunError::Js(e.to_string()))?;

  let clear_timeout = Function::new(ctx.clone(), {
    let timer_loop = Rc::clone(&timer_loop);
    move |id: i32| {
      timer_loop.borrow_mut().clear_timer(id);
    }
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("__fastrender_clear_timeout", clear_timeout)
    .map_err(|e| RunError::Js(e.to_string()))?;

  let clear_interval = Function::new(ctx, {
    let timer_loop = Rc::clone(&timer_loop);
    move |id: i32| {
      timer_loop.borrow_mut().clear_timer(id);
    }
  })
  .map_err(|e| RunError::Js(e.to_string()))?;
  globals
    .set("__fastrender_clear_interval", clear_interval)
    .map_err(|e| RunError::Js(e.to_string()))?;

  Ok(())
}

const HOST_SHIMS: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (g.__fastrender_host_shims_installed) return;
  g.__fastrender_host_shims_installed = true;

  function requireHostFn(name) {
    if (typeof g[name] !== "function") {
      throw new Error("missing host function: " + name);
    }
    return g[name];
  }

  var hostSetTimeout = requireHostFn("__fastrender_set_timeout");
  var hostSetInterval = requireHostFn("__fastrender_set_interval");
  var hostClearTimeout = requireHostFn("__fastrender_clear_timeout");
  var hostClearInterval = requireHostFn("__fastrender_clear_interval");

  var timers = new Map(); // id -> { kind, cb, args }
  g.__fastrender_invoke_timer = function (id) {
    id = Number(id) | 0;
    var entry = timers.get(id);
    if (!entry) return;
    if (entry.kind === "timeout") {
      timers.delete(id);
    }
    var cb = entry.cb;
    if (typeof cb === "function") {
      cb.apply(g, entry.args);
      return;
    }
    if (typeof cb === "string") {
      throw new Error("timer string handler is not supported");
    }
    throw new Error("timer callback is not callable");
  };

  function normalizeDelay(ms) {
    var n = Number(ms);
    if (!isFinite(n) || isNaN(n) || n < 0) n = 0;
    return Math.floor(n);
  }

  g.setTimeout = function (cb, ms /*, ...args */) {
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    if (typeof cb !== "function") {
      if (typeof cb === "string") {
        throw new Error("setTimeout string handler is not supported");
      }
      throw new Error("setTimeout callback is not callable");
    }
    var id = hostSetTimeout(normalizeDelay(ms));
    timers.set(id, { kind: "timeout", cb: cb, args: args });
    return id;
  };

  g.clearTimeout = function (id) {
    id = Number(id) | 0;
    timers.delete(id);
    hostClearTimeout(id);
  };

  g.setInterval = function (cb, ms /*, ...args */) {
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    if (typeof cb !== "function") {
      if (typeof cb === "string") {
        throw new Error("setInterval string handler is not supported");
      }
      throw new Error("setInterval callback is not callable");
    }
    var id = hostSetInterval(normalizeDelay(ms));
    timers.set(id, { kind: "interval", cb: cb, args: args });
    return id;
  };

  g.clearInterval = function (id) {
    id = Number(id) | 0;
    timers.delete(id);
    hostClearInterval(id);
  };

  g.queueMicrotask = function (cb) {
    if (typeof cb !== "function") {
      throw new TypeError("queueMicrotask callback must be a function");
    }
    Promise.resolve().then(cb);
  };

  var NONE = 0;
  var CAPTURING_PHASE = 1;
  var AT_TARGET = 2;
  var BUBBLING_PHASE = 3;

  class Event {
    constructor(type, init) {
      this.type = String(type);
      var opts = init && typeof init === "object" ? init : {};
      this.bubbles = Boolean(opts.bubbles);
      this.cancelable = Boolean(opts.cancelable);
      this.defaultPrevented = false;
      this.eventPhase = NONE;
      this.target = null;
      this.currentTarget = null;
      this.__stopPropagation = false;
      this.__stopImmediatePropagation = false;
      this.__inPassiveListener = false;
    }

    stopPropagation() {
      this.__stopPropagation = true;
    }

    stopImmediatePropagation() {
      this.__stopPropagation = true;
      this.__stopImmediatePropagation = true;
    }

    preventDefault() {
      if (!this.cancelable) return;
      if (this.__inPassiveListener) return;
      this.defaultPrevented = true;
    }
  }

  Event.NONE = NONE;
  Event.CAPTURING_PHASE = CAPTURING_PHASE;
  Event.AT_TARGET = AT_TARGET;
  Event.BUBBLING_PHASE = BUBBLING_PHASE;

  function parseListenerOptions(options) {
    if (options === true || options === false) {
      return { capture: Boolean(options), once: false, passive: false };
    }
    if (options && typeof options === "object") {
      return {
        capture: Boolean(options.capture),
        once: Boolean(options.once),
        passive: Boolean(options.passive),
      };
    }
    return { capture: false, once: false, passive: false };
  }

  function getParentTarget(target) {
    if (!target) return null;
    if (target.parent) return target.parent;
    if (target.parentNode) return target.parentNode;
    return null;
  }

  class EventTarget {
    constructor(parent) {
      this.parent = parent || null;
      this.__listeners = new Map(); // type -> [{ callback, capture, once, passive }]
    }

    addEventListener(type, callback, options) {
      if (callback === null || callback === undefined) return;
      var typeStr = String(type);
      var opts = parseListenerOptions(options);
      var list = this.__listeners.get(typeStr);
      if (!list) {
        list = [];
        this.__listeners.set(typeStr, list);
      }
      for (var i = 0; i < list.length; i++) {
        var l = list[i];
        if (l.callback === callback && l.capture === opts.capture) {
          return;
        }
      }
      list.push({
        callback: callback,
        capture: opts.capture,
        once: opts.once,
        passive: opts.passive,
      });
    }

    removeEventListener(type, callback, options) {
      var typeStr = String(type);
      var opts = parseListenerOptions(options);
      var list = this.__listeners.get(typeStr);
      if (!list) return;
      for (var i = 0; i < list.length; i++) {
        var l = list[i];
        if (l.callback === callback && l.capture === opts.capture) {
          list.splice(i, 1);
          break;
        }
      }
      if (list.length === 0) {
        this.__listeners.delete(typeStr);
      }
    }

    dispatchEvent(event) {
      if (!(event instanceof Event)) {
        throw new TypeError("dispatchEvent expects an Event");
      }

      event.target = this;
      event.currentTarget = null;
      event.eventPhase = NONE;
      event.__stopPropagation = false;
      event.__stopImmediatePropagation = false;
      event.__inPassiveListener = false;

      var path = [];
      var current = this;
      while (current) {
        path.push(current);
        current = getParentTarget(current);
      }

      var typeStr = String(event.type);

      function isRegistered(target, callback, capture) {
        var list = target.__listeners.get(typeStr);
        if (!list) return false;
        for (var i = 0; i < list.length; i++) {
          var l = list[i];
          if (l.callback === callback && l.capture === capture) return true;
        }
        return false;
      }

      function invoke(target, capture) {
        var list = target.__listeners.get(typeStr);
        if (!list) return;
        var snapshot = list.slice();
        for (var i = 0; i < snapshot.length; i++) {
          if (event.__stopImmediatePropagation) break;
          var listener = snapshot[i];
          if (listener.capture !== capture) continue;
          if (!isRegistered(target, listener.callback, listener.capture)) continue;

          if (listener.once) {
            target.removeEventListener(typeStr, listener.callback, {
              capture: listener.capture,
            });
          }

          var prevPassive = event.__inPassiveListener;
          event.__inPassiveListener = listener.passive;
          try {
            if (typeof listener.callback === "function") {
              listener.callback.call(target, event);
            } else if (
              listener.callback &&
              typeof listener.callback.handleEvent === "function"
            ) {
              listener.callback.handleEvent(event);
            }
          } finally {
            event.__inPassiveListener = prevPassive;
          }
        }
      }

      // Capturing: root -> parent of target
      if (path.length > 1) {
        event.eventPhase = CAPTURING_PHASE;
        for (var i = path.length - 1; i >= 1; i--) {
          event.currentTarget = path[i];
          invoke(path[i], /* capture */ true);
          if (event.__stopPropagation) break;
        }
      }

      if (event.__stopPropagation) {
        event.eventPhase = NONE;
        event.currentTarget = null;
        return !event.defaultPrevented;
      }

      // At target: capture listeners then bubble listeners.
      event.eventPhase = AT_TARGET;
      event.currentTarget = this;
      invoke(this, /* capture */ true);
      if (!event.__stopPropagation && !event.__stopImmediatePropagation) {
        invoke(this, /* capture */ false);
      }

      // Bubbling: parent -> root
      if (event.bubbles && !event.__stopPropagation && path.length > 1) {
        event.eventPhase = BUBBLING_PHASE;
        for (var i = 1; i < path.length; i++) {
          event.currentTarget = path[i];
          invoke(path[i], /* capture */ false);
          if (event.__stopPropagation) break;
        }
      }

      event.eventPhase = NONE;
      event.currentTarget = null;
      return !event.defaultPrevented;
    }
  }

  if (typeof g.Event !== "function") g.Event = Event;
  if (typeof g.EventTarget !== "function") g.EventTarget = EventTarget;

  class Document extends EventTarget {
    constructor() {
      super(null);
    }

    createElement(tagName) {
      var el = new Element(tagName);
      el.ownerDocument = this;
      return el;
    }

    appendChild(child) {
      if (child && (typeof child === "object" || typeof child === "function")) {
        child.parentNode = this;
      }
      return child;
    }
  }

  class Element extends EventTarget {
    constructor(tagName) {
      super(null);
      this.tagName = tagName ? String(tagName) : "";
      this.parentNode = null;
      this.ownerDocument = null;
    }

    appendChild(child) {
      if (child && (typeof child === "object" || typeof child === "function")) {
        child.parentNode = this;
      }
      return child;
    }
  }

  if (typeof g.Document !== "function") g.Document = Document;
  if (typeof g.Element !== "function") g.Element = Element;

  if (!g.document) {
    g.document = new Document();
  }
  if (!g.document.__listeners) {
    g.document.__listeners = new Map();
  }
  if (!("parent" in g.document)) {
    g.document.parent = null;
  }
  try {
    Object.setPrototypeOf(g.document, Document.prototype);
  } catch (_e) {
    // Ignore.
  }
  if (typeof g.document.createElement !== "function") {
    g.document.createElement = Document.prototype.createElement;
  }
  if (typeof g.document.appendChild !== "function") {
    g.document.appendChild = Document.prototype.appendChild;
  }
})();
"#;

const FASTR_REPORT_HOOK: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  g.__fastrender_wpt_report_called = false;
  g.__fastrender_wpt_report_json = null;

  g.__fastrender_wpt_report = function (payload) {
    g.__fastrender_wpt_report_called = true;
    try {
      g.__fastrender_wpt_report_json = JSON.stringify(payload);
    } catch (e) {
      g.__fastrender_wpt_report_json = null;
    }
  };
})();
"#;

#[derive(Debug, Clone)]
enum ScriptToRun {
  External(String),
  Inline(String),
}

fn parse_html_scripts(source: &str) -> Result<Vec<ScriptToRun>, RunError> {
  // Minimal (non-spec-compliant) extraction of script tags sufficient for the curated offline
  // corpus under `tests/wpt_dom/tests`.
  let script_re =
    Regex::new(r"(?is)<script([^>]*)>(.*?)</script>").map_err(|e| RunError::Js(e.to_string()))?;
  // `regex` does not support backreferences, so we match single-quoted and double-quoted values
  // separately.
  let src_re = Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
    .map_err(|e| RunError::Js(e.to_string()))?;

  let mut out = Vec::new();
  for cap in script_re.captures_iter(source) {
    let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
    let body = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
    if let Some(src_cap) = src_re.captures(attrs) {
      let src = src_cap
        .get(1)
        .or_else(|| src_cap.get(2))
        .map(|m| m.as_str())
        .unwrap_or_default();
      out.push(ScriptToRun::External(src.to_string()));
    } else {
      out.push(ScriptToRun::Inline(body.to_string()));
    }
  }
  Ok(out)
}

fn first_nonpass_message(subtests: &[crate::wpt_report::WptSubtest]) -> Option<String> {
  for st in subtests {
    if st.status != "pass" {
      if let Some(msg) = &st.message {
        if !msg.is_empty() {
          return Some(msg.clone());
        }
      }
    }
  }
  None
}
