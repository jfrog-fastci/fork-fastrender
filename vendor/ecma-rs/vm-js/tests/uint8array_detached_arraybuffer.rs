use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn uint8array_constructor_throws_on_detached_arraybuffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let ab = rt.exec_script("var ab = new ArrayBuffer(4); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  // Detach the buffer using the host-side heap API (models `DetachArrayBuffer`).
  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script(
    r#"
    var threw = false;
    try { new Uint8Array(ab); } catch (e) { threw = e.name === "TypeError"; }
    threw
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Edge case: a 0-length view is still illegal on a detached ArrayBuffer.
  let value = rt.exec_script(
    r#"
    var threw = false;
    try { new Uint8Array(ab, 0, 0); } catch (e) { threw = e.name === "TypeError"; }
    threw
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
