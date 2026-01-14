use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<new_target_via_super_property>", source)?;
  rt.exec_compiled_script(script)
}

// Mirrors test262 `language/expressions/new.target/value-via-super-property.js`.
const SOURCE: &str = r#"
  var newTarget = null;

  class Parent {
    get attr() {
      newTarget = new.target;
    }
  }

  class Child extends Parent {
    constructor() {
      super();
      super.attr;
    }
  }

  new Child();

  newTarget === undefined;
"#;

#[test]
fn new_target_via_super_property_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(SOURCE)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn new_target_via_super_property_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, SOURCE)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

