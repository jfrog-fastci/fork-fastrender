use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
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

  Ok(scope.heap().get_string(message_s)?.to_utf8_lossy() == "async generator functions")
}

/// Returns `true` if `async function*` is supported by the runtime.
fn async_generators_supported(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Detect runtime async-generator support, not just parsing/prototype wiring. vm-js may accept the
  // syntax and create function objects before it implements the execution semantics.
  match rt.exec_script("async function* __ag_support() { yield 1; } __ag_support();") {
    Ok(_) => Ok(true),
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn async_generator_yield_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var called = 0;
      var out = "";

      var p = Promise.resolve(1);
      var ctor = {};
      ctor[Symbol.species] = function C(executor) {
        called++;
        return new Promise(executor);
      };
      p.constructor = ctor;

      async function* g(){ yield p; }
      g().next().then(r => { out = String(r.value); });
    "#,
  )?;

  assert_eq!(rt.exec_script("called")?, Value::Number(0.0));
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("called")?, Value::Number(0.0));
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1");

  Ok(())
}

#[test]
fn async_generator_yield_promise_resolve_runs_constructor_getter_before_microtasks(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // Like `await`, async generator `yield` uses `Await(value)`, which must perform
  // `Get(value, "constructor")` synchronously even when `value` is already a Promise.
  let value = rt.exec_script(
    r#"
      var log = "";
      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { log += "c"; return Promise; } });

      async function* g(){ yield p; }
      g().next();

      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "c");

  // Flush any pending promise jobs from the `next()` call.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  Ok(())
}

#[test]
fn async_generator_yield_constructor_getter_throw_rejects_next_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // If `PromiseResolve` throws (e.g. user-defined `constructor` getter), the promise returned by
  // `.next()` must be rejected rather than the call throwing synchronously.
  let sync = rt.exec_script(
    r#"
      var out = "";
      var sync = "no";

      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { throw "boom"; } });

      async function* g(){ yield p; }
      var it = g();

      var pr;
      try {
        pr = it.next();
      } catch (e) {
        sync = e;
      }

      if (pr !== undefined) {
        pr.then(
          function () { out = "fulfilled"; },
          function (e) { out = e; },
        );
      }

      sync
    "#,
  )?;
  assert_eq!(value_to_string(&rt, sync), "no");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "boom");

  Ok(())
}

#[test]
fn async_generator_return_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var called = 0;
      var out = "";

      var p = Promise.resolve(1);
      var ctor = {};
      ctor[Symbol.species] = function C(executor) {
        called++;
        return new Promise(executor);
      };
      p.constructor = ctor;

      async function* g(){}
      g().return(p).then(r => { out = String(r.value); });
    "#,
  )?;

  assert_eq!(rt.exec_script("called")?, Value::Number(0.0));
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("called")?, Value::Number(0.0));
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1");

  Ok(())
}

#[test]
fn async_generator_return_promise_resolve_runs_constructor_getter_before_microtasks(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var log = "";
      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { log += "c"; return Promise; } });

      async function* g(){}
      g().return(p);

      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "c");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  Ok(())
}

#[test]
fn async_generator_return_constructor_getter_throw_rejects_return_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let sync = rt.exec_script(
    r#"
      var out = "";
      var sync = "no";

      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { throw "boom"; } });

      async function* g(){}
      var it = g();

      var pr;
      try {
        pr = it.return(p);
      } catch (e) {
        sync = e;
      }

      if (pr !== undefined) {
        pr.then(
          function () { out = "fulfilled"; },
          function (e) { out = e; },
        );
      }

      sync
    "#,
  )?;
  assert_eq!(value_to_string(&rt, sync), "no");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "boom");

  Ok(())
}
