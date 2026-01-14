use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn async_function_default_param_throw_rejects_and_body_not_run() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var callCount = 0;
      var ok = false;

      async function f(_ = (function () { throw "boom"; })()) {
        callCount = callCount + 1;
      }

      f().then(
        function () { ok = false; },
        function (e) { ok = (e === "boom"); }
      );
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  assert_eq!(rt.exec_script("callCount")?, Value::Number(0.0));
  Ok(())
}

#[test]
fn async_function_default_param_ref_later_is_tdz_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var callCount = 0;
      var ok = false;

      async function f(x = y, y) {
        callCount = callCount + 1;
      }

      f().then(
        function () { ok = false; },
        function (e) { ok = (e instanceof ReferenceError); }
      );
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  assert_eq!(rt.exec_script("callCount")?, Value::Number(0.0));
  Ok(())
}

#[test]
fn async_function_default_param_ref_self_is_tdz_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var callCount = 0;
      var ok = false;

      async function f(x = x) {
        callCount = callCount + 1;
      }

      f().then(
        function () { ok = false; },
        function (e) { ok = (e instanceof ReferenceError); }
      );
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  assert_eq!(rt.exec_script("callCount")?, Value::Number(0.0));
  Ok(())
}

#[test]
fn async_function_eval_var_in_params_rejects_syntax_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var callCount = 0;
      var ok = false;

      async function f(a = eval("var a = 42")) {
        callCount = callCount + 1;
      }

      f().then(
        function () { ok = false; },
        function (e) { ok = (e instanceof SyntaxError); }
      );
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  assert_eq!(rt.exec_script("callCount")?, Value::Number(0.0));
  Ok(())
}

#[test]
fn async_function_mapped_arguments_aliases_parameters() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var ok = false;

      async function foo(a) {
        arguments[0] = 2;
        if (a !== 2) throw "bad1";
        a = 3;
        if (arguments[0] !== 3) throw "bad2";
      }

      foo(1).then(
        function () { ok = true; },
        function () { ok = false; }
      );
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  Ok(())
}

