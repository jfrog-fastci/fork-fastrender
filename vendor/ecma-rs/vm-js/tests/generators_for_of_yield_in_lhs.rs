use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_for_of_yield_in_array_pattern_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var a;
          // Ensure the loop iterates over a single *array* value whose first element is `undefined`
          // so the array-pattern default initializer runs.
          var undefined = [undefined];
          for ([a = yield 1] of [undefined]) { return a; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_lexical_array_pattern_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          // `let` for-of creates a per-iteration lexical environment; ensure suspension/resumption
          // within the binding initialization correctly preserves that environment.
          for (let [a = yield 1] of [[undefined]]) { return a; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_const_array_pattern_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          for (const [a = yield 1] of [[undefined]]) { return a; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_const_array_pattern_default_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var out = [];
          for (const [a = yield 1] of [[undefined], [undefined]]) { out.push(a); }
          return out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(20);
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === "10,20"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var v;
          for ({[yield "k"]: v} of [{k: 3}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_lexical_object_pattern_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          for (let {[yield "k"]: v} of [{k: 3}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_const_object_pattern_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          for (const {[yield "k"]: v} of [{k: 3}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_const_object_pattern_computed_key_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var out = [];
          for (const {[(yield 1)]: v} of [{x: 1}, {y: 2}]) { out.push(v); }
          return out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("x");
        var r3 = it.next("y");
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === "1,2"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_lexical_object_pattern_default_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          for (let {a: v = yield 1} of [{a: undefined}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_const_object_pattern_default_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          for (const {a: v = yield 1} of [{a: undefined}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_const_object_pattern_default_value_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var out = [];
          for (const {a: v = yield 1} of [{a: undefined}, {a: undefined}]) { out.push(v); }
          return out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(20);
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === "10,20"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_multiple_yields_in_single_lhs_pattern() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var a, b;
          for ([a = yield 1, b = yield 2] of [[undefined, undefined]]) { return a + b; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(20);
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 2 &&
        r3.done === true && r3.value === 30
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_lhs_does_not_re_evaluate_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var a;
          var calls = 0;
          function rhs() { calls++; return [[undefined]]; }
          for ([a = yield calls] of rhs()) { return calls + ":" + a; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === "1:42"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_let_lhs_preserves_per_iteration_env() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let fs = [];
          for (let [a = yield 1] of [[undefined], [2]]) {
            fs.push(() => a);
          }
          return fs[0]() + "," + fs[1]();
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === "42,2"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_let_object_pattern_default_preserves_per_iteration_env() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let fs = [];
          for (let {0: a = yield 1} of ["", "x"]) {
            fs.push(() => a);
          }
          return fs[0]() + "," + fs[1]();
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === "42,x"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_iterator_is_closed_on_return_while_suspended_in_lhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var closed = false;
        var iterable = {
          [Symbol.iterator]() {
            return {
              next() { return { value: [undefined], done: false }; },
              return() { closed = true; return { done: true }; },
            };
          },
        };

        function* g() {
          for (let [a = yield 1] of iterable) { /* unreachable */ }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.return("done");
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === "done" &&
        closed === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for (obj[yield "k"] of [3]) { return obj.k; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_default_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var v;
          for ({a: v = yield 1} of [{a: undefined}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_multiple_yields_in_object_pattern_computed_key_and_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var v;
          for ({[yield "k"]: v = yield 1} of [{}]) { return v; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        var r3 = it.next(42);
        r1.done === false && r1.value === "k" &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_iterator_is_closed_on_throw_while_suspended_in_lhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var closed = false;
        var iterable = {
          [Symbol.iterator]() {
            return {
              next() { return { value: [undefined], done: false }; },
              return() { closed = true; return { done: true }; },
            };
          },
        };

        function* g() {
          for (let [a = yield 1] of iterable) { /* unreachable */ }
        }
        var it = g();
        var r1 = it.next();
        var threw = false;
        try { it.throw("boom"); } catch (e) { threw = (e === "boom"); }
        r1.done === false && r1.value === 1 &&
        threw === true && closed === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_array_pattern_elem_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ([obj[yield "k"]] of [[3]]) { return obj.k; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_array_pattern_rest_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ([...obj[yield "k"]] of [[1, 2, 3]]) { return obj.k[1]; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_prop_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ({a: obj[yield "k"]} of [{a: 3}]) { return obj.k; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_prop_assignment_target_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          var out = [];
          for ({a: obj[yield 1]} of [{a: 1}, {a: 2}]) {
            out.push(obj.k);
          }
          return out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        var r3 = it.next("k");
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === "1,2"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_rest_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ({...obj[yield "k"]} of [{a: 1, b: 2}]) { return obj.k.b; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_object_pattern_rest_assignment_target_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          var out = [];
          for ({...obj[yield 1]} of ["a", "bb"]) {
            out.push(obj.k[0] + (obj.k[1] || ""));
          }
          return out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        var r3 = it.next("k");
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === "a,bb"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_yield_in_array_pattern_rest_assignment_target_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          var out = [];
          for ([...obj[yield 1]] of ["a", "bb"]) {
            out.push(obj.k.join(""));
          }
          return out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        var r3 = it.next("k");
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 1 &&
        r3.done === true && r3.value === "a,bb"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_let_default_initializer_has_tdz_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var a = 99;
        function* g() {
          for (let [a = (yield 1, a)] of [[undefined]]) { return a; }
        }
        var it = g();
        var r1 = it.next();
        var threw = false;
        try { it.next(0); } catch (e) { threw = e && e.name === "ReferenceError"; }
        r1.done === false && r1.value === 1 && threw === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_let_object_default_initializer_has_tdz_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var a = 99;
        function* g() {
          for (let {a = (yield 1, a)} of [{}]) { return a; }
        }
        var it = g();
        var r1 = it.next();
        var threw = false;
        try { it.next(0); } catch (e) { threw = e && e.name === "ReferenceError"; }
        r1.done === false && r1.value === 1 && threw === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
