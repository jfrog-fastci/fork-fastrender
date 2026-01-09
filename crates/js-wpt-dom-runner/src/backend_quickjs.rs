use crate::backend::{Backend, BackendInit, BackendReport};
use crate::dom_bindings::install_dom_bindings;
use crate::timer_event_loop::{QueueLimits, TimerEventLoop, TimerExecution};
use crate::RunError;
use rquickjs::{Context, Function, Object, Runtime};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
 
pub struct QuickJsBackend {
  rt: Option<Runtime>,
  ctx: Option<Context>,
  timer_loop: Option<Rc<RefCell<TimerEventLoop>>>,
  deadline: Option<Instant>,
  timed_out: bool,
  max_tasks: usize,
  max_microtasks: usize,
  tasks_executed: usize,
  microtasks_executed: usize,
  report_taken: bool,
}
 
impl Default for QuickJsBackend {
  fn default() -> Self {
    Self {
      rt: None,
      ctx: None,
      timer_loop: None,
      deadline: None,
      timed_out: false,
      max_tasks: 0,
      max_microtasks: 0,
      tasks_executed: 0,
      microtasks_executed: 0,
      report_taken: false,
    }
  }
}
 
impl QuickJsBackend {
  pub fn new() -> Self {
    Self::default()
  }
 
  fn rt(&self) -> &Runtime {
    self.rt.as_ref().expect("backend runtime must be initialized")
  }
 
  fn ctx(&self) -> &Context {
    self.ctx.as_ref().expect("backend context must be initialized")
  }
 
  fn timer_loop(&self) -> &Rc<RefCell<TimerEventLoop>> {
    self
      .timer_loop
      .as_ref()
      .expect("backend timer loop must be initialized")
  }
}
 
impl Backend for QuickJsBackend {
  fn init_realm(&mut self, init: BackendInit) -> Result<(), RunError> {
    let timer_loop = Rc::new(RefCell::new(TimerEventLoop::new(QueueLimits::default())));
    let rt = Runtime::new().map_err(|e| RunError::Js(e.to_string()))?;
 
    // Interrupt handler for per-test wall-time.
    let deadline = Instant::now() + init.timeout;
    let deadline_arc = Arc::new(deadline);
    rt.set_interrupt_handler(Some(Box::new({
      let deadline = Arc::clone(&deadline_arc);
      move || Instant::now() >= *deadline
    })));
 
    let ctx = Context::full(&rt).map_err(|e| RunError::Js(e.to_string()))?;
    let test_url = init.test_url;
 
    ctx.with(|ctx| -> Result<(), RunError> {
      let globals = ctx.globals();
      install_window_shims(ctx.clone(), &globals, &test_url)?;
      install_timer_host_fns(ctx.clone(), &globals, Rc::clone(&timer_loop))?;
 
      // Install JS shims for timers + EventTarget.
      ctx
        .eval::<(), _>(HOST_SHIMS)
        .map_err(|e| RunError::Js(e.to_string()))?;

      // DOM bindings (mutations + selectors) for the curated WPT subset.
      //
      // Installed after `HOST_SHIMS` so we can patch its `Document`/`Element` shims rather than
      // replacing them (which would break event tests).
      install_dom_bindings(ctx.clone(), &globals).map_err(|e| RunError::Js(e.to_string()))?;

      // Define the host hook used by `fastrender_testharness_report.js` to emit a result payload.
      ctx
        .eval::<(), _>(FASTR_REPORT_HOOK)
        .map_err(|e| RunError::Js(e.to_string()))?;
 
      Ok(())
    })?;
 
    self.deadline = Some(deadline);
    self.rt = Some(rt);
    self.ctx = Some(ctx);
    self.timer_loop = Some(timer_loop);
    self.timed_out = false;
    self.max_tasks = init.max_tasks;
    self.max_microtasks = init.max_microtasks;
    self.tasks_executed = 0;
    self.microtasks_executed = 0;
    self.report_taken = false;
    Ok(())
  }
 
  fn eval_script(&mut self, source: &str) -> Result<(), RunError> {
    self
      .ctx()
      .with(|ctx| ctx.eval::<(), _>(source).map_err(|e| RunError::Js(e.to_string())))
  }
 
  fn drain_microtasks(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    loop {
      if self.microtasks_executed >= self.max_microtasks {
        self.timed_out = true;
        return Ok(());
      }
      match self.rt().execute_pending_job() {
        Ok(true) => {
          self.microtasks_executed += 1;
          continue;
        }
        Ok(false) => return Ok(()),
        Err(err) => return Err(RunError::Js(err.to_string())),
      }
    }
  }
 
  fn poll_event_loop(&mut self) -> Result<bool, RunError> {
    if self.timed_out {
      return Ok(false);
    }
    if self.tasks_executed >= self.max_tasks {
      self.timed_out = true;
      return Ok(false);
    }

    enum NextAction {
      Task(crate::timer_event_loop::TimerTask),
      AdvancedTime,
      Idle,
    }

    let next_action = {
      let mut loop_state = self.timer_loop().borrow_mut();
      loop_state
        .queue_due_timers()
        .map_err(|e| RunError::Js(e.to_string()))?;

      if let Some(task) = loop_state.pop_task() {
        NextAction::Task(task)
      } else if let Some(due) = loop_state.next_timer_due() {
        if due > loop_state.now() {
          loop_state.advance_to(due);
        }
        NextAction::AdvancedTime
      } else {
        NextAction::Idle
      }
    };

    let task = match next_action {
      NextAction::Task(task) => task,
      NextAction::AdvancedTime => return Ok(true),
      NextAction::Idle => {
        self.timed_out = true;
        return Ok(false);
      }
    };
 
    self.tasks_executed += 1;
    if self.tasks_executed > self.max_tasks {
      self.timed_out = true;
      return Ok(false);
    }
 
    let exec: Option<TimerExecution> = self.timer_loop().borrow_mut().begin_timer_task(task);
    let Some(exec) = exec else {
      return Ok(true);
    };
    let prev_timer_nesting_level = exec.prev_timer_nesting_level;
 
    let invoke_res = self.ctx().with(|ctx| -> Result<(), RunError> {
      let globals = ctx.globals();
      let invoke: Function = globals
        .get("__fastrender_invoke_timer")
        .map_err(|e| RunError::Js(e.to_string()))?;
      invoke
        .call::<(i32,), ()>((exec.id,))
        .map_err(|e| RunError::Js(e.to_string()))?;
      Ok(())
    });
    if let Err(err) = invoke_res {
      if let RunError::Js(msg) = &err {
        if is_interrupt_error(msg) {
          self.timed_out = true;
        }
      }
      return Err(err);
    }
 
    self.timer_loop().borrow_mut().finish_timer_task(exec);
 
    // Drain microtasks before restoring timer nesting level; timers scheduled by microtasks should
    // see the incremented nesting level.
    let drain_res = self.drain_microtasks();
    self
      .timer_loop()
      .borrow_mut()
      .set_timer_nesting_level(prev_timer_nesting_level);
    drain_res?;
 
    Ok(true)
  }
 
  fn take_report(&mut self) -> Result<Option<BackendReport>, RunError> {
    if self.report_taken {
      return Ok(None);
    }
    let report = self.ctx().with(|ctx| -> Result<Option<BackendReport>, RunError> {
      let globals = ctx.globals();
      let did_report: Option<bool> = globals
        .get("__fastrender_wpt_report_called")
        .map_err(|e| RunError::Js(e.to_string()))?;
      if !did_report.unwrap_or(false) {
        return Ok(None);
      }
 
      let json: Option<String> = globals
        .get("__fastrender_wpt_report_json")
        .map_err(|e| RunError::Js(e.to_string()))?;
      let Some(json) = json else {
        return Err(RunError::Js(
          "missing fastrender testharness report".to_string(),
        ));
      };
      let parsed: BackendReport = serde_json::from_str(&json).map_err(|err| {
        RunError::Js(format!("failed to parse fastrender testharness report: {err}"))
      })?;
      Ok(Some(parsed))
    })?;
 
    if report.is_some() {
      self.report_taken = true;
    }
    Ok(report)
  }
 
  fn is_timed_out(&self) -> bool {
    self.timed_out || self.deadline.is_some_and(|deadline| Instant::now() >= deadline)
  }
 
  fn idle_wait(&mut self) {
    // QuickJS backend uses a deterministic virtual timer queue; no wall-clock sleeping is needed.
  }
}
 
fn is_interrupt_error(msg: &str) -> bool {
  msg.contains("interrupted") || msg.contains("Interrupt")
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
      timer_loop.set_timeout(delay).map_err(|e| {
        rquickjs::Error::new_from_js_message("TimerEventLoop", "TimerId", e.to_string())
      })
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
      timer_loop.set_interval(interval).map_err(|e| {
        rquickjs::Error::new_from_js_message("TimerEventLoop", "TimerId", e.to_string())
      })
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
