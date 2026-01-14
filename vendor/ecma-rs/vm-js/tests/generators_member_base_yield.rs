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

#[test]
fn generator_postfix_decrement_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100 };
      var resumed = { a: 10 };
      function* g() {
        var r = (yield yielded).a--;
        return r;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 10 && r2.done === true &&
      resumed.a === 9 &&
      yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_prefix_update_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100 };
      var resumed = { a: 10 };
      function* g() {
        var r = ++(yield yielded).a;
        return r;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 11 && r2.done === true &&
      resumed.a === 11 &&
      yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_prefix_decrement_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100 };
      var resumed = { a: 10 };
      function* g() {
        var r = --(yield yielded).a;
        return r;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 9 && r2.done === true &&
      resumed.a === 9 &&
      yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_update_computed_member_base_yield_evaluates_key_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100, b: 10 };
      var resumed = { a: 1, b: 10 };
      var k = "a";
      function* g() {
        var r = (yield yielded)[k]++;
        return r;
      }
      var it = g();
      var r1 = it.next();
      // The computed key expression should not run until after the `yield` expression resumes, so
      // updating `k` here must affect which property is incremented.
      k = "b";
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 10 && r2.done === true &&
      resumed.b === 11 && resumed.a === 1 &&
      yielded.b === 10 && yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_decrement_computed_member_base_yield_evaluates_key_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var yielded = { a: 100, b: 10 };
      var resumed = { a: 1, b: 10 };
      var k = "a";
      function* g() {
        var r = (yield yielded)[k]--;
        return r;
      }
      var it = g();
      var r1 = it.next();
      // The computed key expression should not run until after the `yield` expression resumes, so
      // updating `k` here must affect which property is decremented.
      k = "b";
      var r2 = it.next(resumed);
      r1.value === yielded && r1.done === false &&
      r2.value === 10 && r2.done === true &&
      resumed.b === 9 && resumed.a === 1 &&
      yielded.b === 10 && yielded.a === 100
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
