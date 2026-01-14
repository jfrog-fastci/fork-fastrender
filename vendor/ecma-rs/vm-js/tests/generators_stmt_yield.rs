use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_for_triple_init() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (var i = yield 0; i < 1; i++) {}
        return i;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      r1.value === 0 && r1.done === false &&
      r2.value === 2 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_for_triple_condition() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (var i = 0; (yield 0) < 1; i++) { break; }
        return i;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      r1.value === 0 && r1.done === false &&
      r2.value === 0 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_switch_discriminant_and_case_expr() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        switch (yield 0) {
          case 1: return 10;
          default: return 20;
        }
      }

      var it1 = g();
      var r1 = it1.next();
      var r2 = it1.next(1);

      function* h() {
        switch (0) {
          case (yield 1): return 100;
          default: return 200;
        }
      }

      var it2 = h();
      var s1 = it2.next();
      var s2 = it2.next(0);

      r1.value === 0 && r1.done === false &&
      r2.value === 10 && r2.done === true &&
      s1.value === 1 && s1.done === false &&
      s2.value === 100 && s2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_with_object_expression() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var x = 1;
        with (yield 0) { return x; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next({ x: 42 });
      r1.value === 0 && r1.done === false &&
      r2.value === 42 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

