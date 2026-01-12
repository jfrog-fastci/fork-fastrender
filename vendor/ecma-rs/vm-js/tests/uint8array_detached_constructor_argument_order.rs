use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn uint8array_constructor_argument_conversions_happen_before_detached_check() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let ab = rt.exec_script("var ab = new ArrayBuffer(4); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  // Detach the buffer using the host-side heap API (models `DetachArrayBuffer`).
  rt.heap_mut().detach_array_buffer(ab)?;

  // `byteOffset` conversion (and side effects) must occur before the detached-buffer TypeError.
  let value = rt.exec_script(
    r#"
    var called = 0;
    var off = { valueOf(){ called++; return 0; } };
    var ok = false;
    try { new Uint8Array(ab, off, 0); } catch(e) { ok = e.name === 'TypeError'; }
    ok && called === 1
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Conversion errors must take precedence over the detached-buffer TypeError.
  let value = rt.exec_script(
    r#"
    var ok = false;
    try { new Uint8Array(ab, { valueOf(){ throw 123; } }, 0); }
    catch(e) { ok = (e === 123); }
    ok
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // `length` conversion (and side effects) must also happen before the detached-buffer TypeError.
  let value = rt.exec_script(
    r#"
    var called = 0;
    var len = { valueOf(){ called++; return 0; } };
    var ok = false;
    try { new Uint8Array(ab, 0, len); } catch(e) { ok = e.name === 'TypeError'; }
    ok && called === 1
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
