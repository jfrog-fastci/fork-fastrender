use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator delegation via `yield*` will allocate Promises, microtask jobs, and iterator
  // wrapper state (`AsyncFromSyncIterator`). Keep the heap limit large enough that these tests
  // exercise conformance semantics rather than heap pressure.
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
fn yield_star_prefers_async_iterator_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var log = "";
      var out = null;

      var iter = {
        next() { log += "n"; return Promise.resolve({ value: 1, done: true }); }
      };

      var obj = {};
      obj[Symbol.asyncIterator] = function () { log += "a"; return iter; };
      obj[Symbol.iterator] = function () {
        log += "i";
        return { next() { throw "should not"; } };
      };

      async function* g() { return yield* obj; }

      g().next().then(r => { out = r.value; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "an");

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(1.0));
  Ok(())
}

#[test]
fn yield_star_uses_sync_iterator_protocol_for_arrays() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var log = "";
      var out = null;

      var saved = Array.prototype[Symbol.iterator];
      Array.prototype[Symbol.iterator] = function () {
        log += "i";
        return saved.call(this);
      };

      async function* g() { yield* [Promise.resolve(1)]; }
      g().next().then(r => { out = r.value; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "i");

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(1.0));
  Ok(())
}

#[test]
fn yield_star_yields_iterator_result_object_directly() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out = "";

      async function run() {
        var valueGetterCalls = 0;

        var iterResult = {};
        Object.defineProperty(iterResult, "done", { value: false });
        Object.defineProperty(iterResult, "value", {
          get: function () {
            valueGetterCalls++;
            return 1;
          }
        });
        iterResult.extra = 123;

        var nextCount = 0;
        var iterator = {
          [Symbol.asyncIterator]: function () { return this; },
          next: function () {
            nextCount++;
            if (nextCount === 1) return Promise.resolve(iterResult);
            return Promise.resolve({ value: 2, done: true });
          },
        };
        var iterable = {};
        iterable[Symbol.asyncIterator] = function () { return iterator; };

        async function* g() { yield* iterable; }

        var it = g();
        var r1 = await it.next();

        var ok1 =
          r1 === iterResult &&
          r1.extra === 123 &&
          valueGetterCalls === 0;

        var v = r1.value;
        var ok2 = v === 1 && valueGetterCalls === 1;

        var r2 = await it.next();
        var ok3 = r2.done === true && r2.value === undefined;

        return ok1 && ok2 && ok3;
      }

      run().then(
        function (v) { out = String(v); },
        function (e) { out = "err:" + ((e && e.name) || e); }
      );

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "true");
  Ok(())
}
