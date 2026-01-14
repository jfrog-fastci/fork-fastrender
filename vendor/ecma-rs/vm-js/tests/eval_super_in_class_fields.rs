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
fn direct_eval_allows_super_in_arrow_within_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { get x() { return this.marker; } }
        class A extends B {
          marker = 777;
          y = (() => eval("super.x"))();
        }
        (new A()).y
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(777.0));
}

#[test]
fn direct_eval_allows_super_in_arrow_within_instance_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { get x() { return this.marker; } }
      class A extends B {
        marker = 777;
        y = (() => eval("super.x"))();
      }
      (new A()).y
    "#,
  );
  assert_eq!(value, Value::Number(777.0));
}

#[test]
fn direct_eval_rejects_super_in_nested_plain_function_within_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { get x() { return 1; } }
        class A extends B {
          y = (() => {
            function f() { return eval("super.x"); }
            try { f(); return "no"; } catch (e) { return e.name; }
          })();
        }
        (new A()).y
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
}

#[test]
fn direct_eval_rejects_super_in_nested_plain_function_within_instance_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { get x() { return 1; } }
      class A extends B {
        y = (() => {
          function f() { return eval("super.x"); }
          try { f(); return "no"; } catch (e) { return e.name; }
        })();
      }
      (new A()).y
    "#,
  );
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
}

#[test]
fn direct_eval_allows_super_set_in_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v) { this.marker = v; } }
        class A extends B {
          marker = 0;
          y = eval("super.x = 321");
        }
        (new A()).marker
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(321.0));
}

#[test]
fn direct_eval_allows_super_set_in_instance_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { set x(v) { this.marker = v; } }
      class A extends B {
        marker = 0;
        y = eval("super.x = 321");
      }
      (new A()).marker
    "#,
  );
  assert_eq!(value, Value::Number(321.0));
}

#[test]
fn direct_eval_allows_computed_super_in_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { get x() { return this.marker; } }
        class A extends B {
          marker = 111;
          y = eval("super['x']");
        }
        (new A()).y
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(111.0));
}

#[test]
fn direct_eval_allows_computed_super_in_instance_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { get x() { return this.marker; } }
      class A extends B {
        marker = 111;
        y = eval("super['x']");
      }
      (new A()).y
    "#,
  );
  assert_eq!(value, Value::Number(111.0));
}

#[test]
fn direct_eval_allows_computed_super_set_in_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v) { this.marker = v; } }
        class A extends B {
          marker = 0;
          y = eval("super['x'] = 222");
        }
        (new A()).marker
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(222.0));
}

#[test]
fn direct_eval_allows_computed_super_set_in_instance_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { set x(v) { this.marker = v; } }
      class A extends B {
        marker = 0;
        y = eval("super['x'] = 222");
      }
      (new A()).marker
    "#,
  );
  assert_eq!(value, Value::Number(222.0));
}

#[test]
fn direct_eval_allows_super_computed_member_key_side_effects_in_instance_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function key() { side++; return "x"; }
        class B { get x() { return this.marker; } }
        class A extends B {
          marker = 333;
          y = eval("super[key()]");
        }
        var a = new A();
        a.y === 333 && side === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn indirect_eval_rejects_super_computed_member_without_running_key_side_effects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function key() { side++; return "x"; }
        var e = eval;
        class B { get x() { return this.marker; } }
        class A extends B {
          marker = 1;
          y = e("super[key()]");
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
fn indirect_eval_rejects_super_computed_member_without_running_key_side_effects_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      function key() { side++; return "x"; }
      var e = eval;
      class B { get x() { return this.marker; } }
      class A extends B {
        marker = 1;
        y = e("super[key()]");
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

#[test]
fn indirect_eval_rejects_super_computed_member_via_parenthesized_eval_without_running_key_side_effects(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function key() { side++; return "x"; }
        class B { get x() { return this.marker; } }
        class A extends B {
          marker = 1;
          y = (eval)("super[key()]");
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
fn indirect_eval_rejects_super_computed_member_via_parenthesized_eval_without_running_key_side_effects_compiled(
) {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      function key() { side++; return "x"; }
      class B { get x() { return this.marker; } }
      class A extends B {
        marker = 1;
        y = (eval)("super[key()]");
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

#[test]
fn indirect_eval_rejects_super_computed_member_via_optional_eval_without_running_key_side_effects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function key() { side++; return "x"; }
        class B { get x() { return this.marker; } }
        class A extends B {
          marker = 1;
          y = eval?.("super[key()]");
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
fn indirect_eval_rejects_super_computed_member_via_optional_eval_without_running_key_side_effects_compiled(
) {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      function key() { side++; return "x"; }
      class B { get x() { return this.marker; } }
      class A extends B {
        marker = 1;
        y = eval?.("super[key()]");
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
fn direct_eval_allows_super_computed_member_key_side_effects_in_static_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function key() { side++; return "x"; }
        class B { static get x() { return this.marker; } }
        class A extends B {
          static marker = 444;
          static y = eval("super[key()]");
        }
        A.y === 444 && side === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_allows_super_computed_member_key_side_effects_in_static_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      function key() { side++; return "x"; }
      class B { static get x() { return this.marker; } }
      class A extends B {
        static marker = 444;
        static y = eval("super[key()]");
      }
      A.y === 444 && side === 1
    "#,
  );
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_allows_super_in_arrow_within_static_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { static get x() { return this.marker; } }
        class A extends B {
          static marker = 888;
          static y = (() => eval("super.x"))();
        }
        A.y
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(888.0));
}

#[test]
fn direct_eval_allows_super_in_arrow_within_static_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { static get x() { return this.marker; } }
      class A extends B {
        static marker = 888;
        static y = (() => eval("super.x"))();
      }
      A.y
    "#,
  );
  assert_eq!(value, Value::Number(888.0));
}

#[test]
fn direct_eval_rejects_super_in_nested_plain_function_within_static_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { static get x() { return 1; } }
        class A extends B {
          static y = (() => {
            function f() { return eval("super.x"); }
            try { f(); return "no"; } catch (e) { return e.name; }
          })();
        }
        A.y
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
}

#[test]
fn direct_eval_rejects_super_in_nested_plain_function_within_static_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { static get x() { return 1; } }
      class A extends B {
        static y = (() => {
          function f() { return eval("super.x"); }
          try { f(); return "no"; } catch (e) { return e.name; }
        })();
      }
      A.y
    "#,
  );
  let Value::String(s) = value else {
    panic!("expected string from caught error name");
  };
  assert_eq!(rt.heap().get_string(s).unwrap().to_utf8_lossy(), "SyntaxError");
}

#[test]
fn direct_eval_allows_super_set_in_static_field_initializer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { static set x(v) { this.marker = v; } }
        class A extends B {
          static marker = 0;
          static y = eval("super.x = 654");
        }
        A.marker
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(654.0));
}

#[test]
fn direct_eval_allows_super_set_in_static_field_initializer_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      class B { static set x(v) { this.marker = v; } }
      class A extends B {
        static marker = 0;
        static y = eval("super.x = 654");
      }
      A.marker
    "#,
  );
  assert_eq!(value, Value::Number(654.0));
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
fn indirect_eval_rejects_super_in_static_field_initializer_without_side_effects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function sideEffect() { side++; return "k"; }
        var e = eval;
        class B { static get x() { return 1; } }
        try {
          class A extends B {
            static y = e("({ [sideEffect()]: super.x })");
          }
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
fn indirect_eval_rejects_super_in_static_field_initializer_without_side_effects_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      function sideEffect() { side++; return "k"; }
      var e = eval;
      class B { static get x() { return 1; } }
      try {
        class A extends B {
          static y = e("({ [sideEffect()]: super.x })");
        }
        false
      } catch (err) {
        err.name === "SyntaxError" && side === 0
      }
    "#,
  );
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn indirect_eval_rejects_super_computed_member_in_static_field_initializer_without_running_key_side_effects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var side = 0;
        function key() { side++; return "x"; }
        var e = eval;
        class B { static get x() { return 1; } }
        try {
          class A extends B {
            static y = e("super[key()]");
          }
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
fn indirect_eval_rejects_super_computed_member_in_static_field_initializer_without_running_key_side_effects_compiled() {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var side = 0;
      function key() { side++; return "x"; }
      var e = eval;
      class B { static get x() { return 1; } }
      try {
        class A extends B {
          static y = e("super[key()]");
        }
        false
      } catch (err) {
        err.name === "SyntaxError" && side === 0
      }
    "#,
  );
  assert_eq!(value, Value::Bool(true));
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
