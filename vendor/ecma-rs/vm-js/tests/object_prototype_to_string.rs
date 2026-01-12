use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_prototype_to_string_respects_symbol_to_string_tag() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = { [Symbol.toStringTag]: "X" }; Object.prototype.toString.call(o) === "[object X]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_arrays() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Object.prototype.toString.call([]) === "[object Array]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_promises() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Object.prototype.toString.call(Promise.resolve()) === "[object Promise]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
#[ignore]
fn object_prototype_to_string_tags_generator_objects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Object.prototype.toString.call((function*() {})()) === "[object Generator]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
