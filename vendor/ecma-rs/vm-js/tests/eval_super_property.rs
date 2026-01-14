use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn direct_eval_allows_super_property_in_object_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var superProp = null;
      var o = {
        test262: null,
        method() {
          superProp = eval('super.test262;');
        }
      };

      o.method();
      var ok1 = superProp === undefined;

      Object.setPrototypeOf(o, { test262: 262 });
      o.method();
      var ok2 = superProp === 262;

      ok1 && ok2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_allows_super_property_in_class_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A {
        get x() { return this.y; }
      }
      class B extends A {
        constructor() { super(); this.y = 42; }
        method() { return eval('super.x'); }
      }

      (new B()).method() === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_rejects_super_property_outside_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var caught;
      function f() {
        try {
          eval('super.x;');
        } catch (err) {
          caught = err;
        }
      }
      f();

      caught && caught.name === 'SyntaxError'
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_rejects_super_computed_property_outside_method_without_evaluating_expression() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var evaluated = false;
      function f() {
        try {
          eval('super[evaluated = true];');
        } catch (_) {}
      }
      f();

      evaluated
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn indirect_eval_rejects_super_property_even_inside_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var caught;
      try {
        (0,eval)('super.property;');
      } catch (err) {
        caught = err;
      }
      var ok1 = caught && caught.name === 'SyntaxError';

      caught = null;
      try {
        ({
          m() { (0,eval)('super.property;'); }
        }).m();
      } catch (err) {
        caught = err;
      }
      var ok2 = caught && caught.name === 'SyntaxError';

      ok1 && ok2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_allows_super_method_call_in_class_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A { m() { return 1; } }
      class B extends A {
        m() { return eval('super.m()'); }
      }
      (new B()).m() === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn direct_eval_rejects_super_property_at_script_top_level() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var caught;
      try {
        eval('super.x');
      } catch (err) {
        caught = err;
      }
      caught && caught.name === 'SyntaxError'
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn indirect_eval_rejects_super_property_when_called_via_alias() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A { m() { return 1; } }
      class B extends A {
        m() {
          let e = eval;
          try { e('super.m()'); } catch (err) { return err.name; }
        }
      }
      (new B()).m() === 'SyntaxError'
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn indirect_eval_rejects_super_computed_property_without_evaluating_expression() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var evaluated = false;
      class A { m() { return 1; } }
      class B extends A {
        m() {
          let e = eval;
          try { e('super[evaluated = true];'); } catch (err) { return err.name; }
        }
      }
      var name = (new B()).m();
      name === 'SyntaxError' && evaluated === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
