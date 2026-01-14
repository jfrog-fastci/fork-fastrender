use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out;
            for (const {[(yield 1)]: x} in {abc: 0}) {
              out = x;
            }
            return out;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("length");
          return r2.done === true && r2.value === 3;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_default_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out;
            for (const {a: x = yield 1} in {abc: 0}) {
              out = x;
            }
            return out;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(5);
          return r2.done === true && r2.value === 5;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_computed_key_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out = [];
            for (const {[(yield 1)]: x} in {a: 0, bb: 0}) {
              out.push(x);
            }
            return out.join(",");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("length");
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next("length");
          return r3.done === true && r3.value === "1,2";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_const_object_pattern_default_value_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out = [];
            for (const {a: x = yield 1} in {a: 0, b: 0}) {
              out.push(x);
            }
            return out.join(",");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(10);
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next(20);
          return r3.done === true && r3.value === "10,20";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_lexical_array_pattern_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            let out;
            // The property name is the empty string, so destructuring `[x = ...]` sees `undefined`
            // as the first element and runs the default initializer.
            for (let [x = yield 1] in {"": 0}) {
              out = x;
            }
            return out;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next(5);
          return r2.done === true && r2.value === 5;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_let_default_initializer_has_tdz_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var a = 99;
          function* g() {
            // The binding must exist (uninitialized) before evaluating the default initializer, so
            // reading `a` after resumption still hits the TDZ rather than the outer `var a`.
            for (let [a = (yield 1, a)] in {"": 0}) { return a; }
          }
          var it = g();
          var r1 = it.next();
          var threw = false;
          try { it.next(0); } catch (e) { threw = e && e.name === "ReferenceError"; }
          return r1.done === false && r1.value === 1 && threw === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_rest_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ({...obj[yield "k"]} in {abc: 0}) { return obj.k[1]; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === "b"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ([...obj[yield "k"]] in {abc: 0}) { return obj.k[1]; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === "b"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for (obj[yield "k"] in {abc: 0}) { return obj.k; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === "abc"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_elem_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ([obj[yield "k"]] in {abc: 0}) { return obj.k; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("k");
        r1.done === false && r1.value === "k" &&
        r2.done === true && r2.value === "a"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_lhs_does_not_re_evaluate_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var a;
          var calls = 0;
          function rhs() { calls++; return {"": 0, x: 0}; }
          let out = [];
          for ([a = yield calls] in rhs()) { out.push(a); }
          return calls + ":" + out.join(",");
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(42);
        r1.done === false && r1.value === 1 &&
        r2.done === true && r2.value === "1:42,x"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_let_lhs_preserves_per_iteration_env() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let fs = [];
          for (let [a = yield 1] in {"": 0, x: 0}) {
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
