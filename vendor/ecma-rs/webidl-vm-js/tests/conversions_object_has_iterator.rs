use std::any::Any;

use vm_js::{
  Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, Value, Vm, VmError, VmHostHooks,
  VmOptions,
};

use webidl_vm_js::bindings_runtime::{BindingsRuntime, DataPropertyAttributes};
use webidl_vm_js::conversions;

struct DummyHooks;

impl VmHostHooks for DummyHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    None
  }
}

fn alloc_key(scope: &mut vm_js::Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn assert_thrown_type_error(
  rt: &mut BindingsRuntime<'_>,
  realm: &Realm,
  err: VmError,
  expected_message: &str,
) -> Result<(), VmError> {
  let thrown = err.thrown_value().expect("expected a thrown exception value");
  let Value::Object(obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };

  let intr = realm.intrinsics();
  assert_eq!(rt.scope.object_get_prototype(obj)?, Some(intr.type_error_prototype()));

  rt.scope.push_root(thrown)?;
  let name_key = alloc_key(&mut rt.scope, "name")?;
  let message_key = alloc_key(&mut rt.scope, "message")?;
  let name_val = rt.scope.heap().get(obj, &name_key)?;
  let message_val = rt.scope.heap().get(obj, &message_key)?;

  let Value::String(name_s) = name_val else {
    return Err(VmError::TypeError("expected error.name to be a string"));
  };
  let Value::String(message_s) = message_val else {
    return Err(VmError::TypeError("expected error.message to be a string"));
  };

  assert_eq!(rt.scope.heap().get_string(name_s)?.to_utf8_lossy(), "TypeError");
  assert_eq!(
    rt.scope.heap().get_string(message_s)?.to_utf8_lossy(),
    expected_message
  );

  Ok(())
}

#[test]
fn object_has_iterator_array_returns_false_when_iterator_is_undefined() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  let arr = rt.alloc_array(0)?;

  // Array.prototype[Symbol.iterator] = undefined
  let intr = rt
    .vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  rt.define_data_property(
    intr.array_prototype(),
    iterator_key,
    Value::Undefined,
    DataPropertyAttributes::METHOD,
  )?;

  let has_iter = conversions::object_has_iterator(&mut rt, &mut dummy_host, &mut hooks, arr)?;
  assert!(!has_iter, "arrays should not be treated as iterable when @@iterator is undefined");

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn object_has_iterator_array_throws_when_iterator_is_not_callable() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  let arr = rt.alloc_array(0)?;

  // Array.prototype[Symbol.iterator] = 1
  let intr = rt
    .vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  rt.define_data_property(
    intr.array_prototype(),
    iterator_key,
    Value::Number(1.0),
    DataPropertyAttributes::METHOD,
  )?;

  let err = conversions::object_has_iterator(&mut rt, &mut dummy_host, &mut hooks, arr)
    .expect_err("expected non-callable @@iterator to throw TypeError");

  assert_thrown_type_error(&mut rt, &realm, err, "GetMethod: target is not callable")?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
