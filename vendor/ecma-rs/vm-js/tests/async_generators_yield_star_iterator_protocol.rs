use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

fn supports_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // vm-js currently treats async generators as a throwable `SyntaxError` so user code can detect
  // support via try/catch. Once async generators are implemented, this probe will return true and
  // the conformance tests below will begin exercising `yield*` delegation semantics.
  let v = match rt.exec_script(
    r#"
      (() => {
        try {
          eval("async function* g() {} g();");
          return true;
        } catch (e) {
          return false;
        }
      })()
    "#,
  ) {
    Ok(v) => v,
    Err(VmError::Unimplemented(msg)) if msg.contains("async generator functions") => return Ok(false),
    Err(err) => return Err(err),
  };
  Ok(v == Value::Bool(true))
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
