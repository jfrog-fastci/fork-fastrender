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
fn await_binding_identifier_is_syntax_error_in_static_block_lexical_decl() {
  let src = r#"
    class C {
      static {
        let await = 0;
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
fn await_identifier_reference_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        await;
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
fn await_is_allowed_as_identifier_in_nested_function_in_static_block() {
  let src = r#"
    class C {
      static {
        function f(await) {
          return await;
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
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn await_is_syntax_error_as_arrow_param_in_static_block() {
  let src = r#"
    class C {
      static {
        (await => 1);
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
fn await_expression_is_syntax_error_in_static_block() {
  let src = r#"
    class A {
      static {
        await 0;
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
fn await_expression_is_syntax_error_in_static_block_inside_function() {
  let src = r#"
    function f() {
      class C {
        static {
          await 0;
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
fn await_expression_is_allowed_in_static_block_inside_async_function() {
  let src = r#"
    async function f() {
      class C {
        static {
          await 0;
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
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn await_expression_is_allowed_in_static_block_in_module() {
  let src = r#"
    class C {
      static {
        await 0;
      }
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

#[test]
fn await_expression_is_syntax_error_in_static_block_in_async_script() {
  let src = r#"
    class C {
      static {
        await 0;
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  parser.set_allow_top_level_await_in_script(true);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_static_block() {
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
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn arguments_identifier_reference_is_syntax_error_in_static_block_inside_function() {
  let src = r#"
    function f() {
      class C {
        static {
          arguments;
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
fn arguments_identifier_reference_is_syntax_error_in_static_block_inside_method() {
  let src = r#"
    class Outer {
      m() {
        class C {
          static {
            arguments;
          }
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
fn arguments_identifier_reference_is_syntax_error_in_arrow_in_static_block() {
  let src = r#"
    class C {
      static {
        (() => arguments);
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
fn arguments_identifier_reference_is_syntax_error_in_arrow_params_in_static_block() {
  let src = r#"
    class C {
      static {
        ((x = arguments) => x);
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

#[test]
fn arguments_identifier_reference_is_allowed_in_arrow_in_nested_function_in_static_block() {
  let src = r#"
    class C {
      static {
        (function() {
          (() => arguments);
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

#[test]
fn arguments_identifier_reference_is_allowed_in_arrow_in_nested_function_param_default_in_static_block(
) {
  let src = r#"
    class C {
      static {
        (function(x = (() => arguments)) {
          return x;
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

#[test]
fn arguments_identifier_reference_in_object_shorthand_is_syntax_error_in_static_block() {
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
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn arguments_identifier_reference_in_object_shorthand_is_allowed_in_nested_function_in_static_block(
) {
  let src = r#"
    class C {
      static {
        (function() {
          ({ arguments });
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

#[test]
fn arguments_is_allowed_as_object_property_name_in_static_block() {
  let src = r#"
    class C {
      static {
        ({ arguments: 1 });
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
fn arguments_identifier_reference_is_allowed_in_object_method_in_static_block() {
  let src = r#"
    class C {
      static {
        ({ m() { return arguments; } });
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
fn arguments_identifier_reference_is_allowed_in_object_method_param_default_in_static_block() {
  let src = r#"
    class C {
      static {
        ({ m(x = arguments) { return x; } });
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
fn arguments_identifier_reference_is_allowed_in_nested_class_method_in_static_block() {
  let src = r#"
    class C {
      static {
        class D { m() { return arguments; } }
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
fn arguments_identifier_reference_is_allowed_in_nested_class_method_param_default_in_static_block() {
  let src = r#"
    class C {
      static {
        class D { m(x = arguments) { return x; } }
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
fn escaped_arguments_identifier_reference_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        argu\u006dents;
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
fn escaped_arguments_identifier_reference_is_syntax_error_in_object_shorthand_in_static_block() {
  let src = r#"
    class C {
      static {
        ({ argu\u006dents });
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
fn arguments_label_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        arguments: ;
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
fn escaped_arguments_label_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        argument\u0073: ;
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
fn arguments_identifier_reference_is_syntax_error_in_computed_method_name_in_static_block() {
  let src = r#"
    class C {
      static {
        (class { [arguments]() {} });
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
fn escaped_arguments_identifier_reference_is_syntax_error_in_computed_method_name_in_static_block() {
  let src = r#"
    class C {
      static {
        (class { [argument\u0073]() {} });
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
fn for_await_of_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        for await (const x of []) {}
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
fn for_await_of_is_syntax_error_in_static_block_inside_function() {
  let src = r#"
    function f() {
      class C {
        static {
          for await (const x of []) {}
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
fn for_await_of_is_allowed_in_static_block_inside_async_function() {
  let src = r#"
    async function f() {
      class C {
        static {
          for await (const x of []) {}
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
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn for_await_of_is_allowed_in_static_block_in_module() {
  let src = r#"
     class C {
      static {
        for await (const x of []) {}
      }
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

#[test]
fn for_await_of_is_syntax_error_in_static_block_in_async_script() {
  let src = r#"
     class C {
      static {
        for await (const x of []) {}
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  parser.set_allow_top_level_await_in_script(true);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}
