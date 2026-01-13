use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn error_stack_is_available_in_catch() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let v = rt.exec_script(
    "try { throw new Error('x'); } catch (e) { typeof e.stack === 'string' && e.stack.includes('Error') }",
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_contains_multiple_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let v = rt.exec_script(
    r#"
function outerCallFunction() { innerCallFunction(); }
function innerCallFunction() { throwerFunction(); }
function throwerFunction() { throw new Error("x"); }
try { outerCallFunction(); false } catch (e) {
  e.stack.includes("outerCallFunction")
    && e.stack.includes("innerCallFunction")
    && e.stack.includes("throwerFunction")
}
"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_is_bounded_for_long_function_names() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let long_name = "a".repeat(8 * 1024);
  let script = format!(
    r#"
function {name}() {{ throw new Error("x"); }}
try {{ {name}(); false }} catch (e) {{
  typeof e.stack === "string" && e.stack.length < 5000
}}
"#,
    name = long_name
  );
  let v = rt.exec_script(&script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

