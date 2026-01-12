use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn typed_array_set_throws_typeerror_when_target_is_detached_after_offset_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      globalThis.ab_t = new ArrayBuffer(4);
      globalThis.t = new Uint8Array(ab_t);

      globalThis.ab_s = new ArrayBuffer(4);
      globalThis.s = new Uint8Array(ab_s);
    "#,
  )?;

  let ab_t = rt.exec_script("ab_t")?;
  let Value::Object(ab_t_obj) = ab_t else {
    panic!("expected ab_t to be an object, got {ab_t:?}");
  };
  rt.heap.detach_array_buffer(ab_t_obj)?;

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          let called = false;
          try {
            t.set(s, { valueOf() { called = true; return 0; } });
            return false;
          } catch (e) {
            return called === true && e.name === "TypeError";
          }
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn typed_array_set_throws_typeerror_when_source_is_detached_after_offset_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      globalThis.ab_t = new ArrayBuffer(4);
      globalThis.t = new Uint8Array(ab_t);

      globalThis.ab_s = new ArrayBuffer(4);
      globalThis.s = new Uint8Array(ab_s);
    "#,
  )?;

  let ab_s = rt.exec_script("ab_s")?;
  let Value::Object(ab_s_obj) = ab_s else {
    panic!("expected ab_s to be an object, got {ab_s:?}");
  };
  rt.heap.detach_array_buffer(ab_s_obj)?;

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          let called = false;
          try {
            t.set(s, { valueOf() { called = true; return 0; } });
            return false;
          } catch (e) {
            return called === true && e.name === "TypeError";
          }
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn typed_array_set_self_throws_typeerror_on_detached_buffer() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script("globalThis.ab = new ArrayBuffer(4); globalThis.u = new Uint8Array(ab);")?;

  let ab = rt.exec_script("ab")?;
  let Value::Object(ab_obj) = ab else {
    panic!("expected ab to be an object, got {ab:?}");
  };
  rt.heap.detach_array_buffer(ab_obj)?;

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          try {
            u.set(u);
            return false;
          } catch (e) {
            return e.name === "TypeError";
          }
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

