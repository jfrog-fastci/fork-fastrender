use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_destructuring_suppresses_iterator_close_throw_on_throw_completion() {
  let mut rt = new_runtime();

  let ok = rt
    .exec_script(
      r#"
      var returnCalled = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () {
            return { value: undefined, done: false };
          },
          return: function () {
            returnCalled = true;
            throw "return";
          },
        };
      };

      var caught;
      try {
        var [x = (() => { throw "default"; })()] = iterable;
      } catch (e) {
        caught = e;
      }

      caught === "default" && returnCalled === true
      "#,
    )
    .unwrap();

  assert_eq!(ok, Value::Bool(true));
}

#[test]
fn array_destructuring_suppresses_iterator_close_non_object_return_on_throw_completion() {
  let mut rt = new_runtime();

  let ok = rt
    .exec_script(
      r#"
      var returnCalled = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () {
            return { value: undefined, done: false };
          },
          return: function () {
            returnCalled = true;
            return 42;
          },
        };
      };

      var caught;
      try {
        var [x = (() => { throw "default"; })()] = iterable;
      } catch (e) {
        caught = e;
      }

      caught === "default" && returnCalled === true
      "#,
    )
    .unwrap();

  assert_eq!(ok, Value::Bool(true));
}

