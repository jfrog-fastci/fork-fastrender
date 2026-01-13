use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_from_uses_this_constructor_for_iterables() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function C() { this.argLen = arguments.length; }
      var a = Array.from.call(C, [1, 2, 3]);
      a.argLen === 0 &&
        a.length === 3 &&
        a[0] === 1 &&
        a[2] === 3 &&
        Object.getPrototypeOf(a) === C.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_from_uses_this_constructor_for_array_like() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function C(len) { this.lenArg = len; this.argLen = arguments.length; }
      var items = { 0: "a", 1: "b", length: 2 };
      var a = Array.from.call(C, items);
      a.argLen === 1 &&
        a.lenArg === 2 &&
        a.length === 2 &&
        a[0] === "a" &&
        a[1] === "b" &&
        Object.getPrototypeOf(a) === C.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_from_non_constructor_creates_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var a = Array.from.call({}, [1, 2]);
      Array.isArray(a) &&
        a.length === 2 &&
        a[0] === 1 &&
        a[1] === 2 &&
        Object.getPrototypeOf(a) === Array.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

