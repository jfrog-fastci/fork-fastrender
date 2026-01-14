use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime_aggressive_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force frequent GC cycles so we catch missing rooting of temporary values during class
  // definition (e.g. ephemeral `extends (class A {})` super constructors).
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<class_extends>", source)?;
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
fn class_extends_prototype_wiring() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class A {}
    class B extends A {}
    Object.getPrototypeOf(B) === A &&
      Object.getPrototypeOf(B.prototype) === A.prototype &&
      (new B() instanceof A)
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_extends_prototype_wiring_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    class A {}
    class B extends A {}
    Object.getPrototypeOf(B) === A &&
      Object.getPrototypeOf(B.prototype) === A.prototype &&
      (new B() instanceof A)
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_extends_non_constructor_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    try { class B extends 123 {} ; 'no' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn class_extends_non_constructor_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    try { class B extends 123 {} ; 'no' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn class_extends_ephemeral_super_survives_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
    class B extends (class A {}) {}
    const A = Object.getPrototypeOf(B);
    Object.getPrototypeOf(B) === A &&
      Object.getPrototypeOf(B.prototype) === A.prototype &&
      (new B() instanceof A)
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_extends_ephemeral_super_survives_gc_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
    class B extends (class A {}) {}
    const A = Object.getPrototypeOf(B);
    Object.getPrototypeOf(B) === A &&
      Object.getPrototypeOf(B.prototype) === A.prototype &&
      (new B() instanceof A)
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_extends_named_class_expr_extends_sees_inner_name_tdz() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var C = function() {};
    try { (class C extends C {}); 'no' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn class_extends_named_class_expr_extends_sees_inner_name_tdz_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var C = function() {};
    try { (class C extends C {}); 'no' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}
