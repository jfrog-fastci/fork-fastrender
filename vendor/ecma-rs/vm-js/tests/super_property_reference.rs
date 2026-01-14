use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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

#[test]
fn super_property_instance_method_get_uses_receiver() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class B {
      get x() { return this.y; }
    }
    class D extends B {
      constructor() { super(); this.y = 42; }
      getX() { return super.x; }
    }
    new D().getX()
    "#,
  )?;
  assert_value_is_number(value, 42.0);
  Ok(())
}

#[test]
fn super_property_base_class_instance_method_get() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    Object.defineProperty(Object.prototype, "x", {
      get() { return this.y; },
      configurable: true,
    });
    class C {
      constructor() { this.y = 7; }
      getX() { return super.x; }
    }
    new C().getX()
    "#,
  )?;
  assert_value_is_number(value, 7.0);
  Ok(())
}

#[test]
fn super_property_static_method_get_uses_receiver() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class B {
      static get x() { return this.y; }
    }
    class D extends B {
      static getX() { return super.x; }
    }
    D.y = 99;
    D.getX()
    "#,
  )?;
  assert_value_is_number(value, 99.0);
  Ok(())
}

#[test]
fn super_property_method_call_uses_receiver() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
    class B {
      m() { return this.y; }
      static m() { return this.y; }
    }
    class D extends B {
      constructor() { super(); this.y = 5; }
      callM() { return super.m(); }
      static callM() { return super.m(); }
    }
    D.y = 123;
    new D().callM() + D.callM()
    "#,
  )?;

  // 5 + 123
  assert_value_is_number(value, 128.0);
  Ok(())
}

#[test]
fn super_property_assignment_sets_receiver_not_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class B {}
    B.prototype.x = 0;
    class D extends B {
      setX(v) { super.x = v; return this.hasOwnProperty("x") + ":" + this.x + ":" + B.prototype.x; }
    }
    new D().setX(1)
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "true:1:0");
  Ok(())
}

#[test]
fn super_property_derived_constructor_before_super_throws_reference_error_and_does_not_run_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
    function testMember() {
      let side = 0;
      class B { get x() { side = 1; return 0; } }
      class D extends B { constructor() { super.x; } }
      try { new D(); } catch (e) { return side + ":" + e.name; }
      return "no";
    }

    function testComputed() {
      let side = 0;
      class B { get x() { return 0; } }
      class D extends B { constructor() { super[(side = 1, "x")]; } }
      try { new D(); } catch (e) { return side + ":" + e.name; }
      return "no";
    }

    function testCallArgs() {
      let side = 0;
      class B { m(v) { return v; } }
      class D extends B { constructor() { super.m(side = 1); } }
      try { new D(); } catch (e) { return side + ":" + e.name; }
      return "no";
    }

    testMember() + "|" + testComputed() + "|" + testCallArgs()
    "#,
  )?;

  assert_value_is_utf8(&rt, value, "0:ReferenceError|0:ReferenceError|0:ReferenceError");
  Ok(())
}

#[test]
fn super_property_computed_key_eval_mutation_affects_super_base_lookup() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    class B {}
    B.prototype.x = 1;
    let newProto = { x: 2 };
    class D extends B {
      getX() {
        // `Expression` is evaluated before `GetSuperBase`, so prototype mutation during the key
        // expression evaluation affects the resolved super base.
        return super[(Object.setPrototypeOf(D.prototype, newProto), "x")];
      }
    }
    new D().getX()
    "#,
  )?;
  assert_value_is_number(value, 2.0);
  Ok(())
}
