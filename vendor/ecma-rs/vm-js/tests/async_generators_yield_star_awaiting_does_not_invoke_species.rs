use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator `yield*` allocates Promise jobs, async iterator wrapper state, and delegation
  // bookkeeping. Keep the heap limit large enough that these tests exercise conformance semantics
  // rather than heap pressure.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_generator_yield_star_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var called = 0;
      var out = "";

      // The promise returned by `next()` is already fulfilled, but `yield*` must still `Await` it,
      // and `Await` must not wrap it into a derived promise that consults `constructor[Symbol.species]`.
      var p1 = Promise.resolve({ value: 1, done: false });
      var p2 = Promise.resolve({ value: undefined, done: true });

      var ctor = {};
      ctor[Symbol.species] = function C(executor) {
        called++;
        return new Promise(executor);
      };

      p1.constructor = ctor;
      p2.constructor = ctor;

      var iter = {
        i: 0,
        next() { return (this.i++ === 0) ? p1 : p2; }
      };

      var obj = {};
      obj[Symbol.asyncIterator] = function () { return iter; };

      async function* g() { yield* obj; }
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
fn async_generator_yield_star_promise_resolve_runs_constructor_getter_before_microtasks(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  // Like `await`, async generator `yield*` awaits the promise returned by the delegate iterator's
  // methods, and must perform `Get(promise, "constructor")` synchronously even when the value is
  // already a Promise.
  let value = rt.exec_script(
    r#"
      var log = "";

      var p1 = Promise.resolve({ value: 1, done: false });
      var p2 = Promise.resolve({ value: undefined, done: true });
      Object.defineProperty(p1, "constructor", { get() { log += "c"; return Promise; } });

      var iter = {
        i: 0,
        next() { return (this.i++ === 0) ? p1 : p2; }
      };

      var obj = {};
      obj[Symbol.asyncIterator] = function () { return iter; };

      async function* g() { yield* obj; }
      g().next();

      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "c");

  // Flush any pending jobs from the `next()` call.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  Ok(())
}

#[test]
fn async_generator_yield_star_constructor_getter_throw_rejects_next_promise() -> Result<(), VmError> {
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

      var p1 = Promise.resolve({ value: 1, done: false });
      var p2 = Promise.resolve({ value: undefined, done: true });
      Object.defineProperty(p1, "constructor", { get() { throw "boom"; } });

      var iter = {
        i: 0,
        next() { return (this.i++ === 0) ? p1 : p2; }
      };

      var obj = {};
      obj[Symbol.asyncIterator] = function () { return iter; };

      async function* g() { yield* obj; }
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

