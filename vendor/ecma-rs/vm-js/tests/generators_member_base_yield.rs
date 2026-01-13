use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_assignment_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { name: "yielded" };
      var resumed = {};
      function* g() {
        (yield yielded).a = 2;
        return 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 0 && r2.done === true &&
      resumed.a === 2 &&
      Object.prototype.hasOwnProperty.call(yielded, "a") === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100 };
      var resumed = { a: 10 };
      function* g() {
        var r = (yield yielded).a += 5;
        return r;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 15 && r2.done === true &&
      resumed.a === 15 &&
      yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_update_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100 };
      var resumed = { a: 10 };
      function* g() {
        var r = (yield yielded).a++;
        return r;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 10 && r2.done === true &&
      resumed.a === 11 &&
      yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

