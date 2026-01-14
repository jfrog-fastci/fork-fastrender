use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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

