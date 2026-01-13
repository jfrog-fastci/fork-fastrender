use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

#[test]
fn reflect_set_prototype_of_sets_prototype_on_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      let proto = { marker: 1 };
      let o = {};
      let ok = Reflect.setPrototypeOf(o, proto);
      ok === true && Object.getPrototypeOf(o) === proto;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

