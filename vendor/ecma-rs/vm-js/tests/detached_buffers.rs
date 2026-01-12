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
fn slice_on_detached_buffers_throws_type_error() {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .expect("register detachArrayBuffer");

  let out = rt
    .exec_script(
      r#"
        var ab = new ArrayBuffer(4);
        var u = new Uint8Array(ab);
        detachArrayBuffer(ab);

        // Integer-indexed access on detached typed arrays should return `undefined`, not crash.
        var idxOk = u[0] === undefined;

        var abOk = (() => { try { ab.slice(0); } catch(e) { return e.name === 'TypeError'; } return false; })();
        var uOk = (() => { try { u.slice(0); } catch(e) { return e.name === 'TypeError'; } return false; })();

        abOk && uOk && idxOk
      "#,
    )
    .expect("script should run");
  assert_eq!(out, Value::Bool(true));
}

#[test]
fn heap_byte_access_helpers_throw_type_error_on_detached() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();

  let ab = scope.alloc_array_buffer(4)?;
  scope.push_root(Value::Object(ab))?;
  let view = scope.alloc_uint8_array(ab, 0, 4)?;
  scope.push_root(Value::Object(view))?;

  // Out-of-bounds behaviour for host writes is a no-op.
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

  // `Uint8Array` writes on detached buffers are a safe no-op.
  assert_eq!(scope.heap_mut().uint8_array_write(view, 0, &[1])?, 0);

  // Preserve out-of-bounds behaviour (no-op write) even if the view is detached.
  assert_eq!(scope.heap_mut().uint8_array_write(view, 4, &[1, 2])?, 0);

  Ok(())
}

#[test]
fn typed_array_set_detach_during_offset_throws_type_error() {
  let mut rt = new_runtime();
  rt
    .register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .unwrap();

  let out = rt
    .exec_script(
      r#"
        // Detachment checks for TypedArray.prototype.set must occur *after* coercing the offset
        // argument, since ToIndex can run user code via `valueOf`/`toString`.

        var ok1 = false;
        try {
          let ab = new ArrayBuffer(1);
          let ta = new Uint8Array(ab);
          let sb = new ArrayBuffer(1);
          let src = new Uint8Array(sb);
          ta.set(src, { valueOf() { detachArrayBuffer(ab); return 0; } });
        } catch (e) {
          ok1 = e && e.name === "TypeError";
        }

        var ok2 = false;
        try {
          let ab2 = new ArrayBuffer(1);
          let ta2 = new Uint8Array(ab2);
          let sb2 = new ArrayBuffer(1);
          let src2 = new Uint8Array(sb2);
          ta2.set(src2, { valueOf() { detachArrayBuffer(sb2); return 0; } });
        } catch (e) {
          ok2 = e && e.name === "TypeError";
        }

        ok1 && ok2
      "#,
    )
    .expect("script should run");

  assert_eq!(out, Value::Bool(true));
}
