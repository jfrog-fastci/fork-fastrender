use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime(limits: HeapLimits) -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(limits);
  JsRuntime::new(vm, heap)
}

#[test]
fn typed_array_indexing_views_and_methods_work() -> Result<(), VmError> {
  let mut rt = new_runtime(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let value = rt.exec_script(
    r#"
      var a = new Uint8Array(4);
      a[0] = 1;
      a[1] = 2;
      a[2] = 3;
      a[3] = 4;

      // OOB writes are ignored.
      a[10] = 9;

      var b = a.subarray(1, 3);
      var ok_subarray =
        b.length === 2 &&
        b.byteOffset === 1 &&
        b.byteLength === 2 &&
        b.buffer === a.buffer &&
        b[0] === 2 &&
        b[1] === 3;

      // Subarrays share the buffer.
      b[0] = 99;
      var ok_shared = a[1] === 99;

      // slice copies into a new buffer.
      var c = a.slice(1, 3);
      c[0] = 7;
      var ok_slice =
        c.length === 2 &&
        c.byteOffset === 0 &&
        c.byteLength === 2 &&
        c.buffer !== a.buffer &&
        a[1] === 99 &&
        c[0] === 7;

      // set copies values from another typed array.
      var dst = new Uint8Array(4);
      var src = new Uint8Array(2);
      src[0] = 5;
      src[1] = 6;
      dst.set(src, 1);
      var ok_set = dst[1] === 5 && dst[2] === 6;

      // set behaves correctly with overlapping ranges.
      var ov = new Uint8Array(4);
      ov[0] = 1;
      ov[1] = 2;
      ov[2] = 3;
      ov[3] = 4;
      ov.set(ov.subarray(0, 3), 1);
      var ok_overlap = ov[0] === 1 && ov[1] === 1 && ov[2] === 2 && ov[3] === 3;

      ok_subarray && ok_shared && ok_slice && ok_set && ok_overlap && (a[10] === undefined)
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_types_and_conversions_work() -> Result<(), VmError> {
  let mut rt = new_runtime(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let value = rt.exec_script(
    r#"
      var i8 = new Int8Array(1);
      i8[0] = 255;

      var u8c = new Uint8ClampedArray(2);
      u8c[0] = -20;
      u8c[1] = 1.5;

      var i16 = new Int16Array(1);
      i16[0] = 65535;

      var f32 = new Float32Array(1);
      f32[0] = 1.5;

      // Int8 wraps, Int16 wraps, Uint8Clamped clamps/rounds to even, floats roundtrip.
      i8[0] === -1 && i16[0] === -1 && u8c[0] === 0 && u8c[1] === 2 && f32[0] === 1.5
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_constructor_from_buffer_and_alignment_checks_work() -> Result<(), VmError> {
  let mut rt = new_runtime(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let value = rt.exec_script(
    r#"
      var buf = new ArrayBuffer(8);
      var a = new Uint8Array(buf, 2, 2);
      a[0] = 7;
      var b = new Uint8Array(buf);
      var ok_share = b[2] === 7 && a.byteOffset === 2 && a.length === 2 && a.byteLength === 2;

      // Alignment: Uint16Array byteOffset must be a multiple of 2.
      var err = (function () {
        try {
          new Uint16Array(new ArrayBuffer(4), 1);
          return "no error";
        } catch (e) {
          return e.name;
        }
      })();

      ok_share && err === "TypeError"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn data_view_get_set_and_is_view_work() -> Result<(), VmError> {
  let mut rt = new_runtime(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let value = rt.exec_script(
    r#"
      var buf = new ArrayBuffer(8);
      var dv = new DataView(buf);

      // Little endian.
      dv.setUint32(0, 0x01020304, true);
      var ok_le = dv.getUint8(0) === 4 && dv.getUint32(0, true) === 0x01020304;

      // Big endian (default when littleEndian is false/undefined).
      dv.setUint16(4, 0x0a0b);
      var ok_be = dv.getUint8(4) === 0x0a && dv.getUint8(5) === 0x0b;

      var dv2 = new DataView(buf, 2, 4);
      var ok_view = dv2.byteOffset === 2 && dv2.byteLength === 4 && dv2.buffer === buf;

      var ok_is_view =
        ArrayBuffer.isView(new Uint8Array(1)) === true &&
        ArrayBuffer.isView(dv) === true &&
        ArrayBuffer.isView({}) === false;

      // `DataView` uses `ToIndex` for the byteOffset argument.
      var ok_get_default_index = dv.getUint8() === 4; // undefined -> 0
      var ok_get_fractional_index = dv.getUint8(1.9) === 3; // 1.9 -> 1

      var ok_get_oob_range_error = false;
      try { dv.getUint8(8); } catch (e) { ok_get_oob_range_error = e.name === "RangeError"; }

      var ok_get_negative_range_error = false;
      try { dv.getUint8(-1); } catch (e) { ok_get_negative_range_error = e.name === "RangeError"; }

      // `ToIndex` also applies to setters.
      dv.setUint8(undefined, 9);
      dv.setUint8(1.9, 7);
      var ok_set_to_index = dv.getUint8(0) === 9 && dv.getUint8(1) === 7;

      ok_le && ok_be && ok_view && ok_is_view &&
        ok_get_default_index && ok_get_fractional_index &&
        ok_get_oob_range_error && ok_get_negative_range_error &&
        ok_set_to_index
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_allocation_hits_oom_under_small_heap_limits() {
  let mut rt = new_runtime(HeapLimits::new(1024 * 1024, 1024 * 1024)).unwrap();
  match rt.exec_script("new Uint8Array(2 * 1024 * 1024)") {
    Err(VmError::OutOfMemory) => {}
    Ok(v) => panic!("expected OutOfMemory, got Ok({v:?})"),
    Err(e) => panic!("expected OutOfMemory, got Err({e:?})"),
  }
}
