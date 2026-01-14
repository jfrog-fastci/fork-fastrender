use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
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

#[test]
fn derived_constructor_param_destructuring_default_this_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {}
        class B extends A {
          constructor({ a = this } = {}) { super(); }
        }
        try { new B({}); "no"; } catch (e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn derived_constructor_param_destructuring_default_this_throws_reference_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {}
      class B extends A {
        constructor({ a = this } = {}) { super(); }
      }
      try { new B({}); "no"; } catch (e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn derived_constructor_body_destructuring_default_this_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {}
        class B extends A {
          constructor() {
            let a;
            ({ a = this } = {});
            super();
          }
        }
        try { new B(); "no"; } catch (e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn derived_constructor_body_destructuring_default_this_throws_reference_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {}
      class B extends A {
        constructor() {
          let a;
          ({ a = this } = {});
          super();
        }
      }
      try { new B(); "no"; } catch (e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn derived_constructor_param_destructuring_default_super_computed_key_does_not_run() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        class A {}
        class B extends A {
          constructor({ a = delete super[(side = 1, "m")] } = {}) { super(); }
        }
        try { new B({}); "no"; } catch (e) { e.name + ":" + side }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError:0");
}

#[test]
fn derived_constructor_param_destructuring_default_super_computed_key_does_not_run_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      class A {}
      class B extends A {
        constructor({ a = delete super[(side = 1, "m")] } = {}) { super(); }
      }
      try { new B({}); "no"; } catch (e) { e.name + ":" + side }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError:0");
  Ok(())
}

#[test]
fn derived_constructor_body_destructuring_default_super_computed_key_does_not_run() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        class A {}
        class B extends A {
          constructor() {
            let a;
            ({ a = delete super[(side = 1, "m")] } = {});
            super();
          }
        }
        try { new B(); "no"; } catch (e) { e.name + ":" + side }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError:0");
}

#[test]
fn derived_constructor_body_destructuring_default_super_computed_key_does_not_run_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      class A {}
      class B extends A {
        constructor() {
          let a;
          ({ a = delete super[(side = 1, "m")] } = {});
          super();
        }
      }
      try { new B(); "no"; } catch (e) { e.name + ":" + side }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError:0");
  Ok(())
}
