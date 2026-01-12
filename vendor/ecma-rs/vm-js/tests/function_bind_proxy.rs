use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn function_prototype_bind_accepts_callable_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let p = new Proxy(function f(a,b){ return a+b; }, {});
      let g = p.bind(null, 1);
      g(2) === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn function_prototype_bind_accepts_constructable_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let p = new Proxy(function C(){ this.x=1; }, {});
      let B = p.bind(null);
      (new B()).x === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
