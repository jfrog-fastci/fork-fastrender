use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn string_replace_all_with_string_replacer() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#""aaa".replaceAll("a", "b") === "bbb""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_replace_all_with_callable_replacer() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#""aba".replaceAll("b", () => "x") === "axa""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

