use crate::lex::Lexer;
use crate::parse::Parser;
use crate::{Dialect, ParseOptions, SourceType};

#[test]
fn yield_expression_is_allowed_in_computed_method_name_in_generator() {
  let src = r#"
    function* g() {
      return { [yield "k"]() { return 1; } };
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn yield_expression_is_allowed_in_computed_getter_name_in_generator() {
  let src = r#"
    function* g() {
      return { get [yield "k"]() { return 1; } };
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn yield_expression_is_allowed_in_computed_setter_name_in_generator() {
  let src = r#"
    function* g() {
      return { set [yield "k"](v) { this.v = v; } };
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}
