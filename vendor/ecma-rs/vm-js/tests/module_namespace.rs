use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, PropertyKind, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

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

#[test]
fn module_namespace_is_cached_and_spec_shaped() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  let record = SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      export const b = 1;
      export const a = 2;
    "#,
  )?;
  let module = graph.add_module(record)?;

  // Module namespaces are backed by module environments; link before requesting the namespace.
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), module)?;

  let mut scope = heap.scope();
  let intrinsics = vm.intrinsics().expect("realm should initialize intrinsics");
  let ns1 = graph.get_module_namespace(module, &mut vm, &mut scope)?;
  let ns2 = graph.get_module_namespace(module, &mut vm, &mut scope)?;
  assert_eq!(ns1, ns2, "namespace object should be cached");
  assert_eq!(scope.heap().object_prototype(ns1)?, None, "namespace prototype must be null");
  assert!(!scope.object_is_extensible(ns1)?, "module namespace must be non-extensible");

  let key = PropertyKey::Symbol(intrinsics.well_known_symbols().to_string_tag);
  let desc = scope
    .heap()
    .object_get_own_property(ns1, &key)?
    .expect("%Symbol.toStringTag% should be defined");

  assert!(!desc.enumerable);
  assert!(!desc.configurable);
  match desc.kind {
    PropertyKind::Data {
      value: Value::String(s),
      writable,
    } => {
      assert!(!writable);
      assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "Module");
    }
    _ => panic!("%Symbol.toStringTag% must be a non-writable data property"),
  }

  assert_eq!(
    graph.module_namespace_exports(module).unwrap(),
    &["a".to_string(), "b".to_string()],
    "exports list should be sorted"
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_namespace_import_star_is_non_extensible() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import * as ns from "a.js";
        export const isExtensible = Object.isExtensible(ns);
        export const tag = Object.prototype.toString.call(ns);
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_consumer = graph.get_module_namespace(consumer, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "isExtensible"
    )?,
    Value::Bool(false),
    "Object.isExtensible(ns) should observe non-extensibility"
  );

  let Value::String(tag) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "tag")? else {
    panic!("expected tag to be a string");
  };
  assert_eq!(scope.heap().get_string(tag)?.to_utf8_lossy(), "[object Module]");

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_namespace_rejects_adding_new_properties_in_strict_mode() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import * as ns from "a.js";
        ns.newProp = 1;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = scope
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  let Value::Object(err_obj) = reason else {
    panic!("promise rejection reason should be an object");
  };

  {
    let mut reason_scope = scope.reborrow();
    reason_scope.push_root(Value::Object(err_obj))?;
    let name_key = PropertyKey::from_string(reason_scope.alloc_string("name")?);
    let Some(desc) = reason_scope.heap().object_get_own_property(err_obj, &name_key)? else {
      panic!("TypeError should have a 'name' property");
    };
    let PropertyKind::Data { value, .. } = desc.kind else {
      panic!("TypeError.name should be a data property");
    };
    let Value::String(name) = value else {
      panic!("TypeError.name should be a string");
    };
    assert_eq!(reason_scope.heap().get_string(name)?.to_utf8_lossy(), "TypeError");
  }

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
