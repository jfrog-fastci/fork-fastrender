use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Keep heap limits small so this test exercises the full runtime + GC pipeline under tight
  // memory conditions.
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_script_true(rt: &mut JsRuntime, src: &str, ctx: &str) {
  match rt.exec_script(src) {
    Ok(Value::Bool(true)) => {}
    Ok(other) => panic!("{ctx}: expected true, got {other:?}"),
    Err(err) => panic!("{ctx}: exec_script failed: {err:?}"),
  }
}

#[test]
fn typed_array_and_dataview_handles_survive_explicit_gc_between_script_evaluations() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      globalThis.ab = new ArrayBuffer(8);
      globalThis.u = new Uint8Array(ab, 1, 4);
      globalThis.dv = new DataView(ab, 2, 3);
    "#,
  )?;

  // Ensure the globals resolve to the expected object kinds before testing GC stability.
  let ab = rt.exec_script("ab")?;
  let Value::Object(ab_obj) = ab else {
    panic!("expected ab to be an object, got {ab:?}");
  };
  assert!(rt.heap.is_array_buffer_object(ab_obj));

  let u = rt.exec_script("u")?;
  let Value::Object(u_obj) = u else {
    panic!("expected u to be an object, got {u:?}");
  };
  assert!(rt.heap.is_uint8_array_object(u_obj));

  let dv = rt.exec_script("dv")?;
  let Value::Object(dv_obj) = dv else {
    panic!("expected dv to be an object, got {dv:?}");
  };
  assert!(rt.heap.is_data_view_object(dv_obj));

  let check_attached_with_global_buffer = r#"
    ab.byteLength === 8 &&
    u.byteLength === 4 &&
    u.byteOffset === 1 &&
    u.buffer === ab &&
    dv.byteLength === 3 &&
    dv.byteOffset === 2 &&
    dv.buffer === ab
  "#;

  // Sanity-check before any GC.
  assert_script_true(
    &mut rt,
    check_attached_with_global_buffer,
    "attached (global buffer reference) before gc",
  );

  for _ in 0..5 {
    rt.heap.collect_garbage();
    assert_script_true(
      &mut rt,
      check_attached_with_global_buffer,
      "attached (global buffer reference) after gc",
    );
  }

  // Drop the direct global reference to the ArrayBuffer. The ArrayBuffer should remain live via the
  // TypedArray/DataView internal slots.
  rt.exec_script("globalThis.ab = undefined;")?;

  let check_attached_via_views = r#"
    u.byteLength === 4 &&
    u.byteOffset === 1 &&
    dv.byteLength === 3 &&
    dv.byteOffset === 2 &&
    u.buffer === dv.buffer &&
    u.buffer.byteLength === 8
  "#;

  assert_script_true(&mut rt, check_attached_via_views, "attached via views before gc");

  for _ in 0..5 {
    rt.heap.collect_garbage();
    assert_script_true(&mut rt, check_attached_via_views, "attached via views after gc");
  }

  // Detach the buffer via the host API and ensure accessors continue to work across explicit GC
  // between script evaluations (TypedArray accessors should report zeros; DataView accessors should
  // throw per spec).
  let ab = rt.exec_script("u.buffer")?;
  let Value::Object(ab_obj) = ab else {
    panic!("expected u.buffer to be an object, got {ab:?}");
  };
  let _ = rt.heap.detach_array_buffer_take_data(ab_obj)?;

  // Spec: TypedArray accessors become zero-length views over detached buffers, but DataView
  // `byteLength`/`byteOffset` accessors throw on detached buffers.
  let check_detached = r#"
    (() => {
      if (u.byteLength !== 0 || u.byteOffset !== 0) return false;
      if (u.buffer !== dv.buffer) return false;
      if (u.buffer.byteLength !== 0) return false;

      let byteLengthThrew = false;
      try { dv.byteLength; } catch (e) { byteLengthThrew = e.name === "TypeError"; }
      let byteOffsetThrew = false;
      try { dv.byteOffset; } catch (e) { byteOffsetThrew = e.name === "TypeError"; }

      return byteLengthThrew && byteOffsetThrew && dv.buffer === u.buffer;
    })()
  "#;

  assert_script_true(&mut rt, check_detached, "detached before gc");

  for _ in 0..5 {
    rt.heap.collect_garbage();
    assert_script_true(&mut rt, check_detached, "detached after gc");
  }

  Ok(())
}
