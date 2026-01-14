use crate::lex::Lexer;
use crate::parse::Parser;
use crate::{Dialect, ParseOptions, SourceType};

#[test]
fn await_binding_identifier_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        class await {}
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_binding_identifier_is_allowed_in_arrow_func_body_in_static_block() {
  let src = r#"
    class C {
      static {
        (() => { class await {} });
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
fn return_in_static_block_is_syntax_error_even_inside_function() {
  let src = r#"
    function f() {
      class C {
        static {
          return;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn yield_in_static_block_is_syntax_error_even_inside_generator() {
  let src = r#"
     function *g() {
       class C {
        static {
          yield;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_expression_parses_in_static_block() {
  let src = r#"
    class C {
      static {
        await 1;
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
fn arguments_identifier_reference_parses_in_static_block() {
  let src = r#"
    class C {
      static {
        arguments;
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
fn arguments_identifier_reference_is_allowed_in_function_in_static_block() {
  let src = r#"
    class C {
      static {
        (function({x = arguments}) {
          arguments;
        });
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
