use vm_js::{
  Budget, ExecutionContext, Heap, HeapLimits, HostDefined, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId,
  Scope, TerminationReason, Value, Vm, VmError, VmHostHooks, VmOptions,
};

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

fn make_supported_keys(count: usize) -> &'static [&'static str] {
  let mut out: Vec<&'static str> = Vec::with_capacity(count);
  for i in 0..count {
    let s = format!("k{i}");
    out.push(Box::leak(s.into_boxed_str()));
  }
  Box::leak(out.into_boxed_slice())
}

fn shuffled_indices(count: usize) -> Vec<usize> {
  let mut out: Vec<usize> = (0..count).collect();

  // Deterministic Fisher–Yates shuffle (avoid pulling in `rand` for a test).
  let mut seed: u64 = 0x243F_6A88_85A3_08D3;
  for i in (1..out.len()).rev() {
    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let j = (seed % ((i + 1) as u64)) as usize;
    out.swap(i, j);
  }

  out
}

struct Host {
  supported: &'static [&'static str],
}

impl VmHostHooks for Host {
  fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<RealmId>) {}

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    self.supported
  }

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
    Ok(())
  }
}

#[test]
fn dynamic_import_attribute_sort_consumes_fuel() {
  let mut vm = Vm::new(VmOptions {
    check_time_every: 1,
    ..VmOptions::default()
  });
  let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

  // Ensure we always call `Realm::teardown` even if the test panics, otherwise `Realm`'s `Drop`
  // will panic in debug builds.
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    // Large enough to require many comparisons during canonicalization, but small enough to keep
    // the test fast. (A fully-supported 50k-key attribute set would make the spec-mandated
    // supported-key scan quadratic and too slow for CI.)
    const ATTR_COUNT: usize = 2_000;
    // Provide a small post-validation fuel margin so we reach the sort. The sort itself should
    // consume fuel and terminate.
    const EXTRA_FUEL_BEFORE_SORT: u64 = 10;

    let supported = make_supported_keys(ATTR_COUNT);
    let mut host = Host { supported };
    let mut modules = ModuleGraph::new();
    let mut scope = heap.scope();

    let options = scope.alloc_object().unwrap();
    let attributes = scope.alloc_object().unwrap();

    let value = scope.alloc_string("x").unwrap();
    let order = shuffled_indices(ATTR_COUNT);
    for i in order {
      let key_s = supported[i];
      let key = scope.alloc_string(key_s).unwrap();
      scope
        .define_property(
          attributes,
          PropertyKey::String(key),
          data_desc(Value::String(value)),
        )
        .unwrap();
    }

    let k_with = scope.alloc_string("with").unwrap();
    scope
      .define_property(
        options,
        PropertyKey::String(k_with),
        data_desc(Value::Object(attributes)),
      )
      .unwrap();

    let ctx = ExecutionContext {
      realm: realm.id(),
      script_or_module: None,
    };
    vm.push_execution_context(ctx).unwrap();

    // Fuel is chosen so:
    // - attribute extraction + supported-key validation can complete, and
    // - the canonicalization sort must run out-of-fuel *during sorting* (provided it ticks).
    let fuel = (ATTR_COUNT as u64)
      // `import_attributes_from_options_with_host_and_hooks` ticks once per key while collecting...
      .saturating_mul(2)
      // ...plus a couple of fixed `tick()` calls around `import()` setup.
      .saturating_add(2)
      .saturating_add(EXTRA_FUEL_BEFORE_SORT);

    vm.set_budget(Budget {
      fuel: Some(fuel),
      deadline: None,
      check_time_every: 1,
    });

    let specifier = scope.alloc_string("m").unwrap();
    let err = vm_js::start_dynamic_import(
      &mut vm,
      &mut scope,
      &mut modules,
      &mut host,
      realm.global_object(),
      Value::String(specifier),
      Value::Object(options),
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
