use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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

  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) =
    scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

fn feature_detect_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Parse-level support for `async function*` isn't sufficient: vm-js can accept the syntax and
  // still surface `VmError::Unimplemented` once the generator is actually executed. Probe a minimal
  // `.next()` call so tests only activate when core async generator machinery exists.
  match rt.exec_script(
    r#"
      async function* __ag_support() { yield 1; }
      __ag_support().next();
    "#,
  ) {
    Ok(_) => {
      // Avoid leaking Promise jobs into subsequent assertions.
      rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
      Ok(true)
    }
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn compiled_script_falls_back_for_async_generators() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_generators_fallback.js",
    r#"
      var result = 0;

      async function* g() {
        yield 1;
      }

      g().next().then(r => { result = r.value; });
    "#,
  )?;

  rt.exec_compiled_script(script)?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

