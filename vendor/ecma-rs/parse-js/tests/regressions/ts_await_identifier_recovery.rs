use parse_js::ast::expr::lit::LitArrElem;
use parse_js::ast::expr::Expr;
use parse_js::ast::stmt::Stmt;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

fn ts_module_opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  }
}

#[test]
fn ts_recovers_await_as_identifier_when_missing_operand() {
  // TypeScript's parser (in its error-recovery mode) will treat `await` as an
  // identifier reference when it appears where an `AwaitExpression` would be
  // missing its required operand.
  //
  // This matches `parse-js`'s TS dialect behavior and prevents internal TS stmt
  // fixtures from failing to parse.
  let ast = parse_with_options("var x = [await];", ts_module_opts()).unwrap();
  assert_eq!(ast.stx.body.len(), 1);

  let Stmt::VarDecl(decl) = ast.stx.body[0].stx.as_ref() else {
    panic!("expected variable declaration");
  };
  assert_eq!(decl.stx.declarators.len(), 1);

  let init = decl.stx.declarators[0]
    .initializer
    .as_ref()
    .expect("expected initializer");
  let Expr::LitArr(arr) = init.stx.as_ref() else {
    panic!("expected array literal");
  };
  assert_eq!(arr.stx.elements.len(), 1);

  let LitArrElem::Single(elem) = &arr.stx.elements[0] else {
    panic!("expected single array element");
  };
  let Expr::Id(id) = elem.stx.as_ref() else {
    panic!("expected identifier expression");
  };
  assert_eq!(id.stx.name, "await");
}

