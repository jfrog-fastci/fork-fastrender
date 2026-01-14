use std::collections::HashMap;

use vm_js::{
  GcObject, HeapLimits, HostDefined, JobCallback, JsRuntime, JsString, MicrotaskQueue, ModuleId,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, PropertyKind,
  SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmOptions,
};

#[derive(Debug)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

/// Minimal host hooks for dynamic `import()` tests.
///
/// This captures `HostLoadImportedModule` requests so tests can complete them manually via
/// `FinishLoadingImportedModule`.
struct TestHostHooks {
  microtasks: MicrotaskQueue,
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

  fn pending_specifier(&self, idx: usize) -> Option<JsString> {
    self.pending.get(idx).map(|p| p.request.specifier.clone())
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

  fn host_make_job_callback(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
    // Some jobs (including Promise reaction jobs) can outlive the current native stack. Keep the
    // callback alive using a persistent root owned by the job itself; the VM will tear it down when
    // the job is run or discarded.
    //
    // `JobCallback::ensure_rooted` is not called here; the job implementation is responsible for
    // rooting any captured callbacks/values via `Job::add_root`.
    JobCallback::try_new(callback)
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

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn generator_dynamic_import_yield_in_specifier() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Register a simple target module.
  let m_record = SourceTextModuleRecord::parse(&mut rt.heap, "export const x = 1;")?;
  let m = rt.modules_mut().add_module(m_record)?;

  let mut host = TestHostHooks::new();
  host.register_module("./m.js", m);

  // Step the generator until the `yield` in the import() specifier expression.
  let out1 = rt.exec_script_with_hooks(
    &mut host,
    r#"
      globalThis.iter = (function*() { return import(yield 'x'); })();
      var r1 = iter.next();
      r1.value === 'x' && r1.done === false;
    "#,
  )?;
  assert!(matches!(out1, Value::Bool(true)));

  // The dynamic import must *not* start until the generator resumes and provides the specifier.
  assert_eq!(host.pending_count(), 0);

  // Resume the generator with a real module specifier.
  let promise_value = rt.exec_script_with_hooks(
    &mut host,
    r#"
      var r2 = iter.next('./m.js');
      if (!r2.done) throw new Error('expected generator to finish');
      r2.value;
    "#,
  )?;
  let promise_root = rt.heap.add_root(promise_value)?;
  let Value::Object(promise_obj) = promise_value else {
    panic!("import() should evaluate to a Promise object");
  };

  assert_eq!(host.pending_count(), 1);
  assert_eq!(
    host.pending_specifier(0),
    Some(JsString::from_str("./m.js").unwrap())
  );
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Pending);

  // Complete the module load and drain microtasks so the import() promise settles.
  host.complete_load_for(&mut rt, "./m.js");
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
    .expect("fulfilled import() promise should have a result");
  let Value::Object(ns_obj) = ns_value else {
    panic!("dynamic import promise should fulfill to an object");
  };

  // Namespace should contain export `x`.
  {
    let mut scope = rt.heap.scope();
    let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
    let mut dummy_host = ();
    let desc_x = scope
      .object_get_own_property_with_host_and_hooks(
        &mut rt.vm,
        &mut dummy_host,
        &mut host,
        ns_obj,
        x_key,
      )?
      .expect("namespace should have an 'x' export");
    match desc_x.kind {
      PropertyKind::Data { value, .. } => assert_eq!(value, Value::Number(1.0)),
      _ => panic!("expected data property for namespace export 'x'"),
    }
  }

  rt.heap.remove_root(promise_root);
  host.teardown_jobs(&mut rt);
  Ok(())
}
