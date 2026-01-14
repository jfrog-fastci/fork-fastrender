use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_assignment_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var o = {m: 1};
          var x;
          ({[yield 0]: x} = o);
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_default_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var x;
          ({a: x = yield 0} = {});
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(7);
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_default_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var a;
          ([a = yield 0] = []);
          return a;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(9);
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 9
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_object_destructuring_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var {[yield 0]: x} = {m: 2};
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_object_destructuring_default_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var {a: x = yield 0} = {};
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(7);
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_array_destructuring_default_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var [a = yield 0] = [];
          return a;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(9);
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 9
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_catch_param_object_destructuring_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          try {
            throw {m: 3};
          } catch ({[yield 0]: x}) {
            return x;
          }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_object_destructuring_assignment_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var xs = [{m: 1}];
          var x;
          for ({[yield 0]: x} of xs) {
            return x;
          }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_of_var_decl_object_destructuring_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var xs = [{m: 4}];
          for (var {[yield 0]: x} of xs) {
            return x;
          }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_let_decl_object_destructuring_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let {[yield 0]: x} = {m: 2};
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_const_decl_object_destructuring_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const {[yield 0]: x} = {m: 5};
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("m");
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_object_destructuring_assignment_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var x;
          for ({[yield 0]: x} in {m: 1}) {
            return x;
          }
        }
        var it = g();
        var r1 = it.next();
        // Resume yield with 0 so the computed key is `ToPropertyKey(0)` => "0".
        var r2 = it.next(0);
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === "m"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_var_decl_object_destructuring_computed_key_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          for (var {[yield 0]: x} in {m: 1}) {
            return x;
          }
        }
        var it = g();
        var r1 = it.next();
        // Resume yield with 0 so the computed key is `ToPropertyKey(0)` => "0".
        var r2 = it.next(0);
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === "m"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_computed_key_survives_gc_between_yield_and_resume() {
  let mut rt = new_runtime();

  rt
    .exec_script(
      r#"
        function* g() {
          var o = {m: 1};
          var x;
          ({[yield 0]: x} = o);
          return x;
        }
        globalThis.it = g();
        globalThis.r1 = it.next();
        r1.done === false && r1.value === 0
      "#,
    )
    .unwrap();

  // Force GC while the generator is suspended inside the destructuring pattern.
  for _ in 0..5 {
    rt.heap.collect_garbage();
  }

  let value = rt
    .exec_script(
      r#"
        var r2 = it.next("m");
        r2.done === true && r2.value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_default_survives_gc_between_yield_and_resume() {
  let mut rt = new_runtime();

  rt
    .exec_script(
      r#"
        function* g() {
          var x;
          ({a: x = yield 0} = {});
          return x;
        }
        globalThis.it = g();
        globalThis.r1 = it.next();
        r1.done === false && r1.value === 0
      "#,
    )
    .unwrap();

  // Force GC while the generator is suspended inside the destructuring pattern.
  for _ in 0..5 {
    rt.heap.collect_garbage();
  }

  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(7);
        r2.done === true && r2.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_computed_key_from_yield_star_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "suspend"; return "m"; }
        function* g() {
          var o = {m: 1};
          var x;
          ({[yield* inner()]: x} = o);
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        r1.done === false && r1.value === "suspend" &&
        r2.done === true && r2.value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_default_from_yield_star_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "suspend"; return 7; }
        function* g() {
          var x;
          ({a: x = yield* inner()} = {});
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        r1.done === false && r1.value === "suspend" &&
        r2.done === true && r2.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_object_destructuring_computed_key_from_yield_star_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "suspend"; return "m"; }
        function* g() {
          var {[yield* inner()]: x} = {m: 2};
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        r1.done === false && r1.value === "suspend" &&
        r2.done === true && r2.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_object_destructuring_default_from_yield_star_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "suspend"; return 9; }
        function* g() {
          var {a: x = yield* inner()} = {};
          return x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        r1.done === false && r1.value === "suspend" &&
        r2.done === true && r2.value === 9
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_default_from_yield_star_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "suspend"; return 9; }
        function* g() {
          var a;
          ([a = yield* inner()] = []);
          return a;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        r1.done === false && r1.value === "suspend" &&
        r2.done === true && r2.value === 9
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_array_destructuring_default_from_yield_star_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "suspend"; return 11; }
        function* g() {
          var [a = yield* inner()] = [];
          return a;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        r1.done === false && r1.value === "suspend" &&
        r2.done === true && r2.value === 11
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_default_yield_star_not_evaluated_when_present() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should not run"; return 99; }
        function* g() {
          var x;
          ({a: x = yield* inner()} = {a: 5});
          return x;
        }
        var it = g();
        var r = it.next();
        r.done === true && r.value === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_default_yield_star_not_evaluated_when_present() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* inner() { yield "should not run"; return 99; }
        function* g() {
          var a;
          ([a = yield* inner()] = [7]);
          return a;
        }
        var it = g();
        var r = it.next();
        r.done === true && r.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
