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
fn subarray_on_detached_buffers_still_evaluates_start_and_end() {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .unwrap();

  let out = rt
    .exec_script(
      r#"
        let log = [];
        let start = { valueOf(){ log.push('start'); return 0; } };
        let end = { valueOf(){ log.push('end'); return 0; } };
        let ab = new ArrayBuffer(4);
        let u = new Uint8Array(ab, 1, 2);
        detachArrayBuffer(ab);
        let threw = false;
        try { u.subarray(start, end); } catch(e) { threw = e.name === 'TypeError'; }
        threw && log.length === 2 && log[0] === 'start' && log[1] === 'end'
      "#,
    )
    .unwrap();
  assert_eq!(out, Value::Bool(true));
}

