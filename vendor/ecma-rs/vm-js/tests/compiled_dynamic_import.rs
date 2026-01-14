use std::collections::HashMap;

use vm_js::{
  CompiledScript, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleId, ModuleLoadPayload,
  JsString, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, PropertyKind,
  SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmOptions,
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
  modules: HashMap<JsString, ModuleId>,
  pending: Vec<PendingLoad>,
  supported_import_attributes: &'static [&'static str],
}

impl TestHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
      pending: Vec::new(),
      supported_import_attributes: &[],
    }
  }

  fn new_with_supported_import_attributes(supported: &'static [&'static str]) -> Self {
    Self {
      supported_import_attributes: supported,
      ..Self::new()
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
  }

  fn pending_count(&self) -> usize {
    self.pending.len()
  }

  fn complete_load_for(&mut self, rt: &mut JsRuntime, specifier: &str) {
    let spec = JsString::from_str(specifier).unwrap();
    let idx = self
      .pending
      .iter()
      .position(|p| p.request.specifier == spec)
      .unwrap_or_else(|| panic!("no pending module load for specifier {specifier:?}"));
    let pending = self.pending.remove(idx);

    let module = *self
      .modules
      .get(&spec)
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
    self.supported_import_attributes
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
  modules: HashMap<JsString, ModuleId>,
}

impl SyncImportHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
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
      .get(&module_request.specifier)
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
    VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => {
      let Value::Object(err_obj) = reason else {
        panic!("expected dynamic import error to throw an object, got {reason:?}");
      };
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(err_obj))?;
      // `coerce_error_to_throw` prefixes unimplemented errors with `unimplemented:`.
      let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
      let message = scope
        .heap()
        .object_get_own_data_property_value(err_obj, &message_key)?
        .expect("expected own message property");
      let Value::String(message_s) = message else {
        panic!("expected Error.message to be a string, got {message:?}");
      };
      let msg = scope.heap().get_string(message_s)?.to_utf8_lossy();
      assert!(
        msg.contains("dynamic import requires a module graph"),
        "expected message to mention missing module graph, got {msg:?}"
      );
    }
    other => panic!("expected unimplemented dynamic import error, got {other:?}"),
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
fn compiled_dynamic_import_rejects_when_with_not_object() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var p = import('./m.js', { with: 1 });
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

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
fn compiled_dynamic_import_rejects_when_attribute_value_not_string() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Even though the host supports no import attributes, the attribute value type check happens
  // before the supported-key check.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var p = import('./m.js', { with: { type: 1 } });
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

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
fn compiled_dynamic_import_rejects_unsupported_import_attributes() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Default `host_get_supported_import_attributes` returns an empty list; "type" is unsupported.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var p = import('./m.js', { with: { type: 'json' } });
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

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
fn compiled_dynamic_import_allows_supported_import_attributes() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let m_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const x = 1;")?;
  let m = rt.modules_mut().add_module(m_record)?;

  static SUPPORTED: [&str; 2] = ["foo", "type"];
  let mut hooks = TestHostHooks::new_with_supported_import_attributes(&SUPPORTED);
  hooks.register_module("./m.js", m);

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var p = import('./m.js', { with: { type: 'json', foo: 'bar' } });
    "#,
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

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

  assert_eq!(hooks.pending_count(), 1, "host loader should be invoked");
  let attrs = &hooks.pending[0].request.attributes;
  assert_eq!(attrs.len(), 2, "expected two import attributes");
  assert_eq!(attrs[0].key, JsString::from_str("foo")?);
  assert_eq!(attrs[0].value, JsString::from_str("bar")?);
  assert_eq!(attrs[1].key, JsString::from_str("type")?);
  assert_eq!(attrs[1].value, JsString::from_str("json")?);

  hooks.complete_load_for(&mut rt, "./m.js");
  assert_eq!(hooks.pending_count(), 0);
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
fn compiled_dynamic_import_options_proxy_get_trap_is_observed() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut hooks = TestHostHooks::new();

  let global = rt.realm().global_object();

  // Create a Proxy target + handler in JS so the traps can mutate JS-visible state (`log`).
  rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      var log = "";
      var __opts_target = { with: { type: "json" } };
      var __opts_handler = {
        get: function (t, k, r) {
          log += "get:" + String(k) + ",";
          return Reflect.get(t, k, r);
        }
      };
    "#,
  )?;

  let Value::Object(target) = rt.exec_script_with_hooks(&mut hooks, "__opts_target")? else {
    return Err(VmError::InvariantViolation("__opts_target should be an object"));
  };
  let Value::Object(handler) = rt.exec_script_with_hooks(&mut hooks, "__opts_handler")? else {
    return Err(VmError::InvariantViolation("__opts_handler should be an object"));
  };

  // Install `opts` as a host-created Proxy object.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

    scope.push_root(Value::Object(global))?;
    scope.push_root(Value::Object(proxy))?;
    let key_s = scope.alloc_string("opts")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.create_data_property_or_throw(global, key, Value::Object(proxy))?;
  }

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      // Host supports no import attributes, so this should reject before invoking HostLoadImportedModule.
      var p = import('./m.js', opts);
    "#,
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  let promise_obj = {
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
  assert_eq!(hooks.pending_count(), 0, "host loader should not be invoked");

  let log_val = {
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
    return Err(VmError::InvariantViolation("expected global `log` to be a string"));
  };
  let log = rt.heap.get_string(log_s)?.to_utf8_lossy();
  assert!(
    log.contains("get:with"),
    "expected options Proxy get trap to be invoked for 'with', got {log:?}"
  );

  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_dynamic_import_attributes_proxy_traps_are_observed() -> Result<(), VmError> {
  // Proxy-based attribute enumeration triggers a handful of allocations during import option
  // normalization; keep the heap small to catch leaks, but large enough to cover the Proxy paths.
  let mut rt = new_runtime_with_heap_limit(2 * 1024 * 1024)?;
  let mut hooks = TestHostHooks::new();

  // Create a Proxy target + handler in JS so the traps can mutate JS-visible state (`log`).
  rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      var log = "";
      var __attrs_target = { type: "json" };
      var __attrs_handler = {
        ownKeys: function (t) {
          log += "ownKeys,";
          return ["type"];
        },
        getOwnPropertyDescriptor: function (t, k) {
          log += "gopd:" + String(k) + ",";
          // vm-js currently exposes `Reflect.getOwnPropertyDescriptor` but may not implement
          // `Object.getOwnPropertyDescriptor` yet. Use Reflect so the trap can return a complete
          // descriptor object and attribute processing can continue to the `Get` path.
          return Reflect.getOwnPropertyDescriptor(t, k);
        },
        get: function (t, k, r) {
          log += "get:" + String(k) + ",";
          return Reflect.get(t, k, r);
        },
      };
      var __opts = {};
    "#,
  )?;

  let Value::Object(target) = rt.exec_script_with_hooks(&mut hooks, "__attrs_target")? else {
    return Err(VmError::InvariantViolation("__attrs_target should be an object"));
  };
  let Value::Object(handler) = rt.exec_script_with_hooks(&mut hooks, "__attrs_handler")? else {
    return Err(VmError::InvariantViolation("__attrs_handler should be an object"));
  };
  let Value::Object(opts) = rt.exec_script_with_hooks(&mut hooks, "__opts")? else {
    return Err(VmError::InvariantViolation("__opts should be an object"));
  };

  // Install `__opts.with` as a host-created Proxy object.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(opts))?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    scope.push_root(Value::Object(proxy))?;

    let with_key_s = scope.alloc_string("with")?;
    scope.push_root(Value::String(with_key_s))?;
    let with_key = PropertyKey::from_string(with_key_s);
    scope.create_data_property_or_throw(opts, with_key, Value::Object(proxy))?;
  }

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      // Host supports no import attributes, so this should reject before invoking HostLoadImportedModule.
      var p = import('./m.js', __opts);
    "#,
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

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
  assert_eq!(hooks.pending_count(), 0, "host loader should not be invoked");

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
    return Err(VmError::InvariantViolation("expected global `log` to be a string"));
  };
  let log = rt.heap.get_string(log_s)?.to_utf8_lossy();
  assert!(
    log.contains("ownKeys"),
    "expected attributes Proxy ownKeys trap to be invoked, got {log:?}"
  );
  assert!(
    log.contains("gopd:type"),
    "expected attributes Proxy getOwnPropertyDescriptor trap to be invoked, got {log:?}"
  );
  assert!(
    log.contains("get:type"),
    "expected attributes Proxy get trap to be invoked for 'type', got {log:?}"
  );

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

#[test]
fn compiled_dynamic_import_evaluates_options_even_when_specifier_to_string_throws() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var log = "";
      function o() { log = log + "o"; return undefined; }
      // `ToString(Symbol(..))` throws a TypeError, but the options expression must still be evaluated.
      var p = import(Symbol('x'), o());
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  // Ensure the options expression was evaluated.
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
  assert_eq!(rt.heap.get_string(log_s)?.to_utf8_lossy(), "o");

  // Promise should be rejected with TypeError and no host load should be requested.
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
fn compiled_dynamic_import_does_not_evaluate_options_when_specifier_expr_throws() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    r#"
      var log = "";
      function s() { log = log + "s"; throw 1; }
      function o() { log = log + "o"; return undefined; }
      try {
        import(s(), o());
      } catch (e) {
        log = log + "c";
      }
    "#,
  )?;

  let mut hooks = TestHostHooks::new();
  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  assert_eq!(
    hooks.pending_count(),
    0,
    "host loader should not be invoked when specifier evaluation throws"
  );

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
    "sc",
    "expected options expression not to run when specifier evaluation throws"
  );

  hooks.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn compiled_dynamic_import_from_promise_callback_works() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Build a tiny module graph with a dependency.
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

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    "var result; Promise.resolve().then(function(){ return import('./m.js'); }).then(function(ns){ result = ns; });",
  )?;

  let mut dummy_host = ();
  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  // No microtask checkpoint yet → no dynamic import started.
  assert_eq!(hooks.pending_count(), 0);

  hooks.perform_microtask_checkpoint(&mut rt)?;
  assert_eq!(hooks.pending_count(), 1);

  hooks.complete_load_for(&mut rt, "./m.js");
  hooks.complete_load_for(&mut rt, "./dep.js");

  // Run the `.then(ns => result = ns)` callback (and any Promise jobs enqueued by dynamic import).
  hooks.perform_microtask_checkpoint(&mut rt)?;

  // Read `result` off the global object.
  let result_value = {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("result")?);
    scope.get_with_host_and_hooks(
      &mut rt.vm,
      &mut dummy_host,
      &mut hooks,
      global,
      key,
      Value::Object(global),
    )?
  };
  let Value::Object(ns_obj) = result_value else {
    return Err(VmError::InvariantViolation(
      "result should be set to a namespace object",
    ));
  };

  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let x_value = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut dummy_host,
    &mut hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  let y_value = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut dummy_host,
    &mut hooks,
    ns_obj,
    y_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  hooks.teardown_jobs(&mut rt);
  Ok(())
}
