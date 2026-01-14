use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_class_expr_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        return class extends (yield 1) {};
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_class_decl_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        class D extends (yield 1) {}
        return D;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var D = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      D.name === "D" &&
      Object.getPrototypeOf(D) === Base &&
      Object.getPrototypeOf(D.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_class_computed_method_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        class C { [yield 1]() { return 2; } }
        return new C().m();
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("m");
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_class_eval_is_strict_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        return class extends (yield 1) { [(x = 1, "m")]() { return 2; } };
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var ok = false;
      try {
        it.next(Base);
      } catch (e) {
        ok = e && e.name === "ReferenceError" && typeof x === "undefined";
      }
      r1.value === 1 && r1.done === false && ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_anonymous_class_inferred_name_across_yield_in_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        return ({ a: class extends (yield 1) { static { globalThis.saw = this.name; } } }).a;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_anonymous_class_inferred_name_across_yield_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        var obj = { a: class { static { globalThis.saw = this.name; } [yield 1]() { return 2; } } };
        return obj.a;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("m");
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      new C().m() === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_anonymous_class_inferred_name_across_yield_in_static_block() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        globalThis.v = null;
        var obj = {
          a: class {
            static {
              globalThis.saw = this.name;
              globalThis.v = yield 1;
            }
          }
        };
        return obj.a;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(7);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      globalThis.v === 7 &&
      C.name === "a"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_var_decl_anonymous_class_inferred_name_across_yield_in_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        var C = class extends (yield 1) { static { globalThis.saw = this.name; } };
        return C;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "C" &&
      C.name === "C" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_anonymous_class_inferred_name_across_yield_in_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        var C;
        C = class extends (yield 1) { static { globalThis.saw = this.name; } };
        return C;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "C" &&
      C.name === "C" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_property_assignment_anonymous_class_inferred_name_across_yield_in_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        var obj = {};
        obj.a = class extends (yield 1) { static { globalThis.saw = this.name; } };
        return obj.a;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_default_anonymous_class_inferred_name_across_yield_in_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        let { a = class extends (yield 1) { static { globalThis.saw = this.name; } } } = {};
        return a;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_default_anonymous_class_inferred_name_across_yield_in_extends(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        let a;
        ({ a = class extends (yield 1) { static { globalThis.saw = this.name; } } } = {});
        return a;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_default_anonymous_class_inferred_name_across_yield_in_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        let [a = class extends (yield 1) { static { globalThis.saw = this.name; } }] = [];
        return a;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_default_anonymous_class_inferred_name_across_yield_in_extends_after_computed_key_yield(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        globalThis.saw = null;
        let { [yield 1]: a = class extends (yield 2) { static { globalThis.saw = this.name; } } } = {};
        return a;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next("a");
      var r3 = it.next(Base);
      var C = r3.value;
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true &&
      globalThis.saw === "a" &&
      C.name === "a" &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_class_yield_in_static_block() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          class C {
            static {
              this.x = yield 1;
            }
          }
          return C.x;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(7);
        r1.value === 1 && r1.done === false &&
        r2.value === 7 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn yield_in_class_field_initializer_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      function* g() {
        class C { x = yield 0; }
      }
    "#,
    )
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn yield_in_class_static_field_initializer_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      function* g() {
        class C { static x = yield 0; }
      }
    "#,
    )
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn generator_yield_in_class_computed_field_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        class C { [yield 1] = 2; }
        return new C();
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("x");
      var o = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      o.x === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
