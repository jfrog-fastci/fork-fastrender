use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_binary_operator_mul_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return 2 * (yield 1); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(3);
        r1.value === 1 && r1.done === false &&
        r2.value === 6 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_binary_operator_strict_equality() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 0) === 123; }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(123);
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_relational_in_and_instanceof_operators() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // Relational (<) with yield in RHS.
        function* g_lt() { return 5 < (yield 0); }
        var it1 = g_lt();
        var a1 = it1.next();
        var a2 = it1.next(6);
        var ok1 = a1.value === 0 && a1.done === false &&
                  a2.value === true && a2.done === true;

        // Relational (>=) with yield in LHS.
        function* g_ge() { return (yield 0) >= 7; }
        var it2 = g_ge();
        var b1 = it2.next();
        var b2 = it2.next(7);
        var ok2 = b1.value === 0 && b1.done === false &&
                  b2.value === true && b2.done === true;

        // `in` with yield in RHS.
        function* g_in() { return "a" in (yield 0); }
        var it3 = g_in();
        var c1 = it3.next();
        var c2 = it3.next({ a: 1 });
        var ok3 = c1.value === 0 && c1.done === false &&
                  c2.value === true && c2.done === true;

        // `instanceof` with yield in RHS.
        function C() {}
        function* g_instanceof() {
          var o = new C();
          return o instanceof (yield 0);
        }
        var it4 = g_instanceof();
        var d1 = it4.next();
        var d2 = it4.next(C);
        var ok4 = d1.value === 0 && d1.done === false &&
                  d2.value === true && d2.done === true;

        ok1 && ok2 && ok3 && ok4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_simple_assignment_rhs_happens_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 1;
        function* g() { x = (yield 0); return x; }
        var it = g();
        var r1 = it.next();
        x = 100;
        var r2 = it.next(5);
        r1.value === 0 && r1.done === false &&
        r2.value === 5 && r2.done === true &&
        x === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_member_assignment_rhs_happens_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var obj = { a: 1 };
        function* g() { obj.a = (yield 0); return obj.a; }
        var it = g();
        var r1 = it.next();
        obj.a = 100;
        var r2 = it.next(5);
        r1.value === 0 && r1.done === false &&
        r2.value === 5 && r2.done === true &&
        obj.a === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_member_assignment_rhs_captures_base_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 1 };
        var o2 = { a: 10 };
        var o = o1;
        function* g(){ o.a = (yield 0); return o1.a === 5 && o2.a === 10; }
        var it = g();
        var r1 = it.next();
        o = o2;
        var r2 = it.next(5);
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_computed_member_assignment_rhs_captures_key_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o = { a: 1, b: 10 };
        var k = 'a';
        function* g(){ o[k] = (yield 0); return o.a === 5 && o.b === 10; }
        var it = g();
        var r1 = it.next();
        k = 'b';
        var r2 = it.next(5);
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_compound_assignment_rhs_captures_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 1;
        function* g() { return x += (yield 0); }
        var it = g();
        var r1 = it.next();
        x = 100;
        var r2 = it.next(5);
        r1.value === 0 && r1.done === false &&
        r2.value === 6 && r2.done === true &&
        x === 6
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_computed_member_assignment_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          obj[(yield 0)] = 1;
          return obj.a === 1;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("a");
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_update_expression_computed_key_postfix() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = { a: 1 };
          var r = obj[(yield 0)]++;
          return r === 1 && obj.a === 2;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("a");
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_update_expression_computed_key_prefix() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = { a: 1 };
          var r = ++obj[(yield 0)];
          return r === 2 && obj.a === 2;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("a");
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_new_expression_arguments() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          function C(x) { this.x = x; }
          var o = new C((yield 1));
          return o.x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_delete_expression_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = { a: 1 };
          var r = delete obj[(yield 0)];
          return r === true && ("a" in obj) === false;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("a");
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
