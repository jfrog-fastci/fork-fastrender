use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn object_prototype_to_string_honors_symbol_to_string_tag_for_async_generators() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Basic tags.
  let value = rt.exec_script("Object.prototype.toString.call(async function*(){})")?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGeneratorFunction]");

  let value = rt.exec_script("Object.prototype.toString.call((async function*(){ yield 1; })())")?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  // Define a stable binding to test `String(it)` and prototype-chain behaviour.
  let value = rt.exec_script(
    "async function* g(){ yield 1; }\n\
     const it = g();\n\
     Object.prototype.toString.call(it)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  let value = rt.exec_script("String(it)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  // Non-string @@toStringTag values must be ignored (fall back to the builtin tag).
  let value = rt.exec_script(
    "Object.defineProperty(g.prototype, Symbol.toStringTag, { configurable: true, get() { return {}; } });\n\
     Object.prototype.toString.call(it)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  let value = rt.exec_script("String(it)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  // Deleting the overridden @@toStringTag should fall back to %AsyncGeneratorPrototype%[@@toStringTag].
  let value = rt.exec_script(
    "delete g.prototype[Symbol.toStringTag];\n\
     if (it[Symbol.toStringTag] !== 'AsyncGenerator') throw new Error('expected %AsyncGeneratorPrototype%[@@toStringTag]');\n\
     Object.prototype.toString.call(it)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  Ok(())
}

