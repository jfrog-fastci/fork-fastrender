use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_assignment_infers_function_name_for_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ var f; f = (yield 0, function(){}); return f.name; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(123);
      r1.value === 0 && r1.done === false && r2.done === true && r2.value === "f"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_infers_function_name_for_property() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ var o = {}; o.m = (yield 0, function(){}); return o.m.name; }
      var it = g();
      it.next();
      var r = it.next(1);
      r.done === true && r.value === "m"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

