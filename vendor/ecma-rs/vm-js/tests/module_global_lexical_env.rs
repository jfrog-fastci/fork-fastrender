use vm_js::{
  Heap, HeapLimits, JsRuntime, MicrotaskQueue, PromiseState, PropertyKey, Scope,
  SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
fn module_env_outer_links_to_global_lexical_env() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(rt.exec_script("let foo = 123; foo")?, Value::Number(123.0));

  let record = SourceTextModuleRecord::parse(&mut rt.heap, "export const bar = foo;")?;
  let module = rt.modules_mut().add_module(record);

  let global_object = rt.realm().global_object();
  let realm_id = rt.realm().id();

  let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let promise = modules.evaluate(
    vm,
    heap,
    global_object,
    realm_id,
    module,
    &mut host,
    &mut hooks,
  )?;

  let mut scope = heap.scope();
  scope.push_root(promise)?;
  let Value::Object(promise_obj) = promise else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };

  let state = scope.heap().promise_state(promise_obj)?;
  if state != PromiseState::Fulfilled {
    let reason = scope
      .heap()
      .promise_result(promise_obj)?
      .unwrap_or(Value::Undefined);
    panic!("expected module evaluation to fulfill, got {state:?} with {reason:?}");
  }

  let ns = modules.get_module_namespace(module, vm, &mut scope)?;
  assert_eq!(
    ns_get(vm, &mut host, &mut hooks, &mut scope, ns, "bar")?,
    Value::Number(123.0)
  );
  Ok(())
}
