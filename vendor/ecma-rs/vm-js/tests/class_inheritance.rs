use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<class_inheritance>", source)?;
  rt.exec_compiled_script(script)
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

// === 1. Prototype chain wiring. ===

#[test]
fn base_class_prototype_chain_wiring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {}
        Object.getPrototypeOf(C) === Function.prototype &&
          Object.getPrototypeOf(C.prototype) === Object.prototype
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn base_class_prototype_chain_wiring_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class C {}
      Object.getPrototypeOf(C) === Function.prototype &&
        Object.getPrototypeOf(C.prototype) === Object.prototype
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_class_prototype_chain_wiring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {}
        class D extends B {}
        Object.getPrototypeOf(D) === B &&
          Object.getPrototypeOf(D.prototype) === B.prototype
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_class_prototype_chain_wiring_compiled() -> Result<(), VmError> {
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
fn class_extends_null_prototype_chain_wiring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class N extends null {}
        Object.getPrototypeOf(N) === Function.prototype &&
          Object.getPrototypeOf(N.prototype) === null
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn class_extends_null_prototype_chain_wiring_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class N extends null {}
      Object.getPrototypeOf(N) === Function.prototype &&
        Object.getPrototypeOf(N.prototype) === null
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

// === 2. Default derived constructor semantics. ===

#[test]
fn derived_default_constructor_calls_super_and_forwards_new_target_and_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          constructor(v) {
            this.x = v;
            this.nt_ok = (new.target === D);
          }
        }
        class D extends B {}
        const o = new D(123);
        o.x === 123 && o.nt_ok === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_default_constructor_calls_super_and_forwards_new_target_and_args_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {
        constructor(v) {
          this.x = v;
          this.nt_ok = (new.target === D);
        }
      }
      class D extends B {}
      const o = new D(123);
      o.x === 123 && o.nt_ok === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_default_constructor_calls_super() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { constructor() { this.x = 1; } }
        class D extends B {}
        new D().x === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_default_constructor_calls_super_compiled() -> Result<(), VmError> {
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

// === 3. Explicit derived constructor semantics. ===

#[test]
fn derived_constructor_super_initializes_this() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { constructor() { this.x = 1; } }
        class D extends B {
          constructor() {
            super();
            this.y = 2;
          }
        }
        const o = new D();
        o.x === 1 && o.y === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_constructor_super_initializes_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { constructor() { this.x = 1; } }
      class D extends B {
        constructor() {
          super();
          this.y = 2;
        }
      }
      const o = new D();
      o.x === 1 && o.y === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_constructor_this_access_before_super_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let out = "";
        class B {}
        class D extends B {
          constructor() {
            try { this.x = 1; } catch (e) { out = e.name; }
            super();
          }
        }
        new D();
        out
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn derived_constructor_this_access_before_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let out = "";
      class B {}
      class D extends B {
        constructor() {
          try { this.x = 1; } catch (e) { out = e.name; }
          super();
        }
      }
      new D();
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn derived_constructor_can_return_object_without_super() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        class B { constructor() { side = 1; this.x = 1; } }
        class D extends B {
          constructor() {
            return { y: 2 };
          }
        }
        const o = new D();
        side === 0 &&
        o.y === 2 &&
          o.x === undefined &&
          (o instanceof D) === false &&
          (o instanceof B) === false &&
          Object.getPrototypeOf(o) === Object.prototype
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_constructor_can_return_object_without_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      class B { constructor() { side = 1; this.x = 1; } }
      class D extends B {
        constructor() {
          return { y: 2 };
        }
      }
      const o = new D();
      side === 0 &&
      o.y === 2 &&
        o.x === undefined &&
        (o instanceof D) === false &&
        (o instanceof B) === false &&
        Object.getPrototypeOf(o) === Object.prototype
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
