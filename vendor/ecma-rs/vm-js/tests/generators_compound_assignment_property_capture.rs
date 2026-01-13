use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_compound_assignment_property_captures_old_value_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = { a: 1 };
      function* g(){ return o.a += (yield 0); }
      var it = g();
      it.next();
      o.a = 100;
      var r = it.next(2);
      // Must use the pre-yield old value (1), not the mutated value (100).
      r.done === true && r.value === 3 && o.a === 3
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_property_captures_base_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o1 = { a: 1 };
      var o2 = { a: 10 };
      var o = o1;
      function* g(){ return o.a += (yield 0); }
      var it = g();
      it.next();
      o = o2;
      var r = it.next(5);
      r.done === true && r.value === 6 && o1.a === 6 && o2.a === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_property_captures_computed_key_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = { a: 1, b: 10 };
      var k = 'a';
      function* g(){ return o[k] += (yield 0); }
      var it = g();
      it.next();
      k = 'b';
      var r = it.next(5);
      r.done === true && r.value === 6 && o.a === 6 && o.b === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_property_captures_base_and_computed_key_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o1 = { a: 1, b: 10 };
      var o2 = { a: 100, b: 1000 };
      var o = o1;
      var k = 'a';
      function* g(){ return o[k] += (yield 0); }
      var it = g();
      it.next();
      // Rebind both base and key after the yield but before resuming.
      o = o2;
      k = 'b';
      var r = it.next(2);
      r.done === true && r.value === 3 &&
      // Must still target the original base/key pair.
      o1.a === 3 && o1.b === 10 &&
      o2.a === 100 && o2.b === 1000
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
