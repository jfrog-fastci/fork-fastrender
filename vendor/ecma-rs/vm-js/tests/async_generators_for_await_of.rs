use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` in async generators exercises async iteration + generator request queues +
  // Promise/job queuing. With ongoing vm-js builtin growth, a 1MiB heap can be too tight and cause
  // spurious `VmError::OutOfMemory` failures that are not relevant to the semantics being tested.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
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

fn async_generators_supported(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Detect runtime support (call semantics), not just parsing/prototype wiring.
  match rt.exec_script("async function* __ag_support() {} void __ag_support();") {
    Ok(_) => Ok(true),
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn for_await_of_inside_async_generator_yields_awaited_values() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function* g() {
          for await (const x of [Promise.resolve(1), 2]) {
            yield x;
          }
        }

        async function f() {
          const it = g();
          const r1 = await it.next();
          const r2 = await it.next();
          const r3 = await it.next();
          return (
            r1.value + "," + r1.done + "," +
            r2.value + "," + r2.done + "," +
            String(r3.value) + "," + r3.done
          );
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "1,false,2,false,undefined,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_break_inside_async_generator_awaits_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                // Side effect happens asynchronously to ensure `for await..of` awaits `return()`.
                return Promise.resolve().then(function () {
                  closed = true;
                  return { done: true };
                });
              },
            };
          };

          async function* g() {
            for await (const _x of iterable) {
              break;
            }
            yield closed;
          }

          const it = g();
          const r1 = await it.next();
          const r2 = await it.next();
          return String(r1.value) + "," + String(r1.done) + "," + String(r2.done);
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "true,false,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
