use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_for_of_assignment_target_computed_member_key_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var o = {};
            for (o[yield 1] of [42]) {}
            return o.x;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("x");
          return r2.done === true && r2.value === 42;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_rest_assignment_target_computed_member_key_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var o = {};
            ([...o[yield 1]] = [1, 2, 3]);
            return o.x.length === 3 && o.x[0] === 1 && o.x[2] === 3;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("x");
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

