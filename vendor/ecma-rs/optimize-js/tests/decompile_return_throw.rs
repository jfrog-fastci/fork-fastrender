#[path = "common/mod.rs"]
mod common;
use common::compile_source;
use optimize_js::decompile::il::decompile_function;
use optimize_js::TopLevelMode;
use parse_js::ast::stmt::Stmt;

#[test]
fn decompile_function_emits_return_stmt() {
  let src = r#"
    const make = () => {
      return 1;
    };
    make();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  let func = program.functions.get(0).expect("expected nested function");
  let stmts = decompile_function(func).expect("decompile");
  assert!(
    stmts.iter().any(|stmt| matches!(stmt.stx.as_ref(), Stmt::Return(_))),
    "expected a Return statement, got: {stmts:?}"
  );
}

#[test]
fn decompile_function_emits_throw_stmt() {
  let src = r#"
    const fail = () => {
      throw 1;
    };
    fail();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  let func = program.functions.get(0).expect("expected nested function");
  let stmts = decompile_function(func).expect("decompile");
  assert!(
    stmts.iter().any(|stmt| matches!(stmt.stx.as_ref(), Stmt::Throw(_))),
    "expected a Throw statement, got: {stmts:?}"
  );
}

