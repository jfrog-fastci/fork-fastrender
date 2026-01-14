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
fn boolean_prototype_to_string_and_value_of_work() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let s = rt.exec_script(r#"(true).toString() + "," + (false).toString()"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "true,false");

  assert_eq!(
    rt.exec_script(r#"Boolean.prototype.valueOf.call(new Boolean(true))"#)?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"Boolean.prototype.valueOf.call(new Boolean(false))"#)?,
    Value::Bool(false)
  );

  assert_eq!(rt.exec_script(r#"Boolean.prototype.valueOf()"#)?, Value::Bool(false));

  let s = rt.exec_script(r#"Boolean.prototype.toString()"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "false");

  let s = rt.exec_script(r#"Boolean.prototype.toString.call(new Boolean(false))"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "false");

  let s = rt.exec_script(r#"Object.prototype.toString.call(Boolean.prototype)"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "[object Boolean]");

  let s = rt.exec_script(r#"try { Boolean.prototype.toString.call("x"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Boolean.prototype.valueOf.call(new Proxy(new Boolean(true), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Boolean.prototype.toString.call(new Proxy(new Boolean(true), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  Ok(())
}

#[test]
fn boolean_prototype_symbol_to_primitive_is_undefined_and_uses_ordinary_to_primitive() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        // Per ES spec / Node.js: Boolean.prototype does *not* define @@toPrimitive; boolean wrapper
        // objects use OrdinaryToPrimitive (toString/valueOf).
        Boolean.prototype[Symbol.toPrimitive] === undefined &&
        Object.getOwnPropertyDescriptor(Boolean.prototype, Symbol.toPrimitive) === undefined
      "#,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
        // OrdinaryToPrimitive with string hint tries toString first.
        (function () {
          let calls = "";
          const b = new Boolean(true);
          b.toString = function () { calls += "s"; return "ok"; };
          b.valueOf = function () { calls += "v"; return true; };
          return String(b) === "ok" && calls === "s";
        })()
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
          const b = new Boolean(false);
          b.valueOf = function () { calls += "v"; return true; };
          b.toString = function () { calls += "s"; return "0"; };
          return Number(b) === 1 && calls === "v";
        })()
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}
