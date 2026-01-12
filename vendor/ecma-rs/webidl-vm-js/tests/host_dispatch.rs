use std::any::Any;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
use webidl_vm_js::{
  host_from_hooks, WebIdlBindingsHost, WebIdlBindingsHostSlot, WEBIDL_BINDINGS_HOST_NOT_AVAILABLE,
};

struct TestBindingsHost {
  calls: usize,
}

impl TestBindingsHost {
  fn new() -> Self {
    Self { calls: 0 }
  }
}

impl WebIdlBindingsHost for TestBindingsHost {
  fn call_operation(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _receiver: Option<Value>,
    _interface: &'static str,
    _operation: &'static str,
    _overload: usize,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self.calls += 1;
    Ok(Value::Number(args.len() as f64))
  }

  fn call_constructor(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _interface: &'static str,
    _overload: usize,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented(
      "constructor dispatch not used in this test",
    ))
  }
}

struct HostHooksWithBindingsHost {
  slot: WebIdlBindingsHostSlot,
}

impl HostHooksWithBindingsHost {
  fn new(host: &mut dyn WebIdlBindingsHost) -> Self {
    Self {
      slot: WebIdlBindingsHostSlot::new(host),
    }
  }
}

impl VmHostHooks for HostHooksWithBindingsHost {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(&mut self.slot)
  }
}

struct HostHooksWithoutBindingsHost;

impl VmHostHooks for HostHooksWithoutBindingsHost {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}
}

fn native_generated_binding(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // This is the critical plumbing: the binding wrapper must obtain the embedder host dispatch via
  // `VmHostHooks` (not `VmHost`, which is often a dummy `()` in real call paths).
  let host = host_from_hooks(hooks)?;
  host.call_operation(vm, scope, Some(this), "TestInterface", "testOp", 0, args)
}

fn alloc_native_function(
  vm: &mut Vm,
  heap: &mut Heap,
  call: vm_js::NativeCall,
) -> Result<GcObject, VmError> {
  let call_id = vm.register_native_call(call)?;
  let mut scope = heap.scope();
  let name = scope.alloc_string("binding")?;
  scope.push_root(Value::String(name))?;
  scope.alloc_native_function(call_id, None, name, 0)
}

#[test]
fn vmjs_webidl_bindings_dispatch_via_hooks_slot() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let func = alloc_native_function(&mut vm, &mut heap, native_generated_binding)?;
  let _func_root = heap.add_root(Value::Object(func))?;

  let mut host = TestBindingsHost::new();
  let mut hooks = HostHooksWithBindingsHost::new(&mut host);
  let mut dummy_vm_host = ();

  // Direct host call path: explicit `VmHost` + hooks.
  {
    let mut scope = heap.scope();
    let out = vm.call_with_host_and_hooks(
      &mut dummy_vm_host,
      &mut scope,
      &mut hooks,
      Value::Object(func),
      Value::Undefined,
      &[Value::Number(1.0), Value::Number(2.0)],
    )?;
    assert_eq!(out, Value::Number(2.0));
  }
  assert_eq!(host.calls, 1);

  // Script-ish call path: `Vm::call_with_host` supplies a dummy `VmHost` internally; bindings must
  // still be able to reach the host via `hooks`.
  {
    let mut scope = heap.scope();
    let out = vm.call_with_host(
      &mut scope,
      &mut hooks,
      Value::Object(func),
      Value::Undefined,
      &[Value::Number(123.0)],
    )?;
    assert_eq!(out, Value::Number(1.0));
  }
  assert_eq!(host.calls, 2);

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn vmjs_webidl_bindings_missing_host_slot_throws_type_error() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let func = alloc_native_function(&mut vm, &mut heap, native_generated_binding)?;
  let _func_root = heap.add_root(Value::Object(func))?;

  let mut hooks = HostHooksWithoutBindingsHost;
  let mut dummy_vm_host = ();

  let err = {
    let mut scope = heap.scope();
    vm.call_with_host_and_hooks(
      &mut dummy_vm_host,
      &mut scope,
      &mut hooks,
      Value::Object(func),
      Value::Undefined,
      &[],
    )
    .expect_err("expected missing host to fail")
  };

  match err {
    VmError::TypeError(msg) => assert_eq!(msg, WEBIDL_BINDINGS_HOST_NOT_AVAILABLE),
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
      let Value::Object(obj) = value else {
        panic!("expected thrown error object, got {value:?}");
      };
      let mut scope = heap.scope();
      scope.push_root(value)?;

      // TypeError objects created by vm-js store `name`/`message` as own data properties.
      let name_key_s = scope.alloc_string("name")?;
      scope.push_root(Value::String(name_key_s))?;
      let msg_key_s = scope.alloc_string("message")?;
      scope.push_root(Value::String(msg_key_s))?;

      let name_key = vm_js::PropertyKey::from_string(name_key_s);
      let msg_key = vm_js::PropertyKey::from_string(msg_key_s);

      let name = scope.heap().get(obj, &name_key)?;
      let message = scope.heap().get(obj, &msg_key)?;

      let Value::String(name_s) = name else {
        panic!("expected error.name to be a string, got {name:?}");
      };
      let Value::String(message_s) = message else {
        panic!("expected error.message to be a string, got {message:?}");
      };
      assert_eq!(
        scope.heap().get_string(name_s)?.to_utf8_lossy(),
        "TypeError"
      );
      assert_eq!(
        scope.heap().get_string(message_s)?.to_utf8_lossy(),
        WEBIDL_BINDINGS_HOST_NOT_AVAILABLE
      );
    }
    other => panic!("expected TypeError, got {other:?}"),
  }

  realm.teardown(&mut heap);
  Ok(())
}
