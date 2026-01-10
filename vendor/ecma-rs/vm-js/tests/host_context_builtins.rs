use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};

#[derive(Debug, Default)]
struct Host {
  counter: u32,
}

fn inc_host_counter(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host
    .as_any_mut()
    .downcast_mut::<Host>()
    .ok_or(VmError::Unimplemented("host context has unexpected type"))?;
  host.counter += 1;
  Ok(Value::Undefined)
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn host_context_is_preserved_when_builtins_invoke_user_code() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();
  assert_eq!(host.counter, 0);

  // The Promise constructor invokes its executor synchronously, which in turn calls into the host
  // through the `inc()` native handler.
  rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "new Promise(() => { inc(); });")?;

  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn host_context_is_preserved_when_promise_resolve_invokes_then_getter() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();
  assert_eq!(host.counter, 0);

  // `Promise.resolve` must synchronously perform `Get(thenable, "then")`, which can invoke an
  // accessor getter.
  rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "Promise.resolve({ get then(){ inc(); } });")?;

  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn host_context_is_preserved_when_ordinary_create_from_constructor_gets_new_target_prototype(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();
  assert_eq!(host.counter, 0);

  // `OrdinaryCreateFromConstructor` must perform `Get(newTarget, "prototype")`, which can invoke an
  // accessor getter. `Promise` uses this algorithm when allocating the promise object.
  //
  // `Promise.prototype` is non-configurable so we can't redefine it, and `vm-js` does not yet
  // implement `Reflect.construct`. Instead, create a bound function (which has no `.prototype`
  // property by default) and pass it as `new_target` from Rust.
  rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      globalThis.P2 = Promise.bind(null);
      Object.defineProperty(P2, "prototype", {
        get() { inc(); return Object.prototype; },
        configurable: true,
      });
      globalThis.exec = () => {};
    "#,
  )?;

  let promise_ctor = rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "Promise")?;
  let new_target = rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "P2")?;
  let executor = rt.exec_script_with_host_and_hooks(&mut host, &mut hooks, "exec")?;

  // Construct `Promise` with `new_target = P2`.
  let mut scope = rt.heap.scope();
  let _ = rt.vm.construct_with_host_and_hooks(
    &mut host,
    &mut scope,
    &mut hooks,
    promise_ctor,
    &[executor],
    new_target,
  )?;

  assert_eq!(host.counter, 1);
  Ok(())
}
