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
fn strict_mode_restricted_binding_identifiers_are_syntax_errors() {
  let mut rt = new_runtime();

  // Restricted identifiers in `var` declarations.
  let err = rt.exec_script(r#""use strict"; var eval = 1;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));

  // Restricted identifiers in catch bindings.
  let err = rt
    .exec_script(r#""use strict"; try { throw 1 } catch (arguments) { }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));

  // Restricted identifiers inside strict function bodies.
  let err = rt
    .exec_script(r#"function f() { "use strict"; var arguments = 1; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn strict_mode_restricted_assignment_targets_are_syntax_errors() {
  let mut rt = new_runtime();

  let err = rt.exec_script(r#""use strict"; eval = 1;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));

  let err = rt.exec_script(r#""use strict"; ++arguments;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));

  // In sloppy mode these forms are allowed.
  let value = rt.exec_script(r#"var eval = 2; (eval + 1) === 3"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn dynamic_function_constructors_reject_restricted_identifiers_in_strict_mode() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
        try {
          Function('"use strict"; eval = 1;');
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
            GeneratorFunction('"use strict"; ++arguments;');
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

