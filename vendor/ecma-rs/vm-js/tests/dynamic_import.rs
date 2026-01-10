use std::collections::HashMap;

use vm_js::{
  HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleId, ModuleLoadPayload, ModuleReferrer,
  ModuleRequest, PromiseState, PropertyKey, PropertyKind, SourceTextModuleRecord, Value, Vm, VmError,
  VmHostHooks, VmOptions,
};

#[derive(Debug)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

/// Minimal host hook implementation for exercising dynamic `import()`.
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

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn dynamic_import_resolves_to_module_namespace() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Build a tiny module graph:
  // - ./m.js re-exports `y` from ./dep.js and exports `x`.
  // - ./dep.js exports `y`.
  let dep = rt
    .modules_mut()
    .add_module(SourceTextModuleRecord::parse("export const y = 1;")?);
  let m = rt.modules_mut().add_module(SourceTextModuleRecord::parse(
    "export { y } from './dep.js'; export const x = 1;",
  )?);

  let mut host = TestHostHooks::new();
  host.register_module("./m.js", m);
  host.register_module("./dep.js", dep);

  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./m.js')")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  assert_eq!(host.pending_count(), 1);
  host.complete_load_for(&mut rt, "./m.js");
  assert_eq!(host.pending_count(), 1);

  let promise_value = rt
    .heap
    .get_root(promise_root)
    .ok_or(VmError::InvalidHandle)?;
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "promise root should reference an object",
    ));
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  // Complete the dependency load and drain again. This should fulfill the dynamic import promise
  // with the module namespace.
  host.complete_load_for(&mut rt, "./dep.js");

  let promise_value = rt
    .heap
    .get_root(promise_root)
    .ok_or(VmError::InvalidHandle)?;
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
    .heap()
    .object_get_own_property(ns_obj, &x_key)?
    .expect("namespace should have an 'x' export");
  assert!(desc_x.enumerable);
  assert!(!desc_x.configurable);
  assert!(matches!(
    desc_x.kind,
    PropertyKind::Accessor {
      get: Value::Object(_),
      set: Value::Undefined,
    }
  ));

  let desc_y = scope
    .heap()
    .object_get_own_property(ns_obj, &y_key)?
    .expect("namespace should have a 'y' export");
  assert!(desc_y.enumerable);
  assert!(!desc_y.configurable);
  assert!(matches!(
    desc_y.kind,
    PropertyKind::Accessor {
      get: Value::Object(_),
      set: Value::Undefined,
    }
  ));

  // Reading the exported bindings should reflect evaluated module state.
  let x_value = scope.ordinary_get_with_host(
    &mut rt.vm,
    &mut host,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  let y_value = scope.ordinary_get_with_host(
    &mut rt.vm,
    &mut host,
    ns_obj,
    y_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_rejects_when_options_not_object() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = TestHostHooks::new();

  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./m.js', 1)")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  assert_eq!(host.pending_count(), 0, "host loader should not be invoked");

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
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_rejects_unsupported_import_attributes() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = TestHostHooks::new();

  // Default `host_get_supported_import_attributes` returns an empty list; "type" is unsupported.
  let promise_value = rt.exec_script_with_hooks(
    &mut host,
    "import('./m.js', { with: { type: 'json' } })",
  )?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  assert_eq!(host.pending_count(), 0, "host loader should not be invoked");

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
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}
