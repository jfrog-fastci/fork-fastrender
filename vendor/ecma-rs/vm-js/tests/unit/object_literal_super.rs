use crate::property::{PropertyKey, PropertyKind};
use crate::{CompiledScript, GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmOptions};

fn assert_object_literal_methods_and_accessors_have_home_object(
  scope: &mut Scope<'_>,
  obj: GcObject,
) -> Result<(), VmError> {
  scope.push_root(Value::Object(obj))?;

  // Method: `{ m() {} }`.
  let m_key_s = scope.alloc_string("m")?;
  let m_desc = scope
    .heap()
    .get_own_property(obj, PropertyKey::from_string(m_key_s))?
    .expect("missing property `m`");
  let PropertyKind::Data { value, .. } = m_desc.kind else {
    panic!("expected data property for `m`, got: {m_desc:?}");
  };
  let Value::Object(m_func) = value else {
    panic!("expected function value for `m`, got: {value:?}");
  };
  assert_eq!(
    scope.heap().get_function_home_object(m_func)?,
    Some(obj),
    "object literal method function missing [[HomeObject]]"
  );

  // Accessors: `{ get x() {}, set x(v) {} }`.
  let x_key_s = scope.alloc_string("x")?;
  let x_desc = scope
    .heap()
    .get_own_property(obj, PropertyKey::from_string(x_key_s))?
    .expect("missing property `x`");
  let PropertyKind::Accessor { get, set } = x_desc.kind else {
    panic!("expected accessor property for `x`, got: {x_desc:?}");
  };
  let Value::Object(get_func) = get else {
    panic!("expected getter function for `x`, got: {get:?}");
  };
  let Value::Object(set_func) = set else {
    panic!("expected setter function for `x`, got: {set:?}");
  };
  assert_eq!(
    scope.heap().get_function_home_object(get_func)?,
    Some(obj),
    "object literal getter function missing [[HomeObject]]"
  );
  assert_eq!(
    scope.heap().get_function_home_object(set_func)?,
    Some(obj),
    "object literal setter function missing [[HomeObject]]"
  );

  Ok(())
}

fn assert_script_returns_true_in_interpreter_and_compiled(source: &str) -> Result<(), VmError> {
  // AST interpreter.
  {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
    let result = rt.exec_script(source)?;
    assert!(
      matches!(result, Value::Bool(true)),
      "unexpected interpreter result: {result:?}"
    );
  }

  // Compiled HIR executor.
  {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
    let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
    assert!(
      !script.requires_ast_fallback,
      "test script should execute via compiled (HIR) script executor"
    );
    let result = rt.exec_compiled_script(script)?;
    assert!(
      matches!(result, Value::Bool(true)),
      "unexpected compiled result: {result:?}"
    );
  }

  Ok(())
}

#[test]
fn object_literal_method_super_call_uses_home_object_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Mirrors test262: language/expressions/object/method.js
  let result = rt.exec_script(
    r#"
      var proto = {
        method(x) {
          return 'proto' + x;
        }
      };

      var object = {
        method(x) {
          return super.method(x);
        }
      };

      Object.setPrototypeOf(object, proto);

      object.method(42) === 'proto42' &&
        proto.method(42) === 'proto42' &&
        Object.getPrototypeOf(object).method(42) === 'proto42';
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

#[test]
fn object_literal_getter_super_prop_uses_home_object_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Mirrors test262: language/expressions/object/getter-super-prop.js
  let result = rt.exec_script(
    r#"
      var proto = {
        _x: 42,
        get x() {
          return 'proto' + this._x;
        }
      };

      var object = {
        get x() {
          return super.x;
        }
      };

      Object.setPrototypeOf(object, proto);

      object.x === 'proto42' &&
        object._x === 42 &&
        Object.getPrototypeOf(object)._x === 42;
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

#[test]
fn object_literal_setter_super_prop_uses_home_object_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Mirrors test262: language/expressions/object/setter-super-prop.js
  let result = rt.exec_script(
    r#"
      var proto = {
        _x: 0,
        set x(v) {
          return this._x = v;
        }
      };

      var object = {
        set x(v) {
          super.x = v;
        }
      };

      Object.setPrototypeOf(object, proto);

      var v = (object.x = 1);
      v === 1 &&
        object._x === 1 &&
        Object.getPrototypeOf(object)._x === 0;
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

#[test]
fn object_literal_arrow_captures_lexical_super_and_observes_dynamic_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Based on test262: language/expressions/super/prop-dot-obj-val-from-arrow.js
  //
  // The key property here is that `super` in the arrow function is *lexical*, and must observe the
  // home object's current prototype (which we mutate after creating the arrow).
  let result = rt.exec_script(
    r#"
      var proto1 = { x: 'p1' };
      var proto2 = { x: 'p2' };

      var object = {
        method() {
          return () => super.x;
        }
      };

      Object.setPrototypeOf(object, proto1);
      var f = object.method();
      Object.setPrototypeOf(object, proto2);

      f() === 'p2';
    "#,
  )?;
  assert!(matches!(result, Value::Bool(true)), "unexpected result: {result:?}");
  Ok(())
}

#[test]
fn object_literal_methods_and_accessors_set_home_object_ast() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_script(r#"({ m() {}, get x() {}, set x(v) {} })"#)?;
  let Value::Object(obj) = result else {
    panic!("expected object, got: {result:?}");
  };

  let mut scope = rt.heap.scope();
  assert_object_literal_methods_and_accessors_have_home_object(&mut scope, obj)?;
  Ok(())
}

#[test]
fn super_computed_getsuperbase_before_topropertykey_getvalue() -> Result<(), VmError> {
  // Regression test (test262: `prop-expr-getsuperbase-before-topropertykey-*`): computed `super[expr]`
  // observes `GetSuperBase` before `ToPropertyKey`, so prototype mutation during key coercion does
  // **not** affect the resolved super base for the current operation.
  assert_script_returns_true_in_interpreter_and_compiled(
    r#"
      var proto = { p: "ok" };
      var proto2 = { p: "bad" };

      var obj = {
        __proto__: proto,
        m() {
          return super[key];
        }
      };

      var key = {
        toString() {
          Object.setPrototypeOf(obj, proto2);
           return "p";
         }
        };

      obj.m() === "ok" &&
        Object.getPrototypeOf(obj) === proto2;
    "#,
  )
}

#[test]
fn super_computed_getsuperbase_before_topropertykey_putvalue() -> Result<(), VmError> {
  // Regression test (test262: `prop-expr-getsuperbase-before-topropertykey-*`): computed `super[expr]`
  // observes `GetSuperBase` before `ToPropertyKey`, so prototype mutation during key coercion does
  // **not** affect the resolved super base for the current operation.
  assert_script_returns_true_in_interpreter_and_compiled(
    r#"
      var result;

      var proto = {
        set p(v) {
          result = "ok";
        }
      };

      var proto2 = {
        set p(v) {
          result = "bad";
        }
      };

      var obj = {
        __proto__: proto,
        m() {
          super[key] = 10;
        }
      };

      var key = {
        toString() {
          Object.setPrototypeOf(obj, proto2);
          return "p";
         }
       };

      obj.m();
      result === "ok" &&
        Object.getPrototypeOf(obj) === proto2;
    "#,
  )
}

#[test]
fn super_computed_getsuperbase_before_topropertykey_putvalue_compound_assign() -> Result<(), VmError>
{
  // Regression test (test262: `prop-expr-getsuperbase-before-topropertykey-*`): computed `super[expr]`
  // observes `GetSuperBase` before `ToPropertyKey`, so prototype mutation during key coercion does
  // **not** affect the resolved super base for the current operation.
  assert_script_returns_true_in_interpreter_and_compiled(
    r#"
      var proto = { p: 1 };
      var proto2 = { p: -1 };

      var obj = {
        __proto__: proto,
        m() {
          return super[key] += 1;
        }
      };

      var key = {
        toString() {
          Object.setPrototypeOf(obj, proto2);
           return "p";
          }
        };

      obj.m() === 2 &&
        obj.p === 2 &&
        Object.getPrototypeOf(obj) === proto2;
    "#,
  )
}

#[test]
fn super_computed_getsuperbase_before_topropertykey_putvalue_increment() -> Result<(), VmError> {
  // Regression test (test262: `prop-expr-getsuperbase-before-topropertykey-*`): computed `super[expr]`
  // observes `GetSuperBase` before `ToPropertyKey`, so prototype mutation during key coercion does
  // **not** affect the resolved super base for the current operation.
  assert_script_returns_true_in_interpreter_and_compiled(
    r#"
      var proto = { p: 1 };
      var proto2 = { p: -1 };

      var obj = {
        __proto__: proto,
        m() {
          return ++super[key];
        }
      };

      var key = {
        toString() {
          Object.setPrototypeOf(obj, proto2);
           return "p";
          }
        };

      obj.m() === 2 &&
        obj.p === 2 &&
        Object.getPrototypeOf(obj) === proto2;
    "#,
  )
}

#[test]
fn compiled_hir_object_literal_methods_and_accessors_set_home_object() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"({ m() {}, get x() {}, set x(v) {} })"#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(obj) = result else {
    panic!("expected object, got: {result:?}");
  };

  let mut scope = rt.heap.scope();
  assert_object_literal_methods_and_accessors_have_home_object(&mut scope, obj)
}

#[test]
fn compiled_hir_arrow_functions_capture_home_object_from_method() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // The compiled path does not yet implement `super`, but we can still validate that arrow
  // functions created inside methods copy `[[HomeObject]]` from the current execution context.
  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"({ m() { return () => 1; } })"#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  let result = rt.exec_compiled_script(script)?;
  let Value::Object(obj) = result else {
    panic!("expected object, got: {result:?}");
  };

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(obj))?;

  let m_key_s = scope.alloc_string("m")?;
  let m_desc = scope
    .heap()
    .get_own_property(obj, PropertyKey::from_string(m_key_s))?
    .expect("missing property `m`");
  let PropertyKind::Data { value, .. } = m_desc.kind else {
    panic!("expected data property for `m`, got: {m_desc:?}");
  };
  let Value::Object(m_func) = value else {
    panic!("expected function value for `m`, got: {value:?}");
  };

  let mut host = ();
  let arrow_val = rt
    .vm
    .call(&mut host, &mut scope, Value::Object(m_func), Value::Object(obj), &[])?;
  let Value::Object(arrow_func) = arrow_val else {
    panic!("expected arrow function object, got: {arrow_val:?}");
  };
  assert_eq!(
    scope.heap().get_function_home_object(arrow_func)?,
    Some(obj),
    "arrow function missing captured [[HomeObject]] from enclosing method"
  );
  Ok(())
}
