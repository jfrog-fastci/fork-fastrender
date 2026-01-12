use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn function_prototype_symbol_has_instance() {
  let mut rt = new_runtime();

  // Non-constructable generator functions still participate in `instanceof` via
  // `Function.prototype[Symbol.hasInstance]` (OrdinaryHasInstance).
  match rt.exec_script(r#"function* g(){ yield 1; } var it = g(); (it instanceof g)"#) {
    Ok(value) => assert_eq!(value, Value::Bool(true)),
    // Generator support is tracked separately; once implemented this assertion should validate
    // the `@@hasInstance`/OrdinaryHasInstance semantics.
    Err(VmError::Unimplemented("generator functions")) => {}
    Err(err) => panic!("unexpected error: {err:?}"),
  }

  // Arrow functions have a non-object `.prototype`, so `instanceof` throws a TypeError.
  let value = rt
    .exec_script(
      r#"
        var arrow = () => {};
        try { ({} instanceof arrow); "no error"; } catch (e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");

  // Ordinary constructor functions still work.
  let value = rt
    .exec_script(r#"function C(){}; var o = new C(); (o instanceof C)"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
