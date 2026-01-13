use vm_js::{
  Budget, ExecutionContext, Heap, HeapLimits, HostDefined, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, Realm, RealmId, Scope, TerminationReason, Value, Vm, VmError,
  VmHostHooks, VmOptions,
};

struct Host;

impl VmHostHooks for Host {
  fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<RealmId>) {}

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _modules: &mut ModuleGraph,
    _referrer: ModuleReferrer,
    _module_request: ModuleRequest,
    _host_defined: HostDefined,
    _payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    // This test is specifically about ensuring the specifier conversion (GcString → Rust String)
    // is budgeted. If we reached the host hook, it means the conversion loop did not run out of
    // fuel early enough.
    Ok(())
  }
}

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

#[test]
fn dynamic_import_specifier_decoding_consumes_fuel() {
  let mut vm = Vm::new(VmOptions {
    check_time_every: 1,
    ..VmOptions::default()
  });
  let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

  // Ensure we always call `Realm::teardown` even if the test panics, otherwise `Realm`'s `Drop`
  // will panic in debug builds.
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let mut host = Host;
    let mut modules = ModuleGraph::new();
    let mut scope = heap.scope();

    // Use a large specifier so that decoding the UTF-16 string into a Rust `String` must yield
    // enough ticks to exhaust a small fuel budget. This specifically guards against un-ticked
    // conversion loops.
    let spec = "a".repeat(200_000);
    let specifier = scope.alloc_string(&spec).unwrap();

    let ctx = ExecutionContext {
      realm: realm.id(),
      script_or_module: None,
    };
    vm.push_execution_context(ctx).unwrap();

    vm.set_budget(Budget {
      fuel: Some(50),
      deadline: None,
      check_time_every: 1,
    });

    let err = vm_js::start_dynamic_import(
      &mut vm,
      &mut scope,
      &mut modules,
      &mut host,
      realm.global_object(),
      Value::String(specifier),
      Value::Undefined,
    )
    .unwrap_err();

    assert_termination_reason(err, TerminationReason::OutOfFuel);

    let popped = vm.pop_execution_context();
    assert_eq!(popped, Some(ctx));
  }));

  realm.teardown(&mut heap);
  if let Err(panic) = result {
    std::panic::resume_unwind(panic);
  }
}
