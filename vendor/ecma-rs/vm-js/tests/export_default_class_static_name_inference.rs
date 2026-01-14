use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm, Scope,
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
fn export_default_anonymous_class_static_name_not_overwritten_and_internal_name_inferred(
) -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export default (class { static name(){} });
      "#,
    )?,
  )?;
  let b = graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import C from "a.js";
        export const typeofName = typeof C.name;
        export const nameName = typeof C.name === "function" ? C.name.name : null;
        export const s = C.toString();
      "#,
    )?,
  )?;
  graph.link_all_by_specifier();
  graph.link(&mut vm, &mut heap, realm.global_object(), realm.id(), b)?;

  let mut hooks = MicrotaskQueue::new();
  let mut host = ();
  let promise = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    b,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let ns_b = graph.get_module_namespace(b, &mut vm, &mut scope)?;
  let Value::String(typeof_name) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "typeofName")?
  else {
    panic!("expected typeofName to be a string");
  };
  assert_eq!(scope.heap().get_string(typeof_name)?.to_utf8_lossy(), "function");

  let Value::String(name_name) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "nameName")?
  else {
    panic!("expected nameName to be a string");
  };
  assert_eq!(scope.heap().get_string(name_name)?.to_utf8_lossy(), "name");

  let Value::String(s) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_b, "s")? else {
    panic!("expected s to be a string");
  };
  assert_eq!(
    scope.heap().get_string(s)?.to_utf8_lossy(),
    "function default() { [native code] }"
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

