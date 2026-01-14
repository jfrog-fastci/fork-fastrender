use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn direct_eval_super_in_field_initializer_allows_super_property_but_not_super_call() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { constructor() { this.y = 2; } }
        class C extends B {
          x = eval("false && super.y, 1");
        }

        class D extends B {
          x = eval("super()");
        }

        var ok_prop = (new C().x === 1);
        var ok_call;
        try { new D(); ok_call = false; } catch (e) { ok_call = (e.name === "SyntaxError"); }

        ok_prop && ok_call
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_super_in_method_allows_super_property() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {}
        class C extends B {
          m() { return eval('false && super.toString, 1'); }
        }
        new C().m() === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_super_call_in_derived_constructor_is_allowed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {}
        class C extends B {
          constructor() {
            super();
            this.v = eval('false && super(), 1');
          }
        }
        new C().v === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_new_target_is_context_aware() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function C() { return eval('new.target'); }
        var ok_ctor = (new C() === C);

        var ok_script;
        try { eval('new.target'); ok_script = false; } catch(e) { ok_script = (e.name === 'SyntaxError'); }

        var ok_arrow;
        try { (() => eval('new.target'))(); ok_arrow = false; } catch(e) { ok_arrow = (e.name === 'SyntaxError'); }

        ok_ctor && ok_script && ok_arrow
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_new_target_in_arrow_nested_in_function_is_allowed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var newTarget = null;
        function C() {
          newTarget = (() => eval('new.target'))();
        }
        C();
        var ok_plain = (newTarget === undefined);

        new C();
        var ok_ctor = (newTarget === C);

        ok_plain && ok_ctor
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_new_target_in_arrow_nested_in_function_is_allowed_compiled() {
  let mut rt = new_runtime();
  let source = r#"
    var newTarget = null;
    function C() {
      newTarget = (() => eval('new.target'))();
    }
    C();
    var ok_plain = (newTarget === undefined);

    new C();
    var ok_ctor = (newTarget === C);

    ok_plain && ok_ctor
  "#;
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source).unwrap();
  let value = rt.exec_compiled_script(script).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_super_property_in_arrow_nested_in_class_method_is_allowed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Foo {
          method() { return () => eval('super.toString'); }
        }
        var f = new Foo().method();
        f() === Object.prototype.toString
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_super_property_in_arrow_nested_in_object_method_is_allowed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var proto = { x: 262 };
        var o = {
          method() { return () => eval('super.x'); }
        };
        Object.setPrototypeOf(o, proto);
        var f = o.method();
        f() === 262
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_super_property_in_arrow_nested_in_object_method_is_allowed_compiled() {
  let mut rt = new_runtime();
  let source = r#"
    var proto = { x: 262 };
    var o = {
      method() { return () => eval('super.x'); }
    };
    Object.setPrototypeOf(o, proto);
    var f = o.method();
    f() === 262
  "#;
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source).unwrap();
  let value = rt.exec_compiled_script(script).unwrap();
  assert_eq!(value, Value::Bool(true));
}
