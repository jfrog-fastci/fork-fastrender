use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_compound_assignment_captures_old_value_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = { a: 1 };
      function* g() {
        o.a += (yield 0);
        return o.a;
      }
      var it = g();
      var r1 = it.next();
      // Mutate after the yield but before resuming.
      o.a = 100;
      var r2 = it.next(2);
      r1.value === 0 && r1.done === false &&
      r2.value === 3 && r2.done === true &&
      // Must use the pre-yield old value (1), not the mutated value (100).
      o.a === 3
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_captures_base_and_key_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o1 = { a: 1, b: 10 };
      var o2 = { a: 100, b: 1000 };
      var o = o1;
      var k = "a";

      function* g() {
        o[k] += (yield 0);
      }

      var it = g();
      var r1 = it.next();

      // Rebind both the base and the key after the yield.
      o = o2;
      k = "b";

      var r2 = it.next(2);

      r1.value === 0 && r1.done === false &&
      r2.value === undefined && r2.done === true &&
      // The assignment must still target the original base/key pair.
      o1.a === 3 && o1.b === 10 &&
      o2.a === 100 && o2.b === 1000
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

