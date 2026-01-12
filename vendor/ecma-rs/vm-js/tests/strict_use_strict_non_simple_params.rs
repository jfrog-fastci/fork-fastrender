use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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
fn use_strict_directive_is_invalid_with_non_simple_parameter_lists() {
  let mut rt = new_runtime();

  // Function declaration with default parameter.
  let err = rt
    .exec_script(r#"function f(a = 1) { "use strict"; return a; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));

  // Arrow function with default parameter.
  let err = rt
    .exec_script(r#"((a = 1) => { "use strict"; return a; })"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));

  // Control: strict directive is allowed with a simple parameter list.
  let value = rt
    .exec_script(r#"function g(a) { "use strict"; return a; } g(2)"#)
    .unwrap();
  assert_eq!(value, Value::Number(2.0));

  // Control: non-simple parameter list is allowed without a strict directive.
  let value = rt.exec_script(r#"function h(a = 1) { return a; } h()"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn function_constructors_reject_use_strict_with_non_simple_parameters_at_creation_time() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
        try {
          Function("a = 1", '"use strict"; return a;');
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
        (function () {
          const GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
          try {
            GeneratorFunction("a = 1", '"use strict"; return a;');
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

