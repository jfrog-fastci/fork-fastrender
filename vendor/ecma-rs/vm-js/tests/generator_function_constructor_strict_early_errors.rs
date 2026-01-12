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
fn generator_function_constructor_rejects_strict_mode_with_at_creation_time() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          try {
            GeneratorFunction('"use strict"; with (x) return foo;');
            return "no";
          } catch (e) {
            return e.name;
          }
        })()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  let value = rt
    .exec_script(
      r#"
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          return typeof GeneratorFunction('with (x) return foo;');
        })()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "function");

  let value = rt
    .exec_script(
      r#"
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          try {
            GeneratorFunction('"use strict"; delete x;');
            return "no";
          } catch (e) {
            return e.name;
          }
        })()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  let value = rt
    .exec_script(
      r#"
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          return typeof GeneratorFunction('delete x;');
        })()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "function");
}

#[test]
fn function_constructor_rejects_strict_mode_with_at_creation_time() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
        try {
          Function('"use strict"; with (x) return foo;');
          "no";
        } catch (e) {
          e.name;
        }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  let value = rt.exec_script(r#"typeof Function('with (x) return foo;')"#).unwrap();
  assert_value_is_utf8(&rt, value, "function");

  let value = rt
    .exec_script(
      r#"
        try {
          Function('"use strict"; delete x;');
          "no";
        } catch (e) {
          e.name;
        }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "SyntaxError");

  let value = rt.exec_script(r#"typeof Function('delete x;')"#).unwrap();
  assert_value_is_utf8(&rt, value, "function");
}
