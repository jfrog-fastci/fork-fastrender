use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn test_unimplemented(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::Unimplemented("test unimplemented"))
}

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
fn for_triple_let_closure_capture() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var set = [];
      for (let i = 0; i < 3; i++) { set[i] = () => i; }
      "" + set[0]() + set[1]() + set[2]()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "012");
}

#[test]
fn for_triple_restores_lexical_env_on_uncatchable_error() {
  let mut rt = new_runtime();

  rt.register_global_native_function("__test_unimplemented", test_unimplemented, 0)
    .unwrap();
  let err = rt
    // Trigger an abrupt error inside the loop body so we can assert the loop restores its
    // lexical environment before unwinding.
    .exec_script(r#"for (let i = 0; i < 1; i++) { __test_unimplemented(); }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));

  // If the loop's lexical environment is not restored when the body returns an uncatchable error,
  // the loop variable binding would leak into subsequent script executions.
  let value = rt
    .exec_script(r#"try { i; "leaked" } catch(e) { "ok" }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "ok");
}

#[test]
fn for_of_let_default_initializer_has_tdz() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var a = 99;
        var threw = false;
        try { for (let [a = a] of [[undefined]]) {} } catch (e) { threw = e && e.name === "ReferenceError"; }
        threw
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_in_let_default_initializer_has_tdz() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var a = 99;
        var threw = false;
        try { for (let [a = a] in {"": 0}) {} } catch (e) { threw = e && e.name === "ReferenceError"; }
        threw
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn async_for_of_let_default_initializer_has_tdz() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var out = "pending";
      async function f() {
        var a = 99;
        for (let [a = a] of [[undefined]]) {}
      }
      f().then(() => out = "resolved", e => out = e && e.name);
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "pending");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn async_for_in_let_default_initializer_has_tdz() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var out = "pending";
      async function f() {
        var a = 99;
        for (let [a = a] in {"": 0}) {}
      }
      f().then(() => out = "resolved", e => out = e && e.name);
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "pending");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn async_for_await_of_let_default_initializer_has_tdz() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var out = "pending";
      async function f() {
        var a = 99;
        for await (let [a = a] of [[undefined]]) {}
      }
      f().then(() => out = "resolved", e => out = e && e.name);
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "pending");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}
