use crate::ast::expr::Expr;
use crate::ast::node::Node;
use crate::ast::type_expr::TypeEntityName;
use crate::ast::type_expr::TypeExpr;
use crate::lex::Lexer;
use crate::operator::OperatorName;
use crate::parse::expr::pat::ParsePatternRules;
use crate::parse::Parser;
use crate::parse::{AsiContext, ParseCtx};
use crate::token::TT;
#[cfg(feature = "serde")]
use crate::util::test::evaluate_test_input_files;
use crate::Dialect;
use crate::ParseOptions;
use crate::SourceType;
#[cfg(feature = "serde")]
use serde_json::Value;

fn parse_expr_with_options(input: &str, opts: ParseOptions) -> Node<Expr> {
  let mut parser = Parser::new(Lexer::new(input), opts);
  let is_module = matches!(opts.source_type, SourceType::Module);
  let ctx = ParseCtx {
    rules: ParsePatternRules {
      await_allowed: !is_module,
      yield_allowed: !is_module,
      await_expr_allowed: is_module,
      yield_expr_allowed: false,
    },
    top_level: true,
    in_namespace: false,
    asi: AsiContext::Statements,
  };
  parser.expr(ctx, [TT::Semicolon]).unwrap()
}

#[cfg(feature = "serde")]
fn parse_expr(input: &str) -> Node<Expr> {
  parse_expr_with_options(
    input,
    ParseOptions {
      dialect: Dialect::Tsx,
      source_type: SourceType::Script,
    },
  )
}

#[cfg(feature = "serde")]
fn parse_expr_and_serialize(input: String) -> Value {
  let node = parse_expr(&input);
  serde_json::to_value(&node).unwrap()
}

#[cfg(feature = "serde")]
#[test]
fn test_parse_expression() {
  evaluate_test_input_files("parse/tests/expr", parse_expr_and_serialize);
}

#[test]
fn parses_angle_bracket_type_assertion_expression() {
  let expr = parse_expr_with_options(
    "<Foo>bar;",
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  );

  match *expr.stx {
    Expr::TypeAssertion(ref assertion) => {
      assert!(!assertion.stx.const_assertion);

      let type_annotation = assertion
        .stx
        .type_annotation
        .as_ref()
        .expect("expected type annotation for assertion");
      match *type_annotation.stx {
        TypeExpr::TypeReference(ref type_ref) => match &type_ref.stx.name {
          TypeEntityName::Identifier(name) => assert_eq!(name, "Foo"),
          other => panic!("expected identifier type name, got {:?}", other),
        },
        ref other => panic!("expected type reference, got {:?}", other),
      }

      match *assertion.stx.expression.as_ref().stx {
        Expr::Id(ref id) => assert_eq!(id.stx.name, "bar"),
        ref other => panic!("expected identifier expression, got {:?}", other),
      }
    }
    ref other => panic!("expected type assertion expression, got {:?}", other),
  }
}

#[test]
fn parses_jsx_element_instead_of_type_assertion() {
  let expr = parse_expr_with_options(
    "<foo>bar</foo>;",
    ParseOptions {
      dialect: Dialect::Tsx,
      source_type: SourceType::Module,
    },
  );

  match *expr.stx {
    Expr::JsxElem(_) => {}
    ref other => panic!("expected JSX element, got {:?}", other),
  }
}

#[test]
fn parses_angle_bracket_type_assertion_with_lowercase_qualified_type_in_ts() {
  let expr = parse_expr_with_options(
    "<foo.E1>0;",
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  );

  match *expr.stx {
    Expr::TypeAssertion(_) => {}
    ref other => panic!("expected type assertion, got {:?}", other),
  }
}

#[test]
fn parses_angle_bracket_type_assertion_with_intrinsic_keyword_type_in_ts() {
  let expr = parse_expr_with_options(
    "<intrinsic>bar;",
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  );

  match *expr.stx {
    Expr::TypeAssertion(ref assertion) => {
      let type_annotation = assertion
        .stx
        .type_annotation
        .as_ref()
        .expect("expected type annotation for assertion");
      assert!(matches!(
        type_annotation.stx.as_ref(),
        TypeExpr::Intrinsic(_)
      ));
    }
    ref other => panic!("expected type assertion expression, got {:?}", other),
  }
}

#[test]
fn new_expression_with_arguments_allows_postfix_member_access() {
  let expr = parse_expr_with_options(
    "new Foo().bar;",
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    },
  );

  // `new Foo().bar` must parse as `(new Foo()).bar`, not `new (Foo().bar)`.
  match *expr.stx {
    Expr::Member(ref member) => match member.stx.left.stx.as_ref() {
      Expr::Unary(unary) => {
        assert_eq!(unary.stx.operator, OperatorName::New);
        assert!(
          matches!(unary.stx.argument.stx.as_ref(), Expr::Call(_)),
          "expected new operand to be a call expression, got {:?}",
          unary.stx.argument.stx
        );
      }
      other => panic!("expected member left to be `new` expression, got {:?}", other),
    },
    other => panic!("expected member expression, got {:?}", other),
  }
}

#[test]
fn new_import_call_is_syntax_error() {
  // `new import("...")` is not valid, but `new (import("..."))` is. Ensure the parser rejects the
  // unparenthesized form so `new` cannot accidentally bind to the dynamic import.
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new("new import('x');"), opts);
  let ctx = ParseCtx {
    rules: ParsePatternRules {
      await_allowed: true,
      yield_allowed: true,
      await_expr_allowed: false,
      yield_expr_allowed: false,
    },
    top_level: true,
    in_namespace: false,
    asi: AsiContext::Statements,
  };

  let res = parser.expr(ctx, [TT::Semicolon]);
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}
