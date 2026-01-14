use crate::ast::expr::pat::Pat;
use crate::ast::expr::Expr;
use crate::ast::stmt::Stmt;
use crate::error::SyntaxErrorType;
use crate::lex::Lexer;
use crate::parse::Parser;
use crate::{Dialect, ParseOptions, SourceType};

fn ecma_script_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  }
}

#[test]
fn decodes_unicode_escapes_in_identifier_tokens() {
  let opts = ecma_script_opts();
  let lexer = Lexer::new("var \\u0061sync = 1; \\u0061sync;");
  let mut parser = Parser::new(lexer, opts);
  let top = parser.parse_top_level().unwrap();

  let body = &top.stx.body;
  assert_eq!(body.len(), 2);

  let Stmt::VarDecl(var_decl) = body[0].stx.as_ref() else {
    panic!("expected VarDecl statement");
  };
  let decl = &var_decl.stx.declarators[0];
  let Pat::Id(id_pat) = decl.pattern.stx.pat.stx.as_ref() else {
    panic!("expected identifier pattern");
  };
  assert_eq!(id_pat.stx.name, "async");

  let Stmt::Expr(expr_stmt) = body[1].stx.as_ref() else {
    panic!("expected ExprStmt statement");
  };
  let Expr::Id(id_expr) = expr_stmt.stx.expr.stx.as_ref() else {
    panic!("expected identifier expression");
  };
  assert_eq!(id_expr.stx.name, "async");
}

#[test]
fn rejects_escaped_yield_and_await_when_disallowed() {
  let opts = ecma_script_opts();

  // `yield` is a reserved word in generator bodies, even when spelled with escapes.
  let lexer = Lexer::new("function* g() { var yi\\u0065ld = 1; }");
  let mut parser = Parser::new(lexer, opts);
  let err = parser.parse_top_level().unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("identifier"));

  // `await` is a reserved word in async function bodies, even when spelled with escapes.
  let lexer = Lexer::new("async function f() { var a\\u0077ait = 1; }");
  let mut parser = Parser::new(lexer, opts);
  let err = parser.parse_top_level().unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::ExpectedSyntax("identifier"));
}
