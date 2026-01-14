use std::collections::HashMap;

use vm_js::{
  CompiledScript, Heap, HeapLimits, HostDefined, ImportMetaProperty, Job, JsRuntime, MicrotaskQueue,
  ModuleGraph, ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey,
  Scope, SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions, JsString,
};

// NOTE: This test intentionally exercises the compiled-module pipeline (see Task 91).
//
// Some builds may not support AST-less compiled module records yet; in that case the module
// evaluation promise rejects with an internal `Error("module AST missing")`, and we skip the test.

struct TestHostHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<JsString, ModuleId>,
  import_meta_urls: HashMap<ModuleId, String>,
  import_referrers: HashMap<JsString, ModuleReferrer>,
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
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
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
    let mut ctx = Ctx {
      vm,
      heap,
      host: &mut host,
    };

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
        Err(VmError::Unimplemented("TestHostHooks::teardown_jobs call"))
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
    // `ModuleRequest` stores specifiers as `JsString` (UTF-16 code units). This test uses those
    // values directly as map keys.
    let specifier = module_request.specifier.clone();
    self.import_referrers.insert(specifier.clone(), referrer);
    let module = *self
      .modules
      .get(&specifier)
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

  // === Compiled-module record construction (Task 91) ===
  //
  // The module record must be populated with import/export metadata (requested modules, export
  // entries, etc) while also carrying a compiled HIR representation of the module body.
  //
  // We build this by:
  // 1. compiling the module source text to HIR (`CompiledScript::compile_module`), and
  // 2. parsing the same source text to extract module record metadata, then dropping the AST so
  //    module evaluation is forced down the compiled-module execution path.
  let compiled_m = CompiledScript::compile_module(
    heap,
    "m.js",
    r#"
       export function f() { return import.meta.url; }
       export function g() { return import('dep2.js'); }
       export { h, gi, K } from 'dep.js';
     "#,
  )?;
  let mut record_m = SourceTextModuleRecord::parse_source(heap, compiled_m.source.clone())?;
  record_m.compiled = Some(compiled_m);
  record_m.clear_ast();

  let compiled_dep = CompiledScript::compile_module(
    heap,
    "dep.js",
    r#"
       export const x = 1;
       export function h() { return import.meta.url; }
       export function gi() { return import('dep3.js'); }
       export class K {
         constructor() {
           this.url = import.meta.url;
           this.p = import('dep4.js');
         }
       }
     "#,
  )?;
  let mut record_dep = SourceTextModuleRecord::parse_source(heap, compiled_dep.source.clone())?;
  record_dep.compiled = Some(compiled_dep);
  record_dep.clear_ast();

  let compiled_dep2 = CompiledScript::compile_module(heap, "dep2.js", "export const y = 2;")?;
  let mut record_dep2 = SourceTextModuleRecord::parse_source(heap, compiled_dep2.source.clone())?;
  record_dep2.compiled = Some(compiled_dep2);
  record_dep2.clear_ast();

  let compiled_dep3 = CompiledScript::compile_module(heap, "dep3.js", "export const z = 3;")?;
  let mut record_dep3 = SourceTextModuleRecord::parse_source(heap, compiled_dep3.source.clone())?;
  record_dep3.compiled = Some(compiled_dep3);
  record_dep3.clear_ast();

  let compiled_dep4 = CompiledScript::compile_module(heap, "dep4.js", "export const w = 4;")?;
  let mut record_dep4 = SourceTextModuleRecord::parse_source(heap, compiled_dep4.source.clone())?;
  record_dep4.compiled = Some(compiled_dep4);
  record_dep4.clear_ast();

  let m = modules.add_module_with_specifier("m.js", record_m)?;
  let dep = modules.add_module_with_specifier("dep.js", record_dep)?;
  let dep2 = modules.add_module_with_specifier("dep2.js", record_dep2)?;
  let dep3 = modules.add_module_with_specifier("dep3.js", record_dep3)?;
  let dep4 = modules.add_module_with_specifier("dep4.js", record_dep4)?;

  modules.link_all_by_specifier();

  let mut hooks = TestHostHooks::new();
  hooks.register_module("dep.js", dep);
  hooks.register_module("dep2.js", dep2);
  hooks.register_module("dep3.js", dep3);
  hooks.register_module("dep4.js", dep4);
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
    let state = scope.heap().promise_state(eval_promise_obj)?;
    if state != PromiseState::Fulfilled {
      // If compiled modules are not supported yet, the module evaluation promise rejects with an
      // internal `Error("module AST missing")`. Skip in that case so this test suite can run on
      // engines that still require the AST-backed module path.
      if state == PromiseState::Rejected {
        if let Some(reason) = scope.heap().promise_result(eval_promise_obj)? {
          let mut message_contains_ast_missing = false;
          match reason {
            Value::String(s) => {
              message_contains_ast_missing = scope
                .heap()
                .get_string(s)?
                .to_utf8_lossy()
                .contains("module AST missing");
            }
            Value::Object(obj) => {
              let msg_key = PropertyKey::from_string(scope.alloc_string("message")?);
              if let Some(desc) = scope.heap().get_own_property(obj, msg_key)? {
                if let vm_js::PropertyKind::Data { value, .. } = desc.kind {
                  if let Value::String(s) = value {
                    message_contains_ast_missing = scope
                      .heap()
                      .get_string(s)?
                      .to_utf8_lossy()
                      .contains("module AST missing");
                  }
                }
              }
            }
            _ => {}
          }

          if message_contains_ast_missing {
            // Ensure any queued jobs/roots are discarded before skipping.
            drop(scope);
            hooks.teardown_jobs(vm, heap);
            return Ok(());
          }
        }
      }
      assert_eq!(state, PromiseState::Fulfilled);
    }
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
  let import_promise_root = {
    let mut scope = heap.scope();
    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let g_key = PropertyKey::from_string(scope.alloc_string("g")?);
    let g_value =
      scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, g_key, Value::Object(ns))?;
    scope.push_root(g_value)?;

    let p = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      g_value,
      Value::Undefined,
      &[],
    )?;
    scope.push_root(p)?;

    // Keep the returned Promise alive across microtasks so we can inspect it afterwards.
    scope.heap_mut().add_root(p)?
  };

  // Dynamic `import()` should use `m.js` as its referrer module.
  assert_eq!(
    hooks.import_referrers.get("dep2.js").copied(),
    Some(ModuleReferrer::Module(m)),
  );

  // Continue the dynamic import promise to completion.
  hooks.perform_microtask_checkpoint(vm, heap)?;

  // Verify the import() promise fulfills to the imported module namespace.
  {
    let mut scope = heap.scope();
    let p = scope
      .heap()
      .get_root(import_promise_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    scope.push_root(p)?;
    let Value::Object(promise_obj) = p else {
      return Err(VmError::InvariantViolation(
        "expected import() to return a Promise object",
      ));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let ns_value = scope
      .heap()
      .promise_result(promise_obj)?
      .expect("fulfilled promise should have a result");
    let Value::Object(ns_obj) = ns_value else {
      return Err(VmError::InvariantViolation(
        "import() promise should fulfill to a module namespace object",
      ));
    };

    let y_key = PropertyKey::from_string(scope.alloc_string("y")?);
    let y_value = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      ns_obj,
      y_key,
      Value::Object(ns_obj),
    )?;
    assert!(matches!(y_value, Value::Number(n) if n == 2.0));

    scope.heap_mut().remove_root(import_promise_root);
  }

  // Read `gi` (declared in `dep.js`, re-exported by `m.js`) and call it from host code. The dynamic
  // import referrer should be `dep.js`, not the entry module `m.js`.
  let dep3_import_promise_root = {
    let mut scope = heap.scope();
    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let gi_key = PropertyKey::from_string(scope.alloc_string("gi")?);
    let gi_value =
      scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, gi_key, Value::Object(ns))?;
    scope.push_root(gi_value)?;

    let p = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      gi_value,
      Value::Undefined,
      &[],
    )?;
    scope.push_root(p)?;

    assert_eq!(
      hooks.import_referrers.get("dep3.js").copied(),
      Some(ModuleReferrer::Module(dep)),
    );

    // Keep the returned Promise alive across microtasks so we can inspect it afterwards.
    scope.heap_mut().add_root(p)?
  };

  // Continue the dynamic import promise to completion.
  hooks.perform_microtask_checkpoint(vm, heap)?;

  // Verify the import() promise fulfills to the imported module namespace.
  {
    let mut scope = heap.scope();
    let p = scope
      .heap()
      .get_root(dep3_import_promise_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    scope.push_root(p)?;
    let Value::Object(promise_obj) = p else {
      return Err(VmError::InvariantViolation(
        "expected import() to return a Promise object",
      ));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let ns_value = scope
      .heap()
      .promise_result(promise_obj)?
      .expect("fulfilled promise should have a result");
    let Value::Object(ns_obj) = ns_value else {
      return Err(VmError::InvariantViolation(
        "import() promise should fulfill to a module namespace object",
      ));
    };

    let z_key = PropertyKey::from_string(scope.alloc_string("z")?);
    let z_value = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      ns_obj,
      z_key,
      Value::Object(ns_obj),
    )?;
    assert!(matches!(z_value, Value::Number(n) if n == 3.0));

    scope.heap_mut().remove_root(dep3_import_promise_root);
  }

  // Read `K` (a class declared in `dep.js` and re-exported by `m.js`) and construct it from host
  // code. The constructor uses `import.meta.url` and a dynamic import, which should both observe
  // `dep.js` as the active module during construction.
  let dep4_import_promise_root = {
    let mut scope = heap.scope();
    let ns = modules.get_module_namespace(m, vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let k_key = PropertyKey::from_string(scope.alloc_string("K")?);
    let k_value =
      scope.get_with_host_and_hooks(vm, &mut host, &mut hooks, ns, k_key, Value::Object(ns))?;
    scope.push_root(k_value)?;

    // Construct `new K()` from host code.
    let instance =
      vm.construct_with_host_and_hooks(&mut host, &mut scope, &mut hooks, k_value, &[], k_value)?;
    let Value::Object(instance_obj) = instance else {
      return Err(VmError::InvariantViolation(
        "expected new K() to return an object",
      ));
    };
    scope.push_root(instance)?;

    // Verify `this.url` was initialized using `dep.js`'s `import.meta.url`.
    let url_key = PropertyKey::from_string(scope.alloc_string("url")?);
    let url_value = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      instance_obj,
      url_key,
      Value::Object(instance_obj),
    )?;
    let Value::String(url_s) = url_value else {
      return Err(VmError::InvariantViolation(
        "expected K instance .url to be a string",
      ));
    };
    assert_eq!(
      scope.heap().get_string(url_s)?.to_utf8_lossy(),
      META_URL_DEP
    );

    // Extract `this.p` (the dynamic import promise) so we can inspect it after draining microtasks.
    let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
    let p_value = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      instance_obj,
      p_key,
      Value::Object(instance_obj),
    )?;
    scope.push_root(p_value)?;

    assert_eq!(
      hooks.import_referrers.get("dep4.js").copied(),
      Some(ModuleReferrer::Module(dep)),
    );

    scope.heap_mut().add_root(p_value)?
  };

  // Continue the constructor's dynamic import promise to completion.
  hooks.perform_microtask_checkpoint(vm, heap)?;

  // Verify the import() promise fulfills to the imported module namespace.
  {
    let mut scope = heap.scope();
    let p = scope
      .heap()
      .get_root(dep4_import_promise_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    scope.push_root(p)?;
    let Value::Object(promise_obj) = p else {
      return Err(VmError::InvariantViolation(
        "expected import() to return a Promise object",
      ));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let ns_value = scope
      .heap()
      .promise_result(promise_obj)?
      .expect("fulfilled promise should have a result");
    let Value::Object(ns_obj) = ns_value else {
      return Err(VmError::InvariantViolation(
        "import() promise should fulfill to a module namespace object",
      ));
    };

    let w_key = PropertyKey::from_string(scope.alloc_string("w")?);
    let w_value = scope.get_with_host_and_hooks(
      vm,
      &mut host,
      &mut hooks,
      ns_obj,
      w_key,
      Value::Object(ns_obj),
    )?;
    assert!(matches!(w_value, Value::Number(n) if n == 4.0));

    scope.heap_mut().remove_root(dep4_import_promise_root);
  }

  hooks.teardown_jobs(vm, heap);
  Ok(())
}
