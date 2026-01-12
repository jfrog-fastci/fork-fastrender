use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
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

#[test]
fn async_function_return_value_resolves_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { return "ok"; }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn async_await_promise_resolve() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { return await Promise.resolve("ok"); }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_variable_declarator_initializer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const v = await Promise.resolve("ok");
        return v;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_yields_to_microtasks() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      log.push(1);
      async function f() {
        log.push(2);
        await 0;
        log.push(4);
      }
      f();
      log.push(3);
      log.join("")
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "123");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("log.join(\"\")")?;
  assert_eq!(value_to_string(&rt, value), "1234");
  Ok(())
}

#[test]
fn async_throw_rejects_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { throw "nope"; }
      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "nope");
  Ok(())
}

#[test]
fn async_throw_await_rejects_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { throw await Promise.resolve("boom"); }
      f().then(function () { out = "bad"; }, function (e) { out = e; });
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
fn return_expression_with_post_await_member_access() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { return (await Promise.resolve({ x: "ok" })).x; }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_var_decl_nested_initializer_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const v = (await Promise.resolve({ x: "ok" })).x;
        return v;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_object_pattern_default() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const { x = await Promise.resolve("ok") } = {};
        return x;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_array_pattern_default() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const [x = await Promise.resolve("ok")] = [];
        return x;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_object_pattern_computed_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const { [await Promise.resolve("x")]: v } = { x: "ok" };
        return v;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_return_method_call() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return (await Promise.resolve({
          x: "ok",
          f() { return this.x; },
        })).f();
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_expression_statement_nested_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        (await Promise.resolve({ x: "ignored" })).x;
        return "ok";
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_async_expression_body_nested_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      (async () => (await Promise.resolve({ x: "ok" })).x)().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn return_expression_with_chained_post_await_member_access() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return (await Promise.resolve({ a: { b: "ok" } })).a.b;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn return_expression_with_chained_post_await_call_and_member_access() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return (await Promise.resolve({
          x: "ok",
          make() { return { y: this.x }; },
        })).make().y;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn call_awaited_function_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        return (await Promise.resolve(function () { return "ok"; }))();
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_operand_throw_rejects_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { return await nope; }
      f().then(function () { out = "bad"; }, function (e) { out = e.name; });
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
fn await_rejection_is_catchable_with_try_catch() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        try {
          await Promise.reject("boom");
          return "bad";
        } catch (e) {
          return "caught:" + e;
        }
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "caught:boom");
  Ok(())
}

#[test]
fn await_rejection_runs_finally() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var finally_ran = "";
      async function f() {
        try {
          await Promise.reject("boom");
        } finally {
          finally_ran = "yes";
        }
      }
      f().then(
        function () { out = "bad"; },
        function (e) { out = finally_ran + ":" + e; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "yes:boom");
  Ok(())
}

#[test]
fn await_in_call_args_and_binary_ops() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const a = 1 + await Promise.resolve(2);
        const b = String.fromCharCode(await Promise.resolve(97));
        return "" + a + b;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "3a");
  Ok(())
}

#[test]
fn multiple_awaits_in_one_function() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const a = await Promise.resolve("a");
        const b = await Promise.resolve("b");
        return a + b;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn await_in_while_loop_preserves_microtask_order() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      async function f() {
        log.push("start");
        var i = 0;
        while (i < 2) {
          log.push("b" + i);
          Promise.resolve().then(function () { log.push("m" + i); });
          await 0;
          log.push("a" + i);
          i++;
        }
        log.push("end");
      }
      f();
      log.join("")
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "startb0");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("log.join(\"\")")?;
  assert_eq!(value_to_string(&rt, value), "startb0m0a0b1m1a1end");
  Ok(())
}

#[test]
fn await_observes_promise_resolve_constructor_getter_side_effects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var count = 0;
      Object.defineProperty(Promise.prototype, "constructor", {
        configurable: true,
        get: function () { count++; return Promise; }
      });
      async function f() { await Promise.resolve(0); }
      f();
      count
    "#,
  )?;
  assert_eq!(value_to_number(value), 1.0);

  // Drain the queued async resumption job so the runtime can be dropped without leaked roots.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  Ok(())
}

#[test]
fn await_rejects_when_promise_constructor_getter_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var p = Promise.resolve(0);
      Object.defineProperty(p, "constructor", {
        get: function () { throw "boom"; }
      });
      async function f() { await p; }
      f().then(
        function () { out = "fulfilled"; },
        function (e) { out = e; }
      );
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
fn await_in_for_loop_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var s = "";
        for (var i = 0; i < 2; i++) {
          s = s + await Promise.resolve("a");
        }
        return s;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "aa");
  Ok(())
}

#[test]
fn await_in_for_of_loop_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var s = "";
        for (var x of ["a", "b"]) {
          s = s + await Promise.resolve(x);
        }
        return s;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn await_in_for_of_break_closes_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let log = "";
        const iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            i: 0,
            next() {
              this.i++;
              if (this.i === 1) return { value: "a", done: false };
              return { value: "b", done: false };
            },
            return() {
              log += "R";
              return {};
            },
          };
        };
        for (const x of iterable) {
          log += x;
          await 0;
          break;
        }
        return log;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "aR");
  Ok(())
}

#[test]
fn await_in_for_of_iterator_close_throw_overrides_break() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            next() { return { value: "a", done: false }; },
            return() { throw "close"; }
          };
        };
        for (const x of iterable) {
          await 0;
          break;
        }
        return "bad";
      }
      f().then(function () { out = "fulfilled"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "close");
  Ok(())
}

#[test]
fn await_in_switch_case_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var x = 1;
        var s = "";
        switch (x) {
          case 1:
            s = await Promise.resolve("ok");
            break;
          default:
            s = "bad";
        }
        return s;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_do_while_loop_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var s = "";
        var i = 0;
        do {
          s = s + await Promise.resolve("a");
          i++;
        } while (i < 2);
        return s;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "aa");
  Ok(())
}

#[test]
fn break_and_continue_across_await_in_loop_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var s = "";
        for (var i = 0; i < 3; i++) {
          await 0;
          if (i == 1) {
            continue;
          }
          s = s + i;
          if (i == 2) {
            break;
          }
        }
        return s;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "02");
  Ok(())
}

#[test]
fn logical_short_circuit_skips_await_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var side = "";
        false && (side = await Promise.resolve("bad"));
        true || (side = await Promise.resolve("bad2"));
        return side === "" ? "ok" : "bad:" + side;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_with_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { with ({ x: "ok" }) { return await Promise.resolve(x); } }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_with_object_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() { with (await Promise.resolve({ x: "ok" })) { return x; } }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}
