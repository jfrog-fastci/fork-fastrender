use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Generator execution and parser setup can allocate a fair bit; keep a slightly larger heap
  // to avoid spurious OOMs in CI.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_delete_optional_chain_short_circuits_to_true_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var o = (yield null);
        return delete o?.x;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_optional_chain_computed_member_does_not_evaluate_key_when_nullish_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var log = 0;
        var o = (yield null);
        var res = delete o?.[log = 1];
        return res === true && log === 0;
      }
      var it = g();
      it.next();
      var r = it.next(null);
      r.value === true && r.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_optional_chain_computed_member_skips_yield_in_key_when_nullish_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var o = (yield null);
        return delete o?.[(yield "should-not-yield")];
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_optional_chain_computed_member_evaluates_yield_in_key_when_not_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var o = { a: 1 };
        var r = delete o?.[(yield "yielded")];
        return r === true && !("a" in o);
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("a");
      r1.value === "yielded" && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_optional_chain_propagates_and_skips_yield_in_key_after_base_short_circuit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        // If the base short-circuits (`(yield null)` resumes to null), the entire optional-chain
        // expression becomes non-reference and `delete` must return true without evaluating any
        // following member operations (including yields in computed keys).
        return delete (yield null)?.x[(yield "should-not-yield")];
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_member_throws_in_strict_mode_when_not_configurable_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(o) {
        'use strict';
        try {
          delete (yield o).x;
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      var o = {};
      Object.defineProperty(o, "x", { value: 1, configurable: false });
      var it = g(o);
      var r1 = it.next();
      var r2 = it.next(o);
      r1.value === o && r1.done === false &&
      r2.value === "TypeError" && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_computed_member_throws_in_strict_mode_when_not_configurable_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(o) {
        'use strict';
        try {
          return delete o[(yield "yielded")];
        } catch (e) {
          return e.name;
        }
      }
      var o = {};
      Object.defineProperty(o, "x", { value: 1, configurable: false });
      var it = g(o);
      var r1 = it.next();
      var r2 = it.next("x");
      r1.value === "yielded" && r1.done === false &&
      r2.value === "TypeError" && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_delete_super_computed_member_evaluates_key_and_to_property_key_before_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var side = 0;
      class C {
        *del() {
          try {
            delete super[(yield (side = 1, "yielded"))];
            return "no";
          } catch (e) {
            return String(side) + ":" + e.name;
          }
        }
      }
      var it = new C().del();
      var r1 = it.next();
      var key = { toString() { side = side + 1; return "m"; } };
      var r2 = it.next(key);
      r1.value === "yielded" && r1.done === false &&
      side === 2 &&
      r2.value === "2:ReferenceError" && r2.done === true
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}
