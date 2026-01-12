use vm_js::{Heap, HeapLimits, JsRuntime, RootId, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_throws_type_error(rt: &mut JsRuntime, script: &str) {
  let err = rt.exec_script(script).unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));

  // Root the thrown value across any subsequent allocations / script runs.
  let root: RootId = rt.heap_mut().add_root(thrown).expect("root thrown value");

  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let type_error_proto = rt
    .exec_script("globalThis.TypeError.prototype")
    .expect("evaluate TypeError.prototype");
  let Value::Object(type_error_proto) = type_error_proto else {
    panic!("expected TypeError.prototype to be an object");
  };

  let thrown_proto = rt
    .heap()
    .object_prototype(thrown_obj)
    .expect("get thrown prototype");
  assert_eq!(thrown_proto, Some(type_error_proto));

  rt.heap_mut().remove_root(root);
}

#[test]
fn array_is_array_treats_proxy_to_array_as_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script("Array.isArray(new Proxy([], {})) === true")
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_is_array_treats_proxy_to_object_as_not_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script("Array.isArray(new Proxy({}, {})) === false")
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_is_array_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    "let r = Proxy.revocable([], {}); r.revoke(); Array.isArray(r.proxy)",
  );
}

