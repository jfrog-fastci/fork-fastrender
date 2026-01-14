use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Some of these tests use Promises/async-await; give them a slightly larger heap to avoid
  // spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_bool(value: Value, expected: bool) {
  let Value::Bool(actual) = value else {
    panic!("expected bool, got {value:?}");
  };
  assert_eq!(actual, expected);
}

#[test]
fn direct_eval_in_public_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A {}
    class C extends A {
      x = eval('executed = true; super.x;');
    }
    new C();
    executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_public_field_initializer_allows_super_computed_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A {}
    class C extends A {
      x = eval('executed = true; super["x"];');
    }
    new C();
    executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_private_field_initializer_allows_super_property_in_arrow() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A {}
    class C extends A {
      #f = eval('executed = true; () => super.x;');
      run() { this.#f(); }
    }
    new C().run();
    executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_private_field_initializer_allows_super_computed_property_in_arrow(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A {}
    class C extends A {
      #f = eval('executed = true; () => super["x"];');
      run() { this.#f(); }
    }
    new C().run();
    executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_field_initializer_rejects_super_property_and_skips_side_effects() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class A {}
    class C extends A {
      x = indirectEval('executed = true; super.x;');
    }
    try { new C(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_field_initializer_rejects_super_computed_property_and_skips_side_effects() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class A {}
    class C extends A {
      x = indirectEval('executed = true; super["x"];');
    }
    try { new C(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_static_public_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static y = eval('executed = true; super.x;');
    }
    executed && C.y === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_static_public_field_initializer_allows_super_computed_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static y = eval('executed = true; super["x"];');
    }
    executed && C.y === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_private_static_field_initializer_allows_super_property_in_arrow() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static #f = eval('executed = true; () => super.x;');
      static getF() { return C.#f; }
    }
    var f = C.getF();
    executed && f.call({ tag: 0 }) === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_private_static_field_initializer_allows_super_computed_property_in_arrow(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static #f = eval('executed = true; () => super["x"];');
      static getF() { return C.#f; }
    }
    var f = C.getF();
    executed && f.call({ tag: 0 }) === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_static_field_initializer_rejects_super_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class B {}
    try {
      class C extends B {
        static y = indirectEval('executed = true; super.x;');
      }
    } catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_static_field_initializer_rejects_super_computed_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class B {}
    try {
      class C extends B {
        static y = indirectEval('executed = true; super["x"];');
      }
    } catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}
