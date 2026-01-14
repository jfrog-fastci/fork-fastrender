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

  // toFixed / toExponential / toPrecision basic + regression cases.
  let s = rt.exec_script(
    r#"
      (1.2345).toFixed(2) + "," +
      (1.25).toFixed(1) + "," +
      (1).toExponential(2) + "," +
      (1.25).toExponential(1) + "," +
      (77).toExponential() + "," +
      (123.45).toPrecision(4) + "," +
      (1.25).toPrecision(2) + "," +
      (9.99).toPrecision(1) + "," +
      // `toPrecision` threshold: fixed for exp == p-1, exponential for exp == p.
      (999).toPrecision(3) + "," +
      (1000).toPrecision(3) + "," +
      // `toPrecision` threshold: fixed for exp == -6, exponential for exp < -6.
      (0.00000123).toPrecision(2) + "," +
      (0.000000123).toPrecision(2)
    "#,
  )?;
  assert_eq!(
    as_utf8_lossy(&rt, s),
    "1.23,1.3,1.00e+0,1.3e+0,7.7e+1,123.5,1.3,1e+1,999,1.00e+3,0.0000012,1.2e-7"
  );

  // Max length / range edge cases (fractionDigits/precision upper bound is 100).
  let s = rt.exec_script(r#"(-0).toFixed(100)"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), format!("0.{}", "0".repeat(100)));

  let s = rt.exec_script(r#"(1e20).toFixed(100)"#)?;
  let out = as_utf8_lossy(&rt, s);
  assert!(!out.contains('e'), "toFixed must not use exponential notation");
  let Some((_int, frac)) = out.split_once('.') else {
    panic!("expected fixed notation, got {out:?}");
  };
  assert_eq!(frac.len(), 100);
  assert!(frac.bytes().all(|b| b'0' <= b && b <= b'9'));

  let s = rt.exec_script(r#"(1.23).toExponential(100)"#)?;
  let out = as_utf8_lossy(&rt, s);
  assert!(out.ends_with("e+0"));
  let Some((mantissa, exp)) = out.split_once('e') else {
    panic!("expected exponential form, got {out:?}");
  };
  assert_eq!(exp, "+0");
  let Some((leading, frac)) = mantissa.split_once('.') else {
    panic!("expected decimal point in mantissa, got {mantissa:?}");
  };
  assert_eq!(leading, "1");
  assert_eq!(frac.len(), 100);
  assert!(frac.bytes().all(|b| b'0' <= b && b <= b'9'));

  let s = rt.exec_script(r#"(1.23).toPrecision(100)"#)?;
  let out = as_utf8_lossy(&rt, s);
  assert!(!out.contains('e'), "expected fixed notation, got {out:?}");
  let Some((leading, frac)) = out.split_once('.') else {
    panic!("expected fixed decimal point, got {out:?}");
  };
  assert_eq!(leading, "1");
  assert_eq!(frac.len(), 99);
  assert!(frac.bytes().all(|b| b'0' <= b && b <= b'9'));

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
fn number_prototype_symbol_to_primitive_is_undefined_and_uses_ordinary_to_primitive() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        // Per ES spec / Node.js: Number.prototype does *not* define @@toPrimitive; number wrapper
        // objects use OrdinaryToPrimitive (valueOf/toString).
        Number.prototype[Symbol.toPrimitive] === undefined &&
        Object.getOwnPropertyDescriptor(Number.prototype, Symbol.toPrimitive) === undefined
      "#,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
        // OrdinaryToPrimitive with number hint tries valueOf first.
        (function () {
          let calls = "";
          const n = new Number(1);
          n.valueOf = function () { calls += "v"; return 2; };
          n.toString = function () { calls += "s"; return "3"; };
          return Number(n) === 2 && calls === "v";
        })()
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}
