use vm_js::{Heap, HeapLimits, Realm, Scope, Value, Vm, VmError, VmOptions};

use webidl_vm_js::{IterableKind, WebIdlBindingsHost};

struct DummyHost;

impl WebIdlBindingsHost for DummyHost {
  fn call_operation(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _receiver: Option<Value>,
    _interface: &'static str,
    _operation: &'static str,
    _overload: usize,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("DummyHost::call_operation"))
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
    Err(VmError::Unimplemented("DummyHost::call_constructor"))
  }
}

#[test]
fn default_iterable_snapshot_throws_type_error_with_interface_and_kind() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host = DummyHost;

  {
    let mut scope = heap.scope();
    let err = host
      .iterable_snapshot(
        &mut vm,
        &mut scope,
        None,
        "TestInterface",
        IterableKind::Entries,
      )
      .expect_err("expected default iterable snapshot to throw");

    let thrown = match err {
      VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => value,
      other => panic!("expected thrown exception, got {other:?}"),
    };

    let Value::Object(obj) = thrown else {
      panic!("expected thrown value to be an object, got {thrown:?}");
    };
    scope.push_root(thrown)?;

    // TypeError objects created by vm-js store `message` as an own data property.
    let msg_key_s = scope.alloc_string("message")?;
    scope.push_root(Value::String(msg_key_s))?;
    let msg_key = vm_js::PropertyKey::from_string(msg_key_s);

    let msg_value = scope.heap().get(obj, &msg_key)?;
    let Value::String(msg_s) = msg_value else {
      panic!("expected error.message to be a string, got {msg_value:?}");
    };
    let msg = scope.heap().get_string(msg_s)?.to_utf8_lossy();

    assert!(msg.contains("TestInterface"), "message missing interface: {msg}");
    assert!(msg.contains("Entries"), "message missing kind: {msg}");

    // Ensure the thrown object is a realm TypeError instance.
    let name_key_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(name_key_s))?;
    let name_key = vm_js::PropertyKey::from_string(name_key_s);
    let name_value = scope.heap().get(obj, &name_key)?;
    let Value::String(name_s) = name_value else {
      panic!("expected error.name to be a string, got {name_value:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "TypeError");
  }

  realm.teardown(&mut heap);
  Ok(())
}

