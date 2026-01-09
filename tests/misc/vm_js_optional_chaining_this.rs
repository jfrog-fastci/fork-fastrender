use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn optional_chain_call_preserves_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function f(){ return this === o; } var o = {}; o.f = f; o?.f() === true"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_call_on_property_preserves_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function f(){ return this === o; } var o = {}; o.f = f; o.f?.() === true"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_call_does_not_evaluate_arguments_when_short_circuited() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var called = false;
        function arg(){ called = true; return 1; }
        var o = null;
        (o?.f(arg()) === undefined) && (called === false)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_call_on_identifier_uses_function_call_this_binding_rules() {
  // `f?.()` is an optional call on an IdentifierReference, so it should behave like `f()`.
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"function f(){ return this === globalThis; } f?.()"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""use strict"; function f(){ return this === undefined; } f?.()"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

