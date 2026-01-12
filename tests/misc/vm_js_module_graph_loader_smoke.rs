use vm_js::{
  load_requested_modules, HostDefined, Job, ModuleGraph, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, ModuleStatus, PromiseState, Realm, SourceTextModuleRecord, Value,
  Vm, VmError, VmHostHooks, VmOptions,
};

// Lightweight integration-smoke test for vm-js' module graph loader + `finish_loading_imported_module`
// caching semantics.
//
// vm-js has its own unit tests, but keeping a small high-level check here helps catch accidental
// regressions when updating vendor/ecma-rs.

struct TestRealm {
  vm: Vm,
  heap: vm_js::Heap,
  realm: Realm,
}

impl TestRealm {
  fn new() -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(1024 * 1024, 1024 * 1024));
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRealm {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

struct TestHost {
  last_loaded: Option<ModuleId>,
  jobs: std::collections::VecDeque<Job>,
}

impl TestHost {
  fn new() -> Self {
    Self {
      last_loaded: None,
      jobs: std::collections::VecDeque::new(),
    }
  }
}

impl VmHostHooks for TestHost {
  fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<vm_js::RealmId>) {
    self.jobs.push_back(job);
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
    // Synchronously complete the host hook by creating a trivial cyclic module and reporting it as
    // loaded. This re-enters the loader via `finish_loading_imported_module`, matching the spec
    // allowance for synchronous completion.
    let loaded = modules.add_module(SourceTextModuleRecord::default());
    self.last_loaded = Some(loaded);
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(loaded),
    )
  }
}

#[test]
fn module_graph_loader_caches_loaded_modules_and_resolves_promise() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let mut scope = rt.heap.scope();
  let mut modules = ModuleGraph::default();
  let mut host = TestHost::new();

  let request = ModuleRequest::new("dep.js", vec![]);
  let mut referrer_record = SourceTextModuleRecord::default();
  referrer_record.requested_modules.push(request.clone());
  let referrer = modules.add_module(referrer_record);

  let promise = load_requested_modules(
    &mut rt.vm,
    &mut scope,
    &mut modules,
    &mut host,
    referrer,
    HostDefined::default(),
  )?;
  scope.push_root(promise)?;
  let Value::Object(promise) = promise else {
    panic!("expected module graph loader to return a Promise object");
  };
  assert_eq!(
    scope.heap().promise_state(promise)?,
    PromiseState::Fulfilled
  );

  let loaded = host
    .last_loaded
    .expect("host should have been invoked for the requested module");

  let record = modules
    .get_module(referrer)
    .expect("referrer module should exist");
  assert_eq!(record.status, ModuleStatus::Unlinked);
  assert_eq!(record.loaded_modules.len(), 1);
  assert!(record.loaded_modules[0].request.spec_equal(&request));
  assert_eq!(record.loaded_modules[0].module, loaded);

  let loaded_record = modules
    .get_module(loaded)
    .expect("loaded module should exist");
  assert_eq!(loaded_record.status, ModuleStatus::Unlinked);

  Ok(())
}

#[derive(Clone)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

#[derive(Default)]
struct PendingHost {
  pending: Vec<PendingLoad>,
  jobs: std::collections::VecDeque<Job>,
}

impl VmHostHooks for PendingHost {
  fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<vm_js::RealmId>) {
    self.jobs.push_back(job);
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut vm_js::Scope<'_>,
    _modules: &mut ModuleGraph,
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

#[test]
fn module_graph_loader_rejects_duplicate_loaded_module_mismatch() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let mut scope = rt.heap.scope();
  let mut modules = ModuleGraph::default();
  let module1 = modules.add_module(SourceTextModuleRecord::default());
  let module2 = modules.add_module(SourceTextModuleRecord::default());

  let request_dup = ModuleRequest::new("dup.js", vec![]);
  let mut referrer_record = SourceTextModuleRecord::default();
  // Intentionally create duplicate entries in `[[RequestedModules]]` so the loader invokes the host
  // hook twice and exercises `finish_loading_imported_module`'s caching/mismatch logic.
  referrer_record.requested_modules = vec![request_dup.clone(), request_dup.clone()];
  let referrer_module = modules.add_module(referrer_record);

  let mut host = PendingHost::default();

  let promise = load_requested_modules(
    &mut rt.vm,
    &mut scope,
    &mut modules,
    &mut host,
    referrer_module,
    HostDefined::default(),
  )?;
  scope.push_root(promise)?;
  let Value::Object(promise) = promise else {
    panic!("expected module graph loader to return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise)?, PromiseState::Pending);

  // Two modules are requested and none have completed yet.
  assert_eq!(host.pending.len(), 2);

  let PendingLoad {
    referrer: load_referrer,
    request,
    payload,
  } = host.pending[0].clone();
  assert!(request.spec_equal(&request_dup));

  let PendingLoad {
    referrer: load_referrer2,
    request: request2,
    payload: payload2,
  } = host.pending[1].clone();
  assert!(request2.spec_equal(&request_dup));

  // Complete the first load.
  rt.vm.finish_loading_imported_module(
    &mut scope,
    &mut modules,
    &mut host,
    load_referrer,
    request.clone(),
    payload.clone(),
    Ok(module1),
  )?;
  assert_eq!(scope.heap().promise_state(promise)?, PromiseState::Pending);

  // A second completion for the same request with a different module id should be treated as an
  // invariant violation and reject the module-graph-loading promise.
  rt.vm.finish_loading_imported_module(
    &mut scope,
    &mut modules,
    &mut host,
    load_referrer2,
    request2,
    payload2,
    Ok(module2),
  )?;

  assert_eq!(scope.heap().promise_state(promise)?, PromiseState::Rejected);

  let record = modules
    .get_module(referrer_module)
    .expect("referrer module should exist");
  assert_eq!(record.loaded_modules.len(), 1);
  assert_eq!(record.loaded_modules[0].module, module1);

  Ok(())
}
