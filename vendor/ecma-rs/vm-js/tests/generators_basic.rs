use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_basic_yield_sequence() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { yield 1; yield 2; return 3; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next();
      var r3 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_first_next_arg_is_ignored() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { var x = yield 1; return x; }
      var it = g();
      var r1 = it.next(10); // ignored for the first resume
      var r2 = it.next(20);
      r1.value === 1 && r1.done === false &&
      r2.value === 20 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_throw_on_fresh_generator_throws_and_closes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = 0;
      function* g() { ran = 1; yield 1; }
      var it = g();
      var caught = false;
      try { it.throw("boom"); } catch (e) { caught = (e === "boom"); }
      var r = it.next();
      caught && ran === 0 && r.done === true && r.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_return_on_fresh_generator_returns_done_without_running_body() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = 0;
      function* g() { ran = 1; yield 1; }
      var it = g();
      var r1 = it.return(42);
      var r2 = it.next();
      r1.value === 42 && r1.done === true &&
      ran === 0 &&
      r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_return_triggers_finally_and_finally_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var f = 0;
      function* g() {
        try { yield 1; }
        finally { f = f + 1; yield 2; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.return(42);
      var r3 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 42 && r3.done === true &&
      f === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_reentrancy_next_while_executing_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var it;
      function* g() {
        yield 1;
        try {
          it.next();
          return false;
        } catch (e) {
          return e.name === "TypeError" && e.message === "Generator is already running";
        }
      }
      it = g();
      var r1 = it.next();
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_without_operand_yields_undefined_even_if_shadowed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { var undefined = 123; yield; }
      var it = g();
      var r1 = it.next();
      r1.value === undefined && r1.done === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_undefined_evaluates_operand_when_explicit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { var undefined = 123; yield undefined; }
      var it = g();
      var r1 = it.next();
      r1.value === 123 && r1.done === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_throw_statement_value_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { throw (yield 1); }
      var it = g();
      var r1 = it.next();
      var caught = false;
      try { it.next("boom"); } catch (e) { caught = (e === "boom"); }
      r1.value === 1 && r1.done === false && caught === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_throw_is_catchable_inside_generator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        try { yield 1; }
        catch (e) { return e; }
      }
      var it = g();
      it.next();
      var r = it.throw("boom");
      var r2 = it.next();
      r.value === "boom" && r.done === true &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_throw_triggers_finally_and_finally_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        try { yield 1; }
        finally { yield 2; }
      }
      var it = g();
      it.next();
      var r1 = it.throw("boom");
      var caught = false;
      try { it.next(); } catch (e) { caught = (e === "boom"); }
      r1.value === 2 && r1.done === false && caught === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_do_while_yield_in_body() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        do { yield 1; } while (false);
        return 2;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_do_while_yield_in_condition() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        do { } while (yield 1);
        return 2;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(false);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_body() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(xs) {
        for (var x of xs) { yield x; }
        return 0;
      }
      var it = g([1, 2, 3]);
      var r1 = it.next();
      var r2 = it.next();
      var r3 = it.next();
      var r4 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.value === 0 && r4.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_labelled_continue_targets_outer_loop() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var x = 0;
        outer: while (x < 2) {
          x = x + 1;
          var y = 0;
          while (y < 2) {
            y = y + 1;
            yield x * 10 + y;
            continue outer;
          }
        }
        return x;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next();
      var r3 = it.next();
      r1.value === 11 && r1.done === false &&
      r2.value === 21 && r2.done === false &&
      r3.value === 2 && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_triple_yield_in_init() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (var i = yield 1; i < 1; i++) { }
        return i;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      r1.value === 1 && r1.done === false &&
      r2.value === 1 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_triple_yield_in_condition() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (var i = 0; yield 1; i++) { }
        return i;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(false);
      r1.value === 1 && r1.done === false &&
      r2.value === 0 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_triple_yield_in_update() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (var i = 0; i < 1; i = yield 1) { }
        return i;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (let k in (yield 1)) {
          return k;
        }
        return "none";
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next({a: 0});
      r1.value === 1 && r1.done === false &&
      r2.value === "a" && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
