use vm_js::{GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn detach_array_buffer(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = arg0 else {
    return Err(VmError::TypeError("detachArrayBuffer expects an ArrayBuffer"));
  };
  scope.heap_mut().detach_array_buffer(obj)?;
  Ok(Value::Undefined)
}

#[test]
fn slice_on_detached_buffers_throws_type_error() {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .unwrap();

  let out = rt
    .exec_script(
      r#"
        var ab = new ArrayBuffer(4);
        var u = new Uint8Array(ab);
        detachArrayBuffer(ab);
        var abOk = (() => { try { ab.slice(0); } catch(e) { return e.name === 'TypeError'; } return false; })();
        var uOk = (() => { try { u.slice(0); } catch(e) { return e.name === 'TypeError'; } return false; })();
        abOk && uOk
      "#,
    )
    .unwrap();
  assert_eq!(out, Value::Bool(true));
}

#[test]
fn heap_byte_access_helpers_throw_type_error_on_detached_and_writes_are_noop() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();

  let ab = scope.alloc_array_buffer(4)?;
  scope.push_root(Value::Object(ab))?;
  let view = scope.alloc_uint8_array(ab, 0, 4)?;
  scope.push_root(Value::Object(view))?;

  // Preserve index-out-of-bounds behaviour (no-op write).
  assert_eq!(scope.heap_mut().uint8_array_write(view, 4, &[1, 2])?, 0);

  scope.heap_mut().detach_array_buffer(ab)?;

  match scope.heap().array_buffer_data(ab).unwrap_err() {
    VmError::TypeError(msg) => assert_eq!(msg, "ArrayBuffer is detached"),
    other => panic!("expected TypeError, got {other:?}"),
  }

  match scope.heap().uint8_array_data(view).unwrap_err() {
    VmError::TypeError(msg) => assert_eq!(msg, "ArrayBuffer is detached"),
    other => panic!("expected TypeError, got {other:?}"),
  }

  // Writes into detached buffers are no-ops.
  assert_eq!(scope.heap_mut().uint8_array_write(view, 0, &[1])?, 0);

  // Preserve out-of-bounds behaviour (no-op write).
  assert_eq!(scope.heap_mut().uint8_array_write(view, 4, &[1, 2])?, 0);

  Ok(())
}
