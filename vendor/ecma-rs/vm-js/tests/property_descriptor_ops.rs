use vm_js::{
  from_property_descriptor, to_property_descriptor_with_host_and_hooks, GcObject, Heap, HeapLimits,
  MicrotaskQueue, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};

#[derive(Default)]
struct TestHost {
  called: bool,
}

fn enumerable_getter(
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
    .downcast_mut::<TestHost>()
    .expect("expected TestHost");
  host.called = true;
  Ok(Value::Bool(true))
}

#[test]
fn to_property_descriptor_observes_accessor_getters_on_descriptor_object() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let mut vm = Vm::new(VmOptions::default());
  let mut hooks = MicrotaskQueue::new();
  let mut host = TestHost::default();

  let call_id = vm.register_native_call(enumerable_getter)?;

  let mut scope = heap.scope();
  let desc_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_obj))?;

  let getter_name = scope.alloc_string("getter")?;
  let getter = scope.alloc_native_function(call_id, None, getter_name, 0)?;
  scope.push_root(Value::Object(getter))?;

  let enumerable_key_s = scope.alloc_string("enumerable")?;
  let enumerable_key = PropertyKey::from_string(enumerable_key_s);
  scope.define_property(
    desc_obj,
    enumerable_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(getter),
        set: Value::Undefined,
      },
    },
  )?;

  let patch =
    to_property_descriptor_with_host_and_hooks(&mut vm, &mut scope, &mut host, &mut hooks, desc_obj)?;
  assert_eq!(patch.enumerable, Some(true));
  assert!(host.called);
  Ok(())
}

#[test]
fn to_property_descriptor_rejects_mixing_value_and_get_even_if_get_is_undefined() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut hooks = MicrotaskQueue::new();
  let mut host = TestHost::default();
  let mut scope = heap.scope();

  let desc_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_obj))?;

  let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
  scope.create_data_property_or_throw(desc_obj, value_key, Value::Number(1.0))?;

  // Important: `get` is present but `undefined`. `ToPropertyDescriptor` must treat it as present.
  let get_key = PropertyKey::from_string(scope.alloc_string("get")?);
  scope.create_data_property_or_throw(desc_obj, get_key, Value::Undefined)?;

  let err =
    to_property_descriptor_with_host_and_hooks(&mut vm, &mut scope, &mut host, &mut hooks, desc_obj)
      .unwrap_err();
  assert!(
    matches!(err, VmError::InvalidPropertyDescriptorPatch | VmError::TypeError(_)),
    "expected InvalidPropertyDescriptorPatch/TypeError, got {err:?}"
  );
  Ok(())
}

#[test]
fn to_property_descriptor_rejects_non_callable_getter() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut hooks = MicrotaskQueue::new();
  let mut host = TestHost::default();
  let mut scope = heap.scope();

  let desc_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_obj))?;

  let get_key = PropertyKey::from_string(scope.alloc_string("get")?);
  scope.create_data_property_or_throw(desc_obj, get_key, Value::Number(1.0))?;

  let err =
    to_property_descriptor_with_host_and_hooks(&mut vm, &mut scope, &mut host, &mut hooks, desc_obj)
      .unwrap_err();
  assert!(matches!(err, VmError::TypeError(_)));
  Ok(())
}

#[test]
fn from_property_descriptor_creates_object_with_expected_fields() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let mut scope = heap.scope();

  let obj = from_property_descriptor(
    &mut scope,
    PropertyDescriptor {
      enumerable: true,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(123.0),
        writable: true,
      },
    },
  )?;
  scope.push_root(Value::Object(obj))?;

  let enumerable_key = PropertyKey::from_string(scope.alloc_string("enumerable")?);
  assert!(matches!(
    scope
      .heap()
      .object_get_own_property(obj, &enumerable_key)?
      .expect("enumerable should exist")
      .kind,
    PropertyKind::Data {
      value: Value::Bool(true),
      ..
    }
  ));

  let configurable_key = PropertyKey::from_string(scope.alloc_string("configurable")?);
  assert!(matches!(
    scope
      .heap()
      .object_get_own_property(obj, &configurable_key)?
      .expect("configurable should exist")
      .kind,
    PropertyKind::Data {
      value: Value::Bool(false),
      ..
    }
  ));

  let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
  assert!(matches!(
    scope
      .heap()
      .object_get_own_property(obj, &value_key)?
      .expect("value should exist")
      .kind,
    PropertyKind::Data {
      value: Value::Number(n),
      ..
    } if n == 123.0
  ));

  let writable_key = PropertyKey::from_string(scope.alloc_string("writable")?);
  assert!(matches!(
    scope
      .heap()
      .object_get_own_property(obj, &writable_key)?
      .expect("writable should exist")
      .kind,
    PropertyKind::Data {
      value: Value::Bool(true),
      ..
    }
  ));

  let get_key = PropertyKey::from_string(scope.alloc_string("get")?);
  assert!(
    scope.heap().object_get_own_property(obj, &get_key)?.is_none(),
    "get should not exist for data descriptor"
  );
  let set_key = PropertyKey::from_string(scope.alloc_string("set")?);
  assert!(
    scope.heap().object_get_own_property(obj, &set_key)?.is_none(),
    "set should not exist for data descriptor"
  );

  Ok(())
}
