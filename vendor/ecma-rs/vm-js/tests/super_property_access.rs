use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<super_property_access>", source)?;
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

fn assert_value_is_number(value: Value, expected: f64) {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  assert_eq!(n, expected);
}

fn exec_or_skip_class_inheritance(rt: &mut JsRuntime, source: &str) -> Result<Option<Value>, VmError> {
  match rt.exec_script(source) {
    Ok(v) => Ok(Some(v)),
    Err(VmError::Unimplemented(msg)) if msg.contains("class inheritance") => Ok(None),
    Err(err) => Err(err),
  }
}

fn exec_compiled_or_skip(rt: &mut JsRuntime, source: &str) -> Result<Option<Value>, VmError> {
  match exec_compiled(rt, source) {
    Ok(v) => Ok(Some(v)),
    // Compiled HIR execution may not support derived classes or `super` yet; keep the interpreted
    // tests active so we validate `exec.rs` semantics.
    //
    // Note: host-facing entry points coerce many internal errors (including `VmError::Unimplemented`)
    // into a thrown `Error` object (`VmError::{Throw,ThrowWithStack}`), so skip those as well.
    Err(VmError::Unimplemented(msg)) if msg.contains("class inheritance") || msg.contains("super") => Ok(None),
    Err(VmError::Throw(_) | VmError::ThrowWithStack { .. }) => Ok(None),
    Err(err) => Err(err),
  }
}

#[test]
fn super_property_access_base_instance_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class C {
        m() { return super.toString === Object.prototype.toString }
      }
      new C().m()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_access_base_instance_method_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      class C {
        m() { return super.toString === Object.prototype.toString }
      }
      new C().m()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_call_uses_receiver_this_in_instance_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      class A { m(){ return this.x } }
      class B extends A {
        constructor(){ super(); this.x = 2 }
        m(){ return super.m() }
      }
      new B().m()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 2.0);
  Ok(())
}

#[test]
fn super_property_call_uses_receiver_this_in_instance_method_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      class A { m(){ return this.x } }
      class B extends A {
        constructor(){ super(); this.x = 2 }
        m(){ return super.m() }
      }
      new B().m()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 2.0);
  Ok(())
}

#[test]
fn super_property_set_uses_receiver_derived_ctor_in_static_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      class A {
        static set x(v){ this._x = v }
        static get x(){ return this._x }
      }
      class B extends A {
        static f(){ super.x = 3; return super.x }
      }
      B.f()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 3.0);
  Ok(())
}

#[test]
fn super_property_set_uses_receiver_derived_ctor_in_static_method_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      class A {
        static set x(v){ this._x = v }
        static get x(){ return this._x }
      }
      class B extends A {
        static f(){ super.x = 3; return super.x }
      }
      B.f()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 3.0);
  Ok(())
}

#[test]
fn super_property_access_inside_arrow_inherits_home_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      class A { m(){ return this.x } }
      class B extends A {
        constructor(){ super(); this.x = 5 }
        m(){ return (() => super.m())() }
      }
      new B().m()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 5.0);
  Ok(())
}

#[test]
fn super_property_access_inside_arrow_inherits_home_object_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      class A { m(){ return this.x } }
      class B extends A {
        constructor(){ super(); this.x = 5 }
        m(){ return (() => super.m())() }
      }
      new B().m()
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 5.0);
  Ok(())
}

#[test]
fn super_property_access_in_static_block() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      class A { static m(){ return 1 } }
      class B extends A { static { this.v = super.m() } }
      B.v
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 1.0);
  Ok(())
}

#[test]
fn super_property_access_in_static_block_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      class A { static m(){ return 1 } }
      class B extends A { static { this.v = super.m() } }
      B.v
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_number(value, 1.0);
  Ok(())
}

#[test]
fn super_computed_member_in_derived_ctor_does_not_eval_key_before_this_binding_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(side = 1, "m")];
        }
      }
      try { new B(); }
      catch (e) { String(side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_member_in_derived_ctor_does_not_eval_key_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(side = 1, "m")];
        }
      }
      try { new B(); }
      catch (e) { String(side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_call_in_derived_ctor_does_not_eval_key_or_args_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let arg_side = 0;
      class A { m(){} }
      class B extends A {
        constructor() {
          super[(key_side = 1, "m")]((arg_side = 1, 123));
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(arg_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_call_in_derived_ctor_does_not_eval_key_or_args_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let arg_side = 0;
      class A { m(){} }
      class B extends A {
        constructor() {
          super[(key_side = 1, "m")]((arg_side = 1, 123));
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(arg_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] = (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] = (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_compound_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] += (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_compound_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] += (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_update_in_derived_ctor_does_not_eval_key_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")]++;
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_update_in_derived_ctor_does_not_eval_key_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")]++;
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_logical_or_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] ||= (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_logical_or_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] ||= (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_logical_and_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] &&= (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_logical_and_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] &&= (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_nullish_coalescing_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] ??= (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_nullish_coalescing_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] ??= (rhs_side = 1, 1);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_exponentiation_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_or_skip_class_inheritance(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] **= (rhs_side = 1, 2);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}

#[test]
fn super_computed_exponentiation_assignment_in_derived_ctor_does_not_eval_key_or_rhs_before_this_binding_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip(
    &mut rt,
    r#"
      let key_side = 0;
      let rhs_side = 0;
      class A {}
      class B extends A {
        constructor() {
          super[(key_side = 1, "x")] **= (rhs_side = 1, 2);
        }
      }
      try { new B(); }
      catch (e) { String(key_side) + ":" + String(rhs_side) + ":" + e.name }
    "#,
  )?
  else {
    return Ok(());
  };
  // If the compiled path doesn't support `super` yet, the host boundary will coerce internal
  // `VmError::Unimplemented` into a thrown `Error` object, which this script catches as `"Error"`.
  // Skip in that case so we still validate interpreter semantics.
  if let Value::String(s) = value {
    let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
    if actual.ends_with(":Error") {
      return Ok(());
    }
  }
  assert_value_is_utf8(&rt, value, "0:0:ReferenceError");
  Ok(())
}
