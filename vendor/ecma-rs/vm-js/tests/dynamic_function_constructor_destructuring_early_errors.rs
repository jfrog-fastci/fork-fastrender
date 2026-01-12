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
fn dynamic_function_constructors_reject_destructuring_decls_without_initializers() {
  let mut rt = new_runtime();

  // %Function%
  let value = rt
    .exec_script(
      r#"
        try {
          Function("var { x };");
          "no";
        } catch (e) {
          e.name;
        }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  let value = rt
    .exec_script(
      r#"
        try {
          Function("let { x };");
          "no";
        } catch (e) {
          e.name;
        }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  // %GeneratorFunction%
  let value = rt
    .exec_script(
      r#"
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          try {
            GeneratorFunction("var { x };");
            return "no";
          } catch (e) {
            return e.name;
          }
        })()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");
}

#[test]
fn destructuring_decls_in_for_in_of_headers_are_allowed() {
  let mut rt = new_runtime();

  // Destructuring bindings in `for-in`/`for-of` headers do not require an initializer.
  let value = rt
    .exec_script(
      r#"
        typeof Function("for (var { x } in { a: 1 }) { }")
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "function");
}

#[test]
fn dynamic_function_constructors_reject_const_decls_without_initializers() {
  let mut rt = new_runtime();

  // %Function%
  let value = rt
    .exec_script(
      r#"
        try {
          Function("const x;");
          "no";
        } catch (e) {
          e.name;
        }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  // %GeneratorFunction%
  let value = rt
    .exec_script(
      r#"
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          try {
            GeneratorFunction("const x;");
            return "no";
          } catch (e) {
            return e.name;
          }
        })()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");
}
