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

#[test]
fn derived_ctor_arrow_this_observes_initialized_this_after_super_called_in_nested_arrow_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          (() => super())();
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
fn derived_ctor_arrow_this_observes_initialized_this_after_eval_super_called_in_nested_arrow_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          (() => eval("super()"))();
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
fn derived_ctor_arrow_this_created_in_eval_observes_initialized_this_after_eval_super_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = eval("(() => this)");
          eval("super()");
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
fn derived_ctor_arrow_super_method_call_uses_initialized_this_after_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { __m() { return this.__x; } }
      class D extends B {
        constructor() {
          let f = () => super.__m();
          super();
          this.__x = 123;
          return { v: f() };
        }
      }
      new D().v === 123
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_super_method_call_before_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B { __m() { return 0; } }
      class D extends B {
        constructor() {
          let f = () => super.__m();
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
fn derived_ctor_arrow_this_can_escape_constructor_after_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          super();
          return f;
        }
      }
      const f = new D();
      const o = f();
      o instanceof D
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_this_escapes_without_super_and_throws_when_called_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor() {
          let f = () => this;
          // Returning an object without calling super() is allowed in derived constructors.
          return f;
        }
      }
      const f = new D();
      try { f(); } catch (e) { ok = e instanceof ReferenceError; }
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_arrow_super_computed_property_before_super_does_not_evaluate_key_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      var ok = false;
      class B { __m() { return 0; } }
      class D extends B {
        constructor() {
          let f = () => super[(side = 1, "__m")];
          try { f(); } catch (e) { ok = e instanceof ReferenceError && side === 0; }
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
fn derived_ctor_arrow_super_computed_call_before_super_does_not_evaluate_key_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      var ok = false;
      class B { __m() { return 0; } }
      class D extends B {
        constructor() {
          let f = () => super[(side = 1, "__m")]();
          try { f(); } catch (e) { ok = e instanceof ReferenceError && side === 0; }
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
fn derived_ctor_arrow_super_computed_assignment_before_super_does_not_evaluate_key_or_rhs_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var key_side = 0;
      var rhs_side = 0;
      var ok = false;
      class B {}
      class D extends B {
        constructor() {
          let f = () => (super[(key_side = 1, "__x")] = (rhs_side = 1, 1));
          try { f(); } catch (e) {
            ok = e instanceof ReferenceError && key_side === 0 && rhs_side === 0;
          }
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
fn derived_ctor_arrow_super_computed_compound_assignment_before_super_does_not_evaluate_key_or_rhs_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var key_side = 0;
      var rhs_side = 0;
      var ok = false;
      class B {}
      class D extends B {
        constructor() {
          let f = () => (super[(key_side = 1, "__x")] += (rhs_side = 1, 1));
          try { f(); } catch (e) {
            ok = e instanceof ReferenceError && key_side === 0 && rhs_side === 0;
          }
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
fn derived_ctor_arrow_super_computed_update_before_super_does_not_evaluate_key_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var key_side = 0;
      var ok = false;
      class B {}
      class D extends B {
        constructor() {
          let f = () => super[(key_side = 1, "__x")]++;
          try { f(); } catch (e) { ok = e instanceof ReferenceError && key_side === 0; }
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
