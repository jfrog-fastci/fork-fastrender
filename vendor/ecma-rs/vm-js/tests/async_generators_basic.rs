use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator tests allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn basic_yield_sequencing() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var actual = [];

      async function* g() {
        yield 1;
        yield 2;
      }

      var iter = g();
      var p1 = iter.next();
      actual.push(p1 instanceof Promise);

      async function run() {
        var r1 = await p1;
        actual.push([r1.value, r1.done]);

        var r2 = await iter.next();
        actual.push([r2.value, r2.done]);

        var r3 = await iter.next();
        actual.push([r3.value === undefined ? "undefined" : r3.value, r3.done]);
      }

      run();
      JSON.stringify(actual)
    "#,
  )?;

  // The first `next()` must synchronously return a Promise.
  assert_eq!(value_to_utf8(&rt, value), r#"[true]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_utf8(&rt, value),
    r#"[true,[1,false],[2,false],["undefined",true]]"#
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );

  Ok(())
}

#[test]
fn yield_awaits_operand_fulfill() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var actual = [];

      async function* g() {
        yield Promise.resolve(7);
      }

      var iter = g();
      async function run() {
        var r1 = await iter.next();
        actual.push([r1.value, r1.done]);

        var r2 = await iter.next();
        actual.push([r2.value === undefined ? "undefined" : r2.value, r2.done]);
      }

      run();
      JSON.stringify(actual)
    "#,
  )?;

  assert_eq!(value_to_utf8(&rt, value), "[]");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, value), r#"[[7,false],["undefined",true]]"#);
  Ok(())
}

#[test]
fn yield_awaits_operand_reject_closes_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var actual = [];
      var error = {};

      async function* g() {
        yield Promise.reject(error);
        actual.push("unreachable");
      }

      var iter = g();
      async function run() {
        try {
          await iter.next();
          actual.push(false);
        } catch (e) {
          actual.push(e === error);
        }

        var r2 = await iter.next();
        actual.push([r2.value === undefined ? "undefined" : r2.value, r2.done]);
      }

      run();
      JSON.stringify(actual)
    "#,
  )?;

  assert_eq!(value_to_utf8(&rt, value), "[]");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, value), r#"[true,["undefined",true]]"#);
  Ok(())
}

// Port of test262: `test/language/statements/async-generator/yield-star-promise-not-unwrapped.js`
#[test]
fn yield_star_does_not_unwrap_promise_values_from_manual_async_iterators() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var actual = [];
      var innerPromise = Promise.resolve("unwrapped value");

      var asyncIter = {
        [Symbol.asyncIterator]() {
          return this;
        },
        next() {
          return {
            done: false,
            value: innerPromise,
          };
        },
        get return() {
          throw ".return should not be accessed";
        },
        get throw() {
          throw ".throw should not be accessed";
        },
      };

      async function* f() {
        yield* asyncIter;
      }

      f().next().then(v => {
        actual.push(v.value === innerPromise);
      });

      JSON.stringify(actual)
    "#,
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[]");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, value), "[true]");
  Ok(())
}

// Port of test262: `test/language/statements/async-generator/yield-return-then-getter-ticks.js`
#[test]
fn return_thenable_then_getter_tick_ordering() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var actual = [];

      async function* f() {
        actual.push("start");
        yield 123;
        actual.push("stop - never reached");
      }

      Promise.resolve(0)
        .then(() => actual.push("tick 1"))
        .then(() => actual.push("tick 2"));

      var it = f();
      it.next();
      it.return({
        get then() {
          actual.push("get then");
        }
      });

      JSON.stringify(actual)
    "#,
  )?;

  // `actual.push("start")` must happen before any queued microtasks run.
  assert_eq!(value_to_utf8(&rt, value), r#"["start"]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_utf8(&rt, value),
    r#"["start","tick 1","get then","tick 2"]"#
  );
  Ok(())
}

