use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

#[test]
fn generator_optional_chain_short_circuit_propagates_through_continuation() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function* g(){ return (yield 0)?.a.b; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.done === false && r2.done === true && r2.value === undefined
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_parenthesized_member_call_does_not_bind_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = { m: function(){ return this === obj; } };
        return ((yield obj).m)();
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r2.done === true && r2.value === false
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

