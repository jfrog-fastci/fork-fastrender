use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators allocate generator state + multiple Promises/microtasks. Use a larger heap
  // limit so these tests exercise `yield*` semantics rather than failing under heap pressure.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn yield_star_return_delegates_to_delegate_return_and_awaits_final_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var ok = false;
      var done = false;
      var error = null;

      var returnCalls = 0;
      var returnArg = null;

      var delegate = {
        next() { return { value: 1, done: false }; },
        return(v) {
          returnCalls++;
          returnArg = v;
          // Ensure the outer async generator awaits the delegate's final completion value.
          return Promise.resolve({ value: Promise.resolve(99), done: true });
        },
        [Symbol.asyncIterator]() { return this; },
      };

      async function* g() { return yield* delegate; }
      var it = g();

      async function run() {
        try {
          var r1 = await it.next();
          var r2 = await it.return("X");
          ok =
            r1.value === 1 && r1.done === false &&
            r2.value === 99 && r2.done === true &&
            returnCalls === 1 && returnArg === "X";
        } catch (e) {
          error = e;
        }
        done = true;
      }

      run();
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script("done && ok && error === null")?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_return_without_delegate_return_completes_with_outer_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var ok = false;
      var done = false;
      var error = null;

      var delegate = {
        next() { return { value: 1, done: false }; },
        [Symbol.asyncIterator]() { return this; },
      };

      async function* g() { return yield* delegate; }
      var it = g();

      async function run() {
        try {
          var r1 = await it.next();
          var r2 = await it.return(Promise.resolve("Y"));
          ok =
            r1.value === 1 && r1.done === false &&
            r2.value === "Y" && r2.done === true;
        } catch (e) {
          error = e;
        }
        done = true;
      }

      run();
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script("done && ok && error === null")?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

