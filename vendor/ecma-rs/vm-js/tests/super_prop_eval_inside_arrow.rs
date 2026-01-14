use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn direct_eval_inside_arrow_inside_method_allows_super_property_access() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          m() {
            return () => eval('super.toString');
          }
        }
        const f = new C().m();
        f() === Object.prototype.toString
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
