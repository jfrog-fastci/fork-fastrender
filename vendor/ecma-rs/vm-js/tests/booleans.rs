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

  Ok(())
}

#[test]
fn boolean_prototype_symbol_to_primitive_is_installed_and_validates_hint() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        typeof Boolean.prototype[Symbol.toPrimitive] === "function" &&
        Object.getOwnPropertyDescriptor(Boolean.prototype, Symbol.toPrimitive).writable === false &&
        Object.getOwnPropertyDescriptor(Boolean.prototype, Symbol.toPrimitive).enumerable === false &&
        Object.getOwnPropertyDescriptor(Boolean.prototype, Symbol.toPrimitive).configurable === true
      "#,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
        const f = Boolean.prototype[Symbol.toPrimitive];
        f.call(new Boolean(true), "string") === true &&
        f.call(new Boolean(false), "number") === false &&
        f.call(new Boolean(true), "default") === true
      "#,
    )?,
    Value::Bool(true)
  );

  let s = rt.exec_script(r#"try { Boolean.prototype[Symbol.toPrimitive].call(true, "bad"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Boolean.prototype[Symbol.toPrimitive].call(true, 1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { Boolean.prototype[Symbol.toPrimitive].call("x", "default"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  Ok(())
}
