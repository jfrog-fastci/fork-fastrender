use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn function_prototype_to_string_ecma_contains_source() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script("Function.prototype.toString.call(function f(){})")?;
  let s = value_to_utf8(&rt, value);
  assert!(s.contains("function f"));
  Ok(())
}

#[test]
fn function_prototype_to_string_native_contains_native_code() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script("Function.prototype.toString.call(Math.abs)")?;
  let s = value_to_utf8(&rt, value);
  assert!(s.contains("[native code]"));
  Ok(())
}

#[test]
fn function_prototype_to_string_throws_on_non_callable_receiver() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value =
    rt.exec_script(r#"try { Function.prototype.toString.call({}); } catch(e) { e.name }"#)?;
  let s = value_to_utf8(&rt, value);
  assert_eq!(s, "TypeError");
  Ok(())
}

