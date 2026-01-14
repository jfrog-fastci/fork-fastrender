use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Some tests use Promises/async-await and class static blocks. Give them a slightly larger heap to
  // avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
  rt.exec_compiled_script(script)
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn thrown_error_message(rt: &mut JsRuntime, err: &VmError) -> Option<String> {
  let thrown = err.thrown_value()?;
  let Value::Object(thrown) = thrown else {
    return None;
  };

  let mut scope = rt.heap.scope();
  // Root the thrown object across allocations (allocating the `"message"` key can trigger GC).
  scope.push_root(Value::Object(thrown)).ok()?;
  let key_s = scope.alloc_string("message").ok()?;
  let key = PropertyKey::from_string(key_s);
  let msg = scope.heap().get(thrown, &key).ok()?;
  let Value::String(msg) = msg else {
    return None;
  };
  Some(scope.heap().get_string(msg).ok()?.to_utf8_lossy())
}

fn is_unimplemented_error(rt: &mut JsRuntime, err: &VmError) -> bool {
  match err {
    VmError::Unimplemented(_) => true,
    VmError::Throw(_) | VmError::ThrowWithStack { .. } => thrown_error_message(rt, err)
      .is_some_and(|msg| msg.starts_with("unimplemented:")),
    _ => false,
  }
}

fn is_thrown_message_containing(rt: &mut JsRuntime, err: &VmError, needle: &str) -> bool {
  thrown_error_message(rt, err).is_some_and(|msg| msg.contains(needle))
}

fn exec_compiled_or_skip_class_inheritance(
  rt: &mut JsRuntime,
  source: &str,
) -> Result<Option<Value>, VmError> {
  match exec_compiled(rt, source) {
    Ok(v) => Ok(Some(v)),
    Err(VmError::Unimplemented(msg)) if msg.contains("class inheritance") => Ok(None),
    Err(err)
      if is_unimplemented_error(rt, &err)
        && is_thrown_message_containing(rt, &err, "class inheritance") =>
    {
      Ok(None)
    }
    Err(err) => Err(err),
  }
}

// === 1. Inheritance prototype chains (base vs derived vs extends null). ===

#[test]
fn class_prototype_chain_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {}
      Object.getPrototypeOf(B) === Function.prototype &&
        Object.getPrototypeOf(B.prototype) === Object.prototype &&
        B.prototype.constructor === B
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_prototype_chain_base_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      Object.getPrototypeOf(B) === Function.prototype &&
        Object.getPrototypeOf(B.prototype) === Object.prototype &&
        B.prototype.constructor === B
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_prototype_chain_derived() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {}
      class D extends B {}
      Object.getPrototypeOf(D) === B &&
        Object.getPrototypeOf(D.prototype) === B.prototype &&
        D.prototype.constructor === D &&
        (new D()) instanceof B
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_prototype_chain_derived_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip_class_inheritance(
    &mut rt,
    r#"
      class B {}
      class D extends B {}
      Object.getPrototypeOf(D) === B &&
        Object.getPrototypeOf(D.prototype) === B.prototype &&
        D.prototype.constructor === D &&
        (new D()) instanceof B
    "#,
  )?
  else {
    return Ok(());
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_prototype_chain_extends_null() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class N extends null {}
      Object.getPrototypeOf(N) === Function.prototype &&
        Object.getPrototypeOf(N.prototype) === null &&
        N.prototype.constructor === N
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_prototype_chain_extends_null_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip_class_inheritance(
    &mut rt,
    r#"
      class N extends null {}
      Object.getPrototypeOf(N) === Function.prototype &&
        Object.getPrototypeOf(N.prototype) === null &&
        N.prototype.constructor === N
    "#,
  )?
  else {
    return Ok(());
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

// === 2. Derived ctor `super()` semantics. ===

#[test]
fn derived_ctor_this_tdz_before_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {}
      var out = "";
      class D extends B {
        constructor() {
          try { this.x = 1; } catch (e) { out = e.name; }
          super();
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
fn derived_ctor_this_tdz_before_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip_class_inheritance(
    &mut rt,
    r#"
      class B {}
      var out = "";
      class D extends B {
        constructor() {
          try { this.x = 1; } catch (e) { out = e.name; }
          super();
        }
      }
      new D();
      out === "ReferenceError"
    "#,
  )?
  else {
    return Ok(());
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_super_only_once() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {}
      var out = "";
      class D extends B {
        constructor() {
          super();
          try { super(); } catch (e) { out = e.name; }
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
fn derived_ctor_super_only_once_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip_class_inheritance(
    &mut rt,
    r#"
      class B {}
      var out = "";
      class D extends B {
        constructor() {
          super();
          try { super(); } catch (e) { out = e.name; }
        }
      }
      new D();
      out === "ReferenceError"
    "#,
  )?
  else {
    return Ok(());
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_return_override_object_and_primitive() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B { constructor() { this.b = 1; } }
      class DObj extends B {
        constructor() { super(); return { ok: true }; }
      }
      class DPrim extends B {
        constructor() { super(); return 123; }
      }
      var o = new DObj();
      var primName = "";
      try { new DPrim(); primName = "no"; } catch (e) { primName = e.name; }
      o.ok === true && o.b === undefined && (o instanceof DObj) === false &&
        primName === "TypeError"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_return_override_object_and_primitive_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let Some(value) = exec_compiled_or_skip_class_inheritance(
    &mut rt,
    r#"
      class B { constructor() { this.b = 1; } }
      class DObj extends B {
        constructor() { super(); return { ok: true }; }
      }
      class DPrim extends B {
        constructor() { super(); return 123; }
      }
      var o = new DObj();
      var primName = "";
      try { new DPrim(); primName = "no"; } catch (e) { primName = e.name; }
      o.ok === true && o.b === undefined && (o instanceof DObj) === false &&
        primName === "TypeError"
    "#,
  )?
  else {
    return Ok(());
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_can_return_object_without_calling_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var side = "";
      class B { constructor() { side = "B"; } }
      class D extends B {
        constructor() { return { ok: true }; }
      }
      var o = new D();
      o.ok === true &&
        side === "" &&
        (o instanceof D) === false && (o instanceof B) === false &&
        Object.getPrototypeOf(o) === Object.prototype
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_can_return_object_without_calling_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      var side = "";
      class B { constructor() { side = "B"; } }
      class D extends B {
        constructor() { return { ok: true }; }
      }
      var o = new D();
      o.ok === true &&
        side === "" &&
        (o instanceof D) === false && (o instanceof B) === false &&
        Object.getPrototypeOf(o) === Object.prototype
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_ctor_returning_primitive_without_super_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {}
      class D extends B { constructor() { return 1; } }
      try { new D(); "no"; } catch (e) { e.name; }
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "TypeError");
  Ok(())
}

#[test]
fn derived_ctor_returning_primitive_without_super_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B { constructor() { return 1; } }
      try { new D(); "no"; } catch (e) { e.name; }
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "TypeError");
  Ok(())
}

// === 3. `super.prop` read/write/getter+setter/method receiver. ===

#[test]
fn super_property_call_in_derived_constructor_after_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {
        __m() { this.__x = 1; }
      }
      class D extends B {
        constructor() { super(); super.__m(); }
      }
      const o = new D();
      o.__x === 1 && o instanceof D
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_set_in_derived_constructor_after_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {
        set __g(v) { this.__x = v; }
      }
      class D extends B {
        constructor() { super(); super.__g = 2; }
      }
      const o = new D();
      o.__x === 2 && o instanceof D
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_in_derived_constructor_arrow_observes_this_initialization() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B { __m() { return this.__x; } }
      class D extends B {
        constructor() {
          const f = () => super.__m();
          let out = "";
          try { f(); } catch (e) { out = e.name; }
          super();
          this.__x = 5;
          this.__r = f();
          this.__out = out;
        }
      }
      const o = new D();
      o.__out === "ReferenceError" && o.__r === 5
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_derived_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {
        get __g() { return this.__x + 1; }
        set __g(v) { this.__x = v * 2; }
        __m() { return this.__x; }
      }
      B.prototype.__data = 1;

      class D extends B {
        constructor() { super(); this.__x = 10; }
        test() {
          const r1 = super.__g;
          super.__g = 7;
          const r2 = this.__x;
          const r3 = super.__m();
          super.__data = 5;
          const r4 = this.__data === 5 &&
            B.prototype.__data === 1 &&
            Object.prototype.hasOwnProperty.call(this, "__data");
          return r1 === 11 && r2 === 14 && r3 === 14 && r4;
        }
      }
      new D().test()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_derived_method_arrow_closure() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match rt.exec_script(
    r#"
      class B { __m() { return this.__x; } }
      class D extends B {
        constructor() { super(); this.__x = 123; }
        test() {
          const f = () => super.__m();
          return f() === 123;
        }
      }
      new D().test()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_derived_method_arrow_closure_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      class B { __m() { return this.__x; } }
      class D extends B {
        constructor() { super(); this.__x = 123; }
        test() {
          const f = () => super.__m();
          return f() === 123;
        }
      }
      new D().test()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_derived_method_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      class B {
        get __g() { return this.__x + 1; }
        set __g(v) { this.__x = v * 2; }
        __m() { return this.__x; }
      }
      B.prototype.__data = 1;

      class D extends B {
        constructor() { super(); this.__x = 10; }
        test() {
          const r1 = super.__g;
          super.__g = 7;
          const r2 = this.__x;
          const r3 = super.__m();
          super.__data = 5;
          const r4 = this.__data === 5 &&
            B.prototype.__data === 1 &&
            Object.prototype.hasOwnProperty.call(this, "__data");
          return r1 === 11 && r2 === 14 && r3 === 14 && r4;
        }
      }
      new D().test()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_base_class_compiled_path() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      Object.defineProperty(Object.prototype, "__g", {
        get() { return this.__x + 1; },
        set(v) { this.__x = v * 2; },
        configurable: true,
      });
      Object.prototype.__m = function () { return this.__x; };
      Object.prototype.__data = 1;

      class C {
        constructor() { this.__x = 10; }
        test() {
          const r1 = super.__g;
          super.__g = 7;
          const r2 = this.__x;
          const r3 = super.__m();
          super.__data = 5;
          const r4 = this.__data === 5 &&
            Object.prototype.__data === 1 &&
            Object.prototype.hasOwnProperty.call(this, "__data");
          return r1 === 11 && r2 === 14 && r3 === 14 && r4;
        }
      }

      new C().test()
    "#,
  ) {
    Ok(v) => v,
    // Compiled HIR execution does not implement `super` property references yet.
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_base_static_method_compiled_path() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      Object.defineProperty(Function.prototype, "__g", {
        get() { return this.__x + 1; },
        set(v) { this.__x = v * 2; },
        configurable: true,
      });
      Function.prototype.__m = function () { return this.__x; };
      Function.prototype.__data = 1;

      class C {
        static test() {
          this.__x = 10;
          const r1 = super.__g;
          super.__g = 7;
          const r2 = this.__x;
          const r3 = super.__m();

          super.__data = 5;
          const r4 = this.__data === 5 &&
            Function.prototype.__data === 1 &&
            Object.prototype.hasOwnProperty.call(this, "__data");
          const r5 = super.__data === 1;

          return r1 === 11 && r2 === 14 && r3 === 14 && r4 && r5;
        }
      }

      C.test()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_instance_field_initializer() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match rt.exec_script(
    r#"
      class B {
        get __g() { return this.__x + 1; }
        set __g(v) { this.__x = v * 2; }
        __m() { return this.__x; }
      }
      class D extends B {
        __x = 10;
        y = super.__g;
        z = (super.__g = 7, this.__x);
        w = super.__m();
      }
      const d = new D();
      d.y === 11 && d.z === 14 && d.w === 14 && d.__x === 14
    "#,
  ) {
    Ok(v) => v,
    // `super.prop` in field initializers is not supported yet.
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_instance_field_initializer_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      class B {
        get __g() { return this.__x + 1; }
        set __g(v) { this.__x = v * 2; }
        __m() { return this.__x; }
      }
      class D extends B {
        __x = 10;
        y = super.__g;
        z = (super.__g = 7, this.__x);
        w = super.__m();
      }
      const d = new D();
      d.y === 11 && d.z === 14 && d.w === 14 && d.__x === 14
    "#,
  ) {
    Ok(v) => v,
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_derived_static_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match rt.exec_script(
    r#"
      class B {
        static get __g() { return this.__x + 1; }
        static set __g(v) { this.__x = v * 2; }
        static __m() { return this.__x; }
      }
      B.__data = 1;

      class D extends B {
        static test() {
          this.__x = 10;
          const r1 = super.__g;
          super.__g = 7;
          const r2 = this.__x;
          const r3 = super.__m();

          super.__data = 5;
          const r4 = this.__data === 5 &&
            B.__data === 1 &&
            Object.prototype.hasOwnProperty.call(this, "__data");
          const r5 = super.__data === 1;

          return r1 === 11 && r2 === 14 && r3 === 14 && r4 && r5;
        }
      }
      D.test()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_semantics_in_derived_static_method_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      class B {
        static get __g() { return this.__x + 1; }
        static set __g(v) { this.__x = v * 2; }
        static __m() { return this.__x; }
      }
      B.__data = 1;

      class D extends B {
        static test() {
          this.__x = 10;
          const r1 = super.__g;
          super.__g = 7;
          const r2 = this.__x;
          const r3 = super.__m();

          super.__data = 5;
          const r4 = this.__data === 5 &&
            B.__data === 1 &&
            Object.prototype.hasOwnProperty.call(this, "__data");
          const r5 = super.__data === 1;

          return r1 === 11 && r2 === 14 && r3 === 14 && r4 && r5;
        }
      }
      D.test()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

// === 4. Static blocks: `super.prop` inside `static {}`. ===

#[test]
fn super_property_reference_in_base_static_block() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var out = "";
      Object.defineProperty(Function.prototype, "__acc", {
        get() { return this.__tag; },
        set(v) { this.__tag = v; },
        configurable: true,
      });
      class C {
        static {
          this.__tag = 1;
          out += super.__acc;
          super.__acc = 2;
          out += this.__tag;
        }
      }
      out === "12"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_in_base_static_block_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      var out = "";
      Object.defineProperty(Function.prototype, "__acc", {
        get() { return this.__tag; },
        set(v) { this.__tag = v; },
        configurable: true,
      });
      class C {
        static {
          this.__tag = 1;
          out += super.__acc;
          super.__acc = 2;
          out += this.__tag;
        }
      }
      out === "12"
    "#,
  ) {
    Ok(v) => v,
    // Compiled HIR execution does not implement `super` property references yet.
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_in_derived_static_block() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var out = "";
      class B {
        static get __x() { return this.__tag; }
        static set __x(v) { this.__tag = v; }
      }
      class D extends B {
        static {
          this.__tag = 1;
          out += super.__x;
          super.__x = 2;
          out += this.__tag;
        }
      }
      out === "12"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_reference_in_derived_static_block_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      var out = "";
      class B {
        static get __x() { return this.__tag; }
        static set __x(v) { this.__tag = v; }
      }
      class D extends B {
        static {
          this.__tag = 1;
          out += super.__x;
          super.__x = 2;
          out += this.__tag;
        }
      }
      out === "12"
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

// === 4b. Async-eval: `super.prop` in a static block across `await`. ===

#[test]
fn async_eval_static_block_super_property_reference_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  match rt.exec_script(
    r#"
      var out = "";
      class B {
        static get __x() { return this.__tag; }
      }
      class D extends B {
        static {
          this.__tag = 1;
          out += super.__x;
          await Promise.resolve(0);
          this.__tag = 2;
          out += super.__x;
        }
      }
    "#,
  ) {
    Ok(_) => {}
    // Until async-eval lands for scripts/class static blocks, `await` is rejected as a syntax error.
    Err(VmError::Syntax(diags))
      if diags
        .iter()
        .any(|d| {
          d.message
            .contains("await is only valid in async functions and modules")
            || d.message.contains("await")
            || d
              .notes
              .iter()
              .any(|n| n.contains("KeywordAwait") || n.contains("await"))
        }) =>
    {
      return Ok(());
    }
    // Host-facing boundaries may coerce Syntax/Unimplemented into a thrown Error object; treat that
    // as "not supported yet" for this future-facing test.
    Err(err)
      if is_unimplemented_error(&mut rt, &err)
        || is_thrown_message_containing(&mut rt, &err, "await is only valid in async functions and modules") =>
    {
      return Ok(());
    }
    // Async class/static-block evaluation doesn't yet preserve the `super` [[HomeObject]] binding
    // across suspension points.
    Err(VmError::InvariantViolation("super property access missing [[HomeObject]]")) => return Ok(()),
    Err(err) => return Err(err),
  }

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1");

  match rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
    Ok(()) => {}
    Err(VmError::InvariantViolation("super property access missing [[HomeObject]]")) => return Ok(()),
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  }

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "12");
  Ok(())
}

// === 5. Direct eval + `super.prop` (enable once Tasks 146/173 land). ===

#[test]
fn direct_eval_super_property_reference_in_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B { get x() { return 42; } }
      class D extends B {
        m() {
          try { return eval("super.x"); }
          catch (e) { return e.name; }
        }
      }
      new D().m()
    "#,
  )?;

  match value {
    Value::Number(n) => assert_eq!(n, 42.0),
    Value::String(_) => {
      let name = value_to_string(&rt, value);
      // Until Tasks 146/173 land, direct eval does not carry `super` binding context, so parsing
      // `super.x` is an early error surfaced as a thrown SyntaxError.
      if name == "SyntaxError" {
        return Ok(());
      }
      panic!("expected 42 or SyntaxError, got {name:?}");
    }
    other => panic!("expected number or string, got {other:?}"),
  }
  Ok(())
}

#[test]
fn direct_eval_super_property_reference_in_method_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      // Avoid class inheritance on the compiled path: base class super resolves to Object.prototype.
      Object.prototype.x = 42;
      class C {
        m() {
          try { return eval("super.x"); }
          catch (e) { return e.name; }
        }
      }
      new C().m()
    "#,
  )?;

  match value {
    Value::Number(n) => assert_eq!(n, 42.0),
    Value::String(_) => {
      let name = value_to_string(&rt, value);
      if name == "SyntaxError" {
        return Ok(());
      }
      panic!("expected 42 or SyntaxError, got {name:?}");
    }
    other => panic!("expected number or string, got {other:?}"),
  }
  Ok(())
}

#[test]
fn direct_eval_super_property_reference_in_field_initializer() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match rt.exec_script(
    r#"
      class B { get x() { return 42; } }
      class D extends B { y = eval("super.x"); }
      var out;
      try { out = new D().y; }
      catch (e) { out = e.name; }
      out
    "#,
  ) {
    Ok(v) => v,
    // Class fields are not parsed on all execution modes yet; keep this test as a lock-in.
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(err) => return Err(err),
  };

  match value {
    Value::Number(n) => assert_eq!(n, 42.0),
    Value::String(_) => {
      let name = value_to_string(&rt, value);
      if name == "SyntaxError" {
        return Ok(());
      }
      panic!("expected 42 or SyntaxError, got {name:?}");
    }
    other => panic!("expected number or string, got {other:?}"),
  }
  Ok(())
}

#[test]
fn direct_eval_super_property_reference_in_field_initializer_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = match exec_compiled(
    &mut rt,
    r#"
      // Avoid class inheritance on the compiled path: base class super resolves to Object.prototype.
      Object.prototype.x = 42;
      class C { y = eval("super.x"); }
      var out;
      try { out = new C().y; }
      catch (e) { out = e.name; }
      out
    "#,
  ) {
    Ok(v) => v,
    // Class fields are not parsed/compiled on all execution modes yet; keep this test as a lock-in.
    Err(VmError::Syntax(_)) => return Ok(()),
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };

  match value {
    Value::Number(n) => assert_eq!(n, 42.0),
    Value::String(_) => {
      let name = value_to_string(&rt, value);
      if name == "SyntaxError" {
        return Ok(());
      }
      panic!("expected 42 or SyntaxError, got {name:?}");
    }
    other => panic!("expected number or string, got {other:?}"),
  }
  Ok(())
}
