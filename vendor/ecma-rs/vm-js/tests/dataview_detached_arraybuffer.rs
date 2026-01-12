use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn dataview_constructor_throws_on_detached_arraybuffer() -> Result<(), VmError> {
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
    try { new DataView(ab); } catch (e) { threw = e.name === "TypeError"; }
    threw
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Edge case: a 0-length view is still illegal on a detached ArrayBuffer.
  let value = rt.exec_script(
    r#"
    var threw = false;
    try { new DataView(ab, 0, 0); } catch (e) { threw = e.name === "TypeError"; }
    threw
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn dataview_getters_throw_on_detached_arraybuffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let ab = rt.exec_script("var ab = new ArrayBuffer(8); var dv = new DataView(ab, 1, 2); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script(
    r#"
    var byteLengthThrew = false;
    try { dv.byteLength; } catch (e) { byteLengthThrew = e.name === "TypeError"; }
    var byteOffsetThrew = false;
    try { dv.byteOffset; } catch (e) { byteOffsetThrew = e.name === "TypeError"; }
    byteLengthThrew && byteOffsetThrew
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn dataview_methods_throw_on_detached_arraybuffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let ab = rt.exec_script("var ab = new ArrayBuffer(8); var dv = new DataView(ab, 0, 8); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script(
    r#"
    var getThrew = false;
    try { dv.getUint8(0); } catch (e) { getThrew = e.name === "TypeError"; }
    var setThrew = false;
    try { dv.setUint8(0, 1); } catch (e) { setThrew = e.name === "TypeError"; }
    getThrew && setThrew
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn dataview_methods_coerce_args_before_throwing_on_detached_arraybuffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let ab = rt.exec_script("var ab = new ArrayBuffer(8); var dv = new DataView(ab, 0, 8); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script(
    r#"
    var called = false;
    var threw = false;
    try {
      dv.getUint8({ valueOf(){ called = true; return 0; } });
    } catch (e) {
      threw = e.name === "TypeError";
    }
    called && threw
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
