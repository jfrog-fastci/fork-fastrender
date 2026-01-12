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
fn module_namespace_is_exotic_and_spec_shaped() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "a.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        export const a = 2;
        export let x = 1;
      "#,
    )?,
  );
  let consumer = graph.add_module_with_specifier(
    "consumer.js",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
        import * as ns from "a.js";

        const desc = Object.getOwnPropertyDescriptor(ns, "x");
        export const descValue = desc.value;
        export const descEnumerable = desc.enumerable;
        export const descWritable = desc.writable;
        export const descConfigurable = desc.configurable;

        export const defineNoOpOk = Reflect.defineProperty(ns, "x", {});
        export const defineChangeOk = Reflect.defineProperty(ns, "x", { value: 2 });

        const keys = Reflect.ownKeys(ns);
        export const ownKeys0 = keys[0];
        export const ownKeys1 = keys[1];
        export const hasToStringTag = keys.indexOf(Symbol.toStringTag) !== -1;

        export const setProtoNullOk = Object.setPrototypeOf(ns, null) === ns;
        export const setProtoThrows = (() => {
          try {
            Object.setPrototypeOf(ns, {});
            return false;
          } catch (e) {
            return e instanceof TypeError;
          }
        })();

        export const assignThrows = (() => {
          try {
            ns.x = 2;
            return false;
          } catch (e) {
            return e instanceof TypeError;
          }
        })();

        export const deleteThrows = (() => {
          try {
            delete ns.x;
            return false;
          } catch (e) {
            return e instanceof TypeError;
          }
        })();
      "#,
    )?,
  );
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
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "descValue")?,
    Value::Number(1.0)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "descEnumerable")?,
    Value::Bool(true)
  );
  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "descWritable")?,
    Value::Bool(true)
  );
  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "descConfigurable"
    )?,
    Value::Bool(false)
  );

  assert_eq!(
    ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "defineNoOpOk")?,
    Value::Bool(true)
  );
  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "defineChangeOk"
    )?,
    Value::Bool(false)
  );

  let Value::String(k0) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "ownKeys0")?
  else {
    panic!("expected ownKeys0 to be a string");
  };
  assert_eq!(scope.heap().get_string(k0)?.to_utf8_lossy(), "a");
  let Value::String(k1) = ns_get(&mut vm, &mut host, &mut hooks, &mut scope, ns_consumer, "ownKeys1")?
  else {
    panic!("expected ownKeys1 to be a string");
  };
  assert_eq!(scope.heap().get_string(k1)?.to_utf8_lossy(), "x");

  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "hasToStringTag"
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "setProtoNullOk"
    )?,
    Value::Bool(true)
  );
  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "setProtoThrows"
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "assignThrows"
    )?,
    Value::Bool(true)
  );
  assert_eq!(
    ns_get(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      ns_consumer,
      "deleteThrows"
    )?,
    Value::Bool(true)
  );

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
