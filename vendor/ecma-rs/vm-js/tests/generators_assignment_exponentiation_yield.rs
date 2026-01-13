use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn exponentiation_assignment_on_binding_uses_pre_yield_old_value() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var x = 2;
      function* g(){ return x **= (yield 0); }
      var it = g();
      var r0 = it.next();
      x = 10; // mutate after yield; should not affect captured old value
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 && x === 8
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_property_captures_base_and_key_before_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o1 = { a: 2 };
      var o2 = { a: 10 };
      var o = o1;
      function* g(){ return o.a **= (yield 0); }
      var it = g();
      var r0 = it.next();
      o = o2;
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 && o1.a === 8 && o2.a === 10
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_property_uses_pre_yield_old_value() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o = { a: 2 };
      function* g(){ return o.a **= (yield 0); }
      var it = g();
      var r0 = it.next();
      o.a = 10; // mutate after yield; should not affect captured old value
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 && o.a === 8
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_computed_property_captures_base_and_key_before_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o1 = { a: 2, b: 3 };
      var o2 = { a: 10, b: 100 };
      var o = o1;
      var k = "a";
      function* g(){ return o[k] **= (yield 0); }
      var it = g();
      var r0 = it.next();
      o = o2;
      k = "b";
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 &&
      o1.a === 8 && o1.b === 3 &&
      o2.a === 10 && o2.b === 100
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
