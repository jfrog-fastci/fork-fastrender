use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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
fn for_await_of_next_throw_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
       var out = "";
       var returnCalls = 0;

       const iterable = {};
       iterable[Symbol.asyncIterator] = function () {
         return {
           next() {
             throw "boom";
           },
           return() {
             returnCalls++;
             return { done: true };
           },
         };
       };

       async function f() {
         for await (const x of iterable) {
           // Never reached.
           out = "bad";
         }
       }

       f().then(function () { out = "bad"; }, function (e) { out = e; });
       out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");
  assert_eq!(rt.exec_script("returnCalls")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint: {:?}",
    rt.vm.microtask_queue()
  );
  Ok(())
}

#[test]
fn for_await_of_done_getter_throw_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
       var out = "";
       var returnCalls = 0;

       const iterable = {};
       iterable[Symbol.asyncIterator] = function () {
         return {
           next() {
             return {
               get done() {
                 throw "boom";
               },
               value: 1,
             };
           },
           return() {
             returnCalls++;
             return { done: true };
           },
         };
       };

       async function f() {
         for await (const x of iterable) {
           // Never reached.
           out = "bad";
         }
       }

       f().then(function () { out = "bad"; }, function (e) { out = e; });
       out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");
  assert_eq!(rt.exec_script("returnCalls")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint: {:?}",
    rt.vm.microtask_queue()
  );
  Ok(())
}

#[test]
fn for_await_of_value_getter_throw_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return {
              done: false,
              get value() {
                throw "boom";
              },
            };
          },
          return() {
            returnCalls++;
            return { done: true };
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          // Never reached.
          out = "bad";
        }
      }

      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");
  assert_eq!(rt.exec_script("returnCalls")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}
