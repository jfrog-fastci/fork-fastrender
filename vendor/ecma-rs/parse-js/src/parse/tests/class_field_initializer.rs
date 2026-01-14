use crate::lex::Lexer;
use crate::parse::Parser;
use crate::{Dialect, ParseOptions, SourceType};

#[test]
fn arguments_identifier_reference_is_syntax_error_in_class_field_initializer() {
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
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_arrow_func_in_class_field_initializer() {
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
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_arrow_params_in_class_field_initializer() {
  let src = r#"
    class C {
      x = (y = arguments) => y;
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
fn arguments_identifier_reference_is_allowed_in_function_in_class_field_initializer() {
  let src = r#"
    class C {
      x = function({y = arguments}) {
        return arguments;
      };
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
fn arguments_identifier_reference_is_allowed_in_arrow_in_nested_function_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = function() {
        return (() => arguments)();
      };
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
fn arguments_identifier_reference_is_allowed_in_arrow_in_nested_function_param_default_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = function(y = (() => arguments)) {
        return y;
      };
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
fn arguments_identifier_reference_is_syntax_error_in_class_field_initializer_inside_function() {
  let src = r#"
    function f() {
      class C {
        x = arguments;
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
fn arguments_identifier_reference_is_syntax_error_in_class_field_initializer_inside_method() {
  let src = r#"
    class Outer {
      m() {
        class C {
          x = arguments;
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
fn arguments_identifier_reference_in_object_shorthand_is_syntax_error_in_class_field_initializer() {
  let src = r#"
    class C {
      x = { arguments };
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
fn arguments_identifier_reference_in_object_shorthand_is_allowed_in_nested_function_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = function() {
        ({ arguments });
      };
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
fn arguments_is_allowed_as_object_property_name_in_class_field_initializer() {
  let src = r#"
    class C {
      x = ({ arguments: 1 });
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
fn arguments_identifier_reference_is_allowed_in_object_method_in_class_field_initializer() {
  let src = r#"
    class C {
      x = ({ m() { return arguments; } });
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
fn arguments_identifier_reference_is_allowed_in_object_method_param_default_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = ({ m(x = arguments) { return x; } });
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
fn arguments_identifier_reference_is_allowed_in_nested_class_method_in_class_field_initializer() {
  let src = r#"
    class C {
      x = class D { m() { return arguments; } };
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
fn arguments_identifier_reference_is_allowed_in_nested_class_method_param_default_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = class D { m(x = arguments) { return x; } };
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
fn arguments_identifier_reference_is_allowed_in_arrow_in_object_method_in_class_field_initializer()
{
  let src = r#"
    class C {
      x = ({ m() { return () => arguments; } });
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
fn arguments_identifier_reference_is_allowed_in_arrow_in_nested_class_method_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = class D { m() { return () => arguments; } };
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
fn escaped_arguments_identifier_reference_is_syntax_error_in_class_field_initializer() {
  let src = r#"
    class C {
      x = argu\u006dents;
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
fn escaped_arguments_identifier_reference_is_syntax_error_in_object_shorthand_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = ({ argu\u006dents });
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
fn arguments_identifier_reference_is_syntax_error_in_computed_method_name_in_nested_class_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = (class { [arguments]() {} });
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
fn escaped_arguments_identifier_reference_is_syntax_error_in_computed_method_name_in_nested_class_in_class_field_initializer(
) {
  let src = r#"
    class C {
      x = (class { [argument\u0073]() {} });
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
fn await_expression_is_syntax_error_in_class_field_initializer_in_module() {
  let src = r#"
    class C {
      x = await 1;
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Module,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_expression_is_syntax_error_in_class_field_initializer_inside_async_function() {
  let src = r#"
    async function f() {
      class C {
        x = await 1;
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
fn await_identifier_reference_is_allowed_in_class_field_initializer_inside_async_function() {
  let src = r#"
    async function f() {
      class C {
        x = await;
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
fn yield_expression_is_syntax_error_in_class_field_initializer_inside_generator_function() {
  let src = r#"
    function *g() {
      class C {
        x = yield 1;
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
