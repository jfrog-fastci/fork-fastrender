use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Keep the heap small-ish to exercise rooting/GC paths without being flaky.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
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

fn thrown_error_name(rt: &mut JsRuntime, err: &VmError) -> Option<String> {
  let thrown = err.thrown_value()?;
  let Value::Object(thrown) = thrown else {
    return None;
  };

  let mut scope = rt.heap.scope();
  // Root the thrown value across key allocation (`"name"`) + property access.
  scope.push_root(Value::Object(thrown)).ok()?;
  let key_s = scope.alloc_string("name").ok()?;
  let key = PropertyKey::from_string(key_s);
  let name = scope.heap().get(thrown, &key).ok()?;
  let Value::String(name) = name else {
    return None;
  };
  Some(scope.heap().get_string(name).ok()?.to_utf8_lossy())
}

#[test]
fn compiled_global_lexical_decl_conflicts_with_existing_var() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let _ = exec_compiled(&mut rt, "var x = 1;")?;
  let err = exec_compiled(&mut rt, "let x = 2;").unwrap_err();
  assert_eq!(
    thrown_error_name(&mut rt, &err).as_deref(),
    Some("SyntaxError")
  );
  Ok(())
}

#[test]
fn compiled_global_lexical_decl_conflicts_with_existing_lexical() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let _ = exec_compiled(&mut rt, "let y = 1;")?;
  let err = exec_compiled(&mut rt, "let y = 2;").unwrap_err();
  assert_eq!(
    thrown_error_name(&mut rt, &err).as_deref(),
    Some("SyntaxError")
  );
  Ok(())
}

#[test]
fn global_var_on_configurable_property_does_not_block_lexical_declaration() -> Result<(), VmError> {
  let mut rt = new_runtime();
  // Script #1: create a configurable global property by simple assignment.
  rt.exec_script("globalThis.z = 1;")?;
  // Script #2: `var` declaration on an existing global property is a no-op.
  rt.exec_script("var z;")?;

  // Global lexical bindings should be allowed to shadow a configurable global property, even after
  // a `var` declaration that did not create a non-deletable binding.
  let value = rt.exec_script(
    r#"
      let z = 2;
      z === 2 && globalThis.z === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn global_var_on_configurable_property_does_not_block_lexical_declaration_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let _ = exec_compiled(&mut rt, "globalThis.w = 1;")?;
  let _ = exec_compiled(&mut rt, "var w;")?;
  let value = exec_compiled(
    &mut rt,
    r#"
      let w = 2;
      w === 2 && globalThis.w === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
