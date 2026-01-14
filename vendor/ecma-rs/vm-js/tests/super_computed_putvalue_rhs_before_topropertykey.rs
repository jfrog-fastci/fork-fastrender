use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script)
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

// Spec regression:
// For `super[expr] = rhs`, `expr` is evaluated to a value while creating the Super Reference, but
// `ToPropertyKey` is deferred until `PutValue` (ECMA-262 6.2.5.6), which happens *after* evaluating
// the RHS. This means RHS side effects must be observed before key-coercion side effects.
const SOURCE: &str = r#"
  (() => {
    let log = [];

    function keyExpr() {
      log.push("keyExpr");
      return {
        toString() {
          log.push("toString");
          return "p";
        }
      };
    }

    function rhs() {
      log.push("rhs");
      return 1;
    }

    class Base {
      set p(v) {
        log.push("set");
      }
    }

    class Derived extends Base {
      m() {
        super[keyExpr()] = rhs();
        return log.join(",");
      }
    }

    return new Derived().m();
  })()
"#;

const EXPECTED: &str = "keyExpr,rhs,toString,set";

#[test]
fn computed_super_assignment_observes_rhs_before_topropertykey() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(SOURCE)?;
  assert_value_is_utf8(&rt, value, EXPECTED);
  Ok(())
}

#[test]
fn computed_super_assignment_observes_rhs_before_topropertykey_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, SOURCE)?;
  assert_value_is_utf8(&rt, value, EXPECTED);
  Ok(())
}

