use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn value_to_number(value: Value) -> f64 {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  n
}

fn value_to_bool(value: Value) -> bool {
  let Value::Bool(b) = value else {
    panic!("expected bool, got {value:?}");
  };
  b
}

#[test]
fn async_default_param_tdz_rejects_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f(a = b, b) {}
      f().then(function () { out = "ok"; }, function (e) { out = e.name; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ReferenceError");
  Ok(())
}

#[test]
fn async_default_param_that_throws_rejects_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f(a = (function () { throw new Error("boom"); })()) {}
      f().then(function () { out = "ok"; }, function (e) { out = e.message; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");
  Ok(())
}

#[test]
fn async_arguments_object_is_mapped_for_sloppy_simple_params() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(a) { a = 2; return arguments[0]; }
      f(1).then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_number(value), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_number(value), 2.0);
  Ok(())
}

#[test]
fn async_arguments_object_write_aliases_params_in_sloppy_simple_params() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(a) { arguments[0] = 3; return a; }
      f(1).then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_number(value), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_number(value), 3.0);
  Ok(())
}

#[test]
fn async_arguments_object_is_unmapped_for_strict_mode() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(a) { "use strict"; a = 2; return arguments[0]; }
      f(1).then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_number(value), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_number(value), 1.0);
  Ok(())
}

#[test]
fn async_arguments_object_is_unmapped_for_non_simple_params() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(a = 0) { a = 2; return arguments[0]; }
      f(1).then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_number(value), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_number(value), 1.0);
  Ok(())
}

#[test]
fn generator_param_errors_throw_at_call_time() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* g(a = b, b) {}
      var ok = false;
      try { g(); } catch (e) { ok = e instanceof ReferenceError; }
      ok
    "#,
  )?;
  assert!(value_to_bool(value));
  Ok(())
}

