use vm_js::{
  ExecutionContext, Heap, HeapLimits, HostDefined, Job, MicrotaskQueue, ModuleCompletion, ModuleGraph,
  ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest, Realm, RealmId, Scope, ScriptId,
  ScriptOrModule, Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

#[derive(Debug)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

/// Host hooks that capture `HostLoadImportedModule` requests so tests can complete them manually.
struct PendingHostHooks {
  microtasks: MicrotaskQueue,
  pending: Vec<PendingLoad>,
}

impl PendingHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      pending: Vec::new(),
    }
  }

  fn take_pending(&mut self) -> PendingLoad {
    assert_eq!(self.pending.len(), 1, "expected exactly one pending load");
    self.pending.remove(0)
  }

  fn teardown_jobs(&mut self, heap: &mut Heap) {
    struct Ctx<'a> {
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("PendingHostHooks::call"))
      }

      fn construct(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("PendingHostHooks::construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut ctx = Ctx { heap };
    self.microtasks.teardown(&mut ctx);
  }
}

impl VmHostHooks for PendingHostHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
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

fn start_import_from_context(
  vm: &mut Vm,
  heap: &mut Heap,
  modules: &mut ModuleGraph,
  host: &mut PendingHostHooks,
  global_object: vm_js::GcObject,
  specifier: &str,
) -> Result<Value, VmError> {
  let mut scope = heap.scope();
  let spec_s = scope.alloc_string(specifier)?;
  vm_js::start_dynamic_import(vm, &mut scope, modules, host, global_object, Value::String(spec_s), Value::Undefined)
}

fn finish_pending_load(
  vm: &mut Vm,
  heap: &mut Heap,
  modules: &mut ModuleGraph,
  host: &mut PendingHostHooks,
  pending: PendingLoad,
  completion: ModuleCompletion,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  vm.finish_loading_imported_module(
    &mut scope,
    modules,
    host,
    pending.referrer,
    pending.request,
    pending.payload,
    completion,
  )
}

#[test]
fn finish_loading_imported_module_caches_script_referrer() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let mut modules = ModuleGraph::new();
  let mut host = PendingHostHooks::new();

  let script_id = ScriptId::from_raw(1);
  let ctx = ExecutionContext {
    realm: realm.id(),
    script_or_module: Some(ScriptOrModule::Script(script_id)),
  };
  let mut vm_ctx = vm.execution_context_guard(ctx)?;

  // First import from the script should cache the `(Script(script_id), request)` mapping.
  let _p1 = start_import_from_context(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    realm.global_object(),
    "./m.js",
  )?;
  let pending1 = host.take_pending();
  assert_eq!(pending1.referrer, ModuleReferrer::Script(script_id));
  finish_pending_load(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    pending1,
    Ok(ModuleId::from_raw(1)),
  )?;

  // Second import from the same Script referrer with the same request must resolve to the same
  // ModuleId; completing it with a different id is an invariant violation.
  let _p2 = start_import_from_context(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    realm.global_object(),
    "./m.js",
  )?;
  let pending2 = host.take_pending();
  assert_eq!(pending2.referrer, ModuleReferrer::Script(script_id));
  let err = finish_pending_load(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    pending2,
    Ok(ModuleId::from_raw(2)),
  )
  .unwrap_err();
  assert!(matches!(
    err,
    VmError::InvariantViolation(
      "FinishLoadingImportedModule invariant violation: module request resolved to different modules"
    )
  ));

  drop(vm_ctx);
  modules.teardown(&mut vm, &mut heap);
  host.teardown_jobs(&mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn finish_loading_imported_module_caches_realm_referrer() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let mut modules = ModuleGraph::new();
  let mut host = PendingHostHooks::new();

  let ctx = ExecutionContext {
    realm: realm.id(),
    script_or_module: None,
  };
  let mut vm_ctx = vm.execution_context_guard(ctx)?;

  let _p1 = start_import_from_context(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    realm.global_object(),
    "./m.js",
  )?;
  let pending1 = host.take_pending();
  assert_eq!(pending1.referrer, ModuleReferrer::Realm(realm.id()));
  finish_pending_load(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    pending1,
    Ok(ModuleId::from_raw(1)),
  )?;

  let _p2 = start_import_from_context(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    realm.global_object(),
    "./m.js",
  )?;
  let pending2 = host.take_pending();
  assert_eq!(pending2.referrer, ModuleReferrer::Realm(realm.id()));
  let err = finish_pending_load(
    &mut vm_ctx,
    &mut heap,
    &mut modules,
    &mut host,
    pending2,
    Ok(ModuleId::from_raw(2)),
  )
  .unwrap_err();
  assert!(matches!(
    err,
    VmError::InvariantViolation(
      "FinishLoadingImportedModule invariant violation: module request resolved to different modules"
    )
  ));

  drop(vm_ctx);
  modules.teardown(&mut vm, &mut heap);
  host.teardown_jobs(&mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

