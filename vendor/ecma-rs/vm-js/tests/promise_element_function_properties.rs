use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
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
fn promise_all_resolve_element_function_name_is_empty() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
var resolveElementFunction;
var thenable = { then: function(fulfill) { resolveElementFunction = fulfill; } };

function NotPromise(executor) { executor(function() {}, function() {}); }
NotPromise.resolve = function(v) { return v; };

Promise.all.call(NotPromise, [thenable]);
resolveElementFunction.name;
"#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "");
}

#[test]
fn promise_all_settled_element_function_names_are_empty() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
var onFulfilled;
var onRejected;
var thenable = { then: function(f, r) { onFulfilled = f; onRejected = r; } };

function NotPromise(executor) { executor(function() {}, function() {}); }
NotPromise.resolve = function(v) { return v; };

Promise.allSettled.call(NotPromise, [thenable]);
onFulfilled.name + "," + onRejected.name;
"#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, ",");
}

#[test]
fn promise_any_reject_element_function_name_is_empty() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
var rejectElementFunction;
var thenable = { then: function(_resolve, reject) { rejectElementFunction = reject; } };

function NotPromise(executor) { executor(function() {}, function() {}); }
NotPromise.resolve = function(v) { return v; };

Promise.any.call(NotPromise, [thenable]);
rejectElementFunction.name;
"#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "");
}

