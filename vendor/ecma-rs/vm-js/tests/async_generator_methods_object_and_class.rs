use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn expect_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap()
    .get_string(s)
    .expect("string handle should be valid")
    .to_utf8_lossy()
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
  match rt.exec_script("async function* __ag_support() {} void __ag_support();") {
    Ok(_) => Ok(true),
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn async_generator_method_object_literal_creation() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

  // Creation: `async *m(){}` in an object literal should create an async generator function object.
  rt.exec_script("var f = ({ async *m() { } }).m;")?;

  let out = rt.exec_script("Object.prototype.toString.call(f)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGeneratorFunction]");

  let out =
    rt.exec_script("Object.getPrototypeOf(f) === Object.getPrototypeOf(async function*(){})")?;
  assert_eq!(out, Value::Bool(true));

  // Async generator functions have a `.prototype` object whose prototype is `%AsyncGeneratorPrototype%`.
  let out = rt.exec_script("Object.prototype.toString.call(f.prototype)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGenerator]");

  let out = rt.exec_script(
    "Object.getPrototypeOf(f.prototype) === Object.getPrototypeOf((async function*(){}).prototype)",
  )?;
  assert_eq!(out, Value::Bool(true));

  Ok(())
}

#[test]
fn async_generator_methods_class_creation() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

  // Creation: `async *m(){}` on classes should create async generator function objects for both
  // prototype and static methods.
  rt.exec_script("class C { async *m() {} static async *s() {} }")?;

  let out = rt.exec_script("Object.prototype.toString.call(C.prototype.m)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGeneratorFunction]");

  let out = rt.exec_script("Object.prototype.toString.call(C.s)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGeneratorFunction]");

  let out = rt.exec_script(
    "Object.getPrototypeOf(C.prototype.m) === Object.getPrototypeOf(async function*(){})",
  )?;
  assert_eq!(out, Value::Bool(true));

  let out = rt.exec_script("Object.getPrototypeOf(C.s) === Object.getPrototypeOf(async function*(){})")?;
  assert_eq!(out, Value::Bool(true));

  let out = rt.exec_script("Object.prototype.toString.call(C.prototype.m.prototype)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGenerator]");

  let out = rt.exec_script("Object.prototype.toString.call(C.s.prototype)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGenerator]");

  Ok(())
}
