use vm_js::{Heap, HeapLimits, SourceTextModuleRecord, VmError};

fn assert_module_syntax_error(source: &str) {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  match SourceTextModuleRecord::parse(&mut heap, source) {
    Err(VmError::Syntax(_)) => {}
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

// Test262:
// - language/module-code/early-dup-top-function.js
// - language/module-code/early-dup-top-function-generator.js
// - language/module-code/early-dup-top-function-async.js
// - language/module-code/early-dup-top-function-async-generator.js
// - language/module-code/parse-err-hoist-lex-fun.js
// - language/module-code/parse-err-hoist-lex-gen.js
// - language/module-code/parse-err-export-dflt-expr.js
// - language/module-code/parse-err-semi-export-star.js
// - language/module-code/parse-err-semi-name-space-export.js
// - language/module-code/parse-err-semi-named-export.js
// - language/module-code/parse-err-semi-named-export-from.js
// - language/module-code/early-lex-and-var.js
// - language/module-code/early-dup-lex.js
// - language/module-code/early-import-eval.js
// - language/module-code/early-import-arguments.js
// - language/module-code/early-import-as-eval.js
// - language/module-code/early-import-as-arguments.js
// - language/module-code/early-dup-export-id.js
// - language/module-code/early-dup-export-decl.js
// - language/module-code/early-dup-export-dflt-id.js
// - language/module-code/early-dup-export-dflt.js
// - language/module-code/early-dup-export-id-as.js
// - language/module-code/early-dup-export-as-star-as.js
// - language/module-code/early-dup-export-star-as-dflt.js
// - language/module-code/early-export-global.js
// - language/module-code/early-export-unresolvable.js
// - language/module-code/early-dup-lables.js
// - language/module-code/early-new-target.js
// - language/module-code/early-strict-mode.js
// - language/module-code/early-super.js
// - language/module-code/early-undef-break.js
// - language/module-code/early-undef-continue.js
#[test]
fn rejects_duplicate_top_level_function_decls() {
  assert_module_syntax_error(
    r#"
      function x() {}
      function x() {}
    "#,
  );
}

#[test]
fn rejects_duplicate_top_level_generator_function_decls() {
  assert_module_syntax_error(
    r#"
      function x() {}
      function* x() {}
    "#,
  );
}

#[test]
fn rejects_duplicate_top_level_async_function_decls() {
  assert_module_syntax_error(
    r#"
      function x() {}
      async function x() {}
    "#,
  );
}

#[test]
fn rejects_duplicate_top_level_async_generator_function_decls() {
  assert_module_syntax_error(
    r#"
      function x() {}
      async function* x() {}
    "#,
  );
}

#[test]
fn rejects_var_and_top_level_function_name_collision() {
  assert_module_syntax_error(
    r#"
      var f;
      function f() {}
    "#,
  );
}

#[test]
fn rejects_var_and_top_level_generator_function_name_collision() {
  assert_module_syntax_error(
    r#"
      var g;
      function* g() {}
    "#,
  );
}

#[test]
fn rejects_export_default_sequence_expression() {
  // `export default` parses an AssignmentExpression, so `,` is not permitted unless
  // parenthesized.
  assert_module_syntax_error("export default null, null;");
}

#[test]
fn rejects_export_star_without_semicolon_or_line_terminator() {
  assert_module_syntax_error(r#"export * from "./m.js" null;"#);
}

#[test]
fn rejects_export_star_as_namespace_without_semicolon_or_line_terminator() {
  assert_module_syntax_error(r#"export * as ns from "./m.js" null;"#);
}

#[test]
fn rejects_export_named_exports_without_semicolon_or_line_terminator() {
  assert_module_syntax_error("export {} null;");
}

#[test]
fn rejects_export_named_exports_from_clause_without_semicolon_or_line_terminator() {
  assert_module_syntax_error(r#"export {} from "./m.js" null;"#);
}

#[test]
fn rejects_let_and_var_name_collision() {
  assert_module_syntax_error(
    r#"
      let x;
      var x;
    "#,
  );
}

#[test]
fn rejects_let_and_top_level_function_name_collision() {
  assert_module_syntax_error(
    r#"
      let f;
      function f() {}
    "#,
  );
}

#[test]
fn rejects_duplicate_lexical_names_in_module_scope() {
  assert_module_syntax_error(
    r#"
      let x;
      const x = 0;
    "#,
  );
}

#[test]
fn rejects_import_binding_named_eval() {
  assert_module_syntax_error(r#"import { eval } from "./m.js";"#);
}

#[test]
fn rejects_import_binding_named_arguments() {
  assert_module_syntax_error(r#"import { arguments } from "./m.js";"#);
}

#[test]
fn rejects_import_binding_aliased_to_eval() {
  assert_module_syntax_error(r#"import { x as eval } from "./m.js";"#);
}

#[test]
fn rejects_import_binding_aliased_to_arguments() {
  assert_module_syntax_error(r#"import { x as arguments } from "./m.js";"#);
}

#[test]
fn rejects_duplicate_exported_names() {
  assert_module_syntax_error(
    r#"
      var x;
      export { x };
      export { x };
    "#,
  );
}

#[test]
fn rejects_exporting_unresolvable_binding() {
  assert_module_syntax_error(r#"export { unresolvable };"#);
}

#[test]
fn rejects_exporting_global_binding() {
  assert_module_syntax_error(r#"export { Number };"#);
}

#[test]
fn rejects_duplicate_exported_name_default_vs_named() {
  assert_module_syntax_error(
    r#"
      var x, y;
      export default x;
      export { y as default };
    "#,
  );
}

#[test]
fn rejects_invalid_export_default_var_syntax() {
  // test262 `early-dup-export-dflt.js` uses `export default var ...`, which is invalid syntax
  // (duplicate exported names are moot once parsing fails).
  assert_module_syntax_error(
    r#"
      export default var x = null;
      export default var x = null;
    "#,
  );
}

#[test]
fn rejects_duplicate_exported_name_between_function_and_generator_decls() {
  assert_module_syntax_error(
    r#"
      export function f() {}
      export function* f() {}
    "#,
  );
}

#[test]
fn rejects_duplicate_exported_name_between_named_and_export_star_as() {
  assert_module_syntax_error(
    r#"
      var x;
      export { x as z };
      export * as z from "./m.js";
    "#,
  );
}

#[test]
fn rejects_duplicate_exported_name_between_default_and_export_star_as_default() {
  assert_module_syntax_error(
    r#"
      var x;
      export default x;
      export * as default from "./m.js";
    "#,
  );
}

#[test]
fn rejects_duplicate_exported_names_via_alias() {
  assert_module_syntax_error(
    r#"
      var x, y;
      export { x as z };
      export { y as z };
    "#,
  );
}

#[test]
fn rejects_duplicate_labels_in_module_item_list() {
  assert_module_syntax_error(
    r#"
      label: {
        label: 0;
      }
    "#,
  );
}

#[test]
fn rejects_new_target_in_module_item_list() {
  assert_module_syntax_error("new.target;");
}

#[test]
fn rejects_strict_mode_reserved_words_in_module_code() {
  // Modules are always strict mode code.
  assert_module_syntax_error("var public;");
}

#[test]
fn rejects_super_in_module_item_list() {
  assert_module_syntax_error("super;");
}

#[test]
fn rejects_break_to_undefined_label_in_module_code() {
  assert_module_syntax_error(
    r#"
      while (false) {
        break undef;
      }
    "#,
  );
}

#[test]
fn rejects_continue_to_undefined_label_in_module_code() {
  assert_module_syntax_error(
    r#"
      while (false) {
        continue undef;
      }
    "#,
  );
}
