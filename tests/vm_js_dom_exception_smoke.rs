use fastrender::js::bindings::DomExceptionClassVmJs;
use vm_js::{
  ExecutionContext, Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmOptions,
};

fn key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn as_utf8(scope: &Scope<'_>, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  scope
    .heap()
    .get_string(s)
    .expect("string handle should be valid")
    .to_utf8_lossy()
}

#[test]
fn dom_exception_constructs_and_has_name_message_and_to_string() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  // Install the DOMException class into the realm global object.
  let class = {
    let mut scope = heap.scope();
    DomExceptionClassVmJs::install(&mut vm, &mut scope, &realm)?
  };

  {
    let mut scope = heap.scope();
    let msg_s = scope.alloc_string("m")?;
    scope.push_root(Value::String(msg_s))?;
    let name_s = scope.alloc_string("SyntaxError")?;
    scope.push_root(Value::String(name_s))?;

    let realm_id = realm.id();
    let mut vm = vm.execution_context_guard(ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    });

    let obj = vm.construct_without_host(
      &mut scope,
      Value::Object(class.constructor),
      &[Value::String(msg_s), Value::String(name_s)],
      Value::Object(class.constructor),
    )?;
    scope.push_root(obj)?;
    let Value::Object(obj_handle) = obj else {
      panic!("expected DOMException constructor to return an object, got {obj:?}");
    };

    // .name === "SyntaxError"
    let name_key = key(&mut scope, "name")?;
    let name_value = vm.get(&mut scope, obj_handle, name_key)?;
    assert_eq!(as_utf8(&scope, name_value), "SyntaxError");

    // .message === "m"
    let message_key = key(&mut scope, "message")?;
    let message_value = vm.get(&mut scope, obj_handle, message_key)?;
    assert_eq!(as_utf8(&scope, message_value), "m");

    // toString() === "SyntaxError: m"
    let to_string_key = key(&mut scope, "toString")?;
    let to_string_fn = vm.get(&mut scope, obj_handle, to_string_key)?;
    let out = vm.call_without_host(&mut scope, to_string_fn, Value::Object(obj_handle), &[])?;
    assert_eq!(as_utf8(&scope, out), "SyntaxError: m");

    // Verify property attributes for own `name`/`message`: non-enumerable.
    let name_desc = scope
      .heap()
      .object_get_own_property(obj_handle, &name_key)?
      .expect("expected own name property");
    assert!(!name_desc.enumerable);
    let PropertyKind::Data { .. } = name_desc.kind else {
      panic!("expected name to be a data property");
    };

    let message_desc = scope
      .heap()
      .object_get_own_property(obj_handle, &message_key)?
      .expect("expected own message property");
    assert!(!message_desc.enumerable);
    let PropertyKind::Data { .. } = message_desc.kind else {
      panic!("expected message to be a data property");
    };
  }

  realm.teardown(&mut heap);

  Ok(())
}
