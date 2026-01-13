use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
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
fn derived_constructor_without_super_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {}
        class B extends A { constructor(){ } }
        try { new B(); 'no' } catch(e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn derived_constructor_without_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {}
      class B extends A { constructor(){ } }
      try { new B(); 'no' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn derived_constructor_without_super_can_return_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {}
        class B extends A { constructor(){ return {}; } }
        try { new B(); 'ok' } catch(e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ok");
}

#[test]
fn derived_constructor_without_super_can_return_object_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {}
      class B extends A { constructor(){ return {}; } }
      try { new B(); 'ok' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ok");
  Ok(())
}

#[test]
fn derived_constructor_this_access_before_super_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {}
        class B extends A { constructor(){ this.x = 1; } }
        try { new B(); 'no' } catch(e) { e.name }
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
      class A {}
      class B extends A { constructor(){ this.x = 1; } }
      try { new B(); 'no' } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn derived_constructor_with_super_initializes_this() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A { constructor() { this.x = 1; } }
        class B extends A { constructor() { super(); } }
        new B().x === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_constructor_with_super_initializes_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A { constructor() { this.x = 1; } }
      class B extends A { constructor() { super(); } }
      new B().x === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_constructor_super_call_twice_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var out = "";
        class A {}
        class B extends A {
          constructor() {
            super();
            try { super(); out = "no"; }
            catch (e) { out = e.name; }
          }
        }
        new B();
        out
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn derived_constructor_super_call_twice_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var out = "";
      class A {}
      class B extends A {
        constructor() {
          super();
          try { super(); out = "no"; }
          catch (e) { out = e.name; }
        }
      }
      new B();
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn derived_constructor_super_on_extends_null_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C extends null { constructor() { super(); } }
        try { new C(); "no" } catch (e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn derived_constructor_super_on_extends_null_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class C extends null { constructor() { super(); } }
      try { new C(); "no" } catch (e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}
