use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests exercise block scoping in loops and (for async/generator variants) create
  // additional microtasks/continuations. Keep the heap limit large enough to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn for_body_let_shadows_outer_parameter() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function(x) {
        for (var i = 0; i < 10; ++i) {
          let x = 'inner' + i;
          continue;
        }
        return x === 'outer';
      })('outer')
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_body_nested_let_shadowing_with_label_continue() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function(x) {
        label: for (var i = 0; i < 3; ++i) {
          let x = 'middle' + i;
          for (var j = 0; j < 3; ++j) {
            let x = 'inner' + j;
            continue label;
          }
        }
        return x === 'outer';
      })('outer')
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_body_let_is_instantiated_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  // Without a fresh lexical env per iteration, the second loop iteration would try to initialize
  // the same `let x` binding again and fail with "binding already initialized".
  let value = rt.exec_script(
    r#"
      for (var i = 0; i < 2; ++i) {
        let x = i;
      }
      true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_body_class_decl_is_block_scoped() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      for (var i = 0; i < 2; ++i) {
        class C {}
      }
      // `typeof` should not throw for an unbound identifier.
      typeof C === 'undefined'
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_body_strict_function_decl_is_block_scoped() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function() {
        'use strict';
        var ok = true;
        for (var i = 0; i < 2; ++i) {
          function g() { return i; }
          ok = ok && (g() === i);
        }
        // In strict mode, block-scoped function declarations are not visible outside the block.
        return ok && (typeof g === 'undefined');
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_for_body_restores_lex_env_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var err = "";
      async function f(x) {
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          await 0;
        }
        out = x;
      }
      f("outer").catch(e => { err = e && e.name; });
      out === "" && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"out === "outer" && err === "" "#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_body_restores_lex_env_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        // Use a for-of loop because the generator evaluator currently supports yield-containing
        // ForOf statements, but not all yield-containing for-triple forms.
        for (var i of [0, 1]) {
          let x = "inner" + i;
          yield x;
        }
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      a === "inner0" && b === "inner1" && c === "outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_body_restores_lex_env_on_break() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function(x) {
        for (var i = 0; i < 10; ++i) {
          let x = 'inner' + i;
          break;
        }
        return x === 'outer';
      })('outer')
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_in_body_let_is_instantiated_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var obj = { a: 1, b: 2 };
      for (var k in obj) {
        let x = k;
      }
      true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_of_body_const_is_instantiated_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      for (var v of [0, 1]) {
        const x = v;
      }
      true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
