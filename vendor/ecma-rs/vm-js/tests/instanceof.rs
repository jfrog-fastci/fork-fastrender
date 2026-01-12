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
fn ordinary_instanceof_true_for_constructed_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function C(){}; var o=new C(); o instanceof C === true"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn ordinary_instanceof_false_for_other_object() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"function C(){}; ({} instanceof C) === false"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn instanceof_throws_type_error_when_prototype_is_not_object() {
  let mut rt = new_runtime();
  assert_throws_type_error(&mut rt, r#"function C(){}; C.prototype = 1; ({} instanceof C)"#);
}

#[test]
fn has_instance_override_is_called() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"function C(){}; C[Symbol.hasInstance] = function(){ return true; }; ({} instanceof C) === true"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn has_instance_override_is_called_for_non_callable_rhs_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var C = {}; C[Symbol.hasInstance] = function(){ return true; }; ({} instanceof C) === true"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn instanceof_throws_type_error_when_rhs_is_not_object() {
  let mut rt = new_runtime();
  assert_throws_type_error(&mut rt, r#"({} instanceof 1)"#);
}

#[test]
fn instanceof_throws_type_error_when_rhs_is_not_callable_and_has_no_hasinstance() {
  let mut rt = new_runtime();
  assert_throws_type_error(&mut rt, r#"({} instanceof ({}))"#);
}

#[test]
fn instanceof_throws_type_error_when_has_instance_is_not_callable() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"function C(){}; C[Symbol.hasInstance] = 1; ({} instanceof C)"#,
  );
}

#[test]
fn instanceof_throws_type_error_for_arrow_function_without_prototype() {
  let mut rt = new_runtime();
  assert_throws_type_error(&mut rt, r#"var f = () => {}; ({} instanceof f)"#);
}

#[test]
fn bound_function_instanceof_delegates_to_bound_target() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"function C(){}; var B = C.bind(null); var o = new C(); (o instanceof B) === true"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bound_function_instanceof_does_not_use_bound_target_has_instance() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function Target() {}
      Target[Symbol.hasInstance] = function () { return true; };
      var Bound = Target.bind(null);
      ({} instanceof Bound) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
