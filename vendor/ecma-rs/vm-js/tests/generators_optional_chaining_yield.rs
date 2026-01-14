use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_optional_chain_computed_member_propagates_short_circuit_and_skips_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var r = (yield 0)?.x[(side++, "toString")];
        return r === undefined && side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_call_computed_member_propagates_short_circuit_and_skips_key_and_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var arg_side = 0;
        function arg() { arg_side++; return 0; }

        var r = (yield 0)?.x[(side++, "toString")](arg());
        return r === undefined && side === 0 && arg_side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_computed_member_does_not_evaluate_key_when_base_is_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var r = (yield 0)?.[(side++, "toString")];
        return r === undefined && side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

