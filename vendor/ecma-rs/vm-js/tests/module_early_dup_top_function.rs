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
