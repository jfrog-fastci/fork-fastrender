use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_yield_in_binary_expressions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // (yield 1) + 2
        function* add_left() { return (yield 1) + 2; }
        const it1 = add_left();
        const a1 = it1.next();
        const a2 = it1.next(10);
        const ok1 = a1.value === 1 && a1.done === false && a2.value === 12 && a2.done === true;

        // 1 + (yield 2)
        function* add_right() { return 1 + (yield 2); }
        const it2 = add_right();
        const b1 = it2.next();
        const b2 = it2.next(10);
        const ok2 = b1.value === 2 && b1.done === false && b2.value === 11 && b2.done === true;

        // (yield 1) * 2
        function* mul_left() { return (yield 1) * 2; }
        const it3 = mul_left();
        const c1 = it3.next();
        const c2 = it3.next(6);
        const ok3 = c1.value === 1 && c1.done === false && c2.value === 12 && c2.done === true;

        // (yield 2) ** 3
        function* exp_left() { return (yield 2) ** 3; }
        const it4 = exp_left();
        const d1 = it4.next();
        const d2 = it4.next(4);
        const ok4 = d1.value === 2 && d1.done === false && d2.value === 64 && d2.done === true;

        // (yield 1) === (yield 1)
        function* strict_eq_both() { return (yield 1) === (yield 1); }
        const it5 = strict_eq_both();
        const e1 = it5.next();
        // Ensure each yield consumes its own resume value.
        const e2 = it5.next(7);
        const e3 = it5.next(8);
        const ok5 =
          e1.value === 1 && e1.done === false &&
          e2.value === 1 && e2.done === false &&
          e3.value === false && e3.done === true;

        ok1 && ok2 && ok3 && ok4 && ok5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_expressions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // x = (yield 1)
        function* assign_rhs() {
          let x = 0;
          const r = (x = (yield 1));
          return r === 10 && x === 10;
        }
        const it1 = assign_rhs();
        const a1 = it1.next();
        const a2 = it1.next(10);
        const ok1 = a1.value === 1 && a1.done === false && a2.value === true && a2.done === true;

        // x += (yield 1)
        function* add_assign_rhs() {
          let x = 5;
          const r = (x += (yield 1));
          return r === 12 && x === 12;
        }
        const it2 = add_assign_rhs();
        const b1 = it2.next();
        const b2 = it2.next(7);
        const ok2 = b1.value === 1 && b1.done === false && b2.value === true && b2.done === true;

        // x **= (yield 1)
        function* exp_assign_rhs() {
          let x = 2;
          const r = (x **= (yield 1));
          return r === 8 && x === 8;
        }
        const it3 = exp_assign_rhs();
        const c1 = it3.next();
        const c2 = it3.next(3);
        const ok3 = c1.value === 1 && c1.done === false && c2.value === true && c2.done === true;

        // o[(yield "a")] = 1
        function* member_assign_key() {
          const o = {};
          o[(yield "a")] = 1;
          return o.b === 1 && o.a === undefined;
        }
        const it4 = member_assign_key();
        const d1 = it4.next();
        const d2 = it4.next("b");
        const ok4 = d1.value === "a" && d1.done === false && d2.value === true && d2.done === true;

        // o[(yield "a")] += 2
        function* member_add_assign_key() {
          const o = { b: 1 };
          const r = (o[(yield "a")] += 2);
          return r === 3 && o.b === 3 && o.a === undefined;
        }
        const it5 = member_add_assign_key();
        const e1 = it5.next();
        const e2 = it5.next("b");
        const ok5 = e1.value === "a" && e1.done === false && e2.value === true && e2.done === true;

        ok1 && ok2 && ok3 && ok4 && ok5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_update_expressions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // o[(yield "a")]++
        function* member_postfix_update_key() {
          const o = { b: 1 };
          const r = o[(yield "a")]++;
          return r === 1 && o.b === 2 && o.a === undefined;
        }
        const it1 = member_postfix_update_key();
        const a1 = it1.next();
        const a2 = it1.next("b");
        const ok1 = a1.value === "a" && a1.done === false && a2.value === true && a2.done === true;

        // ++o[(yield "a")]
        function* member_prefix_update_key() {
          const o = { b: 1 };
          const r = ++o[(yield "a")];
          return r === 2 && o.b === 2 && o.a === undefined;
        }
        const it2 = member_prefix_update_key();
        const b1 = it2.next();
        const b2 = it2.next("b");
        const ok2 = b1.value === "a" && b1.done === false && b2.value === true && b2.done === true;

        ok1 && ok2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_new_and_delete_expressions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // new C(yield 1)
        function C(x) { this.x = x; }
        function* ctor_arg() {
          const o = new C(yield 1);
          return o.x === 42;
        }
        const it1 = ctor_arg();
        const a1 = it1.next();
        const a2 = it1.next(42);
        const ok1 = a1.value === 1 && a1.done === false && a2.value === true && a2.done === true;

        // delete o[(yield "a")]
        function* delete_member_key() {
          const o = { b: 1 };
          const r = delete o[(yield "a")];
          return r === true && !("b" in o) && !("a" in o);
        }
        const it2 = delete_member_key();
        const b1 = it2.next();
        const b2 = it2.next("b");
        const ok2 = b1.value === "a" && b1.done === false && b2.value === true && b2.done === true;

        ok1 && ok2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
