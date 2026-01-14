use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_instance_field_get_set() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #x = 1;
        getX() { return this.#x; }
        setX(v) { this.#x = v; }
      }
      const c = new C();
      c.getX() === 1 && (c.setX(2), c.getX() === 2)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_instance_method_is_shared_and_named() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r##"
      class C {
        #m() { return 42; }
        getRef() { return this.#m; }
      }
      const a = new C();
      const b = new C();
      a.getRef() === b.getRef() && a.getRef().name === "#m" && a.getRef()() === 42
    "##,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_brand_check_operator_basic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #x;
        static has(o) { return #x in o; }
      }
      C.has({}) === false && C.has(new C()) === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_brand_check_rhs_non_object_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let caught = null;
      class C {
        #x;
        static test() {
          try { #x in 1; } catch (e) { caught = e; }
        }
      }
      C.test();
      caught instanceof TypeError
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_brand_check_cross_class_isolation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function classfactory() {
        return class {
          #x;
          static has(o) { return #x in o; }
        };
      }
      const C1 = classfactory();
      const C2 = classfactory();
      C1.has(new C1()) === true && C1.has(new C2()) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn compiled_script_with_private_names_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      class C {
        #x = 1;
        getX() { return this.#x; }
      }
      (new C()).getX();
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn direct_eval_can_access_private_instance_field() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A {
        #x = 14;
        g() {
          return eval("this.#x");
        }
      }
      (new A()).g();
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(14.0));
}

#[test]
fn nested_class_can_reference_outer_private_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A {
        #x = 5;
        makeB() {
          class B {
            #y = 1;
            getX(o) { return o.#x; }
          }
          return new B();
        }
      }
      const a = new A();
      const b = a.makeB();
      b.getX(a) === 5;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn nested_class_private_access_throws_type_error_on_wrong_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        class A {
          #x = 10;
          f() {
            class B {
              #y = 1;
              g() {
                // `#x` resolves to A's private name, but `this` is a B instance,
                // so the brand check fails and should throw a TypeError.
                return this.#x;
              }
            }
            this.y = new B();
          }
          constructor() { this.f(); }
          g() { return this.y.g(); }
        }
        const a = new A();
        try { a.g(); return false; } catch (e) { return e instanceof TypeError; }
      })();
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
