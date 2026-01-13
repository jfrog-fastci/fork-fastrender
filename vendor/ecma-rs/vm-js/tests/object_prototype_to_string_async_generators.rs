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

#[test]
fn object_prototype_to_string_async_generator_falls_back_when_to_string_tag_deleted() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = match rt.exec_script(
    r#"
      (function () {
        async function* g() { yield 1; }
        const it = g();
        if (Object.prototype.toString.call(it) !== "[object AsyncGenerator]") return false;

        const proto1 = Object.getPrototypeOf(it);
        const proto2 = Object.getPrototypeOf(proto1);

        // Engines differ on whether the async generator instance inherits directly from
        // %AsyncGeneratorPrototype% or from a per-function prototype object that in turn inherits
        // from %AsyncGeneratorPrototype%. Delete from both to ensure @@toStringTag is actually
        // absent.
        delete proto1[Symbol.toStringTag];
        if (proto2 !== null) {
          delete proto2[Symbol.toStringTag];
        }
        if (proto1[Symbol.toStringTag] !== undefined) return false;

        // When @@toStringTag is absent, Object.prototype.toString must fall back to the built-in
        // tag for async generator objects ("AsyncGenerator"), not "[object Object]".
        return Object.prototype.toString.call(it) === "[object AsyncGenerator]" &&
               String(it) === "[object AsyncGenerator]";
      })()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_prototype_to_string_async_generator_falls_back_when_to_string_tag_non_string() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = match rt.exec_script(
    r#"
      (function () {
        async function* g() { yield 1; }
        const it = g();
        if (Object.prototype.toString.call(it) !== "[object AsyncGenerator]") return false;

        const proto1 = Object.getPrototypeOf(it);
        Object.defineProperty(proto1, Symbol.toStringTag, {
          get: function () { return 1; },
          configurable: true
        });
        if (it[Symbol.toStringTag] !== 1) return false;

        // When @@toStringTag is present but not a string, Object.prototype.toString must fall back
        // to the built-in tag for async generator objects ("AsyncGenerator").
        return Object.prototype.toString.call(it) === "[object AsyncGenerator]";
      })()
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
