use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests allocate multiple Promises and microtask jobs, especially when exercising
  // async generator execution. Keep the heap limit large enough to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn async_generator_function_is_not_constructable() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }
  let value = rt.exec_script(
    r#"
      async function* g() {}
      try { new g(); 'no error'; } catch(e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn async_generator_default_params_are_evaluated_on_call() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var called = false;
      function f(){ called = true; return 1; }
      var body = false;
      async function* g(x = f()) { body = true; yield x; }
      var it = g();
      called === true && body === false && typeof it.next === "function"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_generator_method_in_object_literal_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out='';
      (async function(){
        var o = { async *g(){ yield 1; } };
        var r = await o.g().next();
        return r.value;
      })().then(v => { out = String(v); });
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "1");
  Ok(())
}

#[test]
fn async_generator_method_in_class_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out='';
      (async function(){
        class C { async *g(){ yield 2; } }
        var r = await new C().g().next();
        return r.value;
      })().then(v => { out = String(v); });
      out
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_value_is_utf8(&rt, value, "2");
  Ok(())
}
