use vm_js::{
  Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, Value, Vm, VmError, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn host_gc(
  _vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn vm_js::VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  scope.heap_mut().collect_garbage();
  Ok(Value::Undefined)
}

/// Minimal host hook implementation that completes `HostLoadImportedModule` immediately with an
/// error *as a throw completion* so dynamic `import()` returns a rejected promise instead of
/// throwing synchronously.
struct RejectingImportHooks {
  microtasks: MicrotaskQueue,
  last_specifier: Option<String>,
}

impl RejectingImportHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      last_specifier: None,
    }
  }

  fn teardown_jobs(&mut self, rt: &mut JsRuntime) {
    self.microtasks.teardown(rt);
  }
}

impl VmHostHooks for RejectingImportHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut vm_js::Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    self.last_specifier = Some(module_request.specifier_utf8_lossy());
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      // Reject the import() promise. The rejection reason is irrelevant for this test.
      Err(VmError::Throw(Value::Undefined)),
    )?;
    Ok(())
  }
}

#[test]
fn generator_dynamic_import_specifier_can_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        return import(yield 'x');
      }

      var it = g();
      var first = it.next();
      var second = it.next("./m.js");

      first.value === 'x' &&
        first.done === false &&
        second.done === true &&
        second.value &&
        typeof second.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), Some("./m.js"));
  Ok(())
}

#[test]
fn generator_dynamic_import_options_can_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        return import(yield 0, yield 1);
      }

      var it = g();
      var first = it.next();
      var second = it.next("./m.js");
      var third = it.next(undefined);

      first.value === 0 &&
        first.done === false &&
        second.value === 1 &&
        second.done === false &&
        third.done === true &&
        third.value &&
        typeof third.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), Some("./m.js"));
  Ok(())
}

#[test]
fn generator_dynamic_import_evaluates_specifier_before_options_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      var log = "";
      function spec() { log += "S"; return "./m.js"; }

      function* g() {
        return import(spec(), (yield 0));
      }

      var it = g();
      var first = it.next();
      var logAfterFirst = log;
      var second = it.next(undefined);

      first.value === 0 &&
        first.done === false &&
        logAfterFirst === "S" &&
        second.done === true &&
        second.value &&
        typeof second.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), Some("./m.js"));
  Ok(())
}

#[test]
fn generator_dynamic_import_options_frame_specifier_is_traced_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();

  // Suspend with the specifier stored only in the generator continuation frame.
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        return import({ toString() { return "./m.js"; } }, (yield 0));
      }
      var it = g();
      var first = it.next();
      first.value === 0 && first.done === false
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Force GC while the generator is suspended so the continuation frame must trace its captured
  // specifier value.
  rt.heap_mut().collect_garbage();

  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      var second = it.next(undefined);
      second.done === true &&
        second.value &&
        typeof second.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);

  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), Some("./m.js"));
  Ok(())
}

#[test]
fn generator_dynamic_import_options_frame_specifier_is_rooted_across_resume_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("__gc", host_gc, 0)?;

  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        // The specifier is stored only in the ImportAfterOptions continuation frame after the first
        // yield. Trigger a GC during resumption (while the continuation is temporarily stored
        // outside the heap) to ensure the specifier is included in `gen_root_values_for_continuation`.
        return import({ toString() { return "./m.js"; } }, ((yield 0), __gc(), undefined));
      }

      var it = g();
      var first = it.next();
      var second = it.next(undefined);

      first.value === 0 &&
        first.done === false &&
        second.done === true &&
        second.value &&
        typeof second.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), Some("./m.js"));
  Ok(())
}

#[test]
fn generator_dynamic_import_allows_trailing_comma_after_specifier() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        return import(yield 0,);
      }

      var it = g();
      var first = it.next();
      var second = it.next("./m.js");

      first.value === 0 &&
        first.done === false &&
        second.done === true &&
        second.value &&
        typeof second.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), Some("./m.js"));
  Ok(())
}

#[test]
fn generator_dynamic_import_does_not_run_when_yield_resumed_with_return() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        return import(yield 0);
      }

      var it = g();
      var first = it.next();
      var second = it.return(5);

      first.value === 0 &&
        first.done === false &&
        second.value === 5 &&
        second.done === true
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), None);
  Ok(())
}

#[test]
fn generator_dynamic_import_does_not_run_when_yield_resumed_with_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        try {
          return import(yield 0);
        } catch (e) {
          return e;
        }
      }

      var it = g();
      var first = it.next();
      var second = it.throw(7);

      first.value === 0 &&
        first.done === false &&
        second.value === 7 &&
        second.done === true
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  assert_eq!(hooks.last_specifier.as_deref(), None);
  Ok(())
}
