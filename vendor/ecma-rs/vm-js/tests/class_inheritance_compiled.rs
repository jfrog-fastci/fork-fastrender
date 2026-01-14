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
fn derived_ctor_eval_super_only_once_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var out = "no error";
      class B {}
      class D extends B {
        constructor() {
          eval("super()");
          try { eval("super()"); }
          catch (e) { out = e.name; }
        }
      }
      new D();
      out === "ReferenceError"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_indirect_eval_super_is_syntax_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var out = "no error";
      class B {}
      class D extends B {
        constructor() {
          const e = eval;
          try { e("super()"); }
          catch (e) { out = e.name; }
          super();
        }
      }
      new D();
      out === "SyntaxError"
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

#[test]
fn derived_ctor_param_arrow_this_before_and_after_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok_before = false;
      var ok_after = false;
      class B {}
      class D extends B {
        constructor(f = () => this) {
          try { f(); } catch (e) { ok_before = e instanceof ReferenceError; }
          super();
          ok_after = f() instanceof D;
        }
      }
      new D();
      ok_before && ok_after
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_param_arrow_super_method_call_before_and_after_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok_before = false;
      var ok_after = false;
      class B { __m() { return this.__x; } }
      class D extends B {
        constructor(f = () => super.__m()) {
          try { f(); } catch (e) { ok_before = e instanceof ReferenceError; }
          super();
          this.__x = 123;
          ok_after = f() === 123;
        }
      }
      new D();
      ok_before && ok_after
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_super_call_argument_eval_does_not_initialize_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var base_called = false;
      var ok = false;
      class B { constructor(arg) { base_called = true; this.arg = arg; } }
      class D extends B {
        constructor() {
          let f = () => this;
          super(f());
        }
      }
      try { new D(); } catch (e) { ok = e instanceof ReferenceError; }
      ok && base_called === false
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_param_super_call_can_initialize_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor(x = super()) {
          ok = x instanceof D && this instanceof D;
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
fn derived_ctor_param_super_call_allows_later_default_param_to_use_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor(a = super(), b = this) {
          ok = a instanceof D && b instanceof D && this instanceof D;
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
fn derived_ctor_param_arrow_defined_before_param_super_call_observes_initialized_this_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor(f = () => this, t = (super(), f())) {
          ok = t === this && t instanceof D;
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
fn derived_ctor_param_super_call_then_body_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor(x = super()) {
          try { super(); } catch (e) { ok = e instanceof ReferenceError; }
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
fn derived_ctor_arrow_delete_super_computed_before_super_does_not_evaluate_key_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      var ok = false;
      class B {}
      class D extends B {
        constructor() {
          let f = () => delete super[(side = 1, "__x")];
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
fn derived_ctor_arrow_super_computed_logical_or_assignment_before_super_does_not_evaluate_key_or_rhs_compiled(
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
          let f = () => (super[(key_side = 1, "__x")] ||= (rhs_side = 1, 1));
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
fn derived_ctor_arrow_super_computed_logical_and_assignment_before_super_does_not_evaluate_key_or_rhs_compiled(
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
          let f = () => (super[(key_side = 1, "__x")] &&= (rhs_side = 1, 1));
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
fn derived_ctor_arrow_super_computed_nullish_assignment_before_super_does_not_evaluate_key_or_rhs_compiled(
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
          let f = () => (super[(key_side = 1, "__x")] ??= (rhs_side = 1, 1));
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
fn derived_ctor_param_this_before_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var base_called = false;
      var ok = false;
      class B { constructor() { base_called = true; } }
      class D extends B {
        constructor(x = this) {
          super();
        }
      }
      try { new D(); } catch (e) { ok = e instanceof ReferenceError; }
      ok && base_called === false
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_param_super_property_before_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var base_called = false;
      var ok = false;
      class B {
        constructor() { base_called = true; }
        __m() { return 1; }
      }
      class D extends B {
        constructor(x = super.__m()) {
          super();
        }
      }
      try { new D(); } catch (e) { ok = e instanceof ReferenceError; }
      ok && base_called === false
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_param_eval_super_initializes_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor(x = eval("super()"), y = this) {
          ok = x === this && y === this && this instanceof D;
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
fn derived_ctor_param_eval_super_only_once_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class B {}
      class D extends B {
        constructor(x = eval("super()")) {
          try { eval("super()"); } catch (e) { ok = e instanceof ReferenceError; }
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
fn derived_ctor_arrow_super_computed_exponentiation_assignment_before_super_does_not_evaluate_key_or_rhs_compiled(
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
          let f = () => (super[(key_side = 1, "__x")] **= (rhs_side = 1, 2));
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
fn derived_ctor_param_arrow_super_call_initializes_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var ok_init = false;
      var ok_second = false;
      class B { constructor() { this.fromB = 1; } }
      class D extends B {
        constructor(f = () => super()) {
          f();
          ok_init = this.fromB === 1 && this instanceof D;
          try { f(); } catch (e) { ok_second = e instanceof ReferenceError; }
        }
      }
      new D();
      ok_init && ok_second
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
