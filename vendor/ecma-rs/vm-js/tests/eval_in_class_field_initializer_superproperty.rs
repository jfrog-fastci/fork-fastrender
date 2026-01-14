use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<eval_class_field_eval>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script)
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
fn direct_eval_in_arrow_in_public_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => eval('executed = true; super.x;');
    }
    var c = new C();
    var before = executed;
    var r = c.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_arrow_in_public_field_initializer_allows_super_computed_property() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => eval('executed = true; super["x"];');
    }
    var c = new C();
    var before = executed;
    var r = c.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
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
fn indirect_eval_in_arrow_in_field_initializer_rejects_super_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => indirectEval('executed = true; super.x;');
    }
    var c = new C();
    try { c.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_arrow_in_field_initializer_rejects_super_computed_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => indirectEval('executed = true; super["x"];');
    }
    var c = new C();
    try { c.f(); }
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
fn direct_eval_in_arrow_in_static_public_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static f = () => eval('executed = true; super.x;');
    }
    var before = executed;
    var r = C.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_arrow_in_static_public_field_initializer_allows_super_computed_property(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static f = () => eval('executed = true; super["x"];');
    }
    var before = executed;
    var r = C.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_direct_eval_in_static_public_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_direct_eval_in_public_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_direct_eval_in_public_field_initializer_allows_super_computed_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_direct_eval_in_arrow_in_field_initializer_allows_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => eval('executed = true; super.x;');
    }
    var c = new C();
    var before = executed;
    var r = c.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_direct_eval_in_arrow_in_field_initializer_allows_super_computed_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => eval('executed = true; super["x"];');
    }
    var c = new C();
    var before = executed;
    var r = c.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_direct_eval_in_static_public_field_initializer_allows_super_computed_property(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_direct_eval_in_arrow_in_static_public_field_initializer_allows_super_property(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static f = () => eval('executed = true; super.x;');
    }
    var before = executed;
    var r = C.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_direct_eval_in_arrow_in_static_public_field_initializer_allows_super_computed_property(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static f = () => eval('executed = true; super["x"];');
    }
    var before = executed;
    var r = C.f.call({ tag: 0 });
    before === false && executed === true && r === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_indirect_eval_in_field_initializer_rejects_super_property_and_skips_side_effects() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_indirect_eval_in_field_initializer_rejects_super_computed_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_indirect_eval_in_static_field_initializer_rejects_super_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
fn compiled_indirect_eval_in_static_field_initializer_rejects_super_computed_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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

#[test]
fn compiled_indirect_eval_in_arrow_in_field_initializer_rejects_super_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => indirectEval('executed = true; super.x;');
    }
    var c = new C();
    try { c.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_indirect_eval_in_arrow_in_field_initializer_rejects_super_computed_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class A {
      constructor() { this.tag = 7; }
      get x() { return this.tag; }
    }
    class C extends A {
      f = () => indirectEval('executed = true; super["x"];');
    }
    var c = new C();
    try { c.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_indirect_eval_in_arrow_in_static_field_initializer_rejects_super_computed_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static f = () => indirectEval('executed = true; super["x"];');
    }
    try { C.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
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
fn indirect_eval_in_arrow_in_static_field_initializer_rejects_super_property_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var ok = false;
    var indirectEval = eval;
    class B { static get x() { return this.tag; } }
    class C extends B {
      static tag = 7;
      static f = () => indirectEval('executed = true; super.x;');
    }
    try { C.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed;
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
fn direct_eval_in_private_static_field_initializer_allows_super_property_set_in_arrow() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static set x(v) { this._x = v; } }
    class C extends B {
      static _x = 0;
      static #f = eval('executed = true; () => { super.x = 7; return this._x; }');
      static getF() { return C.#f; }
    }
    var f = C.getF();
    var before = executed;
    var r = f.call({ _x: 123 });
    before === false && executed === true && r === 7 && C._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_private_static_field_initializer_allows_super_computed_property_set_in_arrow(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static set x(v) { this._x = v; } }
    class C extends B {
      static _x = 0;
      static #f = eval('executed = true; () => { super["x"] = 7; return this._x; }');
      static getF() { return C.#f; }
    }
    var f = C.getF();
    var before = executed;
    var r = f.call({ _x: 123 });
    before === false && executed === true && r === 7 && C._x === 7;
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
fn indirect_eval_in_private_static_field_initializer_rejects_super_property_set_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class B { static set x(v) { setterExecuted = true; } }
    try {
      class C extends B {
        static #f = indirectEval('executed = true; () => { super.x = 7; }');
      }
    } catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
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

#[test]
fn direct_eval_in_public_field_initializer_allows_super_property_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A { set x(v) { this._x = v; } }
    class C extends A {
      y = eval('executed = true; super.x = 7;');
    }
    var c = new C();
    executed && c._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_field_initializer_rejects_super_property_set_and_skips_side_effects() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class A { set x(v) { setterExecuted = true; } }
    class C extends A {
      y = indirectEval('executed = true; super.x = 7;');
    }
    try { new C(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_static_public_field_initializer_allows_super_computed_property_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static set x(v) { this._x = v; } }
    class C extends B {
      static y = eval('executed = true; super["x"] = 7;');
    }
    executed && C._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_static_field_initializer_rejects_super_property_set_and_skips_side_effects() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class B { static set x(v) { setterExecuted = true; } }
    try {
      class C extends B {
        static y = indirectEval('executed = true; super.x = 7;');
      }
    } catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_direct_eval_in_public_field_initializer_allows_super_property_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    class A { set x(v) { this._x = v; } }
    class C extends A {
      y = eval('executed = true; super.x = 7;');
    }
    var c = new C();
    executed && c._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_indirect_eval_in_static_field_initializer_rejects_super_property_set_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class B { static set x(v) { setterExecuted = true; } }
    try {
      class C extends B {
        static y = indirectEval('executed = true; super.x = 7;');
      }
    } catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_arrow_in_public_field_initializer_allows_super_property_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class A { set x(v) { this._x = v; } }
    class C extends A {
      f = () => eval('executed = true; super.x = 7;');
    }
    var c = new C();
    var before = executed;
    c.f.call({});
    before === false && executed === true && c._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_arrow_in_field_initializer_rejects_super_property_set_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class A { set x(v) { setterExecuted = true; } }
    class C extends A {
      f = () => indirectEval('executed = true; super.x = 7;');
    }
    var c = new C();
    try { c.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_arrow_in_static_public_field_initializer_allows_super_computed_property_set(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    class B { static set x(v) { this._x = v; } }
    class C extends B {
      static f = () => eval('executed = true; super["x"] = 7;');
    }
    var before = executed;
    C.f.call({});
    before === false && executed === true && C._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_arrow_in_static_field_initializer_rejects_super_computed_property_set_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class B { static set x(v) { setterExecuted = true; } }
    class C extends B {
      static f = () => indirectEval('executed = true; super["x"] = 7;');
    }
    try { C.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_direct_eval_in_arrow_in_field_initializer_allows_super_property_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    class A { set x(v) { this._x = v; } }
    class C extends A {
      f = () => eval('executed = true; super.x = 7;');
    }
    var c = new C();
    var before = executed;
    c.f.call({});
    before === false && executed === true && c._x === 7;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn compiled_indirect_eval_in_arrow_in_static_field_initializer_rejects_super_computed_property_set_and_skips_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
    var executed = false;
    var setterExecuted = false;
    var ok = false;
    var indirectEval = eval;
    class B { static set x(v) { setterExecuted = true; } }
    class C extends B {
      static f = () => indirectEval('executed = true; super["x"] = 7;');
    }
    try { C.f(); }
    catch (e) { ok = e.name === 'SyntaxError'; }
    ok && !executed && !setterExecuted;
    "#,
  )?;

  assert_value_is_bool(value, true);
  Ok(())
}
