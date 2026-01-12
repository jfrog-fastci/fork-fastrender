use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

fn ns_get(
  vm: &mut Vm,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  ns: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(ns))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.ordinary_get_with_host_and_hooks(vm, &mut (), hooks, ns, key, Value::Object(ns))
}

struct TestCtx<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
}

impl VmJobContext for TestCtx<'_> {
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

#[test]
fn module_tla_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let module = graph.add_module_with_specifier(
    "tla.js",
    SourceTextModuleRecord::parse(
      r#"
        export let called = 0;
        export let out = "";

        const p = Promise.resolve(1);
        const ctor = {};
        ctor[Symbol.species] = function C(executor) {
          called++;
          return new Promise(executor);
        };
        p.constructor = ctor;

        await p;
        out = "ok";
      "#,
    )?,
  );
  graph.link_all_by_specifier();

  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module,
    &mut host,
    &mut hooks,
  )?;

  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };

  // Top-level await: evaluation promise starts pending and settles after Promise jobs run.
  {
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Pending);
  }

  let mut ctx = TestCtx { vm: &mut vm, heap: &mut heap };
  let errors = hooks.perform_microtask_checkpoint(&mut ctx);
  assert!(errors.is_empty(), "microtask checkpoint errors: {errors:?}");

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns = graph.get_module_namespace(module, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut hooks, &mut scope, ns, "called")?,
    Value::Number(0.0)
  );
  let Value::String(out) = ns_get(&mut vm, &mut hooks, &mut scope, ns, "out")? else {
    panic!("expected module namespace export 'out' to be a string");
  };
  assert_eq!(scope.heap().get_string(out)?.to_utf8_lossy(), "ok");

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

