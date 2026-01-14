use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Value {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<eval_super_in_class_fields>", source)
    .unwrap();
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script).unwrap()
}

#[test]
fn direct_eval_allows_super_in_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
       class B { get x() { return this.marker; } }
       class A extends B {
         get x() { return 0; }
         marker = 123;
         y = eval("super.x");
       }
       (new A()).y
     "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(123.0));
}

#[test]
fn direct_eval_allows_super_in_instance_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
       class B { get x() { return this.marker; } }
       class A extends B {
         get x() { return 0; }
         marker = 123;
         y = eval("super.x");
       }
       (new A()).y
     "#,
  );
  assert_eq!(value, Value::Number(123.0));
}

#[test]
fn direct_eval_allows_super_in_private_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class B { get x() { return this.marker; } }
      class A extends B {
        get x() { return 0; }
        marker = 456;
        #y = eval("super.x");
        get y() { return this.#y; }
      }
      (new A()).y
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(456.0));
}

#[test]
fn direct_eval_allows_super_in_static_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class B { static get x() { return this.marker; } }
      class A extends B {
        static get x() { return 0; }
        static marker = 789;
        static y = eval("super.x");
      }
      A.y
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(789.0));
}

#[test]
fn direct_eval_allows_super_in_static_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { static get x() { return this.marker; } }
      class A extends B {
        static get x() { return 0; }
        static marker = 789;
        static y = eval("super.x");
      }
      A.y
    "#,
  );
  assert_eq!(value, Value::Number(789.0));
}

#[test]
fn direct_eval_allows_super_in_private_static_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class B { static get x() { return this.marker; } }
      class A extends B {
        static get x() { return 0; }
        static marker = 999;
        static #y = eval("super.x");
        static get y() { return this.#y; }
      }
      A.y
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(999.0));
}

#[test]
fn indirect_eval_rejects_super_in_field_initializer_without_side_effects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var side = 0;
      function sideEffect() { side++; return "k"; }
      var e = eval;
      class B { get x() { return 1; } }
      class A extends B {
        y = e("({ [sideEffect()]: super.x })");
      }
      try {
        new A();
        false
      } catch (err) {
        err.name === "SyntaxError" && side === 0
      }
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn indirect_eval_rejects_super_in_field_initializer_without_side_effects_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
       var side = 0;
       function sideEffect() { side++; return "k"; }
       var e = eval;
       class B { get x() { return this.marker; } }
       class A extends B {
         marker = 1;
         y = e("({ [sideEffect()]: super.x })");
       }
       try {
         new A();
         false
       } catch (err) {
         err.name === "SyntaxError" && side === 0
       }
     "#,
  );
  assert_eq!(value, Value::Bool(true));
}
