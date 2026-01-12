use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_call_is_lazy_until_next() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var ran = false;
      function* g() { ran = true; }
      var it = g();
      var before = ran;
      it.next();
      var after = ran;
      before === false && after === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_next_throw_is_propagated_through_spread_call() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function f() {}
      try {
        f(...function*() { throw 123; }());
        0
      } catch (e) {
        e
      }
    "#,
  )?;
  assert_eq!(value, Value::Number(123.0));
  Ok(())
}

