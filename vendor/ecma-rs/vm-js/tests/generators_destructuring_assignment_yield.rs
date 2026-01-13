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
