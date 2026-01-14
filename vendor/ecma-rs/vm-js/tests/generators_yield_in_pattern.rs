use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_binding_object_pattern_computed_key_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(o) { let { [yield 'k']: x } = o; return x; }
      var it = g({ a: 42 });
      var r1 = it.next();
      var r2 = it.next('a');
      r1.done === false && r1.value === 'k' && r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binding_array_pattern_default_can_yield_and_preserves_tdz() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(arr) {
        let getX = () => x;
        let [x = (yield getX)] = arr;
        return x;
      }
      var it = g([]);
      var r1 = it.next();
      var getX = r1.value;
      var tdzOk = false;
      try { getX(); } catch (e) { tdzOk = e instanceof ReferenceError; }
      var r2 = it.next(5);
      r1.done === false && typeof getX === 'function' && tdzOk === true && r2.done === true && r2.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_object_pattern_computed_key_can_yield_and_captures_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o1 = { a: 1 };
      var o2 = { a: 2 };
      var o = o1;
      function* g() {
        let x;
        ({ [yield 'k']: x } = o);
        return x;
      }
      var it = g();
      var r1 = it.next();
      o = o2;
      var r2 = it.next('a');
      r1.done === false && r1.value === 'k' && r2.done === true && r2.value === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_array_pattern_default_can_yield_and_captures_iterator_state() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var a1 = [undefined, 10];
      var a2 = [undefined, 99];
      var a = a1;
      function* g() {
        let x, y;
        ([x = (yield 1), y] = a);
        return [x, y];
      }
      var it = g();
      var r1 = it.next();
      a = a2;
      var r2 = it.next(5);
      r1.done === false && r1.value === 1 &&
      r2.done === true && r2.value.length === 2 &&
      r2.value[0] === 5 && r2.value[1] === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_binding_pattern_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var out = [];
        for (let [x = (yield 1)] of [ [] ]) {
          out.push(x);
        }
        return out[0];
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(5);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_catch_parameter_pattern_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        try { throw { a: 1 }; }
        catch ({ [yield 'k']: x }) { return x; }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next('a');
      r1.done === false && r1.value === 'k' && r2.done === true && r2.value === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binding_array_rest_pattern_default_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        let [...[a = (yield 1)]] = [undefined];
        return a;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(5);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_binding_rest_pattern_default_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        for (let [...[a = (yield 1)]] of [[undefined]]) {
          return a;
        }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(5);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_target_identifier_resolution_is_fixed_across_yield_strict() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g() {
          "use strict";
          ({ a: x = (yield 1) } = { a: undefined });
          return x;
        }

        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;

        // Create the global property after the binding reference has already been resolved.
        globalThis.x = 123;

        var threw = false;
        try { it.next(5); } catch (e) { threw = e instanceof ReferenceError; }
        return threw === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
