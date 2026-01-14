use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_then_postfix_update_identifier() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 1;
          // Force a suspension before the update expression so it runs in the continuation path.
          const r = (yield 0, x++);
          return r === 1 && x === 2;
        }
        const it = g();
        const a1 = it.next();
        const a2 = it.next();
        a1.value === 0 && a1.done === false &&
        a2.value === true && a2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_then_prefix_update_identifier() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 1;
          const r = (yield 0, ++x);
          return r === 2 && x === 2;
        }
        const it = g();
        const a1 = it.next();
        const a2 = it.next();
        a1.value === 0 && a1.done === false &&
        a2.value === true && a2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_then_postfix_decrement_identifier() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 2;
          const r = (yield 0, x--);
          return r === 2 && x === 1;
        }
        const it = g();
        const a1 = it.next();
        const a2 = it.next();
        a1.value === 0 && a1.done === false &&
        a2.value === true && a2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_then_prefix_decrement_identifier() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 2;
          const r = (yield 0, --x);
          return r === 1 && x === 1;
        }
        const it = g();
        const a1 = it.next();
        const a2 = it.next();
        a1.value === 0 && a1.done === false &&
        a2.value === true && a2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_bigint_update_expression_computed_key_postfix() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const obj = { a: 1n };
          const r = obj[(yield 0)]++;
          return r === 1n && obj.a === 2n;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("a");
        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
