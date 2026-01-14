use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_optional_chain_nullish_base_skips_yield_star_in_call_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return 0; }
        function* g() {
          var r = null?.(yield* inner());
          return r === undefined;
        }
        var it = g();
        var r = it.next();
        r.value === true && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_nullish_base_skips_yield_star_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return "k"; }
        function* g() {
          var r = null?.[(yield* inner())];
          return r === undefined;
        }
        var it = g();
        var r = it.next();
        r.value === true && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_continuation_propagates_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner(v) { yield 0; return v; }
        function* g(v) { const o = yield* inner(v); return o?.a; }

        const it1 = g(null);
        const r1 = it1.next();
        const r2 = it1.next();

        const it2 = g({ a: 1 });
        it2.next();
        const r3 = it2.next();

        r1.value === 0 && r1.done === false &&
        r2.value === undefined && r2.done === true &&
        r3.value === 1 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_parenthesized_call_after_computed_optional_chain_base_short_circuit_skips_yield_star_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should-not-yield"; return "k"; }
        function* g() {
          const o = (yield 0);
          try {
            // The optional chain short-circuits when `o` is nullish, so the computed key expression
            // (including its `yield*`) must not run. The call is outside the optional chain due to
            // parentheses, so it should still throw.
            return (o?.a[(yield* inner())])();
          } catch (e) {
            return e.name;
          }
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next(null);

        r1.value === 0 && r1.done === false &&
        r2.value === "TypeError" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

