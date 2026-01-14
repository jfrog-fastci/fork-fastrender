use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_literal_yield_in_property_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { a: (yield 1) }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      r2.value.a === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { [(yield "k")]: 1 }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(123);
      r1.value === "k" && r1.done === false &&
      r2.done === true &&
      r2.value["123"] === 1 &&
      ("k" in r2.value) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_in_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { ...(yield 0), b: 2 }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next({ a: 1, b: 1 });
      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      r2.value.a === 1 &&
      r2.value.b === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

