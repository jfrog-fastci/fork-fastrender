use parse_js::ast::expr::Expr;
use parse_js::ast::func::FuncBody;
use parse_js::ast::stmt::Stmt;
use parse_js::operator::OperatorName;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

fn ecma_script_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  }
}

fn parse_ecma_script(src: &str) -> parse_js::ast::node::Node<parse_js::ast::stx::TopLevel> {
  parse_with_options(src, ecma_script_opts()).expect("parse")
}

#[test]
fn using_newline_let_splits_statements_in_sloppy_script() {
  // Explicit Resource Management `using` declarations have a [no LineTerminator here] restriction
  // before the first BindingIdentifier. `using\\nlet = ...` must therefore parse as:
  //   using;
  //   let = ...;
  let src = "using\nlet = 1;";
  let ast = parse_ecma_script(src);
  assert_eq!(ast.stx.body.len(), 2);

  let stmt0 = &ast.stx.body[0];
  let Stmt::Expr(expr_stmt) = stmt0.stx.as_ref() else {
    panic!("expected ExprStmt, got {:?}", stmt0.stx);
  };
  let Expr::Id(id) = expr_stmt.stx.expr.stx.as_ref() else {
    panic!("expected IdExpr, got {:?}", expr_stmt.stx.expr.stx);
  };
  assert_eq!(id.stx.name, "using");

  let stmt1 = &ast.stx.body[1];
  let Stmt::Expr(expr_stmt) = stmt1.stx.as_ref() else {
    panic!("expected ExprStmt, got {:?}", stmt1.stx);
  };
  let Expr::Binary(binary) = expr_stmt.stx.expr.stx.as_ref() else {
    panic!("expected BinaryExpr, got {:?}", expr_stmt.stx.expr.stx);
  };
  assert_eq!(binary.stx.operator, OperatorName::Assignment);
  let Expr::IdPat(id) = binary.stx.left.stx.as_ref() else {
    panic!("expected IdPat assignment target, got {:?}", binary.stx.left.stx);
  };
  assert_eq!(id.stx.name, "let");
}

#[test]
fn using_element_access_is_not_parsed_as_using_declaration() {
  // `using[x] = null` must remain a computed-member assignment, not a `using` declaration.
  let src = "using[x] = null;";
  let ast = parse_ecma_script(src);
  assert_eq!(ast.stx.body.len(), 1);

  let stmt0 = &ast.stx.body[0];
  let Stmt::Expr(expr_stmt) = stmt0.stx.as_ref() else {
    panic!("expected ExprStmt, got {:?}", stmt0.stx);
  };
  let Expr::Binary(binary) = expr_stmt.stx.expr.stx.as_ref() else {
    panic!("expected BinaryExpr, got {:?}", expr_stmt.stx.expr.stx);
  };
  assert_eq!(binary.stx.operator, OperatorName::Assignment);
  let Expr::ComputedMember(member) = binary.stx.left.stx.as_ref() else {
    panic!("expected computed member assignment target, got {:?}", binary.stx.left.stx);
  };
  let Expr::Id(obj) = member.stx.object.stx.as_ref() else {
    panic!("expected identifier base, got {:?}", member.stx.object.stx);
  };
  assert_eq!(obj.stx.name, "using");
}

#[test]
fn await_using_element_access_is_await_expression_not_declaration() {
  // `await using[x];` must parse as `await (using[x])`, not as an `await using` declaration.
  let src = "async function f() { await using[x]; }";
  let ast = parse_ecma_script(src);
  assert_eq!(ast.stx.body.len(), 1);

  let stmt0 = &ast.stx.body[0];
  let Stmt::FunctionDecl(func_decl) = stmt0.stx.as_ref() else {
    panic!("expected FunctionDecl, got {:?}", stmt0.stx);
  };
  let func = &func_decl.stx.function.stx;
  assert!(func.async_);
  let Some(FuncBody::Block(body)) = &func.body else {
    panic!("expected function body block");
  };
  assert_eq!(body.len(), 1);

  let stmt = &body[0];
  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt, got {:?}", stmt.stx);
  };
  let Expr::Unary(unary) = expr_stmt.stx.expr.stx.as_ref() else {
    panic!("expected UnaryExpr, got {:?}", expr_stmt.stx.expr.stx);
  };
  assert_eq!(unary.stx.operator, OperatorName::Await);
  let Expr::ComputedMember(member) = unary.stx.argument.stx.as_ref() else {
    panic!("expected computed member operand, got {:?}", unary.stx.argument.stx);
  };
  let Expr::Id(obj) = member.stx.object.stx.as_ref() else {
    panic!("expected identifier base, got {:?}", member.stx.object.stx);
  };
  assert_eq!(obj.stx.name, "using");
}

#[test]
fn await_using_newline_let_splits_statements_in_async_function() {
  // `await using\\nlet = ...` must parse as two statements:
  //   await using;
  //   let = ...;
  let src = "async function f() { await using\nlet = 1; }";
  let ast = parse_ecma_script(src);

  let stmt0 = ast.stx.body.first().expect("function decl");
  let Stmt::FunctionDecl(func_decl) = stmt0.stx.as_ref() else {
    panic!("expected FunctionDecl, got {:?}", stmt0.stx);
  };
  let func = &func_decl.stx.function.stx;
  let Some(FuncBody::Block(body)) = &func.body else {
    panic!("expected function body block");
  };
  assert_eq!(body.len(), 2);

  let Stmt::Expr(await_stmt) = body[0].stx.as_ref() else {
    panic!("expected ExprStmt for `await using`");
  };
  let Expr::Unary(await_expr) = await_stmt.stx.expr.stx.as_ref() else {
    panic!("expected UnaryExpr for `await using`");
  };
  assert_eq!(await_expr.stx.operator, OperatorName::Await);

  let Stmt::Expr(assign_stmt) = body[1].stx.as_ref() else {
    panic!("expected ExprStmt for assignment");
  };
  let Expr::Binary(assign_expr) = assign_stmt.stx.expr.stx.as_ref() else {
    panic!("expected BinaryExpr for assignment");
  };
  assert_eq!(assign_expr.stx.operator, OperatorName::Assignment);
  let Expr::IdPat(id) = assign_expr.stx.left.stx.as_ref() else {
    panic!("expected IdPat assignment target");
  };
  assert_eq!(id.stx.name, "let");
}

#[test]
fn using_declarations_reject_binding_patterns() {
  // `using` declarations only allow BindingIdentifier, not BindingPattern.
  let src = "{ using [] = null; }";
  assert!(parse_with_options(src, ecma_script_opts()).is_err());
}

#[test]
fn await_using_declarations_reject_binding_patterns_after_comma() {
  // `await using` declarations only allow BindingIdentifier, even after commas.
  let src = "async function f() { await using x = null, [] = null; }";
  assert!(parse_with_options(src, ecma_script_opts()).is_err());
}

#[test]
fn for_of_using_of_of_disambiguates_using_as_identifier() {
  // Spec lookahead restriction: `for (using of ...)` must treat `using` as an identifier assignment
  // target, not as a `using` declaration.
  //
  // `of [0,1,2]` is computed member access (`of[0,1,2]` => `of[2]`), matching the test262 syntax
  // fixture for this disambiguation.
  let src = "for (using of of [0, 1, 2]) {}";
  let ast = parse_ecma_script(src);
  assert_eq!(ast.stx.body.len(), 1);

  let stmt0 = &ast.stx.body[0];
  let Stmt::ForOf(for_of) = stmt0.stx.as_ref() else {
    panic!("expected ForOfStmt, got {:?}", stmt0.stx);
  };
  assert!(!for_of.stx.await_);

  let parse_js::ast::stmt::ForInOfLhs::Assign(pat) = &for_of.stx.lhs else {
    panic!("expected assignment lhs");
  };
  let parse_js::ast::expr::pat::Pat::Id(id) = pat.stx.as_ref() else {
    panic!("expected identifier assignment target");
  };
  assert_eq!(id.stx.name, "using");

  let Expr::ComputedMember(member) = for_of.stx.rhs.stx.as_ref() else {
    panic!("expected computed member rhs");
  };
  let Expr::Id(obj) = member.stx.object.stx.as_ref() else {
    panic!("expected identifier base");
  };
  assert_eq!(obj.stx.name, "of");
}

#[test]
fn for_of_await_using_of_of_parses_as_await_using_declaration() {
  // `for (await using of of []) {}` is an `await using` declaration whose BindingIdentifier is
  // `of`, followed by the `for-of` `of` keyword.
  let src = "async function f() { for (await using of of []) {} }";
  let ast = parse_ecma_script(src);
  assert_eq!(ast.stx.body.len(), 1);

  let stmt0 = &ast.stx.body[0];
  let Stmt::FunctionDecl(func_decl) = stmt0.stx.as_ref() else {
    panic!("expected FunctionDecl, got {:?}", stmt0.stx);
  };
  let func = &func_decl.stx.function.stx;
  assert!(func.async_);
  let Some(FuncBody::Block(body)) = &func.body else {
    panic!("expected function body block");
  };
  assert_eq!(body.len(), 1);

  let Stmt::ForOf(for_of) = body[0].stx.as_ref() else {
    panic!("expected ForOfStmt");
  };
  let parse_js::ast::stmt::ForInOfLhs::Decl((mode, pat_decl)) = &for_of.stx.lhs else {
    panic!("expected declaration lhs");
  };
  assert_eq!(*mode, parse_js::ast::stmt::decl::VarDeclMode::AwaitUsing);
  let parse_js::ast::expr::pat::Pat::Id(id) = pat_decl.stx.pat.stx.as_ref() else {
    panic!("expected BindingIdentifier");
  };
  assert_eq!(id.stx.name, "of");
}

#[test]
fn for_triple_using_of_initializer_is_for_loop_not_for_of() {
  // `for (using of = ...;;)` is a `for(;;)` loop whose init is a `using` declaration of a binding
  // named `of`. (The `using of` lookahead restriction only applies to `for-of` / `for-await-of`.)
  let src = "for (using of = null;;) {}";
  let ast = parse_ecma_script(src);
  assert_eq!(ast.stx.body.len(), 1);

  let stmt0 = &ast.stx.body[0];
  let Stmt::ForTriple(triple) = stmt0.stx.as_ref() else {
    panic!("expected ForTripleStmt, got {:?}", stmt0.stx);
  };
  let parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) = &triple.stx.init else {
    panic!("expected decl init");
  };
  assert_eq!(decl.stx.mode, parse_js::ast::stmt::decl::VarDeclMode::Using);
  let first = decl.stx.declarators.first().expect("declarator");
  let parse_js::ast::expr::pat::Pat::Id(id) = first.pattern.stx.pat.stx.as_ref() else {
    panic!("expected BindingIdentifier");
  };
  assert_eq!(id.stx.name, "of");
}

#[test]
fn using_decl_is_syntax_error_in_for_in_head() {
  // `using` declarations are not permitted in `for-in` statement heads.
  let src = "for (using x in [1,2,3]) {}";
  assert!(parse_with_options(src, ecma_script_opts()).is_err());
}
