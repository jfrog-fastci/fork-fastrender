use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn exponentiation_assignment_captures_binding_old_value_across_yield() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
      (() => {
        var x = 2;
        function* g() {
          return (x **= (yield 0));
        }

        var it = g();
        var r1 = it.next();

        // Mutate the binding after the `yield` but before resuming. The compound
        // assignment must use the old value captured before the yield (2), not
        // the mutated value (4).
        x = 4;

        var r2 = it.next(3);

        return r1.value === 0 && r1.done === false &&
               r2.value === 8 && r2.done === true &&
               x === 8;
      })()
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn exponentiation_assignment_captures_property_reference_and_old_value_across_yield() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
      (() => {
        var o1 = { a: 2 };
        var o = o1;
        function* g() {
          return (o.a **= (yield 0));
        }

        var it = g();
        var r1 = it.next();

        // Mutate the old target and also rebind `o` after the `yield` but
        // before resuming. The compound assignment must operate on the original
        // reference (o1.a) and the old value (2), not the mutated value (4) nor
        // the rebound base object ({a: 5}).
        o1.a = 4;
        o = { a: 5 };

        var r2 = it.next(3);

        return r1.value === 0 && r1.done === false &&
               r2.value === 8 && r2.done === true &&
               o1.a === 8 &&
               o.a === 5;
      })()
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

