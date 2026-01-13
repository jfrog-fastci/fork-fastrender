use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, SourceTextModuleRecord,
  Value, Vm, VmError, VmJobContext, VmOptions,
};

struct RootOnlyJobCtx<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for RootOnlyJobCtx<'_> {
  fn call(
    &mut self,
    _host: &mut dyn vm_js::VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootOnlyJobCtx::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn vm_js::VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootOnlyJobCtx::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: vm_js::RootId) {
    self.heap.remove_root(id)
  }
}

#[test]
fn module_graph_teardown_unregisters_persistent_roots() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let roots_before = heap.persistent_root_count();
  let env_roots_before = heap.persistent_env_root_count();

  let mut graph = ModuleGraph::new();
  let module = graph.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      export const meta = import.meta;
      export const x = 1;
    "#,
  )?);

  // Module namespaces are backed by module environments; link before requesting the namespace.
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), module)?;

  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module,
    &mut host,
    &mut hooks,
  )?;

  let (namespace_obj, import_meta_obj) = {
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = graph.get_module_namespace(module, &mut vm, &mut scope)?;
    scope.push_root(Value::Object(ns))?;

    let key_s = scope.alloc_string("meta")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    let meta_value =
      scope.get_with_host_and_hooks(&mut vm, &mut host, &mut hooks, ns, key, Value::Object(ns))?;
    let Value::Object(meta_obj) = meta_value else {
      panic!("expected exported `meta` binding to be an object");
    };

    (ns, meta_obj)
  };

  // Before teardown, the module graph's persistent roots keep the allocations alive across GCs.
  heap.collect_garbage();
  assert!(heap.is_valid_object(namespace_obj));
  assert!(heap.is_valid_object(import_meta_obj));
  // The module graph persists:
  // - the module namespace object,
  // - the cached `import.meta` object, and
  // - the module evaluation PromiseCapability (promise + resolve + reject) for the SCC root.
  assert_eq!(heap.persistent_root_count(), roots_before + 5);
  assert_eq!(heap.persistent_env_root_count(), env_roots_before + 1);

  // Ensure teardown clears any attached module graph pointer.
  vm.set_module_graph(&mut graph);
  assert!(vm.module_graph_ptr().is_some());

  graph.teardown(&mut vm, &mut heap);
  assert!(vm.module_graph_ptr().is_none());

  // Discard any queued Promise jobs (should be empty, but must be safe to call).
  {
    let mut ctx = RootOnlyJobCtx { heap: &mut heap };
    hooks.teardown(&mut ctx);
  }

  assert_eq!(heap.persistent_root_count(), roots_before);
  assert_eq!(heap.persistent_env_root_count(), env_roots_before);

  heap.collect_garbage();
  assert!(!heap.is_valid_object(namespace_obj));
  assert!(!heap.is_valid_object(import_meta_obj));

  realm.teardown(&mut heap);
  Ok(())
}
