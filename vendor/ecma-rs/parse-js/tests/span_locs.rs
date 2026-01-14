use parse_js::ast::class_or_object::ClassOrObjVal;
use parse_js::ast::class_or_object::ObjMemberType;
use parse_js::ast::expr::lit::LitArrElem;
use parse_js::ast::expr::Expr;
use parse_js::ast::stmt::Stmt;
use parse_js::loc::Loc;
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

fn slice<'a>(src: &'a str, loc: Loc) -> &'a str {
  &src[loc.0..loc.1]
}

#[test]
fn expr_stmt_loc_includes_semicolon_but_expr_loc_does_not() {
  let src = "a;";
  let ast = parse_ecma_script(src);
  let stmt = ast.stx.body.first().expect("statement");
  assert_eq!(slice(src, stmt.loc), "a;");

  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt, got {:?}", stmt.stx);
  };
  let expr = &expr_stmt.stx.expr;
  assert_eq!(slice(src, expr.loc), "a");
}

#[test]
fn call_argument_expr_loc_excludes_trailing_delimiters() {
  let src = "foo(a, b);";
  let ast = parse_ecma_script(src);
  let stmt = ast.stx.body.first().expect("statement");
  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt");
  };
  let expr = &expr_stmt.stx.expr;
  let Expr::Call(call) = expr.stx.as_ref() else {
    panic!("expected CallExpr");
  };

  assert_eq!(slice(src, expr.loc), "foo(a, b)");

  let arg0 = &call.stx.arguments[0].stx.value;
  let arg1 = &call.stx.arguments[1].stx.value;
  assert_eq!(slice(src, arg0.loc), "a");
  assert_eq!(slice(src, arg1.loc), "b");
}

#[test]
fn array_element_expr_loc_excludes_closing_bracket() {
  let src = "[a];";
  let ast = parse_ecma_script(src);
  let stmt = ast.stx.body.first().expect("statement");
  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt");
  };
  let expr = &expr_stmt.stx.expr;
  let Expr::LitArr(arr) = expr.stx.as_ref() else {
    panic!("expected LitArrExpr");
  };

  assert_eq!(slice(src, expr.loc), "[a]");

  let LitArrElem::Single(elem) = &arr.stx.elements[0] else {
    panic!("expected single element");
  };
  assert_eq!(slice(src, elem.loc), "a");
}

#[test]
fn object_prop_value_expr_loc_excludes_closing_brace() {
  let src = "({a:b});";
  let ast = parse_ecma_script(src);
  let stmt = ast.stx.body.first().expect("statement");
  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt");
  };
  let expr = &expr_stmt.stx.expr;
  let Expr::LitObj(obj) = expr.stx.as_ref() else {
    panic!("expected LitObjExpr");
  };

  // `parse-js` does not keep parentheses as nodes; grouped expressions expand the inner node's span
  // to include the parentheses so downstream consumers can slice+reparse reliably.
  assert_eq!(slice(src, expr.loc), "({a:b})");

  let member = obj.stx.members.first().expect("member");
  let ObjMemberType::Valued { val, .. } = &member.stx.typ else {
    panic!("expected valued object member");
  };
  let ClassOrObjVal::Prop(Some(value)) = val else {
    panic!("expected property initializer");
  };
  assert_eq!(slice(src, value.loc), "b");
}

#[test]
fn function_expr_loc_excludes_enclosing_call_delimiters() {
  // Ensure function expressions used as arguments do not include `)` / `;` in their spans.
  let src = "foo(function(){});";
  let ast = parse_ecma_script(src);
  let stmt = ast.stx.body.first().expect("statement");
  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt");
  };
  let expr = &expr_stmt.stx.expr;
  let Expr::Call(call) = expr.stx.as_ref() else {
    panic!("expected CallExpr");
  };
  let arg0 = &call.stx.arguments[0].stx.value;
  assert_eq!(slice(src, arg0.loc), "function(){}");
}

#[test]
fn object_method_func_loc_excludes_enclosing_object_delimiters() {
  // Regression for lazy snippet parsing: method bodies should not absorb the object literal's `}`.
  let src = "({m(){}});";
  let ast = parse_ecma_script(src);
  let stmt = ast.stx.body.first().expect("statement");
  let Stmt::Expr(expr_stmt) = stmt.stx.as_ref() else {
    panic!("expected ExprStmt");
  };
  let expr = &expr_stmt.stx.expr;
  let Expr::LitObj(obj) = expr.stx.as_ref() else {
    panic!("expected LitObjExpr");
  };
  let member = obj.stx.members.first().expect("member");
  let ObjMemberType::Valued { val, .. } = &member.stx.typ else {
    panic!("expected valued object member");
  };
  let ClassOrObjVal::Method(method) = val else {
    panic!("expected method");
  };

  // The function node span should cover the method signature/body only (`(){}`), not the outer `}`.
  assert_eq!(slice(src, method.stx.func.loc), "(){}");
}
