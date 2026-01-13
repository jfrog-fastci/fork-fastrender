use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_optional_chaining_short_circuits_chain_continuation_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = true;
        function* g() { return (yield null)?.a.b; }

        var it = g();
        var r1 = it.next();
        ok = ok && (r1.done === false) && (r1.value === null);

        try {
          var r2 = it.next();
          ok = ok && (r2.done === true) && (r2.value === undefined);
        } catch (e) {
          ok = false;
        }

        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chaining_short_circuits_call_continuation_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = true;
        function* g() { return (yield null)?.b.c(); }

        var it = g();
        var r1 = it.next();
        ok = ok && (r1.done === false) && (r1.value === null);

        try {
          var r2 = it.next();
          ok = ok && (r2.done === true) && (r2.value === undefined);
        } catch (e) {
          ok = false;
        }

        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_parenthesized_callee_does_not_bind_this_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = true;
        var obj = { m: function () { 'use strict'; return this === undefined; } };

        function* g(o) { return ((yield o).m)(); }

        var it = g(obj);
        var r1 = it.next();
        ok = ok && (r1.done === false) && (r1.value === obj);

        var r2 = it.next(obj);
        ok = ok && (r2.done === true) && (r2.value === true);

        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
