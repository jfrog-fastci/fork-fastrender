use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

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
fn detached_slice_does_not_convert_arguments() {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .unwrap();

  let out = rt
    .exec_script(
      r#"
        var abOk = (() => {
          let called = 0;
          let start = { valueOf(){ called++; return 0; } };
          let ab = new ArrayBuffer(4);
          detachArrayBuffer(ab);
          try { ab.slice(start); } catch(e) { return e.name === 'TypeError' && called === 0; }
          return false;
        })();

        var typedArraySliceOk = (() => {
          let called = 0;
          let start = { valueOf(){ called++; return 0; } };
          let ab = new ArrayBuffer(4);
          let u = new Uint8Array(ab);
          detachArrayBuffer(ab);
          try { u.slice(start); } catch(e) { return e.name === 'TypeError' && called === 0; }
          return false;
        })();

        var typedArraySubarrayOk = (() => {
          let called = 0;
          let start = { valueOf(){ called++; return 0; } };
          let ab = new ArrayBuffer(4);
          let u = new Uint8Array(ab);
          detachArrayBuffer(ab);
          try { u.subarray(start); } catch(e) { return e.name === 'TypeError' && called === 0; }
          return false;
        })();

        abOk && typedArraySliceOk && typedArraySubarrayOk
      "#,
    )
    .unwrap();
  assert_eq!(out, Value::Bool(true));
}

