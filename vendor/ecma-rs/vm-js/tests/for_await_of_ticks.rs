use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests assert microtask tick ordering for `for await...of`, and therefore allocate and
  // run a fair amount of Promise/job machinery. Use a slightly larger heap to avoid spurious
  // `VmError::OutOfMemory` failures as vm-js grows its builtin surface area.
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
fn ticks_with_sync_iter_resolved_promise_and_constructor_lookup() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var actual = [];

      async function f() {
        var p = Promise.resolve(0);
        actual.push("pre");
        for await (var x of [p]) {
          actual.push("loop");
        }
        actual.push("post");
      }

      Promise.resolve(0)
        .then(() => actual.push("tick 1"))
        .then(() => actual.push("tick 2"))
        .then(() => actual.push("tick 3"))
        .then(() => actual.push("tick 4"));

      Object.defineProperty(Promise.prototype, "constructor", {
        get() {
          actual.push("constructor");
          return Promise;
        },
        configurable: true,
      });

      f();
    "#,
  )?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_string(&rt, value),
    r#"["pre","constructor","constructor"]"#
  );

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_string(&rt, value),
    r#"["pre","constructor","constructor","tick 1","tick 2","loop","constructor","tick 3","tick 4","post"]"#
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );

  Ok(())
}

#[test]
fn ticks_with_async_iter_resolved_promise_and_constructor_lookup_none() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var actual = [];

      function toAsyncIterator(iterable) {
        return {
          [Symbol.asyncIterator]: function () {
            return iterable[Symbol.iterator]();
          },
        };
      }

      async function f() {
        var p = Promise.resolve(0);
        actual.push("pre");
        for await (var x of toAsyncIterator([p])) {
          actual.push("loop");
        }
        actual.push("post");
      }

      Promise.resolve(0)
        .then(() => actual.push("tick 1"))
        .then(() => actual.push("tick 2"));

      Object.defineProperty(Promise.prototype, "constructor", {
        get() {
          actual.push("constructor");
          return Promise;
        },
        configurable: true,
      });

      f();
    "#,
  )?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_string(&rt, value), r#"["pre"]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_string(&rt, value),
    r#"["pre","tick 1","loop","tick 2","post"]"#
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );

  Ok(())
}

#[test]
fn ticks_with_async_iter_resolved_promise_and_constructor_lookup_next_only() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var actual = [];

      function toAsyncIterator(iterable) {
        return {
          [Symbol.asyncIterator]: function () {
            var iter = iterable[Symbol.iterator]();
            return {
              next: function () {
                return Promise.resolve(iter.next());
              }
            };
          }
        };
      }

      async function f() {
        var p = Promise.resolve(0);
        actual.push("pre");
        for await (var x of toAsyncIterator([p])) {
          actual.push("loop");
        }
        actual.push("post");
      }

      Promise.resolve(0)
        .then(() => actual.push("tick 1"))
        .then(() => actual.push("tick 2"));

      Object.defineProperty(Promise.prototype, "constructor", {
        get() {
          actual.push("constructor");
          return Promise;
        },
        configurable: true,
      });

      f();
    "#,
  )?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_string(&rt, value), r#"["pre","constructor"]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_string(&rt, value),
    r#"["pre","constructor","tick 1","loop","constructor","tick 2","post"]"#
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );

  Ok(())
}
