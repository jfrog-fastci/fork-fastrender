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
fn object_prototype_to_string_honors_symbol_to_string_tag_for_generators() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script("function* g(){ yield 1; }\nObject.prototype.toString.call(g)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object GeneratorFunction]");

  // Calling generator functions is not implemented yet in vm-js, but we can still validate the
  // @@toStringTag behavior via the generator function's instance prototype chain:
  // `it.[[Prototype]] === g.prototype` and `g.prototype.[[Prototype]] === %GeneratorPrototype%`.
  let value = rt.exec_script("const it = Object.create(g.prototype);\nObject.prototype.toString.call(it)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object Generator]");

  // Non-string @@toStringTag values must be ignored.
  let value = rt.exec_script(
    "Object.defineProperty(g.prototype, Symbol.toStringTag, { get() { return {}; } });\n\
     Object.prototype.toString.call(it)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  Ok(())
}
