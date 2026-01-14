use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force frequent GC during generator suspension/resumption so missing roots in continuation
  // frames manifest as stale handles.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_yield_in_binary_addition_left() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 1) + 2; }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(10);
        r1.value === 1 && r1.done === false &&
        r2.value === 12 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_addition_right() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return 1 + (yield 2); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(10);
        r1.value === 2 && r1.done === false &&
        r2.value === 11 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_multiplication_left() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 1) * 2; }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(6);
        r1.value === 1 && r1.done === false &&
        r2.value === 12 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_exponentiation_left() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 2) ** 3; }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(4);
        r1.value === 2 && r1.done === false &&
        r2.value === 64 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_strict_equality_both() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 1) === (yield 1); }
        const it = g();
        const r1 = it.next();
        // Ensure each yield consumes its own resume value.
        const r2 = it.next(7);
        const r3 = it.next(8);
        r1.value === 1 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === false && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 0;
          const r = (x = (yield 1));
          return r === 10 && x === 10;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(10);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_addition_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 5;
          const r = (x += (yield 1));
          return r === 12 && x === 12;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(7);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_exponentiation_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 2;
          const r = (x **= (yield 1));
          return r === 8 && x === 8;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(3);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_assignment_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = {};
          const r = (o[(yield "a")] = 1);
          return r === 1 && o.b === 1 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_addition_assignment_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = (o[(yield "a")] += 2);
          return r === 3 && o.b === 3 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_postfix_update_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = o[(yield "a")]++;
          return r === 1 && o.b === 2 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_prefix_update_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = ++o[(yield "a")];
          return r === 2 && o.b === 2 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_new_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function C(x) { this.x = x; }
        function* g() {
          const o = new C(yield 1);
          return o.x === 42;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_delete_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = delete o[(yield "a")];
          return r === true && !("b" in o) && !("a" in o);
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_template_literals() {
  let mut rt = new_runtime_with_frequent_gc();
  let value = rt
    .exec_script(
      r#"
        function churn() {
          // Allocate enough to force GC under the small `gc_threshold`.
          let junk = [];
          for (let i = 0; i < 200; i++) {
            junk.push(new Uint8Array(1024));
          }
          return junk.length;
        }

        // `a${yield 1}b`
        function* tpl_simple() { return `a${yield 1}b`; }
        const it1 = tpl_simple();
        const a1 = it1.next();
        churn();
        const a2 = it1.next(10);
        const ok1 = a1.value === 1 && a1.done === false && a2.value === "a10b" && a2.done === true;

        // Ensure substitution evaluation is left-to-right and each `yield` consumes its own resume value.
        function* tpl_multi() { return `x${yield 1}y${yield 2}z`; }
        const it2 = tpl_multi();
        const b1 = it2.next();
        churn();
        const b2 = it2.next("A");
        churn();
        const b3 = it2.next("B");
        const ok2 =
          b1.value === 1 && b1.done === false &&
          b2.value === 2 && b2.done === false &&
          b3.value === "xAyBz" && b3.done === true;

        // Yield inside a larger substitution expression.
        function* tpl_nested() { return `a${1 + (yield 2)}b`; }
        const it3 = tpl_nested();
        const c1 = it3.next();
        churn();
        const c2 = it3.next(10);
        const ok3 = c1.value === 2 && c1.done === false && c2.value === "a11b" && c2.done === true;

        // ToString is applied to substitution results (Symbol should throw).
        function* tpl_symbol() { return `${yield 1}`; }
        const it4 = tpl_symbol();
        it4.next();
        churn();
        let ok4 = false;
        try {
          it4.next(Symbol("s"));
        } catch (e) {
          ok4 = e instanceof TypeError;
        }

        ok1 && ok2 && ok3 && ok4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
