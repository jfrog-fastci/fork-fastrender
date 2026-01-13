use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_new_parenthesized_call_with_yield_is_not_new_call() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        function f(x) {
          return function C() { this.v = x; };
        }
        var o = new (f(yield 1))();
        return o.v;
      }

      var it = g();
      var r1 = it.next();
      var r2 = it.next(7);
      r1.value === 1 && r1.done === false &&
      r2.value === 7 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

