use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Use an aggressively low GC threshold so these tests exercise code paths where `IteratorClose`
  // allocates (and may trigger GC) after an error has been captured.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn for_await_of_sync_iterator_close_error_is_ignored_on_promise_resolve_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
       var out = null;
       var closed = false;

       var thenable = {};
       Object.defineProperty(thenable, "then", {
         get: function () { throw { tag: "then" }; },
       });

      var iterable = {};
       iterable[Symbol.iterator] = function () {
         return {
           next: function () { return { value: thenable, done: false }; },
           get "return"() { closed = true; throw "close"; },
         };
       };

       (async function () {
         try {
           for await (const _ of iterable) {}
         } catch (e) {
           out = (e && e.tag) || e;
         }
       })();

       out
     "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "then");

  let closed = rt.exec_script("closed")?;
  assert_eq!(closed, Value::Bool(true));
  Ok(())
}

#[test]
fn for_await_of_sync_iterator_close_error_is_ignored_on_awaited_value_rejection() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
       var out = null;
       var closed = false;

       var iterable = {};
       iterable[Symbol.iterator] = function () {
         return {
           next: function () { return { value: Promise.reject({ tag: "reason" }), done: false }; },
           return: function () { closed = true; throw "close"; },
         };
       };

       (async function () {
         try {
           for await (const _ of iterable) {}
         } catch (e) {
           out = (e && e.tag) || e;
         }
       })();

       out
     "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "reason");

  let closed = rt.exec_script("closed")?;
  assert_eq!(closed, Value::Bool(true));
  Ok(())
}
