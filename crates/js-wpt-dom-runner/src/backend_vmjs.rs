use crate::backend::{Backend, BackendInit};
use crate::wpt_fs::WptFs;
use crate::wpt_report::{WptReport, WptSubtest};
use crate::RunError;
use fastrender::dom2;
use fastrender::js::{
  EventLoop, JsExecutionOptions, MicrotaskCheckpointLimitedOutcome, RunLimits, RunNextTaskLimitedOutcome,
  RunState, VirtualClock, WindowHostState,
};
use fastrender::resource::{FetchedResource, HttpRequest, ResourceFetcher};
use selectors::context::QuirksMode;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use vm_js::{
  GcObject, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks,
};

pub(crate) fn is_available() -> bool {
  true
}

const REPORT_FN_NAME: &str = "__fastrender_wpt_report";
const REPORT_PAYLOAD_KEY: &str = "__fastrender_wpt_report_payload";
const REPORT_STATE_KEY: &str = "__fastrender_wpt_report_state";

const RESOLVE_URL_FN_NAME: &str = "__fastrender_resolve_url";

const REPORT_STATE_NONE: f64 = 0.0;
const REPORT_STATE_AVAILABLE: f64 = 1.0;
const REPORT_STATE_TAKEN: f64 = 2.0;

// Native slot index for storing the owning global object.
const NATIVE_SLOT_GLOBAL: usize = 0;

const DEFAULT_SCRIPT_FUEL: u64 = 10_000_000;

const RESOLVE_URL_RELATIVE_WITHOUT_BASE: &str = "relative URL has no base URL";
const RESOLVE_URL_PARSE_ERROR: &str = "failed to resolve URL";

#[derive(Clone)]
struct WptResourceFetcher {
  fs: WptFs,
}

impl WptResourceFetcher {
  fn new(fs: WptFs) -> Self {
    Self { fs }
  }

  fn content_type_for_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let ct = match ext.as_str() {
      "js" => "application/javascript",
      "mjs" => "application/javascript",
      "html" | "htm" => "text/html",
      "css" => "text/css",
      "json" => "application/json",
      "txt" => "text/plain",
      "svg" => "image/svg+xml",
      _ => return None,
    };
    Some(ct.to_string())
  }

  fn not_found(url: &str) -> fastrender::Result<FetchedResource> {
    let mut res = FetchedResource::new(Vec::new(), None);
    res.status = Some(404);
    res.final_url = Some(url.to_string());
    Ok(res)
  }

  fn fetch_url(&self, url: &str) -> fastrender::Result<FetchedResource> {
    let path = match self.fs.resolve_url("", url) {
      Ok(path) => path,
      Err(_) => return Self::not_found(url),
    };

    match std::fs::read(&path) {
      Ok(bytes) => {
        let mut res = FetchedResource::new(bytes, Self::content_type_for_path(&path));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        Ok(res)
      }
      Err(_) => Self::not_found(url),
    }
  }
}

impl ResourceFetcher for WptResourceFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    self.fetch_url(url)
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> fastrender::Result<FetchedResource> {
    let _ = (req.method, req.headers, req.body, req.redirect);
    self.fetch_url(req.fetch.url)
  }
}

struct VmJsRuntime {
  host: WindowHostState,
  event_loop: EventLoop<WindowHostState>,
  clock: Arc<VirtualClock>,
  run_state: RunState,
  deadline: Duration,
}

pub struct VmJsBackend {
  rt: Option<VmJsRuntime>,
  timed_out: bool,
}

impl Default for VmJsBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl VmJsBackend {
  pub fn new() -> Self {
    Self {
      rt: None,
      timed_out: false,
    }
  }

  fn rt_mut(&mut self) -> Result<&mut VmJsRuntime, RunError> {
    self
      .rt
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js backend is not initialized".to_string()))
  }

  fn is_vm_termination_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("execution terminated: out of fuel")
      || lower.contains("execution terminated: deadline exceeded")
      || lower.contains("execution terminated: interrupted")
      || lower.contains("outoffuel")
      || lower.contains("deadlineexceeded")
      || lower.contains("interrupted")
      || lower.contains("interrupt")
  }

  fn report_state(window: &mut fastrender::js::WindowRealm) -> Result<f64, VmError> {
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;
    let key_s = scope.alloc_string(REPORT_STATE_KEY)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(global, &key)?
    {
      Some(Value::Number(n)) => Ok(n),
      _ => Ok(REPORT_STATE_NONE),
    }
  }

  fn has_report(&mut self) -> Result<bool, RunError> {
    let rt = self.rt_mut()?;
    let state = {
      let window = rt.host.window_mut();
      Self::report_state(window).map_err(|e| RunError::Js(e.to_string()))?
    };
    Ok(state != REPORT_STATE_NONE)
  }

  fn install_wpt_report_hook(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
    fn data_desc(value: Value) -> PropertyDescriptor {
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value,
          writable: true,
        },
      }
    }

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // Initialize `__fastrender_wpt_report_state` + `__fastrender_wpt_report_payload`.
    {
      let state_key_s = scope.alloc_string(REPORT_STATE_KEY)?;
      scope.push_root(Value::String(state_key_s))?;
      let state_key = PropertyKey::from_string(state_key_s);
      scope.define_property(
        global,
        state_key,
        data_desc(Value::Number(REPORT_STATE_NONE)),
      )?;

      let payload_key_s = scope.alloc_string(REPORT_PAYLOAD_KEY)?;
      scope.push_root(Value::String(payload_key_s))?;
      let payload_key = PropertyKey::from_string(payload_key_s);
      scope.define_property(global, payload_key, data_desc(Value::Undefined))?;
    }

    // Install native `__fastrender_wpt_report(payload)` which stores the payload object on the
    // global. The first call wins; subsequent calls are ignored.
    let call_id = vm.register_native_call(wpt_report_native)?;
    let name_s = scope.alloc_string(REPORT_FN_NAME)?;
    scope.push_root(Value::String(name_s))?;
    let func =
      scope.alloc_native_function_with_slots(call_id, None, name_s, 1, &[Value::Object(global)])?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;

    let fn_key = PropertyKey::from_string(name_s);
    scope.define_property(global, fn_key, data_desc(Value::Object(func)))?;

    Ok(())
  }

  fn install_resolve_url_hook(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
    fn data_desc(value: Value) -> PropertyDescriptor {
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value,
          writable: true,
        },
      }
    }

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let call_id = vm.register_native_call(resolve_url_native)?;
    let name_s = scope.alloc_string(RESOLVE_URL_FN_NAME)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(call_id, None, name_s, 2)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;

    let key = PropertyKey::from_string(name_s);
    scope.define_property(global, key, data_desc(Value::Object(func)))?;
    Ok(())
  }

  fn decode_report(heap: &mut Heap, payload: Value) -> Result<WptReport, RunError> {
    match payload {
      Value::String(s) => {
        let text = heap
          .get_string(s)
          .map_err(|e| RunError::Js(e.to_string()))?
          .to_utf8_lossy();
        return Ok(WptReport {
          file_status: text,
          harness_status: "ok".to_string(),
          message: None,
          stack: None,
          subtests: Vec::new(),
        });
      }
      Value::Object(obj) => {
        let mut scope = heap.scope();
        scope
          .push_root(Value::Object(obj))
          .map_err(|e| RunError::Js(e.to_string()))?;

        let get_own =
          |scope: &mut Scope<'_>, obj: GcObject, name: &str| -> Result<Option<Value>, VmError> {
            let key_s = scope.alloc_string(name)?;
            scope.push_root(Value::String(key_s))?;
            let key = PropertyKey::from_string(key_s);
            scope.heap().object_get_own_data_property_value(obj, &key)
          };

        let get_required_string = |scope: &mut Scope<'_>,
                                   obj: GcObject,
                                   name: &str,
                                   default: &str|
         -> Result<String, RunError> {
          match get_own(scope, obj, name).map_err(|e| RunError::Js(e.to_string()))? {
            Some(Value::String(s)) => Ok(
              scope
                .heap()
                .get_string(s)
                .map_err(|e| RunError::Js(e.to_string()))?
                .to_utf8_lossy(),
            ),
            Some(Value::Undefined | Value::Null) | None => Ok(default.to_string()),
            Some(Value::Bool(b)) => Ok(b.to_string()),
            Some(Value::Number(n)) => Ok(n.to_string()),
            Some(_) => Ok(default.to_string()),
          }
        };

        let get_optional_string =
          |scope: &mut Scope<'_>, obj: GcObject, name: &str| -> Result<Option<String>, RunError> {
            let Some(value) = get_own(scope, obj, name).map_err(|e| RunError::Js(e.to_string()))?
            else {
              return Ok(None);
            };
            match value {
              Value::Undefined | Value::Null => Ok(None),
              Value::String(s) => Ok(Some(
                scope
                  .heap()
                  .get_string(s)
                  .map_err(|e| RunError::Js(e.to_string()))?
                  .to_utf8_lossy(),
              )),
              Value::Bool(b) => Ok(Some(b.to_string())),
              Value::Number(n) => Ok(Some(n.to_string())),
              // Some harness shims report nested payloads (e.g. `{name, message}` objects). Decode
              // the subset we care about without executing user JS.
              Value::Object(nested) => {
                let nested_name =
                  match get_own(scope, nested, "name").map_err(|e| RunError::Js(e.to_string()))? {
                    Some(Value::String(s)) => Some(
                      scope
                        .heap()
                        .get_string(s)
                        .map_err(|e| RunError::Js(e.to_string()))?
                        .to_utf8_lossy(),
                    ),
                    Some(Value::Bool(b)) => Some(b.to_string()),
                    Some(Value::Number(n)) => Some(n.to_string()),
                    _ => None,
                  };
                let nested_message = match get_own(scope, nested, "message")
                  .map_err(|e| RunError::Js(e.to_string()))?
                {
                  Some(Value::String(s)) => Some(
                    scope
                      .heap()
                      .get_string(s)
                      .map_err(|e| RunError::Js(e.to_string()))?
                      .to_utf8_lossy(),
                  ),
                  Some(Value::Bool(b)) => Some(b.to_string()),
                  Some(Value::Number(n)) => Some(n.to_string()),
                  _ => None,
                };
                Ok(match (nested_name, nested_message) {
                  (Some(name), Some(msg)) if !name.is_empty() => Some(format!("{name}: {msg}")),
                  (_, Some(msg)) => Some(msg),
                  (Some(name), None) => Some(name),
                  _ => None,
                })
              }
              _ => Ok(None),
            }
          };

        let file_status = get_required_string(&mut scope, obj, "file_status", "error")?;
        let harness_status = get_required_string(&mut scope, obj, "harness_status", "ok")?;
        let message = get_optional_string(&mut scope, obj, "message")?;
        let stack = get_optional_string(&mut scope, obj, "stack")?;

        let mut subtests: Vec<WptSubtest> = Vec::new();
        if let Some(Value::Object(arr)) =
          get_own(&mut scope, obj, "subtests").map_err(|e| RunError::Js(e.to_string()))?
        {
          scope
            .push_root(Value::Object(arr))
            .map_err(|e| RunError::Js(e.to_string()))?;

          let len =
            match get_own(&mut scope, arr, "length").map_err(|e| RunError::Js(e.to_string()))? {
              Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
              _ => 0,
            };

          for idx in 0..len {
            let Some(Value::Object(st_obj)) = get_own(&mut scope, arr, &idx.to_string())
              .map_err(|e| RunError::Js(e.to_string()))?
            else {
              continue;
            };
            scope
              .push_root(Value::Object(st_obj))
              .map_err(|e| RunError::Js(e.to_string()))?;

            let name = get_required_string(&mut scope, st_obj, "name", "")?;
            let status = get_required_string(&mut scope, st_obj, "status", "error")?;
            let message = get_optional_string(&mut scope, st_obj, "message")?;
            let stack = get_optional_string(&mut scope, st_obj, "stack")?;
            subtests.push(WptSubtest {
              name,
              status,
              message,
              stack,
            });
          }
        }

        Ok(WptReport {
          file_status,
          harness_status,
          message,
          stack,
          subtests,
        })
      }
      other => Ok(WptReport {
        file_status: "error".to_string(),
        harness_status: "error".to_string(),
        message: Some(format!("unexpected report payload type: {other:?}")),
        stack: None,
        subtests: Vec::new(),
      }),
    }
  }

  fn drain_microtasks_internal(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(());
    }
    if self.has_report()? {
      return Ok(());
    }

    let rt = self.rt_mut()?;
    match rt
      .event_loop
      .perform_microtask_checkpoint_limited(&mut rt.host, &mut rt.run_state)
    {
      Ok(MicrotaskCheckpointLimitedOutcome::Completed) => Ok(()),
      Ok(MicrotaskCheckpointLimitedOutcome::Stopped(_reason)) => {
        self.timed_out = true;
        Ok(())
      }
      Err(err) => {
        let msg = err.to_string();
        if Self::is_vm_termination_message(&msg) {
          self.timed_out = true;
          Ok(())
        } else {
          Err(RunError::Js(msg))
        }
      }
    }
  }
}

impl Backend for VmJsBackend {
  fn init_realm(
    &mut self,
    init: BackendInit,
    _host: Option<&mut dyn crate::engine::HostEnvironment>,
  ) -> Result<(), RunError> {
    self.timed_out = false;

    // Deterministic virtual clock starting at 0.
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::ZERO);
    let clock_dyn: Arc<dyn fastrender::js::Clock> = clock.clone();

    let event_loop = EventLoop::<WindowHostState>::with_clock(Arc::clone(&clock_dyn));

    let dom = build_dom_skeleton().map_err(|e| RunError::Js(e.to_string()))?;
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(WptResourceFetcher::new(init.fs));
    // Avoid wall-clock deadlines inside the VM budget so the offline WPT runner is deterministic.
    // We rely on a VirtualClock + host RunLimits for timeout enforcement instead.
    let mut js_opts = JsExecutionOptions::default();
    js_opts.event_loop_run_limits.max_wall_time = None;
    js_opts.max_instruction_count = Some(DEFAULT_SCRIPT_FUEL);

    let mut host = WindowHostState::new_with_fetcher_and_clock_and_options(
      dom,
      init.test_url.clone(),
      fetcher,
      clock_dyn,
      js_opts,
    )
    .map_err(|e| RunError::Js(e.to_string()))?;

    {
      let window = host.window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      Self::install_wpt_report_hook(vm, realm, heap).map_err(|e| RunError::Js(e.to_string()))?;
      Self::install_resolve_url_hook(vm, realm, heap).map_err(|e| RunError::Js(e.to_string()))?;
    }

    let deadline = init.timeout;
    let run_state = event_loop.new_run_state(RunLimits {
      max_tasks: init.max_tasks,
      max_microtasks: init.max_microtasks,
      max_wall_time: Some(deadline),
    });

    self.rt = Some(VmJsRuntime {
      host,
      event_loop,
      clock,
      run_state,
      deadline,
    });

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
    if self.has_report()? {
      return Ok(());
    }

    let rt = self.rt_mut()?;
    {
      let window = rt.host.window_mut();
      window.reset_interrupt();
    }

    let result = rt.host.exec_script_with_name_in_event_loop(
      &mut rt.event_loop,
      Arc::<str>::from(name),
      Arc::<str>::from(source),
    );

    match result {
      Ok(_value) => {
        // Microtask checkpoint after every script evaluation.
        self.drain_microtasks_internal()
      }
      Err(err) => {
        let msg = err.to_string();
        if Self::is_vm_termination_message(&msg) {
          self.timed_out = true;
          return Ok(());
        }
        Err(RunError::Js(msg))
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
    if self.has_report()? {
      return Ok(false);
    }

    let rt = self.rt_mut()?;
    match rt
      .event_loop
      .run_next_task_limited(&mut rt.host, &mut rt.run_state)
    {
      Ok(RunNextTaskLimitedOutcome::Ran) => Ok(true),
      Ok(RunNextTaskLimitedOutcome::NoTask) => Ok(false),
      Ok(RunNextTaskLimitedOutcome::Stopped(_reason)) => {
        self.timed_out = true;
        Ok(false)
      }
      Err(err) => {
        let msg = err.to_string();
        if Self::is_vm_termination_message(&msg) {
          self.timed_out = true;
          Ok(false)
        } else {
          Err(RunError::Js(msg))
        }
      }
    }
  }

  fn take_report(&mut self) -> Result<Option<WptReport>, RunError> {
    let rt = self.rt_mut()?;
    let window = rt.host.window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .map_err(|e| RunError::Js(e.to_string()))?;

    let state_key_s = scope
      .alloc_string(REPORT_STATE_KEY)
      .map_err(|e| RunError::Js(e.to_string()))?;
    scope
      .push_root(Value::String(state_key_s))
      .map_err(|e| RunError::Js(e.to_string()))?;
    let state_key = PropertyKey::from_string(state_key_s);
    let state = match scope
      .heap()
      .object_get_own_data_property_value(global, &state_key)
      .map_err(|e| RunError::Js(e.to_string()))?
    {
      Some(Value::Number(n)) => n,
      _ => REPORT_STATE_NONE,
    };

    if state != REPORT_STATE_AVAILABLE {
      return Ok(None);
    }

    let payload_key_s = scope
      .alloc_string(REPORT_PAYLOAD_KEY)
      .map_err(|e| RunError::Js(e.to_string()))?;
    scope
      .push_root(Value::String(payload_key_s))
      .map_err(|e| RunError::Js(e.to_string()))?;
    let payload_key = PropertyKey::from_string(payload_key_s);

    let payload = scope
      .heap()
      .object_get_own_data_property_value(global, &payload_key)
      .map_err(|e| RunError::Js(e.to_string()))?
      .unwrap_or(Value::Undefined);

    let report = Self::decode_report(scope.heap_mut(), payload)?;

    // Mark taken so subsequent polls return None and additional report calls are ignored.
    scope
      .heap_mut()
      .object_set_existing_data_property_value(global, &payload_key, Value::Undefined)
      .map_err(|e| RunError::Js(e.to_string()))?;
    scope
      .heap_mut()
      .object_set_existing_data_property_value(
        global,
        &state_key,
        Value::Number(REPORT_STATE_TAKEN),
      )
      .map_err(|e| RunError::Js(e.to_string()))?;

    Ok(Some(report))
  }

  fn is_timed_out(&self) -> bool {
    if self.timed_out {
      return true;
    }
    let Some(rt) = self.rt.as_ref() else {
      return true;
    };
    rt.clock.now() >= rt.deadline
  }

  fn idle_wait(&mut self) {
    if self.timed_out {
      return;
    }
    let Some(rt) = self.rt.as_mut() else {
      self.timed_out = true;
      return;
    };

    let now = rt.clock.now();
    let deadline = rt.deadline;
    let next_due = rt.event_loop.next_timer_due_time();
    let target = match next_due {
      Some(due) if due > now => due.min(deadline),
      Some(_due) => now,
      None => deadline,
    };

    if target > now {
      rt.clock.set_now(target);
    } else if now < deadline {
      // Nothing runnable and nothing to advance to: force progress to the deadline so we don't spin
      // forever.
      rt.clock.set_now(deadline);
    }

    if rt.clock.now() >= deadline {
      self.timed_out = true;
    }
  }
}

fn build_dom_skeleton() -> Result<dom2::Document, dom2::DomError> {
  let mut doc = dom2::Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let html = doc.create_element("html", "");
  doc.append_child(root, html)?;
  let head = doc.create_element("head", "");
  doc.append_child(html, head)?;
  let body = doc.create_element("body", "");
  doc.append_child(html, body)?;
  Ok(doc)
}

fn global_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slot = scope
    .heap()
    .get_function_native_slots(callee)?
    .get(NATIVE_SLOT_GLOBAL)
    .copied()
    .unwrap_or(Value::Undefined);
  match slot {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "native function missing global binding",
    )),
  }
}

fn wpt_report_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let global = global_from_callee(scope, callee)?;

  // Root `global` and the payload while allocating keys / updating heap properties.
  let payload = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(global))?;
  scope.push_root(payload)?;

  let state_key_s = scope.alloc_string(REPORT_STATE_KEY)?;
  scope.push_root(Value::String(state_key_s))?;
  let state_key = PropertyKey::from_string(state_key_s);
  let state = match scope
    .heap()
    .object_get_own_data_property_value(global, &state_key)?
  {
    Some(Value::Number(n)) => n,
    _ => REPORT_STATE_NONE,
  };
  if state != REPORT_STATE_NONE {
    return Ok(Value::Undefined);
  }

  let payload_key_s = scope.alloc_string(REPORT_PAYLOAD_KEY)?;
  scope.push_root(Value::String(payload_key_s))?;
  let payload_key = PropertyKey::from_string(payload_key_s);
  scope
    .heap_mut()
    .object_set_existing_data_property_value(global, &payload_key, payload)?;
  scope.heap_mut().object_set_existing_data_property_value(
    global,
    &state_key,
    Value::Number(REPORT_STATE_AVAILABLE),
  )?;
  Ok(Value::Undefined)
}

fn resolve_url_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let base = args.get(1).copied().unwrap_or(Value::Undefined);

  let input_s = match scope.heap_mut().to_string(input) {
    Ok(s) => s,
    Err(_) => return Err(VmError::TypeError(RESOLVE_URL_PARSE_ERROR)),
  };
  let input = scope
    .heap()
    .get_string(input_s)
    .map_err(|_| VmError::TypeError(RESOLVE_URL_PARSE_ERROR))?
    .to_utf8_lossy();

  let base_opt: Option<String> = match base {
    Value::Undefined | Value::Null => None,
    other => {
      let s = scope
        .heap_mut()
        .to_string(other)
        .map_err(|_| VmError::TypeError(RESOLVE_URL_PARSE_ERROR))?;
      Some(
        scope
          .heap()
          .get_string(s)
          .map_err(|_| VmError::TypeError(RESOLVE_URL_PARSE_ERROR))?
          .to_utf8_lossy(),
      )
    }
  };

  match fastrender::js::resolve_url(&input, base_opt.as_deref()) {
    Ok(resolved) => {
      let handle = scope.alloc_string(&resolved)?;
      Ok(Value::String(handle))
    }
    Err(fastrender::js::UrlResolveError::RelativeUrlWithoutBase) => {
      Err(VmError::TypeError(RESOLVE_URL_RELATIVE_WITHOUT_BASE))
    }
    Err(_) => Err(VmError::TypeError(RESOLVE_URL_PARSE_ERROR)),
  }
}
