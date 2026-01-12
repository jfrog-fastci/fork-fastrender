use std::any::Any;
use vm_js::{
  Heap, HeapLimits, ImportMetaProperty, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey,
  Realm, RootId, Scope, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks,
  VmJobContext, VmOptions,
};

struct RootOnlyJobCtx<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for RootOnlyJobCtx<'_> {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootOnlyJobCtx::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootOnlyJobCtx::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id)
  }
}

fn ns_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
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
  scope.get_with_host_and_hooks(vm, host, hooks, ns, key, Value::Object(ns))
}

#[derive(Default)]
struct ImportMetaHooks {
  queue: MicrotaskQueue,
  url: String,
  get_calls: u32,
}

impl VmHostHooks for ImportMetaHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.queue.host_enqueue_promise_job(job, realm);
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(self)
  }

  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _module: vm_js::ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    self.get_calls += 1;

    // Root across subsequent allocations in case they trigger GC.
    let url_key = scope.alloc_string("url")?;
    scope.push_root(Value::String(url_key))?;
    let url_value = scope.alloc_string(&self.url)?;
    scope.push_root(Value::String(url_value))?;

    Ok(vec![ImportMetaProperty {
      key: PropertyKey::from_string(url_key),
      value: Value::String(url_value),
    }])
  }
}

#[test]
fn import_meta_cache_is_scoped_to_module_graph() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let roots_before = heap.persistent_root_count();
  let env_roots_before = heap.persistent_env_root_count();

  let mut host = ();

  let mut hooks1 = ImportMetaHooks {
    url: "graph1".to_string(),
    ..Default::default()
  };
  let mut graph1 = ModuleGraph::new();
  let module1 = graph1.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      export const m = import.meta;
      export const url = import.meta.url ?? 1;
    "#,
  )?);
  let promise1 = graph1.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module1,
    &mut host,
    &mut hooks1,
  )?;

  let (ns1, meta1, url1) = {
    let mut scope = heap.scope();
    scope.push_root(promise1)?;
    let Value::Object(promise_obj) = promise1 else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = graph1.get_module_namespace(module1, &mut vm, &mut scope)?;
    let Value::Object(meta) = ns_get(&mut vm, &mut host, &mut hooks1, &mut scope, ns, "m")? else {
      panic!("expected m export to be an object");
    };
    let Value::String(url) = ns_get(&mut vm, &mut host, &mut hooks1, &mut scope, ns, "url")? else {
      panic!("expected url export to be a string");
    };
    let url = scope.heap().get_string(url)?.to_utf8_lossy();
    (ns, meta, url)
  };
  assert_eq!(url1, "graph1");

  let mut hooks2 = ImportMetaHooks {
    url: "graph2".to_string(),
    ..Default::default()
  };
  let mut graph2 = ModuleGraph::new();
  let module2 = graph2.add_module(SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      export const m = import.meta;
      export const url = import.meta.url ?? 1;
    "#,
  )?);
  let promise2 = graph2.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module2,
    &mut host,
    &mut hooks2,
  )?;

  let (ns2, meta2, url2) = {
    let mut scope = heap.scope();
    scope.push_root(promise2)?;
    let Value::Object(promise_obj) = promise2 else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

    let ns = graph2.get_module_namespace(module2, &mut vm, &mut scope)?;
    let Value::Object(meta) = ns_get(&mut vm, &mut host, &mut hooks2, &mut scope, ns, "m")? else {
      panic!("expected m export to be an object");
    };
    let Value::String(url) = ns_get(&mut vm, &mut host, &mut hooks2, &mut scope, ns, "url")? else {
      panic!("expected url export to be a string");
    };
    let url = scope.heap().get_string(url)?.to_utf8_lossy();
    (ns, meta, url)
  };

  assert_ne!(
    meta1, meta2,
    "import.meta must be cached per-module-record; separate graphs must not collide on ModuleId"
  );
  assert_eq!(url2, "graph2");

  assert_eq!(hooks1.get_calls, 1);
  assert_eq!(hooks2.get_calls, 1);

  // The cached objects must stay alive across GC while the graphs hold persistent roots.
  heap.collect_garbage();
  assert!(heap.is_valid_object(ns1));
  assert!(heap.is_valid_object(meta1));
  assert!(heap.is_valid_object(ns2));
  assert!(heap.is_valid_object(meta2));

  graph1.teardown(&mut vm, &mut heap);
  graph2.teardown(&mut vm, &mut heap);

  // Discard any queued Promise jobs so their persistent roots are removed.
  {
    let mut ctx = RootOnlyJobCtx { heap: &mut heap };
    hooks1.queue.teardown(&mut ctx);
    hooks2.queue.teardown(&mut ctx);
  }

  assert_eq!(heap.persistent_root_count(), roots_before);
  assert_eq!(heap.persistent_env_root_count(), env_roots_before);

  heap.collect_garbage();
  assert!(!heap.is_valid_object(ns1));
  assert!(!heap.is_valid_object(meta1));
  assert!(!heap.is_valid_object(ns2));
  assert!(!heap.is_valid_object(meta2));

  realm.teardown(&mut heap);
  Ok(())
}
