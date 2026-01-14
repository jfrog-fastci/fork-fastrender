use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async/await allocates multiple Promises and microtask jobs; keep the heap limit large enough
  // to avoid spurious `VmError::OutOfMemory` failures as builtin coverage grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_for_in_over_typed_array_skips_prototype_numeric_keys() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Prototype numeric index keys should be ignored when the typed array does not have a valid
  // integer index (consistent with the `in` operator and TypedArray `[[HasProperty]]` semantics).
  //
  // Include an `await` in the loop body so the `for..in` statement is evaluated via the async
  // evaluator path even when there are no iterations.
  rt.exec_script(
    r#"
      out = "";
      async function f() {
        Uint8Array.prototype['0']=7;
        Uint8Array.prototype['-0']=7;
        Uint8Array.prototype['1.5']=7;
        Uint8Array.prototype['4294967295']=7;
        var s='';
        for (var k in new Uint8Array(0)) { s+=k; await 0; }
        delete Uint8Array.prototype['0'];
        delete Uint8Array.prototype['-0'];
        delete Uint8Array.prototype['1.5'];
        delete Uint8Array.prototype['4294967295'];
        out = s;
      }
      f();
      out
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "");
  Ok(())
}

#[test]
fn async_for_in_over_typed_array_keeps_non_numeric_prototype_keys() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      out = "";
      async function f() {
        Uint8Array.prototype.foo=1;
        var s='';
        for (var k in new Uint8Array(0)) { s+=k; await 0; }
        delete Uint8Array.prototype.foo;
        out = s;
      }
      f();
      out
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "foo");
  Ok(())
}

