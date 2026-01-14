use parse_js::error::SyntaxErrorType;
use parse_js::lex::Lexer;
use parse_js::parse::Parser;
use parse_js::{Dialect, ParseOptions, SourceType};

#[test]
fn arguments_identifier_reference_is_syntax_error_in_class_static_block_ecma() {
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
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(err.typ, SyntaxErrorType::ArgumentsNotAllowedInClassInit);
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_class_static_block_label_ecma() {
  let src = r#"
    class C {
      static {
        arguments: while (false) {}
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(
    err.typ,
    SyntaxErrorType::ArgumentsNotAllowedInClassInit
  );
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_class_static_block_break_label_ecma() {
  let src = r#"
    class C {
      static {
        while (false) {
          break arguments;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(err.typ, SyntaxErrorType::ArgumentsNotAllowedInClassInit);
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_class_static_block_continue_label_ecma() {
  let src = r#"
    class C {
      static {
        while (false) {
          continue arguments;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(err.typ, SyntaxErrorType::ArgumentsNotAllowedInClassInit);
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_class_static_block_object_shorthand_ecma() {
  let src = r#"
    class C {
      static {
        ({ arguments });
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(
    err.typ,
    SyntaxErrorType::ArgumentsNotAllowedInClassInit
  );
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_allowed_in_class_static_block_ts() {
  let src = r#"
    class C {
      static {
        arguments;
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn class_static_block_without_arguments_parses() {
  let src = r#"
    class C {
      static {
        let x = 1;
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
fn arguments_identifier_reference_is_syntax_error_in_class_field_initializer_ecma() {
  let src = r#"
    class C {
      x = arguments;
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(
    err.typ,
    SyntaxErrorType::ArgumentsNotAllowedInClassInit
  );
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_arrow_in_class_field_initializer_ecma() {
  let src = r#"
    class C {
      x = () => arguments;
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let err = parser
    .parse_top_level()
    .expect_err("parse unexpectedly succeeded");
  assert_eq!(
    err.typ,
    SyntaxErrorType::ArgumentsNotAllowedInClassInit
  );
  assert_eq!(err.actual_token, None);
}

#[test]
fn arguments_identifier_reference_is_allowed_in_nested_function_in_class_field_initializer_ecma() {
  let src = r#"
    class C {
      x = function() { arguments; };
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
fn arguments_identifier_reference_is_allowed_in_class_field_initializer_ts() {
  let src = r#"
    class C {
      x = arguments;
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}
