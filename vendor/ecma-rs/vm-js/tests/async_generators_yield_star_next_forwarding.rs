use vm_js::{Heap, HeapLimits, Intrinsics, JsRuntime, PropertyKey, Scope, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator `yield*` uses Promise jobs and async iterator protocol wiring. Use a slightly
  // larger heap than the default 1MiB used by many unit tests so we exercise delegation semantics
  // rather than heap pressure.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn is_async_generator_syntax_unsupported(
  scope: &mut Scope<'_>,
  intr: &Intrinsics,
  err: &VmError,
) -> Result<bool, VmError> {
  if let VmError::Unimplemented(msg) = err {
    return Ok(msg.contains("async generator functions"));
  }

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
  match rt.exec_script(
    r#"
      (() => {
        async function* g() { yield 1; }
        g();
        return true;
      })()
    "#,
  ) {
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
fn yield_star_forwards_next_values_and_always_calls_delegate_next_with_one_argument(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = r#"
    var nextArgLens = [];
    var nextArgs = [];

    var iter = {
      i: 0,
      next: function (v) {
        nextArgLens.push(arguments.length);
        nextArgs.push(v);
        this.i++;
        if (this.i === 1) return Promise.resolve({ value: 'a', done: false });
        if (this.i === 2) return Promise.resolve({ value: 'b', done: false });
        return Promise.resolve({ value: 99, done: true });
      },
    };
    iter[Symbol.asyncIterator] = function () { return this; };

    var out = false;
    async function test() {
      async function* g() { return yield* iter; }
      var it = g();

      var r1 = await it.next('ignored');
      var r2 = await it.next();
      var r3 = await it.next(123);

      return (
        r1.value === 'a' && r1.done === false &&
        r2.value === 'b' && r2.done === false &&
        r3.value === 99 && r3.done === true &&
        nextArgLens.join(',') === '1,1,1' &&
        nextArgs[0] === undefined &&  // first next arg ignored by generator start
        nextArgs[1] === undefined &&
        nextArgs[2] === 123
      );
    }
    test().then(v => { out = v; });
    out
  "#;

  // Promise jobs have not run yet, so `out` should still be `false`.
  let value = rt.exec_script(script)?;
  assert_eq!(value, Value::Bool(false));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
