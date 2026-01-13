use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

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

#[test]
fn async_generator_yield_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
  if !_async_generator_support::supports_async_generators(&mut rt)? {
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
