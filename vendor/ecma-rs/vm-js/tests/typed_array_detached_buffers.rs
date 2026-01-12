use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn uint8_array_integer_index_semantics_on_detached_buffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Create `ab` + `u` in the global scope and return `ab` so we can detach it from Rust.
  let ab = rt.exec_script("var ab = new ArrayBuffer(1); var u = new Uint8Array(ab); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script(
    r#"
      u[0] === undefined &&
        u.hasOwnProperty('0') === false &&
        ('0' in u) === false &&
        Object.keys(u).length === 0 &&
        (() => { try { u[0] = 1; return true; } catch(e) { return false; } })()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
