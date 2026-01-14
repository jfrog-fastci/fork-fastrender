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

