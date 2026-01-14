use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_assignment_rhs_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { let a = 0; ({a} = yield 0); return a; }
        var it = g();
        var r1 = it.next();
        var r2 = it.next({a: 123});
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 123
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_rhs_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { let a = 0; ([a] = yield 0); return a; }
        var it = g();
        it.next();
        var r = it.next([42]);
        r.done === true && r.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_expression_returns_rhs_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { let a = 0; var v = ({a} = yield 0); return v.a === 7 && a === 7; }
        var it = g();
        it.next();
        var r = it.next({a: 7});
        r.done === true && r.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_rhs_from_yield_then_pattern_yields_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          function* g() {
            var a = 0;
            assigned = ({[(yield 1)]: a} = yield 0);
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;

          var rhs = {x: 5};
          var r2 = it.next(rhs);
          if (r2.done !== false || r2.value !== 1) return false;
          // The assignment expression has not completed yet (it suspended inside the pattern).
          if (typeof assigned !== "undefined") return false;

          var r3 = it.next("x");
          return r3.done === true && r3.value === 5 && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_rhs_from_yield_then_pattern_yields_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          function* g() {
            var a = 0;
            assigned = ([a = yield 1] = yield 0);
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;

          var rhs = [];
          var r2 = it.next(rhs);
          if (r2.done !== false || r2.value !== 1) return false;
          // The assignment expression has not completed yet (it suspended inside the pattern).
          if (typeof assigned !== "undefined") return false;

          var r3 = it.next(7);
          return r3.done === true && r3.value === 7 && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_rhs_from_yield_resumption_elision_rest_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          var rest;
          function* g() {
            var a = 0;
            assigned = ([, a = 9, ...rest] = yield 0);
            return a === 9 && rest.length === 2 && rest[0] === 3 && rest[1] === 4;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var rhs = [1, undefined, 3, 4];
          var r2 = it.next(rhs);
          return r2.done === true && r2.value === true && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_rhs_from_yield_resumption_rest_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          var rest;
          function* g() {
            var a = 0;
            assigned = ({a = 7, ...rest} = yield 0);
            return a === 7 && rest.b === 2 && !Object.prototype.hasOwnProperty.call(rest, "a");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var rhs = {b: 2};
          var r2 = it.next(rhs);
          return r2.done === true && r2.value === true && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
