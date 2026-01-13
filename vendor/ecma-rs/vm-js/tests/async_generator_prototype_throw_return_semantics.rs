use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

// Async generator conformance tests allocate Promises and Promise jobs. Use a slightly larger heap
// to avoid spurious `VmError::OutOfMemory` failures as vm-js grows its builtin surface area.
const HEAP_BYTES: usize = 4 * 1024 * 1024;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(HEAP_BYTES, HEAP_BYTES));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_generator_throw_on_suspended_start_rejects_and_completes_without_executing_body(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = "";
      async function* g() { log += "body"; yield 1; }
      var it = g();

      it.throw(42).then(
        function () { log += "bad"; },
        function (e) { log += "catch:" + e; }
      );

      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "catch:42");

  rt.exec_script(
    r#"
      it.next().then(function (r) {
        log += "|done:" + r.done + ",value:" + r.value;
      });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(
    value_to_string(&rt, log),
    "catch:42|done:true,value:undefined"
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_throw_on_completed_generator_rejects_with_argument() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = -1;
      async function* g() { yield 1; }
      var it = g();

      it.next()
        .then(function () { return it.next(); }) // complete
        .then(function () { return it.throw(7); })
        .then(
          function () { out = 123; },
          function (e) { out = e; }
        );

      out
    "#,
  )?;
  assert_eq!(value, Value::Number(-1.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(7.0));
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_throw_can_be_caught_inside_generator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = "";
      async function* g() {
        try {
          yield 1;
        } catch (e) {
          yield e;
        }
        return 9;
      }

      var it = g();
      it.next()
        .then(function (r1) {
          log += r1.value + "," + r1.done;
          return it.throw(5);
        })
        .then(function (r2) {
          log += "|" + r2.value + "," + r2.done;
          return it.next();
        })
        .then(
          function (r3) { log += "|" + r3.value + "," + r3.done; },
          function (e) { log += "bad:" + e; }
        );
      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "1,false|5,false|9,true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_return_on_suspended_start_resolves_and_awaits_argument_without_executing_body(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = "";
      async function* g() { log += "body"; yield 1; }
      var it = g();

      it.return(Promise.resolve("x")).then(function (r) {
        log += r.value + ":" + r.done;
      });

      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "x:true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_return_on_completed_generator_resolves_to_done_true_with_awaited_argument(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function* g() { yield 1; }
      var it = g();

      it.next()
        .then(function () { return it.next(); }) // complete
        .then(function () { return it.return(Promise.resolve("y")); })
        .then(
          function (r) { out = r.value + ":" + r.done; },
          function (e) { out = "bad:" + e; }
        );

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "y:true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_first_next_argument_is_ignored() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function* g() { const x = yield 1; return x; }
      var it = g();

      it.next("ignored")
        .then(function (r1) {
          out += r1.value + ":" + r1.done;
          return it.next("sent");
        })
        .then(
          function (r2) { out += "|" + r2.value + ":" + r2.done; },
          function (e) { out = "bad:" + e; }
        );

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1:false|sent:true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

