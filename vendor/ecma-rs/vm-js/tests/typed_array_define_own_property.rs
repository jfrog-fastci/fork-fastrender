use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_define_property_on_uint8_array_numeric_indices() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `Object.getOwnPropertyDescriptor` reports typed array element properties as configurable.
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        let d = Object.getOwnPropertyDescriptor(u, "0");
        d.configurable === true && d.enumerable === true && d.writable === true
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Round-tripping a descriptor through `Object.defineProperty` should succeed.
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        let d = Object.getOwnPropertyDescriptor(u, "0");
        try {
          Object.defineProperty(u, "0", d);
          true
        } catch (e) {
          false
        }
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Writes go through `[[DefineOwnProperty]]` and update the backing buffer.
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        Object.defineProperty(u, "0", { value: 2 });
        u[0] === 2
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Rejected descriptor fields.
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        try {
          Object.defineProperty(u, "0", { configurable: false, value: 1 });
          false
        } catch (e) {
          e instanceof TypeError
        }
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        try {
          Object.defineProperty(u, "0", { enumerable: false, value: 1 });
          false
        } catch (e) {
          e instanceof TypeError
        }
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        try {
          Object.defineProperty(u, "0", { writable: false, value: 1 });
          false
        } catch (e) {
          e instanceof TypeError
        }
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Invalid integer index.
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        try {
          Object.defineProperty(u, "99", { value: 1 });
          false
        } catch (e) {
          e instanceof TypeError
        }
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // Canonical numeric index string but invalid integer index.
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(2);
        try {
          Object.defineProperty(u, "-1", { value: 1 });
          false
        } catch (e) {
          e instanceof TypeError
        }
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn object_define_property_rejects_detached_uint8_array_buffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script("globalThis.u = new Uint8Array(2);")?;
  let buf = rt.exec_script("u.buffer")?;
  let Value::Object(buf_obj) = buf else {
    panic!("expected u.buffer to be an object");
  };

  // Detach the backing buffer via the host heap API.
  rt.heap_mut().detach_array_buffer(buf_obj)?;

  let value = rt.exec_script(
    r#"
      try {
        Object.defineProperty(u, "0", { value: 1 });
        false
      } catch (e) {
        e instanceof TypeError
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
