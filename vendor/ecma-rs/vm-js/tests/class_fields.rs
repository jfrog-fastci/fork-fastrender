use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn instance_fields_define_own_enumerable_properties_in_source_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C { x = 1; y; }
        var o = new C();
        var dx = Object.getOwnPropertyDescriptor(o, "x");
        var dy = Object.getOwnPropertyDescriptor(o, "y");
        dx.value === 1 &&
          dy.value === undefined &&
          dx.writable === true && dx.enumerable === true && dx.configurable === true &&
          dy.writable === true && dy.enumerable === true && dy.configurable === true &&
          Object.keys(o).join(",") === "x,y"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn static_fields_define_own_enumerable_properties_on_constructor() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C { static z = 2; }
        var dz = Object.getOwnPropertyDescriptor(C, "z");
        C.z === 2 &&
          dz.value === 2 &&
          dz.writable === true && dz.enumerable === true && dz.configurable === true &&
          Object.keys(C).indexOf("z") !== -1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn base_constructor_body_sees_instance_fields_initialized() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          x = 1;
          constructor() { this.y = this.x; }
        }
        new C().y === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_instance_fields_initialize_after_super_returns() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { constructor() { this.seen = this.x; } }
        class D extends B {
          x = 1;
          constructor() {
            super();
            this.after = this.x;
          }
        }
        var d = new D();
        d.seen === undefined && d.after === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn computed_field_names_are_evaluated_once_at_class_definition() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var keyCount = 0;
      var initCount = 0;
      function k() { keyCount++; return "x"; }
      function v() { initCount++; return initCount; }

      class C { [k()] = v(); }
      var a = new C();
      var b = new C();
      keyCount === 1 && initCount === 2 && a.x === 1 && b.x === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn private_methods_are_available_to_field_initializers_regardless_of_source_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          x = this.#m();
          #m() { return 5; }
        }
        new C().x === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
