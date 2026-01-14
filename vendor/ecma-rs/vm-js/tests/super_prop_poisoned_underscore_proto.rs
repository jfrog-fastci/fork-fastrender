use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn super_prop_does_not_consult_poisoned_proto() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      Object.defineProperty(Object.prototype, "__proto__", {
        get() { throw "poison"; }
      });

      ({ m() {
        return super['CONSTRUCTOR'.toLowerCase()] === Object
          && super.toString() === "[object Object]";
      } }).m()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
