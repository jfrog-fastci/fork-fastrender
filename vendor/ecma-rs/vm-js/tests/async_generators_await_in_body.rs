use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators combine generator suspension (`yield`) with async suspension (`await`). These
  // tests exercise Promise/job machinery and therefore require a slightly larger heap to avoid
  // spurious `VmError::OutOfMemory` failures as vm-js grows its builtin surface area.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn async_generators_supported(rt: &mut JsRuntime) -> Result<bool, VmError> {
  let value = rt.exec_script(
    r#"
      try {
        // vm-js currently throws a *catchable* SyntaxError when encountering `async function*`
        // syntax, so test suites can feature-detect async generators.
        (async function* () {});
        true;
      } catch (e) {
        if (e && e.name === "SyntaxError" && e.message === "async generator functions") {
          false;
        } else {
          throw e;
        }
      }
    "#,
  )?;
  let Value::Bool(b) = value else {
    panic!("expected bool, got {value:?}");
  };
  Ok(b)
}

#[test]
fn internal_await_delays_first_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var log = "";
      var out = "";

      async function* g(){
        log += "s";
        await Promise.resolve().then(()=>{ log += "a"; });
        yield 1;
      }

      var it = g();
      it.next().then(r => { out = String(r.value) + ':' + String(r.done); });

      log + '|' + out
    "#,
  )?;
  // The generator starts running synchronously until it hits `await`. At this point it is
  // suspended on the internal await and has not yet produced a `yield`.
  assert_eq!(value_to_string(&rt, value), "s|");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, value), "sa");

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "1:false");

  Ok(())
}

#[test]
fn next_requests_are_queued_across_internal_await() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var results = [];

      async function* g(){
        await Promise.resolve();
        yield 1;
        yield 2;
      }

      var it = g();
      it.next().then(r=>results.push(r.value));
      it.next().then(r=>results.push(r.value));
      it.next().then(r=>results.push(r.done ? 'done' : 'notdone'));

      results.join(',')
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("results.join(',')")?;
  assert_eq!(value_to_string(&rt, value), "1,2,done");

  Ok(())
}
