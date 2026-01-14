use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmOptions, MAX_PROTOTYPE_CHAIN,
};

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
fn try_catch_binds_param_and_returns_value() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"try { throw "x"; } catch(e){ e }"#).unwrap();
  assert_value_is_utf8(&rt, value, "x");
}

#[test]
fn try_finally_preserves_throw_if_finally_is_normal() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"try { throw "x"; } finally { }"#)
    .unwrap_err();
  let value = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, value, "x");
}

#[test]
fn try_catch_throw_overrides_prior_throw() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"try { throw "x"; } catch(e){ throw "y"; }"#)
    .unwrap_err();
  let value = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));
  assert_value_is_utf8(&rt, value, "y");
}

#[test]
fn var_decl_and_if_statement_execute() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var x = 1; if (x === 1) { x = 2; } x"#)
    .unwrap();
  assert_eq!(value, Value::Number(2.0));
}

#[test]
fn with_statement_reads_object_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var x = 10; var o = { x: 1 }; with (o) { x }"#)
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn with_statement_writes_object_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = { x: 1 }; with (o) { x = 2; } o.x"#)
    .unwrap();
  assert_eq!(value, Value::Number(2.0));
}

#[test]
fn with_statement_missing_identifier_falls_back_to_global_in_sloppy_mode() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"with ({}) { __with_test_global__ = 3; } __with_test_global__"#)
    .unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn with_statement_respects_symbol_unscopables() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var x = "outer"; var o = { x: "inner" }; o[Symbol.unscopables] = { x: true }; with (o) { x }"#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "outer");
}

#[test]
fn with_statement_unscopables_blocks_identifier_for_typeof() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = { x: 1 }; o[Symbol.unscopables] = { x: true }; with (o) { typeof x }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "undefined");
}

#[test]
fn delete_identifier_deletes_with_object_property() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = { x: 1 }; with (o) { delete x; } o.x"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn debugger_statement_is_noop() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1; debugger;"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn try_statement_update_empty_to_undefined_finally_only() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1; try { } finally { }"#).unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn try_statement_update_empty_to_undefined_catch_only() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1; try { } catch { }"#).unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn try_statement_update_empty_to_undefined_catch_and_finally() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1; try { } catch { } finally { }"#).unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn try_finally_preserves_non_empty_value() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"try { 1 } finally { }"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn while_try_break_finally_returns_undefined() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"while(true){ 1; try{ break; } finally {} }"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn var_initializer_assigns_to_var_env_even_when_catch_param_shadows() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var e = 1; try { throw 2; } catch(e){ var e = 3; } e"#)
    .unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn labelled_block_break_consumes_break() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"a: { 1; break a; }"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn nested_labels_break_outer() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"a: b: { 1; break a; 2; }"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn labelled_break_with_empty_value_does_not_clobber_prior_statement_list_value() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1; a: { break a; }"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn while_not_entered_returns_undefined() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"1; while(false) {}"#).unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn while_empty_statement_does_not_clobber_later_value() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"while(false) {} 1"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn while_break_propagates_value() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"while(true) { 1; break; }"#).unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn labelled_continue_targets_outer_loop() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 0;
        outer: while (x === 0) {
          while (true) {
            x = 1;
            continue outer;
          }
        }
        x
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn call_expression_invokes_user_function() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"function f(a){ return a; } f(5)"#).unwrap();
  assert_eq!(value, Value::Number(5.0));
}

#[test]
fn new_target_is_undefined_for_plain_call() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function C(){ return new.target; } C()"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn new_target_is_constructor_for_new_expression() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function C(){ return new.target; } var x = new C(); x === C"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn try_catch_converts_not_callable_into_type_error_object() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"try { (0)(); } catch(e) { e.name }"#).unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn try_catch_converts_builtin_type_error_into_type_error_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { Object.setPrototypeOf({}, 1); } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn try_catch_converts_not_constructable_into_type_error_object() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"try { new 0; } catch(e) { e.name }"#).unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn not_constructable_error_has_message() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new 0; } catch(e) { e.message }"#)
    .unwrap();

  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert!(
    actual.contains("constructor"),
    "expected message to contain 'constructor', got {actual:?}"
  );
}

#[test]
fn type_error_object_has_message() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { (0)(); } catch(e) { e.message }"#)
    .unwrap();

  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert!(
    actual.contains("not callable"),
    "expected message to contain 'not callable', got {actual:?}"
  );
}

#[test]
fn try_catch_converts_prototype_cycle_into_type_error_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { var o = {}; Object.setPrototypeOf(o, o); } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn try_catch_converts_invalid_property_descriptor_patch_into_type_error_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"try { Object.defineProperty({}, "x", { value: 1, get: function() {} }); } catch(e) { e.name }"#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn try_catch_converts_prototype_chain_too_deep_into_type_error_object() {
  // Triggering `PrototypeChainTooDeep` requires building a very deep chain.
  //
  // Doing this via `Object.create` is O(N^2) because each `[[SetPrototypeOf]]` check walks the
  // existing chain; build the chain in Rust using unchecked prototype writes (O(N)) and then
  // trigger the checked path from script.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  let global = rt.realm().global_object();
  {
    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(global)).unwrap();

    let mut leaf = scope.alloc_object().unwrap();
    let leaf_root = scope.heap_mut().add_root(Value::Object(leaf)).unwrap();

    // Build a chain of length `MAX_PROTOTYPE_CHAIN + 1` so the next checked traversal fails.
    for _ in 0..MAX_PROTOTYPE_CHAIN {
      let obj = scope.alloc_object().unwrap();
      unsafe {
        scope
          .heap_mut()
          .object_set_prototype_unchecked(obj, Some(leaf))
          .unwrap();
      }
      leaf = obj;
      scope.heap_mut().set_root(leaf_root, Value::Object(leaf));
    }

    let key = PropertyKey::from_string(scope.alloc_string("deep").unwrap());
    let ok = scope
      .create_data_property(global, key, Value::Object(leaf))
      .unwrap();
    assert!(ok);

    scope.heap_mut().remove_root(leaf_root);
  }

  let value = rt
    .exec_script(r#"try { Object.setPrototypeOf({}, deep); } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn class_constructor_and_methods_execute() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        constructor(x) { this.x = x; }
        m() { return this.x; }
        static s() { return 7; }
      }
      new C(1).m() + C.s()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(8.0));
}

#[test]
fn class_constructor_length_matches_explicit_constructor() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"class C { constructor(a, b) {} } C.length"#).unwrap();
  assert_eq!(value, Value::Number(2.0));
}

#[test]
fn class_constructor_is_not_callable_without_new() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"class C {} try { C(); "no" } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn class_prototype_property_is_not_writable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      "use strict";
      class C {}
      try { C.prototype = {}; "no" } catch(e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn class_methods_are_not_enumerable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C { m(){} static s(){} }
      var a = "";
      for (var k in C.prototype) { a = a + k; }
      var b = "";
      for (var k in C) { b = b + k; }
      a + b
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "");
}

#[test]
fn class_methods_are_strict_mode() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C { m(){ __class_strict_test__ = 1; } }
      try { new C().m(); "no" } catch(e) { e.name }
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ReferenceError");
}

#[test]
fn class_expression_constructor_and_methods_execute() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var C = class {
        constructor(x) { this.x = x; }
        m() { return this.x; }
      };
      new C(2).m()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(2.0));
}

#[test]
fn class_declaration_has_inner_immutable_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static self() { return C; }
        static assign() {
          try { C = 1; return "no"; } catch(e) { return e.name; }
        }
      }
      var D = C;
      C = 1;
      (D.self() === D) && (D.assign() === "TypeError") && (C === 1)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn named_class_expression_creates_inner_const_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var X = class C {
        static f() { return C; }
        static g() { try { C = 1; return "no"; } catch(e) { return e.name; } }
      };

      var a = X.f() === X;
      var b;
      try { C; b = "no"; } catch(e) { b = e.name; }
      var c = X.g();

      a && b === "ReferenceError" && c === "TypeError"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn anonymous_function_names_are_inferred_from_identifier_bindings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var f = function() {};
      var a = () => {};
      f.name === "f" && a.name === "a"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn anonymous_class_name_is_inferred_from_identifier_binding() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var C = class {}; C.name === "C""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn assignment_expression_sets_function_name_for_anonymous_functions() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var f; f = function() {}; f.name === "f""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn assignment_expression_does_not_set_function_name_for_property_assignments() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = {};
      o.f = function() {};
      o.c = class {};
      o.a = () => {};
      o.f.name === "" && o.c.name === "" && o.a.name === ""
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn named_function_expression_name_is_not_overwritten() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var f = function g() {}; f.name === "g""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}
