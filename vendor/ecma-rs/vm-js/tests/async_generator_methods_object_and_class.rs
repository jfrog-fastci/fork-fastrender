use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

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

#[test]
fn async_generator_method_object_literal_creation() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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

  let out =
    rt.exec_script("Object.getPrototypeOf(C.s) === Object.getPrototypeOf(async function*(){})")?;
  assert_eq!(out, Value::Bool(true));

  let out = rt.exec_script("Object.prototype.toString.call(C.prototype.m.prototype)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGenerator]");

  let out = rt.exec_script("Object.prototype.toString.call(C.s.prototype)")?;
  assert_eq!(expect_string(&rt, out), "[object AsyncGenerator]");

  Ok(())
}
