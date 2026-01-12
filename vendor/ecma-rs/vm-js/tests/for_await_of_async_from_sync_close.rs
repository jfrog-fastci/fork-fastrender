use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` exercises async iteration + Promise/job queuing. With ongoing vm-js builtin
  // growth, a 1MiB heap can be too tight and cause spurious `VmError::OutOfMemory` failures that
  // are not relevant to the semantics being tested here.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn for_await_rejected_value_closes_iterator_and_preserves_original_rejection() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next() {
            return { value: Promise.reject("reject"), done: false };
          },
          return() {
            returnCalls++;
            // Non-object return value must not override the original rejection.
            return undefined;
          },
        };
      };

      (async function () {
        try {
          for await (const _ of iterable) {}
          out = "unexpected";
        } catch (e) {
          out = e;
        }
      })();

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "reject");

  let value = rt.exec_script("returnCalls")?;
  assert_eq!(value, Value::Number(1.0));

  Ok(())
}

#[test]
fn for_await_rejected_value_closes_iterator_and_runs_return_side_effect() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var finallyCount = 0;

      // vm-js does not execute generator functions yet; use an explicit iterator with a `return()`
      // side effect to model generator-finally behaviour.
      const iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next() {
            return { value: Promise.reject("reject"), done: false };
          },
          return() {
            finallyCount++;
            return { done: true };
          },
        };
      };

      (async function () {
        try {
          for await (const _ of iterable) {}
          out = "unexpected";
        } catch (e) {
          out = e;
        }
      })();

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "reject");

  let value = rt.exec_script("finallyCount")?;
  assert_eq!(value, Value::Number(1.0));

  Ok(())
}
