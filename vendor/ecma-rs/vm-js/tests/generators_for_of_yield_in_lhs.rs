use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_for_of_yield_in_array_pattern_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var a;
          // Ensure the loop iterates over a single *array* value whose first element is `undefined`
          // so the array-pattern default initializer runs.
          var undefined = [undefined];
          for ([a = yield 1] of [undefined]) { return a; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var v;
          for ({[yield "k"]: v} of [{k: 3}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
