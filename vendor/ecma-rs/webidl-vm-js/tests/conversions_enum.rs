use std::any::Any;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, Value, Vm, VmError, VmHostHooks,
  VmOptions,
};

use webidl_vm_js::bindings_runtime::BindingsRuntime;
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

fn assert_thrown_error(
  rt: &mut BindingsRuntime<'_>,
  realm: &Realm,
  err: VmError,
  expected_proto: GcObject,
  expected_name: &str,
  expected_message: &str,
) -> Result<(), VmError> {
  let thrown = err.thrown_value().expect("expected a thrown exception value");
  let Value::Object(obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };
  assert_eq!(rt.scope.object_get_prototype(obj)?, Some(expected_proto));

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

  assert_eq!(rt.scope.heap().get_string(name_s)?.to_utf8_lossy(), expected_name);
  assert_eq!(
    rt.scope.heap().get_string(message_s)?.to_utf8_lossy(),
    expected_message
  );

  // Keep `realm` live (it owns the intrinsics prototypes).
  let _ = realm;
  Ok(())
}

#[test]
fn enum_conversion_enforces_max_string_code_units() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  rt.set_limits(webidl::WebIdlLimits {
    max_string_code_units: 1,
    ..Default::default()
  });

  let intr = realm.intrinsics();
  let too_long = rt.alloc_string("ab")?;

  let err = conversions::to_enum(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::String(too_long),
    "TestEnum",
    &["x"],
  )
  .expect_err("expected enum length limit to fail");

  assert_thrown_error(
    &mut rt,
    &realm,
    err,
    intr.range_error_prototype(),
    "RangeError",
    "string exceeds maximum length",
  )?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

