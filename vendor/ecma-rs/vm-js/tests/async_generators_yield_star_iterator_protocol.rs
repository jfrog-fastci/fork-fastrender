use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

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

fn supports_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Detect runtime support (call semantics), not just parsing/prototype wiring.
  match rt.exec_script("async function* __ag_support() {} void __ag_support();") {
    Ok(_) => Ok(true),
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn yield_star_prefers_async_iterator_method() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !supports_async_generators(&mut rt)? {
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
  if !supports_async_generators(&mut rt)? {
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
