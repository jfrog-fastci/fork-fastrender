use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_new_parenthesized_call_yield_in_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
    r#"
      function f(x) {
        if (new.target) throw "wrong"; // should not be constructed directly
        function C() { this.x = x; }
        return C;
      }
      function* g() {
        var obj = new (f(yield 1));
        return obj.x;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(42);
      r1.value === 1 && r1.done === false && r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
