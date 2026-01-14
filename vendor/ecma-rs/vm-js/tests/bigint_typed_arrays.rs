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

  assert_eq!(
    rt.exec_script(
      r#"
      (() => {
        // ToBigInt performs ToPrimitive(hint Number) on objects.
        const a = new BigInt64Array([0n]);
        const calls = [];
        const obj = {
          valueOf() { calls.push("valueOf"); return {}; },
          toString() { calls.push("toString"); return "42"; },
        };
        a[0] = obj;
        const ok_to_primitive = a[0] === 42n && calls.join(",") === "valueOf,toString";

        // If ToPrimitive produces a Number, ToBigInt must throw TypeError (and must not consult
        // toString after valueOf succeeds).
        const b = new BigInt64Array([0n]);
        const obj2 = {
          valueOf() { return 1; },
          toString() { throw new Error("toString called"); },
        };
        let ok_type_error = false;
        try {
          b[0] = obj2;
        } catch (e) {
          ok_type_error = e instanceof TypeError;
        }

        // Even for invalid numeric indices, TypedArraySetElement must perform ToBigInt.
        const c = new BigInt64Array([0n]);
        let ok_invalid_index = false;
        try {
          c[-1] = 1;
        } catch (e) {
          ok_invalid_index = e instanceof TypeError;
        }

        return ok_to_primitive && ok_type_error && ok_invalid_index;
      })()
    "#,
    )
    .unwrap(),
    Value::Bool(true)
  );
}
