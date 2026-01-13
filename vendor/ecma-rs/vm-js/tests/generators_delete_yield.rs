use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_delete_optional_chain_short_circuits_on_nullish_base_yield_supplies_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var o = yield 0;
        return delete o?.x;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false && r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_optional_chain_computed_member_does_not_evaluate_key_when_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var o = yield 0;
        var ok = delete o?.[side = 1];
        return ok === true && side === 0;
      }
      var it = g();
      it.next();
      var r = it.next(null);
      r.done === true && r.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_super_computed_member_evaluates_key_and_to_property_key_before_throwing() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var side = 0;
      class B {}
      class C extends B {
        *g() {
          try {
            delete super[(yield { toString(){ side = 1; return "m"; } })];
            return false;
          } catch (e) {
            return e.name === "ReferenceError" && side === 1;
          }
        }
      }
      var it = new C().g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r1.done === false && r2.done === true && r2.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

