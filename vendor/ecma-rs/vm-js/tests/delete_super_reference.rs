use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn delete_super_property_instance_method_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class B { m(){} }
      class D extends B {
        del() {
          try { delete super.m; return "no"; }
          catch (e) { return e.name; }
        }
      }
      new D().del()
      "#,
    )
    .unwrap();

  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn delete_super_property_static_method_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class B { static m(){} }
      class D extends B {
        static del() {
          try { delete super.m; return "no"; }
          catch (e) { return e.name; }
        }
      }
      D.del()
      "#,
    )
    .unwrap();

  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn delete_super_property_computed_member_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class B { m(){} }
      class D extends B {
        del() {
          try { delete super["m"]; return "no"; }
          catch (e) { return e.name; }
        }
      }
      new D().del()
      "#,
    )
    .unwrap();

  assert_value_is_utf8(&rt, value, "ReferenceError");
}
