use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Generator execution and parser setup can allocate a fair bit; keep a slightly larger heap
  // to avoid spurious OOMs in CI.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_delete_optional_chain_short_circuits_to_true_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var o = (yield null);
        return delete o?.x;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_optional_chain_computed_member_does_not_evaluate_key_when_nullish_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var log = 0;
        var o = (yield null);
        var res = delete o?.[log = 1];
        return res === true && log === 0;
      }
      var it = g();
      it.next();
      var r = it.next(null);
      r.value === true && r.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_super_computed_member_evaluates_key_and_to_property_key_before_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var side = 0;
      class C {
        *del() {
          try {
            delete super[(yield (side = 1, "yielded"))];
            return "no";
          } catch (e) {
            return String(side) + ":" + e.name;
          }
        }
      }
      var it = new C().del();
      var r1 = it.next();
      var key = { toString() { side = side + 1; return "m"; } };
      var r2 = it.next(key);
      r1.value === "yielded" && r1.done === false &&
      side === 2 &&
      r2.value === "2:ReferenceError" && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

