use crate::backend::{Backend, BackendInit};
use crate::wpt_fs::WptFs;
use crate::wpt_report::{WptReport, WptSubtest};
use crate::wpt_resource_fetcher::WptResourceFetcher;
use crate::RunError;
use fastrender::debug::inspect::{inspect, InspectQuery};
use fastrender::js::DomHost;
use fastrender::js::webidl::VmJsWebIdlBindingsHostDispatch;
use fastrender::js::window_timers::VmJsEventLoopHooks;
use fastrender::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard, install_window_timers_bindings,
  install_window_xhr_bindings_with_guard, EventLoop, JsExecutionOptions, MicrotaskCheckpointLimitedOutcome,
  RunLimits, RunNextTaskLimitedOutcome, RunState, VirtualClock, WindowFetchBindings, WindowFetchEnv,
  WindowRealm, WindowRealmConfig, WindowRealmHost, WindowXhrBindings, WindowXhrEnv,
};
use fastrender::js::window_realm::DomBindingsBackend;
use fastrender::resource::origin_from_url;
use fastrender::{BrowserDocumentDom2, FastRender, RenderOptions};
use std::sync::Arc;
use std::time::Duration;
use vm_js::{
  GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};
use webidl_vm_js::VmJsHostHooksPayload;

pub(crate) fn is_available() -> bool {
  true
}

// Opt-in WebIDL DOM backend selection for the vm-js-rendered runner.
//
// Default remains `handwritten` so existing CI expectations are not impacted.
const DOM_BINDINGS_BACKEND_ENV_VAR: &str = "FASTERENDER_WPT_DOM_BINDINGS_BACKEND";

fn dom_bindings_backend_from_env() -> Result<DomBindingsBackend, RunError> {
  let Ok(raw) = std::env::var(DOM_BINDINGS_BACKEND_ENV_VAR) else {
    return Ok(DomBindingsBackend::Handwritten);
  };
  let value = raw.trim().to_ascii_lowercase();
  match value.as_str() {
    "" | "handwritten" => Ok(DomBindingsBackend::Handwritten),
    "webidl" | "web-idl" | "web_idl" => Ok(DomBindingsBackend::WebIdl),
    other => Err(RunError::Js(format!(
      "invalid {DOM_BINDINGS_BACKEND_ENV_VAR}={other:?} (expected handwritten|webidl)"
    ))),
  }
}

struct RenderedHost {
  document: BrowserDocumentDom2,
  realm: WindowRealm,
  _fetch_bindings: WindowFetchBindings,
  _xhr_bindings: WindowXhrBindings,
  webidl_bindings_host: VmJsWebIdlBindingsHostDispatch<RenderedHost>,
  document_url: String,
}

impl DomHost for RenderedHost {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&fastrender::dom2::Document) -> R,
  {
    <BrowserDocumentDom2 as DomHost>::with_dom(&self.document, f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut fastrender::dom2::Document) -> (R, bool),
  {
    <BrowserDocumentDom2 as DomHost>::mutate_dom(&mut self.document, f)
  }
}
impl WindowRealmHost for RenderedHost {
  fn vm_host_and_window_realm(
    &mut self,
  ) -> fastrender::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
    Ok((&mut self.document, &mut self.realm))
  }

  fn webidl_bindings_host(&mut self) -> Option<&mut dyn webidl_vm_js::WebIdlBindingsHost> {
    Some(&mut self.webidl_bindings_host)
  }
}

/// `vm-js` backend executed against a renderer-backed `BrowserDocumentDom2`.
///
/// This exists primarily so layout/geometry-sensitive WPT tests (e.g. future IntersectionObserver /
/// ResizeObserver coverage) can run with real layout artifacts rather than the renderer-less
/// `WindowHostState` DOM.
pub struct VmJsRenderedBackend {
  fs: WptFs,

  host: Option<RenderedHost>,
  event_loop: Option<EventLoop<RenderedHost>>,
  virtual_clock: Option<Arc<VirtualClock>>,
  run_state: Option<RunState>,

  deadline: Option<Duration>,
  timed_out: bool,
}

impl VmJsRenderedBackend {
  pub fn new(fs: WptFs) -> Self {
    Self {
      fs,
      host: None,
      event_loop: None,
      virtual_clock: None,
      run_state: None,
      deadline: None,
      timed_out: false,
    }
  }

  fn state_mut(&mut self) -> Result<(&mut RenderedHost, &mut EventLoop<RenderedHost>, &mut RunState), RunError> {
    let host = self
      .host
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js-rendered backend is not initialized".to_string()))?;
    let event_loop = self
      .event_loop
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js-rendered backend is not initialized".to_string()))?;
    let run_state = self
      .run_state
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js-rendered backend is not initialized".to_string()))?;
    Ok((host, event_loop, run_state))
  }

  fn host_mut(&mut self) -> Result<&mut RenderedHost, RunError> {
    self
      .host
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js-rendered backend is not initialized".to_string()))
  }

  fn virtual_now(&self) -> Option<Duration> {
    self.virtual_clock.as_ref().map(|c| c.now())
  }

  fn handle_fastrender_error_as_timeout_or_js(&mut self, err: fastrender::error::Error) -> Result<(), RunError> {
    let msg = err.to_string();
    if is_vm_interrupt_message(&msg) {
      self.timed_out = true;
      Ok(())
    } else {
      Err(RunError::Js(msg))
    }
  }

  fn handle_vm_error_as_timeout_or_js(&mut self, err: VmError) -> Result<(), RunError> {
    let msg = err.to_string();
    if is_vm_interrupt_message(&msg) {
      self.timed_out = true;
      Ok(())
    } else {
      Err(RunError::Js(msg))
    }
  }

  fn render_if_needed(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    let host = self.host_mut()?;
    host
      .document
      .render_if_needed()
      .map_err(|err| RunError::Js(err.to_string()))?;
    Ok(())
  }

  fn perform_microtask_checkpoint_limited(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(());
    }

    let outcome = {
      let (host, event_loop, run_state) = self.state_mut()?;
      event_loop.perform_microtask_checkpoint_limited(host, run_state)
    };

    match outcome {
      Ok(MicrotaskCheckpointLimitedOutcome::Completed) => {
        // Rendering after microtasks keeps layout artifacts in sync for geometry-dependent tests.
        self.render_if_needed()
      }
      Ok(MicrotaskCheckpointLimitedOutcome::Stopped(_reason)) => {
        self.timed_out = true;
        Ok(())
      }
      Err(err) => self.handle_fastrender_error_as_timeout_or_js(err),
    }
  }
}

impl Backend for VmJsRenderedBackend {
  fn init_realm(
    &mut self,
    init: BackendInit,
    _host: Option<&mut dyn crate::engine::HostEnvironment>,
  ) -> Result<(), RunError> {
    self.deadline = Some(init.timeout);
    self.timed_out = false;

    // Deterministic virtual time (starts at 0).
    let virtual_clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<RenderedHost>::with_clock(virtual_clock.clone());

    // Offline-only fetcher mapped to the local curated WPT corpus.
    let fetcher = Arc::new(WptResourceFetcher::from_wpt_fs(&self.fs));

    // JS execution options tuned for deterministic test runs:
    // - disable wall-clock based deadlines (avoid CI flakiness),
    // - keep an instruction/fuel budget so `while(true){}` terminates deterministically.
    let mut options = JsExecutionOptions::default();
    options.event_loop_run_limits.max_wall_time = None;
    options.max_instruction_count = Some(50_000);
    options.supports_module_scripts = true;

    event_loop.set_queue_limits(options.event_loop_queue_limits);

    // Create a renderer-backed document.
    let renderer = FastRender::builder()
      .fetcher(fetcher.clone())
      .dom_scripting_enabled(true)
      .font_sources(fastrender::text::font_db::FontConfig::bundled_only())
      .build()
      .map_err(|err| RunError::Js(err.to_string()))?;

    let mut render_options = RenderOptions::default();
    render_options.viewport = Some((800, 600));

    // Create an HTML-ish DOM with <html><head> and <body> so curated tests can assert:
    // `document.head.tagName === "HEAD"` etc.
    let mut document = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><head></head><body></body></html>",
      render_options,
    )
    .map_err(|err| RunError::Js(err.to_string()))?;
    document.set_animation_clock(virtual_clock.clone());

    // Create the WindowRealm (vm-js realm with Window-like globals).
    let dom_bindings_backend = dom_bindings_backend_from_env()?;
    let mut realm = WindowRealm::new_with_js_execution_options(
      WindowRealmConfig::new(init.test_url.clone())
        .with_clock(virtual_clock.clone())
        .with_dom_bindings_backend(dom_bindings_backend),
      options,
    )
    .map_err(|err| RunError::Js(err.to_string()))?;
    realm.set_cookie_fetcher(fetcher.clone());
    realm.set_resource_fetcher(fetcher.clone());
    if realm.js_execution_options().supports_module_scripts {
      let origin = origin_from_url(&init.test_url);
      realm
        .enable_module_loader(fetcher.clone(), origin)
        .map_err(|err| RunError::Js(err.to_string()))?;
    }

    // Install EventLoop-backed Web APIs (`setTimeout`, `queueMicrotask`, `requestAnimationFrame`, `fetch`).
    let (fetch_bindings, xhr_bindings) = {
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      install_window_timers_bindings::<RenderedHost>(vm, realm_ref, heap)
        .map_err(|err| RunError::Js(err.to_string()))?;
      install_window_animation_frame_bindings::<RenderedHost>(vm, realm_ref, heap)
        .map_err(|err| RunError::Js(err.to_string()))?;
      let fetch_bindings = install_window_fetch_bindings_with_guard::<RenderedHost>(
        vm,
        realm_ref,
        heap,
        WindowFetchEnv::for_document(fetcher.clone(), Some(init.test_url.clone())),
      )
      .map_err(|err| RunError::Js(err.to_string()))?;
      let xhr_bindings = install_window_xhr_bindings_with_guard::<RenderedHost>(
        vm,
        realm_ref,
        heap,
        WindowXhrEnv::for_document(fetcher.clone(), Some(init.test_url.clone())),
      )
      .map_err(|err| RunError::Js(err.to_string()))?;

      (fetch_bindings, xhr_bindings)
    };

    let webidl_bindings_host = VmJsWebIdlBindingsHostDispatch::<RenderedHost>::new(realm.global_object());

    let mut host = RenderedHost {
      document,
      realm,
      _fetch_bindings: fetch_bindings,
      _xhr_bindings: xhr_bindings,
      webidl_bindings_host,
      document_url: init.test_url.clone(),
    };

    install_import_map_register_hook(&mut host).map_err(|err| RunError::Js(err.to_string()))?;
    install_layout_inspection_hooks(&mut host).map_err(|err| RunError::Js(err.to_string()))?;

    let run_state = event_loop.new_run_state(RunLimits {
      max_tasks: init.max_tasks,
      max_microtasks: init.max_microtasks,
      max_wall_time: None,
    });

    self.virtual_clock = Some(virtual_clock);
    self.event_loop = Some(event_loop);
    self.host = Some(host);
    self.run_state = Some(run_state);

    // Install runner-specific globals:
    // - `__fastrender_wpt_report(payload)` stores the first payload for `take_report`.
    // - `__fastrender_resolve_url(input, base)` (legacy helper used by curated tests).
    // - `__fastrender_register_import_map(json)` delegates to the native import map hook.
    const BOOTSTRAP: &str = r#"
      (function () {
        var g = typeof globalThis !== "undefined" ? globalThis : this;

        // Single-shot report hook used by `resources/fastrender_testharness_report.js`.
        try { g.__fastrender_wpt_report_called = false; } catch (_e) {}
        try { g.__fastrender_wpt_report_payload = undefined; } catch (_e2) {}

        // Define via `var` so very small runtimes (historically) could resolve it as an identifier
        // binding, while also reflecting it on `globalThis`.
        var __fastrender_wpt_report = function (payload) {
          try {
            if (g.__fastrender_wpt_report_called) return;
            g.__fastrender_wpt_report_called = true;
            g.__fastrender_wpt_report_payload = payload;
          } catch (_e3) {}
        };
        try { g.__fastrender_wpt_report = __fastrender_wpt_report; } catch (_e4) {}

        // Legacy URL resolver used by curated tests and compatibility shims.
        //
        // Note: `base` is treated as a required argument once present (mirrors WebIDL where
        // `base: USVString?` is *not* nullable). Curated tests assert that `base === null`
        // throws a real TypeError.
        var __fastrender_resolve_url = function (input, base) {
          if (base === null) {
            throw new TypeError("Invalid base URL");
          }
          if (base === undefined) {
            return (new URL(input)).href;
          }
          return (new URL(input, base)).href;
        };
        try { g.__fastrender_resolve_url = __fastrender_resolve_url; } catch (_e5) {}

        // Test-only helper for registering import maps from within curated WPT tests.
        var __fastrender_register_import_map = function (json) {
          if (json === null) {
            throw new TypeError("import map JSON must not be null");
          }
          if (typeof json !== "string") {
            json = JSON.stringify(json);
          }
          return g.__fastrender_register_import_map_native(json);
        };
        try { g.__fastrender_register_import_map = __fastrender_register_import_map; } catch (_e6) {}

        // --- Test-only layout/geometry helpers (vmjs-rendered backend only) ---
        //
        // The upstream WPT IntersectionObserver/ResizeObserver suites are layout-sensitive. FastRender
        // does not yet ship a full spec implementation of these observers, but we still want local
        // tests to be able to validate that:
        // - layout/paint runs,
        // - geometry is non-zero and consistent,
        // - and observer-like callbacks can be scheduled deterministically.
        //
        // These minimal polyfills are installed only when the platform API is missing so future
        // native implementations can take precedence.
        function __fastrender_viewport_rect() {
          var w = (typeof g.innerWidth === "number" && isFinite(g.innerWidth)) ? g.innerWidth : 800;
          var h = (typeof g.innerHeight === "number" && isFinite(g.innerHeight)) ? g.innerHeight : 600;
          return { x: 0, y: 0, width: w, height: h };
        }
        try { g.__fastrender_viewport_rect = __fastrender_viewport_rect; } catch (_e7) {}

        function __fastrender_intersect_rect(a, b) {
          var x1 = Math.max(a.x, b.x);
          var y1 = Math.max(a.y, b.y);
          var x2 = Math.min(a.x + a.width, b.x + b.width);
          var y2 = Math.min(a.y + a.height, b.y + b.height);
          return { x: x1, y: y1, width: Math.max(0, x2 - x1), height: Math.max(0, y2 - y1) };
        }

        if (typeof g.ResizeObserver === "undefined" && typeof g.__fastrender_get_rect_by_id === "function") {
          g.ResizeObserver = class ResizeObserver {
            constructor(callback) {
              if (typeof callback !== "function") throw new TypeError("ResizeObserver callback must be callable");
              this._callback = callback;
              this._targets = [];
              this._scheduled = false;
            }
            observe(target) {
              if (!target) return;
              if (this._targets.indexOf(target) === -1) this._targets.push(target);
              this._schedule();
            }
            unobserve(target) {
              var i = this._targets.indexOf(target);
              if (i !== -1) this._targets.splice(i, 1);
            }
            disconnect() { this._targets = []; }
            _schedule() {
              if (this._scheduled) return;
              this._scheduled = true;
              var self = this;
              setTimeout(function () {
                self._scheduled = false;
                var entries = [];
                for (var i = 0; i < self._targets.length; i++) {
                  var t = self._targets[i];
                  var id = t && typeof t.id === "string" ? t.id : "";
                  if (!id) continue;
                  var rect = g.__fastrender_get_rect_by_id(id);
                  if (!rect) continue;
                  entries.push({ target: t, contentRect: rect });
                }
                if (entries.length > 0) self._callback(entries, self);
              }, 0);
            }
          };
        }

        if (typeof g.IntersectionObserver === "undefined" && typeof g.__fastrender_get_rect_by_id === "function") {
          g.IntersectionObserver = class IntersectionObserver {
            constructor(callback, options) {
              if (typeof callback !== "function") throw new TypeError("IntersectionObserver callback must be callable");
              this._callback = callback;
              this._options = options || {};
              this._targets = [];
              this._scheduled = false;
            }
            observe(target) {
              if (!target) return;
              if (this._targets.indexOf(target) === -1) this._targets.push(target);
              this._schedule();
            }
            unobserve(target) {
              var i = this._targets.indexOf(target);
              if (i !== -1) this._targets.splice(i, 1);
            }
            disconnect() { this._targets = []; }
            takeRecords() { return []; }
            _schedule() {
              if (this._scheduled) return;
              this._scheduled = true;
              var self = this;
              setTimeout(function () {
                self._scheduled = false;
                var entries = [];
                var rootBounds = __fastrender_viewport_rect();
                for (var i = 0; i < self._targets.length; i++) {
                  var t = self._targets[i];
                  var id = t && typeof t.id === "string" ? t.id : "";
                  if (!id) continue;
                  var rect = g.__fastrender_get_rect_by_id(id);
                  if (!rect) continue;
                  var intersectionRect = __fastrender_intersect_rect(rect, rootBounds);
                  var isIntersecting = intersectionRect.width > 0 && intersectionRect.height > 0;
                  var area = rect.width * rect.height;
                  var intersectionArea = intersectionRect.width * intersectionRect.height;
                  var ratio = area > 0 ? (intersectionArea / area) : 0;
                  entries.push({
                    target: t,
                    boundingClientRect: rect,
                    rootBounds: rootBounds,
                    intersectionRect: intersectionRect,
                    isIntersecting: isIntersecting,
                    intersectionRatio: ratio,
                    time: 0
                  });
                }
                if (entries.length > 0) self._callback(entries, self);
              }, 0);
            }
          };
        }
      })();
    "#;

    self.eval_script(BOOTSTRAP, "fastrender_wpt_bootstrap.js")?;

    // Render an initial frame so layout artifacts exist before tests query geometry.
    self.render_if_needed()?;

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

    let (exec_result, finish_err) = {
      let (host, event_loop, _run_state) = self.state_mut()?;

      // IMPORTANT: use `new_with_host` so `VmJsHostHooksPayload` exposes the embedder state. This is
      // required by runner-installed native helpers like `__fastrender_register_import_map_native`.
      let mut hooks = VmJsEventLoopHooks::<RenderedHost>::new_with_host(host)
        .map_err(|err| RunError::Js(err.to_string()))?;
      hooks.set_event_loop(event_loop);

      let (vm_host, window_realm) = host
        .vm_host_and_window_realm()
        .map_err(|err| RunError::Js(err.to_string()))?;
      let exec_result =
        window_realm.exec_script_with_name_and_host_and_hooks(vm_host, &mut hooks, name, source);

      let finish_err = hooks.finish(window_realm.heap_mut());
      (exec_result, finish_err)
    };

    if let Some(err) = finish_err {
      return self.handle_fastrender_error_as_timeout_or_js(err);
    }

    match exec_result {
      Ok(_value) => {
        // Microtask checkpoint after every script evaluation.
        self.perform_microtask_checkpoint_limited()?;
        // Rendering after each script keeps layout artifacts current for subsequent task turns.
        self.render_if_needed()
      }
      Err(err) => self.handle_vm_error_as_timeout_or_js(err),
    }
  }

  fn drain_microtasks(&mut self) -> Result<(), RunError> {
    self.perform_microtask_checkpoint_limited()
  }

  fn poll_event_loop(&mut self) -> Result<bool, RunError> {
    if self.timed_out {
      return Ok(false);
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(false);
    }

    let outcome = {
      let (host, event_loop, run_state) = self.state_mut()?;
      event_loop.run_next_task_limited(host, run_state)
    };

    let ran = match outcome {
      Ok(RunNextTaskLimitedOutcome::Ran) => true,
      Ok(RunNextTaskLimitedOutcome::NoTask) => false,
      Ok(RunNextTaskLimitedOutcome::Stopped(_reason)) => {
        self.timed_out = true;
        false
      }
      Err(err) => {
        self.handle_fastrender_error_as_timeout_or_js(err)?;
        false
      }
    };

    if ran {
      self.render_if_needed()?;
    }

    Ok(ran)
  }

  fn take_report(&mut self) -> Result<Option<WptReport>, RunError> {
    let host = self.host_mut()?;
    let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let global = realm.global_object();
    if let Err(err) = scope.push_root(Value::Object(global)) {
      return Ok(Some(harness_error_report(format!(
        "failed to root global object while reading report payload: {err}"
      ))));
    }

    let payload_key = match alloc_key(&mut scope, "__fastrender_wpt_report_payload") {
      Ok(key) => key,
      Err(err) => {
        return Ok(Some(harness_error_report(format!(
          "failed to allocate report payload key: {err}"
        ))))
      }
    };

    let payload = match vm.get(&mut scope, global, payload_key) {
      Ok(value) => value,
      Err(err) => {
        return Ok(Some(harness_error_report(format!(
          "failed to read report payload: {err}"
        ))))
      }
    };

    if matches!(payload, Value::Undefined | Value::Null) {
      return Ok(None);
    }

    let report = report_from_js_value(vm, &mut scope, payload);

    // Clear the stored payload so subsequent `take_report` calls return `None`.
    let _ = scope.ordinary_set(vm, global, payload_key, Value::Undefined, Value::Object(global));

    Ok(Some(report))
  }

  fn is_timed_out(&self) -> bool {
    if self.timed_out {
      return true;
    }
    let Some(deadline) = self.deadline else {
      return true;
    };
    let Some(now) = self.virtual_now() else {
      return true;
    };
    if now >= deadline {
      return true;
    }

    let Some(run_state) = self.run_state.as_ref() else {
      return true;
    };
    let limits = run_state.limits();
    run_state.tasks_executed() >= limits.max_tasks || run_state.microtasks_executed() >= limits.max_microtasks
  }

  fn idle_wait(&mut self) {
    if self.timed_out {
      return;
    }
    let Some(deadline) = self.deadline else {
      self.timed_out = true;
      return;
    };
    let Some(clock) = self.virtual_clock.as_ref() else {
      self.timed_out = true;
      return;
    };
    let Some(event_loop) = self.event_loop.as_mut() else {
      self.timed_out = true;
      return;
    };

    let now = clock.now();
    let next_due = event_loop.next_timer_due_time();
    let target = match next_due {
      Some(due) if due > now => due.min(deadline),
      Some(_due) => now,
      None => deadline,
    };

    if target > now {
      clock.set_now(target);
    } else if now < deadline {
      // Force progress to the deadline so the runner doesn't spin forever if nothing becomes
      // runnable (defensive against unexpected event-loop states).
      clock.set_now(deadline);
    }

    if clock.now() >= deadline {
      self.timed_out = true;
    }
  }
}

fn is_vm_interrupt_message(msg: &str) -> bool {
  // `vm-js` termination messages are currently formatted as:
  // - "execution terminated: out of fuel"
  // - "execution terminated: deadline exceeded"
  // - "execution terminated: interrupted"
  //
  // Keep this matching permissive so runner-level timeout classification remains robust even if
  // formatting changes.
  let msg = msg.to_ascii_lowercase();
  msg.contains("execution terminated: out of fuel")
    || msg.contains("execution terminated: deadline exceeded")
    || msg.contains("execution terminated: interrupted")
    || msg.contains("outoffuel")
    || msg.contains("deadlineexceeded")
    || msg.contains("interrupted")
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn install_import_map_register_hook(host: &mut RenderedHost) -> Result<(), VmError> {
  let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let call_id = vm.register_native_call(register_import_map_native)?;
  let name_s = scope.alloc_string("__fastrender_register_import_map_native")?;
  scope.push_root(Value::String(name_s))?;
  let func = scope.alloc_native_function(call_id, None, name_s, 1)?;
  scope.push_root(Value::Object(func))?;

  let key_s = scope.alloc_string("__fastrender_register_import_map_native")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, data_desc(Value::Object(func)))?;
  Ok(())
}

fn register_import_map_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let json_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::String(json_s) = json_value else {
    return Err(VmError::TypeError(
      "__fastrender_register_import_map expected a JSON string argument",
    ));
  };
  scope.push_root(Value::String(json_s))?;
  let json = scope.heap().get_string(json_s)?.to_utf8_lossy();

  let Some(any) = hooks.as_any_mut() else {
    return Err(VmError::Unimplemented(
      "__fastrender_register_import_map requires VmHostHooks::as_any_mut",
    ));
  };
  let Some(payload) = any.downcast_mut::<VmJsHostHooksPayload>() else {
    return Err(VmError::Unimplemented(
      "__fastrender_register_import_map expected VmJsHostHooksPayload",
    ));
  };
  let Some(host) = payload.embedder_state_mut::<RenderedHost>() else {
    return Err(VmError::Unimplemented(
      "__fastrender_register_import_map requires embedder state (RenderedHost)",
    ));
  };

  let base_url = url::Url::parse(&host.document_url).map_err(|_err| {
    VmError::TypeError("__fastrender_register_import_map invalid base URL")
  })?;

  let limits = fastrender::js::ImportMapLimits::default();
  let parse_result = fastrender::js::import_maps::create_import_map_parse_result_with_limits(&json, &base_url, &limits);
  {
    // Keep the `Rc<RefCell<...>>` module loader handle alive for the duration of the borrow.
    let loader_handle = host.realm.module_loader_handle();
    let mut loader = loader_handle.borrow_mut();
    fastrender::js::import_maps::register_import_map_with_limits(loader.import_map_state_mut(), parse_result, &limits)
      .map_err(|err| {
        let message = err.to_string();
        let Some(intr) = vm.intrinsics() else {
          return VmError::Unimplemented(
            "__fastrender_register_import_map requires VM intrinsics (Realm must be initialized)",
          );
        };
        match vm_js::new_type_error_object(scope, &intr, &message) {
          Ok(value) => VmError::Throw(value),
          Err(err) => err,
        }
      })?;
  }

  Ok(Value::Undefined)
}

fn install_layout_inspection_hooks(host: &mut RenderedHost) -> Result<(), VmError> {
  let (vm, realm, heap) = host.realm.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let hooks: &[(&str, u32, vm_js::NativeCall)] = &[
    ("__fastrender_force_render", 0u32, force_render_native),
    ("__fastrender_get_rect_by_id", 1u32, get_rect_by_id_native),
  ];

  for (name, argc, callback) in hooks {
    let call_id = vm.register_native_call(*callback)?;
    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(call_id, None, name_s, *argc)?;
    scope.push_root(Value::Object(func))?;

    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(global, key, data_desc(Value::Object(func)))?;
  }

  Ok(())
}

fn force_render_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
    return Err(VmError::Unimplemented(
      "__fastrender_force_render requires BrowserDocumentDom2 VmHost context",
    ));
  };
  document
    .render_if_needed()
    .map_err(|err| throw_error(scope, &err.to_string()))?;
  Ok(Value::Undefined)
}

fn get_rect_by_id_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
    return Err(VmError::Unimplemented(
      "__fastrender_get_rect_by_id requires BrowserDocumentDom2 VmHost context",
    ));
  };

  let id_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::String(id_s) = id_value else {
    return Err(VmError::TypeError("__fastrender_get_rect_by_id expected a string id"));
  };
  scope.push_root(Value::String(id_s))?;
  let id = scope.heap().get_string(id_s)?.to_utf8_lossy();

  document
    .render_if_needed()
    .map_err(|err| throw_error(scope, &err.to_string()))?;

  let Some(prepared) = document.prepared() else {
    return Ok(Value::Null);
  };

  let snapshots = inspect(
    prepared.dom(),
    prepared.styled_tree(),
    &prepared.box_tree().root,
    prepared.fragment_tree(),
    InspectQuery::Id(id),
  )
  .map_err(|err| throw_error(scope, &err.to_string()))?;

  let Some(first) = snapshots.first() else {
    return Ok(Value::Null);
  };

  let mut min_x = f32::INFINITY;
  let mut min_y = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  let mut max_y = f32::NEG_INFINITY;
  for frag in &first.fragments {
    let b = &frag.bounds;
    min_x = min_x.min(b.x);
    min_y = min_y.min(b.y);
    max_x = max_x.max(b.x + b.width);
    max_y = max_y.max(b.y + b.height);
  }

  if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
    return Ok(Value::Null);
  }

  let rect_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(rect_obj))?;

  let width = (max_x - min_x).max(0.0);
  let height = (max_y - min_y).max(0.0);

  let x_key = alloc_key(scope, "x")?;
  let y_key = alloc_key(scope, "y")?;
  let w_key = alloc_key(scope, "width")?;
  let h_key = alloc_key(scope, "height")?;

  scope.define_property(rect_obj, x_key, data_desc(Value::Number(min_x as f64)))?;
  scope.define_property(rect_obj, y_key, data_desc(Value::Number(min_y as f64)))?;
  scope.define_property(rect_obj, w_key, data_desc(Value::Number(width as f64)))?;
  scope.define_property(rect_obj, h_key, data_desc(Value::Number(height as f64)))?;

  Ok(Value::Object(rect_obj))
}

fn string_from_value(heap: &vm_js::Heap, value: Value) -> Option<String> {
  let Value::String(s) = value else {
    return None;
  };
  heap.get_string(s).ok().map(|js| js.to_utf8_lossy())
}

fn harness_error_report(message: impl Into<String>) -> WptReport {
  WptReport {
    file_status: "error".to_string(),
    harness_status: "error".to_string(),
    message: Some(message.into()),
    stack: None,
    subtests: Vec::new(),
  }
}

fn report_from_js_value(vm: &mut vm_js::Vm, scope: &mut vm_js::Scope<'_>, payload: Value) -> WptReport {
  let heap = scope.heap();

  match payload {
    // Some curated tests call `__fastrender_wpt_report("pass")` directly.
    Value::String(s) => WptReport {
      file_status: heap
        .get_string(s)
        .ok()
        .map(|js| js.to_utf8_lossy())
        .unwrap_or_else(|| "error".to_string()),
      harness_status: "ok".to_string(),
      message: None,
      stack: None,
      subtests: Vec::new(),
    },
    Value::Object(obj) => report_from_js_object(vm, scope, obj),
    other => harness_error_report(format!("unexpected report payload type: {other:?}")),
  }
}

fn report_from_js_object(vm: &mut vm_js::Vm, scope: &mut vm_js::Scope<'_>, obj: vm_js::GcObject) -> WptReport {
  // Root the payload object while allocating property keys and reading properties.
  if let Err(err) = scope.push_root(Value::Object(obj)) {
    return harness_error_report(format!("failed to root report payload object: {err}"));
  }

  let file_status_key = match alloc_key(scope, "file_status") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate file_status key: {err}")),
  };
  let harness_status_key = match alloc_key(scope, "harness_status") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate harness_status key: {err}")),
  };
  let message_key = match alloc_key(scope, "message") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate message key: {err}")),
  };
  let stack_key = match alloc_key(scope, "stack") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate stack key: {err}")),
  };
  let subtests_key = match alloc_key(scope, "subtests") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate subtests key: {err}")),
  };
  let length_key = match alloc_key(scope, "length") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate length key: {err}")),
  };
  let name_key = match alloc_key(scope, "name") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate name key: {err}")),
  };
  let status_key = match alloc_key(scope, "status") {
    Ok(key) => key,
    Err(err) => return harness_error_report(format!("failed to allocate status key: {err}")),
  };

  let file_status = match vm.get(scope, obj, file_status_key) {
    Ok(value) => {
      let heap = scope.heap();
      string_from_value(heap, value).unwrap_or_else(|| "error".to_string())
    }
    Err(_err) => "error".to_string(),
  };
  let harness_status = match vm.get(scope, obj, harness_status_key) {
    Ok(value) => {
      let heap = scope.heap();
      string_from_value(heap, value).unwrap_or_else(|| "ok".to_string())
    }
    Err(_err) => "ok".to_string(),
  };

  let message = match vm.get(scope, obj, message_key) {
    Ok(value) => {
      let heap = scope.heap();
      string_from_value(heap, value).filter(|s| !s.is_empty())
    }
    Err(_err) => None,
  };
  let stack = match vm.get(scope, obj, stack_key) {
    Ok(value) => {
      let heap = scope.heap();
      string_from_value(heap, value).filter(|s| !s.is_empty())
    }
    Err(_err) => None,
  };

  let subtests = match vm.get(scope, obj, subtests_key) {
    Ok(Value::Object(arr)) => parse_subtests_array(vm, scope, arr, length_key, name_key, status_key, message_key, stack_key),
    _ => Vec::new(),
  };

  WptReport {
    file_status,
    harness_status,
    message,
    stack,
    subtests,
  }
}

fn parse_subtests_array(
  vm: &mut vm_js::Vm,
  scope: &mut vm_js::Scope<'_>,
  arr: vm_js::GcObject,
  length_key: PropertyKey,
  name_key: PropertyKey,
  status_key: PropertyKey,
  message_key: PropertyKey,
  stack_key: PropertyKey,
) -> Vec<WptSubtest> {
  let len = match vm.get(scope, arr, length_key) {
    Ok(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0,
  };

  let mut out = Vec::new();
  if out.try_reserve(len.min(1024)).is_err() {
    // Keep host allocations bounded; fall back to incremental growth.
  }

  for idx in 0..len {
    // Use a nested scope per element so roots (index key strings, subtest objects) do not
    // accumulate unboundedly across large subtest arrays.
    let mut iter_scope = scope.reborrow();

    let idx_key = match alloc_key(&mut iter_scope, &idx.to_string()) {
      Ok(key) => key,
      Err(_) => continue,
    };

    let Ok(Value::Object(st_obj)) = vm.get(&mut iter_scope, arr, idx_key) else {
      continue;
    };

    if iter_scope.push_root(Value::Object(st_obj)).is_err() {
      continue;
    }

    let name = match vm.get(&mut iter_scope, st_obj, name_key) {
      Ok(value) => string_from_value(iter_scope.heap(), value).unwrap_or_default(),
      Err(_err) => String::new(),
    };
    let status = match vm.get(&mut iter_scope, st_obj, status_key) {
      Ok(value) => {
        string_from_value(iter_scope.heap(), value).unwrap_or_else(|| "error".to_string())
      }
      Err(_err) => "error".to_string(),
    };

    let message = match vm.get(&mut iter_scope, st_obj, message_key) {
      Ok(value) => string_from_value(iter_scope.heap(), value).filter(|s| !s.is_empty()),
      Err(_err) => None,
    };
    let stack = match vm.get(&mut iter_scope, st_obj, stack_key) {
      Ok(value) => string_from_value(iter_scope.heap(), value).filter(|s| !s.is_empty()),
      Err(_err) => None,
    };

    out.push(WptSubtest {
      name,
      status,
      message,
      stack,
    });
  }

  out
}
