use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn find_last_length_abrupt_before_predicate_is_callable_check() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("[].findLast.call({ get length() { throw 'boom' } })")
    .expect_err("expected abrupt completion from length getter");

  let thrown = err.thrown_value().expect("expected a thrown value");
  assert_eq!(value_to_utf8(&rt, thrown), "boom");
  Ok(())
}

#[test]
fn find_last_index_length_abrupt_before_predicate_is_callable_check() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("[].findLastIndex.call({ get length() { throw 'boom' } })")
    .expect_err("expected abrupt completion from length getter");

  let thrown = err.thrown_value().expect("expected a thrown value");
  assert_eq!(value_to_utf8(&rt, thrown), "boom");
  Ok(())
}

#[test]
fn find_last_length_valueof_abrupt_before_predicate_is_callable_check() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("[].findLast.call({ length: { valueOf() { throw 'boom' } } })")
    .expect_err("expected abrupt completion from length valueOf");

  let thrown = err.thrown_value().expect("expected a thrown value");
  assert_eq!(value_to_utf8(&rt, thrown), "boom");
  Ok(())
}

#[test]
fn find_last_index_length_valueof_abrupt_before_predicate_is_callable_check() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("[].findLastIndex.call({ length: { valueOf() { throw 'boom' } } })")
    .expect_err("expected abrupt completion from length valueOf");

  let thrown = err.thrown_value().expect("expected a thrown value");
  assert_eq!(value_to_utf8(&rt, thrown), "boom");
  Ok(())
}

#[test]
fn find_last_missing_predicate_throws_type_error_after_length() -> Result<(), VmError> {
  for src in [
    "[].findLast.call({ length: 0 })",
    "[].findLastIndex.call({ length: 0 })",
  ] {
    let mut rt = new_runtime();
    let err = rt.exec_script(src).expect_err("expected a TypeError");

    let thrown = err.thrown_value().expect("expected a thrown value");
    let Value::Object(err_obj) = thrown else {
      panic!("expected a TypeError object, got {thrown:?}");
    };

    let type_error_proto = rt.realm().intrinsics().type_error_prototype();
    assert_eq!(rt.heap().object_prototype(err_obj)?, Some(type_error_proto));
  }

  Ok(())
}
