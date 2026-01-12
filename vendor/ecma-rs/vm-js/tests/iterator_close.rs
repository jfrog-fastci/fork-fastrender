use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn iterator_close_calls_return_on_break() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        var i = 0;
        return {
          next: function () { return { value: i++, done: false }; },
          "return": function () { closed = true; return {}; }
        };
      };
      for (var x of iterable) { break; }
      closed
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn iterator_close_return_throw_overrides_break() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": function () { throw "close"; }
        };
      };
      for (var x of iterable) { break; }
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "close");
}

#[test]
fn iterator_close_return_non_object_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": function () { return 1; }
        };
      };
      try {
        for (var x of iterable) { break; }
      } catch (e) {
        ok = e && e.name === "TypeError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn iterator_close_calls_return_on_array_destructuring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        var i = 0;
        return {
          next: function () { return { value: i++, done: false }; },
          "return": function () { closed = true; return {}; }
        };
      };
      var x;
      var [x] = iterable;
      closed
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn iterator_close_return_throw_overrides_array_destructuring() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": function () { throw "close"; }
        };
      };
      var x;
      var [x] = iterable;
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "close");
}

#[test]
fn iterator_close_return_non_object_throws_type_error_in_array_destructuring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": function () { return 1; }
        };
      };
      try {
        var x;
        var [x] = iterable;
      } catch (e) {
        ok = e && e.name === "TypeError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn spread_step_error_does_not_invoke_iterator_close() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { throw "next"; }
        };
      };
      var xs = [...iterable];
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "next");

  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { throw "next"; },
          "return": function () { closed = true; return {}; }
        };
      };
      try {
        var xs = [...iterable];
      } catch (e) {}
      closed
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn iterator_close_get_method_throw_overrides_break() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          get "return"() { throw "getter"; }
        };
      };
      for (var x of iterable) { break; }
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "getter");
}

#[test]
fn iterator_close_get_method_throw_overrides_throw_completion() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          get "return"() { throw "getter"; }
        };
       };
       for (var x of iterable) { throw "body"; }
     "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "getter");
}

#[test]
fn iterator_close_get_method_non_callable_overrides_break() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": 1
        };
      };
      try {
        for (var x of iterable) { break; }
      } catch (e) {
        ok = e && e.name === "TypeError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn iterator_close_get_method_non_callable_overrides_throw_completion() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": 1
        };
      };
      try {
        for (var x of iterable) { throw "body"; }
      } catch (e) {
        ok = e && e.name === "TypeError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn iterator_close_get_method_throw_suppressed_on_throw_completion_in_array_destructuring_assignment() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          get "return"() { throw "close"; }
        };
      };

      var target = {};
      Object.defineProperty(target, "x", {
        set: function (v) { throw "assign"; },
      });

      [target.x] = iterable;
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "assign");
}

#[test]
fn iterator_step_error_does_not_invoke_iterator_close() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { throw "next"; },
          "return": function () { closed = true; return {}; }
        };
      };
      try {
        for (var x of iterable) {}
      } catch (e) {}
      closed
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn iterator_close_get_method_throw_suppressed_on_throw_completion_in_array_destructuring() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: undefined, done: false }; },
          get "return"() { throw "close"; }
        };
      };
      var x;
      var [x = (function () { throw "body"; })()] = iterable;
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "body");
}

#[test]
fn iterator_close_get_method_non_callable_suppressed_on_throw_completion_in_array_destructuring() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: undefined, done: false }; },
          "return": 1
        };
      };
      var x;
      var [x = (function () { throw "body"; })()] = iterable;
    "#,
    )
    .unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, thrown, "body");
}

#[test]
fn iterator_step_done_getter_error_does_not_invoke_iterator_close() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () {
            return {
              get done() { throw "done"; }
            };
          },
          "return": function () { closed = true; return {}; }
        };
      };
      try {
        for (var x of iterable) {}
      } catch (e) {}
      closed
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn iterator_value_getter_error_does_not_invoke_iterator_close() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () {
            return {
              done: false,
              get value() { throw "value"; }
            };
          },
          "return": function () { closed = true; return {}; }
        };
      };
      try {
        for (var x of iterable) {}
      } catch (e) {}
      closed
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}
