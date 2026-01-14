use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script)
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn object_literal_anonymous_class_static_name_not_overwritten() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let o = { a: class { static name(){} } };
        JSON.stringify([typeof o.a.name, o.a.name.name])
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, r#"["function","name"]"#);
}

#[test]
fn object_literal_anonymous_class_static_name_not_overwritten_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let o = { a: class { static name(){} } };
      JSON.stringify([typeof o.a.name, o.a.name.name])
    "#,
  )?;
  assert_value_is_utf8(&rt, value, r#"["function","name"]"#);
  Ok(())
}
