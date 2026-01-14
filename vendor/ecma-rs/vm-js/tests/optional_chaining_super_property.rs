use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_aggressive_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force frequent GC cycles so we catch missing rooting of temporary values during super property
  // evaluation and optional chaining.
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<optional_chaining_super_property>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "script unexpectedly requires AST fallback"
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
fn optional_chaining_super_property_member_expression() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      class A {
        a () {}
        undf () {
          return super.a?.c;
        }
      }
      class B extends A {
        dot () {
          return super.a?.name;
        }
        expr () {
          return super['a']?.name;
        }
        undf2 () {
          return super.b?.c;
        }
      }
      const subcls = new B();
      subcls.dot() === 'a' &&
        subcls.expr() === 'a' &&
        subcls.undf2() === undefined &&
        (new A()).undf() === undefined
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn optional_chaining_super_property_member_expression_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {
        a () {}
        undf () {
          return super.a?.c;
        }
      }
      class B extends A {
        dot () {
          return super.a?.name;
        }
        expr () {
          return super['a']?.name;
        }
        undf2 () {
          return super.b?.c;
        }
      }
      const subcls = new B();
      subcls.dot() === 'a' &&
        subcls.expr() === 'a' &&
        subcls.undf2() === undefined &&
        (new A()).undf() === undefined
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn optional_chaining_super_property_optional_call_binds_this() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      let called = false;
      let context;
      class Base {
        method() {
          called = true;
          context = this;
        }
      }
      class Foo extends Base {
        method() {
          super.method?.();
        }
      }
      const foo = new Foo();
      foo.method();
      called === true && context === foo
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn optional_chaining_super_property_optional_call_binds_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      let called = false;
      let context;
      class Base {
        method() {
          called = true;
          context = this;
        }
      }
      class Foo extends Base {
        method() {
          super.method?.();
        }
      }
      const foo = new Foo();
      foo.method();
      called === true && context === foo
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn optional_chaining_super_property_optional_call_binds_this_computed() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      let called = false;
      let context;
      class Base {
        method() {
          called = true;
          context = this;
        }
      }
      class Foo extends Base {
        method() {
          super['method']?.();
        }
      }
      const foo = new Foo();
      foo.method();
      called === true && context === foo
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn optional_chaining_super_property_optional_call_binds_this_computed_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      let called = false;
      let context;
      class Base {
        method() {
          called = true;
          context = this;
        }
      }
      class Foo extends Base {
        method() {
          super['method']?.();
        }
      }
      const foo = new Foo();
      foo.method();
      called === true && context === foo
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_optional_call_short_circuits_on_null() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      let side = 0;
      class B {
        get m() { side = side + 1; return null; }
      }
      class D extends B {
        f() { const r = super.m?.(); return String(side) + ":" + String(r); }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:undefined");
  Ok(())
}

#[test]
fn super_optional_call_short_circuits_on_null_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      let side = 0;
      class B {
        get m() { side = side + 1; return null; }
      }
      class D extends B {
        f() { const r = super.m?.(); return String(side) + ":" + String(r); }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:undefined");
  Ok(())
}

#[test]
fn super_optional_call_short_circuits_on_null_computed_key() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      let side = 0;
      let key_side = 0;
      class B {
        get m() { side = side + 1; return null; }
      }
      class D extends B {
        f() {
          function key() { key_side = key_side + 1; return 'm'; }
          const r = super[key()]?.();
          return String(side) + ":" + String(key_side) + ":" + String(r);
        }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:1:undefined");
  Ok(())
}

#[test]
fn super_optional_call_short_circuits_on_null_computed_key_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      let side = 0;
      let key_side = 0;
      class B {
        get m() { side = side + 1; return null; }
      }
      class D extends B {
        f() {
          function key() { key_side = key_side + 1; return 'm'; }
          const r = super[key()]?.();
          return String(side) + ":" + String(key_side) + ":" + String(r);
        }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:1:undefined");
  Ok(())
}

#[test]
fn super_optional_chain_member_continuation_short_circuits_on_null() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      let side = 0;
      class B {
        get a() { side = side + 1; return null; }
      }
      class D extends B {
        f() { const r = super.a?.b; return String(side) + ":" + String(r); }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:undefined");
  Ok(())
}

#[test]
fn super_optional_chain_member_continuation_short_circuits_on_null_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      let side = 0;
      class B {
        get a() { side = side + 1; return null; }
      }
      class D extends B {
        f() { const r = super.a?.b; return String(side) + ":" + String(r); }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:undefined");
  Ok(())
}

#[test]
fn super_optional_chain_member_continuation_short_circuits_on_null_computed_base() -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = rt.exec_script(
    r#"
      let side = 0;
      let key_side = 0;
      class B {
        get a() { side = side + 1; return null; }
      }
      class D extends B {
        f() {
          function key() { key_side = key_side + 1; return 'a'; }
          const r = super[key()]?.b;
          return String(side) + ":" + String(key_side) + ":" + String(r);
        }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:1:undefined");
  Ok(())
}

#[test]
fn super_optional_chain_member_continuation_short_circuits_on_null_computed_base_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime_aggressive_gc();
  let value = exec_compiled(
    &mut rt,
    r#"
      let side = 0;
      let key_side = 0;
      class B {
        get a() { side = side + 1; return null; }
      }
      class D extends B {
        f() {
          function key() { key_side = key_side + 1; return 'a'; }
          const r = super[key()]?.b;
          return String(side) + ":" + String(key_side) + ":" + String(r);
        }
      }
      new D().f()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "1:1:undefined");
  Ok(())
}
