use std::collections::HashMap;

use vm_js::{
  Heap, HeapLimits, HostDefined, Job, MicrotaskQueue, ModuleGraph, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, PromiseState, PropertyKey, Realm, Scope, SourceTextModuleRecord,
  Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
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
  modules: HashMap<String, ModuleId>,
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
    self.modules.insert(specifier.to_string(), module);
  }

  fn complete_load_for(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    modules: &mut ModuleGraph,
    specifier: &str,
  ) -> Result<(), VmError> {
    let idx = self
      .pending
      .iter()
      .position(|p| p.request.specifier == specifier)
      .ok_or_else(|| VmError::InvariantViolation("no pending module load for specifier"))?;
    let pending = self.pending.remove(idx);

    let module = *self
      .modules
      .get(specifier)
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

#[test]
fn dynamic_import_works_inside_module_evaluation_without_attached_graph() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut modules = ModuleGraph::new();
  let dep = modules.add_module(SourceTextModuleRecord::parse("export const y = 1;")?);
  let m = modules.add_module(SourceTextModuleRecord::parse(
    "export { y } from './dep.js'; export const x = 1;",
  )?);
  let consumer =
    modules.add_module(SourceTextModuleRecord::parse("export const p = import('./m.js');")?);

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
  let p_value = scope.ordinary_get_with_host_and_hooks(
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
  let x_value = scope.ordinary_get_with_host_and_hooks(
    &mut vm,
    &mut dummy_host,
    &mut host_hooks,
    ns_obj,
    x_key,
    Value::Object(ns_obj),
  )?;
  let y_value = scope.ordinary_get_with_host_and_hooks(
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
  host_hooks.teardown_jobs(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
