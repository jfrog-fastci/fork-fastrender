use vm_js::{
  GcObject, Heap, HeapLimits, HostDefined, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

#[derive(Debug, Default)]
struct Host {
  counter: u32,
}

#[derive(Debug, Default)]
struct ImportFailingHooks {
  microtasks: MicrotaskQueue,
}

impl VmHostHooks for ImportFailingHooks {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<RealmId>) {
    self.microtasks.host_enqueue_promise_job(job, realm);
  }

  fn host_load_imported_module(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _modules: &mut ModuleGraph,
    _referrer: ModuleReferrer,
    _module_request: ModuleRequest,
    _host_defined: HostDefined,
    _payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    // Signal a *loading failure* (dynamic `import()` promise rejection) via a thrown value rather
    // than an internal VM error.
    let err_s = scope.alloc_string("HostLoadImportedModule")?;
    Err(VmError::Throw(Value::String(err_s)))
  }
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

#[test]
fn host_context_is_preserved_when_instanceof_gets_symbol_has_instance() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();
  assert_eq!(host.counter, 0);

  // `instanceof` performs `GetMethod(C, @@hasInstance)`, which can invoke an accessor getter.
  // Ensure the getter runs with the embedder host context.
  rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      function C() {}
      Object.defineProperty(C, Symbol.hasInstance, {
        get: function () {
          inc();
          return function () { return true; };
        },
        configurable: true,
      });
      ({} instanceof C);
    "#,
  )?;

  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn host_context_is_preserved_when_global_assignment_invokes_accessor_setter() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();
  assert_eq!(host.counter, 0);

  // Sloppy-mode assignment to an unqualified identifier uses the global object as the target.
  // If an accessor setter exists on the global object, it must be invoked with the embedder host
  // context.
  rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      Object.defineProperty(globalThis, "x", {
        set(v) { inc(); },
        configurable: true,
      });
      x = 1;
    "#,
  )?;

  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn host_context_is_preserved_when_dynamic_import_coerces_specifier_and_options() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = ImportFailingHooks::default();
  assert_eq!(host.counter, 0);

  // Dynamic `import()` must synchronously:
  // - coerce the module specifier via `ToString`, and
  // - inspect the `options.with` property when import attributes are present.
  //
  // Both operations can invoke user code.
  rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      import(
        { toString() { inc(); return "./m.js"; } },
        { get with() { inc(); } }
      );
    "#,
  )?;

  hooks.microtasks.cancel_all(&mut rt);
  assert_eq!(host.counter, 2);
  Ok(())
}
