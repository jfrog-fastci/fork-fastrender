use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<super_property>", source)?;
  rt.exec_compiled_script(script)
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
fn super_property_in_base_class_method_reads_from_object_prototype_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {
        m() { return super.toString === Object.prototype.toString; }
      }
      new A().m()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
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
fn super_property_in_base_class_static_method_reads_from_function_prototype_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {
        static m() { return super.toString === Function.prototype.toString; }
      }
      A.m()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
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
fn super_in_arrow_closure_observes_dynamic_home_object_prototype_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
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
fn super_in_static_arrow_closure_observes_dynamic_home_object_prototype_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_method_call_uses_primitive_this_binding_as_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m() { return typeof this; } }
        class D extends B { m() { return super.m(); } }
        D.prototype.m.call(1) === "number"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_method_call_uses_primitive_this_binding_as_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { m() { return typeof this; } }
      class D extends B { m() { return super.m(); } }
      D.prototype.m.call(1) === "number"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_computed_member_call_supports_symbol_property_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var sym = Symbol("m");
        Object.prototype[sym] = function () { return this.x; };
        class C {
          constructor() { this.x = 1; }
          m() { return super[sym](); }
        }
        new C().m() === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_computed_member_call_supports_symbol_property_keys_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var sym = Symbol("m");
      Object.prototype[sym] = function () { return this.x; };
      class C {
        constructor() { this.x = 1; }
        m() { return super[sym](); }
      }
      new C().m() === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
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
fn super_method_call_uses_this_binding_as_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { m(){ return this.v; } }
      class D extends B {
        constructor(){ super(); this.v = 1; }
        m(){ return super.m(); }
      }
      new D().m() === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_method_call_uses_call_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m(){ return this.v; } }
        class D extends B {
          g(){ return super.m(); }
        }
        D.prototype.g.call({ v: 3 }) === 3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_method_call_uses_call_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { m(){ return this.v; } }
      class D extends B {
        g(){ return super.m(); }
      }
      D.prototype.g.call({ v: 3 }) === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_method_call_in_derived_constructor_after_super_uses_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m(){ return this.v; } }
        class D extends B {
          constructor(){
            super();
            this.v = 1;
            this.out = super.m();
          }
        }
        new D().out === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_method_call_in_derived_constructor_after_super_uses_this_binding_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { m(){ return this.v; } }
      class D extends B {
        constructor(){
          super();
          this.v = 1;
          this.out = super.m();
        }
      }
      new D().out === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
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
fn super_property_getter_setter_use_this_binding_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_getter_setter_uses_call_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          setX(v){ super.x = v; }
          getX(){ return super.x; }
        }
        const o = { _x: 0 };
        D.prototype.setX.call(o, 10);
        D.prototype.getX.call(o) === 10 && o._x === 10
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_property_getter_setter_uses_call_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {
        get x(){ return this._x; }
        set x(v){ this._x = v; }
      }
      class D extends B {
        setX(v){ super.x = v; }
        getX(){ return super.x; }
      }
      const o = { _x: 0 };
      D.prototype.setX.call(o, 10);
      D.prototype.getX.call(o) === 10 && o._x === 10
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_static_getter_setter_use_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {
          static get x(){ return this._x; }
          static set x(v){ this._x = v; }
        }
        class B extends A {
          static setX(v){ super.x = v; }
          static getX(){ return super.x; }
        }
        B.setX(42);
        B.getX() === 42 && B._x === 42 && A._x === undefined
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_static_getter_setter_use_this_binding_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {
        static get x(){ return this._x; }
        static set x(v){ this._x = v; }
      }
      class B extends A {
        static setX(v){ super.x = v; }
        static getX(){ return super.x; }
      }
      B.setX(42);
      B.getX() === 42 && B._x === 42 && A._x === undefined
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_static_getter_setter_uses_call_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A {
          static get x(){ return this._x; }
          static set x(v){ this._x = v; }
        }
        class B extends A {
          static setX(v){ super.x = v; }
          static getX(){ return super.x; }
        }
        function C() {}
        B.setX(42);
        B.setX.call(C, 7);
        B.getX() === 42 &&
          B._x === 42 &&
          C._x === 7 &&
          B.getX.call(C) === 7 &&
          A._x === undefined
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_static_getter_setter_uses_call_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A {
        static get x(){ return this._x; }
        static set x(v){ this._x = v; }
      }
      class B extends A {
        static setX(v){ super.x = v; }
        static getX(){ return super.x; }
      }
      function C() {}
      B.setX(42);
      B.setX.call(C, 7);
      B.getX() === 42 &&
        B._x === 42 &&
        C._x === 7 &&
        B.getX.call(C) === 7 &&
        A._x === undefined
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_assignment_to_non_writable_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {}
        Object.defineProperty(B.prototype, "x", { value: 0, writable: false, configurable: true });
        class D extends B {
          constructor() { super(); }
          setX() {
            try { super.x = 1; return "no"; }
            catch (e) { return e.name + ":" + e.message; }
          }
        }
        new D().setX()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError:Cannot assign to read-only property");
}

#[test]
fn super_property_assignment_to_non_writable_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      Object.defineProperty(B.prototype, "x", { value: 0, writable: false, configurable: true });
      class D extends B {
        constructor() { super(); }
        setX() {
          try { super.x = 1; return "no"; }
          catch (e) { return e.name + ":" + e.message; }
        }
      }
      new D().setX()
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError:Cannot assign to read-only property");
  Ok(())
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
fn super_call_in_static_method_uses_this_binding_as_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A { static f() { return this.x; } }
      class B extends A {
        static g() { return super.f(); }
      }
      B.x = 1;
      B.g() === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_call_in_static_method_uses_call_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class A { static f() { return this.tag; } }
        class B extends A { static g() { return super.f(); } }
        function C() {}
        C.tag = 7;
        B.g.call(C) === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_call_in_static_method_uses_call_receiver_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class A { static f() { return this.tag; } }
      class B extends A { static g() { return super.f(); } }
      function C() {}
      C.tag = 7;
      B.g.call(C) === 7
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
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
fn derived_ctor_super_prop_before_super_throws_reference_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
  )?;
  assert_value_is_utf8(
    &rt,
    value,
    "ReferenceError:Must call super constructor in derived class before accessing 'this'",
  );
  Ok(())
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
fn derived_ctor_super_computed_before_super_does_not_evaluate_key_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
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
  )?;
  assert_value_is_utf8(&rt, value, "0:ReferenceError");
  Ok(())
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

#[test]
fn super_property_with_null_super_base_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class N extends null {
        constructor() {
          return Object.create(new.target.prototype);
        }
        m() {
          return super.toString;
        }
      }
      try { new N().m(); "no"; } catch (e) { e.name }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}

#[test]
fn super_is_lexical_in_arrow_functions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m(){ return this.v; } }
        class D extends B {
          constructor(){ super(); this.v = 5; }
          make(){ return () => super.m(); }
        }
        const o = new D();
        const f = o.make();
        f.call({ v: 100 }) === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn super_is_lexical_in_arrow_functions_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { m(){ return this.v; } }
      class D extends B {
        constructor(){ super(); this.v = 5; }
        make(){ return () => super.m(); }
      }
      const o = new D();
      const f = o.make();
      f.call({ v: 100 }) === 5
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_assignment_with_null_super_base_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class N extends null {
          constructor() {
            return Object.create(new.target.prototype);
          }
          m() {
            try { super.x = 1; return "no"; }
            catch (e) { return e.name + ":" + e.message; }
          }
        }
        new N().m()
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError:Cannot convert undefined or null to object");
}

#[test]
fn super_property_assignment_with_null_super_base_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class N extends null {
        constructor() {
          return Object.create(new.target.prototype);
        }
        m() {
          try { super.x = 1; return "no"; }
          catch (e) { return e.name; }
        }
      }
      new N().m()
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "TypeError");
  Ok(())
}
