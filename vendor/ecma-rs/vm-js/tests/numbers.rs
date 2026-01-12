use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn as_utf8_lossy(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn number_constants_and_statics_exist() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Constants.
  assert_eq!(
    rt.exec_script(
      r#"
        Number.MAX_SAFE_INTEGER === 9007199254740991 &&
        Number.MIN_SAFE_INTEGER === -9007199254740991 &&
        Number.EPSILON > 0 &&
        (1 + Number.EPSILON) !== 1 &&
        (1 + Number.EPSILON / 2) === 1 &&
        Number.MIN_VALUE === 5e-324
      "#,
    )?,
    Value::Bool(true)
  );

  // Static predicates.
  assert_eq!(
    rt.exec_script(
      r#"
        Number.isNaN(NaN) === true &&
        Number.isNaN("NaN") === false &&
        Number.isFinite(0) === true &&
        Number.isFinite(Infinity) === false &&
        Number.isFinite("0") === false &&
        Number.isInteger(1) === true &&
        Number.isInteger(1.1) === false &&
        Number.isSafeInteger(9007199254740991) === true &&
        Number.isSafeInteger(9007199254740992) === false
      "#,
    )?,
    Value::Bool(true)
  );

  // `parseInt` / `parseFloat` aliases.
  assert_eq!(
    rt.exec_script(r#"Number.parseInt === parseInt && Number.parseFloat === parseFloat"#)?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn number_prototype_formatting_methods_work() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // toString(radix)
  let s = rt.exec_script(r#"(15).toString(16) + "," + (10).toString(2) + "," + (1.5).toString(2)"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "f,1010,1.1");

  // -0 handling.
  let s = rt.exec_script(r#"(-0).toString(2) + "," + (-0).toExponential() + "," + (-0).toFixed(2)"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "0,0e+0,0.00");

  // toFixed / toExponential / toPrecision basic cases.
  let s = rt.exec_script(
    r#"
      (1.2345).toFixed(2) + "," +
      (1).toExponential(2) + "," +
      (77).toExponential() + "," +
      (123.45).toPrecision(4) + "," +
      (9.99).toPrecision(1)
    "#,
  )?;
  assert_eq!(as_utf8_lossy(&rt, s), "1.23,1.00e+0,7.7e+1,123.5,1e+1");

  // toLocaleString is present (minimal placeholder).
  assert_eq!(
    rt.exec_script(r#"typeof (1).toLocaleString === "function" && (1).toLocaleString() === "1""#)?,
    Value::Bool(true)
  );

  assert_eq!(rt.exec_script(r#"Number.prototype.valueOf()"#)?, Value::Number(0.0));

  let s = rt.exec_script(r#"Number.prototype.toString()"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "0");

  let s = rt.exec_script(r#"Object.prototype.toString.call(Number.prototype)"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "[object Number]");

  // Range errors / Type errors.
  let s = rt.exec_script(r#"try { (1).toString(1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "RangeError");
  let s = rt.exec_script(r#"try { (1).toFixed(101); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "RangeError");
  let s = rt.exec_script(r#"try { Number.prototype.toString.call("x"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Number.prototype.valueOf.call(new Proxy(new Number(1), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Number.prototype.toString.call(new Proxy(new Number(1), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  Ok(())
}

#[test]
fn number_prototype_symbol_to_primitive_is_installed_and_validates_hint() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        typeof Number.prototype[Symbol.toPrimitive] === "function" &&
        Object.getOwnPropertyDescriptor(Number.prototype, Symbol.toPrimitive).writable === false &&
        Object.getOwnPropertyDescriptor(Number.prototype, Symbol.toPrimitive).enumerable === false &&
        Object.getOwnPropertyDescriptor(Number.prototype, Symbol.toPrimitive).configurable === true
      "#,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
        const f = Number.prototype[Symbol.toPrimitive];
        f.call(new Number(1), "string") === "1" &&
        f.call(new Number(1), "number") === 1 &&
        f.call(new Number(1), "default") === 1
      "#,
    )?,
    Value::Bool(true)
  );

  let s = rt.exec_script(r#"try { Number.prototype[Symbol.toPrimitive].call(1, "bad"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Number.prototype[Symbol.toPrimitive].call(1, 1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Number.prototype[Symbol.toPrimitive].call("x", "number"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(
    r#"try { Number.prototype[Symbol.toPrimitive].call(new Proxy(new Number(1), {}), "number"); } catch (e) { e.name }"#,
  )?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  Ok(())
}
