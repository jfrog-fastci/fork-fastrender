use vm_js::{
  Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, RealmId, Scope, Value, Vm, VmError, VmHostHooks, VmOptions,
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
fn dynamic_import_uses_script_referrer_for_classic_scripts_and_promise_jobs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut hooks = CaptureReferrerHooks::default();

  // One import at top-level (directly in the script), plus one inside a Promise job (microtask).
  rt.exec_script_with_hooks(
    &mut hooks,
    "import('x'); Promise.resolve().then(() => import('x'));",
  )?;

  assert_eq!(
    hooks.referrers.len(),
    1,
    "expected the top-level import() call to reach HostLoadImportedModule synchronously"
  );
  let first_script_id = match hooks.referrers[0] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  // Run the Promise job that executes the `.then` callback (and therefore the second import()).
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

  // Discard any queued jobs so `Job` persistent roots are cleaned up before the test ends.
  hooks.microtasks.teardown(&mut rt);
  Ok(())
}

#[test]
fn dynamic_import_from_function_uses_defining_script_referrer_even_when_called_from_another_script() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut hooks = CaptureReferrerHooks::default();

  // Script #1: define a function that performs import(), and also perform an import() at top-level
  // so we can capture the ScriptId for this script.
  rt.exec_script_with_hooks(&mut hooks, "globalThis.f = () => import('x'); import('x');")?;

  assert_eq!(hooks.referrers.len(), 1);
  let script1_id = match hooks.referrers[0] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  // Script #2: perform one import() directly (should use a fresh ScriptId for this script), then
  // call `f()`. The function call should activate the function's `[[ScriptOrModule]]` (from script
  // #1), so the import() inside `f` uses script #1's referrer ScriptId.
  rt.exec_script_with_hooks(&mut hooks, "import('x'); f();")?;

  assert_eq!(hooks.referrers.len(), 3);
  let script2_id = match hooks.referrers[1] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };
  let from_f_id = match hooks.referrers[2] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  assert_ne!(
    script2_id, script1_id,
    "expected separate ScriptIds for distinct script executions"
  );
  assert_eq!(
    from_f_id, script1_id,
    "import() inside a function should use the function's defining ScriptId as the referrer, even when called from another script"
  );

  hooks.microtasks.teardown(&mut rt);
  Ok(())
}
