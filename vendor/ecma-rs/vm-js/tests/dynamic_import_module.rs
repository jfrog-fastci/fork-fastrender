use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, Job, JsString, MicrotaskQueue, ModuleGraph, ModuleId,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

#[derive(Debug)]
struct PendingLoad {
  referrer: ModuleReferrer,
  request: ModuleRequest,
  payload: ModuleLoadPayload,
}

/// Host hooks for module-evaluation dynamic import tests.
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

  fn complete_load_for(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    modules: &mut ModuleGraph,
    specifier: &str,
  ) -> Result<(), VmError> {
    let spec = JsString::from_str(specifier).unwrap();
    let idx = self
      .pending
      .iter()
      .position(|p| p.request.specifier == spec)
      .ok_or_else(|| VmError::InvariantViolation("no pending module load for specifier"))?;
    let pending = self.pending.remove(idx);

    let module = *self
      .modules
      .get(&spec)
      .ok_or_else(|| VmError::InvariantViolation("no module registered for specifier"))?;

    let mut scope = heap.scope();
    vm.finish_loading_imported_module(
      &mut scope,
      modules,
      self,
      pending.referrer,
      pending.request,
      pending.payload,
      Ok(module),
    )?;
    Ok(())
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
        self.heap.remove_root(id)
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
        self.heap.remove_root(id)
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

impl VmHostHooks for TestHostHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _modules: &mut ModuleGraph,
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

/// Host hook implementation that completes `HostLoadImportedModule` synchronously by immediately
/// calling `FinishLoadingImportedModule`.
///
/// This is a useful stress test for module-graph pointer restoration: dynamic import may start
/// evaluating the imported module (including top-level await) *before* the surrounding evaluation
/// returns to the caller.
struct SyncHostHooks {
  microtasks: MicrotaskQueue,
  modules: HashMap<JsString, ModuleId>,
}

impl SyncHostHooks {
  fn new() -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      modules: HashMap::new(),
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
        self.heap.remove_root(id)
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
        self.heap.remove_root(id)
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

impl VmHostHooks for SyncHostHooks {
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
    let module = *self
      .modules
      .get(&module_request.specifier)
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
fn dynamic_import_works_inside_module_evaluation_without_attached_graph() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let dep =
    modules.add_module(SourceTextModuleRecord::parse(&mut heap, "export const y = 1;")?)?;
  let m = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?)?;
  let consumer =
    modules.add_module(SourceTextModuleRecord::parse(&mut heap, "export const p = import('./m.js');")?)?;

  let mut host_hooks = TestHostHooks::new();
  host_hooks.register_module("./m.js", m);
  host_hooks.register_module("./dep.js", dep);

  // Evaluate the consumer module. This should execute the `import('./m.js')` expression and return a
  // pending Promise (stored in the exported binding `p`).
  //
  // Importantly: we intentionally do NOT call `vm.set_module_graph(&mut modules)` here; the module
  // evaluator should attach the graph automatically so dynamic import works from module code.
  let mut dummy_host = ();
  let _eval_promise = modules.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut dummy_host,
    &mut host_hooks,
  )?;

  assert_eq!(host_hooks.pending.len(), 1);

  // Read `p` from the consumer module namespace.
  let mut scope = heap.scope();
  let ns_consumer = modules.get_module_namespace(consumer, &mut vm, &mut scope)?;
  let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
  let p_value = scope.get_with_host_and_hooks(
    &mut vm,
    &mut dummy_host,
    &mut host_hooks,
    ns_consumer,
    p_key,
    Value::Object(ns_consumer),
  )?;

  let Value::Object(promise_obj) = p_value else {
    return Err(VmError::InvariantViolation(
      "module export p should be a promise object",
    ));
  };
  let promise_root = scope.heap_mut().add_root(p_value)?;
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
  drop(scope);

  // Complete the dynamic import module loads.
  host_hooks.complete_load_for(&mut vm, &mut heap, &mut modules, "./m.js")?;
  host_hooks.complete_load_for(&mut vm, &mut heap, &mut modules, "./dep.js")?;
  // `ContinueDynamicImport` settles the import() promise via a Promise reaction job, so drain host
  // microtasks before observing the final promise state.
  host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  // The promise stored in `p` should now be fulfilled to the imported module namespace.
  let mut scope = heap.scope();
  let p_value = scope
    .heap()
    .get_root(promise_root)
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

  // Verify the namespace exports are readable and reflect evaluated module state.
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);
  let x_value = scope.get_with_host_and_hooks(
    &mut vm,
    &mut dummy_host,
    &mut host_hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  let y_value = scope.get_with_host_and_hooks(
    &mut vm,
    &mut dummy_host,
    &mut host_hooks,
    ns_obj,
    y_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  scope.heap_mut().remove_root(promise_root);
  drop(scope);
  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn dynamic_import_tla_module_works_with_sync_host_completion_without_attached_graph() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let tla = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "await Promise.resolve(); export const x = 1;",
  )?)?;
  let consumer =
    modules.add_module(SourceTextModuleRecord::parse(&mut heap, "export const p = import('./tla.js');")?)?;

  // Evaluate without pre-attaching the module graph pointer to the VM.
  assert!(vm.module_graph_ptr().is_none());

  let mut host_hooks = SyncHostHooks::new();
  host_hooks.register_module("./tla.js", tla);

  let mut dummy_host = ();
  let _eval_promise = modules.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut dummy_host,
    &mut host_hooks,
  )?;

  // The consumer module itself evaluated synchronously, but `import('./tla.js')` already started
  // evaluating the imported module. Since that evaluation is suspended on top-level await, the VM
  // must keep the module graph pointer installed so the resume microtask can run even though the
  // embedding never permanently attached a module graph.
  assert!(vm.module_graph_ptr().is_some());

  // Read `p` from the consumer module namespace.
  let p_root = {
    let mut scope = heap.scope();
    let ns_consumer = modules.get_module_namespace(consumer, &mut vm, &mut scope)?;
    let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
    let p_value = scope.get_with_host_and_hooks(
      &mut vm,
      &mut dummy_host,
      &mut host_hooks,
      ns_consumer,
      p_key,
      Value::Object(ns_consumer),
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

  host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  // Once the dynamic import promise settles, the temporary graph attachment should be cleaned up.
  assert!(vm.module_graph_ptr().is_none());

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
  drop(scope);

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn dynamic_import_works_with_evaluate_sync_and_sync_host_completion_without_attached_graph(
) -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let dep =
    modules.add_module(SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?)?;
  let consumer = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "export const p = import('./dep.js');",
  )?)?;

  // Evaluate without pre-attaching the module graph pointer to the VM.
  assert!(vm.module_graph_ptr().is_none());

  let mut host_hooks = SyncHostHooks::new();
  host_hooks.register_module("./dep.js", dep);

  let mut dummy_host = ();

  modules.evaluate_sync(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut dummy_host,
    &mut host_hooks,
  )?;

  // The consumer module evaluated synchronously, but `import('./dep.js')` registered a pending
  // dynamic import evaluation continuation. Since `ContinueDynamicImport` settles the import()
  // promise via a Promise reaction job, the VM must keep the module graph pointer installed until
  // that job runs even though the embedding never permanently attached a module graph.
  assert!(vm.module_graph_ptr().is_some());

  // Read `p` from the consumer module namespace.
  let p_root = {
    let mut scope = heap.scope();
    let ns_consumer = modules.get_module_namespace(consumer, &mut vm, &mut scope)?;
    let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
    let p_value = scope.get_with_host_and_hooks(
      &mut vm,
      &mut dummy_host,
      &mut host_hooks,
      ns_consumer,
      p_key,
      Value::Object(ns_consumer),
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

  host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  // Once the dynamic import promise settles, the temporary graph attachment should be cleaned up.
  assert!(vm.module_graph_ptr().is_none());

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
  drop(scope);

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn dynamic_import_uses_callback_module_as_referrer_in_promise_job() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let dep = modules.add_module(SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?)?;
  let m = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "export const p = Promise.resolve().then(() => import('./dep.js'));",
  )?)?;

  let mut host_hooks = TestHostHooks::new();
  host_hooks.register_module("./dep.js", dep);

  let mut dummy_host = ();

  let result: Result<(), VmError> = (|| {
    let _eval_promise = modules.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      m,
      &mut dummy_host,
      &mut host_hooks,
    )?;

    // `import()` inside Promise jobs requires the VM's module graph pointer to be available after the
    // module has finished evaluating. In `JsRuntime` this is installed for the runtime-owned module
    // graph; in this low-level test we install it explicitly.
    vm.set_module_graph(&mut modules);

    // Read `p` from the module namespace.
    let promise_root = {
      let mut scope = heap.scope();
      let ns_m = modules.get_module_namespace(m, &mut vm, &mut scope)?;
      let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
      let p_value = scope.ordinary_get_with_host_and_hooks(
        &mut vm,
        &mut dummy_host,
        &mut host_hooks,
        ns_m,
        p_key,
        Value::Object(ns_m),
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

    assert_eq!(host_hooks.pending.len(), 0);
    assert!(vm.current_realm().is_none());

    // Drain microtasks so the Promise reaction job runs. This should execute the dynamic `import()`
    // call inside the callback, resulting in a host module load request whose `referrer` reflects the
    // callback's defining module (`m`).
    host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    assert_eq!(host_hooks.pending.len(), 1);
    assert_eq!(
      host_hooks.pending[0].request.specifier,
      JsString::from_str("./dep.js").unwrap()
    );
    assert_eq!(host_hooks.pending[0].referrer, ModuleReferrer::Module(m));

    // Complete the dynamic import module load.
    host_hooks.complete_load_for(&mut vm, &mut heap, &mut modules, "./dep.js")?;
    host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

    // The promise stored in `p` should now be fulfilled to the imported module namespace.
    let mut scope = heap.scope();
    let p_value = scope
      .heap()
      .get_root(promise_root)
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

    // Verify the namespace exports are readable and reflect evaluated module state.
    let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
    let x_value = scope.ordinary_get_with_host_and_hooks(
      &mut vm,
      &mut dummy_host,
      &mut host_hooks,
      ns_obj,
      x_key,
      Value::Object(ns_obj),
    )?;
    assert!(matches!(x_value, Value::Number(n) if n == 1.0));

    scope.heap_mut().remove_root(promise_root);
    Ok(())
  })();

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  result
}

#[test]
fn dynamic_import_works_after_tla_resumption_without_attached_graph() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let dep =
    modules.add_module(SourceTextModuleRecord::parse(&mut heap, "export const y = 1;")?)?;
  let m = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "export { y } from './dep.js'; export const x = 1;",
  )?)?;
  let consumer = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "await 0; export const p = import('./m.js');",
  )?)?;

  let mut host_hooks = TestHostHooks::new();
  host_hooks.register_module("./m.js", m);
  host_hooks.register_module("./dep.js", dep);

  let mut dummy_host = ();

  // Evaluate without pre-attaching the module graph pointer to the VM. The graph pointer should be
  // installed by `ModuleGraph::evaluate` and kept installed until the evaluation promise settles.
  let eval_promise = modules.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut dummy_host,
    &mut host_hooks,
  )?;

  let (eval_promise_obj, eval_promise_root) = {
    let mut scope = heap.scope();
    let Value::Object(obj) = eval_promise else {
      return Err(VmError::InvariantViolation(
        "module evaluation did not return a promise object",
      ));
    };
    scope.push_root(eval_promise)?;
    let root = scope.heap_mut().add_root(eval_promise)?;
    assert_eq!(scope.heap().promise_state(obj)?, PromiseState::Pending);
    (obj, root)
  };

  // The module graph pointer must remain installed while evaluation is suspended at top-level
  // await. If it were restored immediately after `evaluate` returned, the resumption job would run
  // with `vm.module_graph_ptr == None` and dynamic `import()` would fail.
  assert!(vm.module_graph_ptr().is_some());

  // Drain microtasks so `await 0;` resumes and executes `import('./m.js')`.
  host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  assert_eq!(host_hooks.pending.len(), 1);
  assert_eq!(
    host_hooks.pending[0].request.specifier,
    JsString::from_str("./m.js").unwrap()
  );

  // After the module evaluation promise settles, the VM's module graph pointer should be restored
  // back to its previous value (None in this test).
  {
    let scope = heap.scope();
    assert_eq!(scope.heap().promise_state(eval_promise_obj)?, PromiseState::Fulfilled);
  }
  assert!(vm.module_graph_ptr().is_none());

  // Read `p` from the consumer module namespace. This is the Promise returned by dynamic `import()`.
  let p_root = {
    let mut scope = heap.scope();
    let ns_consumer = modules.get_module_namespace(consumer, &mut vm, &mut scope)?;
    let p_key = PropertyKey::from_string(scope.alloc_string("p")?);
    let p_value = scope.get_with_host_and_hooks(
      &mut vm,
      &mut dummy_host,
      &mut host_hooks,
      ns_consumer,
      p_key,
      Value::Object(ns_consumer),
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

  // Complete the dynamic import module loads.
  host_hooks.complete_load_for(&mut vm, &mut heap, &mut modules, "./m.js")?;
  host_hooks.complete_load_for(&mut vm, &mut heap, &mut modules, "./dep.js")?;

  // Drain microtasks again in case module loading enqueued any promise jobs.
  host_hooks.perform_microtask_checkpoint(&mut vm, &mut heap)?;

  // The promise stored in `p` should now be fulfilled to the imported module namespace.
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

  // Verify the namespace exports are readable and reflect evaluated module state.
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);
  let x_value = scope.get_with_host_and_hooks(
    &mut vm,
    &mut dummy_host,
    &mut host_hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  let y_value = scope.get_with_host_and_hooks(
    &mut vm,
    &mut dummy_host,
    &mut host_hooks,
    ns_obj,
    y_key,
    Value::Object(ns_obj),
  )?;
  assert!(matches!(x_value, Value::Number(n) if n == 1.0));
  assert!(matches!(y_value, Value::Number(n) if n == 1.0));

  // The module evaluation promise should still be fulfilled and the module graph pointer should
  // remain restored.
  let eval_promise_value = scope
    .heap()
    .get_root(eval_promise_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let Value::Object(eval_promise_obj) = eval_promise_value else {
    return Err(VmError::InvariantViolation(
      "evaluation promise root should reference an object",
    ));
  };
  assert_eq!(scope.heap().promise_state(eval_promise_obj)?, PromiseState::Fulfilled);
  drop(scope);
  assert!(vm.module_graph_ptr().is_none());

  let mut scope = heap.scope();
  scope.heap_mut().remove_root(p_root);
  scope.heap_mut().remove_root(eval_promise_root);
  drop(scope);

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn abort_tla_evaluation_restores_module_graph_ptr() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let m = modules.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    "await new Promise(() => {}); export const x = 1;",
  )?)?;

  let mut host_hooks = TestHostHooks::new();
  let mut dummy_host = ();
  let _eval_promise = modules.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    m,
    &mut dummy_host,
    &mut host_hooks,
  )?;

  assert!(vm.module_graph_ptr().is_some());

  modules.abort_tla_evaluation(&mut vm, &mut heap, m);

  assert!(vm.module_graph_ptr().is_none());

  modules.teardown(&mut vm, &mut heap);
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
