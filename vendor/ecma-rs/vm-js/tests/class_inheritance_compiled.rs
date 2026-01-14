use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions, VmError};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  rt.exec_compiled_script(script)
}

#[test]
fn class_extends_prototype_chain_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {}
      Object.getPrototypeOf(D) === B &&
        Object.getPrototypeOf(D.prototype) === B.prototype
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_extends_default_derived_constructor_calls_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { constructor() { this.x = 1; } }
      class D extends B {}
      new D().x === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_this_observes_initialized_this_after_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          super();
          // Return an object wrapper so the arrow's return value is observable even if it is
          // `undefined` (constructor primitive return values are ignored).
          return { v: f() };
        }
      }
      const o = new D();
      o.v instanceof D
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_this_before_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          try { f(); } catch (e) { ok = e instanceof ReferenceError; }
          super();
        }
      }
      new D();
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_this_observes_initialized_this_after_eval_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          eval("super()");
          // Return an object wrapper so the arrow's return value is observable even if it is
          // `undefined` (constructor primitive return values are ignored).
          return { v: f() };
        }
      }
      const o = new D();
      o.v instanceof D
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_this_created_in_eval_observes_initialized_this_after_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = eval("(() => this)");
          super();
          return { v: f() };
        }
      }
      const o = new D();
      o.v instanceof D
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
