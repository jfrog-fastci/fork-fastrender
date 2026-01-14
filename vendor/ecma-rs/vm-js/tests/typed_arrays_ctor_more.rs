use vm_js::{Budget, Heap, HeapLimits, JsRuntime, TerminationReason, Value, Vm, VmError, VmOptions};

fn new_runtime(limits: HeapLimits) -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(limits);
  JsRuntime::new(vm, heap)
}

#[test]
fn typed_array_constructors_accept_array_like_iterable_and_typed_array_sources() -> Result<(), VmError> {
  let mut rt = new_runtime(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024))?;
  let value = rt.exec_script(
    r#"
      // Iterable (Array uses @@iterator).
      var a = new Uint8Array([1, 2, 3]);
      var ok_array = a.length === 3 && a[0] === 1 && a[1] === 2 && a[2] === 3;

      // Array-like (no @@iterator): LengthOfArrayLike + Get.
      var b = new Uint8Array({ length: 2, 0: "7", 1: 8 });
      var ok_array_like = b.length === 2 && b[0] === 7 && b[1] === 8;

      // String primitive is treated as a length (TypedArray ( ...args ) step 1).
      var c1 = new Uint8Array("12");
      var ok_string_primitive = c1.length === 12 && c1[0] === 0 && c1[11] === 0;

      // Iterable (string iterator) via a String wrapper object.
      var c = new Uint8Array(Object("12"));
      var ok_string_object = c.length === 2 && c[0] === 1 && c[1] === 2;

      // TypedArray source copies into a new buffer.
      var src = new Uint8Array([9, 10]);
      var dst = new Uint8Array(src);
      dst[0] = 1;
      var ok_typed_array_copy =
        src[0] === 9 && dst[0] === 1 && dst.length === 2 && dst.buffer !== src.buffer;

      // Cross-kind typed array conversion.
      var s16 = new Int16Array(2);
      s16[0] = -1;
      s16[1] = 256;
      var d8 = new Uint8Array(s16);
      var ok_cross_kind = d8.length === 2 && d8[0] === 255 && d8[1] === 0;

      // Non-Uint8 constructors share the same typed array constructor logic.
      var i16 = new Int16Array([1, 2]);
      var ok_i16 = i16.length === 2 && i16[0] === 1 && i16[1] === 2;

      var f32 = new Float32Array([1.5]);
      var ok_f32 = f32.length === 1 && f32[0] === 1.5;

      ok_array && ok_array_like && ok_string_primitive && ok_string_object && ok_typed_array_copy && ok_cross_kind && ok_i16 && ok_f32
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_constructor_from_array_like_consumes_fuel_in_native_loop() {
  let mut rt = new_runtime(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024)).unwrap();
  rt.vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });

  // This is intentionally a single JS statement so budget exhaustion is driven primarily by the
  // internal Rust loop (rather than by statement/expression ticks).
  let err = rt.exec_script("new Uint8Array({ length: 1000000 })").unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected Termination(OutOfFuel), got {other:?}"),
  }
}
