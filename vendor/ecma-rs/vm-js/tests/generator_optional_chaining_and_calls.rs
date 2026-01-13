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
fn generator_parenthesized_member_call_loses_this_binding() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = { m: function(){ "use strict"; return this === undefined; } };
        return ((yield obj).m)();
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r2.done === true && r2.value === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_optional_chain_call_short_circuits_and_skips_yield_in_args() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function* g(){
        var o = (yield null);
        return o?.b.c(yield "should-not-yield");
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === undefined && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_optional_chain_call_evaluates_yield_in_args_when_not_nullish() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function* g(){
        var called = 0;
        var obj = { b: { c: function(x){ "use strict"; called++; return (this === obj.b) && x; } } };
        var r = obj?.b.c(yield "yielded");
        return r === 123 && called === 1;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(123);
      r1.value === "yielded" && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_parenthesized_optional_chain_callee_does_not_propagate_into_call() -> Result<(), VmError>
{
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(
    r#"
      function* g(){
        try {
          // The optional chain short-circuits after resuming `(yield null)` to null, so the computed
          // key yield must be skipped. However, because the callee is parenthesized, the call is not
          // part of the chain, and the argument yield must still run before the call throws.
          ((yield null)?.x[(yield "should-not-yield")])(yield "arg");
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      var r3 = it.next(0);
      r1.value === null && r1.done === false &&
      r2.value === "arg" && r2.done === false &&
      r3.value === "TypeError" && r3.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
