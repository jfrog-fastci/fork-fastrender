use crate::backend::{Backend, BackendInit};
use crate::wpt_fs::WptFs;
use crate::wpt_report::{WptReport, WptSubtest};
use crate::wpt_resource_fetcher::WptResourceFetcher;
use crate::RunError;
use fastrender::js::{
  EventLoop, JsExecutionOptions, MicrotaskCheckpointLimitedOutcome, RunLimits,
  RunNextTaskLimitedOutcome, RunState, VirtualClock, WindowHostState,
};
use fastrender::js::window_realm::DomBindingsBackend;
use std::sync::Arc;
use std::time::Duration;
use vm_js::{
  GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};
use webidl_vm_js::VmJsHostHooksPayload;

pub(crate) fn is_available() -> bool {
  true
}

// Opt-in WebIDL DOM backend selection for the vm-js runner.
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

/// `vm-js` backend implemented as a thin adapter over FastRender's real Window-shaped runtime:
/// - `WindowHostState` / `WindowRealm` (vm-js realm with DOM-ish globals)
/// - `EventLoop` (tasks/microtasks/timers)
/// - `VirtualClock` (deterministic time for tests)
/// - `WptResourceFetcher` (offline-only fetch implementation)
pub struct VmJsBackend {
  fs: WptFs,

  host: Option<WindowHostState>,
  event_loop: Option<EventLoop<WindowHostState>>,
  virtual_clock: Option<Arc<VirtualClock>>,
  run_state: Option<RunState>,

  deadline: Option<Duration>,
  timed_out: bool,
}

impl VmJsBackend {
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

  fn state_mut(
    &mut self,
  ) -> Result<
    (
      &mut WindowHostState,
      &mut EventLoop<WindowHostState>,
      &mut RunState,
    ),
    RunError,
  > {
    let host = self
      .host
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js backend is not initialized".to_string()))?;
    let event_loop = self
      .event_loop
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js backend is not initialized".to_string()))?;
    let run_state = self
      .run_state
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js backend is not initialized".to_string()))?;
    Ok((host, event_loop, run_state))
  }

  fn host_mut(&mut self) -> Result<&mut WindowHostState, RunError> {
    self
      .host
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js backend is not initialized".to_string()))
  }

  fn virtual_now(&self) -> Option<Duration> {
    self.virtual_clock.as_ref().map(|c| c.now())
  }

  fn handle_fastrender_error_as_timeout_or_js(
    &mut self,
    err: fastrender::error::Error,
  ) -> Result<(), RunError> {
    let msg = err.to_string();
    if is_vm_interrupt_message(&msg) {
      self.timed_out = true;
      Ok(())
    } else {
      Err(RunError::Js(msg))
    }
  }

  fn perform_microtask_checkpoint_limited(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(());
    }

    let (host, event_loop, run_state) = self.state_mut()?;

    match event_loop.perform_microtask_checkpoint_limited(host, run_state) {
      Ok(MicrotaskCheckpointLimitedOutcome::Completed) => Ok(()),
      Ok(MicrotaskCheckpointLimitedOutcome::Stopped(_reason)) => {
        self.timed_out = true;
        Ok(())
      }
      Err(err) => self.handle_fastrender_error_as_timeout_or_js(err),
    }
  }
}

impl Backend for VmJsBackend {
  fn init_realm(
    &mut self,
    init: BackendInit,
    _host: Option<&mut dyn crate::engine::HostEnvironment>,
  ) -> Result<(), RunError> {
    self.deadline = Some(init.timeout);
    self.timed_out = false;

    // Tear down any previous realm so we don't keep global state (e.g. session storage namespace
    // registrations) alive while resetting thread-local subsystems for the next test.
    self.host = None;
    self.event_loop = None;
    self.virtual_clock = None;
    self.run_state = None;

    // Each curated WPT test expects a fresh browsing context. FastRender's Web Storage hub is
    // thread-local, so it must be reset between tests executed on the same worker thread.
    fastrender::js::web_storage::clear_default_web_storage_hub();

    // Deterministic virtual time (starts at 0).
    let virtual_clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<WindowHostState>::with_clock(virtual_clock.clone());

    // Create an HTML-ish DOM with <html><head> and <body> so curated tests can assert:
    // `document.head.tagName === "HEAD"` etc.
    let dom =
      fastrender::dom2::parse_html("<!doctype html><html><head></head><body></body></html>")
        .map_err(|e| RunError::Js(e.to_string()))?;

    // Offline-only fetcher mapped to the local curated WPT corpus.
    let fetcher = Arc::new(WptResourceFetcher::from_wpt_fs(&self.fs));

    // JS execution options tuned for deterministic test runs:
    // - disable wall-clock based deadlines (avoid CI flakiness),
    // - keep an instruction/fuel budget so `while(true){}` terminates deterministically.
    let mut options = JsExecutionOptions::default();
    options.event_loop_run_limits.max_wall_time = None;
    // The WPT runner relies on a deterministic instruction budget (fuel) rather than wall-clock
    // timeouts. Keep this budget conservative so `while(true){}` tests terminate quickly even in
    // debug builds.
    options.max_instruction_count = Some(50_000);
    // Enable module scripts and dynamic `import()` for curated module tests. This is still guarded by
    // the host's module loader support inside FastRender's `WindowHostState`.
    options.supports_module_scripts = true;

    // Keep event-loop queue limits consistent with the VM configuration.
    event_loop.set_queue_limits(options.event_loop_queue_limits);

    let dom_bindings_backend = dom_bindings_backend_from_env()?;

    let mut host = WindowHostState::new_with_fetcher_and_clock_and_options_and_dom_backend(
      dom,
      init.test_url,
      fetcher,
      virtual_clock.clone(),
      options,
      dom_bindings_backend,
    )
    .map_err(|e| RunError::Js(e.to_string()))?;

    install_import_map_register_hook(&mut host).map_err(|e| RunError::Js(e.to_string()))?;

    // Install runner-specific globals:
    // - `__fastrender_wpt_report(payload)` stores the first payload for `take_report`.
    // - `__fastrender_resolve_url(input, base)` (legacy helper used by curated tests).
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
        //
        // This delegates to a native function installed by the runner so tests can deterministically
        // set up import maps before evaluating `import()` expressions.
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
      })();
    "#;

    host
      .exec_script_with_name_in_event_loop(
        &mut event_loop,
        "fastrender_wpt_bootstrap.js",
        BOOTSTRAP,
      )
      .map_err(|e| RunError::Js(e.to_string()))?;

    let run_state = event_loop.new_run_state(RunLimits {
      max_tasks: init.max_tasks,
      max_microtasks: init.max_microtasks,
      max_wall_time: None,
    });

    self.virtual_clock = Some(virtual_clock);
    self.event_loop = Some(event_loop);
    self.host = Some(host);
    self.run_state = Some(run_state);

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

    let (host, event_loop, _run_state) = self.state_mut()?;
    let exec_result = host.exec_script_with_name_in_event_loop(event_loop, name, source);

    match exec_result {
      Ok(_value) => {
        // Microtask checkpoint after every script evaluation.
        self.perform_microtask_checkpoint_limited()
      }
      Err(err) => self.handle_fastrender_error_as_timeout_or_js(err),
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

    let (host, event_loop, run_state) = self.state_mut()?;
    match event_loop.run_next_task_limited(host, run_state) {
      Ok(RunNextTaskLimitedOutcome::Ran) => Ok(true),
      Ok(RunNextTaskLimitedOutcome::NoTask) => Ok(false),
      Ok(RunNextTaskLimitedOutcome::Stopped(_reason)) => {
        self.timed_out = true;
        Ok(false)
      }
      Err(err) => {
        self.handle_fastrender_error_as_timeout_or_js(err)?;
        Ok(false)
      }
    }
  }

  fn take_report(&mut self) -> Result<Option<WptReport>, RunError> {
    let host = self.host_mut()?;
    let window = host.window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
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
    let _ = scope.ordinary_set(
      vm,
      global,
      payload_key,
      Value::Undefined,
      Value::Object(global),
    );

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
    run_state.tasks_executed() >= limits.max_tasks
      || run_state.microtasks_executed() >= limits.max_microtasks
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

fn alloc_key(scope: &mut vm_js::Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn install_import_map_register_hook(host: &mut WindowHostState) -> Result<(), VmError> {
  let window = host.window_mut();
  let (vm, realm, heap) = window.vm_realm_and_heap_mut();
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
  scope.define_property(
    global,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(func),
        writable: true,
      },
    },
  )?;
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
  let Some(host) = payload.embedder_state_mut::<WindowHostState>() else {
    return Err(VmError::Unimplemented(
      "__fastrender_register_import_map requires embedder state (WindowHostState)",
    ));
  };

  host
    .register_import_map_using_document_base(&json)
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

  Ok(Value::Undefined)
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

fn report_from_js_value(
  vm: &mut vm_js::Vm,
  scope: &mut vm_js::Scope<'_>,
  payload: Value,
) -> WptReport {
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

fn report_from_js_object(
  vm: &mut vm_js::Vm,
  scope: &mut vm_js::Scope<'_>,
  obj: vm_js::GcObject,
) -> WptReport {
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
    Err(err) => {
      return harness_error_report(format!("failed to allocate harness_status key: {err}"))
    }
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
    Ok(Value::Object(arr)) => parse_subtests_array(
      vm,
      scope,
      arr,
      length_key,
      name_key,
      status_key,
      message_key,
      stack_key,
    ),
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

#[cfg(test)]
mod tests {
  use super::VmJsBackend;
  use crate::engine::{Backend, BackendInit};
  use crate::wpt_fs::WptFs;
  use fastrender::js::web_storage::{
    get_local_area, get_session_area, origin_key_from_document_url, with_default_hub_mut,
    SessionNamespaceId,
  };
  use std::path::PathBuf;
  use std::time::Duration;

  #[test]
  fn init_realm_resets_default_web_storage_hub() {
    let wpt_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/wpt_dom");
    let fs = WptFs::new(&wpt_root).expect("failed to open tests/wpt_dom corpus");

    let init = BackendInit {
      test_url: "https://web-platform.test/wpt_dom_web_storage_reset.html".to_string(),
      timeout: Duration::from_millis(50),
      max_tasks: 10,
      max_microtasks: 100,
    };

    let mut backend = VmJsBackend::new(fs);
    backend.init_realm(init.clone(), None).expect("init_realm 1 failed");

    let origin = origin_key_from_document_url(&init.test_url).expect("test url should have an origin");
    let local_area = get_local_area(Some(origin.as_str()));
    local_area
      .lock()
      .set_item("leak", "1")
      .expect("failed to set local area item");

    // Session storage areas are only persisted by the hub when the namespace is registered as
    // "active" (i.e. when a window/browsing context is alive). Simulate an active namespace without
    // relying on `WindowHostState` internals.
    let session_ns = SessionNamespaceId(1);
    with_default_hub_mut(|hub| hub.register_window(session_ns).disarm());

    let session_area = get_session_area(session_ns, Some(origin.as_str()));
    session_area
      .lock()
      .set_item("leak", "1")
      .expect("failed to set session area item");

    // A second realm (next WPT test) must start with a clean storage hub, even when executed on the
    // same thread.
    backend.init_realm(init, None).expect("init_realm 2 failed");

    assert_eq!(local_area.lock().get_item("leak"), None);
    assert_eq!(session_area.lock().get_item("leak"), None);
  }
}
