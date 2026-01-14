use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out;
            for (const {[(yield 1)]: x} in {abc: 0}) {
              out = x;
            }
            return out;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("length");
          return r2.done === true && r2.value === 3;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_default_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out;
            for (const {a: x = yield 1} in {abc: 0}) {
              out = x;
            }
            return out;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(5);
          return r2.done === true && r2.value === 5;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_computed_key_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out = [];
            for (const {[(yield 1)]: x} in {a: 0, bb: 0}) {
              out.push(x);
            }
            return out.join(",");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("length");
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next("length");
          return r3.done === true && r3.value === "1,2";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_default_value_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out = [];
            for (const {a: x = yield 1} in {a: 0, b: 0}) {
              out.push(x);
            }
            return out.join(",");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(10);
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next(20);
          return r3.done === true && r3.value === "10,20";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
