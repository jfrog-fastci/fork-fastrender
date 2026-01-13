use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

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

fn value_to_usize(value: Value) -> usize {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  usize::try_from(n as i64).expect("expected length to fit in usize")
}

fn run_microtask_step(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Borrow-splitting: running a job requires `&mut JsRuntime` (as `VmJobContext`) and `&mut dyn
  // VmHostHooks` (the microtask queue). Move the queue out of the VM temporarily so we can pass it
  // as the active hooks while still holding `&mut rt`.
  let mut hooks = std::mem::take(rt.vm.microtask_queue_mut());

  let result = (|| {
    if !hooks.begin_checkpoint() {
      return Ok(false);
    }
    let Some((_realm, job)) = hooks.pop_front() else {
      hooks.end_checkpoint();
      return Ok(false);
    };
    let res = job.run(rt, &mut hooks);
    hooks.end_checkpoint();
    res?;
    Ok(true)
  })();

  // Drain any Promise jobs that were enqueued into the VM-owned queue while it was moved out.
  while let Some((realm, job)) = rt.vm.microtask_queue_mut().pop_front() {
    hooks.enqueue_promise_job(job, realm);
  }
  *rt.vm.microtask_queue_mut() = hooks;

  result
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

  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

fn feature_detect_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Parse-level support for `async function*` isn't sufficient: vm-js can accept the syntax and
  // still surface `VmError::Unimplemented` once the generator is actually executed. Probe a minimal
  // `.next()` call so tests only activate when core async generator machinery exists.
  match rt.exec_script(
    r#"
      async function* __ag_support() { yield 1; }
      __ag_support().next();
    "#,
  ) {
    Ok(_) => {
      // Avoid leaking Promise jobs into subsequent assertions.
      rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
      Ok(true)
    }
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn basic_yield_sequencing() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }
 
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
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }
 
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
fn yield_awaits_thenable_operand_fulfill() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var actual = [];

      var thenable = {
        then(resolve, _reject) {
          actual.push("then");
          resolve(7);
        }
      };

      async function* g() {
        actual.push("start");
        yield thenable;
      }

      g().next().then(r => {
        actual.push([r.value, r.done]);
      });

      JSON.stringify(actual)
    "#,
  )?;

  // `then` must be called synchronously as part of awaiting the operand.
  assert_eq!(value_to_utf8(&rt, value), r#"["start","then"]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, value), r#"["start","then",[7,false]]"#);
  Ok(())
}

#[test]
fn yield_awaits_thenable_operand_reject_closes_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var actual = [];
      var error = {};

      var thenable = {
        then(_resolve, reject) {
          actual.push("then");
          reject(error);
        }
      };

      async function* g() {
        actual.push("start");
        yield thenable;
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
  assert_eq!(value_to_utf8(&rt, value), r#"["start","then"]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, value), r#"["start","then",true,["undefined",true]]"#);
  Ok(())
}

#[test]
fn yield_awaits_operand_reject_closes_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }
 
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
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }
 
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
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }
 
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

  // Step the microtask queue one job at a time so the test asserts ordering *across* microtask
  // turns, matching test262's "ticks" framing.
  //
  // - "tick 1" must occur before the thenable `then` getter is accessed.
  // - The `then` getter must be accessed before "tick 2".
  for (target_len, expected) in [
    (2, r#"["start","tick 1"]"#),
    (3, r#"["start","tick 1","get then"]"#),
    (4, r#"["start","tick 1","get then","tick 2"]"#),
  ] {
    for _ in 0..100 {
      let len = value_to_usize(rt.exec_script("actual.length")?);
      if len >= target_len {
        break;
      }
      let ran = run_microtask_step(&mut rt)?;
      assert!(ran, "expected microtask queue to have pending jobs");
    }

    let value = rt.exec_script("JSON.stringify(actual)")?;
    assert_eq!(value_to_utf8(&rt, value), expected);
  }
  Ok(())
}
