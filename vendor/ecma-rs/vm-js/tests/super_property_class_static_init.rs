use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn super_property_works_in_class_static_block() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class A { static get x(){ return 1 } }
      class B extends A { static { globalThis.v = super.x } }
      v === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_works_in_private_static_field_initializer() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class A { static y = 2 }
      class B extends A {
        static #x = super.y;
        static getX(){ return this.#x }
      }
      B.getX() === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

