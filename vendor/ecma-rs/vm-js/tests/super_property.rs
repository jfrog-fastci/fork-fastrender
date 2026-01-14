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
fn super_property_in_base_class_method_reads_from_object_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {
          m() { return super.toString === Object.prototype.toString; }
        }
        new A().m()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_property_in_base_class_static_method_reads_from_function_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {
          static m() { return super.toString === Function.prototype.toString; }
        }
        A.m()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_in_arrow_closure_observes_dynamic_home_object_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var proto1 = {
          get x() { return this.tag + ":p1"; }
        };
        var proto2 = {
          get x() { return this.tag + ":p2"; }
        };

        class C {
          constructor() { this.tag = "t"; }
          m() { return () => super.x; }
        }

        Object.setPrototypeOf(C.prototype, proto1);
        var f = new C().m();
        Object.setPrototypeOf(C.prototype, proto2);

        f() === "t:p2"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_in_static_arrow_closure_observes_dynamic_home_object_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var proto1 = {
          get x() { return this.tag + ":p1"; }
        };
        var proto2 = {
          get x() { return this.tag + ":p2"; }
        };

        class C {
          static m() { return () => super.x; }
        }
        C.tag = "t";

        Object.setPrototypeOf(C, proto1);
        var f = C.m();
        Object.setPrototypeOf(C, proto2);

        f() === "t:p2"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
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
fn super_call_in_static_method_uses_this_binding_as_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A { static f() { return this.x; } }
        class B extends A {
          static g() { return super.f(); }
        }
        B.x = 1;
        B.g() === 1
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

#[test]
fn derived_ctor_super_computed_before_super_does_not_evaluate_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        class B {}
        class D extends B {
          constructor() {
            super[side = 1];
            super();
          }
        }
        try { new D(); "no"; } catch (e) { String(side) + ":" + e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
}

#[test]
fn super_property_with_null_super_base_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class N extends null {
          constructor() {
            // `extends null` cannot call `super()`, but derived constructors may return an object
            // without initializing `this`.
            return Object.create(new.target.prototype);
          }
          m() {
            return super.toString;
          }
        }
        try { new N().m(); "no"; } catch (e) { e.name + ":" + e.message }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError:Cannot convert undefined or null to object");
}
