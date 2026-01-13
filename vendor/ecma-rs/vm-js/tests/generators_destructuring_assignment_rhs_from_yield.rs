use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_assignment_rhs_from_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var assigned;
      function* g() {
        var a;
        assigned = ({a} = yield 0);
        return a;
      }
      var it = g();
      var r1 = it.next();
      var obj = {a: 5};
      var r2 = it.next(obj);
      r1.value === 0 && r1.done === false &&
      r2.value === 5 && r2.done === true &&
      assigned === obj
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_rhs_from_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var assigned;
      function* g() {
        var a;
        assigned = ([a] = yield 0);
        return a;
      }
      var it = g();
      var r1 = it.next();
      var arr = [5];
      var r2 = it.next(arr);
      r1.value === 0 && r1.done === false &&
      r2.value === 5 && r2.done === true &&
      assigned === arr
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

