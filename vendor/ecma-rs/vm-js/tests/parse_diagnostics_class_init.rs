use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

const EARLY_ERROR_CODE: &str = "VMJS0004";

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_syntax_error(err: VmError) -> Vec<diagnostics::Diagnostic> {
  match err {
    VmError::Syntax(diags) => diags,
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn await_expression_in_class_static_block_in_non_async_function_is_vmjs0004() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(
    rt
      .exec_script("function f(){ class C { static { await 0; } } }")
      .unwrap_err(),
  );
  assert!(
    diags.iter().any(|d| {
      d.code.as_str() == EARLY_ERROR_CODE
        && d.message.contains("await")
        && d.message.contains("class")
        && d.message.contains("static")
    }),
    "expected early error VMJS0004 for await in static block, got {diags:?}"
  );
}

#[test]
fn arguments_identifier_reference_in_class_field_initializer_is_vmjs0004() {
  let mut rt = new_runtime();
  let diags =
    assert_syntax_error(rt.exec_script("class C { x = arguments; }").unwrap_err());
  assert!(
    diags.iter().any(|d| d.code.as_str() == EARLY_ERROR_CODE
      && d.message.contains("arguments")
      && d.message.contains("class")),
    "expected early error VMJS0004 for arguments in class init, got {diags:?}"
  );
}

#[test]
fn arguments_identifier_reference_in_arrow_in_class_field_initializer_is_vmjs0004() {
  let mut rt = new_runtime();
  let diags =
    assert_syntax_error(rt.exec_script("class C { x = () => arguments; }").unwrap_err());
  assert!(
    diags.iter().any(|d| d.code.as_str() == EARLY_ERROR_CODE
      && d.message.contains("arguments")
      && d.message.contains("class")),
    "expected early error VMJS0004 for arguments in class init arrow, got {diags:?}"
  );
}

#[test]
fn arguments_identifier_reference_in_arrow_default_param_in_class_field_initializer_is_vmjs0004() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(
    rt
      .exec_script("class C { x = (a = arguments.length) => a; }")
      .unwrap_err(),
  );
  assert!(
    diags.iter().any(|d| d.code.as_str() == EARLY_ERROR_CODE
      && d.message.contains("arguments")
      && d.message.contains("class")),
    "expected early error VMJS0004 for arguments in class init arrow param default, got {diags:?}"
  );
}

#[test]
fn arguments_identifier_reference_in_nested_function_in_class_field_initializer_is_allowed() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    "class C { x = function () { return arguments.length; } } new C().x(1,2) === 2",
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn arguments_identifier_reference_in_nested_function_param_initializer_in_class_field_initializer_is_allowed(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    "class C { x = function (a = arguments.length) { return a; } } new C().x() === 0",
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn arguments_identifier_reference_in_arrow_inside_nested_function_in_class_field_initializer_is_allowed(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    "class C { x = function () { return (() => arguments.length)(); } } new C().x(1,2,3) === 3",
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
