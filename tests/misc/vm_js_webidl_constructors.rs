use fastrender::js::bindings::install_window_bindings_vm_js;
use std::any::Any;
use vm_js::{
  ExecutionContext, GcObject, Heap, HeapLimits, Job, PropertyDescriptor, PropertyKey, PropertyKind,
  RealmId, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
use webidl_vm_js::{WebIdlBindingsHost, WebIdlBindingsHostSlot};

struct TestHost;

impl WebIdlBindingsHost for TestHost {
  fn call_operation(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut vm_js::Scope<'_>,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    match (interface, operation, overload) {
      // For constructor semantics tests we only need a no-op initialization hook.
      ("URLSearchParams", "constructor", 0) => Ok(Value::Undefined),
      _ => Err(VmError::Unimplemented("unexpected host operation call in test")),
    }
  }

  fn call_constructor(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut vm_js::Scope<'_>,
    _interface: &'static str,
    _overload: usize,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented(
      "unexpected host constructor call in test",
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
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(&mut self.slot)
  }
}

fn dummy_call(
  _vm: &mut Vm,
  _scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn dummy_construct(
  _vm: &mut Vm,
  _scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn get_global(vm: &mut Vm, scope: &mut vm_js::Scope<'_>, global: GcObject, name: &str) -> Value {
  let key_s = scope.alloc_string(name).unwrap();
  scope.push_root(Value::String(key_s)).unwrap();
  vm.get(scope, global, PropertyKey::from_string(key_s)).unwrap()
}

fn get_string_prop(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  obj: GcObject,
  name: &str,
) -> String {
  let key_s = scope.alloc_string(name).unwrap();
  scope.push_root(Value::String(key_s)).unwrap();
  let v = vm.get(scope, obj, PropertyKey::from_string(key_s)).unwrap();
  let Value::String(s) = v else {
    panic!("expected string for {name}, got {v:?}");
  };
  scope.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn assert_thrown_type_error_message(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  err: VmError,
  expected_message: &str,
) {
  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected Throw, got {err:?}"));
  scope.push_root(thrown).unwrap();
  let Value::Object(obj) = thrown else {
    panic!("expected thrown object, got {thrown:?}");
  };
  assert_eq!(get_string_prop(vm, scope, obj, "name"), "TypeError");
  assert_eq!(get_string_prop(vm, scope, obj, "message"), expected_message);
}

#[test]
fn webidl_interface_objects_have_constructor_semantics() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  // Always tear down the realm, even if assertions panic, to avoid triggering `Realm`'s drop-time
  // debug assert about leaked persistent roots.
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let urlsp_ctor = get_global(&mut vm, &mut scope, global, "URLSearchParams");
    let Value::Object(urlsp_ctor_obj) = urlsp_ctor else {
      panic!("expected URLSearchParams to be an object");
    };

    let mut host = TestHost;
    let mut hooks = HostHooksWithBindingsHost::new(&mut host);

    let mut vm = vm.execution_context_guard(ExecutionContext {
      realm: realm.id(),
      script_or_module: None,
    });

    // `new URLSearchParams('a=b')` succeeds and the returned object uses the constructor's prototype.
    let arg = Value::String(scope.alloc_string("a=b")?);
    let value = vm.construct_with_host(&mut scope, &mut hooks, urlsp_ctor, &[arg], urlsp_ctor)?;
    let Value::Object(obj) = value else {
      panic!("expected new URLSearchParams(...) to return an object, got {value:?}");
    };
    // Root the instance before we allocate the `"prototype"` key below.
    scope.push_root(Value::Object(obj))?;

    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let proto_val = vm.get(&mut scope, urlsp_ctor_obj, PropertyKey::from_string(key_s))?;
    let Value::Object(proto_obj) = proto_val else {
      panic!("expected URLSearchParams.prototype to be an object, got {proto_val:?}");
    };
    assert_eq!(
      scope.heap().object_prototype(obj)?,
      Some(proto_obj),
      "expected constructed object to use URLSearchParams.prototype"
    );

    // If `new_target` differs, the constructed wrapper's `[[Prototype]]` is derived from
    // `new_target.prototype` (subclassing semantics).
    let custom_proto = scope.alloc_object_with_prototype(Some(realm.intrinsics().object_prototype()))?;
    scope.push_root(Value::Object(custom_proto))?;

    let call_id = vm.register_native_call(dummy_call)?;
    let construct_id = vm.register_native_construct(dummy_construct)?;

    let new_target_name = scope.alloc_string("SubURLSearchParams")?;
    scope.push_root(Value::String(new_target_name))?;
    let new_target_obj =
      scope.alloc_native_function(call_id, Some(construct_id), new_target_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(new_target_obj, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(new_target_obj))?;

    let proto_key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(proto_key_s))?;
    scope.define_property(
      new_target_obj,
      PropertyKey::from_string(proto_key_s),
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(custom_proto),
          writable: false,
        },
      },
    )?;

    let arg = Value::String(scope.alloc_string("a=b")?);
    let value = vm.construct_with_host(
      &mut scope,
      &mut hooks,
      urlsp_ctor,
      &[arg],
      Value::Object(new_target_obj),
    )?;
    let Value::Object(obj) = value else {
      panic!("expected subclassed URLSearchParams construction to return an object, got {value:?}");
    };
    scope.push_root(Value::Object(obj))?;
    assert_eq!(
      scope.heap().object_prototype(obj)?,
      Some(custom_proto),
      "expected constructed object to use new_target.prototype"
    );

    // Calling without `new` throws a TypeError.
    let arg = Value::String(scope.alloc_string("a=b")?);
    let err = vm
      .call_with_host(&mut scope, &mut hooks, urlsp_ctor, Value::Undefined, &[arg])
      .unwrap_err();
    assert_thrown_type_error_message(
      &mut vm,
      &mut scope,
      err,
      "Illegal constructor",
    );

    Ok(())
  }));

  realm.teardown(&mut heap);

  match result {
    Ok(r) => r,
    Err(payload) => std::panic::resume_unwind(payload),
  }
}
