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
fn string_prototype_value_of_and_symbol_to_primitive_work() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        typeof String.prototype.valueOf === "function" &&
        String.prototype.valueOf.call("x") === "x" &&
        String.prototype.valueOf.call(Object("x")) === "x"
      "#,
    )?,
    Value::Bool(true)
  );

  let s = rt.exec_script(r#"try { String.prototype.valueOf.call(1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  assert_eq!(
    rt.exec_script(
      r#"
        typeof String.prototype[Symbol.toPrimitive] === "function" &&
        Object.getOwnPropertyDescriptor(String.prototype, Symbol.toPrimitive).writable === false &&
        Object.getOwnPropertyDescriptor(String.prototype, Symbol.toPrimitive).enumerable === false &&
        Object.getOwnPropertyDescriptor(String.prototype, Symbol.toPrimitive).configurable === true
      "#,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(
      r#"
        const f = String.prototype[Symbol.toPrimitive];
        f.call(Object("x"), "string") === "x" &&
        f.call(Object("x"), "number") === "x" &&
        f.call(Object("x"), "default") === "x"
      "#,
    )?,
    Value::Bool(true)
  );

  let s = rt.exec_script(r#"try { String.prototype[Symbol.toPrimitive].call("x", "bad"); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { String.prototype[Symbol.toPrimitive].call("x", 1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  // Ensure `String.prototype.toString` throws a TypeError on incompatible receivers (instead of
  // surfacing as an internal `Unimplemented` error).
  let s = rt.exec_script(r#"try { String.prototype.toString.call(1); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  // Proxy wrapper objects must not be treated as String wrapper objects (internal slots do not
  // exist on the Proxy itself).
  let s = rt.exec_script(r#"try { String.prototype.valueOf.call(new Proxy(Object("x"), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(r#"try { String.prototype.toString.call(new Proxy(Object("x"), {})); } catch (e) { e.name }"#)?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");
  let s = rt.exec_script(
    r#"try { String.prototype[Symbol.toPrimitive].call(new Proxy(Object("x"), {}), "string"); } catch (e) { e.name }"#,
  )?;
  assert_eq!(as_utf8_lossy(&rt, s), "TypeError");

  Ok(())
}
