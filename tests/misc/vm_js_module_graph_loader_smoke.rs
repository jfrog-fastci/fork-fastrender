use vm_js::module_graph_loader::{
  finish_loading_imported_module, load_requested_modules, CyclicModuleRecord, HostDefined,
  ModuleGraphLoadPromiseState, ModuleLoadPayload, ModuleLoaderHost, ModuleStatus, ModuleStore,
};
use vm_js::{ModuleId, ModuleRequest, VmError};

// Lightweight integration-smoke test for vm-js' module graph loader + `finish_loading_imported_module`
// caching semantics.
//
// vm-js has its own unit tests, but keeping a small high-level check here helps catch accidental
// regressions when bumping the engines/ecma-rs submodule.

struct TestHost {
  last_loaded: Option<ModuleId>,
}

impl TestHost {
  fn new() -> Self {
    Self { last_loaded: None }
  }
}

impl ModuleLoaderHost for TestHost {
  fn host_load_imported_module(
    &mut self,
    modules: &mut ModuleStore,
    referrer: ModuleId,
    request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) {
    // Synchronously complete the host hook by creating a trivial cyclic module and reporting it as
    // loaded. This re-enters the loader via `finish_loading_imported_module`, matching the spec
    // allowance for synchronous completion.
    let loaded = match modules.insert_cyclic(CyclicModuleRecord::new(Vec::new())) {
      Ok(loaded) => loaded,
      Err(err) => {
        finish_loading_imported_module(modules, self, referrer, request, payload, Err(err));
        return;
      }
    };
    self.last_loaded = Some(loaded);
    finish_loading_imported_module(modules, self, referrer, request, payload, Ok(loaded));
  }
}

#[test]
fn module_graph_loader_caches_loaded_modules_and_resolves_promise() -> Result<(), VmError> {
  let mut modules = ModuleStore::default();
  let mut host = TestHost::new();

  let request = ModuleRequest::new("dep.js", vec![]);
  let referrer = modules.insert_cyclic(CyclicModuleRecord::new(vec![request.clone()]))?;

  let promise = load_requested_modules(&mut modules, &mut host, referrer, HostDefined::default());
  assert_eq!(promise.state(), ModuleGraphLoadPromiseState::Fulfilled);

  let loaded = host
    .last_loaded
    .expect("host should have been invoked for the requested module");

  let record = modules
    .get_cyclic(referrer)
    .expect("referrer module should exist");
  assert_eq!(record.status, ModuleStatus::Unlinked);
  assert_eq!(record.loaded_modules.len(), 1);
  assert!(record.loaded_modules[0].request.spec_equal(&request));
  assert_eq!(record.loaded_modules[0].module, loaded);

  let loaded_record = modules.get_cyclic(loaded).expect("loaded module should exist");
  assert_eq!(loaded_record.status, ModuleStatus::Unlinked);

  Ok(())
}

#[derive(Clone)]
struct PendingLoad {
  referrer: ModuleId,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

#[derive(Default)]
struct PendingHost {
  pending: Vec<PendingLoad>,
}

impl ModuleLoaderHost for PendingHost {
  fn host_load_imported_module(
    &mut self,
    _modules: &mut ModuleStore,
    referrer: ModuleId,
    request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) {
    self.pending.push(PendingLoad {
      referrer,
      request,
      payload,
    });
  }
}

#[test]
fn module_graph_loader_rejects_duplicate_loaded_module_mismatch() -> Result<(), VmError> {
  let mut modules = ModuleStore::default();
  let module1 = modules.insert_cyclic(CyclicModuleRecord::new(Vec::new()))?;
  let module2 = modules.insert_cyclic(CyclicModuleRecord::new(Vec::new()))?;

  let request_dup = ModuleRequest::new("dup.js", vec![]);
  let referrer = modules.insert_cyclic(CyclicModuleRecord::new(vec![
    request_dup.clone(),
    request_dup.clone(),
  ]))?;

  let mut host = PendingHost::default();

  let promise = load_requested_modules(&mut modules, &mut host, referrer, HostDefined::default());
  assert_eq!(promise.state(), ModuleGraphLoadPromiseState::Pending);

  // Two modules are requested and none have completed yet.
  assert_eq!(host.pending.len(), 2);

  let PendingLoad {
    referrer,
    request,
    payload,
  } = host.pending[0].clone();
  assert!(request.spec_equal(&request_dup));

  let PendingLoad {
    referrer: referrer2,
    request: request2,
    payload: payload2,
  } = host.pending[1].clone();
  assert!(request2.spec_equal(&request_dup));

  // Complete the first load.
  finish_loading_imported_module(
    &mut modules,
    &mut host,
    referrer,
    request.clone(),
    payload.clone(),
    Ok(module1),
  );
  assert_eq!(promise.state(), ModuleGraphLoadPromiseState::Pending);

  // A second completion for the same request with a different module id should be treated as an
  // invariant violation and reject the module-graph-loading promise.
  finish_loading_imported_module(
    &mut modules,
    &mut host,
    referrer2,
    request2,
    payload2,
    Ok(module2),
  );

  assert!(
    matches!(
      promise.state(),
      ModuleGraphLoadPromiseState::Rejected(VmError::InvariantViolation(_))
    ),
    "expected loader promise rejection due to duplicate request resolving to different module"
  );

  let record = modules.get_cyclic(referrer).expect("referrer module should exist");
  assert_eq!(record.loaded_modules.len(), 1);
  assert_eq!(record.loaded_modules[0].module, module1);

  Ok(())
}
