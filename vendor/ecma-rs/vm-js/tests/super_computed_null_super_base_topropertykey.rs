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

const COERCION_HAPPENS_BEFORE_NULL_BASE_ERROR: &str = r#"
  (() => {
    let log = [];
    let key = {
      toString() {
        log.push("toString");
        return "p";
      }
    };

    let obj = {
      __proto__: null,
      get() {
        try { super[key]; } catch (e) { log.push("get:" + e.name); }
      },
      set() {
        try { super[key] = 1; } catch (e) { log.push("set:" + e.name); }
      },
      call() {
        try { super[key](); } catch (e) { log.push("call:" + e.name); }
      },
      inc() {
        try { ++super[key]; } catch (e) { log.push("inc:" + e.name); }
      },
    };

    obj.get();
    obj.set();
    obj.call();
    obj.inc();

    return log.join(",");
  })()
"#;

const EXPECTED: &str =
  "toString,get:TypeError,toString,set:TypeError,toString,call:TypeError,toString,inc:TypeError";

#[test]
fn computed_super_key_coercion_is_observed_before_null_super_base_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(COERCION_HAPPENS_BEFORE_NULL_BASE_ERROR)?;
  assert_value_is_utf8(&rt, value, EXPECTED);
  Ok(())
}

#[test]
fn computed_super_key_coercion_is_observed_before_null_super_base_type_error_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, COERCION_HAPPENS_BEFORE_NULL_BASE_ERROR)?;
  assert_value_is_utf8(&rt, value, EXPECTED);
  Ok(())
}

