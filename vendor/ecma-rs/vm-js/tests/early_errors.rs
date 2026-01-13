use vm_js::{Heap, HeapLimits, JsRuntime, SourceTextModuleRecord, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn return_outside_function_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("return 1;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn break_outside_loop_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("break;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_outside_loop_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("continue;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_to_non_iteration_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("lbl: { continue lbl; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn rest_parameter_must_be_last() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function f(...a, b) {}").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn optional_chaining_is_invalid_assignment_target() {
  let mut rt = new_runtime();
  let err = rt.exec_script("var o = null; o?.x = 1;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn optional_chaining_is_invalid_destructuring_assignment_target() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("var o = null; ({ a: o?.x } = { a: 1 });")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn optional_chaining_on_super_member_is_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      try {
        eval("class B {} class A extends B { m() { super?.bar; } }");
      } catch (e) {
        ok = e && e.name === "SyntaxError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_on_super_computed_member_is_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      try {
        eval("class B {} class A extends B { m() { super?.['bar']; } }");
      } catch (e) {
        ok = e && e.name === "SyntaxError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_on_super_call_is_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      try {
        eval("class B {} class A extends B { constructor() { super?.(); } }");
      } catch (e) {
        ok = e && e.name === "SyntaxError";
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn duplicate_parameter_names_in_strict_mode_are_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#""use strict"; function f(a, a) {}"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn duplicate_parameter_names_in_non_simple_parameter_list_are_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function f(a = 0, a) {}").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_outside_async_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function f(){ await 1; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_in_async_function_params_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("async function f(a = await 1) {}").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn yield_in_generator_function_params_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function* g(a = yield 1) {}").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_in_class_field_initializer_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"async function f(){ class C { x = await 0; } }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn yield_in_class_field_initializer_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"function* g(){ class C { x = yield 0; } }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_await_as_binding_identifier_in_async_generator_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"async function* g(){ var \u0061wait; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_await_as_label_in_async_generator_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"async function* g(){ \u0061wait: for(;;) break \u0061wait; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_eval_as_function_name_in_strict_mode_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#""use strict"; function \u0065val() {}"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_eval_assignment_target_in_strict_mode_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#""use strict"; \u0065val = 1;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_as_binding_identifier_in_module_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, "let await = 1;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_as_binding_identifier_in_nested_function_in_module_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, "function f(){ let await = 1; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_await_as_import_binding_identifier_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, r#"import { \u0061wait } from "m";"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_eval_as_import_binding_identifier_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, r#"import { \u0065val } from "m";"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn eval_early_errors_are_catchable_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      try { eval("break"); } catch (e) { ok = e.name === "SyntaxError"; }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn function_constructor_early_errors_are_catchable_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = false;
      try { new Function("a", "a?.b = 1"); } catch (e) { ok = e.name === "SyntaxError"; }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_statement_head_const_decl_conflicts_with_body_var_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("for (const x = 0; false; ) { var x; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn for_statement_head_let_decl_conflicts_with_body_var_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("for (let x = 0; false; ) { var x; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn async_function_param_name_conflicts_with_body_lexical_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function foo(bar) { let bar; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn function_param_name_conflicts_with_body_lexical_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function foo(bar) { let bar; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn strict_mode_yield_identifier_reference_in_destructuring_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#""use strict"; for ({ yield } in [{}]) ;"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn let_newline_await_disambiguates_to_lexical_decl_syntax_error_in_async_fn() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ let\nawait 0; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn let_newline_yield_disambiguates_to_lexical_decl_syntax_error_in_generator_fn() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function* g(){ let\nyield 0; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_at_script_top_level_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("using x = null;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_in_for_in_head_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("for (using x in [1,2,3]) {}").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_in_switch_case_clause_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("switch (true) { case true: using x = null; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_in_switch_default_clause_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("switch (true) { default: using x = null; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_does_not_allow_destructuring_pattern_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("{ using [] = null; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_at_script_top_level_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("await using x = null;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_does_not_allow_object_destructuring_pattern_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("{ using {} = null; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_does_not_allow_destructuring_pattern_in_declarator_list_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ await using x = null, [] = null; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_in_for_in_head_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ for (await using x in [1,2,3]) {} }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_in_switch_clause_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ switch (true) { default: await using x = null; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_in_for_in_head_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ for (await using x in [1]) {} }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_in_switch_case_clause_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ switch (true) { case true: await using x = null; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_in_switch_default_clause_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ switch (true) { default: await using x = null; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_does_not_allow_destructuring_pattern_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
      async function f() {
        {
          await using [] = null;
        }
      }
    "#,
    )
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn using_declaration_missing_initializer_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("{ using x; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_using_declaration_missing_initializer_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ { await using x; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}
