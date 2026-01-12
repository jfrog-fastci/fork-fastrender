use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_limits(limits: HeapLimits) -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(limits);
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime() -> JsRuntime {
  new_runtime_with_limits(HeapLimits::new(1024 * 1024, 1024 * 1024))
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn function_prototype_caller_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      try { Function.prototype.caller; "no"; }
      catch (e) { e.constructor.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn function_prototype_caller_roundtrips_through_descriptor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var d = Object.getOwnPropertyDescriptor(Function.prototype, "caller");
      delete Function.prototype.caller;
      Object.defineProperty(Function.prototype, "caller", d);

      try { Function.prototype.caller; "no"; }
      catch (e) { e.constructor.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn function_prototype_caller_still_throws_after_test262_harness_load() -> Result<(), VmError> {
  // The test262 `wellKnownIntrinsicObjects.js` harness allocates a fairly large array of
  // objects/strings; use a larger heap than the default 1MiB unit-test limit so this doesn't OOM.
  let mut rt = new_runtime_with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

  let harness = format!(
    "{}\n{}\n{}\n{}\n",
    include_str!("../../test262-semantic/data/harness/assert.js"),
    include_str!("../../test262-semantic/data/harness/sta.js"),
    include_str!("../../test262-semantic/data/harness/propertyHelper.js"),
    include_str!("../../test262-semantic/data/harness/wellKnownIntrinsicObjects.js"),
  );

  let script = format!(
    r#"{harness}
      try {{ Function.prototype.caller; "no"; }}
      catch (e) {{ e.constructor.name; }}
    "#
  );

  let value = rt.exec_script(&script)?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn named_function_expression_binds_name_in_own_scope() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      try { (function fn() { return typeof fn; })(); }
      catch (e) { e.constructor.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "function");
  Ok(())
}

#[test]
fn strict_arguments_caller_and_callee_share_thrower() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function() {
        "use strict";
        var callee = Object.getOwnPropertyDescriptor(arguments, "callee").get;
        var caller = Object.getOwnPropertyDescriptor(arguments, "caller").get;
        return callee === caller ? "yes" : "no";
      })()
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "yes");
  Ok(())
}

#[test]
fn non_strict_function_caller_get_does_not_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function f() {}
      try { f.caller === null ? "yes" : "no"; }
      catch (e) { "throw"; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "yes");
  Ok(())
}

#[test]
fn non_strict_function_caller_set_does_not_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function f() {}
      try { f.caller = 1; "ok"; }
      catch (e) { e.constructor.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ok");
  Ok(())
}

#[test]
fn generator_function_caller_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* gen() {}
      try { gen.caller; "no"; }
      catch (e) { e.constructor.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}
