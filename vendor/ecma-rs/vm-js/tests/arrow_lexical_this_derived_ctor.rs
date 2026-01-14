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
fn arrow_created_pre_super_called_post_super_sees_initialized_this() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {}
        class B extends A {
          constructor() {
            const f = () => this instanceof B;
            super();
            this.ok = f();
          }
        }
        new B().ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn arrow_created_pre_super_called_post_super_sees_initialized_this_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {}
      class B extends A {
        constructor() {
          const f = () => this instanceof B;
          super();
          this.ok = f();
        }
      }
      new B().ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn arrow_created_pre_super_can_use_super_post_super() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A { m() { return this.x; } }
        class B extends A {
          constructor() {
            const f = () => super.m();
            super();
            this.x = 123;
            this.v = f();
          }
        }
        new B().v
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(123.0));
}

#[test]
fn arrow_created_pre_super_can_use_super_post_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A { m() { return this.x; } }
      class B extends A {
        constructor() {
          const f = () => super.m();
          super();
          this.x = 123;
          this.v = f();
        }
      }
      new B().v
    "#,
  )?;
  assert_eq!(value, Value::Number(123.0));
  Ok(())
}

#[test]
fn arrow_called_pre_super_throws_reference_error_for_this_and_super() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var out = "";
        class A { m() { return 1; } }
        class B extends A {
          constructor() {
            const f = () => this;
            const g = () => super.m();
            try { f(); out = "no-this"; } catch(e) { out = e.name; }
            try { g(); out += "|no-super"; } catch(e) { out += "|" + e.name; }
            super();
          }
        }
        new B();
        out
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError|ReferenceError");
}

#[test]
fn arrow_called_pre_super_throws_reference_error_for_this_and_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var out = "";
      class A { m() { return 1; } }
      class B extends A {
        constructor() {
          const f = () => this;
          const g = () => super.m();
          try { f(); out = "no-this"; } catch(e) { out = e.name; }
          try { g(); out += "|no-super"; } catch(e) { out += "|" + e.name; }
          super();
        }
      }
      new B();
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError|ReferenceError");
  Ok(())
}

