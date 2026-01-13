use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_vm_heap_realm() -> Result<(Vm, Heap, Realm), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let realm = Realm::new(&mut vm, &mut heap)?;
  Ok((vm, heap, realm))
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

#[test]
fn module_import_default_binding_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(&mut heap, "export default 41;")?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import x from "a.js";
        export const y = x + 1;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

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
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "y")?,
    Value::Number(42.0)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_import_namespace_binding_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export const foo = 1;
        export const bar = 2;
      "#,
    )?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import * as ns from "a.js";
        export const sum = ns.foo + ns.bar;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

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
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "sum")?,
    Value::Number(3.0)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_import_named_alias_binding_executes() -> Result<(), VmError> {
  let (mut vm, mut heap, mut realm) = new_vm_heap_realm()?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(&mut heap, "export const foo = 10;")?,
  )?;
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import { foo as bar } from "a.js";
        export const val = bar;
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();

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
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "val")?,
    Value::Number(10.0)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

