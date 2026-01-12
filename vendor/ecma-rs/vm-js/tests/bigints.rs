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
fn bigint_constructor_and_statics_work() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        typeof BigInt === "function" &&
        BigInt(10) === 10n &&
        BigInt(true) === 1n &&
        BigInt(false) === 0n &&
        BigInt("123") === 123n &&
        BigInt("-123") === -123n &&
        BigInt(" 0x10 ") === 16n &&
        BigInt.asUintN(8, 0x1ffn) === 0xffn &&
        BigInt.asIntN(8, 0xffn) === -1n &&
        BigInt.asIntN(8, 0x80n) === -128n &&
        BigInt.asUintN(0, 123n) === 0n &&
        BigInt.asIntN(0, 123n) === 0n
      "#,
    )?,
    Value::Bool(true)
  );

  let s = rt.exec_script(r#"try { BigInt(null); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  let s = rt.exec_script(r#"try { BigInt(1.5); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "RangeError");

  let s = rt.exec_script(r#"try { BigInt("nope"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "SyntaxError");

  Ok(())
}

#[test]
fn bigint_prototype_methods_work() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let s = rt.exec_script(r#"(255n).toString(16) + "," + (-10n).toString(16) + "," + (0n).toString(2)"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "ff,-a,0");

  // `toLocaleString` is present (minimal placeholder).
  assert_eq!(
    rt.exec_script(r#"typeof (1n).toLocaleString === "function" && (1n).toLocaleString() === "1""#)?,
    Value::Bool(true)
  );

  // Wrapper objects and receiver checks.
  assert_eq!(
    rt.exec_script(r#"Object(1n).valueOf() === 1n && BigInt.prototype.valueOf.call(1n) === 1n"#)?,
    Value::Bool(true)
  );

  let s = rt.exec_script(r#"try { (1n).toString(1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "RangeError");

  let s = rt.exec_script(r#"try { BigInt.prototype.toString.call("x"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { BigInt.prototype.valueOf.call(new Proxy(Object(1n), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { BigInt.prototype.toString.call(new Proxy(Object(1n), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  Ok(())
}
