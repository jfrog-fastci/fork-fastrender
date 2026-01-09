use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
  Vm, VmError, VmOptions,
};

// Lightweight integration-smoke test for vm-js' Function.prototype.call intrinsic.
//
// vm-js has its own unit tests, but keeping a small high-level check here helps catch accidental
// regressions when bumping the engines/ecma-rs submodule.

struct TestRealm {
  vm: Vm,
  heap: Heap,
  realm: Realm,
}

impl TestRealm {
  fn new() -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRealm {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

fn reflect_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn vm_js::VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if args.is_empty() {
    Ok(this)
  } else {
    Ok(args[0])
  }
}

fn define_enumerable_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  };
  scope.define_property(obj, key, desc)
}

#[test]
fn function_call_apply_bind_smoke() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let mut scope = rt.heap.scope();

  // Create a host-native function with Function.prototype in its prototype chain.
  let reflect_id = rt.vm.register_native_call(reflect_native)?;
  let name = scope.alloc_string("reflect")?;
  scope.push_root(Value::String(name))?;
  let reflect_fn = scope.alloc_native_function(reflect_id, None, name, 0)?;
  // `alloc_native_function` does not wire up `[[Prototype]]` automatically; mirror the embedder
  // behavior in `src/js/*` by linking native functions to `Function.prototype`.
  scope
    .heap_mut()
    .object_set_prototype(reflect_fn, Some(rt.realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(reflect_fn))?;

  let this_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(this_obj))?;

  // --- Function.prototype.call ---
  let call_key_s = scope.alloc_string("call")?;
  scope.push_root(Value::String(call_key_s))?;
  let call_key = PropertyKey::from_string(call_key_s);
  let call = rt.vm.get(&mut scope, reflect_fn, call_key)?;
  let Value::Object(call) = call else {
    panic!("expected Function.prototype.call to be callable");
  };
  let result = rt.vm.call(
    &mut scope,
    Value::Object(call),
    Value::Object(reflect_fn),
    &[Value::Object(this_obj)],
  )?;
  assert_eq!(result, Value::Object(this_obj));

  // --- Function.prototype.apply ---
  //
  // `apply`/`bind` are optional here: this smoke test is primarily intended to ensure that native
  // host-created functions can reach `Function.prototype.call` via the `[[Prototype]]` chain.
  let apply_key_s = scope.alloc_string("apply")?;
  scope.push_root(Value::String(apply_key_s))?;
  let apply_key = PropertyKey::from_string(apply_key_s);
  let apply = rt.vm.get(&mut scope, reflect_fn, apply_key)?;
  match apply {
    Value::Undefined => {}
    Value::Object(apply) => {
      let args_arr = scope.alloc_array(1)?;
      scope.push_root(Value::Object(args_arr))?;
      define_enumerable_data_property(&mut scope, args_arr, "0", Value::Number(7.0))?;

      let result = rt.vm.call(
        &mut scope,
        Value::Object(apply),
        Value::Object(reflect_fn),
        &[Value::Object(this_obj), Value::Object(args_arr)],
      )?;
      assert_eq!(result, Value::Number(7.0));
    }
    other => panic!("expected Function.prototype.apply to be callable or undefined, got {other:?}"),
  }

  // --- Function.prototype.bind ---
  let bind_key_s = scope.alloc_string("bind")?;
  scope.push_root(Value::String(bind_key_s))?;
  let bind_key = PropertyKey::from_string(bind_key_s);
  let bind = rt.vm.get(&mut scope, reflect_fn, bind_key)?;
  match bind {
    Value::Undefined => {}
    Value::Object(bind) => {
      // Bind only `thisArg`.
      let bound_this = rt.vm.call(
        &mut scope,
        Value::Object(bind),
        Value::Object(reflect_fn),
        &[Value::Object(this_obj)],
      )?;
      let Value::Object(bound_this) = bound_this else {
        panic!("expected bind() to return a function object");
      };
      scope.push_root(Value::Object(bound_this))?;

      let result = rt
        .vm
        .call(&mut scope, Value::Object(bound_this), Value::Undefined, &[])?;
      assert_eq!(result, Value::Object(this_obj));

      // Bind `thisArg` + a leading argument.
      let bound = rt.vm.call(
        &mut scope,
        Value::Object(bind),
        Value::Object(reflect_fn),
        &[Value::Object(this_obj), Value::Number(5.0)],
      )?;
      let Value::Object(bound_fn) = bound else {
        panic!("expected bind() to return a function object");
      };
      scope.push_root(Value::Object(bound_fn))?;

      // Bound function prepends bound args.
      let result = rt.vm.call(
        &mut scope,
        Value::Object(bound_fn),
        Value::Undefined,
        &[Value::Number(6.0)],
      )?;
      assert_eq!(result, Value::Number(5.0));
    }
    other => panic!("expected Function.prototype.bind to be callable or undefined, got {other:?}"),
  }

  Ok(())
}
