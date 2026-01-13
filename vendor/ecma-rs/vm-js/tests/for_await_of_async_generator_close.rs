use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` over async generator objects exercises async iteration + Promise/job queuing.
  // With ongoing vm-js builtin growth, a 1MiB heap can be too tight and cause spurious
  // `VmError::OutOfMemory` failures that are not relevant to the semantics being tested here.
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
  // vm-js historically parsed `async function*` but deliberately rejected it at runtime (via a
  // throwable SyntaxError) while async generator semantics were unimplemented. These tests should
  // start running automatically once that support lands.
  let value = rt.exec_script(
    r#"
      var supported = true;
      try {
        var f = (async function* () { yield 1; });
        void f;
      } catch (e) {
        // Only treat the known feature-detection SyntaxError as "unsupported". Any other exception
        // should fail the test so we don't accidentally mask bugs once async generators exist.
        if (e && e.name === "SyntaxError" && String(e.message).includes("async generator")) {
          supported = false;
        } else {
          throw e;
        }
      }
      supported
    "#,
  )?;
  Ok(value == Value::Bool(true))
}

#[test]
fn for_await_break_closes_async_generator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = "";

      async function* gen() {
        try {
          yield 1;
          yield 2;
        } finally {
          log += "F";
        }
      }

      async function run() {
        for await (const x of gen()) {
          break;
        }
        return log;
      }

      run().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "F");
  Ok(())
}

#[test]
fn for_await_throw_closes_async_generator_before_catch() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = "";

      async function* gen() {
        try {
          yield 1;
          yield 2;
        } finally {
          log += "F";
        }
      }

      async function run() {
        try {
          for await (const x of gen()) {
            throw "boom";
          }
        } catch (e) {}
        return log;
      }

      run().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  let out = value_to_string(&rt, out);
  assert!(
    out.contains('F'),
    "expected `finally` to run and write 'F', got {out:?}"
  );
  Ok(())
}
