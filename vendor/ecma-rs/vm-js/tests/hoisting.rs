use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn function_declarations_are_hoisted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script("f(); function f() { return 1; }")
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn var_declarations_are_hoisted_to_undefined() {
  let mut rt = new_runtime();

  let value = rt.exec_script("x === undefined; var x = 1;").unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script("x").unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn lexical_declarations_have_tdz() {
  let mut rt = new_runtime();
  let err = rt.exec_script("{ x; let x = 1; }").unwrap_err();
  match err {
    VmError::Throw(_) | VmError::ThrowWithStack { .. } => {}
    other => panic!("expected ReferenceError/TDZ throw, got {other:?}"),
  }
}

#[test]
fn const_without_initializer_is_syntax_error_and_does_not_pollute_global_env() {
  let mut rt = new_runtime();
  let err = rt.exec_script("const x;").unwrap_err();
  match err {
    VmError::Syntax(_) => {}
    other => panic!("expected syntax error, got {other:?}"),
  }

  // If hoisting ran before the early error, we would have left behind an
  // uninitialised lexical binding, and `typeof x` would throw instead of
  // returning `"undefined"`.
  let value = rt.exec_script(r#"typeof x === "undefined""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}
