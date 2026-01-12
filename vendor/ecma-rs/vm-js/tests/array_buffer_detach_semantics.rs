use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn uint8_array_over_detached_array_buffer_behaves_like_empty_view() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script("globalThis.ab = new ArrayBuffer(4); globalThis.u = new Uint8Array(ab); u[0]=7;")?;

  let ab = rt.exec_script("ab")?;
  let Value::Object(ab_obj) = ab else {
    panic!("expected ab to be an object, got {ab:?}");
  };
  let _ = rt.heap.detach_array_buffer_take_data(ab_obj)?;

  assert_eq!(rt.exec_script("ab.byteLength === 0")?, Value::Bool(true));
  assert_eq!(
    rt.exec_script("u.length === 0 && u.byteLength === 0 && u.byteOffset === 0")?,
    Value::Bool(true)
  );
  assert_eq!(rt.exec_script("u[0] === undefined")?, Value::Bool(true));
  assert_eq!(
    rt.exec_script("u.hasOwnProperty('0') === false")?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script("(() => { 'use strict'; u[0]=1; return true; })()")?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(
      "(() => { try { new Uint8Array(ab); } catch(e) { return e.name==='TypeError'; } return false; })()",
    )?,
    Value::Bool(true)
  );

  Ok(())
}
#[test]
fn typed_array_views_with_non_zero_offset_report_zero_length_and_offset_after_detach() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    "globalThis.ab = new ArrayBuffer(8);
     globalThis.u = new Uint8Array(ab, 1);
     globalThis.i = new Int16Array(ab, 2);
     u[0]=7;
     i[0]=9;",
  )?;

  assert_eq!(
    rt.exec_script("u.byteOffset === 1 && i.byteOffset === 2")?,
    Value::Bool(true)
  );

  let ab = rt.exec_script("ab")?;
  let Value::Object(ab_obj) = ab else {
    panic!("expected ab to be an object, got {ab:?}");
  };
  let _ = rt.heap.detach_array_buffer_take_data(ab_obj)?;

  assert_eq!(
    rt.exec_script("u.length === 0 && u.byteLength === 0 && u.byteOffset === 0")?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script("i.length === 0 && i.byteLength === 0 && i.byteOffset === 0")?,
    Value::Bool(true)
  );

  Ok(())
}
