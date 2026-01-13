use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_object_destructuring_default_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ let a = 0; ({a = yield 1} = {}); return a; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(42);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_default_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ let a = 0; ([a = yield 1] = []); return a; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(7);
      r1.value === 1 && r2.done === true && r2.value === 7
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_computed_property_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ let a = 0; ({[(yield 1)]: a} = {x: 5}); return a; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next('x');
      r1.value === 1 && r2.done === true && r2.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

