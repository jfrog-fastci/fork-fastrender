use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_array_literal_yield_multiple_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ (yield 1), (yield 2) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(20);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true &&
      Array.isArray(r3.value) &&
      r3.value.length === 2 &&
      r3.value[0] === 10 &&
      r3.value[1] === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ ...(yield 0), 4 ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next([1, 2, 3]);
      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 4 &&
      r2.value[0] === 1 &&
      r2.value[1] === 2 &&
      r2.value[2] === 3 &&
      r2.value[3] === 4
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_elision_and_yield_preserves_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ , (yield 1) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("x");
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 2 &&
      r2.value[1] === "x" &&
      Object.prototype.hasOwnProperty.call(r2.value, 0) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_throw_aborts_remaining_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ (yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var threw = false;
      try { it.throw("boom"); } catch (e) { threw = (e === "boom"); }
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      threw === true &&
      ran === false &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_return_aborts_remaining_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ (yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.return(99);
      var r3 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 99 && r2.done === true &&
      ran === false &&
      r3.done === true && r3.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_throw_aborts_remaining_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ ...(yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var threw = false;
      try { it.throw("boom"); } catch (e) { threw = (e === "boom"); }
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      threw === true &&
      ran === false &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_return_aborts_remaining_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ ...(yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.return(99);
      var r3 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 99 && r2.done === true &&
      ran === false &&
      r3.done === true && r3.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
