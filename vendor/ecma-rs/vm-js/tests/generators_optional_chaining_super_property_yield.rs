use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<generators_optional_chaining_super_property_yield>",
    source,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "script unexpectedly requires AST fallback"
  );
  rt.exec_compiled_script(script)
}

#[test]
fn generator_optional_super_property_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        class C extends Base {
          *g() { return super.missing?.(yield "should-not-yield"); }
        }
        const it = (new C()).g();
        const r = it.next();
        r.value === undefined && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_member_short_circuits_and_skips_yield_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        class C extends Base {
          *g() { return super.missing?.[(yield "should-not-yield")]; }
        }
        const it = (new C()).g();
        const r = it.next();
        r.value === undefined && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_chain_continuation_skips_yield_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        class C extends Base {
          *g() { return super.missing?.a[(yield "should-not-yield")]; }
        }
        const it = (new C()).g();
        const r = it.next();
        r.value === undefined && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_member_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        class C extends Base {
          *g() { return super.missing?.a(yield "should-not-yield"); }
        }
        const it = (new C()).g();
        const r = it.next();
        r.value === undefined && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_call_binds_this_across_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let inst;
        class Base {
          m(x) { return this === inst && x === 1; }
        }
        class C extends Base {
          *g() { return super.m?.(yield 0); }
        }
        inst = new C();
        const it = inst.g();
        const r1 = it.next();
        const r2 = it.next(1);
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_yield_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let ok = true;

      class Base0 {}
      class C0 extends Base0 {
        *g() { return super.missing?.(yield "should-not-yield"); }
      }
      const r0 = (new C0()).g().next();
      ok = ok && r0.value === undefined && r0.done === true;

      class Base1 {}
      class C1 extends Base1 {
        *g() { return super.missing?.[(yield "should-not-yield")]; }
      }
      const r1 = (new C1()).g().next();
      ok = ok && r1.value === undefined && r1.done === true;

      class Base2 {}
      class C2 extends Base2 {
        *g() { return super.missing?.a[(yield "should-not-yield")]; }
      }
      const r2 = (new C2()).g().next();
      ok = ok && r2.value === undefined && r2.done === true;

      class Base3 {}
      class C3 extends Base3 {
        *g() { return super.missing?.a(yield "should-not-yield"); }
      }
      const r3 = (new C3()).g().next();
      ok = ok && r3.value === undefined && r3.done === true;

      let inst;
      class Base4 {
        m(x) { return this === inst && x === 1; }
      }
      class C4 extends Base4 {
        *g() { return super.m?.(yield 0); }
      }
      inst = new C4();
      const it4 = inst.g();
      const r4_1 = it4.next();
      const r4_2 = it4.next(1);
      ok = ok && r4_1.value === 0 && r4_1.done === false &&
        r4_2.value === true && r4_2.done === true;

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
