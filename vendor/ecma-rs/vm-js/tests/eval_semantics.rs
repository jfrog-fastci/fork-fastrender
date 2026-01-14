use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn direct_eval_sees_local_lexical_bindings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function f(){ let x = 1; return eval("x"); } f()"#)
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn indirect_eval_does_not_see_local_lexical_bindings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function f(){ let x = 1; const e = eval; return e("typeof x"); } f()"#)
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from typeof");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "undefined");
}

#[test]
fn parenthesized_eval_is_indirect() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function f(){ let x = 1; return (eval)("typeof x"); } f()"#)
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from typeof");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "undefined");
}

#[test]
fn direct_eval_inherits_strictness_from_caller() {
  let mut rt = new_runtime();

  // Strict direct eval: `with` is an early error (and must be catchable).
  let value = rt
    .exec_script(
      r#"
        function f(){
          "use strict";
          try { eval("with ({x:1}) { x }"); return "no error"; }
          catch (e) { return e.name; }
        }
        f()
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");

  // Indirect eval: caller strictness does not apply; `with` is allowed unless the eval source is
  // strict.
  let value = rt
    .exec_script(r#"function f(){ "use strict"; const e = eval; return e("with ({x:1}) { x }"); } f()"#)
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn eval_syntax_errors_are_catchable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function f(){
          try { eval("let a = 1; let a = 2;"); return "no error"; }
          catch (e) { return e.name; }
        }
        f()
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
}

#[test]
fn direct_eval_var_decl_conflicts_with_outer_let() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function f(){
          let x = 1;
          try { eval("var x = 2"); return "no error"; }
          catch (e) { return e.name; }
        }
        f()
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
}

#[test]
fn strict_direct_eval_does_not_leak_var_declarations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function f(){ "use strict"; eval("var x = 1"); return typeof x; } f()"#)
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from typeof");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "undefined");
}

#[test]
fn eval_assignment_targets_local_for_direct_and_global_for_indirect() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(r#"function f(){ let x = 1; eval("x = 2"); return x; } f()"#)
    .unwrap();
  assert_eq!(value, Value::Number(2.0));

  let value = rt
    .exec_script(r#"function f(){ let x = 1; const e = eval; e("x = 2"); return x === 1 && globalThis.x === 2; } f()"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_with_awaited_argument_is_direct() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        let x = 1;
        return eval(await "x");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn direct_eval_with_awaited_argument_sees_catch_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      // Global `e` should be shadowed by the catch binding for direct eval.
      var e = 2;
      async function f() {
        try { throw 5; }
        catch (e) { return eval(await "e"); }
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(5.0));
  Ok(())
}

#[test]
fn direct_eval_with_awaited_argument_sees_with_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 2;
      async function f() {
        with ({x: 3}) {
          return eval(await "x");
        }
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn direct_eval_with_await_in_later_argument_is_direct() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        let x = 1;
        // The `await` occurs while evaluating the *second* argument, so the call must still be
        // treated as a direct eval.
        return eval("x", await "ignored");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn direct_eval_with_await_spread_argument_is_direct() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        let x = 1;
        // `await` suspends while evaluating the spread source; after resumption the spread produces
        // the eval argument list.
        return eval(...(await ["x"]));
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn direct_eval_with_awaited_argument_inherits_strictness() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        "use strict";
        try { return eval(await "with ({x:1}) { x }"); }
        catch (e) { return e.name; }
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
  Ok(())
}

#[test]
fn strict_direct_eval_with_awaited_argument_does_not_leak_var_declarations() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        "use strict";
        eval(await "var x = 1");
        return typeof x;
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  let Value::String(s) = value else {
    panic!("expected string from typeof");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "undefined");
  Ok(())
}

#[test]
fn direct_eval_var_decl_conflicts_with_outer_let_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        let x = 1;
        try { eval(await "var x = 2"); return "no error"; }
        catch (e) { return e.name; }
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
  Ok(())
}

#[test]
fn parenthesized_eval_with_awaited_argument_is_indirect() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 2;
      async function f() {
        let x = 1;
        return (eval)(await "x");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(2.0));
  Ok(())
}

#[test]
fn optional_chain_eval_with_awaited_argument_is_indirect() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 2;
      async function f() {
        let x = 1;
        return eval?.(await "x");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(2.0));
  Ok(())
}

#[test]
fn shadowed_eval_with_awaited_argument_is_not_direct() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        let eval = function (_) { return 123; };
        return eval(await "x");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(123.0));
  Ok(())
}

#[test]
fn direct_eval_var_decl_creates_local_binding_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 2;
      async function f() {
        eval(await "var x = 1");
        return x === 1 && globalThis.x === 2;
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_with_awaited_argument_sees_this_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 2;
      var obj = {
        x: 123,
        f: async function () {
          return eval(await "this.x");
        }
      };
      obj.f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(123.0));
  Ok(())
}

#[test]
fn parenthesized_eval_with_awaited_argument_sees_global_this() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 2;
      var obj = {
        x: 123,
        f: async function () {
          return (eval)(await "this.x");
        }
      };
      obj.f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(2.0));
  Ok(())
}

#[test]
fn direct_eval_with_awaited_argument_sees_arguments_object() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f(a) {
        return eval(await "arguments[0]");
      }
      f(123).then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(123.0));
  Ok(())
}

#[test]
fn indirect_eval_does_not_inherit_strictness_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        "use strict";
        const e = eval;
        return e(await "with ({x:1}) { x }");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn indirect_eval_var_decl_does_not_conflict_with_outer_let_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var x = 0;
      async function f() {
        "use strict";
        let x = 1;
        const e = eval;
        try { e(await "var x = 2"); }
        catch (e) { return e.name; }
        return x === 1 && globalThis.x === 2;
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_with_multiple_awaited_arguments_is_direct() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        let x = 1;
        return eval(await "x", await "ignored");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn direct_eval_with_multiple_awaits_is_still_direct_if_eval_is_overwritten_between_awaits() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = 0;
      var outside = 0;
      async function f() {
        let x = 1;
        // This call suspends twice while evaluating arguments.
        return eval(await "x", await "ignored");
      }
      f().then(function (v) { out = v; }, function () { out = -1; });

      // Enqueue a microtask that runs after the first `await` resumes but before the second `await`
      // resumes. If the call incorrectly re-resolves `eval` after resumption, it will observe the
      // overwritten binding and stop being a direct eval.
      Promise.resolve().then(function () {
        globalThis.eval = function(_) { return 123; };
        outside = eval("0");
      });

      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("outside")?;
  assert_eq!(value, Value::Number(123.0));

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}
