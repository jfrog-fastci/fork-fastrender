use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn arguments_is_iterable_via_spread_and_for_of() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function f() {
          var sum = 0;
          for (var x of arguments) sum += x;
          var a = [...arguments];
          return sum === 6 && a.length === 3 && a[0] === 1 && a[1] === 2 && a[2] === 3;
        }
        f(1,2,3)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn arguments_symbol_iterator_is_array_values_and_is_own_property() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function f() {
          var same = arguments[Symbol.iterator] === Array.prototype[Symbol.iterator];
          var desc = Object.getOwnPropertyDescriptor(arguments, Symbol.iterator);
          var attrsOk = desc && desc.writable === true && desc.enumerable === false && desc.configurable === true;
          var inBefore = (Symbol.iterator in arguments);
          var delOk = delete arguments[Symbol.iterator];
          var inAfter = (Symbol.iterator in arguments);
          return same && attrsOk && inBefore === true && delOk === true && inAfter === false;
        }
        f(1,2,3)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn strict_mode_arguments_is_iterable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function f() {
          'use strict';
          var a = [...arguments];
          return a.length === 3 && a[0] === 1 && a[1] === 2 && a[2] === 3;
        }
        f(1,2,3)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

