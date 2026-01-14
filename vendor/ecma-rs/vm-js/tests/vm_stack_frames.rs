use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks, VmOptions,
};

fn native_error(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let stack = vm.capture_stack();
  assert_eq!(stack.len(), 1);
  assert_eq!(stack[0].function.as_deref(), Some("f"));
  assert_eq!(stack[0].source.as_ref(), "<native>");
  Err(VmError::Unimplemented("x"))
}

#[test]
fn vm_call_pushes_and_pops_stack_frame_even_on_error() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let mut scope = heap.scope();
  let call_id = vm.register_native_call(native_error)?;
  let name = scope.alloc_string("f")?;
  let callee = scope.alloc_native_function(call_id, None, name, 0)?;

  let err = vm
    .call_without_host(&mut scope, Value::Object(callee), Value::Undefined, &[])
    .unwrap_err();
  assert!(matches!(err, VmError::Unimplemented("x")));

  assert!(vm.capture_stack().is_empty());
  Ok(())
}

fn recursive(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let callee = args.get(0).copied().unwrap_or(Value::Undefined);
  vm.call_with_host(scope, hooks, callee, Value::Undefined, args)
}

#[test]
fn vm_stack_overflow_on_deep_manual_frames() -> Result<(), VmError> {
  let max_stack_depth = 4;

  let mut opts = VmOptions::default();
  opts.max_stack_depth = max_stack_depth;
  let vm = Vm::new(opts);
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  // Create a runtime so VM intrinsics are initialized; stack overflow should surface as a thrown
  // RangeError object rather than an internal helper error.
  let mut rt = JsRuntime::new(vm, heap)?;
  let mut scope = rt.heap.scope();
  let call_id = rt.vm.register_native_call(recursive)?;
  let name = scope.alloc_string("recurse")?;
  let callee = scope.alloc_native_function(call_id, None, name, 1)?;

  let args = [Value::Object(callee)];
  let err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(callee), Value::Undefined, &args)
    .unwrap_err();

  let VmError::ThrowWithStack { value, stack } = err else {
    panic!("expected thrown RangeError, got: {err:?}");
  };

  // Best-effort validate it's a RangeError instance: check own `name` data property.
  let Value::Object(err_obj) = value else {
    panic!("expected error object, got: {value:?}");
  };
  scope.push_root(Value::Object(err_obj))?;
  let name_key_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(name_key_s))?;
  let name_key = PropertyKey::from_string(name_key_s);
  let name_desc = scope
    .heap()
    .object_get_own_property(err_obj, &name_key)?
    .expect("RangeError should have an own 'name' property");
  let PropertyKind::Data { value: Value::String(name_s), .. } = name_desc.kind else {
    panic!("expected error.name to be a string data property");
  };
  let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
  assert_eq!(name, "RangeError");

  assert_eq!(stack.len(), max_stack_depth);
  for frame in &stack {
    assert_eq!(frame.source.as_ref(), "<native>");
    assert_eq!(frame.function.as_deref(), Some("recurse"));
    assert_eq!(frame.line, 0);
    assert_eq!(frame.col, 0);
  }

  assert!(rt.vm.capture_stack().is_empty());
  Ok(())
}
