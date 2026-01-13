use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

// Mirrors test262 `language/statements/class/static-init-super-property.js`.
const SOURCE: &str = r#"
  function Parent() {}
  Parent.test262 = 'test262';
  var value;

  class C extends Parent {
    static {
      value = super.test262;
    }
  }

  value;
"#;

#[test]
fn static_init_super_property_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result = rt.exec_script(SOURCE)?;
  assert_eq!(value_to_string(&rt, result), "test262");
  Ok(())
}

#[test]
fn static_init_super_property_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", SOURCE)?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(value_to_string(&rt, result), "test262");
  Ok(())
}
