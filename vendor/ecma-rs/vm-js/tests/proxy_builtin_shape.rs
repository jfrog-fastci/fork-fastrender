use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn proxy_constructor_has_no_own_prototype_property() {
  let mut rt = new_runtime();

  let has_own = rt
    .exec_script(r#"Object.prototype.hasOwnProperty.call(Proxy, "prototype")"#)
    .unwrap();
  assert_eq!(has_own, Value::Bool(false));

  let proto_is_undef = rt.exec_script(r#"Proxy.prototype === undefined"#).unwrap();
  assert_eq!(proto_is_undef, Value::Bool(true));
}

