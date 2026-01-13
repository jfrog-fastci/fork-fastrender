use vm_js::{
  CompiledScript, Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn obj_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  obj: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))
}

fn ns_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  ns: vm_js::GcObject,
  name: &str,
) -> Result<Value, VmError> {
  obj_get(vm, host, hooks, scope, ns, name)
}

#[test]
fn compiled_module_export_default_expr_preserves_statement_order() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let src = r#"
    export const order = [];
    function sideEffect(x) { order.push(x); }
    order.push("before");
    export default (sideEffect("default"), 123);
    order.push("after");
  "#;

  let mut record = SourceTextModuleRecord::parse(&mut heap, src)?;
  record.compiled = Some(CompiledScript::compile_module(&mut heap, "m.js", src)?);

  let mut graph = ModuleGraph::new();
  let module = graph.add_module(record)?;

  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), module)?;

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns = graph.get_module_namespace(module, &mut vm, &mut scope)?;
  let default_value = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "default")?;
  assert_eq!(default_value, Value::Number(123.0));

  let Value::Object(order_obj) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns, "order")? else {
    panic!("expected exported `order` to be an object");
  };

  let Value::String(s0) = obj_get(&mut vm, &mut host, &mut hooks, &mut scope, order_obj, "0")? else {
    panic!("expected order[0] to be a string");
  };
  let Value::String(s1) = obj_get(&mut vm, &mut host, &mut hooks, &mut scope, order_obj, "1")? else {
    panic!("expected order[1] to be a string");
  };
  let Value::String(s2) = obj_get(&mut vm, &mut host, &mut hooks, &mut scope, order_obj, "2")? else {
    panic!("expected order[2] to be a string");
  };

  assert_eq!(scope.heap().get_string(s0)?.to_utf8_lossy(), "before");
  assert_eq!(scope.heap().get_string(s1)?.to_utf8_lossy(), "default");
  assert_eq!(scope.heap().get_string(s2)?.to_utf8_lossy(), "after");

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_module_exports_can_be_imported_by_another_module() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();

  let producer_src = "export const x = 1;";
  let mut producer = SourceTextModuleRecord::parse(&mut heap, producer_src)?;
  producer.compiled = Some(CompiledScript::compile_module(&mut heap, "a.js", producer_src)?);
  let producer_id = graph.add_module_with_specifier("a.js", producer)?;

  let consumer_id = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { x } from "a.js";
        export const y = x;
        export default x;
      "#,
    )?,
  )?;

  graph.link_all_by_specifier();

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    consumer_id,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_producer = graph.get_module_namespace(producer_id, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_producer, "x")?,
    Value::Number(1.0)
  );

  let ns_consumer = graph.get_module_namespace(consumer_id, &mut vm, &mut scope)?;
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "y")?,
    Value::Number(1.0)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "default")?,
    Value::Number(1.0)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_module_import_meta_is_cached_per_module() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();

  let module_src = r#"
    export const meta1 = import.meta;
    export const meta2 = import.meta;
  "#;

  let mut record_a = SourceTextModuleRecord::parse(&mut heap, module_src)?;
  record_a.compiled = Some(CompiledScript::compile_module(&mut heap, "a.js", module_src)?);
  let a = graph.add_module_with_specifier("a.js", record_a)?;

  let mut record_b = SourceTextModuleRecord::parse(&mut heap, module_src)?;
  record_b.compiled = Some(CompiledScript::compile_module(&mut heap, "b.js", module_src)?);
  let b = graph.add_module_with_specifier("b.js", record_b)?;

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  // Evaluate both modules so their `import.meta` objects are created.
  for root in [a, b] {
    let promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      root,
      &mut host,
      &mut hooks,
    )?;
    let mut scope = heap.scope();
    scope.push_root(promise)?;
    let Value::Object(promise_obj) = promise else {
      panic!("ModuleGraph::evaluate should return a Promise object");
    };
    assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  }

  let mut scope = heap.scope();

  let ns_a = graph.get_module_namespace(a, &mut vm, &mut scope)?;
  let Value::Object(a1) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "meta1")? else {
    panic!("expected meta1 to be an object");
  };
  let Value::Object(a2) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_a, "meta2")? else {
    panic!("expected meta2 to be an object");
  };
  assert_eq!(a1, a2, "import.meta should be cached per module");
  assert_eq!(
    scope.heap().object_prototype(a1)?,
    None,
    "import.meta should be a null-prototype object"
  );

  let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
  let Value::Object(b1) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "meta1")? else {
    panic!("expected meta1 to be an object");
  };
  assert_ne!(
    a1, b1,
    "import.meta objects should be distinct for different modules"
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
