use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

// Ported from test262:
// `test/language/statements/for-await-of/iterator-close-non-throw-get-method-abrupt.js`
#[test]
fn for_await_of_break_get_method_abrupt_overrides_completion() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var getterCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            getterCalls++;
            throw "close";
          },
        };
      };

      async function f() {
        for await (const _ of iterable) {
          break;
        }
        return "ok";
      }

      f().then(
        v => { out = v; },
        e => { out = e; }
      );

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "close");

  let getter_calls = rt.exec_script("getterCalls")?;
  assert_eq!(getter_calls, Value::Number(1.0));

  Ok(())
}

// Ported from test262:
// `test/language/statements/for-await-of/iterator-close-non-throw-get-method-non-callable.js`
#[test]
fn for_await_of_break_get_method_non_callable_overrides_completion() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var getterCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            getterCalls++;
            return 1;
          },
        };
      };

      async function f() {
        for await (const _ of iterable) {
          break;
        }
        return "ok";
      }

      f().then(
        v => { out = v; },
        e => { out = (e && e.name) || e; }
      );

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "TypeError");

  let getter_calls = rt.exec_script("getterCalls")?;
  assert_eq!(getter_calls, Value::Number(1.0));

  Ok(())
}

// Ported from test262:
// `test/language/statements/for-await-of/iterator-close-non-throw-get-method-is-null.js`
#[test]
fn for_await_of_break_get_method_null_completes_normally() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var getterCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            getterCalls++;
            return null;
          },
        };
      };

      async function f() {
        for await (const _ of iterable) {
          break;
        }
        return "ok";
      }

      f().then(
        v => { out = v; },
        e => { out = (e && e.name) || e; }
      );

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");

  let getter_calls = rt.exec_script("getterCalls")?;
  assert_eq!(getter_calls, Value::Number(1.0));

  Ok(())
}

// Ported from test262:
// `test/language/statements/for-await-of/iterator-close-throw-get-method-abrupt.js`
#[test]
fn for_await_of_throw_suppresses_get_method_abrupt() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = false;
      var closed = false;

      var bodyError = new Error("body");

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            closed = true;
            throw "close";
          },
        };
      };

      async function f() {
        for await (const _ of iterable) {
          throw bodyError;
        }
      }

      f().then(
        () => { out = false; },
        e => { out = e === bodyError; }
      );

      out
    "#,
  )?;
  assert_eq!(value, Value::Bool(false));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Bool(true));

  let closed = rt.exec_script("closed")?;
  assert_eq!(closed, Value::Bool(true));

  Ok(())
}

// Ported from test262:
// `test/language/statements/for-await-of/iterator-close-throw-get-method-non-callable.js`
#[test]
fn for_await_of_throw_suppresses_get_method_non_callable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = false;
      var closed = false;

      var bodyError = new Error("body");

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            closed = true;
            return 1;
          },
        };
      };

      async function f() {
        for await (const _ of iterable) {
          throw bodyError;
        }
      }

      f().then(
        () => { out = false; },
        e => { out = e === bodyError; }
      );

      out
    "#,
  )?;
  assert_eq!(value, Value::Bool(false));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Bool(true));

  let closed = rt.exec_script("closed")?;
  assert_eq!(closed, Value::Bool(true));

  Ok(())
}
