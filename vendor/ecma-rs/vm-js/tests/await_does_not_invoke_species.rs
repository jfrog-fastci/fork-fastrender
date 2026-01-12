use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn await_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var called = 0;
      var out = "";

      var p = Promise.resolve(1);
      var ctor = {};
      ctor[Symbol.species] = function C(executor) {
        called++;
        return new Promise(executor);
      };
      p.constructor = ctor;

      async function f() {
        await p;
        out = "ok";
      }
      f();
    "#,
  )?;

  assert_eq!(rt.exec_script("called")?, Value::Number(0.0));
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("called")?, Value::Number(0.0));
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}
