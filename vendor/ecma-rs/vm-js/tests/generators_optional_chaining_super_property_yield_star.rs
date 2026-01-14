use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<generators_optional_chaining_super_property_yield_star>",
    source,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "script unexpectedly requires AST fallback"
  );
  rt.exec_compiled_script(script)
}

#[test]
fn generator_optional_super_property_call_short_circuits_and_skips_yield_star_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return 0; }
        class Base {}
        class C extends Base {
          *g() { return super.missing?.(yield* inner()); }
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
fn generator_optional_super_property_computed_member_short_circuits_and_skips_yield_star_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return "k"; }
        class Base {}
        class C extends Base {
          *g() { return super.missing?.[(yield* inner())]; }
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
fn generator_optional_super_property_chain_continuation_skips_yield_star_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return "k"; }
        class Base {}
        class C extends Base {
          *g() { return super.missing?.a[(yield* inner())]; }
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
fn generator_optional_super_property_member_call_short_circuits_and_skips_yield_star_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return 0; }
        class Base {}
        class C extends Base {
          *g() { return super.missing?.a(yield* inner()); }
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
fn generator_optional_super_property_call_binds_this_across_yield_star_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield 0; return 1; }
        let inst;
        class Base {
          m(x) { return this === inst && x === 1; }
        }
        class C extends Base {
          *g() { return super.m?.(yield* inner()); }
        }
        inst = new C();
        const it = inst.g();
        const r1 = it.next();
        const r2 = it.next();
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_call_short_circuits_and_skips_yield_star_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* key() { yield "key"; return "missing"; }
        function* arg() { yield "should-not-yield"; return 0; }

        class Base {}
        class C extends Base {
          *g() { return super[yield* key()]?.(yield* arg()); }
        }

        const it = (new C()).g();
        const r1 = it.next();
        const r2 = it.next();

        r1.value === "key" && r1.done === false &&
        r2.value === undefined && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_call_binds_this_across_yield_star_in_key_and_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* key() { yield "key"; return "m"; }
        function* arg() { yield 0; return 1; }

        let inst;
        class Base {
          m(x) { return this === inst && x === 1; }
        }
        class C extends Base {
          *g() { return super[yield* key()]?.(yield* arg()); }
        }

        inst = new C();
        const it = inst.g();
        const r1 = it.next();
        const r2 = it.next();
        const r3 = it.next();

        r1.value === "key" && r1.done === false &&
        r2.value === 0 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_yield_star_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let ok = true;

      function* inner0() { yield "should-not-yield"; return 0; }
      class Base0 {}
      class C0 extends Base0 {
        *g() { return super.missing?.(yield* inner0()); }
      }
      const r0 = (new C0()).g().next();
      ok = ok && r0.value === undefined && r0.done === true;

      function* inner1() { yield "should-not-yield"; return "k"; }
      class Base1 {}
      class C1 extends Base1 {
        *g() { return super.missing?.[(yield* inner1())]; }
      }
      const r1 = (new C1()).g().next();
      ok = ok && r1.value === undefined && r1.done === true;

      function* inner2() { yield "should-not-yield"; return "k"; }
      class Base2 {}
      class C2 extends Base2 {
        *g() { return super.missing?.a[(yield* inner2())]; }
      }
      const r2 = (new C2()).g().next();
      ok = ok && r2.value === undefined && r2.done === true;

      function* inner3() { yield "should-not-yield"; return 0; }
      class Base3 {}
      class C3 extends Base3 {
        *g() { return super.missing?.a(yield* inner3()); }
      }
      const r3 = (new C3()).g().next();
      ok = ok && r3.value === undefined && r3.done === true;

      function* inner4() { yield 0; return 1; }
      let inst;
      class Base4 {
        m(x) { return this === inst && x === 1; }
      }
      class C4 extends Base4 {
        *g() { return super.m?.(yield* inner4()); }
      }
      inst = new C4();
      const it4 = inst.g();
      const r4_1 = it4.next();
      const r4_2 = it4.next();
      ok = ok && r4_1.value === 0 && r4_1.done === false &&
        r4_2.value === true && r4_2.done === true;

      function* key5() { yield "key"; return "missing"; }
      function* arg5() { yield "should-not-yield"; return 0; }
      class Base5 {}
      class C5 extends Base5 {
        *g() { return super[yield* key5()]?.(yield* arg5()); }
      }
      const it5 = (new C5()).g();
      const r5_1 = it5.next();
      const r5_2 = it5.next();
      ok = ok && r5_1.value === "key" && r5_1.done === false &&
        r5_2.value === undefined && r5_2.done === true;

      function* key6() { yield "key"; return "m"; }
      function* arg6() { yield 0; return 1; }
      let inst6;
      class Base6 {
        m(x) { return this === inst6 && x === 1; }
      }
      class C6 extends Base6 {
        *g() { return super[yield* key6()]?.(yield* arg6()); }
      }
      inst6 = new C6();
      const it6 = inst6.g();
      const r6_1 = it6.next();
      const r6_2 = it6.next();
      const r6_3 = it6.next();
      ok = ok && r6_1.value === "key" && r6_1.done === false &&
        r6_2.value === 0 && r6_2.done === false &&
        r6_3.value === true && r6_3.done === true;

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
