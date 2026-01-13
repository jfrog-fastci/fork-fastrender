use vm_js::{
  Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, RealmId, Scope, Value, Vm, VmError, VmHostHooks, VmOptions,
};

#[derive(Debug, Default)]
struct CaptureReferrerHooks {
  microtasks: MicrotaskQueue,
  referrers: Vec<ModuleReferrer>,
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
fn script_id_is_consumed_even_when_parsing_fails() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut hooks = CaptureReferrerHooks::default();

  rt.exec_script_with_hooks(&mut hooks, "import('x');")?;
  assert_eq!(hooks.referrers.len(), 1);
  let first_id = match hooks.referrers[0] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  // This should fail during parsing (before any evaluation). Even so, the runtime should allocate a
  // ScriptId at the start of the entry point so hosts can deterministically associate metadata with
  // the ScriptId that would have been used if parsing had succeeded.
  let parse_err = rt.exec_script_with_hooks(&mut hooks, "function {");
  assert!(
    matches!(parse_err, Err(VmError::Syntax(_))),
    "expected a syntax error, got {parse_err:?}"
  );

  rt.exec_script_with_hooks(&mut hooks, "import('x');")?;
  assert_eq!(hooks.referrers.len(), 2);
  let second_id = match hooks.referrers[1] {
    ModuleReferrer::Script(id) => id,
    other => panic!("expected ModuleReferrer::Script(_), got {other:?}"),
  };

  assert_eq!(
    second_id.to_raw(),
    first_id
      .to_raw()
      .checked_add(2)
      .expect("ScriptId overflow in test"),
    "expected the parse-failed script to still consume a ScriptId"
  );

  // Discard any queued jobs so `Job` persistent roots are cleaned up before the test ends.
  hooks.microtasks.teardown(&mut rt);
  Ok(())
}
