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
fn break_to_undefined_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("break missing;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_outside_loop_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("continue;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_to_undefined_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("continue missing;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_to_non_iteration_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("lbl: { continue lbl; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn duplicate_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("lbl: lbl: ;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn break_label_does_not_cross_function_boundary_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("outer: { (() => { break outer; }); }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_label_does_not_cross_function_boundary_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("outer: for(;;) { (() => { for(;;) { continue outer; } }); break; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn duplicate_catch_parameter_bound_names_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("try {} catch ([a, a]) {}").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn catch_parameter_name_conflicts_with_lexical_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("try {} catch (e) { let e; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn catch_parameter_name_conflicts_with_function_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("try {} catch (e) { function e(){} }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn return_in_class_static_block_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { static { return; } }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn break_in_class_static_block_does_not_target_enclosing_loop_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("for(;;) { class C { static { break; } } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_in_class_static_block_does_not_target_enclosing_loop_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("for(;;) { class C { static { continue; } } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn break_label_in_class_static_block_does_not_target_enclosing_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("outer: for(;;) { class C { static { break outer; } } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn continue_label_in_class_static_block_does_not_target_enclosing_label_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("outer: for(;;) { class C { static { continue outer; } } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn arguments_identifier_reference_in_class_static_block_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { static { arguments; } }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn arguments_identifier_reference_in_arrow_in_class_static_block_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { static { () => arguments; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_as_binding_identifier_in_class_static_block_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { static { let await = 0; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn yield_expression_in_class_static_block_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("function* g(){ class C { static { yield 0; } } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn var_and_lexical_decl_conflict_in_class_static_block_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { static { var x; let x; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn duplicate_private_name_in_class_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { #x; #x; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn private_name_may_not_be_both_static_and_instance_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { #x; static #x; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn private_in_operator_without_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("#x in {};").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn delete_private_reference_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { #x; m() { delete this.#x; } }")
    .unwrap_err();
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
fn destructuring_assignment_pattern_with_non_equals_operator_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("({ a } += 1);").unwrap_err();
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
fn super_call_outside_derived_constructor_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class B {} class A extends B { m() { super(); } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn super_call_in_arrow_function_outside_derived_constructor_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class B {} class A extends B { m() { () => super(); } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn with_statement_in_strict_mode_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#""use strict"; with ({}) {}"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
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
fn duplicate_parameter_names_in_function_made_strict_by_directive_are_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"function f(a, a) { "use strict"; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn restricted_eval_param_in_function_made_strict_by_directive_are_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"function f(eval) { "use strict"; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn restricted_arguments_param_in_function_made_strict_by_directive_are_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"function f(arguments) { "use strict"; }"#)
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
fn duplicate_parameter_names_in_arrow_function_are_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("((a, a) => {});").unwrap_err();
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
fn strict_mode_delete_unqualified_identifier_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#""use strict"; delete x;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn strict_mode_assignment_to_arguments_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#""use strict"; arguments = 1;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn strict_mode_postfix_increment_eval_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#""use strict"; eval++;"#).unwrap_err();
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
fn await_identifier_reference_in_destructuring_in_module_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, "({ await } = {});").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_binding_identifier_in_destructuring_declaration_in_module_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, "let { await } = {};").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn let_newline_await_disambiguates_to_lexical_decl_syntax_error_in_module() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, "let\nawait 0;").unwrap_err();
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
fn escaped_await_as_nested_function_decl_name_in_async_function_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"async function f(){ function \u0061wait() {} }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_await_as_class_name_in_module_is_syntax_error() {
  let mut rt = new_runtime();
  let err = SourceTextModuleRecord::parse(&mut rt.heap, r#"class \u0061wait {}"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_yield_as_function_name_in_strict_function_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"function \u0079ield() { "use strict"; }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_yield_as_class_name_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#"class \u0079ield {}"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_yield_in_class_extends_expression_is_syntax_error() {
  // Class definitions are strict-mode code; the `extends` (heritage) expression is evaluated in
  // that strict context as well.
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"class C extends (\u0079ield = 1) {}"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn escaped_eval_assignment_in_class_extends_expression_is_syntax_error() {
  // Strict-mode early errors (like assignment to `eval`) also apply within the `extends` expression.
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#"class C extends (\u0065val = 1) {}"#)
    .unwrap_err();
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
fn lexical_declaration_may_not_bind_let_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("let let = 0;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn const_declaration_may_not_bind_let_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("const let = 0;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn function_decl_name_conflicts_with_lexical_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function f() {} let f;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
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
fn for_statement_head_const_destructuring_decl_conflicts_with_body_var_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("for (const { x } = { x: 0 }; false; ) { var x; }")
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
fn for_statement_head_let_destructuring_decl_conflicts_with_body_var_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("for (let { x } = { x: 0 }; false; ) { var x; }")
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
fn async_function_param_pattern_name_conflicts_with_body_lexical_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function foo({ bar }) { let bar; }")
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
fn function_param_pattern_name_conflicts_with_body_lexical_decl_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("function foo({ bar }) { let bar; }")
    .unwrap_err();
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
fn strict_mode_yield_binding_identifier_in_destructuring_declaration_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#""use strict"; let { yield } = {};"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn strict_mode_yield_identifier_reference_in_destructuring_assignment_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#""use strict"; ({ yield } = {});"#)
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn generator_yield_binding_identifier_in_destructuring_declaration_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("function* g(){ let { yield } = {}; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn generator_yield_identifier_reference_in_destructuring_assignment_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("function* g(){ ({ yield } = {}); }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_identifier_reference_in_destructuring_in_async_fn_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ ({ await } = {}); }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_binding_identifier_in_destructuring_declaration_in_async_fn_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function f(){ let { await } = {}; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_identifier_reference_in_destructuring_in_async_generator_fn_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function* g(){ ({ await } = {}); }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn await_binding_identifier_in_destructuring_declaration_in_async_generator_fn_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function* g(){ let { await } = {}; }")
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
fn let_newline_await_disambiguates_to_lexical_decl_syntax_error_in_async_generator_fn() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function* g(){ let\nawait 0; }")
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
fn let_newline_yield_disambiguates_to_lexical_decl_syntax_error_in_async_generator_fn() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("async function* g(){ let\nyield 0; }")
    .unwrap_err();
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
fn await_using_declaration_in_for_in_head_singleton_is_syntax_error() {
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
