use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  rt.exec_compiled_script(script)
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn compiled_direct_eval_allows_super_property_in_class_field_initializer() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let result = exec_compiled(
    &mut rt,
    r#"
      class Base {}
      class Derived extends Base {
        x = eval("super.toString === Object.prototype.toString");
      }
      new Derived().x;
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

#[test]
fn compiled_direct_eval_rejects_super_call_in_class_field_initializer() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let result = exec_compiled(
    &mut rt,
    r#"
      class Base {}
      class Derived extends Base {
        x = (() => {
          try {
            eval("super()");
            return "no";
          } catch (e) {
            return e.name;
          }
        })();
      }
      new Derived().x;
    "#,
  )?;
  assert_value_is_utf8(&rt, result, "SyntaxError");
  Ok(())
}

#[test]
fn compiled_direct_eval_allows_super_property_in_class_method() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let result = exec_compiled(
    &mut rt,
    r#"
      class C {
        m() {
          return eval("super.toString === Object.prototype.toString");
        }
      }
      new C().m();
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

#[test]
fn compiled_direct_eval_rejects_super_call_in_class_method() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let result = exec_compiled(
    &mut rt,
    r#"
      class C {
        m() {
          try {
            eval("super()");
            return "no";
          } catch (e) {
            return e.name;
          }
        }
      }
      new C().m();
    "#,
  )?;
  assert_value_is_utf8(&rt, result, "SyntaxError");
  Ok(())
}

#[test]
fn compiled_direct_eval_allows_super_property_in_class_static_block() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let result = exec_compiled(
    &mut rt,
    r#"
      var ok = false;
      class C {
        static {
          ok = eval("super.toString === Function.prototype.toString");
        }
      }
      ok;
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}
