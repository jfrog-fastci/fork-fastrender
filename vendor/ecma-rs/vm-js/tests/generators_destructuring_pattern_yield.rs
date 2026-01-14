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
