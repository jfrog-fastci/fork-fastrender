use std::collections::HashMap;

use vm_js::{
  GcObject, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleId, ModuleLoadPayload,
  JsString, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, PropertyKind, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmOptions,
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
  modules: HashMap<JsString, ModuleId>,
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
struct SyncHostHooks {
  microtasks: MicrotaskQueue,
  /// Specifier → module id mapping used by `host_load_imported_module`.
  modules: HashMap<JsString, ModuleId>,
}

impl SyncHostHooks {
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

impl VmHostHooks for SyncHostHooks {
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

fn define_global(scope: &mut Scope<'_>, global: GcObject, name: &str, value: Value) -> Result<(), VmError> {
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.create_data_property_or_throw(global, key, value)
}

fn expect_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap()
    .get_string(s)
    .expect("string handle should be valid")
    .to_utf8_lossy()
}

#[test]
fn dynamic_import_resolves_to_module_namespace() -> Result<(), VmError> {
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
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "promise root should reference an object",
    ));
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  // Complete the dependency load and drain again. This should fulfill the dynamic import promise
  // with the module namespace.
  host.complete_load_for(&mut rt, "./dep.js");
  // `ContinueDynamicImport` uses `PerformPromiseThen` even when module evaluation completes
  // synchronously, so the import() promise is fulfilled via a microtask.
  host.perform_microtask_checkpoint(&mut rt)?;

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

  let mut dummy_host = ();
  let desc_x = scope
    .object_get_own_property_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key)?
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
    .object_get_own_property_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, y_key)?
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
  let x_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key, Value::Object(ns_obj))?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  let y_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, y_key, Value::Object(ns_obj))?;
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_sync_host_completion_fulfills_promise() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Build a tiny module graph with a dependency.
  let dep_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const y = 1;")?;
  let dep = rt.modules_mut().add_module(dep_record)?;
  let m_record = SourceTextModuleRecord::parse(
    &mut rt.heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut host = SyncHostHooks::new();
  host.register_module("./m.js", m);
  host.register_module("./dep.js", dep);

  // Synchronous host completion should still produce a pending import() promise; fulfillment is
  // driven by Promise microtasks (`PerformPromiseThen`).
  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./m.js')")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };

  // Even when the host completes module loading synchronously, `ContinueDynamicImport` settles the
  // import() promise via a microtask (per `PerformPromiseThen`).
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  host.perform_microtask_checkpoint(&mut rt)?;

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

  // Reading the exported bindings should reflect evaluated module state.
  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let mut dummy_host = ();
  let x_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key, Value::Object(ns_obj))?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  let y_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, y_key, Value::Object(ns_obj))?;
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_rejects_when_module_evaluation_throws() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // A module that throws during evaluation should cause the dynamic import promise to reject with
  // the thrown value (via `PerformPromiseThen(evaluatePromise, ...)`).
  let err_record = SourceTextModuleRecord::parse(&mut rt.heap, "throw 1;")?;
  let err = rt.modules_mut().add_module(err_record)?;

  let mut host = SyncHostHooks::new();
  host.register_module("./err.js", err);

  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./err.js')")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "import() should evaluate to a Promise object",
    ));
  };

  // Even though evaluation fails synchronously, `ContinueDynamicImport` settles the import()
  // promise via a Promise reaction job (microtask).
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  host.perform_microtask_checkpoint(&mut rt)?;

  let promise_value = rt
    .heap
    .get_root(promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "promise root should reference an object",
    ));
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = rt
    .heap
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert!(matches!(reason, Value::Number(n) if n == 1.0));

  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_waits_for_top_level_await() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // A module that uses top-level await should produce a pending evaluation promise, and dynamic
  // import should wait for it before resolving the import() promise.
  let tla_record = SourceTextModuleRecord::parse(
    &mut rt.heap,
    "await Promise.resolve(); export const x = 1;",
  )?;
  let tla = rt.modules_mut().add_module(tla_record)?;

  let mut host = TestHostHooks::new();
  host.register_module("./tla.js", tla);

  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./tla.js')")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);
  assert_eq!(host.pending_count(), 1);

  host.complete_load_for(&mut rt, "./tla.js");

  let promise_value = rt
    .heap
    .get_root(promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::InvariantViolation(
      "promise root should reference an object",
    ));
  };
  assert_eq!(
    rt.heap.promise_state(promise_obj)?,
    PromiseState::Pending,
    "import() promise should stay pending while module evaluation is pending (TLA)"
  );

  host.perform_microtask_checkpoint(&mut rt)?;

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
    return Err(VmError::InvariantViolation(
      "dynamic import promise should fulfill to a namespace object",
    ));
  };

  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let mut dummy_host = ();
  let x_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key, Value::Object(ns_obj))?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  drop(scope);
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_from_function_body_works() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let dep_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const y = 1;")?;
  let dep = rt.modules_mut().add_module(dep_record)?;
  let m_record = SourceTextModuleRecord::parse(
    &mut rt.heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut host = TestHostHooks::new();
  host.register_module("./m.js", m);
  host.register_module("./dep.js", dep);

  // Exercise `import()` from inside an invoked function body (nested ECMAScript call).
  let promise_value = rt.exec_script_with_hooks(
    &mut host,
    "function f(){ return import('./m.js'); } f();",
  )?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  host.complete_load_for(&mut rt, "./m.js");
  host.complete_load_for(&mut rt, "./dep.js");
  host.perform_microtask_checkpoint(&mut rt)?;

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

  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let mut dummy_host = ();
  let x_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key, Value::Object(ns_obj))?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));

  let y_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, y_key, Value::Object(ns_obj))?;
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_with_awaited_specifier_works() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let dep_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const y = 1;")?;
  let dep = rt.modules_mut().add_module(dep_record)?;
  let m_record = SourceTextModuleRecord::parse(
    &mut rt.heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut host = TestHostHooks::new();
  host.register_module("./m.js", m);
  host.register_module("./dep.js", dep);

  // Exercise `import()` where the specifier expression itself contains an `await`.
  rt.exec_script_with_hooks(
    &mut host,
    r#"
      var result;
      async function f() {
        return import(await Promise.resolve("./m.js"));
      }
      f().then(function (ns) { result = ns; });
    "#,
  )?;

  // `f` should suspend on the awaited specifier before reaching the dynamic import.
  assert_eq!(host.pending_count(), 0);

  host.perform_microtask_checkpoint(&mut rt)?;

  // Resumption should trigger the dynamic import and enqueue a host module load.
  assert_eq!(host.pending_count(), 1);
  host.complete_load_for(&mut rt, "./m.js");
  assert_eq!(host.pending_count(), 1);
  host.complete_load_for(&mut rt, "./dep.js");

  // Run the `.then(ns => result = ns)` callback.
  host.perform_microtask_checkpoint(&mut rt)?;

  let result_value = rt.exec_script_with_hooks(&mut host, "result")?;
  let Value::Object(ns_obj) = result_value else {
    return Err(VmError::InvariantViolation(
      "result should be set to a namespace object",
    ));
  };

  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let mut dummy_host = ();
  let x_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key, Value::Object(ns_obj))?;
  let y_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, y_key, Value::Object(ns_obj))?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_from_promise_callback_works() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let dep_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const y = 1;")?;
  let dep = rt.modules_mut().add_module(dep_record)?;
  let m_record = SourceTextModuleRecord::parse(
    &mut rt.heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut host = TestHostHooks::new();
  host.register_module("./m.js", m);
  host.register_module("./dep.js", dep);

  // `import()` should work from Promise callbacks (microtasks). This exercises VM execution paths
  // that call back into the evaluator via `VmJobContext::call`.
  rt.exec_script_with_hooks(
    &mut host,
    "var result; Promise.resolve().then(function(){ return import('./m.js'); }).then(function(ns){ result = ns; });",
  )?;

  // No microtask checkpoint yet → no dynamic import started.
  assert_eq!(host.pending_count(), 0);

  host.perform_microtask_checkpoint(&mut rt)?;
  assert_eq!(host.pending_count(), 1);

  host.complete_load_for(&mut rt, "./m.js");
  host.complete_load_for(&mut rt, "./dep.js");

  // Run the `.then(ns => result = ns)` callback.
  host.perform_microtask_checkpoint(&mut rt)?;

  let result_value = rt.exec_script_with_hooks(&mut host, "result")?;
  let Value::Object(ns_obj) = result_value else {
    return Err(VmError::InvariantViolation(
      "result should be set to a namespace object",
    ));
  };

  let mut scope = rt.heap.scope();
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let mut dummy_host = ();
  let x_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, x_key, Value::Object(ns_obj))?;
  let y_value =
    scope.get_with_host_and_hooks(&mut rt.vm, &mut dummy_host, &mut host, ns_obj, y_key, Value::Object(ns_obj))?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  drop(scope);
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
fn dynamic_import_rejects_when_with_not_object() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = TestHostHooks::new();

  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./m.js', { with: 1 })")?;
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
fn dynamic_import_rejects_when_attribute_value_not_string() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = TestHostHooks::new();

  // Even though the host supports no import attributes, the attribute value type check happens
  // before the supported-key check.
  let promise_value =
    rt.exec_script_with_hooks(&mut host, "import('./m.js', { with: { type: 1 } })")?;
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

#[test]
fn dynamic_import_options_proxy_get_trap_is_observed() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let mut host = TestHostHooks::new();

  let global = rt.realm().global_object();

  // Create a Proxy target + handler in JS so the traps can mutate JS-visible state (`log`).
  rt.exec_script_with_hooks(
    &mut host,
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

  let Value::Object(target) = rt.exec_script_with_hooks(&mut host, "__opts_target")? else {
    return Err(VmError::InvariantViolation("__opts_target should be an object"));
  };
  let Value::Object(handler) = rt.exec_script_with_hooks(&mut host, "__opts_handler")? else {
    return Err(VmError::InvariantViolation("__opts_handler should be an object"));
  };

  // Install `opts` as a host-created Proxy object.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "opts", Value::Object(proxy))?;
  }

  // Host supports no import attributes, so this should reject before invoking `HostLoadImportedModule`.
  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./m.js', opts)")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  assert_eq!(host.pending_count(), 0, "host loader should not be invoked");

  let log_value = rt.exec_script_with_hooks(&mut host, "log")?;
  let log = expect_string(&rt, log_value);
  assert!(
    log.contains("get:with"),
    "expected options Proxy get trap to be invoked for 'with', got {log:?}"
  );

  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_attributes_proxy_traps_are_observed() -> Result<(), VmError> {
  // Proxy-based attribute enumeration triggers a handful of allocations during import option
  // normalization; keep the heap small to catch leaks, but large enough to cover the Proxy paths.
  let mut rt = new_runtime_with_heap_limit(2 * 1024 * 1024)?;
  let mut host = TestHostHooks::new();

  // Create a Proxy target + handler in JS so the traps can mutate JS-visible state (`log`).
  rt.exec_script_with_hooks(
    &mut host,
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

  let Value::Object(target) = rt.exec_script_with_hooks(&mut host, "__attrs_target")? else {
    return Err(VmError::InvariantViolation("__attrs_target should be an object"));
  };
  let Value::Object(handler) = rt.exec_script_with_hooks(&mut host, "__attrs_handler")? else {
    return Err(VmError::InvariantViolation("__attrs_handler should be an object"));
  };
  let Value::Object(opts) = rt.exec_script_with_hooks(&mut host, "__opts")? else {
    return Err(VmError::InvariantViolation("__opts should be an object"));
  };

  // Install `opts.with` as a host-created Proxy object.
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

  // Host supports no import attributes, so this should reject before invoking `HostLoadImportedModule`.
  let promise_value = rt.exec_script_with_hooks(&mut host, "import('./m.js', __opts)")?;
  let promise_root = rt.heap.add_root(promise_value)?;

  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  assert_eq!(host.pending_count(), 0, "host loader should not be invoked");

  let log_value = rt.exec_script_with_hooks(&mut host, "log")?;
  let log = expect_string(&rt, log_value);
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

  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_rejects_escape_sequences_in_import_keyword() -> Result<(), VmError> {
  // test262: language/expressions/dynamic-import/escape-sequence-import.js
  let mut rt = new_runtime()?;
  let err = rt.exec_script(r"im\u0070ort('./empty_FIXTURE.js');").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
  Ok(())
}
