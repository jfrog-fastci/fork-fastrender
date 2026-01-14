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
fn generator_for_in_let_object_default_initializer_has_tdz_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var a = 99;
          function* g() {
            // The binding must exist (uninitialized) before evaluating the default initializer, so
            // reading `a` after resumption still hits the TDZ rather than the outer `var a`.
            for (let {a = (yield 1, a)} in {abc: 0}) { return a; }
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
fn generator_for_in_const_default_initializer_has_tdz_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var a = 99;
          function* g() {
            for (const [a = (yield 1, a)] in {"": 0}) { return a; }
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
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          for ({1: obj[yield "k"]} in {abc: 0}) { return obj.k; }
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
fn generator_for_in_yield_in_assignment_target_super_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for (super[yield "k"] in {abc: 0}) { return this._k; }
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_yield_in_assignment_target_super_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            var out = [];
            for (super[yield 1] in {a: 0, bb: 0}) { out.push(this._k); }
            return out.join(",");
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_yield_in_object_pattern_rest_assignment_target_super_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for ({...super[yield "k"]} in {abc: 0}) { return this._k[1]; }
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_super_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for ({1: super[yield "k"]} in {abc: 0}) { return this._k; }
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_super_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for ([...super[yield "k"]] in {abc: 0}) { return this._k[1]; }
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_yield_in_array_pattern_elem_assignment_target_super_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for ([super[yield "k"]] in {abc: 0}) { return this._k; }
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_multiple_yields_in_array_pattern_elem_super_computed_member_and_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for ([super[yield "k"] = yield 1] in {"": 0}) { return this._k; }
          }
        }
        var it = (new Derived()).g();
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
fn generator_for_in_multiple_yields_in_object_pattern_prop_super_computed_member_and_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { set k(v) { this._k = v; } }
        class Derived extends Base {
          *g() {
            for ({missing: super[yield "k"] = yield 1} in {abc: 0}) { return this._k; }
          }
        }
        var it = (new Derived()).g();
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

#[test]
fn generator_for_in_yield_in_const_lhs_preserves_per_iteration_env() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let fs = [];
          for (const [a = yield 1] in {"": 0, x: 0}) {
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
fn generator_for_in_yield_in_let_object_pattern_default_preserves_per_iteration_env() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let fs = [];
          for (let {0: a = yield 1} in {"": 0, x: 0}) {
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
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          var out = [];
          for ({length: obj[yield 1]} in {a: 0, bb: 0}) {
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
fn generator_for_in_yield_in_object_pattern_rest_assignment_target_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          var out = [];
          for ({...obj[yield 1]} in {a: 0, bb: 0}) {
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
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_computed_member_multiple_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var obj = {};
          var out = [];
          for ([...obj[yield 1]] in {a: 0, bb: 0}) {
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
fn generator_for_in_yield_in_assignment_target_computed_member_base_yield_evaluates_key_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var k = "x";
          var resumed = {};
          function* g() {
            for ((yield 1)[k] in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          k = "k";
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === 1 &&
            r2.done === true && r2.value === 0 &&
            resumed.k === "abc" &&
            Object.prototype.hasOwnProperty.call(resumed, "x") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_computed_member_base_yield_evaluates_key_after_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var k = "x";
          var resumed = {};
          function* g() {
            for ({1: (yield 1)[k]} in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          k = "k";
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === 1 &&
            r2.done === true && r2.value === 0 &&
            resumed.k === "b" &&
            Object.prototype.hasOwnProperty.call(resumed, "x") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_rest_assignment_target_computed_member_base_yield_evaluates_key_after_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var k = "x";
          var resumed = {};
          function* g() {
            for ({...(yield 1)[k]} in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          k = "k";
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === 1 &&
            r2.done === true && r2.value === 0 &&
            resumed.k[0] === "a" && resumed.k[1] === "b" && resumed.k[2] === "c" &&
            Object.prototype.hasOwnProperty.call(resumed, "x") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_elem_assignment_target_computed_member_base_yield_evaluates_key_after_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var k = "x";
          var resumed = {};
          function* g() {
            for ([(yield 1)[k]] in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          k = "k";
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === 1 &&
            r2.done === true && r2.value === 0 &&
            resumed.k === "a" &&
            Object.prototype.hasOwnProperty.call(resumed, "x") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_computed_member_base_yield_evaluates_key_after_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var k = "x";
          var resumed = {};
          function* g() {
            for ([...(yield 1)[k]] in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          k = "k";
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === 1 &&
            r2.done === true && r2.value === 0 &&
            resumed.k.join("") === "abc" &&
            Object.prototype.hasOwnProperty.call(resumed, "x") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_assignment_target_member_base_yield_constructs_reference_on_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var yielded = { name: "yielded" };
          var resumed = {};
          function* g() {
            for ((yield yielded).k in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === yielded &&
            r2.done === true && r2.value === 0 &&
            resumed.k === "abc" &&
            Object.prototype.hasOwnProperty.call(yielded, "k") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_member_base_yield_constructs_reference_on_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var yielded = { name: "yielded" };
          var resumed = {};
          function* g() {
            for ({1: (yield yielded).k} in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === yielded &&
            r2.done === true && r2.value === 0 &&
            resumed.k === "b" &&
            Object.prototype.hasOwnProperty.call(yielded, "k") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_rest_assignment_target_member_base_yield_constructs_reference_on_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var yielded = { name: "yielded" };
          var resumed = {};
          function* g() {
            for ({...(yield yielded).k} in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === yielded &&
            r2.done === true && r2.value === 0 &&
            resumed.k[0] === "a" && resumed.k[1] === "b" && resumed.k[2] === "c" &&
            Object.prototype.hasOwnProperty.call(yielded, "k") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_elem_assignment_target_member_base_yield_constructs_reference_on_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var yielded = { name: "yielded" };
          var resumed = {};
          function* g() {
            for ([(yield yielded).k] in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === yielded &&
            r2.done === true && r2.value === 0 &&
            resumed.k === "a" &&
            Object.prototype.hasOwnProperty.call(yielded, "k") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_member_base_yield_constructs_reference_on_resume()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var yielded = { name: "yielded" };
          var resumed = {};
          function* g() {
            for ([...(yield yielded).k] in {abc: 0}) { return 0; }
          }
          var it = g();
          var r1 = it.next();
          var r2 = it.next(resumed);
          return r1.done === false && r1.value === yielded &&
            r2.done === true && r2.value === 0 &&
            resumed.k.join("") === "abc" &&
            Object.prototype.hasOwnProperty.call(yielded, "k") === false;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_computed_member_key_yield_happens_before_getv() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var gets = 0;
          Object.defineProperty(String.prototype, "missing", {
            configurable: true,
            get() { gets++; return 3; },
          });
          function* g() {
            var obj = {};
            for ({missing: obj[yield 1]} in {abc: 0}) { return gets + ":" + obj.k; }
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1 || gets !== 0) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === "1:3";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_object_pattern_prop_assignment_target_super_computed_member_key_yield_happens_before_getv()
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var gets = 0;
          Object.defineProperty(String.prototype, "missing", {
            configurable: true,
            get() { gets++; return 3; },
          });

          class Base { set k(v) { this._k = v; } }
          class Derived extends Base {
            *g() {
              for ({missing: super[yield 1]} in {abc: 0}) { return gets + ":" + this._k; }
            }
          }

          var it = (new Derived()).g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1 || gets !== 0) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === "1:3";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_elem_assignment_target_computed_member_key_yield_happens_before_rhs_iterator_step(
)
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCalls = 0;
          String.prototype[Symbol.iterator] = function() {
            var s = String(this);
            var i = 0;
            return {
              next() {
                nextCalls++;
                if (i < s.length) return { value: s[i++], done: false };
                return { value: undefined, done: true };
              },
            };
          };

          function* g() {
            var obj = {};
            for ([obj[yield 1]] in {abc: 0}) { return nextCalls + ":" + obj.k; }
          }

          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1 || nextCalls !== 0) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === "1:a";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_elem_assignment_target_super_computed_member_key_yield_happens_before_rhs_iterator_step(
)
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCalls = 0;
          String.prototype[Symbol.iterator] = function() {
            var s = String(this);
            var i = 0;
            return {
              next() {
                nextCalls++;
                if (i < s.length) return { value: s[i++], done: false };
                return { value: undefined, done: true };
              },
            };
          };

          class Base { set k(v) { this._k = v; } }
          class Derived extends Base {
            *g() {
              for ([super[yield 1]] in {abc: 0}) { return nextCalls + ":" + this._k; }
            }
          }

          var it = (new Derived()).g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1 || nextCalls !== 0) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === "1:a";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_computed_member_key_yield_happens_before_rhs_iterator_step(
)
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCalls = 0;
          String.prototype[Symbol.iterator] = function() {
            var s = String(this);
            var i = 0;
            return {
              next() {
                nextCalls++;
                if (i < s.length) return { value: s[i++], done: false };
                return { value: undefined, done: true };
              },
            };
          };

          function* g() {
            var obj = {};
            for ([...obj[yield 1]] in {abc: 0}) { return nextCalls + ":" + obj.k[1]; }
          }

          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1 || nextCalls !== 0) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === "4:b";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_yield_in_array_pattern_rest_assignment_target_super_computed_member_key_yield_happens_before_rhs_iterator_step(
)
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCalls = 0;
          String.prototype[Symbol.iterator] = function() {
            var s = String(this);
            var i = 0;
            return {
              next() {
                nextCalls++;
                if (i < s.length) return { value: s[i++], done: false };
                return { value: undefined, done: true };
              },
            };
          };

          class Base { set k(v) { this._k = v; } }
          class Derived extends Base {
            *g() {
              for ([...super[yield 1]] in {abc: 0}) { return nextCalls + ":" + this._k[1]; }
            }
          }

          var it = (new Derived()).g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1 || nextCalls !== 0) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === "4:b";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_array_destructuring_iterator_is_closed_on_return_while_suspended_in_rest_super_computed_member_key(
)
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCount = 0;
          var returnCount = 0;
          String.prototype[Symbol.iterator] = function() {
            var s = String(this);
            var i = 0;
            return {
              next() {
                nextCount++;
                if (i < s.length) return { value: s[i++], done: false };
                return { value: undefined, done: true };
              },
              return() { returnCount++; return { done: true }; },
            };
          };

          class Base { set k(v) { this._k = v; } }
          class Derived extends Base {
            *g() {
              for ([...super[yield 1]] in {abc: 0}) { /* unreachable */ }
            }
          }

          var inst = new Derived();
          var it = inst.g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.return("done");
          return (
            r2.done === true &&
            r2.value === "done" &&
            nextCount === 0 &&
            returnCount === 1 &&
            inst._k === undefined
          );
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_array_destructuring_iterator_is_closed_on_throw_while_suspended_in_rest_super_computed_member_key(
)
{
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCount = 0;
          var returnCount = 0;
          String.prototype[Symbol.iterator] = function() {
            var s = String(this);
            var i = 0;
            return {
              next() {
                nextCount++;
                if (i < s.length) return { value: s[i++], done: false };
                return { value: undefined, done: true };
              },
              return() { returnCount++; return { done: true }; },
            };
          };

          class Base { set k(v) { this._k = v; } }
          class Derived extends Base {
            *g() {
              for ([...super[yield 1]] in {abc: 0}) { /* unreachable */ }
            }
          }

          var inst = new Derived();
          var it = inst.g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var threw = false;
          try { it.throw("boom"); } catch (e) { threw = (e === "boom"); }
          return (
            threw === true &&
            nextCount === 0 &&
            returnCount === 1 &&
            inst._k === undefined
          );
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
