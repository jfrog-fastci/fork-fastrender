use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn new_target_is_undefined_in_plain_call() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function C(){ return new.target; } C()"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn new_target_is_constructor_in_construct_call() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function C(){ return new.target; } var x = new C(); x === C"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn new_target_is_not_propagated_into_nested_plain_calls() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function C(){
          function inner(){ return new.target; }
          this.ok = (inner() === undefined);
        }
        (new C()).ok === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
