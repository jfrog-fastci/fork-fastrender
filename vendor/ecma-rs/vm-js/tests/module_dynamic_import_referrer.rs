use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, Job, JsString, MicrotaskQueue, ModuleGraph, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Realm, Scope, SourceText, SourceTextModuleRecord, Value,
  Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

/// Host hooks that:
/// - enqueue Promise jobs into an embedded `MicrotaskQueue`
/// - synchronously complete module loads by calling `FinishLoadingImportedModule`
/// - record the `referrer` passed when requesting `dep.js`
struct ReferrerRecordingHostHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<JsString, ModuleId>,
  dep_referrer: Option<ModuleReferrer>,
}

impl ReferrerRecordingHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
      dep_referrer: None,
    }
  }

  fn register_module(&mut self, specifier: &str, module: ModuleId) {
    self
      .modules
      .insert(JsString::from_str(specifier).unwrap(), module);
  }

  fn teardown_jobs(&mut self, vm: &mut Vm, heap: &mut Heap) {
    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        host: &mut dyn VmHostHooks,
        callee: Value,
        this: Value,
        args: &[Value],
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self.vm.call_with_host(&mut scope, host, callee, this, args)
      }

      fn construct(
        &mut self,
        host: &mut dyn VmHostHooks,
        callee: Value,
        args: &[Value],
        new_target: Value,
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self
          .vm
          .construct_with_host(&mut scope, host, callee, args, new_target)
      }

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut ctx = Ctx { vm, heap };
    self.microtasks.teardown(&mut ctx);
  }

  fn perform_microtask_checkpoint(&mut self, vm: &mut Vm, heap: &mut Heap) -> Result<(), VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    struct Ctx<'a> {
      vm: &'a mut Vm,
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        host: &mut dyn VmHostHooks,
        callee: Value,
        this: Value,
        args: &[Value],
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self.vm.call_with_host(&mut scope, host, callee, this, args)
      }

      fn construct(
        &mut self,
        host: &mut dyn VmHostHooks,
        callee: Value,
        args: &[Value],
        new_target: Value,
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self
          .vm
          .construct_with_host(&mut scope, host, callee, args, new_target)
      }

      fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: vm_js::RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut ctx = Ctx { vm, heap };

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
}

impl VmHostHooks for ReferrerRecordingHostHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
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
    if module_request.specifier == JsString::from_str("dep.js").unwrap() {
      self.dep_referrer = Some(referrer);
    }

    let module = *self
      .modules
      .get(&module_request.specifier)
      .ok_or_else(|| {
        VmError::InvariantViolation(
          "ReferrerRecordingHostHooks: no module registered for specifier",
        )
      })?;

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
fn dynamic_import_inside_imported_function_uses_callee_module_as_referrer() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let a = modules.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      "export function doImport() { return import('dep.js'); }",
    )?,
  )?;
  let b = modules.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      "import { doImport } from 'a.js'; export const p = doImport();",
    )?,
  )?;
  let dep = modules.add_module_with_specifier(
    "dep.js",
    SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?,
  )?;
  modules.link_all_by_specifier();

  let mut host_hooks = ReferrerRecordingHostHooks::new();
  host_hooks.register_module("dep.js", dep);

  let mut dummy_host = ();
  let result: Result<(), VmError> = (|| {
    let _eval_promise = modules.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut dummy_host,
      &mut host_hooks,
    )?;

    // Read `p` from the module namespace (should be the Promise returned by dynamic `import()`).
    let p_root = {
      let mut scope = heap.scope();
      let ns_b = modules.get_module_namespace(b, &mut vm, &mut scope)?;
      let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
      let p_value = scope.get_with_host_and_hooks(
        &mut vm,
        &mut dummy_host,
        &mut host_hooks,
        ns_b,
        p_key,
        Value::Object(ns_b),
      )?;

      let Value::Object(promise_obj) = p_value else {
        return Err(VmError::InvariantViolation(
          "module export p should be a promise object",
        ));
      };
      let root = scope.heap_mut().add_root(p_value)?;
      assert_eq!(
        scope.heap().promise_state(promise_obj)?,
        PromiseState::Pending
      );
      root
    };

    // Regression assertion:
    // `import('dep.js')` is evaluated *inside* a function whose `[[ScriptOrModule]]` is module A, so
    // `HostLoadImportedModule` must observe `referrer == Module(a)`, not the caller module B.
    assert_eq!(host_hooks.dep_referrer, Some(ModuleReferrer::Module(a)));

    // Drain microtasks so the dynamic import promise settles and so queued jobs release their roots.
    host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    // Optional: verify the import promise fulfills to a namespace where `x === 1`.
    let mut scope = heap.scope();
    let p_value = scope
      .heap()
      .get_root(p_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Value::Object(promise_obj) = p_value else {
      return Err(VmError::InvariantViolation(
        "promise root should reference an object",
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
        "dynamic import promise should fulfill to a namespace object",
      ));
    };

    let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
    let x_value = scope.get_with_host_and_hooks(
      &mut vm,
      &mut dummy_host,
      &mut host_hooks,
      ns_obj,
      x_key,
      Value::Object(ns_obj),
    )?;
    assert!(matches!(x_value, Value::Number(n) if n == 1.0));

    scope.heap_mut().remove_root(p_root);
    Ok(())
  })();

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  result
}

#[test]
fn dynamic_import_inside_imported_function_uses_callee_module_as_referrer_for_compiled_instantiation(
) -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();

  // Module A: drop its retained AST and ensure linking/instantiation can proceed using only
  // compiled HIR.
  let src_a = "export function doImport() { return import('dep.js'); }";
  let src_a = SourceText::new_charged_arc(&mut heap, "a.js", src_a)?;
  let mut rec_a = SourceTextModuleRecord::compile_source(&mut heap, src_a)?;
  rec_a.ast = None;
  let a = modules.add_module_with_specifier("a.js", rec_a)?;

  let b = modules.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      "import { doImport } from 'a.js'; export const p = doImport();",
    )?,
  )?;
  let dep = modules.add_module_with_specifier(
    "dep.js",
    SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?,
  )?;
  modules.link_all_by_specifier();

  let mut host_hooks = ReferrerRecordingHostHooks::new();
  host_hooks.register_module("dep.js", dep);

  let mut dummy_host = ();

  // Link first (compiled HIR instantiation runs here).
  modules.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;
  assert!(
    modules.module(a).ast.is_none(),
    "linking should not parse/retain an AST when compiled HIR is available"
  );

  let result: Result<(), VmError> = (|| {
    let _eval_promise = modules.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      b,
      &mut dummy_host,
      &mut host_hooks,
    )?;

    // Read `p` from the module namespace (should be the Promise returned by dynamic `import()`).
    let p_root = {
      let mut scope = heap.scope();
      let ns_b = modules.get_module_namespace(b, &mut vm, &mut scope)?;
      let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
      let p_value = scope.get_with_host_and_hooks(
        &mut vm,
        &mut dummy_host,
        &mut host_hooks,
        ns_b,
        p_key,
        Value::Object(ns_b),
      )?;

      let Value::Object(promise_obj) = p_value else {
        return Err(VmError::InvariantViolation(
          "module export p should be a promise object",
        ));
      };
      let root = scope.heap_mut().add_root(p_value)?;
      assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
      root
    };

    // Regression assertion:
    // `import('dep.js')` is evaluated *inside* a function whose `[[ScriptOrModule]]` is module A, so
    // `HostLoadImportedModule` must observe `referrer == Module(a)`, not the caller module B.
    assert_eq!(host_hooks.dep_referrer, Some(ModuleReferrer::Module(a)));

    // Drain microtasks so the dynamic import promise settles and so queued jobs release their roots.
    host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    // Optional: verify the import promise fulfills to a namespace where `x === 1`.
    let mut scope = heap.scope();
    let p_value = scope
      .heap()
      .get_root(p_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Value::Object(promise_obj) = p_value else {
      return Err(VmError::InvariantViolation(
        "promise root should reference an object",
      ));
    };
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
    let ns_value = scope
      .heap()
      .promise_result(promise_obj)?
      .expect("fulfilled promise should have a result");
    let Value::Object(ns_obj) = ns_value else {
      return Err(VmError::InvariantViolation(
        "dynamic import promise should fulfill to a namespace object",
      ));
    };

    let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
    let x_value = scope.get_with_host_and_hooks(
      &mut vm,
      &mut dummy_host,
      &mut host_hooks,
      ns_obj,
      x_key,
      Value::Object(ns_obj),
    )?;
    assert!(matches!(x_value, Value::Number(n) if n == 1.0));

    scope.heap_mut().remove_root(p_root);
    Ok(())
  })();

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  result
}
