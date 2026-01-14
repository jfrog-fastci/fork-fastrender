use vm_js::{
  Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, Value, Vm, VmError, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

/// Minimal host hook implementation that completes `HostLoadImportedModule` immediately with an
/// error *as a throw completion* so dynamic `import()` returns a rejected promise instead of
/// throwing synchronously.
struct RejectingImportHooks {
  microtasks: MicrotaskQueue,
}

impl RejectingImportHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
    }
  }

  fn teardown_jobs(&mut self, rt: &mut JsRuntime) {
    self.microtasks.teardown(rt);
  }
}

impl VmHostHooks for RejectingImportHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
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
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      // Reject the import() promise. The rejection reason is irrelevant for this test.
      Err(VmError::Throw(Value::Undefined)),
    )?;
    Ok(())
  }
}

#[test]
fn generator_dynamic_import_specifier_can_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut hooks = RejectingImportHooks::new();
  let value = rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      function* g() {
        return import(yield 0);
      }

      var it = g();
      var first = it.next();
      var second = it.next("./m.js");

      first.value === 0 &&
        first.done === false &&
        second.done === true &&
        second.value &&
        typeof second.value.then === "function"
    "#,
  )?;
  hooks.teardown_jobs(&mut rt);
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
