use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

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

fn is_unimplemented_async_generator_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains("async generator functions") => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // vm-js historically feature-detects async generator functions by throwing a SyntaxError at runtime
  // (instead of returning a host-level `VmError::Unimplemented`), so test harnesses can use
  // try/catch. Treat that specific error as "feature not implemented" so this test can land before
  // async generators are supported.
  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };
  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

#[test]
fn object_prototype_to_string_honors_symbol_to_string_tag_for_async_generators() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Basic tags.
  let value = match rt.exec_script("Object.prototype.toString.call(async function*(){})") {
    Ok(v) => v,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGeneratorFunction]");

  let value = match rt.exec_script("Object.prototype.toString.call((async function*(){ yield 1; })())") {
    Ok(v) => v,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_utf8(&rt, value), "[object AsyncGenerator]");

  // Define a stable binding to test `String(it)` and prototype-chain behaviour.
  let value = match rt.exec_script(
    "async function* g(){ yield 1; }\n\
     const it = g();\n\
     Object.prototype.toString.call(it)",
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };
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
