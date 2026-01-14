use vm_js::{GcObject, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn get_prop(rt: &mut JsRuntime, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(obj))?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut host,
    &mut hooks,
    obj,
    key,
    Value::Object(obj),
  )
}

fn call_next(rt: &mut JsRuntime, it: GcObject, arg: Option<Value>) -> Result<Value, VmError> {
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(it))?;

  let key_s = scope.alloc_string("next")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let next = scope.get_with_host_and_hooks(
    &mut rt.vm,
    &mut host,
    &mut hooks,
    it,
    key,
    Value::Object(it),
  )?;
  scope.push_root(next)?;
  let args = arg.map_or(Vec::new(), |v| vec![v]);
  rt.vm
    .call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, next, Value::Object(it), &args)
}

#[test]
fn generator_binary_addition_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) + (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);  // left = 10, now yields RHS prompt 2
      var r3 = it.next(20);  // right = 20
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 30
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_addition_string_concatenation_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield "a") + (yield "b"); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10); // left = 10
      var r3 = it.next("x"); // right = "x" -> string concatenation
      r1.value === "a" && r1.done === false &&
      r2.value === "b" && r2.done === false &&
      r3.done === true && r3.value === "10x"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_addition_to_primitive_happens_after_rhs_evaluation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){
        var log = [];
        var o = { valueOf() { log.push("valueOf"); return 10; } };
        Object.defineProperty(globalThis, "x", { get() { log.push("get x"); return 1; }, configurable: true });
        var r = (yield o) + x;
        delete globalThis.x;
        return r === 11 && log.length === 2 && log[0] === "get x" && log[1] === "valueOf";
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r1.done === false && r2.done === true && r2.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_addition_to_primitive_happens_after_rhs_yield_and_is_left_to_right() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      function* g(){
        var lhs = { valueOf() { log.push("lhs"); return 10; } };
        var rhs = { valueOf() { log.push("rhs"); return 20; } };
        return (yield lhs) + (yield rhs);
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      // `+` must not call `ToPrimitive` on the left operand until after the RHS yield.
      var ok_mid = r1.done === false && r2.done === false && log.length === 0;
      var r3 = it.next(r2.value);
      ok_mid &&
      r3.done === true && r3.value === 30 &&
      log.length === 2 && log[0] === "lhs" && log[1] === "rhs"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_multiplication_yield_on_lhs_only() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) * 2; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(3);
      r1.value === 1 && r1.done === false &&
      r2.done === true && r2.value === 6
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_multiplication_to_numeric_happens_after_rhs_yield_and_is_left_to_right() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      function* g(){
        var lhs = { valueOf() { log.push("lhs"); return 6; } };
        var rhs = { valueOf() { log.push("rhs"); return 7; } };
        return (yield lhs) * (yield rhs);
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      // `*` must not call `ToNumeric` on the left operand until after the RHS yield.
      var ok_mid = r1.done === false && r2.done === false && log.length === 0;
      var r3 = it.next(r2.value);
      ok_mid &&
      r3.done === true && r3.value === 42 &&
      log.length === 2 && log[0] === "lhs" && log[1] === "rhs"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_subtraction_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) - (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(3);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 7
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_subtraction_to_numeric_happens_after_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      var lhs = { valueOf() { log.push("lhs"); return 10; } };
      var rhs = { valueOf() { log.push("rhs"); return 1; } };
      function* g(){ return (yield lhs) - (yield rhs); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(lhs);
      // Must not coerce the left operand before yielding RHS.
      var ok_mid = log.length === 0 && r2.value === rhs && r2.done === false;
      var r3 = it.next(rhs);
      ok_mid &&
      r1.value === lhs && r1.done === false &&
      r3.done === true && r3.value === 9 &&
      log.length === 2 && log[0] === "lhs" && log[1] === "rhs"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_division_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) / (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(20);
      var r3 = it.next(4);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_addition_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1n) + (yield 2n); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10n);
      var r3 = it.next(20n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.done === true && r3.value === 30n
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_division_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1n) / (yield 2n); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(20n);
      var r3 = it.next(6n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.done === true && r3.value === 3n
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_mixing_error_is_catchable_after_both_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){
        try { return (yield 1n) + (yield 2); }
        catch (e) { return e && e.name === 'TypeError'; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(1n);
      var r3 = it.next(1);
      r1.value === 1n && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_remainder_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) % (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(20);
      var r3 = it.next(6);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_exponentiation_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) ** (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      var r3 = it.next(3);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 8
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_strict_equality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) === (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(5);
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_strict_equality_roots_left_value_across_rhs_yield_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let it = rt.exec_script("function* g(){ return (yield {}) === (yield 0); } g();")?;
  let Value::Object(it) = it else {
    panic!("expected generator iterator object, got {it:?}");
  };

  // Keep the generator object alive across the explicit GC below.
  let it_root = rt.heap_mut().add_root(Value::Object(it))?;

  // First yield returns an object.
  let r1 = call_next(&mut rt, it, None)?;
  let Value::Object(r1) = r1 else {
    panic!("expected IteratorResult object, got {r1:?}");
  };
  assert_eq!(get_prop(&mut rt, r1, "done")?, Value::Bool(false));
  let left_obj = get_prop(&mut rt, r1, "value")?;
  // Keep the yielded object alive until we've resumed the generator once; after that, the only
  // remaining reference should be the generator continuation's captured left operand.
  let left_root = rt.heap_mut().add_root(left_obj)?;

  // Resume generator: capture left operand (the object), then yield the RHS prompt.
  let r2 = call_next(&mut rt, it, Some(left_obj))?;
  let Value::Object(r2) = r2 else {
    panic!("expected IteratorResult object, got {r2:?}");
  };
  assert_eq!(get_prop(&mut rt, r2, "done")?, Value::Bool(false));
  assert_eq!(get_prop(&mut rt, r2, "value")?, Value::Number(0.0));
  rt.heap_mut().remove_root(left_root);

  // While the generator is suspended on the RHS yield, force a GC cycle. The generator continuation
  // must keep the captured left operand alive.
  rt.heap_mut().collect_garbage();

  // Resume RHS yield with the same object so `===` returns true.
  let r3 = call_next(&mut rt, it, Some(left_obj))?;
  let Value::Object(r3) = r3 else {
    panic!("expected IteratorResult object, got {r3:?}");
  };
  assert_eq!(get_prop(&mut rt, r3, "done")?, Value::Bool(true));
  assert_eq!(get_prop(&mut rt, r3, "value")?, Value::Bool(true));

  rt.heap_mut().remove_root(it_root);
  Ok(())
}

#[test]
fn generator_binary_relational_comparison_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) < (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(1);
      var r3 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_abstract_equality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) == (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("5");
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_strict_inequality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) !== (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      var r3 = it.next(0);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_abstract_inequality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) != (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("5");
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_greater_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) > (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      var r3 = it.next(1);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_greater_preserves_to_primitive_order_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      var lhs = { valueOf() { log.push("lhs"); return 10; } };
      var rhs = { valueOf() { log.push("rhs"); return 5; } };
      function* g(){ return (yield lhs) > (yield rhs); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(lhs);
      // `>` is specified via AbstractRelationalComparison with swapped args; conversion order must
      // still be left-to-right and must not happen before the RHS yield.
      var ok_mid = log.length === 0 && r2.value === rhs && r2.done === false;
      var r3 = it.next(rhs);
      ok_mid &&
      r1.value === lhs && r1.done === false &&
      r3.done === true && r3.value === true &&
      log.length === 2 && log[0] === "lhs" && log[1] === "rhs"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_less_than_or_equal_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) <= (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(3);
      var r3 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_greater_than_or_equal_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) >= (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(5);
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_nested_expression_preserves_outer_left_across_inner_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) + ((yield 2) * (yield 3)); }
      var it = g();
      var r1 = it.next();      // yield outer LHS prompt 1
      var r2 = it.next(10);    // outer left = 10, now evaluating RHS -> yields prompt 2
      var r3 = it.next(5);     // mul left = 5, now yields prompt 3
      var r4 = it.next(2);     // mul right = 2
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_operator_precedence_with_multiple_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `*` has higher precedence than `+`: a + (b * c)
      function* g(){ return (yield 1) + (yield 2) * (yield 3); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(5);
      var r4 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_exponentiation_is_right_associative_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `**` is right-associative: a ** (b ** c)
      function* g(){ return (yield 1) ** (yield 2) ** (yield 3); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      var r3 = it.next(3);
      var r4 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 512
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_exponentiation_is_right_associative_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `**` is right-associative for BigInt too: a ** (b ** c)
      function* g(){ return (yield 1n) ** (yield 2n) ** (yield 3n); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2n);
      var r3 = it.next(3n);
      var r4 = it.next(2n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.value === 3n && r3.done === false &&
      r4.done === true && r4.value === 512n
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_negative_exponent_throws_range_error_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){
        try { return (yield 1n) ** (yield 2n); }
        catch (e) { return e && e.name === 'RangeError'; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2n);
      var r3 = it.next(-1n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_in_operator_rhs_non_object_error_is_catchable_after_yields_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){
        try { return (yield "a") in (yield 0); }
        catch (e) { return e && e.name === "TypeError"; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("a");
      var r3 = it.next(null);
      r1.value === "a" && r1.done === false &&
      r2.value === 0 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_instanceof_rhs_non_object_error_is_catchable_after_yields_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){
        try { return (yield 1) instanceof (yield 0); }
        catch (e) { return e && e.name === "TypeError"; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next({});
      var r3 = it.next(null);
      r1.value === 1 && r1.done === false &&
      r2.value === 0 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_in_operator_to_property_key_happens_after_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      function* g(){
        var key = { toString() { log.push("toString"); return "a"; } };
        var obj = { a: 1 };
        return (yield key) in (yield obj);
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      var ok_mid =
        r1.done === false && r1.value &&
        r2.done === false && r2.value && r2.value.a === 1 &&
        log.length === 0;
      var r3 = it.next(r2.value);
      ok_mid &&
      r3.done === true && r3.value === true &&
      log.length === 1 && log[0] === "toString"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_in_operator_rhs_type_error_prevents_to_property_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      function* g(){
        var key = { toString() { log.push("toString"); return "a"; } };
        try { return (yield key) in (yield 0); }
        catch (e) { return e && e.name === "TypeError" && log.length === 0; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      var r3 = it.next(null);
      r1.done === false && r1.value &&
      r2.done === false && r2.value === 0 && log.length === 0 &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_instanceof_observes_has_instance_after_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      function* g(){
        var left = {};
        var right = {};
        Object.defineProperty(right, Symbol.hasInstance, {
          get() {
            log.push("get hasInstance");
            return function(v) {
              log.push("call hasInstance");
              return v === left;
            };
          }
        });
        return (yield left) instanceof (yield right);
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      var ok_mid =
        r1.done === false && r1.value &&
        r2.done === false && r2.value &&
        log.length === 0;
      var r3 = it.next(r2.value);
      ok_mid &&
      r3.done === true && r3.value === true &&
      log.length === 2 && log[0] === "get hasInstance" && log[1] === "call hasInstance"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_left_associativity_with_multiple_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `-` is left-associative: ((a - b) - c)
      function* g(){ return (yield 1) - (yield 2) - (yield 3); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(3);
      var r4 = it.next(4);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 3
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
