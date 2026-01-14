use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn var_decl_name_inference_respects_anonymous_function_definition_syntax() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var xCover = (0, function() {});
        var cover = (function() {});
        var desc = Object.getOwnPropertyDescriptor(cover, 'name');
        var desc2 = Object.getOwnPropertyDescriptor(xCover, 'name');
        xCover.name !== 'xCover'
          && desc.value === 'cover'
          && desc.writable === false
          && desc.enumerable === false
          && desc.configurable === true
          && desc2.value === ''
          && desc2.writable === false
          && desc2.enumerable === false
          && desc2.configurable === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn var_decl_does_not_overwrite_class_static_name_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var cls = class {};
        var xCls = class X {};
        var xCls2 = class { static name() {} };
        cls.name === 'cls'
          && xCls.name === 'X'
          && typeof xCls2.name === 'function'
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_default_initializer_infers_name_like_spec() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var { xCover = (0, function() {}) } = {};
        var { cover = (function() {}) } = {};
        xCover.name !== 'xCover' && cover.name === 'cover'
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn parameter_default_initializer_infers_function_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function f(x = function() {}) {
          return x.name;
        }
        f() === 'x'
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn assignment_does_not_infer_name_for_parenthesized_identifier_lhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var fn;
        (fn) = function() {};
        var desc = Object.getOwnPropertyDescriptor(fn, 'name');
        desc.value === '' && desc.writable === false && desc.enumerable === false && desc.configurable === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn assignment_infers_name_for_member_expression_lhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o = {};
        o.attr = function() {};
        var desc = Object.getOwnPropertyDescriptor(o.attr, 'name');
        desc.value === 'attr' && desc.writable === false && desc.enumerable === false && desc.configurable === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_does_not_infer_name_for_parenthesized_identifier_target() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var fn;
        ({ x: (fn) = function() {} } = {});
        var desc = Object.getOwnPropertyDescriptor(fn, 'name');
        desc.value === '' && desc.writable === false && desc.enumerable === false && desc.configurable === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_infers_name_for_member_expression_target() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o = {};
        ({ x: o.attr = function() {} } = {});
        var desc = Object.getOwnPropertyDescriptor(o.attr, 'name');
        desc.value === 'attr' && desc.writable === false && desc.enumerable === false && desc.configurable === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_default_initializer_infers_after_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var fn;
          ({ [yield 'k']: fn = function() {} } = {});
          return fn.name;
        }
        var it = g();
        var first = it.next();
        var second = it.next('x');
        first.value === 'k' && second.value === 'fn'
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
