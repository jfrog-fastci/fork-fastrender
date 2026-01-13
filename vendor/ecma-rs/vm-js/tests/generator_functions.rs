use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn generator_function_decl_is_callable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {}
        typeof g === "function"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_function_is_not_constructable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {}
        try { new g(); "no error"; } catch(e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn generator_call_returns_object_with_next_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {}
        typeof g().next === "function"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_method_in_object_literal_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o = { *g() { yield 1; } };
        o.g().next().value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_method_in_class_works() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C { *g() { yield 2; } }
        new C().g().next().value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn yield_without_operand_is_true_undefined_even_if_shadowed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { var undefined = 1; yield; }
        var it = g();
        it.next().value === undefined
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_default_params_are_not_evaluated_until_first_next() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var called = false;
        function f(){ called = true; return 1; }
        function* g(x = f()) { yield x; }
        var it = g();
        var before = called;
        var v = it.next().value;
        var after = called;
        before === false && after === true && v === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn yield_in_object_destructuring_assignment_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var a;
            ({a = yield 1} = {});
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.value !== 1 || r1.done !== false) return false;
          var r2 = it.next(5);
          return r2.value === 5 && r2.done === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn yield_in_array_destructuring_assignment_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var a;
            ([a = yield 1] = []);
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.value !== 1 || r1.done !== false) return false;
          var r2 = it.next(5);
          return r2.value === 5 && r2.done === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn yield_in_object_destructuring_assignment_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var a;
            ({[(yield 1)]: a} = {x: 5});
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.value !== 1 || r1.done !== false) return false;
          var r2 = it.next("x");
          return r2.value === 5 && r2.done === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
