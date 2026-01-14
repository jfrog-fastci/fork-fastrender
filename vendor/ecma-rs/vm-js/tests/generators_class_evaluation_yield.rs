use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_class_extends_expression_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            class Base {}
            var D = class extends (yield Base) {};
            return Object.getPrototypeOf(D.prototype) === Base.prototype;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false) return false;
          // Pass the yielded Base constructor back into the `yield` expression.
          var r2 = it.next(r1.value);
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
