use vm_js::{SourceTextModuleRecord, VmError};

fn assert_module_syntax_error(source: &str) {
  match SourceTextModuleRecord::parse(source) {
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
// - language/module-code/early-lex-and-var.js
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
