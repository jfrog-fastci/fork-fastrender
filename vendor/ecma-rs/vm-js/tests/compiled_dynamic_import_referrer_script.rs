use vm_js::{
  CompiledScript, Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleGraph,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, RealmId, Scope, Value, Vm, VmError, VmHostHooks,
  VmOptions,
};

#[derive(Debug, Default)]
struct CaptureReferrerHooks {
  microtasks: MicrotaskQueue,
  referrers: Vec<ModuleReferrer>,
}

impl CaptureReferrerHooks {
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
          // Hard stop: discard any remaining queued jobs so we don't leak persistent roots.
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

impl VmHostHooks for CaptureReferrerHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    _host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    self.referrers.push(referrer);

    // Immediately complete the load with a thrown error so we don't leave the payload outstanding
    // (which would leak its persistent roots for the import() promise capability).
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Err(VmError::Throw(Value::Undefined)),
    )?;
    Ok(())
  }
}

#[test]
fn compiled_script_dynamic_import_uses_script_referrer_for_promise_jobs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "test.js",
    "import('x'); Promise.resolve().then(() => import('x'));",
  )?;

  let mut hooks = CaptureReferrerHooks::default();
  let mut dummy_host = ();

  rt.exec_compiled_script_with_host_and_hooks(&mut dummy_host, &mut hooks, script)?;

  assert_eq!(
    hooks.referrers.len(),
    1,
    "expected the top-level import() call to reach HostLoadImportedModule synchronously"
  );
  let first_script_id = match hooks.referrers[0] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  hooks.perform_microtask_checkpoint(&mut rt)?;

  assert_eq!(
    hooks.referrers.len(),
    2,
    "expected the Promise job to trigger a second HostLoadImportedModule call"
  );
  let second_script_id = match hooks.referrers[1] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  assert_eq!(
    first_script_id, second_script_id,
    "import() inside a Promise job should preserve the initiating script identity"
  );

  hooks.microtasks.teardown(&mut rt);
  Ok(())
}

