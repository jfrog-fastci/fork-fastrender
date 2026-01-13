use std::collections::HashMap;

use vm_js::{
  CompiledScript, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, PropertyKind, SourceTextModuleRecord,
  Value, Vm, VmError, VmHostHooks, VmOptions,
};

#[derive(Debug)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

/// Minimal host hook implementation for exercising dynamic `import()` from compiled HIR scripts.
///
/// This captures `HostLoadImportedModule` requests so tests can complete them manually by calling
/// `FinishLoadingImportedModule` on the runtime.
struct TestHostHooks {
  microtasks: MicrotaskQueue,
  /// Specifier → module id mapping used by `complete_load_for`.
  modules: HashMap<String, ModuleId>,
  pending: Vec<PendingLoad>,
}

impl TestHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
      pending: Vec::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self.modules.insert(specifier.to_string(), module);
  }

  fn pending_count(&self) -> usize {
    self.pending.len()
  }

  fn complete_load_for(&mut self, rt: &mut JsRuntime, specifier: &str) {
    let idx = self
      .pending
      .iter()
      .position(|p| p.request.specifier == specifier)
      .unwrap_or_else(|| panic!("no pending module load for specifier {specifier:?}"));
    let pending = self.pending.remove(idx);

    let module = *self
      .modules
      .get(specifier)
      .unwrap_or_else(|| panic!("no module registered for specifier {specifier:?}"));

    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    let mut scope = heap.scope();
    vm.finish_loading_imported_module(
      &mut scope,
      modules,
      self,
      pending.referrer,
      pending.request,
      pending.payload,
      Ok(module),
    )
    .unwrap();
  }

  fn teardown_jobs(&mut self, rt: &mut JsRuntime) {
    self.microtasks.teardown(rt);
  }

  fn perform_microtask_checkpoint(&mut self, rt: &mut JsRuntime) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    let mut errors = Vec::new();
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = job.run(rt, self) {
        let is_termination = matches!(err, VmError::Termination(_));
        errors.push(err);
        if is_termination {
          // Termination is a hard stop; discard remaining queued jobs so we don't leak persistent
          // roots.
          self.microtasks.teardown(rt);
          break;
        }
      }
    }
    self.microtasks.end_checkpoint();

    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }
    Ok(())
  }
}

impl VmHostHooks for TestHostHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut vm_js::Scope<'_>,
    _modules: &mut vm_js::ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    self.pending.push(PendingLoad {
      referrer,
      request: module_request,
      payload,
    });
    Ok(())
  }
}

/// Host hook implementation that completes `HostLoadImportedModule` synchronously by immediately
/// calling `FinishLoadingImportedModule`.
struct SyncImportHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<String, ModuleId>,
}

impl SyncImportHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self.modules.insert(specifier.to_string(), module);
  }

  fn teardown_jobs(&mut self, rt: &mut JsRuntime) {
    self.microtasks.teardown(rt);
  }

  fn perform_microtask_checkpoint(&mut self, rt: &mut JsRuntime) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    let mut errors = Vec::new();
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = job.run(rt, self) {
        let is_termination = matches!(err, VmError::Termination(_));
        errors.push(err);
        if is_termination {
          // Termination is a hard stop; discard remaining queued jobs so we don't leak persistent
          // roots.
          self.microtasks.teardown(rt);
          break;
        }
      }
    }
    self.microtasks.end_checkpoint();

    if let Some(err) = errors.into_iter().next() {
      return Err(err);
    }
    Ok(())
  }
}

impl VmHostHooks for SyncImportHooks {
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
    modules: &mut vm_js::ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    let module = *self
      .modules
      .get(module_request.specifier.as_str())
      .unwrap_or_else(|| panic!("no module registered for specifier {:?}", module_request.specifier));
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(module),
    )
  }
}

fn new_runtime_with_heap_limit(bytes: usize) -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(bytes, bytes));
  JsRuntime::new(vm, heap)
}

fn new_runtime() -> Result<JsRuntime, VmError> {
  // Dynamic import allocates module graph state and Promise capabilities before the first microtask
  // checkpoint. Keep the heap reasonably small to catch leaks, but large enough to cover the import
  // pipeline.
  new_runtime_with_heap_limit(8 * 1024 * 1024)
}

#[test]
fn compiled_dynamic_import_resolves_to_module_namespace() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Build a tiny module graph:
  // - ./m.js re-exports `y` from ./dep.js and exports `x`.
  // - ./dep.js exports `y`.
  let dep_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const y = 1;")?;
  let dep = rt.modules_mut().add_module(dep_record)?;
  let m_record = SourceTextModuleRecord::parse(
    &mut rt.heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut hooks = TestHostHooks::new();
  hooks.register_module("./m.js", m);
  hooks.register_module("./dep.js", dep);

  let script = CompiledScript::compile_script(&mut rt.heap, "test.js", "import('./m.js')")?;

  let mut dummy_host = ();
  let promise_value = rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  assert_eq!(hooks.pending_count(), 1);
  hooks.complete_load_for(&mut rt, "./m.js");
  assert_eq!(hooks.pending_count(), 1);

  let promise_value = rt
    .heap
    .get_root(promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "promise root should reference an object",
    ));
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  // Complete the dependency load and drain again. This should fulfill the dynamic import promise
  // with the module namespace.
  hooks.complete_load_for(&mut rt, "./dep.js");
  // `ContinueDynamicImport` uses `PerformPromiseThen` even when module evaluation completes
  // synchronously, so the import() promise is fulfilled via a microtask.
  hooks.perform_microtask_checkpoint(&mut rt)?;

  let promise_value = rt
    .heap
    .get_root(promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "promise root should reference an object",
    ));
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_value = rt
    .heap
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  let Value::Object(ns_obj) = ns_value else {
    panic!("dynamic import promise should fulfill to an object");
  };

  // Namespace should contain exports `x` and `y`.
  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let desc_x = scope
    .object_get_own_property_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      ns_obj,
      x_key,
    )?
    .expect("namespace should have an 'x' export");
  assert!(desc_x.enumerable);
  assert!(!desc_x.configurable);
  assert!(matches!(
    desc_x.kind,
    PropertyKind::Data {
      value: Value::Number(n),
      writable: true,
    } if n == 1.0
  ));

  let desc_y = scope
    .object_get_own_property_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      ns_obj,
      y_key,
    )?
    .expect("namespace should have a 'y' export");
  assert!(desc_y.enumerable);
  assert!(!desc_y.configurable);
  assert!(matches!(
    desc_y.kind,
    PropertyKind::Data {
      value: Value::Number(n),
      writable: true,
    } if n == 1.0
  ));

  // Reading the exported bindings should reflect evaluated module state.
  let x_value = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut dummy_host,
    &mut hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  let y_value = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut dummy_host,
    &mut hooks,
    ns_obj,
    y_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  rt.heap.remove_root(promise_root);
  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_script_dynamic_import_works() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let m_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const x = 1;")?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut hooks = SyncImportHooks::new();
  hooks.register_module("m.js", m);

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var p = import('m.js');
    "#,
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  // Read `p` off the global object.
  let promise_obj = {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("p")?);
    let promise_value = scope.get_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      global,
      key,
      Value::Object(global),
    )?;
    let Value::Object(obj) = promise_value else {
      panic!("import() should assign a Promise object to global `p`");
    };
    assert_eq!(scope.heap().promise_state(obj)?, PromiseState::Pending);
    obj
  };

  hooks.perform_microtask_checkpoint(&mut rt)?;

  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let ns_value = rt
    .heap
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  let Value::Object(ns_obj) = ns_value else {
    panic!("dynamic import promise should fulfill to an object");
  };

  // Namespace should expose `x === 1`.
  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let x_value = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut dummy_host,
    &mut hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  drop(scope);
  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_function_dynamic_import_works() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let m_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const x = 1;")?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut hooks = SyncImportHooks::new();
  hooks.register_module("m.js", m);

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      function f() {
        return import('m.js');
      }
      var p = f();
    "#,
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  // Read `p` off the global object.
  let promise_obj = {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("p")?);
    let promise_value = scope.get_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      global,
      key,
      Value::Object(global),
    )?;
    let Value::Object(obj) = promise_value else {
      panic!("import() should assign a Promise object to global `p`");
    };
    assert_eq!(scope.heap().promise_state(obj)?, PromiseState::Pending);
    obj
  };

  hooks.perform_microtask_checkpoint(&mut rt)?;

  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let ns_value = rt
    .heap
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  let Value::Object(ns_obj) = ns_value else {
    panic!("dynamic import promise should fulfill to an object");
  };

  // Namespace should expose `x === 1`.
  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let x_value = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut dummy_host,
    &mut hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  drop(scope);
  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_dynamic_import_evaluates_specifier_then_options() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let m_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const x = 1;")?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut hooks = SyncImportHooks::new();
  hooks.register_module("m.js", m);

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var log = "";
      function s() { log = log + "s"; return "m.js"; }
      function o() { log = log + "o"; return undefined; }
      var p = import(s(), o());
    "#,
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  // The engine should evaluate the specifier expression first, then the options expression.
  let log_val = {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("log")?);
    scope.get_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      global,
      key,
      Value::Object(global),
    )?
  };
  let Value::String(log_s) = log_val else {
    return Err(VmError::InvariantViolation(
      "expected global `log` to be a string",
    ));
  };
  assert_eq!(
    rt.heap.get_string(log_s)?.to_utf8_lossy(),
    "so",
    "expected import(s(), o()) to evaluate specifier then options"
  );

  hooks.perform_microtask_checkpoint(&mut rt)?;
  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_dynamic_import_requires_module_graph() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Simulate an embedding that did not install a module graph pointer.
  rt.vm.clear_module_graph();

  let script = CompiledScript::compile_script(&mut rt.heap, "test.js", "import('m.js')")?;

  let mut hooks = SyncImportHooks::new();
  let mut dummy_host = ();
  let err = rt
    .exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)
    .unwrap_err();
  match err {
    VmError::Unimplemented(msg) => assert_eq!(msg, "dynamic import requires a module graph"),
    other => panic!("expected Unimplemented error, got {other:?}"),
  }

  // Restore the module graph pointer so runtime teardown behaves like normal.
  {
    let (vm, modules, _heap) = rt.vm_modules_and_heap_mut();
    vm.set_module_graph(modules);
  }
  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_dynamic_import_rejects_when_options_not_object() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var p = import('./m.js', 1);
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  // Read `p` off the global object.
  let promise_obj = {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("p")?);
    let promise_value = scope.get_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      global,
      key,
      Value::Object(global),
    )?;
    let Value::Object(obj) = promise_value else {
      panic!("import() should assign a Promise object to global `p`");
    };
    obj
  };

  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  assert_eq!(
    hooks.pending_count(),
    0,
    "host loader should not be invoked"
  );

  let reason = rt
    .heap
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  let Value::Object(err_obj) = reason else {
    panic!("promise rejection reason should be an object");
  };

  let mut scope = rt.heap.scope();
  let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
  let Some(desc) = scope.heap().object_get_own_property(err_obj, &name_key)? else {
    panic!("TypeError should have a 'name' property");
  };
  let PropertyKind::Data { value, .. } = desc.kind else {
    panic!("TypeError.name should be a data property");
  };
  let Value::String(name) = value else {
    panic!("TypeError.name should be a string");
  };
  assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "TypeError");

  drop(scope);
  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_dynamic_import_roots_specifier_across_options_eval_under_gc_stress() -> Result<(), VmError> {
  // Force a GC on every allocation so an unrooted specifier Value would be collected while
  // evaluating the second argument.
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 0));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      // Specifier expression allocates a new string.
      // Options expression allocates enough to trigger GC.
      var p = import(
        ('m' + '.js'),
        (function () {
          let i = 0;
          while (i < 25) {
            ({});
            i = i + 1;
          }
          return 1;
        })()
      );
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  assert!(rt.heap.gc_runs() > 0, "expected at least one GC cycle to run");

  // Read `p` off the global object.
  let promise_obj = {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("p")?);
    let promise_value = scope.get_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      global,
      key,
      Value::Object(global),
    )?;
    let Value::Object(obj) = promise_value else {
      panic!("import() should assign a Promise object to global `p`");
    };
    obj
  };

  // The import should reject due to the invalid options value, but it should not crash due to an
  // invalid/unrooted specifier handle.
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  assert_eq!(
    hooks.pending_count(),
    0,
    "host loader should not be invoked"
  );

  hooks.teardown_jobs(&mut rt);
  Ok(())
}
