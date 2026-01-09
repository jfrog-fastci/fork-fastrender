use crate::error::{Error, Result};
use crate::js::event_loop::{EventLoop, TimerId};
use crate::js::time::WebTime;
use crate::js::window_timers::{
  QUEUE_MICROTASK_NOT_CALLABLE_ERROR, QUEUE_MICROTASK_STRING_HANDLER_ERROR,
  SET_INTERVAL_NOT_CALLABLE_ERROR, SET_INTERVAL_STRING_HANDLER_ERROR, SET_TIMEOUT_NOT_CALLABLE_ERROR,
  SET_TIMEOUT_STRING_HANDLER_ERROR,
};
use rquickjs::{CatchResultExt, Coerced, Context, Ctx, CaughtError, Exception, Function, Runtime, Value};
use std::cell::Cell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

struct HostState {
  web_time: WebTime,
  now: Cell<Duration>,
  current_event_loop: Cell<Option<NonNull<EventLoop<JsVmHost>>>>,
}

/// Minimal JS host embedding backed by `rquickjs` (QuickJS).
///
/// This host installs deterministic timer and time APIs (`setTimeout`, `setInterval`, `Date.now`,
/// `performance.now`) that are wired into FastRender's [`EventLoop`].
///
/// The intent is that renderer tests (and eventually the browser embedding) can execute JS while
/// driving time deterministically via [`VirtualClock`](crate::js::VirtualClock).
pub struct JsVmHost {
  // Keep the runtime alive for the lifetime of the context.
  _rt: Runtime,
  ctx: Context,
  state: Rc<HostState>,
}

impl JsVmHost {
  pub fn new(web_time: WebTime) -> Result<Self> {
    let rt = Runtime::new().map_err(map_js_err)?;
    let ctx = Context::full(&rt).map_err(map_js_err)?;

    let state = Rc::new(HostState {
      web_time,
      now: Cell::new(Duration::from_millis(0)),
      current_event_loop: Cell::new(None),
    });

    let host = Self {
      _rt: rt,
      ctx,
      state,
    };
    host.install_bindings()?;
    Ok(host)
  }

  pub fn eval<T>(&self, code: &str) -> Result<T>
  where
    for<'js> T: rquickjs::FromJs<'js>,
  {
    self.ctx.with(|ctx| {
      let result = ctx.eval::<T, _>(code);
      result.catch(&ctx).map_err(map_caught_err)
    })
  }

  /// Execute a JS classic script while wiring the host's "current time" + event loop pointer.
  pub fn exec_script(&mut self, event_loop: &mut EventLoop<Self>, code: &str) -> Result<()> {
    self.enter_js(event_loop, |ctx| ctx.eval::<(), _>(code))
  }

  fn fire_timer(&mut self, event_loop: &mut EventLoop<Self>, id: TimerId) -> Result<()> {
    self.enter_js(event_loop, |ctx| {
      let globals = ctx.globals();
      let fire: Function = globals.get("__fr_fire_timer")?;
      fire.call::<(TimerId,), ()>((id,))?;
      Ok(())
    })
  }

  fn fire_microtask(&mut self, event_loop: &mut EventLoop<Self>, id: i32) -> Result<()> {
    self.enter_js(event_loop, |ctx| {
      let globals = ctx.globals();
      let fire: Function = globals.get("__fr_fire_microtask")?;
      fire.call::<(i32,), ()>((id,))?;
      Ok(())
    })
  }

  fn enter_js<R>(
    &mut self,
    event_loop: &mut EventLoop<Self>,
    f: impl for<'js> FnOnce(Ctx<'js>) -> rquickjs::Result<R>,
  ) -> Result<R> {
    // `event_loop` is already mutably borrowed by the currently-running task/microtask. If a JS
    // native function re-enters Rust (e.g. `setTimeout`) and tries to borrow the same event loop
    // mutably again, that would violate Rust's aliasing rules (UB).
    //
    // To keep the embedding sound, temporarily *move* the event loop out of the caller's `&mut`
    // reference and store a raw pointer to the owned value while JS is executing.
    let mut loop_owned = std::mem::take(event_loop);

    let previous_now = self.state.now.replace(loop_owned.now());
    let previous_loop = self
      .state
      .current_event_loop
      .replace(Some(NonNull::from(&mut loop_owned)));

    let result = self.ctx.with(|ctx| {
      let result = f(ctx.clone());
      result.catch(&ctx).map_err(map_caught_err)
    });

    self.state.current_event_loop.set(previous_loop);
    self.state.now.set(previous_now);
    *event_loop = loop_owned;
    result
  }

  fn install_bindings(&self) -> Result<()> {
    let state = Rc::clone(&self.state);
    self
      .ctx
      .with(|ctx| {
        let result: rquickjs::Result<()> = (|| {
          let globals = ctx.globals();

          // Host hooks used by the JS timer wrappers.
          globals.set(
            "__fr_schedule_timeout",
            Function::new(ctx.clone(), {
              let state = Rc::clone(&state);
              move |delay_ms: f64| -> rquickjs::Result<TimerId> {
                let event_loop = current_event_loop(&state)?;
                let delay = normalize_ms(delay_ms);
                let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
                let id_cell_for_cb = Rc::clone(&id_cell);
                let id = event_loop
                  .set_timeout(delay, move |host, event_loop| {
                    host.fire_timer(event_loop, id_cell_for_cb.get())
                  })
                  .map_err(|e| {
                    rquickjs::Error::new_from_js_message("EventLoop", "timer", e.to_string())
                  })?;
                id_cell.set(id);
                Ok(id)
              }
            })?,
          )?;

          globals.set(
            "__fr_schedule_interval",
            Function::new(ctx.clone(), {
              let state = Rc::clone(&state);
              move |interval_ms: f64| -> rquickjs::Result<TimerId> {
                let event_loop = current_event_loop(&state)?;
                let interval = normalize_ms(interval_ms);
                let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
                let id_cell_for_cb = Rc::clone(&id_cell);
                let id = event_loop
                  .set_interval(interval, move |host, event_loop| {
                    host.fire_timer(event_loop, id_cell_for_cb.get())
                  })
                  .map_err(|e| {
                    rquickjs::Error::new_from_js_message("EventLoop", "timer", e.to_string())
                  })?;
                id_cell.set(id);
                Ok(id)
              }
            })?,
          )?;

          globals.set(
            "__fr_clear_timer",
            Function::new(ctx.clone(), {
              let state = Rc::clone(&state);
              move |id: TimerId| -> rquickjs::Result<()> {
                let event_loop = current_event_loop(&state)?;
                event_loop.clear_timeout(id);
                Ok(())
              }
            })?,
          )?;

          globals.set(
            "__fr_queue_microtask",
            Function::new(ctx.clone(), {
              let state = Rc::clone(&state);
              move |id: i32| -> rquickjs::Result<()> {
                let event_loop = current_event_loop(&state)?;
                event_loop
                  .queue_microtask(move |host, event_loop| host.fire_microtask(event_loop, id))
                  .map_err(|e| {
                    rquickjs::Error::new_from_js_message("EventLoop", "microtask", e.to_string())
                  })?;
                Ok(())
              }
            })?,
          )?;

          // Deterministic time bindings backed by the event loop clock.
          globals.set(
            "__fr_date_now",
            Function::new(ctx.clone(), {
              let state = Rc::clone(&state);
              move || -> rquickjs::Result<f64> {
                let now = state.now.get();
                Ok(state.web_time.date_now_from_duration(now) as f64)
              }
            })?,
          )?;

          globals.set(
            "__fr_performance_now",
            Function::new(ctx.clone(), {
              let state = Rc::clone(&state);
              move || -> rquickjs::Result<f64> {
                let now = state.now.get();
                Ok(state.web_time.performance_now_from_duration(now))
              }
            })?,
          )?;

          // JS-visible wrappers + callback storage.
          let wrapper = format!(
            r#"(function () {{
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.__fr_fire_timer === "function") return;

  var timers = new Map(); // id -> {{ kind, cb, args }}
  var microtasks = new Map(); // id -> cb
  var next_microtask = 1;

  function normalizeDelay(ms) {{
    var n = Number(ms);
    if (!isFinite(n) || isNaN(n)) n = 0;
    if (n < 0) n = 0;
    return Math.floor(n);
  }}

  g.setTimeout = function (cb, ms /*, ...args */) {{
    if (typeof cb === "string") throw new TypeError("{set_timeout_string}");
    if (typeof cb !== "function") throw new TypeError("{set_timeout_not_callable}");
    var delay = normalizeDelay(ms);
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    var id = g.__fr_schedule_timeout(delay);
    timers.set(id, {{ kind: 0, cb: cb, args: args }});
    return id;
  }};

  g.clearTimeout = function (id) {{
    id = Number(id);
    if (!isFinite(id) || isNaN(id)) id = 0;
    id = id | 0;
    timers.delete(id);
    g.__fr_clear_timer(id);
  }};

  g.setInterval = function (cb, ms /*, ...args */) {{
    if (typeof cb === "string") throw new TypeError("{set_interval_string}");
    if (typeof cb !== "function") throw new TypeError("{set_interval_not_callable}");
    var interval = normalizeDelay(ms);
    var args = [];
    for (var i = 2; i < arguments.length; i++) args.push(arguments[i]);
    var id = g.__fr_schedule_interval(interval);
    timers.set(id, {{ kind: 1, cb: cb, args: args }});
    return id;
  }};

  g.clearInterval = g.clearTimeout;

  g.queueMicrotask = function (cb) {{
    if (typeof cb === "string") throw new TypeError("{queue_microtask_string}");
    if (typeof cb !== "function") throw new TypeError("{queue_microtask_not_callable}");
    var id = next_microtask++;
    // Store the callback before queueing so `__fr_fire_microtask` can find it, but ensure we
    // clean up if queueing fails (e.g. EventLoop microtask queue limit).
    microtasks.set(id, cb);
    try {{
      g.__fr_queue_microtask(id);
    }} catch (e) {{
      microtasks.delete(id);
      throw e;
    }}
  }};

  g.__fr_fire_timer = function (id) {{
    var entry = timers.get(id);
    if (!entry) return;
    if (entry.kind === 0) timers.delete(id);
    entry.cb.apply(g, entry.args);
  }};

  g.__fr_fire_microtask = function (id) {{
    var cb = microtasks.get(id);
    if (!cb) return;
    microtasks.delete(id);
    // HTML `queueMicrotask` invokes callbacks with an `undefined` callback-this value.
    cb.call(undefined);
  }};

  // Time APIs.
  // QuickJS defines `Date.now` as a read-only property. Instead of mutating the intrinsic Date
  // function object, replace the global binding with a delegating wrapper whose `.now()` is
  // deterministic (backed by FastRender's `EventLoop` clock).
  (function () {{
    var OriginalDate = g.Date;
    function PatchedDate() {{
      var args = Array.prototype.slice.call(arguments);
      if (new.target) {{
        return Reflect.construct(OriginalDate, args, new.target);
      }}
      return OriginalDate.apply(this, args);
    }}
    PatchedDate.prototype = OriginalDate.prototype;
    Object.setPrototypeOf(PatchedDate, OriginalDate);
    Object.defineProperty(PatchedDate, "now", {{
      value: g.__fr_date_now,
      writable: true,
      configurable: true,
    }});
    try {{
      g.Date = PatchedDate;
    }} catch (e) {{
      try {{
        Object.defineProperty(g, "Date", {{
          value: PatchedDate,
          writable: true,
          configurable: true,
        }});
      }} catch (e2) {{}}
    }}
  }})();

  (function () {{
    var basePerf = g.performance;
    if (!basePerf) basePerf = {{}};
    var PatchedPerf = Object.create(basePerf);
    Object.defineProperty(PatchedPerf, "now", {{
      value: g.__fr_performance_now,
      writable: true,
      configurable: true,
    }});
    Object.defineProperty(PatchedPerf, "timeOrigin", {{
      value: {time_origin},
      writable: false,
      configurable: true,
    }});
    try {{
      g.performance = PatchedPerf;
    }} catch (e) {{
      try {{
        Object.defineProperty(g, "performance", {{
          value: PatchedPerf,
          writable: true,
          configurable: true,
        }});
      }} catch (e2) {{}}
    }}
  }})();
 }})();"#,
            set_timeout_string = SET_TIMEOUT_STRING_HANDLER_ERROR,
             set_timeout_not_callable = SET_TIMEOUT_NOT_CALLABLE_ERROR,
             set_interval_string = SET_INTERVAL_STRING_HANDLER_ERROR,
             set_interval_not_callable = SET_INTERVAL_NOT_CALLABLE_ERROR,
             queue_microtask_string = QUEUE_MICROTASK_STRING_HANDLER_ERROR,
             queue_microtask_not_callable = QUEUE_MICROTASK_NOT_CALLABLE_ERROR,
             time_origin = state.web_time.time_origin_unix_ms
           );

          ctx.eval::<(), _>(wrapper)?;
          Ok(())
        })();

        result.catch(&ctx).map_err(map_caught_err)
      })?;
    Ok(())
  }
}

fn normalize_ms(ms: f64) -> Duration {
  if !ms.is_finite() || ms.is_nan() || ms <= 0.0 {
    return Duration::from_millis(0);
  }
  // Mirror browser timers: delay is clamped to >= 0 and treated as an integer.
  let ms = ms.floor();
  let ms = if ms >= u64::MAX as f64 {
    u64::MAX
  } else {
    ms as u64
  };
  Duration::from_millis(ms)
}

fn current_event_loop(state: &HostState) -> rquickjs::Result<&mut EventLoop<JsVmHost>> {
  let Some(ptr) = state.current_event_loop.get() else {
    return Err(rquickjs::Error::new_from_js_message(
      "FastRender",
      "EventLoop",
      "JS host is not currently executing inside an EventLoop task",
    ));
  };
  // Safety: `current_event_loop` is set only while `JsVmHost::enter_js` is executing. All JS-visible
  // host functions are invoked synchronously on that stack, so the pointer is valid and unique.
  Ok(unsafe { &mut *ptr.as_ptr() })
}

fn map_js_err(err: rquickjs::Error) -> Error {
  // rquickjs's `Debug` representation for `Error::Exception` is just "Exception" which loses the
  // underlying JS error message. `Display` preserves the exception text (e.g. "TypeError: ...").
  Error::Other(err.to_string())
}

fn map_caught_err(err: CaughtError<'_>) -> Error {
  match err {
    CaughtError::Error(err) => map_js_err(err),
    CaughtError::Exception(exception) => Error::Other(format_exception(&exception)),
    CaughtError::Value(value) => Error::Other(format!("Uncaught exception: {}", format_value(&value))),
  }
}

fn format_exception(exception: &Exception<'_>) -> String {
  // `Exception::message` returns the raw `error.message` string without the error type/name.
  // Include `name` so callers see e.g. `TypeError: ...`.
  let name: Option<String> = exception.as_object().get("name").ok();
  let message = exception.message();
  let mut out = match (name, message) {
    (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
    (Some(name), _) => name,
    (None, Some(message)) => message,
    (None, None) => "Uncaught exception".to_string(),
  };

  if let Some(stack) = exception.stack() {
    if !stack.is_empty() {
      out.push('\n');
      out.push_str(&stack);
    }
  }
  out
}

fn format_value(value: &Value<'_>) -> String {
  // Best-effort `ToString` coercion for non-Error throws (`throw 3`, `throw "x"`).
  match value.get::<Coerced<String>>() {
    Ok(Coerced(str)) => str,
    Err(_) => format!("<{}>", value.type_name()),
  }
}
