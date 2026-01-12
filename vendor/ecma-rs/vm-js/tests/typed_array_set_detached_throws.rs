use vm_js::{GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
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
fn typed_array_set_detached_target_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .expect("register detachArrayBuffer");

  let value = rt.exec_script(
    r#"
      let ab = new ArrayBuffer(4);
      let u = new Uint8Array(ab);
      detachArrayBuffer(ab);
      let ok = false;
      try { u.set(new Uint8Array(0)); } catch(e) { ok = e.name === 'TypeError'; }
      ok
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_set_detached_source_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .expect("register detachArrayBuffer");

  let value = rt.exec_script(
    r#"
      let ab = new ArrayBuffer(4);
      let u = new Uint8Array(ab);
      let srcAb = new ArrayBuffer(0);
      let src = new Uint8Array(srcAb);
      detachArrayBuffer(srcAb);
      let ok = false;
      try { u.set(src); } catch(e) { ok = e.name === 'TypeError'; }
      ok
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

