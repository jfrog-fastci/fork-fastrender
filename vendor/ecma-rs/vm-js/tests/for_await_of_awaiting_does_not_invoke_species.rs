use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` exercises async iteration + Promise/job queuing. Use a slightly larger heap to
  // avoid spurious `VmError::OutOfMemory` failures as vm-js grows its builtin surface area.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn for_await_of_does_not_invoke_species_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();

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

      (async function () {
        for await (const x of [p]) {
          out = String(x);
        }
      })();
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
fn for_await_of_promise_resolve_runs_constructor_getter_before_microtasks() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `for await...of` over a sync iterable uses `AsyncFromSyncIterator`, which must `Await(value)`
  // for each iterator result. Like `await`, this must perform `Get(promise, "constructor")`
  // synchronously even when `value` is already a Promise.
  let value = rt.exec_script(
    r#"
      var log = "";
      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { log += "c"; return Promise; } });

      (async function () {
        for await (const _x of [p]) {}
      })();

      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "c");

  // Flush any queued promise jobs from the loop setup.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  Ok(())
}

#[test]
fn for_await_of_constructor_getter_throw_rejects_async_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // If `PromiseResolve` throws (e.g. user-defined `constructor` getter), the async function promise
  // must be rejected rather than the call throwing synchronously.
  let sync = rt.exec_script(
    r#"
      var out = "";
      var sync = "no";

      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { throw "boom"; } });

      var pr;
      try {
        pr = (async function () {
          for await (const _x of [p]) {}
        })();
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
fn for_await_of_close_constructor_getter_runs_once() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `AsyncIteratorClose` awaits the iterator `return()` result. That uses `Await`, which must perform
  // `Get(promise, "constructor")` exactly once.
  //
  // Regression test: vm-js previously invoked the constructor getter twice (once during
  // `AsyncIteratorClose` and again while scheduling the outer `await` suspension).
  rt.exec_script(
    r#"
      globalThis.calls = 0;
      globalThis.result = -1;

      const p = Promise.resolve({});
      Object.defineProperty(p, "constructor", {
        get() { globalThis.calls++; return Promise; }
      });

      const iterable = {
        [Symbol.asyncIterator]() {
          let i = 0;
          return {
            next() {
              if (i++ === 0) return Promise.resolve({ value: 1, done: false });
              return Promise.resolve({ done: true });
            },
            return() { return p; },
          };
        }
      };

      (async function () {
        for await (const _x of iterable) { break; }
        globalThis.result = globalThis.calls;
      })();
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("result")?, Value::Number(1.0));
  Ok(())
}
