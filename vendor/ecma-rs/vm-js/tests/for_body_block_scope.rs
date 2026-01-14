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
        // Use a for-of loop for simplicity; generator execution supports yield in for-of, for-in
        // and for(;;) forms.
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
fn async_for_body_preserves_inner_let_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = [];
      var err = "";
      async function f(x) {
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          await 0;
          out.push(x);
        }
        out.push(x);
      }
      f("outer").catch(e => { err = e && e.name; });
      out.length === 0 && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(
    r#"
      out.length === 3
        && out[0] === "inner0"
        && out[1] === "inner1"
        && out[2] === "outer"
        && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_body_preserves_inner_let_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = [];
      function* gen(x) {
        for (var i of [0, 1]) {
          let x = "inner" + i;
          yield x;
          out.push(x);
        }
        out.push(x);
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      a === "inner0"
        && b === "inner1"
        && c === "outer"
        && out.length === 3
        && out[0] === "inner0"
        && out[1] === "inner1"
        && out[2] === "outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_for_body_restores_env_on_break_after_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = [];
      async function f(x) {
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          await 0;
          log.push(x);
          break;
        }
        out = x;
      }
      f("outer").catch(e => { out = "err:" + (e && e.name); });
      out === "" && log.length === 0
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"out === "outer" && log.length === 1 && log[0] === "inner0""#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_body_restores_env_on_break_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        for (var i of [0, 1]) {
          let x = "inner" + i;
          yield x;
          break;
        }
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      a === "inner0" && b === "outer"
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

#[test]
fn for_body_let_closure_captures_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var fns = [];
      for (var i = 0; i < 3; ++i) {
        let x = i;
        fns.push(function() { return x; });
      }
      fns[0]() === 0 && fns[1]() === 1 && fns[2]() === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_in_body_let_closure_captures_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var obj = { a: 1, b: 2 };
      var fns = [];
      for (var k in obj) {
        let x = k;
        fns.push(function() { return x; });
      }
      var r0 = fns[0]();
      var r1 = fns[1]();
      (r0 === "a" && r1 === "b") || (r0 === "b" && r1 === "a")
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_of_body_let_closure_captures_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var fns = [];
      for (var v of [0, 1, 2]) {
        let x = v;
        fns.push(function() { return x; });
      }
      fns[0]() === 0 && fns[1]() === 1 && fns[2]() === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn for_body_restores_lex_env_on_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function(x) {
        try {
          for (var i = 0; i < 10; ++i) {
            let x = "inner" + i;
            throw i;
          }
        } catch (e) {}
        return x === "outer";
      })("outer")
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_for_body_restores_env_on_continue_after_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = [];
      async function f(x) {
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          await 0;
          log.push(x);
          continue;
        }
        out = x;
      }
      f("outer").catch(e => { out = "err:" + (e && e.name); });
      out === "" && log.length === 0
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value =
    rt.exec_script(r#"out === "outer" && log.length === 2 && log[0] === "inner0" && log[1] === "inner1""#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_body_restores_env_on_continue_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        // Use for-of for simplicity; generator execution supports yield in for-of, for-in and
        // for(;;) forms.
        var out = [];
        for (var i of [0, 1]) {
          let x = "inner" + i;
          yield x;
          out.push(x);
          continue;
        }
        out.push(x);
        return out.join(",");
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      a === "inner0" && b === "inner1" && c === "inner0,inner1,outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_triple_body_restores_lex_env_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        for (var i = 0; i < 2; ++i) {
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
fn generator_for_triple_body_preserves_inner_let_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = [];
      function* gen(x) {
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          yield x;
          out.push(x);
        }
        out.push(x);
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      a === "inner0"
        && b === "inner1"
        && c === "outer"
        && out.length === 3
        && out[0] === "inner0"
        && out[1] === "inner1"
        && out[2] === "outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_triple_body_restores_env_on_break_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          yield x;
          break;
        }
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      a === "inner0" && b === "outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_triple_body_restores_env_on_continue_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        var out = [];
        for (var i = 0; i < 2; ++i) {
          let x = "inner" + i;
          yield x;
          out.push(x);
          continue;
        }
        out.push(x);
        return out.join(",");
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      a === "inner0" && b === "inner1" && c === "inner0,inner1,outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_in_body_restores_lex_env_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        for (var k in {a: 0, b: 1}) {
          let x = "inner" + k;
          yield x;
        }
        return x;
      }
      var g = gen("outer");
      var r1 = g.next();
      var r2 = g.next();
      var r3 = g.next();
      var ys = [r1.value, r2.value];
      ys.sort();
      r1.done === false
        && r2.done === false
        && r3.done === true
        && ys.join(",") === "innera,innerb"
        && r3.value === "outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_in_body_preserves_inner_let_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = [];
      function* gen(x) {
        for (var k in {a: 0, b: 1}) {
          let x = "inner" + k;
          yield x;
          out.push(x);
        }
        out.push(x);
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      var ys = [a, b];
      ys.sort();
      out.sort();
      ys.join(",") === "innera,innerb"
        && c === "outer"
        && out.join(",") === "innera,innerb,outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_in_body_restores_env_on_break_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        for (var k in {a: 0, b: 1}) {
          let x = "inner" + k;
          yield x;
          break;
        }
        return x;
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      (a === "innera" || a === "innerb") && b === "outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_for_in_body_restores_env_on_continue_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      function* gen(x) {
        var out = [];
        for (var k in {a: 0, b: 1}) {
          let x = "inner" + k;
          yield x;
          out.push(x);
          continue;
        }
        out.push(x);
        out.sort();
        return out.join(",");
      }
      var g = gen("outer");
      var a = g.next().value;
      var b = g.next().value;
      var c = g.next().value;
      var ys = [a, b];
      ys.sort();
      ys.join(",") === "innera,innerb" && c === "innera,innerb,outer"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_for_await_of_body_preserves_inner_let_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = [];
      var err = "";
      async function f(x) {
        for await (var v of [0, 1]) {
          let x = "inner" + v;
          await 0;
          out.push(x);
        }
        out.push(x);
      }
      f("outer").catch(e => { err = e && e.name; });
      out.length === 0 && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(
    r#"
      out.length === 3
        && out[0] === "inner0"
        && out[1] === "inner1"
        && out[2] === "outer"
        && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_for_await_of_body_let_closure_captures_per_iteration() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = [];
      var err = "";
      async function f() {
        var fns = [];
        for await (var v of [0, 1, 2]) {
          let x = v;
          fns.push(function() { return x; });
        }
        out = [fns[0](), fns[1](), fns[2]()];
      }
      f().catch(e => { err = e && e.name; });
      out.length === 0 && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"out.length === 3 && out[0] === 0 && out[1] === 1 && out[2] === 2 && err === "" "#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
