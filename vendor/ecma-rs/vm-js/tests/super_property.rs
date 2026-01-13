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
fn super_method_call_uses_this_binding_as_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m(){ return this.v; } }
        class D extends B {
          constructor(){ super(); this.v = 1; }
          m(){ return super.m(); }
        }
        new D().m() === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_property_getter_setter_use_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          setX(v){ super["x"] = v; }
          getX(){ return super["x"]; }
        }
        var d = new D();
        d.setX(42);
        d.getX() === 42 && d._x === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn derived_ctor_super_prop_before_super_throws_reference_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {}
        class D extends B {
          constructor(){
            super.x;
            super();
          }
        }
        try { new D(); "no error"; } catch(e) { e.name + ":" + e.message }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(
    &rt,
    value,
    "ReferenceError:Must call super constructor in derived class before accessing 'this'",
  );
}

