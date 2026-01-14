use crate::lex::Lexer;
use crate::parse::Parser;
use crate::{Dialect, ParseOptions, SourceType};

#[test]
fn await_in_class_decl_computed_key_in_async_function_parses() {
  let src = r#"
    async function f() {
      class C {
        [await 1]() {}
      }
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
fn await_in_named_class_expr_computed_key_in_async_function_parses() {
  let src = r#"
    async function f() {
      let C = class D {
        [await D.name]() {}
      };
      return C;
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
fn await_in_object_literal_computed_key_parses_in_module() {
  let src = r#"({[(await Promise.resolve("m"))]() { return "ok"; }})"#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Module,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn await_in_class_decl_computed_key_parses_in_module() {
  let src = r#"
    class C {
      [await 1]() {}
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Module,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}
