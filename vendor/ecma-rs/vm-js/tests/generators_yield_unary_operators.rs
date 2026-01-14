use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_yield_in_unary_plus() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return +(yield 1); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_unary_minus() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return -(yield 1); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.value === -2 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_unary_not() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return !(yield true); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(false);
        r1.value === true && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_unary_bitwise_not() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return ~(yield 1); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(0);
        r1.value === 1 && r1.done === false &&
        r2.value === -1 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_unary_typeof() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return typeof (yield 1); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("x");
        r1.value === 1 && r1.done === false &&
        r2.value === "string" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_unary_void() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return void (yield 1); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(123);
        r1.value === 1 && r1.done === false &&
        r2.value === undefined && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

