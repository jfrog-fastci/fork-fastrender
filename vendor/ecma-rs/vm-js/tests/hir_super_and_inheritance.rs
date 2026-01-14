use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<hir_super_and_inheritance>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script)
}

fn assert_value_is_number(value: Value, expected: f64) {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  assert_eq!(n, expected);
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn compiled_derived_constructor_super_and_super_method_call() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    class A { m(){ return this.x } }
    class B extends A {
      constructor(){ super(); this.x = 2 }
      m(){ return super.m() }
    }
    new B().m()
    "#,
  )?;

  assert_value_is_number(value, 2.0);
  Ok(())
}

#[test]
fn compiled_static_super_method_call() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    class A { static m(){ return this.v } }
    class B extends A { static f(){ return super.m() } }
    B.v = 3;
    B.f()
    "#,
  )?;

  assert_value_is_number(value, 3.0);
  Ok(())
}

#[test]
fn compiled_derived_constructor_this_before_super_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    (function() {
      class A {}
      class B extends A {
        constructor() { this.x = 1; super(); }
      }
      try { new B(); return "no"; }
      catch (e) { return e.name; }
    })()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn compiled_derived_constructor_missing_super_throws_reference_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    (function() {
      class A {}
      class B extends A { constructor() {} }
      try { new B(); return "no"; }
      catch (e) { return e.name; }
    })()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn compiled_derived_constructor_arrow_before_super_captures_initialized_this() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    (function() {
      var out;
      class A {}
      class B extends A {
        constructor() {
          const f = () => this;
          super();
          out = (f() === this);
        }
      }
      new B();
      return out;
    })()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
