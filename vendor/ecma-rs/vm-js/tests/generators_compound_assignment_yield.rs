use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_compound_assignment_subtraction_rhs_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var x = 5;
            x -= yield 1;
            return x;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(2);
          return r2.done === true && r2.value === 3;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_shift_rhs_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var x = 1;
            x <<= yield 1;
            return x;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(3);
          return r2.done === true && r2.value === 8;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_can_skip_yield_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var x = 0;
            x &&= yield 1;
            return x === 0;
          }
          var it = g();
          var r1 = it.next();
          return r1.done === true && r1.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

