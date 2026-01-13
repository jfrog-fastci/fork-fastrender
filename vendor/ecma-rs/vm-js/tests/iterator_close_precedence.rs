use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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
fn close_throw_is_suppressed_for_body_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var it = {};
    it[Symbol.iterator] = function () {
      return {
        next: function () { return { value: 1, done: false }; },
        "return": function () { throw "close"; },
      };
    };
    var out;
    try { for (var x of it) { throw "body"; } } catch (e) { out = e; }
    out;
  "#,
  )?;
  // Per ECMA-262 `IteratorClose`, errors thrown while calling `iterator.return` are ignored for
  // throw completions (the original throw is preserved).
  assert_value_is_utf8(&rt, value, "body");
  Ok(())
}

#[test]
fn non_object_return_is_suppressed_for_body_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var it = {};
    it[Symbol.iterator] = function () {
      return {
        next: function () { return { value: 1, done: false }; },
        "return": function () { return 1; },
      };
    };
    var out;
    try { for (var x of it) { throw "body"; } } catch (e) { out = e; }
    out;
  "#,
  )?;
  assert_value_is_utf8(&rt, value, "body");
  Ok(())
}
