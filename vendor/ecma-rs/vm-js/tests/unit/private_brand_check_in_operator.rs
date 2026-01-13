use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn basic_true_false() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C { #x; hasX(o){ return #x in o; } }
        let c=new C();
        c.hasX(c) === true && c.hasX({}) === false
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn rhs_non_object_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        try {
          (new (class C{ #x; f(){ return #x in 1; } })()).f();
          false;
        } catch(e) {
          e.name === 'TypeError';
        }
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_does_not_forward_private_brand() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C { #x; hasX(o){ return #x in o; } }
        let c = new C();
        c.hasX(new Proxy(c, {})) === false
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

