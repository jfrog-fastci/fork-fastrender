use vm_js::{Heap, HeapLimits, JsRuntime, TypedArrayKind, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn heap_introspection_is_prototype_independent() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
    globalThis.ab = new ArrayBuffer(256);
    globalThis.i8 = new Int8Array(ab, 1, 3);
    globalThis.u8 = new Uint8Array(ab, 4, 4);
    globalThis.u8c = new Uint8ClampedArray(ab, 8, 5);
    globalThis.i16 = new Int16Array(ab, 16, 6);
    globalThis.u16 = new Uint16Array(ab, 28, 7);
    globalThis.i32 = new Int32Array(ab, 44, 8);
    globalThis.u32 = new Uint32Array(ab, 80, 9);
    globalThis.f32 = new Float32Array(ab, 120, 10);
    globalThis.f64 = new Float64Array(ab, 168, 11);
    globalThis.dv = new DataView(ab, 7, 13);
    globalThis.rx = /a+b/gi;

    Object.setPrototypeOf(i8, {});
    Object.setPrototypeOf(u8, {});
    Object.setPrototypeOf(u8c, {});
    Object.setPrototypeOf(i16, {});
    Object.setPrototypeOf(u16, {});
    Object.setPrototypeOf(i32, {});
    Object.setPrototypeOf(u32, {});
    Object.setPrototypeOf(f32, {});
    Object.setPrototypeOf(f64, {});
    Object.setPrototypeOf(dv, {});
    Object.setPrototypeOf(rx, {});
    "#,
  )?;

  let Value::Object(ab) = rt.exec_script("ab")? else {
    panic!("expected ArrayBuffer object");
  };

  let cases = [
    ("i8", TypedArrayKind::Int8, 1usize, 3usize),
    ("u8", TypedArrayKind::Uint8, 4, 4),
    ("u8c", TypedArrayKind::Uint8Clamped, 8, 5),
    ("i16", TypedArrayKind::Int16, 16, 6),
    ("u16", TypedArrayKind::Uint16, 28, 7),
    ("i32", TypedArrayKind::Int32, 44, 8),
    ("u32", TypedArrayKind::Uint32, 80, 9),
    ("f32", TypedArrayKind::Float32, 120, 10),
    ("f64", TypedArrayKind::Float64, 168, 11),
  ];

  for (name, kind, byte_offset, length) in cases {
    let Value::Object(obj) = rt.exec_script(name)? else {
      panic!("expected {name} to be an object");
    };
    assert_eq!(rt.heap.typed_array_kind(obj)?, kind);
    assert_eq!(rt.heap.typed_array_byte_offset(obj)?, byte_offset);
    assert_eq!(rt.heap.typed_array_length(obj)?, length);
    assert_eq!(
      rt.heap.typed_array_byte_length(obj)?,
      length * kind.bytes_per_element()
    );
    assert_eq!(rt.heap.typed_array_buffer(obj)?, ab);
  }

  let Value::Object(dv) = rt.exec_script("dv")? else {
    panic!("expected dv to be an object");
  };
  assert_eq!(rt.heap.data_view_byte_offset(dv)?, 7);
  assert_eq!(rt.heap.data_view_byte_length(dv)?, 13);
  assert_eq!(rt.heap.data_view_buffer(dv)?, ab);

  let Value::Object(rx) = rt.exec_script("rx")? else {
    panic!("expected rx to be an object");
  };
  let source = rt.heap.regexp_original_source(rx)?;
  assert_eq!(rt.heap.get_string(source)?.to_utf8_lossy(), "a+b");
  let flags = rt.heap.regexp_original_flags(rx)?;
  assert_eq!(rt.heap.get_string(flags)?.to_utf8_lossy(), "gi");

  Ok(())
}

#[test]
fn heap_introspection_incompatible_receivers_throw_typeerror() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script("globalThis.o = {};")?;
  let Value::Object(o) = rt.exec_script("o")? else {
    panic!("expected o to be an object");
  };

  assert!(matches!(rt.heap.typed_array_length(o).unwrap_err(), VmError::TypeError(_)));
  assert!(matches!(rt.heap.data_view_byte_length(o).unwrap_err(), VmError::TypeError(_)));
  assert!(matches!(rt.heap.regexp_original_source(o).unwrap_err(), VmError::TypeError(_)));
  Ok(())
}

#[test]
fn heap_introspection_reports_zero_for_detached_buffers() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
    globalThis.ab = new ArrayBuffer(8);
    globalThis.u = new Uint8Array(ab, 1, 2);
    globalThis.dv = new DataView(ab, 1, 2);
    "#,
  )?;

  let Value::Object(ab) = rt.exec_script("ab")? else {
    panic!("expected ab to be an object");
  };
  let Value::Object(u) = rt.exec_script("u")? else {
    panic!("expected u to be an object");
  };
  let Value::Object(dv) = rt.exec_script("dv")? else {
    panic!("expected dv to be an object");
  };

  let _ = rt.heap.detach_array_buffer_take_data(ab)?;

  assert_eq!(rt.heap.typed_array_length(u)?, 0);
  assert_eq!(rt.heap.typed_array_byte_length(u)?, 0);
  assert_eq!(rt.heap.typed_array_byte_offset(u)?, 0);
  assert_eq!(rt.heap.typed_array_buffer(u)?, ab);

  assert_eq!(rt.heap.data_view_byte_length(dv)?, 0);
  assert_eq!(rt.heap.data_view_byte_offset(dv)?, 0);
  assert_eq!(rt.heap.data_view_buffer(dv)?, ab);

  Ok(())
}

