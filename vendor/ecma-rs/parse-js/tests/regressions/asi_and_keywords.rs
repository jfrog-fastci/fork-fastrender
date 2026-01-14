use parse_js::ast::stmt::Stmt;
use parse_js::error::SyntaxErrorType;
use parse_js::operator::OperatorName;
use parse_js::parse_with_options;
use parse_js::ast::func::FuncBody;
use parse_js::{Dialect, ParseOptions, SourceType};

fn ecma_script_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  }
}

fn ecma_module_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Module,
  }
}

#[test]
fn asi_splits_identifiers_only_across_line_terminators() {
  let parsed = parse_with_options("a\nb", ecma_script_opts()).expect("expected ASI split");
  assert_eq!(parsed.stx.body.len(), 2);
  assert!(matches!(parsed.stx.body[0].stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(parsed.stx.body[1].stx.as_ref(), Stmt::Expr(_)));

  assert!(parse_with_options("a b", ecma_script_opts()).is_err());
}

#[test]
fn asi_does_not_split_before_brace_without_line_terminator() {
  assert!(parse_with_options("a {}", ecma_script_opts()).is_err());
}

#[test]
fn let_in_statement_position_allows_asi_split_before_identifier() {
  // `let` may be an IdentifierReference in non-strict scripts. In statement
  // positions, a LineTerminator after `let` allows ASI to split it into its own
  // ExpressionStatement.
  let parsed =
    parse_with_options("if (false) let // ASI\nx = 1;", ecma_script_opts()).expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 2);

  let Stmt::If(if_stmt) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected if statement");
  };
  assert!(matches!(if_stmt.stx.consequent.stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(parsed.stx.body[1].stx.as_ref(), Stmt::Expr(_)));
}

#[test]
fn let_in_statement_position_allows_asi_split_before_block() {
  let parsed = parse_with_options("if (false) let // ASI\n{}", ecma_script_opts()).expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 2);

  let Stmt::If(if_stmt) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected if statement");
  };
  assert!(matches!(if_stmt.stx.consequent.stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(parsed.stx.body[1].stx.as_ref(), Stmt::Block(_)));
}

#[test]
fn let_in_statement_position_rejects_let_bracket_lookahead_even_with_line_terminator() {
  // ExpressionStatement has a lookahead restriction for `let [` and this applies
  // even when `let` and `[` are split by a LineTerminator.
  let err = parse_with_options("if (false) let\n[a] = 0;", ecma_script_opts()).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("statement (not a declaration)")
  );
}

#[test]
fn let_in_for_body_allows_asi_split() {
  // `for` loop bodies use the same `stmt_in_statement_position` parsing as
  // `if` statements, but do not permit legacy function declarations. Ensure the
  // `let`/ASI disambiguation works in both codepaths.
  let parsed = parse_with_options(
    "for (; false; ) let // ASI\nx = 1;",
    ecma_script_opts(),
  )
  .expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 2);

  let Stmt::ForTriple(for_stmt) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected for statement");
  };
  assert_eq!(for_stmt.stx.body.stx.body.len(), 1);
  assert!(matches!(
    for_stmt.stx.body.stx.body[0].stx.as_ref(),
    Stmt::Expr(_)
  ));
  assert!(matches!(parsed.stx.body[1].stx.as_ref(), Stmt::Expr(_)));
}

#[test]
fn using_in_statement_position_allows_asi_split_before_identifier() {
  // `using` is contextual: it can start a `using` declaration or be an IdentifierReference.
  // In statement positions, declarations are not permitted, but ASI can still split:
  // `if (false) using // ASI\nx = 1;` => `if (false) using; x = 1;`
  let parsed =
    parse_with_options("if (false) using // ASI\nx = 1;", ecma_script_opts()).expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 2);

  let Stmt::If(if_stmt) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected if statement");
  };
  assert!(matches!(if_stmt.stx.consequent.stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(parsed.stx.body[1].stx.as_ref(), Stmt::Expr(_)));
}

#[test]
fn using_in_for_body_allows_asi_split() {
  // `for` loop bodies use the same `stmt_in_statement_position` parsing as
  // `if` statements, but do not permit legacy function declarations. Ensure the
  // `using`/ASI disambiguation works in both codepaths.
  let parsed = parse_with_options(
    "for (; false; ) using // ASI\nx = 1;",
    ecma_script_opts(),
  )
  .expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 2);

  let Stmt::ForTriple(for_stmt) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected for statement");
  };
  assert_eq!(for_stmt.stx.body.stx.body.len(), 1);
  assert!(matches!(
    for_stmt.stx.body.stx.body[0].stx.as_ref(),
    Stmt::Expr(_)
  ));
  assert!(matches!(parsed.stx.body[1].stx.as_ref(), Stmt::Expr(_)));
}

#[test]
fn await_using_in_statement_position_allows_asi_split_before_identifier() {
  // `await using` is contextual: it can start an `await using` declaration, or
  // be an `await` expression whose operand is an IdentifierReference named `using`.
  // Like `using`, statement positions disallow declarations, but ASI can still split:
  // `if (false) await using // ASI\nx = 1;` => `if (false) await using; x = 1;`
  let parsed = parse_with_options(
    "async function f() { if (false) await using // ASI\nx = 1; }",
    ecma_script_opts(),
  )
  .expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 1);

  let Stmt::FunctionDecl(func_decl) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected function declaration");
  };
  let Some(FuncBody::Block(body)) = &func_decl.stx.function.stx.body else {
    panic!("expected function body block");
  };
  assert_eq!(body.len(), 2);

  let Stmt::If(if_stmt) = body[0].stx.as_ref() else {
    panic!("expected if statement");
  };
  assert!(matches!(if_stmt.stx.consequent.stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(body[1].stx.as_ref(), Stmt::Expr(_)));
}

#[test]
fn labelled_function_in_statement_position_is_syntax_error() {
  // Static Semantics: IsLabelledFunction(Statement) early errors apply to statement positions
  // (if/while/do/for/with bodies), regardless of non-strict Annex B allowances.
  for src in [
    "if (false) label1: label2: function f() {}",
    "while (false) label1: label2: function f() {}",
    "do label1: label2: function f() {} while (false)",
    "for (; false; ) label1: label2: function f() {}",
    "with ({}) label1: label2: function f() {}",
  ] {
    assert!(
      parse_with_options(src, ecma_script_opts()).is_err(),
      "expected parse error for {src:?}"
    );
  }
}

#[test]
fn labelled_function_outside_statement_position_remains_allowed_in_non_strict() {
  // Annex B labelled function declarations remain allowed in non-strict statement-list contexts.
  assert!(parse_with_options("label: function f() {}", ecma_script_opts()).is_ok());
  assert!(parse_with_options("label1: label2: function f() {}", ecma_script_opts()).is_ok());
  assert!(parse_with_options("{ label: function f() {} }", ecma_script_opts()).is_ok());
}

#[test]
fn labelled_function_inside_block_statement_position_remains_allowed() {
  // The IsLabelledFunction early error only applies when the *Statement* itself is in a
  // statement-position context. Wrapping in a block avoids the restriction.
  assert!(parse_with_options(
    "if (false) { label1: label2: function f() {} }",
    ecma_script_opts(),
  )
  .is_ok());
}

#[test]
fn labelled_function_is_disallowed_in_strict_mode_and_modules() {
  // LabelledItem : FunctionDeclaration is a syntax error in strict mode (and modules).
  assert!(parse_with_options(
    "'use strict'; label1: label2: function f() {}",
    ecma_script_opts(),
  )
  .is_err());
  assert!(parse_with_options(
    "label1: label2: function f() {}",
    ecma_module_opts(),
  )
  .is_err());
}

#[test]
fn asi_does_not_backtrack_to_treat_slash_as_regex_literal() {
  // In expression context, `/` is a division operator, not a regex literal. The
  // parser must not insert ASI at an earlier LineTerminator just because later
  // tokens would make the division parse fail.
  assert!(parse_with_options("a\n/b/.test('x')", ecma_script_opts()).is_err());
}

#[test]
fn asi_does_not_split_division_expression_after_line_terminator() {
  // `a\n/b/2` is a valid division expression and must not trigger ASI.
  let parsed =
    parse_with_options("a\n/b/2", ecma_script_opts()).expect("expected division expression");
  assert_eq!(parsed.stx.body.len(), 1);
}

#[test]
fn asi_does_not_split_before_tagged_template() {
  let parsed =
    parse_with_options("tag\n`x`", ecma_script_opts()).expect("expected tagged template parse");
  assert_eq!(parsed.stx.body.len(), 1);
}

#[test]
fn async_keyword_requires_no_line_terminator_before_function() {
  // `async\nfunction` does not form an async function; `async` is an identifier
  // and ASI splits the statements.
  let parsed =
    parse_with_options("async\nfunction f(){}", ecma_script_opts()).expect("expected parse");
  assert_eq!(parsed.stx.body.len(), 2);
  assert!(matches!(parsed.stx.body[0].stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(
    parsed.stx.body[1].stx.as_ref(),
    Stmt::FunctionDecl(_)
  ));
}

#[test]
fn async_keyword_requires_no_line_terminator_before_arrow_parameters() {
  // `async\nx => x` is not an async arrow; `async` is an identifier statement.
  let parsed = parse_with_options("var f = async\nx => x;", ecma_script_opts()).expect("parse ok");
  assert_eq!(parsed.stx.body.len(), 2);
}

#[test]
fn yield_does_not_form_tagged_template_across_line_terminator() {
  let parsed =
    parse_with_options("function* g(){ yield\n`x`; }", ecma_script_opts()).expect("expected parse");
  let Stmt::FunctionDecl(func_decl) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected function declaration");
  };
  let Some(parse_js::ast::func::FuncBody::Block(body)) = &func_decl.stx.function.stx.body else {
    panic!("expected function body");
  };
  assert_eq!(body.len(), 2);
}

#[test]
fn await_allows_line_terminator_before_operand() {
  assert!(parse_with_options("async function f(){ await\nfoo(); }", ecma_script_opts()).is_ok());
}

#[test]
fn await_allows_line_terminator_before_operand_in_module() {
  assert!(parse_with_options("await\nfoo()", ecma_module_opts()).is_ok());
}

#[test]
fn await_requires_operand() {
  assert!(
    parse_with_options("async function f(){ await; }", ecma_script_opts()).is_err(),
    "await must not accept a missing operand"
  );
}

#[test]
fn await_requires_operand_in_module() {
  assert!(
    parse_with_options("await;", ecma_module_opts()).is_err(),
    "await must not accept a missing operand"
  );
}

#[test]
fn yield_star_disallows_line_terminator_between_yield_and_star() {
  let err = parse_with_options("function* g(){ yield\n* other; }", ecma_script_opts()).unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::LineTerminatorAfterYield);
}

#[test]
fn yield_is_restricted_production_across_line_terminators() {
  let parsed = parse_with_options(
    "function* g(){ const x = yield\n+1; return x; }",
    ecma_script_opts(),
  )
  .unwrap();

  let Stmt::FunctionDecl(func_decl) = parsed.stx.body[0].stx.as_ref() else {
    panic!("expected function declaration");
  };
  let Some(parse_js::ast::func::FuncBody::Block(body)) = &func_decl.stx.function.stx.body else {
    panic!("expected function body");
  };

  assert_eq!(body.len(), 3);
  assert!(matches!(body[0].stx.as_ref(), Stmt::VarDecl(_)));
  assert!(matches!(body[1].stx.as_ref(), Stmt::Expr(_)));
  assert!(matches!(body[2].stx.as_ref(), Stmt::Return(_)));

  let Stmt::VarDecl(var_decl) = body[0].stx.as_ref() else {
    unreachable!();
  };
  let init = var_decl.stx.declarators[0]
    .initializer
    .as_ref()
    .expect("initializer missing");
  match init.stx.as_ref() {
    parse_js::ast::expr::Expr::Unary(unary) => assert_eq!(unary.stx.operator, OperatorName::Yield),
    other => panic!("expected yield initializer, got {other:?}"),
  }
}

#[test]
fn yield_requires_parentheses_in_higher_precedence_expressions() {
  let err =
    parse_with_options("function* g(){ return 1 + yield 2; }", ecma_script_opts()).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("parenthesized expression")
  );

  parse_with_options("function* g(){ return 1 + (yield 2); }", ecma_script_opts()).unwrap();
}

#[test]
fn yield_requires_parentheses_in_conditional_test() {
  let err =
    parse_with_options("function* g(){ return yield ? 1 : 2; }", ecma_script_opts()).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("parenthesized expression")
  );

  parse_with_options(
    "function* g(){ return (yield) ? 1 : 2; }",
    ecma_script_opts(),
  )
  .unwrap();
}

#[test]
fn yield_requires_parentheses_before_exponentiation_operand() {
  let err =
    parse_with_options("function* g(){ return 2 ** yield 1; }", ecma_script_opts()).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("parenthesized expression")
  );

  parse_with_options(
    "function* g(){ return 2 ** (yield 1); }",
    ecma_script_opts(),
  )
  .unwrap();
}

#[test]
fn await_requires_parentheses_before_exponentiation_operand() {
  let err = parse_with_options(
    "async function f(){ return await 2 ** 2; }",
    ecma_script_opts(),
  )
  .unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("parenthesized expression")
  );

  parse_with_options(
    "async function f(){ return await (2 ** 2); }",
    ecma_script_opts(),
  )
  .unwrap();
}

#[test]
fn yield_accepts_regex_operand() {
  parse_with_options("function* g(){ yield /x/; }", ecma_script_opts()).unwrap();
}

#[test]
fn await_accepts_regex_operand() {
  parse_with_options(
    "async function f(){ return await /x/.test('x'); }",
    ecma_script_opts(),
  )
  .unwrap();
}

#[test]
fn yield_requires_parentheses_before_relational_operator() {
  let err =
    parse_with_options("function* g(){ return yield < 1; }", ecma_script_opts()).unwrap_err();
  assert_eq!(
    err.typ,
    SyntaxErrorType::ExpectedSyntax("parenthesized expression")
  );

  parse_with_options("function* g(){ return (yield) < 1; }", ecma_script_opts()).unwrap();
}
