use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_generator_function_to_string_slices_source_text() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script("async function* g() { yield 1; }\ng.toString()")?;
  let s = value_to_utf8(&rt, value);
  assert_eq!(s, "async function* g() { yield 1; }");
  Ok(())
}

#[test]
fn async_generator_function_constructor_to_string_matches_test262() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    "const AsyncGeneratorFunction = Object.getPrototypeOf(async function*(){}).constructor;\n\
     AsyncGeneratorFunction('yield 10').toString()",
  )?;
  let s = value_to_utf8(&rt, value);
  assert_eq!(s, "async function* anonymous(\n) {\nyield 10\n}");
  Ok(())
}

