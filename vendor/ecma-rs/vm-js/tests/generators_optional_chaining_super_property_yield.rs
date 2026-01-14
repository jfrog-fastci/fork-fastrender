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
fn generator_optional_super_property_call_short_circuits_on_nullish_getter_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let side = 0;
        class Base {
          get maybe() { side++; return null; }
        }
        class C extends Base {
          *g() { return super.maybe?.(yield "should-not-yield"); }
        }
        const it = (new C()).g();
        const r = it.next();
        r.value === undefined && r.done === true && side === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_member_short_circuits_on_nullish_getter_and_skips_yield_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let side = 0;
        class Base {
          get maybe() { side++; return null; }
        }
        class C extends Base {
          *g() { return super.maybe?.[(yield "should-not-yield")]; }
        }
        const it = (new C()).g();
        const r = it.next();
        r.value === undefined && r.done === true && side === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_call_evaluates_yield_in_arg_before_throwing_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { get m() { return 0; } }
        class C extends Base {
          *g() {
            try {
              super.m?.(yield 1);
              return "no";
            } catch (e) {
              return e.name;
            }
          }
        }

        const it = (new C()).g();
        const r1 = it.next();
        const r2 = it.next(0);

        r1.value === 1 && r1.done === false &&
        r2.value === "TypeError" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        class C extends Base {
          *g() { return super[(yield "key")]?.(yield "should-not-yield-arg"); }
        }

        const it = (new C()).g();
        const r1 = it.next();
        const r2 = it.next("missing");

        r1.value === "key" && r1.done === false &&
        r2.value === undefined && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_call_evaluates_yield_in_arg_before_throwing_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { get m() { return 0; } }
        class C extends Base {
          *g() {
            try {
              super[(yield "key")]?.(yield 1);
              return "no";
            } catch (e) {
              return e.name;
            }
          }
        }

        const it = (new C()).g();
        const r1 = it.next();
        const r2 = it.next("m");
        const r3 = it.next(0);

        r1.value === "key" && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === "TypeError" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_super_property_computed_call_binds_this_across_yield_in_key_and_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let inst;
        class Base {
          m(x) { return this === inst && x === 1; }
        }
        class C extends Base {
          *g() { return super[(yield "key")]?.(yield 0); }
        }

        inst = new C();
        const it = inst.g();
        const r1 = it.next();
        const r2 = it.next("m");
        const r3 = it.next(1);

        r1.value === "key" && r1.done === false &&
        r2.value === 0 && r2.done === false &&
        r3.value === true && r3.done === true
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

      class Base5 {}
      class C5 extends Base5 {
        *g() { return super[(yield "key")]?.(yield "should-not-yield-arg"); }
      }
      const it5 = (new C5()).g();
      const r5_1 = it5.next();
      const r5_2 = it5.next("missing");
      ok = ok && r5_1.value === "key" && r5_1.done === false &&
        r5_2.value === undefined && r5_2.done === true;

      let inst6;
      class Base6 {
        m(x) { return this === inst6 && x === 1; }
      }
      class C6 extends Base6 {
        *g() { return super[(yield "key")]?.(yield 0); }
      }
      inst6 = new C6();
      const it6 = inst6.g();
      const r6_1 = it6.next();
      const r6_2 = it6.next("m");
      const r6_3 = it6.next(1);
      ok = ok && r6_1.value === "key" && r6_1.done === false &&
        r6_2.value === 0 && r6_2.done === false &&
        r6_3.value === true && r6_3.done === true;

      class Base7 { get m() { return 0; } }
      class C7 extends Base7 {
        *g() {
          try {
            super.m?.(yield 1);
            return "no";
          } catch (e) {
            return e.name;
          }
        }
      }
      const it7 = (new C7()).g();
      const r7_1 = it7.next();
      const r7_2 = it7.next(0);
      ok = ok && r7_1.value === 1 && r7_1.done === false &&
        r7_2.value === "TypeError" && r7_2.done === true;

      class Base8 { get m() { return 0; } }
      class C8 extends Base8 {
        *g() {
          try {
            super[(yield "key")]?.(yield 1);
            return "no";
          } catch (e) {
            return e.name;
          }
        }
      }
      const it8 = (new C8()).g();
      const r8_1 = it8.next();
      const r8_2 = it8.next("m");
      const r8_3 = it8.next(0);
      ok = ok && r8_1.value === "key" && r8_1.done === false &&
        r8_2.value === 1 && r8_2.done === false &&
        r8_3.value === "TypeError" && r8_3.done === true;

      let side9 = 0;
      class Base9 { get maybe() { side9++; return null; } }
      class C9 extends Base9 {
        *g() { return super.maybe?.(yield "should-not-yield"); }
      }
      const r9 = (new C9()).g().next();
      ok = ok && r9.value === undefined && r9.done === true && side9 === 1;

      let side10 = 0;
      class Base10 { get maybe() { side10++; return null; } }
      class C10 extends Base10 {
        *g() { return super.maybe?.[(yield "should-not-yield")]; }
      }
      const r10 = (new C10()).g().next();
      ok = ok && r10.value === undefined && r10.done === true && side10 === 1;

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
