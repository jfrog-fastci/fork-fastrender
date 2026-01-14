use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_template_literal_single_substitution_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return `a${yield 1}b`; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("X");
      r1.value === 1 && r1.done === false &&
      r2.value === "aXb" && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_template_literal_multiple_substitutions_yield_left_to_right() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return `a${yield 1}b${yield 2}c${yield 3}d`; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("X");
      var r3 = it.next("Y");
      var r4 = it.next("Z");
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.value === "aXbYcZd" && r4.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

