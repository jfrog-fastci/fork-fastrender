use vm_js::{Heap, HeapLimits, Intrinsics, JsRuntime, PropertyKey, Scope, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators allocate generator state + multiple Promises/microtasks. Use a larger heap
  // limit so these tests exercise `yield*` semantics rather than failing under heap pressure.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn is_async_generator_syntax_unsupported(
  scope: &mut Scope<'_>,
  intr: &Intrinsics,
  err: &VmError,
) -> Result<bool, VmError> {
  let thrown = match err {
    VmError::Throw(v) => *v,
    VmError::ThrowWithStack { value, .. } => *value,
    _ => return Ok(false),
  };
  let Value::Object(obj) = thrown else {
    return Ok(false);
  };

  // Root the error object across message property access.
  let mut scope = scope.reborrow();
  scope.push_root(thrown)?;

  if scope.heap().object_prototype(obj)? != Some(intr.syntax_error_prototype()) {
    return Ok(false);
  }

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let message = scope.heap().object_get_own_data_property_value(obj, &message_key)?;
  let Some(Value::String(message_s)) = message else {
    return Ok(false);
  };

  Ok(scope.heap().get_string(message_s)?.to_utf8_lossy() == "async generator functions")
}

fn feature_detect_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  let intr = *rt.realm().intrinsics();
  match rt.exec_script("async function* __ag_support() {}") {
    Ok(_) => Ok(true),
    Err(err) => {
      let mut scope = rt.heap.scope();
      if is_async_generator_syntax_unsupported(&mut scope, &intr, &err)? {
        return Ok(false);
      }
      Err(err)
    }
  }
}

#[test]
fn yield_star_return_delegates_to_delegate_return_and_awaits_final_value() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

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
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

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
