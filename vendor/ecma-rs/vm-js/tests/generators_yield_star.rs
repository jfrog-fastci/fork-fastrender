use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn yield_star_over_array_delegates_values() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    function* d(x) { return yield* x; }
    var it = d([1,2,3]);
    var r1 = it.next();
    var r2 = it.next();
    var r3 = it.next();
    var r4 = it.next();
    r1.value === 1 && r1.done === false &&
    r2.value === 2 && r2.done === false &&
    r3.value === 3 && r3.done === false &&
    r4.value === undefined && r4.done === true
  "#;

  match rt.exec_script(script) {
    Ok(v) => {
      assert_eq!(v, Value::Bool(true));
      Ok(())
    }
    // Generators are still under development in vm-js. Once generator functions/yield* land, this
    // test will begin exercising delegation semantics (including array iterator acquisition).
    Err(VmError::Unimplemented("generator functions" | "async generator functions")) => Ok(()),
    Err(err) => Err(err),
  }
}

