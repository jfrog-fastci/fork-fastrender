use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, ImportMetaProperty, Job, JsRuntime, MicrotaskQueue, ModuleGraph,
  ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Scope,
  Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

// NOTE: This test intentionally exercises the compiled-module pipeline (see Task 91).
// It should fail on engines that only support AST-backed module records.

struct TestHostHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<String, ModuleId>,
  import_meta_urls: HashMap<ModuleId, String>,
  import_referrers: HashMap<String, ModuleReferrer>,
}

impl TestHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
      import_meta_urls: HashMap::new(),
      import_referrers: HashMap::new(),
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self.modules.insert(specifier.to_string(), module);
  }

  fn register_import_meta_url(&mut self, module: ModuleId, url: &str) {
    self.import_meta_urls.insert(module, url.to_string());
  }

  fn perform_microtask_checkpoint(&mut self, vm: &mut Vm, heap: &mut Heap) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
      host: &'a mut (),
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        hooks: &mut dyn VmHostHooks,
        callee: Value,
        this: Value,
        args: &[Value],
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self
          .vm
          .call_with_host_and_hooks(&mut *self.host, &mut scope, hooks, callee, this, args)
      }

      fn construct(
        &mut self,
        hooks: &mut dyn VmHostHooks,
        callee: Value,
        args: &[Value],
        new_target: Value,
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self.vm.construct_with_host_and_hooks(
          &mut *self.host,
          &mut scope,
          hooks,
          callee,
          args,
          new_target,
        )
      }

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id)
      }
    }

    let mut host = ();
    let mut ctx = Ctx { vm, heap, host: &mut host };

    let mut first_err: Option<VmError> = None;
    let mut termination_err: Option<VmError> = None;
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      let job_result = job.run(&mut ctx, self);
      match job_result {
        Ok(()) => {}
        Err(e @ VmError::Termination(_)) => {
          termination_err = Some(e);
          break;
        }
        Err(e) => {
          if first_err.is_none() {
            first_err = Some(e);
          }
        }
      }
    }

    if termination_err.is_some() {
      self.microtasks.teardown(&mut ctx);
    }

    self.microtasks.end_checkpoint();
    match termination_err {
      Some(e) => Err(e),
      None => first_err.map_or(Ok(()), Err),
    }
  }

  fn teardown_jobs(&mut self, vm: &mut Vm, heap: &mut Heap) {
    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented(
          "TestHostHooks::teardown_jobs call",
        ))
      }

      fn construct(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented(
          "TestHostHooks::teardown_jobs construct",
        ))
      }

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id)
      }
    }

    let mut ctx = Ctx { vm, heap };
    self.microtasks.teardown(&mut ctx);
  }
}

impl VmHostHooks for TestHostHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    module: ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    let meta_url = self
      .import_meta_urls
      .get(&module)
      .ok_or_else(|| VmError::InvariantViolation("no import.meta.url registered for module"))?;
    let key_s = scope.alloc_string("url")?;
    let val_s = scope.alloc_string(meta_url)?;
    Ok(vec![ImportMetaProperty {
      key: PropertyKey::from_string(key_s),
      value: Value::String(val_s),
    }])
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
    self
      .import_referrers
      .insert(module_request.specifier.clone(), referrer);
    let module = *self
      .modules
      .get(module_request.specifier.as_str())
      .ok_or_else(|| VmError::InvariantViolation("no module registered for specifier"))?;
    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(module),
    )
  }
}

#[test]
fn compiled_module_decl_functions_capture_realm_and_module_for_host_calls() -> Result<(), VmError> {
  const META_URL_M: &str = "https://example.invalid/m.js";
  const META_URL_DEP: &str = "https://example.invalid/dep.js";

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let realm_id = rt.realm().id();
  let global_object = rt.realm().global_object();

  let (vm, modules, heap) = rt.vm_modules_and_heap_mut();

  // These modules must be built using the compiled-module record API (Task 91).
  let m = modules.add_module_with_specifier(
    "m.js",
    vm_js::CompiledModuleRecord::compile(
      heap,
      "m.js",
      r#"
        export function f() { return import.meta.url; }
        export function g() { return import('dep.js'); }
        export { h } from 'dep.js';
      "#,
    )?,
  )?;
  let dep = modules.add_module_with_specifier(
    "dep.js",
    vm_js::CompiledModuleRecord::compile(
      heap,
      "dep.js",
      r#"
        export function h() { return import.meta.url; }
        export const x = 1;
      "#,
    )?,
  )?;

  modules.link_all_by_specifier();

  let mut hooks = TestHostHooks::new();
  hooks.register_module("dep.js", dep);
  hooks.register_import_meta_url(m, META_URL_M);
  hooks.register_import_meta_url(dep, META_URL_DEP);

  let mut host = ();

  // Evaluate `m.js`. This should fulfill synchronously (no top-level await), but `Evaluate` is
  // spec-visible as a Promise, so we drain microtasks before asserting it is fulfilled.
  let eval_promise =
    modules.evaluate(vm, heap, global_object, realm_id, m, &mut host, &mut hooks)?;
  let Value::Object(eval_promise_obj) = eval_promise else {
    return Err(VmError::InvariantViolation(
      "ModuleGraph::evaluate should return a Promise object",
    ));
  };
  hooks.perform_microtask_checkpoint(vm, heap)?;
  {
    let mut scope = heap.scope();
    scope.push_root(eval_promise)?;
    assert_eq!(
      scope.heap().promise_state(eval_promise_obj)?,
      PromiseState::Fulfilled
    );
  }

  // Ensure we are calling from host code with no active execution context so we exercise the VM's
  // call-time realm/module restoration from the function's captured metadata.
  assert!(vm.current_realm().is_none());
  assert!(vm.get_active_script_or_module().is_none());

  // Read `f` and call it from host code; it should still be able to resolve `import.meta`.
  {
    let mut scope = heap.scope();
    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let f_key = PropertyKey::from_string(scope.alloc_string("f")?);
    let f_value =
      scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, f_key, Value::Object(ns))?;
    scope.push_root(f_value)?;

    let f_result = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      f_value,
      Value::Undefined,
      &[],
    )?;
    let Value::String(url_s) = f_result else {
      return Err(VmError::InvariantViolation(
        "expected f() to return a string (import.meta.url)",
      ));
    };
    assert_eq!(scope.heap().get_string(url_s)?.to_utf8_lossy(), META_URL_M);
  }

  // Read `h` (a function declared in `dep.js` and re-exported by `m.js`) and call it from host
  // code. It should still be able to resolve `import.meta`, and `import.meta.url` should be
  // specific to `dep.js` (not `m.js`).
  {
    let mut scope = heap.scope();
    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let h_key = PropertyKey::from_string(scope.alloc_string("h")?);
    let h_value =
      scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, h_key, Value::Object(ns))?;
    scope.push_root(h_value)?;

    let h_result = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      h_value,
      Value::Undefined,
      &[],
    )?;
    let Value::String(url_s) = h_result else {
      return Err(VmError::InvariantViolation(
        "expected h() to return a string (import.meta.url)",
      ));
    };
    assert_eq!(
      scope.heap().get_string(url_s)?.to_utf8_lossy(),
      META_URL_DEP
    );
  }

  // Read `g` and call it from host code; it should be able to start a dynamic import even with no
  // active execution context.
  {
    let mut scope = heap.scope();
    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let g_key = PropertyKey::from_string(scope.alloc_string("g")?);
    let g_value =
      scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, g_key, Value::Object(ns))?;
    scope.push_root(g_value)?;

    let _p = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      g_value,
      Value::Undefined,
      &[],
    )?;
  }

  hooks.perform_microtask_checkpoint(vm, heap)?;
  assert_eq!(
    hooks.import_referrers.get("dep.js").copied(),
    Some(ModuleReferrer::Module(m))
  );

  hooks.teardown_jobs(vm, heap);
  Ok(())
}

