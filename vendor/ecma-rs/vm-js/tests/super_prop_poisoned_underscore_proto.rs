use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

#[test]
fn super_prop_does_not_consult_poisoned_proto_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
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
  assert!(
    !script.requires_ast_fallback,
    "test should execute on the compiled (HIR) path"
  );
  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_destructuring_assignment_does_not_consult_poisoned_proto() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      Object.defineProperty(Object.prototype, "__proto__", {
        get() { throw "poison"; }
      });

      ({ m() {
        ({ x: super['CONSTRUCTOR'.toLowerCase()] } = { x: 123 });
        return this.constructor === 123;
      } }).m()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_destructuring_assignment_does_not_consult_poisoned_proto_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      Object.defineProperty(Object.prototype, "__proto__", {
        get() { throw "poison"; }
      });

      ({ m() {
        ({ x: super['CONSTRUCTOR'.toLowerCase()] } = { x: 123 });
        return this.constructor === 123;
      } }).m()
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "test should execute on the compiled (HIR) path"
  );
  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
