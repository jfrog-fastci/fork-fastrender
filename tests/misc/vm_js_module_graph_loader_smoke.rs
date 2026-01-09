use vm_js::module_graph_loader::{
  finish_loading_imported_module, load_requested_modules, CyclicModuleRecord, HostDefined,
  ModuleGraphLoadPromiseState, ModuleLoaderHost, ModuleStatus, ModuleStore,
};
use vm_js::{ModuleId, ModuleRequest, VmError};

// Lightweight integration-smoke test for vm-js' module graph loader + `FinishLoadingImportedModule`
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
    payload: vm_js::module_graph_loader::ModuleLoadPayload,
  ) {
    // Synchronously complete the host hook by creating a trivial cyclic module and reporting it as
    // loaded. This re-enters the loader via `finish_loading_imported_module`, matching the spec
    // allowance for synchronous completion.
    let loaded = modules
      .insert_cyclic(CyclicModuleRecord::new(Vec::new()))
      .expect("module store insert should not OOM in smoke test");
    self.last_loaded = Some(loaded);
    finish_loading_imported_module(modules, self, referrer, request, payload, Ok(loaded));
  }
}

#[test]
fn module_graph_loader_caches_loaded_modules_and_resolves_promise() -> Result<(), VmError> {
  let mut modules = ModuleStore::default();
  let mut host = TestHost::new();

  let request = ModuleRequest::new("dep.js", Vec::new());
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

  let loaded_record = modules
    .get_cyclic(loaded)
    .expect("loaded module should exist");
  assert_eq!(loaded_record.status, ModuleStatus::Unlinked);

  Ok(())
}

#[test]
fn cyclic_module_record_rejects_duplicate_loaded_module_mismatch() -> Result<(), VmError> {
  let mut record = CyclicModuleRecord::new(Vec::new());
  let request = ModuleRequest::new("dup.js", Vec::new());
  record.set_loaded_module(request.clone(), ModuleId::from_raw(1))?;

  let err = record
    .set_loaded_module(request, ModuleId::from_raw(2))
    .expect_err("duplicate request mismatch should error");
  match err {
    VmError::InvariantViolation(_) => {}
    other => panic!("expected invariant violation, got {other:?}"),
  }

  Ok(())
}
