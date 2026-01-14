use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_object_destructuring_default_preserves_assignment_result() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = {};
          let res = ({a = yield 1} = rhs);
          return res === rhs && a === 42;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next(42);
        return r2.done === true && r2.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_default_preserves_assignment_result() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = [];
          let res = ([a = yield 1] = rhs);
          return res === rhs && a === 7;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next(7);
        return r2.done === true && r2.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_computed_key_preserves_assignment_result() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = {x: 5};
          let res = ({[(yield 1)]: a} = rhs);
          return res === rhs && a === 5;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next("x");
        return r2.done === true && r2.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_computed_key_then_default_yields_twice() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let b = 0;
          ({[(yield 1)]: a, b = yield 2} = {x: 5});
          return a === 5 && b === 7;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next("x");
        if (r2.done !== false || r2.value !== 2) return false;
        var r3 = it.next(7);
        return r3.done === true && r3.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_two_defaults_yields_twice() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let b = 0;
          ([a = yield 1, b = yield 2] = []);
          return a === 3 && b === 4;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next(3);
        if (r2.done !== false || r2.value !== 2) return false;
        var r3 = it.next(4);
        return r3.done === true && r3.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
