use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn bigint_typed_arrays_basic_construction_and_element_access() {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"typeof BigInt64Array === "function""#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"typeof BigUint64Array === "function""#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"BigInt64Array.BYTES_PER_ELEMENT === 8"#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"BigUint64Array.prototype.BYTES_PER_ELEMENT === 8"#)
      .unwrap(),
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(r#"new BigInt64Array([1n, 2n]).length === 2"#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new BigUint64Array([1n, 2n]).length === 2"#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new BigInt64Array([1n, 2n])[0] === 1n"#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new BigInt64Array([1n, 2n])[1] === 2n"#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new BigUint64Array([1n, 2n])[0] === 1n"#)
      .unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new BigUint64Array([1n, 2n])[1] === 2n"#)
      .unwrap(),
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
      (() => {
        const a = new BigInt64Array([1n]);
        a[0] = -1n;
        return a[0] === -1n;
      })()
    "#,
    )
    .unwrap(),
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
      (() => {
        const a = new BigInt64Array([1n]);
        a[0] = -1n;
        const desc = Object.getOwnPropertyDescriptor(a, "0");
        return !!desc && desc.value === -1n;
      })()
    "#,
    )
    .unwrap(),
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
      (() => {
        const a = new BigInt64Array([1n]);
        let threw = false;
        try {
          a[0] = 1;
        } catch (e) {
          threw = e instanceof TypeError;
        }
        return threw;
      })()
    "#,
    )
    .unwrap(),
    Value::Bool(true)
  );
}
